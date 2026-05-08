//! `gaffer upload` — direct upload of test reports / artifacts to the Gaffer
//! dashboard. Backs the `gaffer-uploader@v2` GitHub Action via prebuilt
//! binaries (the Action shrinks to a thin install + invoke wrapper).
//!
//! Routing and retry behavior live in `gaffer_core::upload`; this module is
//! only the clap surface, the structured-error renderer, and the tokio
//! runtime entry point.

use std::path::PathBuf;

use gaffer_core::upload::{self, UploadConfig, UploadError, UploadOutcome};

/// CLI flag bag — kebab-case names mirror the v1 GitHub Action input names
/// 1:1 so the v2 Action wrapper can pass them through without renaming.
pub struct UploadArgs {
    pub paths: Vec<PathBuf>,
    pub token: Option<String>,
    pub api_url: Option<String>,
    pub commit_sha: Option<String>,
    pub branch: Option<String>,
    pub test_framework: Option<String>,
    pub test_suite: Option<String>,
    pub timeout_secs: u64,
    pub max_file_size_mb: u64,
    pub debug: bool,
}

/// Run the upload. Returns a process exit code:
/// * `0` — success.
/// * `1` — user error (missing token, file too large, path not found).
/// * `2` — server error (4xx/5xx, network).
/// * `3` — unexpected (panic in a part task, JSON parse, runtime init).
pub fn run(args: UploadArgs) -> i32 {
    let token = match resolve_token(args.token) {
        Ok(t) => t,
        Err(e) => {
            emit_error(&e);
            return e.exit_code();
        }
    };

    let config = UploadConfig {
        token,
        api_url: args
            .api_url
            .or_else(|| std::env::var("GAFFER_API_URL").ok()),
        commit_sha: args.commit_sha,
        branch: args.branch,
        test_framework: args.test_framework,
        test_suite: args.test_suite,
        timeout_secs: args.timeout_secs,
        max_file_size_mb: args.max_file_size_mb,
        debug: args.debug,
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let err = UploadError::unexpected(
                "runtime_init",
                format!("Failed to start tokio runtime: {}", e),
            );
            emit_error(&err);
            return err.exit_code();
        }
    };

    match runtime.block_on(upload::run(&args.paths, &config)) {
        Ok(outcome) => {
            emit_success(&outcome, config.debug);
            0
        }
        Err(err) => {
            emit_error(&err);
            err.exit_code()
        }
    }
}

fn resolve_token(flag: Option<String>) -> Result<String, UploadError> {
    if let Some(t) = flag.filter(|s| !s.is_empty()) {
        return Ok(t);
    }
    // Honour both env var names — the GitHub Action exposes
    // GAFFER_UPLOAD_TOKEN; legacy scripts may have GAFFER_TOKEN.
    if let Ok(t) = std::env::var("GAFFER_UPLOAD_TOKEN") {
        if !t.is_empty() {
            return Ok(t);
        }
    }
    if let Ok(t) = std::env::var("GAFFER_TOKEN") {
        if !t.is_empty() {
            return Ok(t);
        }
    }
    Err(UploadError::user(
        "missing_token",
        "Upload token not provided. Pass --token or set GAFFER_UPLOAD_TOKEN.",
    ))
}

fn emit_success(outcome: &UploadOutcome, debug: bool) {
    let json = serde_json::json!({
        "status": "success",
        "uploadSessionId": outcome.upload_session_id,
        "projectId": outcome.project_id,
        "filesUploaded": outcome.files_uploaded,
        "bytesUploaded": outcome.bytes_uploaded,
        "elapsedMs": outcome.elapsed_ms,
    });
    println!("{}", json);
    if debug {
        eprintln!(
            "[gaffer] uploaded {} bytes in {} responses ({:.2}s)",
            outcome.bytes_uploaded,
            outcome.responses.len(),
            (outcome.elapsed_ms as f64) / 1000.0,
        );
    }
}

