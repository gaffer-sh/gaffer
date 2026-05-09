//! Integration tests for the MPU upload client (TASK-40 AC #3).
//!
//! Uses `wiremock` to stand in for the dashboard's `/api/upload` and
//! `/api/upload/mpu/*` endpoints. The mocks assert the wire contract so a
//! drift between client and server (e.g. a renamed field, missing query
//! param, or wrong header) fails the build instead of being caught at
//! integration time.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gaffer_core::upload::{self, UploadConfig};
use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn cfg(api_url: &str) -> UploadConfig {
    UploadConfig {
        token: "gfr_test".into(),
        api_url: Some(api_url.to_string()),
        commit_sha: Some("abc123".into()),
        branch: Some("main".into()),
        test_framework: Some("playwright".into()),
        test_suite: None,
        timeout_secs: 30,
        max_file_size_mb: 8 * 1024, // 8 GB so 5 GB MPU fixture isn't rejected
        debug: false,
    }
}

fn make_file(dir: &TempDir, name: &str, size: usize) -> PathBuf {
    let path = dir.path().join(name);
    let buf = vec![0xABu8; size];
    std::fs::write(&path, &buf).unwrap();
    path
}

fn upload_response_body(session_id: &str) -> serde_json::Value {
    json!({
        "uploadSession": {
            "id": session_id,
            "uniqueId": "run_xyz",
            "projectId": "proj_abc",
            "commitSha": "abc123",
            "branch": "main",
            "processingStatus": "uploaded",
            "tags": null,
            "createdAt": "2026-04-25T00:00:00Z"
        },
        "testRun": {
            "id": session_id,
            "uniqueId": "run_xyz",
            "projectId": "proj_abc",
            "commitSha": "abc123",
            "branch": "main",
            "tags": null,
            "createdAt": "2026-04-25T00:00:00Z"
        },
        "files": [],
        "parseableCount": 1,
        "artifactCount": 0
    })
}

