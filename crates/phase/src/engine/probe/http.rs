//! The `http` and `tcp` checks, which connect to 127.0.0.1 with std's
//! `TcpStream`. An HTTP check writes one minimal HTTP/1.1 request and reads
//! only the response's status line, so phase needs no HTTP client.

use std::io::{self, Read, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use super::{Attempt, Failure, MOST_RESPONSE_BYTES};
use crate::cancel::Cancel;
use crate::clock::now;
use crate::plan::probe::is_sendable_path;

/// The longest that one read of a response blocks, so the read notices a
/// cancel or its deadline in good time.
const READ_SLICE: Duration = Duration::from_millis(50);

/// Connects to 127.0.0.1 at `port` within `timeout`.
pub(super) fn connect(port: u16, timeout: Duration) -> Result<TcpStream, Failure> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&address, timeout).map_err(|error| match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Failure::TimedOut,
        kind => Failure::Connect(kind),
    })
}

/// Returns the request that an HTTP check sends for `path` to `port`.
pub(super) fn request(port: u16, path: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUser-Agent: phase-probe\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    )
}

/// Sends `GET path` to 127.0.0.1 at `port` and passes on a 2xx status,
/// all within `timeout` and until `cancel` is set.
pub(super) fn attempt(port: u16, path: &str, timeout: Duration, cancel: &Cancel) -> Attempt {
    get(port, path, timeout, cancel).into()
}

/// Sends the request and checks the status, failing with the reason.
fn get(port: u16, path: &str, timeout: Duration, cancel: &Cancel) -> Result<(), Failure> {
    // The plan refuses such a path. This check keeps one from reaching the
    // request line if a caller builds a check without a plan.
    if !is_sendable_path(path) {
        return Err(Failure::BadPath);
    }
    let deadline = now() + timeout;
    let mut stream = connect(port, timeout)?;
    stream
        .set_write_timeout(Some(left(deadline)?))
        .map_err(exchange)?;
    stream
        .write_all(request(port, path).as_bytes())
        .map_err(exchange)?;
    stream
        .set_read_timeout(Some(READ_SLICE))
        .map_err(exchange)?;
    let line = status_line(&mut stream, deadline, cancel)?;
    match status(&line) {
        Some(code) if (200..300).contains(&code) => Ok(()),
        Some(code) => Err(Failure::Status(code)),
        None => Err(Failure::BadResponse),
    }
}

/// Reads `response` until the end of its status line, at most
/// [`MOST_RESPONSE_BYTES`] bytes, before `deadline` and until `cancel` is
/// set. The line may end with CRLF or a bare LF, and the line returned
/// holds neither. A read that times out is tried again until the deadline,
/// so `response` should time out its reads after a short slice.
pub(super) fn status_line(
    response: &mut impl Read,
    deadline: Instant,
    cancel: &Cancel,
) -> Result<Vec<u8>, Failure> {
    let mut read = Vec::new();
    let mut buffer = [0_u8; 512];
    loop {
        if let Some(end) = read.iter().position(|byte| *byte == b'\n') {
            read.truncate(end);
            if read.last() == Some(&b'\r') {
                read.pop();
            }
            return Ok(read);
        }
        if read.len() >= MOST_RESPONSE_BYTES {
            return Err(Failure::BadResponse);
        }
        if cancel.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        left(deadline)?;
        let room = MOST_RESPONSE_BYTES
            .saturating_sub(read.len())
            .min(buffer.len());
        let into = buffer.get_mut(..room).ok_or(Failure::BadResponse)?;
        match response.read(into) {
            Ok(0) => return Err(Failure::BadResponse),
            Ok(count) => read.extend(into.iter().take(count)),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) => {}
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
    use std::io::{self, Read};
    use std::time::{Duration, Instant};

    use super::{Failure, MOST_RESPONSE_BYTES, status, status_line};
    use crate::cancel::Cancel;

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

    /// An endless response of `x` that counts the bytes read from it.
    struct Endless(usize);

    impl Read for Endless {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            buffer.fill(b'x');
            self.0 += buffer.len();
            Ok(buffer.len())
        }
    }

    #[test]
    fn a_status_line_read_takes_no_more_than_its_bound_from_the_response() {
        let mut response = Endless(0);
        let deadline = Instant::now() + Duration::from_secs(5);

        let read = status_line(&mut response, deadline, &Cancel::new());

        assert_eq!(read, Err(Failure::BadResponse));
        assert!(response.0 <= MOST_RESPONSE_BYTES, "read {}", response.0);
    }

    #[test]
    fn a_status_line_may_end_with_a_bare_line_feed() {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut bare = &b"HTTP/1.1 200 OK\nServer: x\n\n"[..];
        let mut crlf = &b"HTTP/1.1 200 OK\r\n\r\n"[..];

        assert_eq!(
            status_line(&mut bare, deadline, &Cancel::new()).unwrap(),
            b"HTTP/1.1 200 OK"
        );
        assert_eq!(
            status_line(&mut crlf, deadline, &Cancel::new()).unwrap(),
            b"HTTP/1.1 200 OK"
        );
    }
}
