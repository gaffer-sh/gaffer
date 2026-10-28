//! Upload — canonical Rust client for the Gaffer dashboard upload endpoints.
//!
//! Backs the `gaffer upload` CLI command and (via the prebuilt binary) the
//! `gaffer-uploader@v2` GitHub Action. Two paths share one entry point:
//!
//! * **Small bundle** — `multipart/form-data` POST to `/api/upload`, used when
//!   the *encoded* request body fits inside the server's v1 ceiling
//!   (`V1_UPLOAD_CEILING_BYTES`). The server rejects on `Content-Length`, so
//!   routing budgets the multipart framing too — see `encoded_bundle_size`.
//!
//! * **Multipart upload (MPU)** — used per-file for anything that cannot fit
//!   a single POST. Drives the `/api/upload/mpu/{create,part,complete,abort}`
//!   endpoints landed in TASK-39. Part PUTs run in parallel under a tokio
//!   `Semaphore(8)` so a single 5 GB upload doesn't fan out unbounded.
//!
//! When the server disagrees with our ceiling it says so: a 413 carries
//! `{ ceiling, declaredSize, migration }` in its error envelope. Routing
//! honours that figure and re-plans once rather than failing the run
//! (GAF-255), so a server-side ceiling change doesn't need a CLI release.
//!
//! Errors surface as `UploadError` so the CLI command can render a structured
//! JSON line + human-readable block on failure (see `commands/upload.rs`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt};
use reqwest::{header, multipart, Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio::sync::Semaphore;

const DEFAULT_API_URL: &str = "https://app.gaffer.sh";

/// Largest request body `/api/upload` will accept. Must equal
/// `MAX_V1_UPLOAD_SIZE_BYTES` in `@gaffer/pricing`; the server rejects a
/// larger `Content-Length` with a 413 before the handler runs.
///
/// This is a default, not an authority — the server's 413 envelope reports
/// the live figure and routing re-plans against it. Two parity tests keep the
/// default honest from either side of the language boundary (GAF-258):
/// `tests/v1_ceiling_parity.rs` here, and `cli-upload-ceiling-parity.spec.ts`
/// in the dashboard. Editing one constant without the other fails whichever
/// suite the editor was already running.
pub const V1_UPLOAD_CEILING_BYTES: u64 = 75 * 1024 * 1024;

/// Per-file cost of multipart framing: the boundary line, the
/// `Content-Disposition` and `Content-Type` headers, and the trailing CRLF.
/// 256 bytes covers that fixed text plus reqwest's 70-char boundary. The
/// filename is the only unbounded term, so routing counts it separately.
const MULTIPART_PART_FRAMING_BYTES: u64 = 256;

/// Whole-form framing cost: the closing boundary and the `tags` part's own
/// headers. The `tags` value length is added on top by `encoded_bundle_size`.
const MULTIPART_FORM_FRAMING_BYTES: u64 = 512;

/// Hard ceiling on a single MPU object — matches R2's per-object limit and
/// the server-side `MAX_MPU_TOTAL_BYTES`.
pub const MAX_MPU_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Maximum concurrent in-flight part PUTs per MPU. The server has no
/// per-upload concurrency budget, so the cap is purely client-side memory:
/// 8 * 50 MB part = 400 MB peak in the worst case.
pub const MPU_PART_CONCURRENCY: usize = 8;

/// Number of times to retry a single HTTP attempt on transient (5xx /
/// network) failure. Exponential backoff between attempts.
const MAX_RETRIES: usize = 3;

/// Initial backoff. Each subsequent retry doubles.
const RETRY_BASE_DELAY_MS: u64 = 1_000;

/// Configuration for an upload run. Mirrors v1 GitHub Action input names so a
/// future Action wrapper can pass them through 1:1.
///
/// Manual `Debug` impl redacts the token — `--debug` mode dumps this struct
/// in places we want to surface, and we don't want a token landing in CI
/// logs by accident.
#[derive(Clone)]
pub struct UploadConfig {
    pub token: String,
    pub api_url: Option<String>,
    pub commit_sha: Option<String>,
    pub branch: Option<String>,
    pub test_framework: Option<String>,
    pub test_suite: Option<String>,
    pub timeout_secs: u64,
    /// Per-file size limit in megabytes. Files exceeding this are rejected
    /// up front as a user error before any HTTP traffic.
    pub max_file_size_mb: u64,
    /// Version of the binary making the request, stamped into `User-Agent`
    /// and `gaffer-client-version`. Callers pass their *own*
    /// `CARGO_PKG_VERSION` — reading it inside this crate would report
    /// gaffer-core's version instead, which is what used to happen and left
    /// every server-side mitigation guessing (GAF-255).
    pub client_version: String,
    pub debug: bool,
}

impl std::fmt::Debug for UploadConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadConfig")
            .field("token", &"<redacted>")
            .field("api_url", &self.api_url)
            .field("commit_sha", &self.commit_sha)
            .field("branch", &self.branch)
            .field("test_framework", &self.test_framework)
            .field("test_suite", &self.test_suite)
            .field("timeout_secs", &self.timeout_secs)
            .field("max_file_size_mb", &self.max_file_size_mb)
            .field("client_version", &self.client_version)
            .field("debug", &self.debug)
            .finish()
    }
}

impl UploadConfig {
    pub fn api_url(&self) -> &str {
        self.api_url.as_deref().unwrap_or(DEFAULT_API_URL)
    }

    pub fn tags(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        if let Some(v) = &self.commit_sha {
            obj.insert("commitSha".into(), serde_json::Value::String(v.clone()));
        }
        if let Some(v) = &self.branch {
            obj.insert("branch".into(), serde_json::Value::String(v.clone()));
        }
        if let Some(v) = &self.test_framework {
            obj.insert("framework".into(), serde_json::Value::String(v.clone()));
        }
        if let Some(v) = &self.test_suite {
            obj.insert("testSuite".into(), serde_json::Value::String(v.clone()));
        }
        serde_json::Value::Object(obj)
    }
}

/// One file ready to upload. `relative_path` is what the server should use as
/// the filename — preserves directory structure when uploading from a folder.
///
/// `pub(crate)` because the type is an implementation detail — external
/// callers configure via `UploadConfig` and observe via `UploadOutcome`.
#[derive(Debug, Clone)]
pub(crate) struct UploadFile {
    pub absolute_path: PathBuf,
    pub relative_path: String,
    pub size: u64,
}

