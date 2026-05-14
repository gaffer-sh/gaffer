//! Integration tests for `gaffer test --affected` — exercises the flag dispatch path
//! that lives in `fn main()` and so isn't reachable from unit tests. Each test invokes
//! the built binary, asserts on the exit code, and inspects the diagnostic stderr.
//!
//! These are smoke tests, not full E2E coverage: we don't actually need the wrapped
//! runner to exist, only the `--affected` empty-path branches to behave as documented.

use std::process::Command;

fn gaffer() -> Command {
    Command::new(env!("CARGO_BIN_EXE_gaffer"))
}

/// Path that obviously isn't a real source file. The affected-tests heuristic + graph
/// will return zero matches, exercising the empty-path branch.
const NONEXISTENT_SOURCE: &str = "this/path/does/not/exist/anywhere.rs";

#[test]
fn affected_without_files_exits_with_clear_error() {
    let output = gaffer()
        .args(["test", "--affected"])
        .output()
        .expect("failed to spawn gaffer");

    assert!(!output.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--files"),
        "stderr should mention the missing flag; got: {}",
        stderr
    );
}

#[test]
fn affected_empty_auto_exits_nonzero_in_degraded_mode() {
    // CLI-only mode means coverage_history + failure_history are always unavailable.
    // Auto (default) should therefore exit non-zero when the affected list is empty,
    // because we can't actually confirm "nothing was affected" — only "the signals
    // we could run found nothing."
    let output = gaffer()
        .args(["test", "--affected", "--files", NONEXISTENT_SOURCE])
        .output()
        .expect("failed to spawn gaffer");

    assert!(
        !output.status.success(),
        "Auto default must fail on empty + degraded signals; exit={:?}, stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("signals were unavailable"),
        "stderr should explain the degraded mode; got: {}",
        stderr
    );
}

#[test]
fn affected_empty_skip_exits_zero_even_when_degraded() {
    // Explicit Skip overrides the safe default; the caller has decided silent-skip is right.
    let output = gaffer()
        .args([
            "test",
            "--affected",
            "--files",
            NONEXISTENT_SOURCE,
            "--on-empty",
            "skip",
        ])
        .output()
        .expect("failed to spawn gaffer");

    assert!(
        output.status.success(),
        "explicit --on-empty=skip must exit 0; exit={:?}, stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn affected_empty_fail_exits_nonzero() {
    // Explicit Fail forces non-zero on empty regardless of signal coverage.
    let output = gaffer()
        .args([
            "test",
            "--affected",
            "--files",
            NONEXISTENT_SOURCE,
            "--on-empty",
            "fail",
        ])
        .output()
        .expect("failed to spawn gaffer");

    assert!(
        !output.status.success(),
        "explicit --on-empty=fail must exit non-zero; exit={:?}",
        output.status.code(),
    );
}

#[test]
fn no_command_and_no_affected_exits_with_usage_hint() {
    let output = gaffer()
        .arg("test")
        .output()
        .expect("failed to spawn gaffer");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Either the clap "required" error or our own "no command provided" message is fine —
    // both surface the missing usage clearly. We just need the user to see something useful.
    assert!(
        stderr.to_lowercase().contains("required")
            || stderr.contains("No command provided")
            || stderr.contains("--affected"),
        "stderr should explain missing command or --affected; got: {}",
        stderr
    );
}
