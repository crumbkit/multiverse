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

/// Connects to 127.0.0.1 at `port` within `timeout`. The connection blocks
/// in `connect_timeout`, so a cancel that comes meanwhile takes effect only
/// once the connection succeeds or fails, at most `timeout` later.
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
/// all within `timeout` and until `cancel` is set. The connect step does not
/// see `cancel` until `connect_timeout` returns, which is at most `timeout`
/// later, and the read of the response sees it within [`READ_SLICE`].
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
    let line = status_line(&mut stream, deadline, cancel)?;
    match status(&line) {
        Some(code) if (200..300).contains(&code) => Ok(()),
        Some(code) => Err(Failure::Status(code)),
        None => Err(Failure::BadResponse),
    }
}

/// A response that a status line is read from, whose reads can be told how
/// long to block.
pub(super) trait Response: Read {
    /// Makes each later read block for at most `slice`, which is more than
    /// zero, and then fail with a timeout.
    ///
    /// # Errors
    ///
    /// Returns the error that kept the limit from being set.
    fn set_slice(&mut self, slice: Duration) -> io::Result<()>;
}

impl Response for TcpStream {
    fn set_slice(&mut self, slice: Duration) -> io::Result<()> {
        self.set_read_timeout(Some(slice))
    }
}

/// Reads `response` until the end of its status line, at most
/// [`MOST_RESPONSE_BYTES`] bytes, before `deadline` and until `cancel` is
/// set. The line may end with CRLF or a bare LF, and the line returned
/// holds neither. Before each read it limits the read to [`READ_SLICE`] or
/// the time left, whichever is shorter, so it notices a cancel in good time
/// and never reads past the deadline. A read that times out is tried again
/// until the deadline.
pub(super) fn status_line(
    response: &mut impl Response,
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
        let slice = READ_SLICE.min(left(deadline)?);
        response.set_slice(slice).map_err(exchange)?;
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

    use super::{Failure, MOST_RESPONSE_BYTES, READ_SLICE, Response, status, status_line};
    use crate::cancel::Cancel;

    impl Response for &[u8] {
        fn set_slice(&mut self, _: Duration) -> io::Result<()> {
            Ok(())
        }
    }

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

    impl Response for Endless {
        fn set_slice(&mut self, _: Duration) -> io::Result<()> {
            Ok(())
        }
    }

    /// A response that never answers, and records the time it is told to
    /// block each read and the time left until a deadline when it is told.
    struct Stalled {
        deadline: Instant,
        slices: Vec<(Duration, Duration)>,
    }

    impl Read for Stalled {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            let slice = self
                .slices
                .last()
                .map_or(Duration::ZERO, |(slice, _)| *slice);
            crate::clock::pause(slice);
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
    }

    impl Response for Stalled {
        fn set_slice(&mut self, slice: Duration) -> io::Result<()> {
            let left = self.deadline.saturating_duration_since(Instant::now());
            self.slices.push((slice, left));
            Ok(())
        }
    }

    #[test]
    fn a_status_line_read_never_blocks_past_its_deadline() {
        let deadline = Instant::now() + READ_SLICE + READ_SLICE / 2;
        let mut response = Stalled {
            deadline,
            slices: Vec::new(),
        };

        let read = status_line(&mut response, deadline, &Cancel::new());

        assert_eq!(read, Err(Failure::TimedOut));
        // The read sets each limit from the time left a moment before the
        // fake measures it, so a limit may exceed the measured time left by
        // that moment, which a busy machine stretches.
        let moment = Duration::from_millis(25);
        for (slice, left) in &response.slices {
            assert!(
                *slice <= *left + moment,
                "a read blocks {slice:?} with {left:?} left"
            );
        }
        // A busy machine may also run the read a little past its deadline.
        let margin = Duration::from_millis(500);
        assert!(Instant::now() < deadline + margin);
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