/// Final outcome of one upload run. Either way the server returns the same
/// `UploadResponse` shape; we capture the bits the CLI surfaces as outputs.
#[derive(Debug, Clone, Serialize)]
pub struct UploadOutcome {
    pub upload_session_id: String,
    pub project_id: Option<String>,
    pub files_uploaded: usize,
    pub bytes_uploaded: u64,
    pub elapsed_ms: u128,
    /// The finalized session. Always exactly one: an upload that needs more
    /// than one request still lands as a single multi-file session (GAF-255).
    /// Kept as a Vec so the CLI's JSON output shape is unchanged.
    pub responses: Vec<ServerUploadResponse>,
}

/// What the dashboard returns for a non-final write into a multi-file
/// session (an append POST, or `/mpu/complete` on a joined MPU).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionAppendResponse {
    #[allow(dead_code)]
    files_received: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionOpenResponse {
    upload_session_id: String,
}

/// Header that turns `POST /api/upload` into an append to an open session.
const UPLOAD_SESSION_HEADER: &str = "X-Gaffer-Upload-Session";

/// Mirrors the dashboard's `UploadResponse` — the relevant subset.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerUploadResponse {
    pub upload_session: SessionRef,
    /// Deprecated v1 alias kept for the Action's `test_run_id` output.
    #[serde(default)]
    pub test_run: Option<SessionRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRef {
    pub id: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub processing_status: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("{message}")]
    UserError {
        message: String,
        kind: &'static str,
    },
    #[error("{message}")]
    ServerError {
        message: String,
        kind: &'static str,
        status: Option<u16>,
        upload_session_id: Option<String>,
        cf_ray_id: Option<String>,
        /// Byte ceiling the server reported in a 413 envelope, when it sent
        /// one. Routing re-plans against this instead of failing the run.
        ceiling: Option<u64>,
    },
    #[error("{message}")]
    Unexpected {
        message: String,
        kind: &'static str,
    },
}

impl UploadError {
    pub fn user(kind: &'static str, message: impl Into<String>) -> Self {
        UploadError::UserError {
            kind,
            message: message.into(),
        }
    }

    pub fn server(kind: &'static str, message: impl Into<String>) -> Self {
        UploadError::ServerError {
            kind,
            message: message.into(),
            status: None,
            upload_session_id: None,
            cf_ray_id: None,
            ceiling: None,
        }
    }

    pub fn unexpected(kind: &'static str, message: impl Into<String>) -> Self {
        UploadError::Unexpected {
            kind,
            message: message.into(),
        }
    }

    pub fn with_status(mut self, code: u16) -> Self {
        if let UploadError::ServerError { status, .. } = &mut self {
            *status = Some(code);
        }
        self
    }

    pub fn with_session(mut self, sid: impl Into<String>) -> Self {
        if let UploadError::ServerError {
            upload_session_id, ..
        } = &mut self
        {
            *upload_session_id = Some(sid.into());
        }
        self
    }

    pub fn with_ray(mut self, ray: impl Into<String>) -> Self {
        if let UploadError::ServerError { cf_ray_id, .. } = &mut self {
            *cf_ray_id = Some(ray.into());
        }
        self
    }

    fn with_ceiling(mut self, bytes: u64) -> Self {
        if let UploadError::ServerError { ceiling, .. } = &mut self {
            *ceiling = Some(bytes);
        }
        self
    }

    /// Ceiling the server reported in a 413 envelope, if any.
    pub fn ceiling(&self) -> Option<u64> {
        match self {
            UploadError::ServerError { ceiling, .. } => *ceiling,
            _ => None,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            UploadError::UserError { kind, .. }
            | UploadError::ServerError { kind, .. }
            | UploadError::Unexpected { kind, .. } => kind,
        }
    }

    /// Process exit code mirroring the contract documented in the CLI help:
    /// 0 success, 1 user error, 2 server error, 3 unexpected.
    pub fn exit_code(&self) -> i32 {
        match self {
            UploadError::UserError { .. } => 1,
            UploadError::ServerError { .. } => 2,
            UploadError::Unexpected { .. } => 3,
        }
    }
}

pub type UploadResult<T> = Result<T, UploadError>;

// ----- Public entry --------------------------------------------------------

/// Upload one or more files / directories to the Gaffer dashboard.
///
/// Routing rules:
/// * A file whose encoded multipart body would exceed the v1 ceiling goes
///   over MPU — it cannot fit a single POST at all.
/// * Everything else is packed into `/api/upload` bundles whose encoded body
///   stays at or under the ceiling.
/// * When that takes more than one request, every request writes into one
///   multi-file session, finalized once (GAF-255). Splitting across sessions
///   would break a hosted HTML report's links to assets in another session,
///   and fail any request that carried only artifacts.
/// * A 413 naming a lower ceiling re-plans that bundle once at the server's
///   figure.
pub async fn run(paths: &[PathBuf], config: &UploadConfig) -> UploadResult<UploadOutcome> {
    let start = Instant::now();

    if config.token.is_empty() {
        return Err(UploadError::user(
            "missing_token",
            "Project token is empty. Pass --token or set GAFFER_PROJECT_TOKEN.",
        ));
    }

    let files = enumerate_files(paths, config.max_file_size_mb)?;
    if files.is_empty() {
        return Err(UploadError::user(
            "no_files",
            "No files found at the provided path(s).",
        ));
    }

    for f in &files {
        if f.size > MAX_MPU_BYTES {
            return Err(UploadError::user(
                "file_too_large",
                format!(
                    "{} is {} bytes, exceeds the 5 GB per-object ceiling.",
                    f.relative_path, f.size
                ),
            ));
        }
    }

    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let total_files = files.len();

    let client = build_http_client(config)?;
    let tags_len = config.tags().to_string().len() as u64;
    let plan = UploadPlan::new(files, V1_UPLOAD_CEILING_BYTES, tags_len);
    let response = upload_planned(&client, config, plan, tags_len).await?;

    Ok(UploadOutcome {
        upload_session_id: response.upload_session.id.clone(),
        project_id: response.upload_session.project_id.clone(),
        files_uploaded: total_files,
        bytes_uploaded: total_bytes,
        elapsed_ms: start.elapsed().as_millis(),
        responses: vec![response],
    })
}

/// How a file set goes over the wire at a given ceiling.
#[derive(Debug)]
struct UploadPlan {
    /// Files too large for any single POST: one MPU each.
    large: Vec<UploadFile>,
    /// POST bodies, each at or under the ceiling.
    bundles: Vec<Vec<UploadFile>>,
    ceiling: u64,
}

impl UploadPlan {
    fn new(files: Vec<UploadFile>, ceiling: u64, tags_len: u64) -> Self {
        let (large, small) = partition_by_ceiling(files, ceiling, tags_len);
        let bundles = pack_small_bundles(small, ceiling, tags_len);
        UploadPlan {
            large,
            bundles,
            ceiling,
        }
    }

    /// Requests needed to send everything. One needs no multi-file session.
    fn request_count(&self) -> usize {
        self.large.len() + self.bundles.len()
    }
}

