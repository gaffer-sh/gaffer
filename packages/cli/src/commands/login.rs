//! `gaffer login`: the RFC 8628 device authorization grant.
//!
//! The CLI has no browser and no client secret. It asks
//! `POST /oauth/device_authorization` for a pair of codes, prints the short one
//! plus a URL, opens the browser, and then polls `POST /oauth/token` with
//! `grant_type=urn:ietf:params:oauth:grant-type:device_code` until a human
//! approves. The credential that comes back is a normal Gaffer token: a `gfr_`
//! project token for `scope=upload`, or a `gaf_` user API key for `scope=read`.
//!
//! `gaffer init` uses the same flow, falling back to the older
//! `/api/v1/cli/{setup,token}` endpoints when the device endpoint 404s, so a
//! new CLI still works against a deployment that predates GAF-245.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use colored::Colorize;
use serde::Deserialize;

use crate::config;

/// Matches `gaffer-core`'s own `DEFAULT_API_URL` constants (sync.rs, upload.rs).
const DEFAULT_API_URL: &str = "https://app.gaffer.sh";

/// Public client id this authorization server issues device codes for.
const CLIENT_ID: &str = "gaffer-cli";
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEVICE_AUTH_PATH: &str = "/oauth/device_authorization";
const TOKEN_PATH: &str = "/oauth/token";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Scope that mints a project-scoped `gfr_` upload token. What `gaffer init`
/// has always produced, and the default for `gaffer login`.
pub const UPLOAD_SCOPE: &str = "upload";

/// Fallback poll interval when the server doesn't advertise one.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);

/// RFC 8628 §3.5: a `slow_down` response means add 5 seconds to the interval.
const SLOW_DOWN_INCREMENT: Duration = Duration::from_secs(5);

/// Ceiling on the whole poll loop, used when the server sends no `expires_in`.
const DEFAULT_LIFETIME: Duration = Duration::from_secs(300);

/// Consecutive transport/5xx failures tolerated before giving up. The flow is
/// long-lived and a single blip shouldn't cost the user their approval.
const MAX_CONSECUTIVE_ERRORS: u32 = 5;

/// RFC 8628 §3.2 device authorization response.
#[derive(Debug, Deserialize)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub interval: Option<u64>,
}

impl DeviceAuthorization {
    /// The URL to send the human to, preferring the one that pre-fills the code.
    pub fn browser_url(&self) -> &str {
        self.verification_uri_complete
            .as_deref()
            .unwrap_or(&self.verification_uri)
    }

    fn poll_config(&self) -> PollConfig {
        PollConfig {
            interval: self.interval.map_or(DEFAULT_INTERVAL, Duration::from_secs),
            slow_down_increment: SLOW_DOWN_INCREMENT,
            deadline: Instant::now()
                + self.expires_in.map_or(DEFAULT_LIFETIME, Duration::from_secs),
        }
    }
}

/// The credential a completed device flow yields.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct DeviceCredential {
    pub access_token: String,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub organization_id: Option<String>,
}

/// Why a device flow didn't produce a credential.
#[derive(Debug)]
pub enum DeviceFlowError {
    /// The deployment predates the device endpoint. Callers that have an older
    /// flow available should use it instead of surfacing an error.
    Unsupported,
    Failed(anyhow::Error),
}

impl DeviceFlowError {
    fn failed(message: impl Into<String>) -> Self {
        DeviceFlowError::Failed(anyhow::anyhow!(message.into()))
    }
}

impl std::fmt::Display for DeviceFlowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceFlowError::Unsupported => {
                write!(f, "this Gaffer deployment does not support the device authorization flow")
            }
            DeviceFlowError::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

/// How aggressively to poll. Split out so tests can drive the loop without
/// sleeping for real seconds.
pub struct PollConfig {
    pub interval: Duration,
    pub slow_down_increment: Duration,
    pub deadline: Instant,
}

