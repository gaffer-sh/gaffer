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
        client_version: "9.9.9-test".into(),
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
    // the size bucket: 25 MB fits a single POST under the v1 ceiling, so we
    // need a file past V1_UPLOAD_CEILING_BYTES to take the MPU path.
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

/// Enforces the dashboard's real v1 behaviour: reject on `Content-Length`
/// over the ceiling, with the structured envelope, and accept otherwise.
/// Mirrors `checkV1UploadCeiling` in
/// `apps/dashboard/server/api/upload/index.post.ts`.
struct V1CeilingServer {
    ceiling: u64,
}

impl Respond for V1CeilingServer {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let declared = req.body.len() as u64;
        if declared > self.ceiling {
            return ResponseTemplate::new(413).set_body_json(json!({
                "statusMessage": "File exceeds maximum size of 75MB",
                "data": {
                    "reason": "file_too_large_for_v1",
                    "migration": "https://gaffer.sh/install.sh",
                    "ceiling": self.ceiling,
                    "declaredSize": declared,
                },
            }));
        }
        // An append into a multi-file session answers 202 without finalizing.
        if let Some(sid) = req.headers.get("x-gaffer-upload-session") {
            return ResponseTemplate::new(202)
                .set_body_json(append_response_body(sid.to_str().unwrap_or_default()));
        }
        ResponseTemplate::new(200).set_body_json(upload_response_body("upl_split"))
    }
}

fn append_response_body(session_id: &str) -> serde_json::Value {
    json!({ "uploadSessionId": session_id, "status": "receiving", "filesReceived": 1 })
}

/// Mounts `POST /api/upload/session` (expected exactly `opens` times) and the
/// matching finalize, which returns the canonical upload response.
async fn mount_session(server: &MockServer, session_id: &str, opens: u64) {
    Mock::given(method("POST"))
        .and(path("/api/upload/session"))
        .and(body_partial_json(
            json!({ "tags": { "commitSha": "abc123", "branch": "main" } }),
        ))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(json!({ "uploadSessionId": session_id })),
        )
        .expect(opens)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/api/upload/session/{}/finalize", session_id)))
        .respond_with(ResponseTemplate::new(201).set_body_json(upload_response_body(session_id)))
        .expect(opens)
        .mount(server)
        .await;
}

/// GAF-255, the defect itself: a bundle of individually-small files whose
/// total lands between the client's old 200 MiB threshold and the server's
/// 75 MiB ceiling. Every released CLI single-POSTs this and takes a 413 it
/// cannot recover from. Routing must now split it below the real ceiling,
/// and the split must land as ONE session: the dashboard serves a hosted
/// HTML report's assets from the report's own session, and rejects any
/// session holding no parseable file.
///
/// Two 40 MiB files rather than many 1 MiB ones: same routing decision at
/// HEAD (largest < 90 MiB, total < 200 MiB → one POST), a third of the I/O.
#[tokio::test]
async fn bundle_between_the_ceiling_and_the_old_threshold_splits_into_one_session() {
    let server = MockServer::start().await;
    let dir = TempDir::new().unwrap();
    let a = make_file(&dir, "index.html", 40 * 1024 * 1024);
    let b = make_file(&dir, "shot.png", 40 * 1024 * 1024);

    mount_session(&server, "upl_multi", 1).await;
    Mock::given(method("POST"))
        .and(path("/api/upload"))
        .and(header("x-gaffer-upload-session", "upl_multi"))
        .respond_with(V1CeilingServer {
            ceiling: upload::V1_UPLOAD_CEILING_BYTES,
        })
        // Exactly two appends: one per 40 MiB bundle. Three would mean we
        // wasted a round-trip discovering the ceiling we already knew.
        .expect(2)
        .mount(&server)
        .await;

    let outcome = upload::run(&[a, b], &cfg(&server.uri())).await.unwrap();
    assert_eq!(outcome.files_uploaded, 2);
    assert_eq!(outcome.upload_session_id, "upl_multi");
    assert_eq!(
        outcome.responses.len(),
        1,
        "one finalized session, not one per request"
    );
}