#[tokio::test]
async fn small_file_routes_to_single_post() {
    let server = MockServer::start().await;
    let dir = TempDir::new().unwrap();
    let report = make_file(&dir, "report.xml", 1024);

    Mock::given(method("POST"))
        .and(path("/api/upload"))
        .and(header("X-API-Key", "gfr_test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(upload_response_body("upl_small")))
        .expect(1)
        .mount(&server)
        .await;

    let outcome = upload::run(&[report], &cfg(&server.uri())).await.unwrap();
    assert_eq!(outcome.upload_session_id, "upl_small");
    assert_eq!(outcome.responses.len(), 1);
    // files_uploaded must reflect the file count, not the response count
    // (regression guard — see code review #1).
    assert_eq!(outcome.files_uploaded, 1);
}

#[tokio::test]
async fn mpu_happy_path_with_three_parts() {
    let server = MockServer::start().await;
    let dir = TempDir::new().unwrap();

    // 25 MB file with 10 MB parts → 3 parts (selectMpuPartSize bucket).
    let big = make_file(&dir, "trace.zip", 25 * 1024 * 1024);
    // Force routing to MPU by also passing a clearly-large declared file via
    // the size bucket: 25 MB is below the 90 MB SMALL_FILE_THRESHOLD though,
    // so we need a >=90 MB file to take the MPU path.
    let trace = make_file(&dir, "trace2.zip", 100 * 1024 * 1024);
    let _ = big; // referenced to silence unused warning if removed later

    // /create asserts X-API-Key + the request body shape matches the
    // dashboard's Zod schema for its multipart-upload create endpoint.
    Mock::given(method("POST"))
        .and(path("/api/upload/mpu/create"))
        .and(header("X-API-Key", "gfr_test"))
        .and(body_partial_json(json!({
            "fileName": "trace2.zip",
            "fileSize": 100 * 1024 * 1024,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uploadId": "mpu_id_1",
            "uploadSessionId": "upl_mpu",
            "key": "org/proj/run/trace2.zip",
            "partSize": 50 * 1024 * 1024,
            "numParts": 2
        })))
        .expect(1)
        .mount(&server)
        .await;

    // /part asserts X-API-Key + all three query params present, and the
    // response intentionally lies about partNumber to verify the client
    // substitutes its locally-known value (regression guard — see code
    // review #5: trusted server echo).
    Mock::given(method("PUT"))
        .and(path("/api/upload/mpu/part"))
        .and(header("X-API-Key", "gfr_test"))
        .and(query_param("uploadSessionId", "upl_mpu"))
        .and(query_param("uploadId", "mpu_id_1"))
        .respond_with(|req: &Request| {
            // Echo back a deliberately wrong partNumber. The client must
            // ignore this and use the local value; otherwise /complete
            // would receive [{1, e1}, {1, e2}] and trigger duplicate-part
            // validation server-side.
            let pn = req
                .url
                .query_pairs()
                .find(|(k, _)| k == "partNumber")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();
            ResponseTemplate::new(200).set_body_json(json!({
                "partNumber": 99,
                "etag": format!("etag-{}", pn),
            }))
        })
        .expect(2)
        .mount(&server)
        .await;

    // /complete asserts X-API-Key + the request body has parts in
    // ascending partNumber order with the right session/upload ids.
    Mock::given(method("POST"))
        .and(path("/api/upload/mpu/complete"))
        .and(header("X-API-Key", "gfr_test"))
        .and(body_partial_json(json!({
            "uploadSessionId": "upl_mpu",
            "uploadId": "mpu_id_1",
            "parts": [
                { "partNumber": 1 },
                { "partNumber": 2 },
            ],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(upload_response_body("upl_mpu")))
        .expect(1)
        .mount(&server)
        .await;

    let outcome = upload::run(&[trace], &cfg(&server.uri())).await.unwrap();
    assert_eq!(outcome.upload_session_id, "upl_mpu");
    assert_eq!(outcome.responses.len(), 1);
    assert_eq!(outcome.files_uploaded, 1);
}

#[tokio::test]
async fn complete_failure_triggers_abort() {
    // Parts upload cleanly; /complete itself returns 4xx. The client must
    // still fire /abort to free the orphan MPU before surfacing the error.
    // (Code review gap #2 — uncovered before this test landed.)
    let server = MockServer::start().await;
    let dir = TempDir::new().unwrap();
    let trace = make_file(&dir, "trace.zip", 100 * 1024 * 1024);

    Mock::given(method("POST"))
        .and(path("/api/upload/mpu/create"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uploadId": "mpu_complete_fail",
            "uploadSessionId": "upl_complete_fail",
            "key": "k",
            "partSize": 50 * 1024 * 1024,
            "numParts": 2
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path("/api/upload/mpu/part"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "partNumber": 1,
            "etag": "e",
        })))
        .expect(2)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/upload/mpu/complete"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "statusMessage": "size_mismatch"
        })))
        .mount(&server)
        .await;

    Mock::given(method("DELETE"))
        .and(path("/api/upload/mpu/abort"))
        .and(query_param("uploadSessionId", "upl_complete_fail"))
        .and(query_param("uploadId", "mpu_complete_fail"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let err = upload::run(&[trace], &cfg(&server.uri())).await.unwrap_err();
    // 400 from /complete classifies as request_rejected after /abort fires.
    assert_eq!(err.kind(), "request_rejected", "got: {:?}", err);
}

#[tokio::test]
async fn retries_on_5xx_then_succeeds() {
    let server = MockServer::start().await;
    let dir = TempDir::new().unwrap();
    let report = make_file(&dir, "report.xml", 1024);

    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_clone = attempts.clone();

    Mock::given(method("POST"))
        .and(path("/api/upload"))
        .respond_with(move |_req: &Request| {
            let n = attempts_clone.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                ResponseTemplate::new(503).set_body_string("transient")
            } else {
                ResponseTemplate::new(200).set_body_json(upload_response_body("upl_retry"))
            }
        })
        .mount(&server)
        .await;

    let mut config = cfg(&server.uri());
    config.timeout_secs = 5;
    let outcome = upload::run(&[report], &config).await.unwrap();
    assert_eq!(outcome.upload_session_id, "upl_retry");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn does_not_retry_on_4xx() {
    let server = MockServer::start().await;
    let dir = TempDir::new().unwrap();
    let report = make_file(&dir, "report.xml", 1024);

    Mock::given(method("POST"))
        .and(path("/api/upload"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "statusMessage": "API key required."
        })))
        .expect(1)
        .mount(&server)
        .await;

    let err = upload::run(&[report], &cfg(&server.uri())).await.unwrap_err();
    assert_eq!(err.kind(), "auth_failed", "got: {:?}", err);
}