#[derive(Deserialize)]
struct OAuthErrorBody {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

impl OAuthErrorBody {
    fn describe(&self) -> String {
        match &self.error_description {
            Some(desc) => format!("{} ({desc})", self.error),
            None => self.error.clone(),
        }
    }
}

/// One poll's outcome, before it is turned into a loop decision.
enum PollOutcome {
    Granted(Box<DeviceCredential>),
    Pending,
    SlowDown,
    Terminal(String),
    Transient(String),
}

fn build_agent() -> ureq::Agent {
    let config = ureq::config::Config::builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        // Error responses carry the RFC 6749 `{ error, error_description }`
        // body we need to read, so non-2xx must come back as `Ok`.
        .http_status_as_error(false)
        .build();
    ureq::Agent::new_with_config(config)
}

/// Ask the authorization server for a device code and user code.
///
/// A `404` means the deployment has no device endpoint, which is a fallback
/// signal rather than a failure, so it gets its own variant.
pub fn request_device_authorization(
    origin: &str,
    scope: &str,
) -> Result<DeviceAuthorization, DeviceFlowError> {
    let url = format!("{}{DEVICE_AUTH_PATH}", origin.trim_end_matches('/'));
    let mut response = build_agent()
        .post(&url)
        .send_form([("client_id", CLIENT_ID), ("scope", scope)])
        .map_err(|e| DeviceFlowError::failed(e.to_string()))?;

    let status = response.status().as_u16();
    if status == 404 {
        return Err(DeviceFlowError::Unsupported);
    }
    if !(200..300).contains(&status) {
        let detail = match response.body_mut().read_json::<OAuthErrorBody>() {
            Ok(err) => format!("HTTP {status} ({})", err.describe()),
            Err(_) => format!("HTTP {status}"),
        };
        return Err(DeviceFlowError::failed(detail));
    }

    response
        .body_mut()
        .read_json()
        .map_err(|e| DeviceFlowError::failed(format!("invalid device authorization response: {e}")))
}

/// Classify one `POST /oauth/token` attempt.
fn poll_once(origin: &str, device_code: &str) -> PollOutcome {
    let url = format!("{}{TOKEN_PATH}", origin.trim_end_matches('/'));
    let mut response = match build_agent().post(&url).send_form([
        ("grant_type", GRANT_TYPE),
        ("device_code", device_code),
        ("client_id", CLIENT_ID),
    ]) {
        Ok(response) => response,
        Err(e) => return PollOutcome::Transient(e.to_string()),
    };

    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return match response.body_mut().read_json::<DeviceCredential>() {
            Ok(credential) => PollOutcome::Granted(Box::new(credential)),
            Err(e) => PollOutcome::Terminal(format!("invalid token response: {e}")),
        };
    }

    let Ok(body) = response.body_mut().read_json::<OAuthErrorBody>() else {
        return PollOutcome::Transient(format!("HTTP {status}"));
    };

    match body.error.as_str() {
        "authorization_pending" => PollOutcome::Pending,
        "slow_down" => PollOutcome::SlowDown,
        "expired_token" => PollOutcome::Terminal(
            "the login request expired before it was approved".to_string(),
        ),
        "access_denied" => PollOutcome::Terminal("the login was denied in the browser".to_string()),
        _ if status >= 500 => PollOutcome::Transient(body.describe()),
        _ => PollOutcome::Terminal(body.describe()),
    }
}

/// Poll the token endpoint until the request is approved, denied, or expires.
pub fn poll_for_credential(
    origin: &str,
    device_code: &str,
    mut config: PollConfig,
) -> Result<DeviceCredential, DeviceFlowError> {
    let mut consecutive_errors: u32 = 0;

    loop {
        if Instant::now() >= config.deadline {
            return Err(DeviceFlowError::failed(
                "timed out waiting for the login to be approved",
            ));
        }

        thread::sleep(config.interval);

        match poll_once(origin, device_code) {
            PollOutcome::Granted(credential) => return Ok(*credential),
            PollOutcome::Pending => consecutive_errors = 0,
            PollOutcome::SlowDown => {
                consecutive_errors = 0;
                config.interval += config.slow_down_increment;
            }
            PollOutcome::Terminal(detail) => return Err(DeviceFlowError::failed(detail)),
            PollOutcome::Transient(detail) => {
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    return Err(DeviceFlowError::failed(format!(
                        "gave up after {consecutive_errors} consecutive failures: {detail}"
                    )));
                }
            }
        }
    }
}