/// Send a plan. One request goes out session-free, exactly as before GAF-255;
/// anything more opens a multi-file session.
async fn upload_planned(
    client: &Client,
    config: &UploadConfig,
    mut plan: UploadPlan,
    tags_len: u64,
) -> UploadResult<ServerUploadResponse> {
    if plan.request_count() > 1 {
        return upload_in_session(client, config, plan, tags_len).await;
    }
    if let Some(file) = plan.large.pop() {
        return upload_mpu(client, config, &file, None).await;
    }
    let bundle = plan.bundles.pop().unwrap_or_default();
    let err = match upload_small_bundle(client, config, &bundle, None).await {
        Ok(resp) => return Ok(resp),
        Err(e) => e,
    };
    let Some(server_ceiling) = lower_ceiling(&err, plan.ceiling, config.debug, bundle.len()) else {
        return Err(err);
    };
    let replanned = UploadPlan::new(bundle, server_ceiling, tags_len);
    if replanned.request_count() > 1 {
        return upload_in_session(client, config, replanned, tags_len).await;
    }
    // Still one request at the server's ceiling: it can only be a lone MPU,
    // since the same bundle just failed as a POST.
    match replanned.large.into_iter().next() {
        Some(file) => upload_mpu(client, config, &file, None).await,
        None => Err(err),
    }
}

/// The server's 413 `ceiling`, when it is lower than the one we planned at.
/// An equal or higher figure is a disagreement re-planning cannot fix.
fn lower_ceiling(err: &UploadError, planned: u64, debug: bool, files: usize) -> Option<u64> {
    let server_ceiling = err.ceiling().filter(|c| *c < planned)?;
    if debug {
        eprintln!(
            "[gaffer] server reported a {} byte v1 ceiling (we assumed {}); \
             re-planning {} file(s)",
            server_ceiling, planned, files,
        );
    }
    Some(server_ceiling)
}

/// Open a multi-file session, send every request of the plan into it, and
/// finalize once. A failure part-way leaves the session `receiving`; the
/// dashboard's stale-receiving sweep closes it, so there is nothing to undo.
async fn upload_in_session(
    client: &Client,
    config: &UploadConfig,
    plan: UploadPlan,
    tags_len: u64,
) -> UploadResult<ServerUploadResponse> {
    let session_id = open_session(client, config).await?;
    let sent = async {
        for file in &plan.large {
            upload_mpu::<SessionAppendResponse>(client, config, file, Some(&session_id)).await?;
        }
        for bundle in plan.bundles {
            append_bundle(client, config, &session_id, bundle, plan.ceiling, tags_len).await?;
        }
        finalize_session(client, config, &session_id).await
    };
    sent.await.map_err(|e| e.with_session(session_id.clone()))
}

/// Append one bundle to a session, re-planning it **once** if the server
/// answers with a lower ceiling. The session stays open across that 413 (the
/// dashboard rejects it without marking the session failed). A second 413
/// after re-planning at the server's own number is a real failure — looping
/// would just retry the same rejection.
async fn append_bundle(
    client: &Client,
    config: &UploadConfig,
    session_id: &str,
    bundle: Vec<UploadFile>,
    ceiling: u64,
    tags_len: u64,
) -> UploadResult<()> {
    let err = match upload_small_bundle::<SessionAppendResponse>(
        client,
        config,
        &bundle,
        Some(session_id),
    )
    .await
    {
        Ok(_) => return Ok(()),
        Err(e) => e,
    };
    let Some(server_ceiling) = lower_ceiling(&err, ceiling, config.debug, bundle.len()) else {
        return Err(err);
    };
    let replanned = UploadPlan::new(bundle, server_ceiling, tags_len);
    for file in &replanned.large {
        upload_mpu::<SessionAppendResponse>(client, config, file, Some(session_id)).await?;
    }
    for b in &replanned.bundles {
        upload_small_bundle::<SessionAppendResponse>(client, config, b, Some(session_id)).await?;
    }
    Ok(())
}

async fn open_session(client: &Client, config: &UploadConfig) -> UploadResult<String> {
    let url = format!(
        "{}/api/upload/session",
        config.api_url().trim_end_matches('/')
    );
    let body = serde_json::json!({ "tags": config.tags() });
    let send = || async {
        client
            .post(&url)
            .header("X-API-Key", &config.token)
            .json(&body)
            .send()
            .await
            .map_err(reqwest_to_upload_error)
    };
    let resp = send_with_retry(send, config.debug, "POST /api/upload/session").await?;
    let opened: SessionOpenResponse = parse_json_response(resp).await?;
    if config.debug {
        eprintln!(
            "[gaffer] opened multi-file session {}",
            opened.upload_session_id
        );
    }
    Ok(opened.upload_session_id)
}

async fn finalize_session(
    client: &Client,
    config: &UploadConfig,
    session_id: &str,
) -> UploadResult<ServerUploadResponse> {
    let url = format!(
        "{}/api/upload/session/{}/finalize",
        config.api_url().trim_end_matches('/'),
        session_id
    );
    let send = || async {
        client
            .post(&url)
            .header("X-API-Key", &config.token)
            .send()
            .await
            .map_err(reqwest_to_upload_error)
    };
    let resp = send_with_retry(send, config.debug, "POST /api/upload/session/finalize").await?;
    parse_upload_response(resp).await
}

// ----- File enumeration ----------------------------------------------------

/// Expand the supplied paths into a flat file list.
///
/// * A direct file path becomes a single entry whose `relative_path` is the
///   filename (no directory prefix).
/// * A directory walks recursively; entries' `relative_path` is the path
///   relative to that directory's root, so nested folder structure survives.
///
/// Files larger than `max_file_size_mb` raise a `file_too_large` user error
/// at this stage so the CLI doesn't even open a connection.
pub(crate) fn enumerate_files(paths: &[PathBuf], max_mb: u64) -> UploadResult<Vec<UploadFile>> {
    let max_bytes = max_mb.saturating_mul(1024 * 1024);
    let mut out = Vec::new();

    for raw in paths {
        let meta = std::fs::metadata(raw).map_err(|e| {
            UploadError::user(
                "path_not_found",
                format!("Cannot access {}: {}", raw.display(), e),
            )
        })?;

        if meta.is_file() {
            let size = meta.len();
            if size > max_bytes {
                return Err(over_max_file_error(raw, size, max_mb));
            }
            let name = raw
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "upload.bin".to_string());
            out.push(UploadFile {
                absolute_path: raw.to_path_buf(),
                relative_path: name,
                size,
            });
        } else if meta.is_dir() {
            for entry in walkdir::WalkDir::new(raw).follow_links(false) {
                let entry = entry.map_err(|e| {
                    UploadError::user(
                        "path_walk_failed",
                        format!("Failed to walk {}: {}", raw.display(), e),
                    )
                })?;
                if !entry.file_type().is_file() {
                    continue;
                }
                // Don't silently treat a stat-failed file as size 0 — it
                // would still get uploaded under the user's intended name
                // with empty content. Surface the read failure instead.
                let size = entry.metadata().map(|m| m.len()).map_err(|e| {
                    UploadError::user(
                        "path_walk_failed",
                        format!(
                            "Failed to stat {}: {}",
                            entry.path().display(),
                            e
                        ),
                    )
                })?;
                if size > max_bytes {
                    return Err(over_max_file_error(entry.path(), size, max_mb));
                }
                let rel = entry
                    .path()
                    .strip_prefix(raw)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| {
                        entry
                            .path()
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "upload.bin".to_string())
                    });
                out.push(UploadFile {
                    absolute_path: entry.path().to_path_buf(),
                    relative_path: rel,
                    size,
                });
            }
        } else {
            return Err(UploadError::user(
                "path_unsupported",
                format!("{} is neither a file nor a directory.", raw.display()),
            ));
        }
    }

    Ok(out)
}

