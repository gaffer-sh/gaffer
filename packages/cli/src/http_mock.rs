//! Minimal in-process HTTP server for tests.
//!
//! Both the OIDC token exchange (`oidc.rs`) and the device authorization flow
//! (`commands/login.rs`) are pure HTTP against a base URL, so they are tested
//! by pointing them at a throwaway listener rather than by abstracting `ureq`
//! behind a trait for the sake of the tests.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

/// Spin up an HTTP mock server on 127.0.0.1 that replies to each sequential
/// request with the next `(status_line, body)` pair, then exits once the list
/// is exhausted. Returns the "http://host:port" base URL.
///
/// A request beyond the list finds no listener (connection refused); tests rely
/// on that to prove a call wasn't retried.
pub fn mock_sequence(responses: Vec<(&'static str, String)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock listener");
    let addr = listener.local_addr().expect("mock listener addr");
    thread::spawn(move || {
        for (status_line, body) in responses {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        }
    });
    format!("http://{addr}")
}

/// Serve exactly one request with the given status line and body.
pub fn mock_once(status_line: &'static str, body: String) -> String {
    mock_sequence(vec![(status_line, body)])
}
