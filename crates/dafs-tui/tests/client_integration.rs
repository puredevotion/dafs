//! Integration tests for [`dafs_tui::client::Client`] against a real HTTP
//! server on a real loopback socket — not a direct call into the parsing
//! functions, which are already unit-tested in `src/client.rs`. This is the
//! layer those unit tests can't cover: does a real `ureq` request against a
//! real response actually produce the `Status` the daemon's API is supposed
//! to produce.
//!
//! The mock server is a few dozen lines of `std::net`, not a new dependency —
//! `dafs-tui` deliberately has no async runtime, and axum/tokio (already in
//! the workspace) would be a heavier dev-dependency than this test needs.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use dafs_tui::client::Client;

fn respond(mut stream: TcpStream, status_line: &str, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

/// Reads just enough of the request to know its path; a GET has no body
/// worth reading further, and every fixture server here only inspects the
/// request line.
fn request_path(stream: &mut TcpStream) -> String {
    let mut buf = [0u8; 4096];
    let mut data = Vec::new();
    loop {
        let n = stream.read(&mut buf).unwrap_or(0);
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        if data.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&data);
    let request_line = text.lines().next().unwrap_or("");
    request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .to_string()
}

/// Serves a full, healthy daemon snapshot matching the real shapes in
/// `crates/dafs-api/src/lib.rs`: `/readyz` 200, `/version`, `/metrics`
/// (Prometheus text), `/events` (two items).
fn spawn_healthy_daemon() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    thread::spawn(move || {
        for stream in listener.incoming().take(4) {
            let Ok(mut stream) = stream else { continue };
            let path = request_path(&mut stream);
            match path.as_str() {
                "/readyz" => respond(
                    stream,
                    "200 OK",
                    "application/json",
                    r#"{"ready":true,"schema_version":2}"#,
                ),
                "/version" => respond(
                    stream,
                    "200 OK",
                    "application/json",
                    r#"{"version":"9.9.9","schema_version":2,"uptime_seconds":42}"#,
                ),
                "/metrics" => respond(
                    stream,
                    "200 OK",
                    "text/plain; version=0.0.4",
                    "# TYPE dafs_ready gauge\n\
                     dafs_ready 1\n\
                     dafs_resident_bytes 12345678\n\
                     dafs_events_total 7\n\
                     dafs_files_known 3\n",
                ),
                "/events" => respond(
                    stream,
                    "200 OK",
                    "application/json",
                    r#"{"events":[
                        {"id":2,"path":"/a/b.txt","kind":"modified","at_unix_ms":1000,"size_bytes":10,"is_dir":false},
                        {"id":1,"path":"/a","kind":"created","at_unix_ms":900,"is_dir":true}
                    ],"next_before_id":1}"#,
                ),
                _ => respond(stream, "404 Not Found", "text/plain", "not found"),
            }
        }
    });

    format!("http://{addr}")
}

/// Serves `/readyz` as 503 and nothing else useful — matches a daemon that's
/// alive but still migrating.
fn spawn_not_ready_daemon() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    thread::spawn(move || {
        for stream in listener.incoming().take(1) {
            let Ok(mut stream) = stream else { continue };
            let _ = request_path(&mut stream);
            respond(
                stream,
                "503 Service Unavailable",
                "application/json",
                r#"{"ready":false,"schema_version":0}"#,
            );
        }
    });

    format!("http://{addr}")
}

#[test]
fn a_healthy_daemon_produces_a_fully_populated_status() {
    let url = spawn_healthy_daemon();
    let client = Client::new(url);

    let status = client.poll(10);

    assert!(status.connected);
    assert!(status.ready);
    assert_eq!(status.version.as_deref(), Some("9.9.9"));
    assert_eq!(status.schema_version, Some(2));
    assert_eq!(status.uptime_seconds, Some(42));
    assert_eq!(status.resident_bytes, Some(12_345_678));
    assert_eq!(status.events_total, Some(7));
    assert_eq!(status.files_known, Some(3));
    assert_eq!(status.events.len(), 2);
    assert_eq!(status.events[0].path, "/a/b.txt");
    assert!(status.events[1].is_dir);
    assert!(status.error.is_none());
}

#[test]
fn a_not_ready_daemon_is_connected_but_not_ready() {
    let url = spawn_not_ready_daemon();
    let client = Client::new(url);

    let status = client.poll(10);

    assert!(status.connected, "a 503 still means the daemon answered");
    assert!(!status.ready);
}

#[test]
fn an_unreachable_address_is_disconnected_with_an_error() {
    // Nothing is listening on this port — a real connection-refused, not a
    // simulated one.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener); // free the port, guaranteeing nothing answers it

    let client = Client::new(format!("http://{addr}"));
    let status = client.poll(10);

    assert!(!status.connected);
    assert!(status.error.is_some());
    assert!(status.events.is_empty());
}