/// Convenience overload taking owned `PathBuf` slice.
fn over_max_file_error(path: &Path, size: u64, max_mb: u64) -> UploadError {
    let size_mb = (size as f64) / (1024.0 * 1024.0);
    UploadError::user(
        "file_too_large",
        format!(
            "{} is {:.2} MB, exceeds the {} MB per-file limit (--max-file-size-mb).",
            path.display(),
            size_mb,
            max_mb
        ),
    )
}

// ----- Ceiling-aware routing -----------------------------------------------

/// Bytes a single file contributes to a `multipart/form-data` body: its
/// content, its filename (echoed in `Content-Disposition`), and the fixed
/// framing around it.
fn encoded_part_size(file: &UploadFile) -> u64 {
    file.size + file.relative_path.len() as u64 + MULTIPART_PART_FRAMING_BYTES
}

/// Bytes the whole request body would declare in `Content-Length`.
///
/// The server rejects on the declared length, not on the sum of file sizes,
/// so routing has to budget the framing. Overshooting here costs an extra
/// request; undershooting costs a 413 — so the estimate is deliberately
/// generous (see `MULTIPART_PART_FRAMING_BYTES`).
fn encoded_bundle_size(files: &[UploadFile], tags_len: u64) -> u64 {
    files.iter().map(encoded_part_size).sum::<u64>() + MULTIPART_FORM_FRAMING_BYTES + tags_len
}

/// Split files into those that cannot fit a single POST at all and those that
/// can be bundled. A file is "large" when its own one-file request body would
/// already exceed `ceiling`.
fn partition_by_ceiling(
    files: Vec<UploadFile>,
    ceiling: u64,
    tags_len: u64,
) -> (Vec<UploadFile>, Vec<UploadFile>) {
    files
        .into_iter()
        .partition(|f| encoded_bundle_size(std::slice::from_ref(f), tags_len) > ceiling)
}

/// Pack bundleable files into requests whose encoded body stays at or under
/// `ceiling`.
///
/// First-fit decreasing: sort largest-first, then drop each file into the
/// first bundle that still has room; otherwise start a new one. Common case
/// is one bundle — this only matters when a project ships enough files to
/// overflow the ceiling, which is exactly the Playwright-artifact shape that
/// used to 413 (GAF-255).
///
/// Callers must have removed un-bundleable files first (`partition_by_ceiling`)
/// so every file is guaranteed to fit a bundle of its own.
fn pack_small_bundles(
    mut files: Vec<UploadFile>,
    ceiling: u64,
    tags_len: u64,
) -> Vec<Vec<UploadFile>> {
    files.sort_by_key(|f| std::cmp::Reverse(f.size));
    let overhead = MULTIPART_FORM_FRAMING_BYTES + tags_len;
    let mut bundles: Vec<(u64, Vec<UploadFile>)> = Vec::new();
    for f in files {
        let part = encoded_part_size(&f);
        let mut placed = false;
        for (sz, bucket) in bundles.iter_mut() {
            if *sz + part <= ceiling {
                *sz += part;
                bucket.push(f.clone());
                placed = true;
                break;
            }
        }
        if !placed {
            bundles.push((overhead + part, vec![f]));
        }
    }
    bundles.into_iter().map(|(_, b)| b).collect()
}

// ----- HTTP client ---------------------------------------------------------

fn build_http_client(config: &UploadConfig) -> UploadResult<Client> {
    // `gaffer-client` / `gaffer-client-version` are read by the dashboard's
    // upload analytics and `X-Gaffer-Source` by its audit log. Sending them on
    // every request — not just the v1 POST — means a server-side mitigation
    // can tell an old CLI from a new one on any endpoint (GAF-255).
    let mut headers = header::HeaderMap::new();
    headers.insert("gaffer-client", header::HeaderValue::from_static("gaffer-cli"));
    headers.insert("X-Gaffer-Source", header::HeaderValue::from_static("cli"));
    let version = header::HeaderValue::from_str(&config.client_version).map_err(|e| {
        UploadError::unexpected(
            "http_init",
            format!(
                "client version {:?} is not a valid header value: {}",
                config.client_version, e
            ),
        )
    })?;
    headers.insert("gaffer-client-version", version);

    Client::builder()
        // The per-request timeout governs each individual HTTP call. A large
        // MPU upload is many calls, so this is *not* a wall-clock budget for
        // the whole upload — it's the deadline for any single round-trip.
        .timeout(Duration::from_secs(config.timeout_secs))
        .user_agent(format!("gaffer-cli/{}", config.client_version))
        .default_headers(headers)
        .build()
        .map_err(|e| UploadError::unexpected("http_init", format!("HTTP client init: {}", e)))
}

// ----- Small bundle path ---------------------------------------------------

/// Infer a Content-Type from a file path's extension.
///
/// Mirrors the dashboard's `getMimeType` table
/// (`apps/dashboard/server/utils/mime-types.ts`) so the small-bundle multipart
/// upload stores the same accurate type the server infers for the MPU path.
/// Sending `application/octet-stream` for everything made hosted reports
/// download instead of render inline. Keep this table in parity with
/// mime-types.ts. Unknown/missing extensions fall back to octet-stream, which
/// the server then re-infers defensively at upload time.
fn content_type_for(path: &str) -> &'static str {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("html" | "htm") => "text/html",
        Some("xml" | "trx") => "application/xml",
        Some("json") => "application/json",
        Some("css") => "text/css",
        Some("js" | "mjs") => "application/javascript",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("ico") => "image/x-icon",
        Some("webp") => "image/webp",
        // Video — Playwright/Vitest embed failure videos in HTML reports.
        Some("webm") => "video/webm",
        Some("mp4") => "video/mp4",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("eot") => "application/vnd.ms-fontobject",
        Some("gz") => "application/gzip",
        Some("zip") => "application/zip",
        Some("tar") => "application/x-tar",
        Some("txt" | "lcov" | "info") => "text/plain",
        Some("md") => "text/markdown",
        Some("csv") => "text/csv",
        Some("webmanifest") => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