/// Run the whole flow: request codes, show them, open a browser, poll.
///
/// Used by both `gaffer login` and `gaffer init`; the caller decides what to do
/// with the credential and how to react to `DeviceFlowError::Unsupported`.
pub fn authenticate(origin: &str, scope: &str) -> Result<DeviceCredential, DeviceFlowError> {
    let authorization = request_device_authorization(origin, scope)?;
    let browser_url = authorization.browser_url().to_string();

    println!();
    println!("  {} {}", "Verify code:".bold(), authorization.user_code.bold());
    println!("  Opening browser to authenticate...");
    println!("  {}", browser_url.dimmed());

    if let Err(e) = open::that(&browser_url) {
        eprintln!(
            "  {} Could not open browser: {}\n  Open this URL manually: {}",
            "Warning:".yellow().bold(),
            e,
            browser_url
        );
    }

    println!("  Waiting for approval...");
    poll_for_credential(origin, &authorization.device_code, authorization.poll_config())
}

/// `gaffer login`: authenticate this machine and store the credential in the
/// global config, so `gaffer test` works from any directory.
pub fn run(api_url: Option<&str>, scope: &str) -> Result<()> {
    let api_base = api_url.unwrap_or(DEFAULT_API_URL);

    let credential = authenticate(api_base, scope).map_err(|e| match e {
        DeviceFlowError::Unsupported => anyhow::anyhow!(
            "{e}. Upgrade the deployment, or run `gaffer init` which falls back to the older flow."
        ),
        DeviceFlowError::Failed(inner) => inner,
    })?;

    let persist_api_url = if api_base != DEFAULT_API_URL { Some(api_base) } else { None };
    config::write_global_config(Some(&credential.access_token), persist_api_url)
        .context("Failed to write global config")?;

    let global_path = config::global_config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.config/gaffer/config.toml".to_string());

    println!();
    println!("  {} {}", "Authenticated:".green().bold(), global_path);
    if let Some(project_id) = &credential.project_id {
        println!("  {} {}", "Project:".bold(), project_id);
    }
    println!(
        "  Run {} from any directory to capture and sync test results.",
        "gaffer test -- <your test command>".bold()
    );
    println!();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_mock::{mock_once, mock_sequence};

    const AUTH_BODY: &str = r#"{"device_code":"dev_abc","user_code":"WDJB4MJK","verification_uri":"https://app.gaffer.sh/cli/setup","verification_uri_complete":"https://app.gaffer.sh/cli/setup?code=WDJB4MJK","expires_in":300,"interval":5}"#;
    const GRANTED_BODY: &str = r#"{"access_token":"gfr_abc123","token_type":"Bearer","scope":"upload:prj_1","project_id":"prj_1","organization_id":"org_1"}"#;

    /// Poll immediately and forever, so tests exercise the loop without sleeping.
    fn fast_poll() -> PollConfig {
        PollConfig {
            interval: Duration::ZERO,
            slow_down_increment: Duration::ZERO,
            deadline: Instant::now() + Duration::from_secs(30),
        }
    }

    #[test]
    fn request_device_authorization_happy_path() {
        let base = mock_once("HTTP/1.1 200 OK", AUTH_BODY.to_string());
        let auth = request_device_authorization(&base, UPLOAD_SCOPE).expect("should succeed");
        assert_eq!(auth.device_code, "dev_abc");
        assert_eq!(auth.user_code, "WDJB4MJK");
        assert_eq!(auth.browser_url(), "https://app.gaffer.sh/cli/setup?code=WDJB4MJK");
        assert_eq!(auth.interval, Some(5));
    }

    #[test]
    fn request_device_authorization_404_reports_unsupported() {
        let base = mock_once("HTTP/1.1 404 Not Found", "{}".to_string());
        let err = request_device_authorization(&base, UPLOAD_SCOPE).expect_err("404 is an error");
        assert!(matches!(err, DeviceFlowError::Unsupported));
    }

    #[test]
    fn request_device_authorization_surfaces_error_body() {
        let base = mock_once(
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"invalid_client","error_description":"Unknown client_id: nope"}"#.to_string(),
        );
        let err = request_device_authorization(&base, UPLOAD_SCOPE).expect_err("400 is an error");
        let msg = err.to_string();
        assert!(msg.contains("invalid_client"), "{msg}");
        assert!(msg.contains("Unknown client_id"), "{msg}");
    }

    #[test]
    fn poll_returns_the_credential_on_first_success() {
        let base = mock_once("HTTP/1.1 200 OK", GRANTED_BODY.to_string());
        let credential = poll_for_credential(&base, "dev_abc", fast_poll()).expect("should grant");
        assert_eq!(credential.access_token, "gfr_abc123");
        assert_eq!(credential.project_id.as_deref(), Some("prj_1"));
        assert_eq!(credential.organization_id.as_deref(), Some("org_1"));
    }

    #[test]
    fn poll_keeps_going_while_authorization_is_pending() {
        let base = mock_sequence(vec![
            ("HTTP/1.1 400 Bad Request", r#"{"error":"authorization_pending"}"#.to_string()),
            ("HTTP/1.1 400 Bad Request", r#"{"error":"authorization_pending"}"#.to_string()),
            ("HTTP/1.1 200 OK", GRANTED_BODY.to_string()),
        ]);
        let credential = poll_for_credential(&base, "dev_abc", fast_poll()).expect("should grant");
        assert_eq!(credential.access_token, "gfr_abc123");
    }

    #[test]
    fn poll_backs_off_on_slow_down_and_still_succeeds() {
        let base = mock_sequence(vec![
            ("HTTP/1.1 400 Bad Request", r#"{"error":"slow_down"}"#.to_string()),
            ("HTTP/1.1 200 OK", GRANTED_BODY.to_string()),
        ]);
        let mut config = fast_poll();
        config.slow_down_increment = Duration::from_millis(1);

        let credential = poll_for_credential(&base, "dev_abc", config).expect("should grant");
        assert_eq!(credential.access_token, "gfr_abc123");
    }

    #[test]
    fn poll_stops_on_expired_token() {
        // Only one response queued: a retry would hit connection refused, whose
        // message would not mention expiry.
        let base = mock_once("HTTP/1.1 400 Bad Request", r#"{"error":"expired_token"}"#.to_string());
        let err = poll_for_credential(&base, "dev_abc", fast_poll()).expect_err("should stop");
        assert!(err.to_string().contains("expired"), "{err}");
    }

    #[test]
    fn poll_stops_on_access_denied() {
        let base = mock_once("HTTP/1.1 400 Bad Request", r#"{"error":"access_denied"}"#.to_string());
        let err = poll_for_credential(&base, "dev_abc", fast_poll()).expect_err("should stop");
        assert!(err.to_string().contains("denied"), "{err}");
    }

    #[test]
    fn poll_stops_on_invalid_grant_from_a_replayed_device_code() {
        let base = mock_once(
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"invalid_grant","error_description":"Unknown device code"}"#.to_string(),
        );
        let err = poll_for_credential(&base, "dev_abc", fast_poll()).expect_err("should stop");
        assert!(err.to_string().contains("invalid_grant"), "{err}");
    }

    #[test]
    fn poll_gives_up_after_consecutive_transport_failures() {
        // No listener at all: every attempt is connection refused.
        let base = "http://127.0.0.1:1".to_string();
        let err = poll_for_credential(&base, "dev_abc", fast_poll()).expect_err("should give up");
        assert!(err.to_string().contains("consecutive failures"), "{err}");
    }
}
