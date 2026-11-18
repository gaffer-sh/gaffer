//! Network requests and correlation IDs from a Playwright trace.
//!
//! Not a report parser, so it does not go through `ParserRegistry`. The zip is
//! opened on the JS side (`trace-network.js`, fflate) because the crate has no
//! inflate dependency; this module gets the decompressed text.

use serde::{Deserialize, Serialize};

/// Trace format versions (`context-options.version`) whose `.network` shape we
/// have fixtures for: 7 is Playwright 1.49, 8 is 1.57.
const KNOWN_TRACE_VERSIONS: [u64; 2] = [7, 8];

/// Headers that carry an ID a backend log line may also carry.
const ID_HEADERS: [&str; 7] = [
    "traceparent",
    "x-request-id",
    "x-trace-id",
    "x-correlation-id",
    "x-datadog-trace-id",
    "sentry-trace",
    "x-gaffer-test-id",
];

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TraceNetwork {
    pub playwright_version: Option<String>,
    pub requests: Vec<TraceRequest>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TraceRequest {
    pub method: String,
    pub url: String,
    /// 0 for a network error (Playwright writes -1).
    pub status: u16,
    /// `startedDateTime` verbatim; Playwright writes it with `toISOString()`.
    pub started_at: String,
    pub duration_ms: f64,
    pub ids: Vec<TraceId>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct TraceId {
    pub header: String,
    pub value: String,
    pub on: &'static str,
}

#[derive(Deserialize)]
struct ContextOptions {
    #[serde(rename = "type")]
    kind: String,
    version: Option<u64>,
    #[serde(rename = "playwrightVersion")]
    playwright_version: Option<String>,
}

#[derive(Deserialize)]
struct NetworkLine {
    #[serde(rename = "type")]
    kind: String,
    snapshot: Option<HarEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HarEntry {
    started_date_time: String,
    time: f64,
    request: HarRequest,
    response: HarResponse,
}

#[derive(Deserialize)]
struct HarRequest {
    method: String,
    url: String,
    headers: Vec<HarHeader>,
}

#[derive(Deserialize)]
struct HarResponse {
    status: i64,
    headers: Vec<HarHeader>,
}

#[derive(Deserialize)]
struct HarHeader {
    name: String,
    value: String,
}

/// Parse the decompressed parts of a trace zip.
///
/// `trace` holds the `context-options` lines of the `.trace` files (the first
/// line of each is enough). `network` holds every `.network` file, JSONL.
/// Each line is parsed and dropped before the next, so memory is the input
/// plus the small output, not a tree of the whole file.
///
/// # Errors
///
/// No `context-options` line (not a Playwright trace), a trace format version
/// we have no fixture for, or a `.network` line that is not the HAR shape.
pub fn parse_trace_network(network: &str, trace: &str) -> Result<TraceNetwork, String> {
    let playwright_version = read_context_options(trace)?;
    let requests = network
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .filter_map(|(i, line)| match serde_json::from_str::<NetworkLine>(line) {
            Ok(NetworkLine { kind, snapshot: Some(entry) }) if kind == "resource-snapshot" => {
                Some(Ok(to_request(entry)))
            }
            Ok(_) => None,
            Err(e) => Some(Err(format!(".network line {}: {}", i + 1, e))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(TraceNetwork { playwright_version, requests })
}

/// Check every `context-options` line's version; return the first `playwrightVersion`
/// (absent before Playwright 1.50, so `None` there).
fn read_context_options(trace: &str) -> Result<Option<String>, String> {
    let options: Vec<ContextOptions> = trace
        .lines()
        .filter_map(|line| serde_json::from_str::<ContextOptions>(line).ok())
        .filter(|o| o.kind == "context-options")
        .collect();
    if options.is_empty() {
        return Err("Not a Playwright trace: no context-options event".to_string());
    }
    if let Some(o) = options
        .iter()
        .find(|o| !o.version.is_some_and(|v| KNOWN_TRACE_VERSIONS.contains(&v)))
    {
        return Err(format!("Unsupported Playwright trace version: {:?}", o.version));
    }
    Ok(options.into_iter().find_map(|o| o.playwright_version))
}

fn to_request(entry: HarEntry) -> TraceRequest {
    let ids = ids_from(entry.request.headers, "request")
        .chain(ids_from(entry.response.headers, "response"))
        .collect();
    TraceRequest {
        method: entry.request.method,
        url: entry.request.url,
        status: u16::try_from(entry.response.status).unwrap_or(0),
        started_at: entry.started_date_time,
        duration_ms: if entry.time.is_finite() && entry.time > 0.0 { entry.time } else { 0.0 },
        ids,
    }
}

fn ids_from(headers: Vec<HarHeader>, on: &'static str) -> impl Iterator<Item = TraceId> {
    headers.into_iter().filter_map(move |h| {
        let header = h.name.to_ascii_lowercase();
        ID_HEADERS
            .contains(&header.as_str())
            .then_some(TraceId { header, value: h.value, on })
    })
}
