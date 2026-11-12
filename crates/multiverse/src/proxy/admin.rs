//! A small HTTP/1.1 client for Caddy's admin API on a Unix socket. It
//! sends one request on one connection and reads the response to the end.

use std::io::{self, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// How long one read or write on the socket waits.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// The most bytes of a response that the client keeps.
const MOST_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;

/// A response from the admin API.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Response {
    /// The status code.
    pub(super) status: u16,
    /// The body, with any chunked encoding removed.
    pub(super) body: Vec<u8>,
}

/// Sends `GET path` to the admin socket at `socket`.
pub(super) fn get(socket: &Path, path: &str) -> io::Result<Response> {
    request(socket, "GET", path, None)
}

/// Sends `POST path` with the JSON `body` to the admin socket at `socket`.
pub(super) fn post(socket: &Path, path: &str, body: &[u8]) -> io::Result<Response> {
    request(socket, "POST", path, Some(body))
}

/// Returns the bytes of the request for `method`, `path` and `body`. The
/// `Host` header names a loopback host, which Caddy's origin check accepts.
fn bytes(method: &str, path: &str, body: Option<&[u8]>) -> Vec<u8> {
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if let Some(body) = body {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("\r\n");
    let mut request = head.into_bytes();
    request.extend_from_slice(body.unwrap_or_default());
    request
}

/// Sends one request and reads its response.
fn request(socket: &Path, method: &str, path: &str, body: Option<&[u8]>) -> io::Result<Response> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.write_all(&bytes(method, path, body))?;
    let mut raw = Vec::new();
    stream.take(MOST_RESPONSE_BYTES).read_to_end(&mut raw)?;
    parse(&raw)
}

/// Parses a complete response.
fn parse(raw: &[u8]) -> io::Result<Response> {
    let invalid = |reason: &str| io::Error::new(io::ErrorKind::InvalidData, reason.to_owned());
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| invalid("the response has no end of headers"))?;
    let head = String::from_utf8_lossy(raw.get(..split).unwrap_or_default());
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| invalid("the response has no status line"))?;
    let chunked = lines.any(|line| {
        let line = line.to_ascii_lowercase();
        line.starts_with("transfer-encoding:") && line.contains("chunked")
    });
    let rest = raw.get(split + 4..).unwrap_or_default();
    let body = if chunked {
        dechunk(rest).ok_or_else(|| invalid("the chunked body is malformed"))?
    } else {
        rest.to_vec()
    };
    Ok(Response { status, body })
}

/// Returns the body that the chunked encoding `raw` carries.
fn dechunk(mut raw: &[u8]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line_end = raw.windows(2).position(|window| window == b"\r\n")?;
        let size_text = std::str::from_utf8(raw.get(..line_end)?).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        raw = raw.get(line_end + 2..)?;
        if size == 0 {
            return Some(body);
        }
        body.extend_from_slice(raw.get(..size)?);
        raw = raw.get(size + 2..)?;
    }
}
