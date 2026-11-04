//! The `http` and `tcp` checks, which connect to 127.0.0.1 with std's
//! `TcpStream`. An HTTP check writes one minimal HTTP/1.1 request and reads
//! only the response's status line, so phase needs no HTTP client.

use std::io::{self, Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use super::{Attempt, Failure, MOST_RESPONSE_BYTES};
use crate::clock::now;

/// Connects to 127.0.0.1 at `port` within `timeout`.
pub(super) fn connect(port: u16, timeout: Duration) -> Result<TcpStream, Failure> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&address, timeout).map_err(|error| match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Failure::TimedOut,
        kind => Failure::Connect(kind),
    })
}

/// Sends `GET path` to 127.0.0.1 at `port` and passes on a 2xx status,
/// all within `timeout`.
pub(super) fn attempt(port: u16, path: &str, timeout: Duration) -> Attempt {
    get(port, path, timeout).into()
}

/// Sends the request and checks the status, failing with the reason.
fn get(port: u16, path: &str, timeout: Duration) -> Result<(), Failure> {
    if path
        .chars()
        .any(|character| character == ' ' || character.is_control())
    {
        return Err(Failure::BadPath);
    }
    let deadline = now() + timeout;
    let mut stream = connect(port, timeout)?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUser-Agent: phase-probe\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    stream
        .set_write_timeout(Some(left(deadline)?))
        .map_err(exchange)?;
    stream.write_all(request.as_bytes()).map_err(exchange)?;
    let line = status_line(&mut stream, deadline)?;
    match status(&line) {
        Some(code) if (200..300).contains(&code) => Ok(()),
        Some(code) => Err(Failure::Status(code)),
        None => Err(Failure::BadResponse),
    }
}

/// Reads the response until the end of its status line, at most
/// [`MOST_RESPONSE_BYTES`] bytes, before `deadline`.
fn status_line(stream: &mut TcpStream, deadline: Instant) -> Result<Vec<u8>, Failure> {
    let mut read = Vec::new();
    let mut buffer = [0_u8; 512];
    loop {
        if let Some(end) = read.windows(2).position(|pair| pair == b"\r\n") {
            read.truncate(end);
            return Ok(read);
        }
        if read.len() >= MOST_RESPONSE_BYTES {
            return Err(Failure::BadResponse);
        }
        stream
            .set_read_timeout(Some(left(deadline)?))
            .map_err(exchange)?;
        let room = MOST_RESPONSE_BYTES
            .saturating_sub(read.len())
            .min(buffer.len());
        let into = buffer.get_mut(..room).ok_or(Failure::BadResponse)?;
        match stream.read(into) {
            Ok(0) => return Err(Failure::BadResponse),
            Ok(count) => read.extend(into.iter().take(count)),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(exchange(error)),
        }
    }
}

/// Returns the status code of an HTTP/1 status line such as
/// `HTTP/1.1 204 No Content`, or `None` when the line is not one.
fn status(line: &[u8]) -> Option<u16> {
    let rest = line
        .strip_prefix(b"HTTP/1.1 ")
        .or_else(|| line.strip_prefix(b"HTTP/1.0 "))?;
    let digits = rest.get(..3)?;
    if !digits.iter().all(u8::is_ascii_digit) || rest.get(3).is_some_and(|byte| *byte != b' ') {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// Returns the time left before `deadline`, or [`Failure::TimedOut`] when
/// none is left.
fn left(deadline: Instant) -> Result<Duration, Failure> {
    let left = deadline.saturating_duration_since(now());
    if left.is_zero() {
        Err(Failure::TimedOut)
    } else {
        Ok(left)
    }
}

/// Returns the failure of a request that could not be sent or a response
/// that could not be read.
fn exchange(error: io::Error) -> Failure {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Failure::TimedOut,
        kind => Failure::Exchange(kind),
    }
}

#[cfg(test)]
mod tests {
    use super::status;

    #[test]
    fn a_status_line_gives_its_code() {
        assert_eq!(status(b"HTTP/1.1 204 No Content"), Some(204));
        assert_eq!(status(b"HTTP/1.0 500"), Some(500));
    }

    #[test]
    fn a_line_that_is_not_an_http_1_status_line_gives_no_code() {
        assert_eq!(status(b"HTTP/2 200 OK"), None);
        assert_eq!(status(b"HTTP/1.1 20"), None);
        assert_eq!(status(b"HTTP/1.1 2000 OK"), None);
        assert_eq!(status(b"SSH-2.0-OpenSSH"), None);
    }
}