/// One `/api/upload` POST. With `session` it appends to that open multi-file
/// session (the server answers with a `SessionAppendResponse`); without, it
/// creates and finalizes its own session (a `ServerUploadResponse`).
async fn upload_small_bundle<T: for<'de> Deserialize<'de>>(
    client: &Client,
    config: &UploadConfig,
    files: &[UploadFile],
    session: Option<&str>,
) -> UploadResult<T> {
    let url = format!("{}/api/upload", config.api_url().trim_end_matches('/'));
    let tags_json = config.tags().to_string();

    let send = || async {
        let mut form = multipart::Form::new();
        for f in files {
            let bytes = tokio::fs::read(&f.absolute_path).await.map_err(|e| {
                UploadError::user(
                    "io_error",
                    format!("Failed to read {}: {}", f.absolute_path.display(), e),
                )
            })?;
            let part = multipart::Part::bytes(bytes)
                .file_name(f.relative_path.clone())
                .mime_str(content_type_for(&f.relative_path))
                .map_err(|e| UploadError::unexpected("mime", e.to_string()))?;
            form = form.part("files", part);
        }
        form = form.text("tags", tags_json.clone());

        let mut req = client
            .post(&url)
            .header("X-API-Key", &config.token)
            .multipart(form);
        if let Some(sid) = session {
            req = req.header(UPLOAD_SESSION_HEADER, sid);
        }

        if config.debug {
            eprintln!("[gaffer] POST {} ({} files)", url, files.len());
        }

        req.send().await.map_err(reqwest_to_upload_error)
    };

    let resp = send_with_retry(send, config.debug, "POST /api/upload").await?;
    parse_json_response(resp).await
}

// ----- MPU path ------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MpuCreateRequest<'a> {
    file_name: &'a str,
    file_size: u64,
    tags: serde_json::Value,
    /// Join this open multi-file session instead of creating one (GAF-255).
    #[serde(skip_serializing_if = "Option::is_none")]
    upload_session_id: Option<&'a str>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct MpuCreateResponse {
    upload_id: String,
    upload_session_id: String,
    #[allow(dead_code)]
    key: String,
    part_size: u64,
    num_parts: u64,
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadedPart {
    part_number: u32,
    etag: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MpuCompleteRequest<'a> {
    upload_session_id: &'a str,
    upload_id: &'a str,
    parts: &'a [UploadedPart],
}

/// One file over R2 multipart. With `session` the MPU joins that open
/// multi-file session and `/complete` appends rather than finalizes, so the
/// response is a `SessionAppendResponse`; without, a `ServerUploadResponse`.
async fn upload_mpu<T: for<'de> Deserialize<'de>>(
    client: &Client,
    config: &UploadConfig,
    file: &UploadFile,
    session: Option<&str>,
) -> UploadResult<T> {
    let base = config.api_url().trim_end_matches('/').to_string();
    let create_url = format!("{}/api/upload/mpu/create", base);
    let part_url = format!("{}/api/upload/mpu/part", base);
    let complete_url = format!("{}/api/upload/mpu/complete", base);
    let abort_url = format!("{}/api/upload/mpu/abort", base);

    // ---- /create ----
    let create_start = Instant::now();
    let create_body = MpuCreateRequest {
        file_name: &file.relative_path,
        file_size: file.size,
        tags: config.tags(),
        upload_session_id: session,
    };
    let create_send = || async {
        client
            .post(&create_url)
            .header("X-API-Key", &config.token)
            .json(&create_body)
            .send()
            .await
            .map_err(reqwest_to_upload_error)
    };
    let create_resp = send_with_retry(create_send, config.debug, "POST /mpu/create").await?;
    let create: MpuCreateResponse = parse_json_response(create_resp).await?;
    if config.debug {
        eprintln!(
            "[gaffer] mpu/create {} ({} parts × {} bytes) in {:.2}s",
            create.upload_session_id,
            create.num_parts,
            create.part_size,
            create_start.elapsed().as_secs_f64()
        );
    }

    // ---- /part (parallel, capped at MPU_PART_CONCURRENCY) ----
    let parts_result = upload_parts(
        client,
        config,
        &part_url,
        file,
        &create.upload_session_id,
        &create.upload_id,
        create.part_size,
        create.num_parts,
    )
    .await;

    let parts = match parts_result {
        Ok(p) => p,
        Err(err) => {
            // Best-effort abort — we don't want to bubble a second failure
            // and obscure the original cause.
            let _ = abort_mpu(
                client,
                config,
                &abort_url,
                &create.upload_session_id,
                &create.upload_id,
            )
            .await;
            return Err(err.with_session(create.upload_session_id.clone()));
        }
    };

    // ---- /complete ----
    let complete_body = MpuCompleteRequest {
        upload_session_id: &create.upload_session_id,
        upload_id: &create.upload_id,
        parts: &parts,
    };
    let complete_send = || async {
        client
            .post(&complete_url)
            .header("X-API-Key", &config.token)
            .json(&complete_body)
            .send()
            .await
            .map_err(reqwest_to_upload_error)
    };
    let complete_resp = match send_with_retry(complete_send, config.debug, "POST /mpu/complete")
        .await
    {
        Ok(r) => r,
        Err(err) => {
            let _ = abort_mpu(
                client,
                config,
                &abort_url,
                &create.upload_session_id,
                &create.upload_id,
            )
            .await;
            return Err(err.with_session(create.upload_session_id.clone()));
        }
    };

    parse_json_response(complete_resp).await
}

