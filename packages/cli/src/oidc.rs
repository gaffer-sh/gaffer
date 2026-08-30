//! GitHub Actions OIDC -> Gaffer project token exchange (RFC 8693).
//!
//! A workflow that grants `permissions: id-token: write` gets a short-lived
//! runner identity token for free, with no secret to store or rotate. Both
//! `resolve_token` (upload.rs) and `Config::resolve` (config.rs) fall back
//! here when no `--token` / `GAFFER_*_TOKEN` is configured, so `gaffer
//! upload` and `gaffer test` authenticate automatically on GitHub Actions.
//!
//! Two hops:
//!  1. GET `$ACTIONS_ID_TOKEN_REQUEST_URL?audience=<origin>`, bearer
//!     `$ACTIONS_ID_TOKEN_REQUEST_TOKEN` -> `{ "value": "<jwt>" }`. `origin`
//!     is sent with no trailing slash. The server matches it exactly
//!     (trailing slash tolerated on its side) against the `aud` claim, so
//!     the same normalized value is used here and for the POST below.
//!  2. POST `<origin>/oauth/token`, form-encoded RFC 8693 token exchange ->
//!     `{ "access_token": "gfr_...", "claimed": bool, "claim_url": "..."? }`.
//!     A `429` (`slow_down`) or `500` (`server_error`) response is retried a
//!     couple of times with a short backoff; any `400` (`invalid_grant`,
//!     `invalid_request`, `invalid_target`, `unsupported_grant_type`) is
//!     terminal: the request itself is wrong, so retrying changes nothing.
//!
//! `gaffer-core`'s HTTP layer is untouched: the exchanged `access_token` is
//! just another `gfr_` project token, sent via `X-API-Key` like any other.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;

/// Matches `gaffer-core`'s own `DEFAULT_API_URL` constants (sync.rs, upload.rs).
const DEFAULT_API_URL: &str = "https://app.gaffer.sh";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Retries after a transient exchange failure (429 rate limit, 500 server
/// error). Total attempts = 1 + this. Short backoff: this runs inline in a
/// CI job, so it shouldn't burn much wall-clock time on a flaky server.
const EXCHANGE_MAX_RETRIES: u32 = 2;
const EXCHANGE_RETRY_BACKOFF: Duration = Duration::from_millis(300);

const REQUEST_URL_VAR: &str = "ACTIONS_ID_TOKEN_REQUEST_URL";
const REQUEST_TOKEN_VAR: &str = "ACTIONS_ID_TOKEN_REQUEST_TOKEN";

/// Serializes tests that mutate the OIDC/token env vars so they don't race
/// each other across modules (`oidc::tests` and `commands::upload::tests`
/// both touch these). Only compiled for test builds.
#[cfg(test)]
pub(crate) static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A successfully exchanged project token, plus the org-claim metadata the
/// server returns so the caller can print a claim hint for unclaimed orgs.
#[derive(Debug, PartialEq, Eq)]
pub struct ExchangedToken {
    pub access_token: String,
    pub claimed: bool,
    pub claim_url: Option<String>,
}

/// Failure of a *committed* exchange attempt (the OIDC env vars were
/// present, so we tried). Does not cover "OIDC not configured", which is
/// `try_exchange` returning `None`, not an error.
#[derive(Debug)]
pub enum OidcError {
    IdTokenFetch(String),
    Exchange(String),
}

impl fmt::Display for OidcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OidcError::IdTokenFetch(msg) => {
                write!(f, "failed to fetch the GitHub Actions runner ID token: {msg}")
            }
            OidcError::Exchange(msg) => {
                write!(f, "failed to exchange the OIDC token for a Gaffer project token: {msg}")
            }
        }
    }
}

#[derive(Deserialize)]
struct IdTokenResponse {
    value: String,
}

#[derive(Deserialize)]
struct ExchangeResponse {
    access_token: String,
    #[serde(default)]
    claimed: bool,
    #[serde(default)]
    claim_url: Option<String>,
}

