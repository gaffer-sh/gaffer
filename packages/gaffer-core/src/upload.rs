//! Upload — canonical Rust client for the Gaffer dashboard upload endpoints.
//!
//! Backs the `gaffer upload` CLI command and (via the prebuilt binary) the
//! `gaffer-uploader@v2` GitHub Action. Two paths share one entry point:
//!
//! * **Small bundle** — `multipart/form-data` POST to `/api/upload`, used when
//!   no individual file exceeds 90 MB AND the cumulative size of the bundle
//!   stays under 200 MB. The 90 MB ceiling matches Cloudflare's Free zone
//!   per-request body cap; the 200 MB total keeps the streamed upload well
//!   under the same edge cap with margin for form encoding overhead.
//!
//! * **Multipart upload (MPU)** — used per-large-file when any single file
//!   exceeds 90 MB. Drives the `/api/upload/mpu/{create,part,complete,abort}`
//!   endpoints landed in TASK-39. Part PUTs run in parallel under a tokio
//!   `Semaphore(8)` so a single 5 GB upload doesn't fan out unbounded.
//!
//! Errors surface as `UploadError` so the CLI command can render a structured
//! JSON line + human-readable block on failure (see `commands/upload.rs`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt};
use reqwest::{multipart, Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio::sync::Semaphore;

const DEFAULT_API_URL: &str = "https://app.gaffer.sh";

/// Files larger than this go through MPU. Matches the Cloudflare Free zone
/// per-request body cap (~100 MiB) with a margin to keep us safely under.
pub const SMALL_FILE_THRESHOLD: u64 = 90 * 1024 * 1024;

/// Bundles whose cumulative size exceeds this get split rather than going to
/// `/api/upload` as a single request.
pub const SMALL_BUNDLE_TOTAL_THRESHOLD: u64 = 200 * 1024 * 1024;

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
    /// One entry per `/api/upload` or `/api/upload/mpu/complete` call. The
    /// CLI prints the first one's session id as `test_run_id` for v1 parity.
    pub responses: Vec<ServerUploadResponse>,
}

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
/// Routing rules (per AC #1 of TASK-40):
/// * Largest individual file `< 90 MB` AND total `< 200 MB`
///   → single `multipart/form-data` POST to `/api/upload`.
/// * Otherwise: each file `>= 90 MB` takes its own MPU cycle, and remaining
///   small files are bundled into one or more `/api/upload` calls (split if
///   the bundle would itself exceed the 200 MB threshold).
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
    let largest = files.iter().map(|f| f.size).max().unwrap_or(0);
    let total_files = files.len();

    let client = build_http_client(config.timeout_secs)?;
    let mut responses = Vec::new();

    if largest < SMALL_FILE_THRESHOLD && total_bytes < SMALL_BUNDLE_TOTAL_THRESHOLD {
        let resp = upload_small_bundle(&client, config, &files).await?;
        responses.push(resp);
    } else {
        let (large_files, small_files): (Vec<_>, Vec<_>) = files
            .into_iter()
            .partition(|f| f.size >= SMALL_FILE_THRESHOLD);

        for large in &large_files {
            let resp = upload_mpu(&client, config, large).await?;
            responses.push(resp);
        }

        // Pack remaining small files into bundles capped at the 200 MB
        // single-request threshold. One bundle in the common case; multiple
        // only when there's enough small-file payload to overflow it.
        for bundle in pack_small_bundles(small_files) {
            if bundle.is_empty() {
                continue;
            }
            let resp = upload_small_bundle(&client, config, &bundle).await?;
            responses.push(resp);
        }
    }

    let elapsed_ms = start.elapsed().as_millis();
    let primary_session_id = responses
        .first()
        .map(|r| r.upload_session.id.clone())
        .unwrap_or_default();
    let primary_project_id = responses
        .first()
        .and_then(|r| r.upload_session.project_id.clone());

    Ok(UploadOutcome {
        upload_session_id: primary_session_id,
        project_id: primary_project_id,
        files_uploaded: total_files,
        bytes_uploaded: total_bytes,
        elapsed_ms,
        responses,
    })
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