/// A file too large for any POST joins the same session over MPU as the
/// report it belongs with, instead of becoming an artifact-only session the
/// dashboard would reject.
#[tokio::test]
async fn large_artifact_joins_the_reports_session_over_mpu() {
    let server = MockServer::start().await;
    let dir = TempDir::new().unwrap();
    let trace = make_file(&dir, "trace.zip", 76 * 1024 * 1024);
    let report = make_file(&dir, "report.xml", 2048);

    mount_session(&server, "upl_multi", 1).await;
    Mock::given(method("POST"))
        .and(path("/api/upload/mpu/create"))
        .and(body_partial_json(
            json!({ "fileName": "trace.zip", "uploadSessionId": "upl_multi" }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uploadId": "mpu_joined",
            "uploadSessionId": "upl_multi",
            "key": "k",
            "partSize": 80 * 1024 * 1024,
            "numParts": 1
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/upload/mpu/part"))
        .and(query_param("uploadSessionId", "upl_multi"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "partNumber": 1, "etag": "e1" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/upload/mpu/complete"))
        .and(body_partial_json(
            json!({ "uploadSessionId": "upl_multi", "uploadId": "mpu_joined" }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(append_response_body("upl_multi")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/upload"))
        .and(header("x-gaffer-upload-session", "upl_multi"))
        .respond_with(V1CeilingServer {
            ceiling: upload::V1_UPLOAD_CEILING_BYTES,
        })
        .expect(1)
        .mount(&server)
        .await;

    let outcome = upload::run(&[trace, report], &cfg(&server.uri()))
        .await
        .unwrap();
    assert_eq!(outcome.upload_session_id, "upl_multi");
    assert_eq!(outcome.files_uploaded, 2);
}

/// A lower-ceiling 413 on an append re-plans that bundle into the SAME
/// session: the dashboard leaves the session open across that rejection.
#[tokio::test]
async fn append_413_replans_within_the_same_session() {
    let server = MockServer::start().await;
    let dir = TempDir::new().unwrap();
    let a = make_file(&dir, "a.xml", 40 * 1024 * 1024);
    let b = make_file(&dir, "b.xml", 40 * 1024 * 1024);
    // A server whose real ceiling (30 MiB) is lower than the client's: each
    // 40 MiB bundle is rejected once, and at 30 MiB its file can only go
    // over MPU, joined to the same session.
    let lower = 30 * 1024 * 1024;

    mount_session(&server, "upl_multi", 1).await;
    Mock::given(method("POST"))
        .and(path("/api/upload/mpu/create"))
        .and(body_partial_json(json!({ "uploadSessionId": "upl_multi" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uploadId": "mpu_replan",
            "uploadSessionId": "upl_multi",
            "key": "k",
            "partSize": 50 * 1024 * 1024,
            "numParts": 1
        })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/upload/mpu/part"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "partNumber": 1, "etag": "e1" })),
        )
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/upload/mpu/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(append_response_body("upl_multi")))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/upload"))
        .and(header("x-gaffer-upload-session", "upl_multi"))
        .respond_with(V1CeilingServer { ceiling: lower })
        .expect(2)
        .mount(&server)
        .await;

    let outcome = upload::run(&[a, b], &cfg(&server.uri())).await.unwrap();
    assert_eq!(outcome.upload_session_id, "upl_multi");
}

/// A failure part-way through names the session, so support can find the
/// half-received upload (the dashboard's stale-receiving sweep closes it).
#[tokio::test]
async fn mid_session_failure_reports_the_session_id() {
    let server = MockServer::start().await;
    let dir = TempDir::new().unwrap();
    let a = make_file(&dir, "a.xml", 40 * 1024 * 1024);
    let b = make_file(&dir, "b.xml", 40 * 1024 * 1024);

    Mock::given(method("POST"))
        .and(path("/api/upload/session"))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(json!({ "uploadSessionId": "upl_multi" })),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/upload"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "statusMessage": "Invalid filename provided",
            "data": { "reason": "invalid_filename" },
        })))
        .mount(&server)
        .await;

    let err = upload::run(&[a, b], &cfg(&server.uri())).await.unwrap_err();
    match err {
        upload::UploadError::ServerError {
            upload_session_id, ..
        } => {
            assert_eq!(upload_session_id.as_deref(), Some("upl_multi"));
        }
        other => panic!("expected a server error, got {:?}", other),
    }
}

/// GAF-255. Before the fix a 413 was terminal: the CLI discarded the
/// `ceiling` the server stated and failed the run with `file_too_large` and a
/// fix hint about a 5 GB MPU limit that had nothing to do with the rejection.
/// Now the stated ceiling re-plans the bundle, so the upload lands.
#[tokio::test]
async fn v1_ceiling_413_replans_the_bundle_through_mpu() {
    let server = MockServer::start().await;
    let dir = TempDir::new().unwrap();
    let report = make_file(&dir, "report.xml", 2048);

    // The dashboard's real envelope for `file_too_large_for_v1`
    // (apps/dashboard/server/api/upload/index.post.ts). A deliberately tiny
    // ceiling keeps the fixture small while still being *lower* than the
    // client's default, which is what triggers the re-plan.
    Mock::given(method("POST"))
        .and(path("/api/upload"))
        .respond_with(ResponseTemplate::new(413).set_body_json(json!({
            "statusMessage": "File exceeds maximum size of 75MB",
            "data": {
                "reason": "file_too_large_for_v1",
                "uploadSessionId": "upl_413",
                "cfRayId": null,
                "migration": "https://gaffer.sh/install.sh",
                "ceiling": 1024,
                "declaredSize": 3000,
            },
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/upload/mpu/create"))
        .and(body_partial_json(json!({ "fileName": "report.xml", "fileSize": 2048 })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uploadId": "mpu_replan",
            "uploadSessionId": "upl_replan",
            "key": "k",
            "partSize": 5 * 1024 * 1024,
            "numParts": 1
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path("/api/upload/mpu/part"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "partNumber": 1,
            "etag": "e1",
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/upload/mpu/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(upload_response_body("upl_replan")))
        .expect(1)
        .mount(&server)
        .await;

    let outcome = upload::run(&[report], &cfg(&server.uri())).await.unwrap();
    assert_eq!(outcome.upload_session_id, "upl_replan");
    assert_eq!(outcome.files_uploaded, 1);
}

/// A 413 with no `ceiling` in its envelope (a proxy's own 413, say) has
/// nothing to re-plan against. It must surface rather than loop.
#[tokio::test]
async fn bare_413_surfaces_without_replanning() {
    let server = MockServer::start().await;
    let dir = TempDir::new().unwrap();
    let report = make_file(&dir, "report.xml", 2048);

    Mock::given(method("POST"))
        .and(path("/api/upload"))
        .respond_with(ResponseTemplate::new(413).set_body_string("413 Payload Too Large"))
        .expect(1)
        .mount(&server)
        .await;

    let err = upload::run(&[report], &cfg(&server.uri())).await.unwrap_err();
    assert_eq!(err.kind(), "file_too_large", "got: {:?}", err);
}

/// A 413 whose ceiling is *not* lower than ours is a disagreement we can't
/// route around — re-planning at the same number would send the same request.
#[tokio::test]
async fn equal_or_higher_ceiling_does_not_replan() {
    let server = MockServer::start().await;
    let dir = TempDir::new().unwrap();
    let report = make_file(&dir, "report.xml", 2048);

    Mock::given(method("POST"))
        .and(path("/api/upload"))
        .respond_with(ResponseTemplate::new(413).set_body_json(json!({
            "statusMessage": "too large",
            "data": { "ceiling": upload::V1_UPLOAD_CEILING_BYTES },
        })))
        .expect(1)
        .mount(&server)
        .await;

    let err = upload::run(&[report], &cfg(&server.uri())).await.unwrap_err();
    assert_eq!(err.kind(), "file_too_large", "got: {:?}", err);
    assert_eq!(err.ceiling(), Some(upload::V1_UPLOAD_CEILING_BYTES));
}

/// Client attribution headers the dashboard reads for upload analytics
/// (`gaffer-client`, `gaffer-client-version`) and the audit log
/// (`X-Gaffer-Source`). All three were absent before GAF-255, and the
/// `User-Agent` reported gaffer-core's version rather than the CLI's.
#[tokio::test]
async fn sends_client_attribution_headers() {
    let server = MockServer::start().await;
    let dir = TempDir::new().unwrap();
    let report = make_file(&dir, "report.xml", 1024);

    Mock::given(method("POST"))
        .and(path("/api/upload"))
        .and(header("gaffer-client", "gaffer-cli"))
        .and(header("gaffer-client-version", "9.9.9-test"))
        .and(header("X-Gaffer-Source", "cli"))
        .and(header("User-Agent", "gaffer-cli/9.9.9-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(upload_response_body("upl_hdr")))
        .expect(1)
        .mount(&server)
        .await;

    let outcome = upload::run(&[report], &cfg(&server.uri())).await.unwrap();
    assert_eq!(outcome.upload_session_id, "upl_hdr");
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