/// Emit a machine-readable JSON line to stderr (one line, parseable) followed
/// by a human-readable block with problem / cause / fix / IDs. The Action
/// wrapper consumes the JSON line; humans see the block.
fn emit_error(err: &UploadError) {
    let (kind, status, sid, ray) = match err {
        UploadError::UserError { kind, .. } => (*kind, None, None, None),
        UploadError::ServerError {
            kind,
            status,
            upload_session_id,
            cf_ray_id,
            ..
        } => (
            *kind,
            *status,
            upload_session_id.clone(),
            cf_ray_id.clone(),
        ),
        UploadError::Unexpected { kind, .. } => (*kind, None, None, None),
    };

    let json = serde_json::json!({
        "status": "error",
        "kind": kind,
        "exitCode": err.exit_code(),
        "message": err.to_string(),
        "httpStatus": status,
        "uploadSessionId": sid,
        "cfRayId": ray,
    });
    eprintln!("{}", json);

    eprintln!();
    eprintln!("Upload failed.");
    eprintln!("  Problem: {}", err);
    eprintln!("  Cause:   {}", classify_cause(err));
    eprintln!("  Fix:     {}", suggested_fix(err));
    if let Some(sid) = sid {
        eprintln!("  Upload session: {}", sid);
    }
    if let Some(ray) = ray {
        eprintln!("  Cloudflare Ray ID: {}", ray);
    }
}

fn classify_cause(err: &UploadError) -> &'static str {
    match err {
        UploadError::UserError { kind, .. } => match *kind {
            "missing_token" => "no upload token was supplied",
            "no_files" => "the path did not resolve to any files",
            "file_too_large" => "a file exceeds the configured per-file limit",
            "path_not_found" => "the path does not exist or is unreadable",
            "path_walk_failed" => "directory walk failed",
            "path_unsupported" => "path is neither a regular file nor a directory",
            "io_error" => "local filesystem error reading the file",
            _ => "client-side validation rejected the request",
        },
        UploadError::ServerError { kind, status, .. } => match *kind {
            "auth_failed" => "the dashboard rejected the upload token",
            "quota_exceeded" => "the org's storage quota would be exceeded",
            "file_too_large" => "the dashboard reported the file as too large",
            "server_error" => "the dashboard returned a transient error after retries",
            "network" => "network connectivity to the dashboard failed",
            "empty_response" => "the dashboard returned a 2xx with no body — likely a transport hiccup; the upload may have succeeded",
            _ => match status {
                Some(c) if (500u16..600).contains(c) => "server-side failure after retries",
                Some(_) => "the dashboard rejected the request",
                None => "transport-layer failure",
            },
        },
        UploadError::Unexpected { .. } => "an unexpected internal error occurred",
    }
}

fn suggested_fix(err: &UploadError) -> &'static str {
    match err {
        UploadError::UserError { kind, .. } => match *kind {
            "missing_token" => "pass --token <gfr_…> or set GAFFER_UPLOAD_TOKEN",
            "no_files" => "verify the path or glob expands to at least one file",
            "file_too_large" => "raise --max-file-size-mb or split the file",
            "path_not_found" => "double-check the path and CI working directory",
            "io_error" => "verify the file is readable and not in use",
            _ => "review the error message and adjust the invocation",
        },
        UploadError::ServerError { kind, .. } => match *kind {
            "auth_failed" => "regenerate the upload token in the project's settings",
            "quota_exceeded" => "upgrade the plan or remove old reports to free space",
            "file_too_large" => "the server's MPU ceiling is 5 GB — split the upload",
            "server_error" => "retry; if it persists, contact support with the Ray ID",
            "network" => "retry from a host with network access to the dashboard",
            "empty_response" => "check the dashboard for the upload session before retrying — the upload may already have landed",
            _ => "retry; if it persists, share the JSON error line with support",
        },
        UploadError::Unexpected { .. } => "rerun with --debug and share the full output with support",
    }
}