/// Pack small files into ≤200 MB bundles.
///
/// First-fit decreasing: sort largest-first, then drop each file into the
/// first bundle that has room; otherwise start a new one. Common case is one
/// bundle — this only matters when a project ships many small files whose
/// cumulative size exceeds `SMALL_BUNDLE_TOTAL_THRESHOLD`.
fn pack_small_bundles(mut files: Vec<UploadFile>) -> Vec<Vec<UploadFile>> {
    files.sort_by(|a, b| b.size.cmp(&a.size));
    let mut bundles: Vec<(u64, Vec<UploadFile>)> = Vec::new();
    for f in files {
        let mut placed = false;
        for (sz, bucket) in bundles.iter_mut() {
            if *sz + f.size < SMALL_BUNDLE_TOTAL_THRESHOLD {
                *sz += f.size;
                bucket.push(f.clone());
                placed = true;
                break;
            }
        }
        if !placed {
            bundles.push((f.size, vec![f]));
        }
    }
    bundles.into_iter().map(|(_, b)| b).collect()
}

// ----- HTTP client ---------------------------------------------------------

fn build_http_client(timeout_secs: u64) -> UploadResult<Client> {
    Client::builder()
        // The per-request timeout governs each individual HTTP call. A large
        // MPU upload is many calls, so this is *not* a wall-clock budget for
        // the whole upload — it's the deadline for any single round-trip.
        .timeout(Duration::from_secs(timeout_secs))
        .user_agent(format!("gaffer-cli/{}", env!("CARGO_PKG_VERSION")))
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

async fn upload_small_bundle(
    client: &Client,
    config: &UploadConfig,
    files: &[UploadFile],
) -> UploadResult<ServerUploadResponse> {
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

        let req = client
            .post(&url)
            .header("X-API-Key", &config.token)
            .multipart(form);

        if config.debug {
            eprintln!("[gaffer] POST {} ({} files)", url, files.len());
        }

        req.send().await.map_err(reqwest_to_upload_error)
    };

    let resp = send_with_retry(send, config.debug, "POST /api/upload").await?;
    parse_upload_response(resp).await
}

// ----- MPU path ------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MpuCreateRequest<'a> {
    file_name: &'a str,
    file_size: u64,
    tags: serde_json::Value,
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

async fn upload_mpu(
    client: &Client,
    config: &UploadConfig,
    file: &UploadFile,
) -> UploadResult<ServerUploadResponse> {
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

    parse_upload_response(complete_resp).await
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
    let (message, sid) = parse_error_body(&body);
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
    if let Some(sid) = sid {
        err = err.with_session(sid);
    }
    if let Some(ray) = cf_ray {
        err = err.with_ray(ray);
    }
    err
}

/// Try to extract a human message + uploadSessionId from the H3 error envelope
/// or from a free-form error body. Falls back to the raw body on parse fail.
fn parse_error_body(body: &str) -> (String, Option<String>) {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        let msg = v
            .get("statusMessage")
            .or_else(|| v.get("message"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| body.to_string());
        let sid = v
            .pointer("/data/uploadSessionId")
            .or_else(|| v.get("uploadSessionId"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());
        return (msg, sid);
    }
    (body.to_string(), None)
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
            debug: false,
        }
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
        let files = enumerate_files(&[file.clone()], 100).unwrap();
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
    fn pack_small_bundles_keeps_under_threshold() {
        let mk = |size| UploadFile {
            absolute_path: PathBuf::new(),
            relative_path: format!("f{}", size),
            size,
        };
        let files = vec![
            mk(150 * 1024 * 1024), // 150 MB
            mk(150 * 1024 * 1024), // 150 MB
            mk(10 * 1024 * 1024),  // 10 MB
        ];
        let bundles = pack_small_bundles(files);
        assert!(bundles.iter().all(|b| {
            let sz: u64 = b.iter().map(|f| f.size).sum();
            sz < SMALL_BUNDLE_TOTAL_THRESHOLD
        }));
        // Two big files cannot share a bundle.
        assert_eq!(bundles.len(), 2);
    }

    #[test]
    fn upload_error_exit_codes() {
        assert_eq!(UploadError::user("k", "x").exit_code(), 1);
        assert_eq!(UploadError::server("k", "x").exit_code(), 2);
        assert_eq!(UploadError::unexpected("k", "x").exit_code(), 3);
    }
}