#[derive(Deserialize)]
struct ExchangeErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

/// Try the GitHub Actions OIDC fallback.
///
/// Returns `None` when the runner has no OIDC token to hand out. That's the
/// ordinary case outside CI, or a workflow missing `permissions: id-token:
/// write`. That's not a failure, just "this fallback doesn't apply", so
/// callers should fall through to their existing missing-token handling.
///
/// Returns `Some(Err(_))` only once we've committed to the exchange (the env
/// vars were present) and it failed, so the caller can surface a specific
/// hint instead of the generic "no token" message.
pub fn try_exchange(api_url: Option<&str>) -> Option<Result<ExchangedToken, OidcError>> {
    let request_url = non_empty_env(REQUEST_URL_VAR)?;
    let request_token = non_empty_env(REQUEST_TOKEN_VAR)?;

    let origin = api_url.unwrap_or(DEFAULT_API_URL).trim_end_matches('/');
    Some(
        fetch_id_token(&request_url, &request_token, origin)
            .and_then(|id_token| exchange_for_project_token(origin, &id_token)),
    )
}

/// Print the one-line confirmation on a successful exchange, plus a claim
/// hint when the org the token belongs to hasn't been claimed yet.
pub fn print_auth_success(token: &ExchangedToken) {
    let repo = non_empty_env("GITHUB_REPOSITORY").unwrap_or_else(|| "this repository".to_string());
    eprintln!("gaffer: authenticated via GitHub Actions OIDC for {repo}");
    if !token.claimed {
        if let Some(url) = &token.claim_url {
            eprintln!("gaffer: this organization hasn't been claimed yet, claim it at {url}");
        }
    }
}

fn non_empty_env(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.is_empty())
}

fn build_agent() -> ureq::Agent {
    let config = ureq::config::Config::builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        // We read the body on error responses too (token-exchange failures
        // return a JSON `{ "error", "error_description" }` we want to
        // surface), so ask ureq to hand back non-2xx as `Ok` rather than
        // `Err(StatusCode)`.
        .http_status_as_error(false)
        .build();
    ureq::Agent::new_with_config(config)
}

/// Fetch the runner's ambient ID token, scoped to `audience`.
fn fetch_id_token(request_url: &str, bearer: &str, audience: &str) -> Result<String, OidcError> {
    let agent = build_agent();
    let mut response = agent
        .get(request_url)
        .query("audience", audience)
        .header("Authorization", format!("bearer {bearer}"))
        .call()
        .map_err(|e| OidcError::IdTokenFetch(e.to_string()))?;

    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(OidcError::IdTokenFetch(format!("HTTP {status}")));
    }

    let body: IdTokenResponse = response
        .body_mut()
        .read_json()
        .map_err(|e| OidcError::IdTokenFetch(format!("invalid response body: {e}")))?;
    Ok(body.value)
}

/// `429` (`slow_down`, rate limited) and `500` (`server_error`) are the
/// server's transient codes, worth a retry. Every other status (400 with
/// `invalid_grant` / `invalid_request` / `invalid_target` /
/// `unsupported_grant_type`) means the request itself is wrong and retrying
/// changes nothing.
fn is_retryable_exchange_status(status: u16) -> bool {
    status == 429 || status == 500
}

/// Exchange the runner ID token for a Gaffer project token via RFC 8693
/// token exchange, retrying transient (429/500) failures a couple of times
/// with a short backoff before giving up.
fn exchange_for_project_token(origin: &str, id_token: &str) -> Result<ExchangedToken, OidcError> {
    let mut attempt = 0;
    loop {
        match exchange_attempt(origin, id_token) {
            Ok(token) => return Ok(token),
            Err((Some(status), _detail)) if is_retryable_exchange_status(status) && attempt < EXCHANGE_MAX_RETRIES => {
                attempt += 1;
                std::thread::sleep(EXCHANGE_RETRY_BACKOFF);
            }
            Err((_, detail)) => return Err(OidcError::Exchange(detail)),
        }
    }
}