#[allow(clippy::too_many_arguments)]
async fn upload_parts(
    client: &Client,
    config: &UploadConfig,
    part_url: &str,
    file: &UploadFile,
    upload_session_id: &str,
    upload_id: &str,
    part_size: u64,
    num_parts: u64,
) -> UploadResult<Vec<UploadedPart>> {
    let sem = Arc::new(Semaphore::new(MPU_PART_CONCURRENCY));
    let mut futs = FuturesUnordered::new();

    for part_number in 1..=num_parts {
        let permit = sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| UploadError::unexpected("semaphore", e.to_string()))?;
        let client = client.clone();
        let part_url = part_url.to_string();
        let upload_session_id = upload_session_id.to_string();
        let upload_id = upload_id.to_string();
        let token = config.token.clone();
        let path = file.absolute_path.clone();
        let total = file.size;
        let debug = config.debug;

        futs.push(tokio::spawn(async move {
            let _permit = permit;
            let part_start = Instant::now();

            let offset = (part_number - 1) * part_size;
            let len = part_size.min(total.saturating_sub(offset));
            if len == 0 {
                return Err(UploadError::unexpected(
                    "empty_part",
                    format!("Part {} computed to zero bytes", part_number),
                ));
            }

            let mut f = File::open(&path).await.map_err(|e| {
                UploadError::user("io_error", format!("Open {}: {}", path.display(), e))
            })?;
            f.seek(SeekFrom::Start(offset)).await.map_err(|e| {
                UploadError::user("io_error", format!("Seek {}: {}", path.display(), e))
            })?;
            let mut buf = vec![0u8; len as usize];
            f.read_exact(&mut buf).await.map_err(|e| {
                UploadError::user("io_error", format!("Read {}: {}", path.display(), e))
            })?;

            let send = || {
                let buf = buf.clone();
                let client = client.clone();
                let part_url = part_url.clone();
                let token = token.clone();
                let upload_session_id = upload_session_id.clone();
                let upload_id = upload_id.clone();
                async move {
                    client
                        .put(&part_url)
                        .query(&[
                            ("uploadSessionId", upload_session_id.as_str()),
                            ("uploadId", upload_id.as_str()),
                            ("partNumber", &part_number.to_string()),
                        ])
                        .header("X-API-Key", token)
                        .header("Content-Type", "application/octet-stream")
                        .body(buf)
                        .send()
                        .await
                        .map_err(reqwest_to_upload_error)
                }
            };

            let resp = send_with_retry(send, debug, "PUT /mpu/part").await?;
            let server_part: UploadedPart = parse_json_response(resp).await?;

            if debug {
                let mb = (len as f64) / (1024.0 * 1024.0);
                let secs = part_start.elapsed().as_secs_f64().max(0.001);
                eprintln!(
                    "[gaffer] part {}/{} {:.2} MB @ {:.2} MB/s",
                    part_number,
                    num_parts,
                    mb,
                    mb / secs,
                );
            }
            // Substitute the locally-known partNumber. The server echoes
            // whatever it parsed from the query string, but trusting that
            // would let a buggy or compromised server scramble our /complete
            // ordering. The etag is the only field we actually need from
            // the response.
            Ok::<_, UploadError>(UploadedPart {
                part_number: part_number as u32,
                etag: server_part.etag,
            })
        }));
    }

    let mut parts = Vec::with_capacity(num_parts as usize);
    while let Some(joined) = futs.next().await {
        match joined {
            Ok(Ok(part)) => parts.push(part),
            Ok(Err(e)) => return Err(e),
            Err(join_err) => {
                return Err(UploadError::unexpected(
                    "join_error",
                    format!("part task panicked: {}", join_err),
                ));
            }
        }
    }

    // Catch a partial-failure scenario where a future was cancelled or the
    // FuturesUnordered was drained early — we'd otherwise send /complete
    // with a truncated parts list and R2 would assemble a corrupt object.
    if parts.len() != num_parts as usize {
        return Err(UploadError::unexpected(
            "part_count_mismatch",
            format!(
                "Expected {} parts but collected {} — refusing to /complete",
                num_parts,
                parts.len()
            ),
        ));
    }

    parts.sort_by_key(|p| p.part_number);
    Ok(parts)
}

async fn abort_mpu(
    client: &Client,
    config: &UploadConfig,
    abort_url: &str,
    upload_session_id: &str,
    upload_id: &str,
) -> UploadResult<()> {
    // Abort is fired from failure-handling paths where the original error
    // is what we want to surface to the user. We log abort failures
    // unconditionally (not gated on --debug) so support can correlate
    // orphaned R2 multipart uploads with the user-visible failure — but
    // we still return Ok(()) so the abort doesn't mask the real cause.
    let result = client
        .delete(abort_url)
        .query(&[
            ("uploadSessionId", upload_session_id),
            ("uploadId", upload_id),
        ])
        .header("X-API-Key", &config.token)
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() || resp.status() == StatusCode::NO_CONTENT => {
            if config.debug {
                eprintln!(
                    "[gaffer] mpu/abort {} → {}",
                    upload_session_id,
                    resp.status()
                );
            }
        }
        Ok(resp) => {
            eprintln!(
                "[gaffer] WARNING: mpu/abort returned {} for session {}; \
                 R2 multipart parts may leak until the daily orphan sweep.",
                resp.status(),
                upload_session_id,
            );
        }
        Err(e) => {
            eprintln!(
                "[gaffer] WARNING: mpu/abort transport error for session {}: {}; \
                 R2 multipart parts may leak until the daily orphan sweep.",
                upload_session_id, e,
            );
        }
    }
    Ok(())
}

// ----- Retry + response handling -------------------------------------------

async fn send_with_retry<F, Fut>(
    mut send: F,
    debug: bool,
    label: &str,
) -> UploadResult<Response>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = UploadResult<Response>>,
{
    let mut attempt: usize = 0;
    loop {
        let outcome = send().await;
        match outcome {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return Ok(resp);
                }
                // 4xx: do not retry. Surface as a server error so the CLI can
                // print the structured failure.
                if status.is_client_error() {
                    return Err(server_error_from_response(label, resp).await);
                }
                // 5xx + 429: retry with backoff.
                if attempt >= MAX_RETRIES {
                    return Err(server_error_from_response(label, resp).await);
                }
                let delay = backoff_delay(attempt);
                if debug {
                    eprintln!(
                        "[gaffer] {} returned {}; retrying in {}ms (attempt {}/{})",
                        label,
                        status,
                        delay.as_millis(),
                        attempt + 1,
                        MAX_RETRIES
                    );
                }
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(e) => {
                // Retry network errors only for transient kinds. The
                // reqwest_to_upload_error helper already maps these to
                // ServerError so we use that as the marker.
                let retriable = matches!(&e, UploadError::ServerError { .. });
                if retriable && attempt < MAX_RETRIES {
                    let delay = backoff_delay(attempt);
                    if debug {
                        eprintln!(
                            "[gaffer] {} network error: {}; retrying in {}ms",
                            label,
                            e,
                            delay.as_millis(),
                        );
                    }
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                    continue;
                }
                return Err(e);
            }
        }
    }
}

fn backoff_delay(attempt: usize) -> Duration {
    Duration::from_millis(RETRY_BASE_DELAY_MS << attempt)
}

