//! Fixtures in `tests/fixtures/playwright-trace/<version>/` come from running
//! `@playwright/test` at that version with `trace: 'on'` against a local server:
//! a page load, a GET with `traceparent` + `X-Request-ID` (the server answers
//! with `x-request-id`), a POST that gets a 500, and a fetch to a blocked port.
//! `trace.zip` is the runner's output; the `0-trace.*` files are unzipped from it.

use gaffer_parsers::trace_network::{parse_trace_network, TraceId};
use std::fs;
use std::path::PathBuf;

fn fixture(version: &str, file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/playwright-trace")
        .join(version)
        .join(file);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("Failed to load {}: {}", path.display(), e))
}

fn parse(version: &str) -> gaffer_parsers::trace_network::TraceNetwork {
    parse_trace_network(&fixture(version, "0-trace.network"), &fixture(version, "0-trace.trace"))
        .expect("fixture parses")
}

fn assert_requests(version: &str) {
    let parsed = parse(version);
    let summary: Vec<(&str, &str, u16)> = parsed
        .requests
        .iter()
        .map(|r| (r.method.as_str(), r.url.as_str(), r.status))
        .collect();
    assert_eq!(
        summary,
        [
            ("GET", "http://127.0.0.1:4588/", 200),
            ("GET", "http://127.0.0.1:1/unreachable", 0),
            ("GET", "http://127.0.0.1:4588/api/ok", 200),
            ("POST", "http://127.0.0.1:4588/api/fail", 500),
        ]
    );

    let ok = &parsed.requests[2];
    assert_eq!(
        ok.ids,
        [
            TraceId { header: "x-request-id".into(), value: "req-abc-123".into(), on: "request" },
            TraceId {
                header: "traceparent".into(),
                value: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(),
                on: "request"
            },
            TraceId { header: "x-request-id".into(), value: "srv-req-42".into(), on: "response" },
        ]
    );
    assert!(ok.started_at.starts_with("2026-11-13T") && ok.started_at.ends_with('Z'));
    assert!(ok.duration_ms > 0.0);

    let failed = &parsed.requests[1];
    assert_eq!(failed.duration_ms, 0.0);
    assert!(failed.ids.is_empty());
}

#[test]
fn parses_playwright_1_57_trace() {
    assert_requests("1.57.0");
    assert_eq!(parse("1.57.0").playwright_version.as_deref(), Some("1.57.0"));
}

#[test]
fn parses_playwright_1_49_trace_without_version_field() {
    assert_requests("1.49.1");
    assert_eq!(parse("1.49.1").playwright_version, None);
}

#[test]
fn serializes_to_contract_shape() {
    let json = serde_json::to_value(parse("1.57.0")).expect("serializes");
    let first = &json["requests"][0];
    assert_eq!(json["playwrightVersion"], "1.57.0");
    for key in ["method", "url", "status", "startedAt", "durationMs", "ids"] {
        assert!(first.get(key).is_some(), "missing {key}");
    }
    assert_eq!(first.as_object().map(|o| o.len()), Some(6));
}

#[test]
fn rejects_input_that_is_not_a_trace() {
    let err = parse_trace_network("", "PK\u{3}\u{4} not json").unwrap_err();
    assert!(err.contains("Not a Playwright trace"), "{err}");
}

#[test]
fn rejects_unknown_trace_version() {
    let trace = fixture("1.57.0", "0-trace.trace").replacen("\"version\":8", "\"version\":99", 1);
    let err = parse_trace_network(&fixture("1.57.0", "0-trace.network"), &trace).unwrap_err();
    assert!(err.contains("Unsupported Playwright trace version: Some(99)"), "{err}");
}

#[test]
fn rejects_network_line_of_unknown_shape() {
    let network = fixture("1.57.0", "0-trace.network").replacen("\"startedDateTime\"", "\"renamed\"", 1);
    let err = parse_trace_network(&network, &fixture("1.57.0", "0-trace.trace")).unwrap_err();
    assert!(err.starts_with(".network line 1:"), "{err}");
}