/// One attempt at the exchange POST. `Err` carries the HTTP status (`None`
/// for a transport-level failure, which is never retried) alongside the
/// detail message, so the retry loop above can decide what to do with it.
fn exchange_attempt(origin: &str, id_token: &str) -> Result<ExchangedToken, (Option<u16>, String)> {
    let url = format!("{origin}/oauth/token");
    let agent = build_agent();
    let mut response = agent
        .post(&url)
        .send_form([
            ("grant_type", "urn:ietf:params:oauth:grant-type:token-exchange"),
            ("subject_token", id_token),
            ("subject_token_type", "urn:ietf:params:oauth:token-type:jwt"),
            ("scope", "upload"),
        ])
        .map_err(|e| (None, e.to_string()))?;

    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let detail = match response.body_mut().read_json::<ExchangeErrorResponse>() {
            Ok(err) => match err.error_description {
                Some(desc) => format!("HTTP {status} ({}: {desc})", err.error),
                None => format!("HTTP {status} ({})", err.error),
            },
            Err(_) => format!("HTTP {status}"),
        };
        return Err((Some(status), detail));
    }

    let body: ExchangeResponse = response
        .body_mut()
        .read_json()
        .map_err(|e| (None, format!("invalid response body: {e}")))?;

    Ok(ExchangedToken {
        access_token: body.access_token,
        claimed: body.claimed,
        claim_url: body.claim_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_mock::{mock_once, mock_sequence};

    /// Serves two sequential requests (GET then POST) with a 200, so
    /// `try_exchange`'s full two-hop flow can be exercised end-to-end
    /// against a single base URL.
    fn mock_two_hops(id_token_body: String, exchange_body: String) -> String {
        mock_sequence(vec![("HTTP/1.1 200 OK", id_token_body), ("HTTP/1.1 200 OK", exchange_body)])
    }

    #[test]
    fn try_exchange_returns_none_without_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        std::env::remove_var(REQUEST_URL_VAR);
        std::env::remove_var(REQUEST_TOKEN_VAR);
        assert!(try_exchange(None).is_none());
    }

    #[test]
    fn try_exchange_returns_none_with_only_one_var_set() {
        let _lock = ENV_MUTEX.lock().unwrap();
        std::env::set_var(REQUEST_URL_VAR, "http://example.invalid/token");
        std::env::remove_var(REQUEST_TOKEN_VAR);
        let result = try_exchange(None);
        std::env::remove_var(REQUEST_URL_VAR);
        assert!(result.is_none());
    }

    #[test]
    fn try_exchange_happy_path() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let base = mock_two_hops(
            r#"{"value":"fake.jwt"}"#.to_string(),
            r#"{"access_token":"gfr_abc123","token_type":"Bearer","expires_in":3600,"scope":"upload","project_id":"p1","organization_id":"o1","claimed":false,"claim_url":"https://app.gaffer.sh/claim/xyz"}"#.to_string(),
        );
        std::env::set_var(REQUEST_URL_VAR, format!("{base}/token"));
        std::env::set_var(REQUEST_TOKEN_VAR, "runner-bearer");

        let result = try_exchange(Some(&base));

        std::env::remove_var(REQUEST_URL_VAR);
        std::env::remove_var(REQUEST_TOKEN_VAR);

        let exchanged = result.expect("OIDC env vars were set").expect("exchange should succeed");
        assert_eq!(exchanged.access_token, "gfr_abc123");
        assert!(!exchanged.claimed);
        assert_eq!(exchanged.claim_url.as_deref(), Some("https://app.gaffer.sh/claim/xyz"));
    }

    #[test]
    fn fetch_id_token_http_error() {
        let base = mock_once("HTTP/1.1 403 Forbidden", "{}".to_string());
        let err = fetch_id_token(&format!("{base}/token"), "bearer-value", "https://app.gaffer.sh")
            .expect_err("403 should be an error");
        assert!(matches!(err, OidcError::IdTokenFetch(_)));
        assert!(err.to_string().contains("HTTP 403"));
    }

    #[test]
    fn exchange_for_project_token_http_error_surfaces_body() {
        let base = mock_once(
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"invalid_grant","error_description":"subject_token is expired"}"#.to_string(),
        );
        let err = exchange_for_project_token(&base, "expired.jwt").expect_err("400 should be an error");
        assert!(matches!(err, OidcError::Exchange(_)));
        let msg = err.to_string();
        assert!(msg.contains("invalid_grant"));
        assert!(msg.contains("subject_token is expired"));
    }

    #[test]
    fn exchange_for_project_token_happy_path() {
        let base = mock_once(
            "HTTP/1.1 200 OK",
            r#"{"access_token":"gfr_xyz","token_type":"Bearer","expires_in":3600,"scope":"upload","project_id":"p1","organization_id":"o1","claimed":true}"#.to_string(),
        );
        let exchanged = exchange_for_project_token(&base, "some.jwt").expect("exchange should succeed");
        assert_eq!(exchanged.access_token, "gfr_xyz");
        assert!(exchanged.claimed);
        assert_eq!(exchanged.claim_url, None);
    }

    #[test]
    fn exchange_for_project_token_retries_429_then_succeeds() {
        let success_body = r#"{"access_token":"gfr_after_retry","token_type":"Bearer","expires_in":3600,"scope":"upload","project_id":"p1","organization_id":"o1","claimed":true}"#.to_string();
        let base = mock_sequence(vec![
            ("HTTP/1.1 429 Too Many Requests", r#"{"error":"slow_down"}"#.to_string()),
            ("HTTP/1.1 200 OK", success_body),
        ]);
        let exchanged = exchange_for_project_token(&base, "some.jwt").expect("should succeed after retry");
        assert_eq!(exchanged.access_token, "gfr_after_retry");
    }

    #[test]
    fn exchange_for_project_token_retries_500_then_succeeds() {
        let success_body = r#"{"access_token":"gfr_after_500","token_type":"Bearer","expires_in":3600,"scope":"upload","project_id":"p1","organization_id":"o1","claimed":true}"#.to_string();
        let base = mock_sequence(vec![
            ("HTTP/1.1 500 Internal Server Error", r#"{"error":"server_error","error_description":"transient"}"#.to_string()),
            ("HTTP/1.1 200 OK", success_body),
        ]);
        let exchanged = exchange_for_project_token(&base, "some.jwt").expect("should succeed after retry");
        assert_eq!(exchanged.access_token, "gfr_after_500");
    }

    #[test]
    fn exchange_for_project_token_gives_up_after_exhausting_retries() {
        // 1 initial attempt + EXCHANGE_MAX_RETRIES retries, all 500. The mock
        // only queues that many responses, so an extra retry attempt would
        // hit connection-refused instead of the terminal message we assert on.
        let responses = (0..=EXCHANGE_MAX_RETRIES)
            .map(|_| ("HTTP/1.1 500 Internal Server Error", r#"{"error":"server_error"}"#.to_string()))
            .collect();
        let base = mock_sequence(responses);
        let err = exchange_for_project_token(&base, "some.jwt").expect_err("500s should exhaust retries");
        assert!(err.to_string().contains("HTTP 500"));
    }

    #[test]
    fn exchange_for_project_token_does_not_retry_terminal_400() {
        // Only one response queued; a mistaken retry would hit connection
        // refused, which wouldn't contain "invalid_grant" below.
        let base = mock_once(
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"invalid_grant","error_description":"expired"}"#.to_string(),
        );
        let err = exchange_for_project_token(&base, "some.jwt").expect_err("400 should be terminal");
        assert!(err.to_string().contains("invalid_grant"));
    }
}
