//! A minimal blocking HTTP/1.1 client for talking to a sibling `dafs`
//! process's own control API (`GET`/`PUT /watch`).
//!
//! Hand-rolled rather than a crate dependency, for the same reason
//! `self_update` embeds a shell script instead of adding an HTTP client: this
//! is a handful of lines of `std::net` talking to our own process on
//! loopback, with a known, tiny protocol — small JSON bodies, no redirects,
//! no chunked transfer encoding — not a general-purpose HTTP need.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use anyhow::Context as _;

const TIMEOUT: Duration = Duration::from_secs(3);

fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: &str,
) -> anyhow::Result<(u16, String)> {
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT)
        .with_context(|| format!("connecting to {addr}"))?;
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();

    let request = format!(
        "{method} {path} HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Connection: close\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).context("writing the request")?;
    // `Connection: close` above asks the server to close after responding,
    // which is what makes reading to EOF the right way to collect the body
    // here rather than needing to parse Content-Length ourselves. Half-closing
    // the write side here (as if to say "done sending") looks equivalent but
    // is not: hyper treats the incoming FIN as the client hanging up and
    // never writes a response at all — confirmed by hand against a real
    // server. `Connection: close` alone is sufficient and correct.
    let mut response = String::new();
    stream.read_to_string(&mut response).context("reading the response")?;

    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((response.as_str(), ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .context("parsing the response status line")?;

    Ok((status, body.to_string()))
}

#[derive(serde::Deserialize)]
struct RootsBody {
    roots: Vec<String>,
}

pub fn get_roots(addr: SocketAddr) -> anyhow::Result<Vec<String>> {
    let (status, body) = request(addr, "GET", "/watch", "")?;
    anyhow::ensure!(status == 200, "GET /watch returned {status}: {body}");
    serde_json::from_str::<RootsBody>(&body).map(|r| r.roots).context("parsing /watch response")
}

/// `mode` is `"add"` or `"replace"`, matching `dafs_api::WatchMode`'s wire
/// format directly — a plain string rather than that type itself, so this
/// module doesn't need to depend on `dafs-api` just to build a request body.
pub fn change_roots(
    addr: SocketAddr,
    mode: &str,
    roots: Vec<String>,
) -> anyhow::Result<Vec<String>> {
    let body = serde_json::json!({ "mode": mode, "roots": roots }).to_string();
    let (status, resp_body) = request(addr, "PUT", "/watch", &body)?;
    anyhow::ensure!(status == 200, "PUT /watch returned {status}: {resp_body}");
    serde_json::from_str::<RootsBody>(&resp_body)
        .map(|r| r.roots)
        .context("parsing /watch response")
}