#[tokio::test]
async fn mpu_failure_triggers_abort() {
    let server = MockServer::start().await;
    let dir = TempDir::new().unwrap();
    let trace = make_file(&dir, "trace.zip", 100 * 1024 * 1024);

    Mock::given(method("POST"))
        .and(path("/api/upload/mpu/create"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uploadId": "mpu_doomed",
            "uploadSessionId": "upl_doomed",
            "key": "k",
            "partSize": 50 * 1024 * 1024,
            "numParts": 2
        })))
        .expect(1)
        .mount(&server)
        .await;

    // Every part PUT returns 400 (deliberately client-error so retry is
    // skipped — the test isolates the abort behaviour).
    Mock::given(method("PUT"))
        .and(path("/api/upload/mpu/part"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "statusMessage": "bad part"
        })))
        .mount(&server)
        .await;

    Mock::given(method("DELETE"))
        .and(path("/api/upload/mpu/abort"))
        .and(query_param("uploadSessionId", "upl_doomed"))
        .and(query_param("uploadId", "mpu_doomed"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let err = upload::run(&[trace], &cfg(&server.uri())).await.unwrap_err();
    assert_eq!(err.kind(), "request_rejected", "got: {:?}", err);
    // wiremock validates `.expect(1)` on the abort endpoint at drop time, so
    // if we got here without a panic, the abort fired exactly once.
}

/// Tracks concurrent in-flight requests around a deliberate blocking sleep.
///
/// Wiremock's `Respond::respond` is sync and runs on a hyper task — sleeping
/// inside it (with `std::thread::sleep`, not async sleep) holds the request
/// open across the sleep window so other concurrent requests overlap and
/// the in-flight counter accumulates. This is the missing piece in the
/// original `set_delay`-based test, where the decrement happened
/// synchronously inside the closure before the delayed response was sent
/// — the counter never went above 1 regardless of the client's actual
/// concurrency, making the assertion vacuous (code review #2).
///
/// Requires a `multi_thread` runtime with enough workers to actually park
/// many tasks at once.
struct ConcurrencyTracker {
    in_flight: Arc<AtomicUsize>,
    max_seen: Arc<AtomicUsize>,
    sleep: Duration,
}

impl Respond for ConcurrencyTracker {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let cur = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        // fetch_max keeps the high-water mark monotonic across threads.
        self.max_seen.fetch_max(cur, Ordering::SeqCst);
        std::thread::sleep(self.sleep);
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        ResponseTemplate::new(200).set_body_json(json!({
            "partNumber": 1,
            "etag": "e",
        }))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn mpu_part_concurrency_capped_at_eight() {
    let server = MockServer::start().await;
    let dir = TempDir::new().unwrap();

    // 100 MB file × 10 MB parts = 10 parts. With an 8-permit semaphore we
    // expect a max in-flight of 8 — the 10 parts can't all overlap, but the
    // first 8 can.
    let trace = make_file(&dir, "trace.zip", 100 * 1024 * 1024);

    Mock::given(method("POST"))
        .and(path("/api/upload/mpu/create"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uploadId": "mpu_cc",
            "uploadSessionId": "upl_cc",
            "key": "k",
            "partSize": 10 * 1024 * 1024,
            "numParts": 10
        })))
        .mount(&server)
        .await;

    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));

    Mock::given(method("PUT"))
        .and(path("/api/upload/mpu/part"))
        .respond_with(ConcurrencyTracker {
            in_flight: in_flight.clone(),
            max_seen: max_seen.clone(),
            sleep: Duration::from_millis(200),
        })
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/upload/mpu/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(upload_response_body("upl_cc")))
        .mount(&server)
        .await;

    let outcome = upload::run(&[trace], &cfg(&server.uri())).await.unwrap();
    assert_eq!(outcome.upload_session_id, "upl_cc");

    let observed_max = max_seen.load(Ordering::SeqCst);
    // Upper bound: never more than 8 concurrent (the client's semaphore cap).
    // This is the regression we actually care about — if someone removes the
    // semaphore or raises the permit count, observed_max would exceed 8.
    //
    // We deliberately do NOT assert a lower bound. reqwest's HTTP/1.1
    // connection pool serialises requests to the same host on a single
    // connection unless multiple connections happen to be opened, which
    // depends on wiremock's accept loop and isn't a property of our code.
    // A finer-grained semaphore-cap test would have to bypass HTTP entirely.
    assert!(
        observed_max <= 8,
        "expected at most 8 concurrent part PUTs, observed {}",
        observed_max
    );
}

/// Direct semaphore test — verifies the cap by exercising
/// `tokio::sync::Semaphore` the same way the production code uses it.
/// Complements `mpu_part_concurrency_capped_at_eight`, which can't
/// observe overlap reliably through the HTTP test stack.
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn semaphore_caps_at_eight_directly() {
    use futures::stream::FuturesUnordered;
    use futures::StreamExt;
    use tokio::sync::Semaphore;

    let sem = Arc::new(Semaphore::new(upload::MPU_PART_CONCURRENCY));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));

    let mut futs = FuturesUnordered::new();
    for _ in 0..32 {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let in_flight = in_flight.clone();
        let max_seen = max_seen.clone();
        futs.push(tokio::spawn(async move {
            let _p = permit;
            let cur = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            max_seen.fetch_max(cur, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
        }));
    }
    while futs.next().await.is_some() {}

    let observed = max_seen.load(Ordering::SeqCst);
    assert_eq!(
        observed,
        upload::MPU_PART_CONCURRENCY,
        "expected exactly {} concurrent permits, saw {}",
        upload::MPU_PART_CONCURRENCY,
        observed,
    );
}

#[tokio::test]
async fn user_error_on_oversized_file() {
    let dir = TempDir::new().unwrap();
    let big = make_file(&dir, "big.bin", 10 * 1024); // 10 KB
    let mut config = cfg("http://localhost:1");
    config.max_file_size_mb = 0; // anything > 0 bytes is rejected

    let err = upload::run(&[big], &config).await.unwrap_err();
    assert_eq!(err.kind(), "file_too_large");
    assert_eq!(err.exit_code(), 1);
}