/// Map a reqwest transport error into the right `UploadError` variant.
///
/// `is_builder` errors (bad URL, malformed body) are not transient and
/// should never be retried — they're caller-side bugs surfaced as
/// `Unexpected`. Everything else (connect, timeout, TLS, redirect) is a
/// `ServerError { kind: "network" }` which the retry loop will reattempt.
fn reqwest_to_upload_error(e: reqwest::Error) -> UploadError {
    if e.is_builder() {
        return UploadError::unexpected(
            "request_build",
            format!("HTTP request build failed: {}", e),
        );
    }
    UploadError::server("network", format!("HTTP request failed: {}", e))
}

async fn server_error_from_response(label: &str, resp: Response) -> UploadError {
    let status = resp.status().as_u16();
    let cf_ray = resp
        .headers()
        .get("cf-ray")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let body = resp.text().await.unwrap_or_default();
    let envelope = parse_error_body(&body);
    let message = envelope.message;
    let kind: &'static str = if status >= 500 || status == 429 {
        "server_error"
    } else if status == 401 || status == 403 {
        "auth_failed"
    } else if status == 402 {
        "quota_exceeded"
    } else if status == 413 {
        "file_too_large"
    } else {
        "request_rejected"
    };
    let mut err = UploadError::server(
        kind,
        format!("{} failed ({}): {}", label, status, message),
    )
    .with_status(status);
    if let Some(sid) = envelope.upload_session_id {
        err = err.with_session(sid);
    }
    if let Some(ray) = cf_ray {
        err = err.with_ray(ray);
    }
    // Only a 413 carries a meaningful ceiling. Reading it off other statuses
    // would let an unrelated field named `ceiling` re-plan the upload.
    if status == 413 {
        if let Some(ceiling) = envelope.ceiling {
            err = err.with_ceiling(ceiling);
        }
    }
    err
}

/// Fields worth recovering from the dashboard's H3 error envelope
/// (`{ statusMessage, data: { uploadSessionId, ceiling, … } }`).
struct ErrorEnvelope {
    message: String,
    upload_session_id: Option<String>,
    /// `data.ceiling` from the v1 413 (`file_too_large_for_v1`). The server's
    /// live `MAX_V1_UPLOAD_SIZE_BYTES`, which may differ from our default.
    ceiling: Option<u64>,
}

/// Try to extract a human message + uploadSessionId + ceiling from the H3
/// error envelope, or from a free-form error body. Falls back to the raw body
/// on parse fail.
fn parse_error_body(body: &str) -> ErrorEnvelope {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return ErrorEnvelope {
            message: body.to_string(),
            upload_session_id: None,
            ceiling: None,
        };
    };
    ErrorEnvelope {
        message: v
            .get("statusMessage")
            .or_else(|| v.get("message"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| body.to_string()),
        upload_session_id: v
            .pointer("/data/uploadSessionId")
            .or_else(|| v.get("uploadSessionId"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_string()),
        ceiling: v
            .pointer("/data/ceiling")
            .or_else(|| v.get("ceiling"))
            .and_then(|c| c.as_u64()),
    }
}

async fn parse_upload_response(resp: Response) -> UploadResult<ServerUploadResponse> {
    parse_json_response(resp).await
}

async fn parse_json_response<T: for<'de> Deserialize<'de>>(resp: Response) -> UploadResult<T> {
    let status = resp.status();
    let body = resp.text().await.map_err(|e| {
        UploadError::server("network", format!("Failed to read response body: {}", e))
            .with_status(status.as_u16())
    })?;
    // 2xx with empty body is a server-side oddity but worth distinguishing
    // from genuine JSON garbage — most likely the operation succeeded
    // server-side and we just lost the response. The upload session id
    // already lives on the server; the user can verify in the dashboard.
    if body.trim().is_empty() {
        return Err(UploadError::server(
            "empty_response",
            format!(
                "Server returned {} with empty body — the operation may have \
                 succeeded; check the dashboard before retrying.",
                status.as_u16()
            ),
        )
        .with_status(status.as_u16()));
    }
    serde_json::from_str(&body).map_err(|e| {
        UploadError::unexpected(
            "bad_response",
            format!("Server returned unparseable JSON: {} (body: {})", e, body),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn cfg() -> UploadConfig {
        UploadConfig {
            token: "gfr_test".into(),
            api_url: Some("http://localhost".into()),
            commit_sha: None,
            branch: None,
            test_framework: None,
            test_suite: None,
            timeout_secs: 30,
            max_file_size_mb: 100,
            client_version: "9.9.9-test".into(),
            debug: false,
        }
    }

    const MIB: u64 = 1024 * 1024;

    fn mk(name: &str, size: u64) -> UploadFile {
        UploadFile {
            absolute_path: PathBuf::new(),
            relative_path: name.to_string(),
            size,
        }
    }

    /// Largest file content that still fits one request at the v1 ceiling,
    /// given a filename of `name_len` bytes and no tags.
    fn max_content_for(name_len: u64) -> u64 {
        V1_UPLOAD_CEILING_BYTES
            - MULTIPART_FORM_FRAMING_BYTES
            - MULTIPART_PART_FRAMING_BYTES
            - name_len
    }

    #[test]
    fn content_type_for_infers_report_types() {
        assert_eq!(content_type_for("index.html"), "text/html");
        assert_eq!(content_type_for("report.htm"), "text/html");
        assert_eq!(content_type_for("junit.xml"), "application/xml");
        assert_eq!(content_type_for("results.trx"), "application/xml");
        assert_eq!(content_type_for("data.json"), "application/json");
        assert_eq!(content_type_for("lcov.info"), "text/plain");
    }

    #[test]
    fn content_type_for_infers_html_report_assets() {
        // Playwright/Vitest HTML reports ship a tree of assets that must each
        // serve with the right type for the report to render.
        assert_eq!(content_type_for("assets/index-X8b7Z_4p.css"), "text/css");
        assert_eq!(content_type_for("assets/index-D_ryMEPs.js"), "application/javascript");
        assert_eq!(content_type_for("trace/screenshot.png"), "image/png");
        assert_eq!(content_type_for("fonts/codicon.woff2"), "font/woff2");
        // Embedded failure videos must serve as video/* so the report's
        // <video> element plays instead of downloading.
        assert_eq!(content_type_for("data/retry1-video.webm"), "video/webm");
        assert_eq!(content_type_for("site.webmanifest"), "application/manifest+json");
    }

    #[test]
    fn content_type_for_is_case_insensitive_and_uses_last_extension() {
        assert_eq!(content_type_for("REPORT.HTML"), "text/html");
        assert_eq!(content_type_for("html.meta.json.gz"), "application/gzip");
    }

    #[test]
    fn content_type_for_falls_back_to_octet_stream() {
        assert_eq!(content_type_for("trace.bin"), "application/octet-stream");
        assert_eq!(content_type_for("Makefile"), "application/octet-stream");
    }

    #[test]
    fn enumerate_single_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("report.xml");
        std::fs::write(&file, b"<x/>").unwrap();
        let files = enumerate_files(std::slice::from_ref(&file), 100).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative_path, "report.xml");
        assert_eq!(files[0].size, 4);
    }

    #[test]
    fn enumerate_directory_preserves_relative_paths() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("a.xml"), b"a").unwrap();
        std::fs::write(dir.path().join("nested/b.json"), b"b").unwrap();
        let mut files = enumerate_files(&[dir.path().to_path_buf()], 100).unwrap();
        files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].relative_path, "a.xml");
        assert!(files[1].relative_path.ends_with("b.json"));
    }

    #[test]
    fn enumerate_rejects_oversized_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("big.bin");
        std::fs::write(&file, vec![0u8; 200 * 1024]).unwrap(); // 200 KB
        let err = enumerate_files(&[file], 0).unwrap_err(); // 0 MB max
        match err {
            UploadError::UserError { kind, .. } => assert_eq!(kind, "file_too_large"),
            _ => panic!("expected user error"),
        }
    }

    #[test]
    fn tags_omits_unset_fields() {
        let mut c = cfg();
        c.commit_sha = Some("abc".into());
        let v = c.tags();
        assert_eq!(v["commitSha"], "abc");
        assert!(v.get("branch").is_none());
    }

    #[test]
    fn pack_small_bundles_never_exceeds_the_ceiling() {
        // The GAF-255 shape: a Playwright artifact tree totalling 120 MiB of
        // individually-small files. The old routing sent this as one POST into
        // a 75 MiB ceiling; it must now split.
        let files: Vec<_> = (0..120).map(|i| mk(&format!("trace-{i}.zip"), MIB)).collect();
        let bundles = pack_small_bundles(files, V1_UPLOAD_CEILING_BYTES, 0);
        assert!(bundles.len() > 1, "120 MiB must not go out as one request");
        for b in &bundles {
            assert!(
                encoded_bundle_size(b, 0) <= V1_UPLOAD_CEILING_BYTES,
                "bundle of {} files encodes to {} bytes, over the ceiling",
                b.len(),
                encoded_bundle_size(b, 0),
            );
        }
        let packed: usize = bundles.iter().map(|b| b.len()).sum();
        assert_eq!(packed, 120, "every file must land in exactly one bundle");
    }

    /// Boundary arithmetic on the bundling decision: a bundle whose encoded
    /// body lands exactly on the ceiling is one request; one byte more is two.
    #[test]
    fn pack_small_bundles_at_under_and_over_the_ceiling() {
        let budget = max_content_for("f.bin".len() as u64);

        // Just under, and exactly at: one request.
        for content in [budget - 1, budget] {
            let bundles =
                pack_small_bundles(vec![mk("f.bin", content)], V1_UPLOAD_CEILING_BYTES, 0);
            assert_eq!(bundles.len(), 1, "content {content} should fit one request");
            assert!(encoded_bundle_size(&bundles[0], 0) <= V1_UPLOAD_CEILING_BYTES);
        }

        // One byte over the ceiling in aggregate: two requests.
        let bundles = pack_small_bundles(
            vec![mk("f.bin", budget), mk("g.bin", 1)],
            V1_UPLOAD_CEILING_BYTES,
            0,
        );
        assert_eq!(bundles.len(), 2, "one byte over the ceiling must split");
    }

    /// Boundary arithmetic on the MPU decision: a file is only routed to MPU
    /// when it cannot fit a single POST *including* its multipart framing.
    #[test]
    fn partition_by_ceiling_at_under_and_over_the_ceiling() {
        let budget = max_content_for("f.bin".len() as u64);

        for content in [budget - 1, budget] {
            let (large, small) =
                partition_by_ceiling(vec![mk("f.bin", content)], V1_UPLOAD_CEILING_BYTES, 0);
            assert!(large.is_empty(), "content {content} fits a POST");
            assert_eq!(small.len(), 1);
        }

        let (large, small) =
            partition_by_ceiling(vec![mk("f.bin", budget + 1)], V1_UPLOAD_CEILING_BYTES, 0);
        assert_eq!(large.len(), 1, "one byte over the ceiling must take MPU");
        assert!(small.is_empty());
    }

    /// A file whose *content* is under the ceiling but whose encoded body is
    /// not. The pre-GAF-255 routing compared raw file bytes to a threshold
    /// above the server's limit and 413'd on exactly this shape.
    #[test]
    fn framing_overhead_counts_against_the_ceiling() {
        let at_ceiling = mk("f.bin", V1_UPLOAD_CEILING_BYTES);
        assert!(
            encoded_bundle_size(std::slice::from_ref(&at_ceiling), 0) > V1_UPLOAD_CEILING_BYTES,
            "a file sized exactly at the ceiling cannot fit once framed",
        );
        let (large, _) = partition_by_ceiling(vec![at_ceiling], V1_UPLOAD_CEILING_BYTES, 0);
        assert_eq!(large.len(), 1);
    }

    #[test]
    fn tags_length_shrinks_the_usable_budget() {
        let budget = max_content_for("f.bin".len() as u64);
        let tags_len = 64;
        let (large, _) =
            partition_by_ceiling(vec![mk("f.bin", budget)], V1_UPLOAD_CEILING_BYTES, tags_len);
        assert_eq!(large.len(), 1, "tags part must count against the ceiling");
    }

    #[test]
    fn parse_error_body_reads_the_v1_ceiling_envelope() {
        let body = serde_json::json!({
            "statusMessage": "File exceeds maximum size of 75MB",
            "data": {
                "reason": "file_too_large_for_v1",
                "uploadSessionId": "upl_1",
                "ceiling": 78_643_200u64,
                "declaredSize": 125_829_120u64,
                "migration": "https://gaffer.sh/install.sh",
            },
        })
        .to_string();
        let envelope = parse_error_body(&body);
        assert_eq!(envelope.ceiling, Some(78_643_200));
        assert_eq!(envelope.upload_session_id.as_deref(), Some("upl_1"));
        assert!(envelope.message.contains("75MB"));
    }

    #[test]
    fn parse_error_body_tolerates_a_non_json_body() {
        let envelope = parse_error_body("413 Payload Too Large");
        assert_eq!(envelope.ceiling, None);
        assert_eq!(envelope.message, "413 Payload Too Large");
    }

    #[test]
    fn upload_error_exit_codes() {
        assert_eq!(UploadError::user("k", "x").exit_code(), 1);
        assert_eq!(UploadError::server("k", "x").exit_code(), 2);
        assert_eq!(UploadError::unexpected("k", "x").exit_code(), 3);
    }
}
