//! The `http` and `tcp` checks, which connect to 127.0.0.1, or to ::1
//! when 127.0.0.1 does not accept, with std's `TcpStream`. An HTTP check
//! writes one minimal HTTP/1.1 request. Without JSON expectations it reads
//! only the response's status line. With them it also reads the headers
//! and a bounded body, decoding chunked transfer coding itself, so phase
//! needs no HTTP client.

use std::io::{self, Read, Write as _};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{Attempt, Failure, MOST_BODY_BYTES, MOST_RESPONSE_BYTES};
use crate::cancel::Cancel;
use crate::clock::now;
use crate::plan::probe::{JsonExpectation, JsonScalar, is_sendable_path};

/// The longest that one read of a response blocks, so the read notices a
/// cancel or its deadline in good time.
const READ_SLICE: Duration = Duration::from_millis(50);

/// The most bytes of one chunk-size line of a chunked body, with its
/// extensions and line end. A longer line fails the attempt with
/// [`Failure::BadBody`].
const MOST_CHUNK_LINE_BYTES: usize = 1024;

/// The most bytes that one read of a response asks for.
const READ_PIECE: usize = 4096;

/// Connects to `port` on the loopback address within `timeout`. It tries
/// 127.0.0.1 first, and then ::1 with the time that is left when 127.0.0.1
/// refuses or fails, because a dev server such as Vite may listen only on
/// ::1. Each connection blocks in `connect_timeout`, so a cancel that comes
/// meanwhile takes effect only once the connections succeed or fail, at
/// most `timeout` later.
///
/// When both fail, it returns the failure of ::1 if that connection timed
/// out or failed for another reason than a refusal or a missing address,
/// and the failure of 127.0.0.1 otherwise.
pub(super) fn connect(port: u16, timeout: Duration) -> Result<TcpStream, Failure> {
    let deadline = now() + timeout;
    let first = match connect_to(SocketAddr::from((Ipv4Addr::LOCALHOST, port)), timeout) {
        Ok(stream) => return Ok(stream),
        Err(failure) => failure,
    };
    let Ok(rest) = left(deadline) else {
        return Err(first);
    };
    connect_to(SocketAddr::from((Ipv6Addr::LOCALHOST, port)), rest).map_err(|second| {
        let nothing_there = matches!(
            second,
            Failure::Connect(
                io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::AddrNotAvailable
                    | io::ErrorKind::NetworkUnreachable
                    | io::ErrorKind::HostUnreachable
            )
        );
        if nothing_there { first } else { second }
    })
}

/// Connects to `address` within `timeout`.
fn connect_to(address: SocketAddr, timeout: Duration) -> Result<TcpStream, Failure> {
    TcpStream::connect_timeout(&address, timeout).map_err(|error| match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Failure::TimedOut,
        kind => Failure::Connect(kind),
    })
}

/// Returns the request that an HTTP check sends for `path` to `port`. Its
/// `Host` header names `localhost`, which a dev server that checks the
/// header accepts whichever loopback address the connection reached.
pub(super) fn request(port: u16, path: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: localhost:{port}\r\nUser-Agent: phase-probe\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    )
}

/// One HTTP check's request and what it expects of the response.
pub(super) struct Get<'a> {
    /// The process's host port.
    pub(super) port: u16,
    /// The request path.
    pub(super) path: &'a str,
    /// The one status that passes, or `None` for any 2xx status.
    pub(super) status: Option<u16>,
    /// What the JSON body must hold. When it is empty, no body is read.
    pub(super) json: &'a [JsonExpectation],
}

/// Sends `GET path` to `port` on the loopback address that [`connect`]
/// reaches, and passes on the status and body that `get` expects, all
/// within `timeout` and until `cancel` is set. The connect step does not
/// see `cancel` until `connect_timeout` returns, which is at most `timeout`
/// later, and the read of the response sees it within [`READ_SLICE`].
pub(super) fn attempt(get: &Get<'_>, timeout: Duration, cancel: &Cancel) -> Attempt {
    send(get, timeout, cancel).into()
}

/// Sends the request and judges the response, failing with the reason.
fn send(get: &Get<'_>, timeout: Duration, cancel: &Cancel) -> Result<(), Failure> {
    // The plan refuses such a path. This check keeps one from reaching the
    // request line if a caller builds a check without a plan.
    if !is_sendable_path(get.path) {
        return Err(Failure::BadPath);
    }
    let deadline = now() + timeout;
    let mut stream = connect(get.port, timeout)?;
    stream
        .set_write_timeout(Some(left(deadline)?))
        .map_err(exchange)?;
    stream
        .write_all(request(get.port, get.path).as_bytes())
        .map_err(exchange)?;
    if get.json.is_empty() {
        let line = status_line(&mut stream, deadline, cancel)?;
        return judge_status(&line, get.status);
    }
    judge_response(&mut stream, get.status, get.json, deadline, cancel)
}

/// Judges the status line `line` against the one status `expected`, or
/// against any 2xx status when `expected` is `None`.
fn judge_status(line: &[u8], expected: Option<u16>) -> Result<(), Failure> {
    match (status(line), expected) {
        (None, _) => Err(Failure::BadResponse),
        (Some(got), Some(expected)) if got == expected => Ok(()),
        (Some(got), Some(expected)) => Err(Failure::StatusDiffers { expected, got }),
        (Some(code), None) if (200..300).contains(&code) => Ok(()),
        (Some(code), None) => Err(Failure::Status(code)),
    }
}

/// Reads `response` and judges it: first its status against `expected`,
/// and only on a passing status its headers and body, and then the body as
/// JSON against `json`. It reads at most [`MOST_RESPONSE_BYTES`] bytes of
/// status line and headers and [`MOST_BODY_BYTES`] bytes of decoded body,
/// before `deadline` and until `cancel` is set.
pub(super) fn judge_response(
    response: &mut impl Response,
    expected: Option<u16>,
    json: &[JsonExpectation],
    deadline: Instant,
    cancel: &Cancel,
) -> Result<(), Failure> {
    let mut reader = Reader::new(response, deadline, cancel);
    let line = reader.line(MOST_RESPONSE_BYTES, Failure::BadResponse)?;
    judge_status(&line, expected)?;
    let framing = reader.framing()?;
    let body = reader.body(framing)?;
    judge_json(&body, json)
}

/// Judges `body` as JSON against each expectation in `json`, in order. A
/// failure names the pointer only, so no byte of the body reaches it.
fn judge_json(body: &[u8], json: &[JsonExpectation]) -> Result<(), Failure> {
    let document: Value = serde_json::from_slice(body).map_err(|_| Failure::NotJson)?;
    for expectation in json {
        let Some(field) = document.pointer(expectation.pointer.as_str()) else {
            return Err(Failure::FieldMissing(expectation.pointer.clone()));
        };
        if let Some(expected) = &expectation.equals
            && !holds(field, expected)
        {
            return Err(Failure::FieldDiffers(expectation.pointer.clone()));
        }
    }
    Ok(())
}

/// Reports whether `field` holds `expected`. A number holds an integer
/// only when serde reads it as that same `i64`, so a number written with a
/// fraction or an exponent, such as `3.0`, holds none.
fn holds(field: &Value, expected: &JsonScalar) -> bool {
    match expected {
        JsonScalar::Bool(expected) => field.as_bool() == Some(*expected),
        JsonScalar::Integer(expected) => field.as_i64() == Some(*expected),
        JsonScalar::String(expected) => field.as_str() == Some(expected.as_str()),
        JsonScalar::Null => field.is_null(),
    }
}

/// How the body of a response ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    /// The body is the number of bytes that `Content-Length` gives.
    Length(usize),
    /// The body is in chunked transfer coding.
    Chunked,
    /// The body ends when the server closes the connection.
    Close,
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
/// holds neither. Each read is bounded as [`Reader::more`] describes.
pub(super) fn status_line(
    response: &mut impl Response,
    deadline: Instant,
    cancel: &Cancel,
) -> Result<Vec<u8>, Failure> {
    Reader::new(response, deadline, cancel).line(MOST_RESPONSE_BYTES, Failure::BadResponse)
}

/// A response being read, with the bytes read and not yet taken.
struct Reader<'a, R> {
    /// The response.
    response: &'a mut R,
    /// The time by which the attempt ends.
    deadline: Instant,
    /// The caller's cancel.
    cancel: &'a Cancel,
    /// The bytes read and not yet taken.
    buffered: Vec<u8>,
    /// The count of bytes taken so far.
    taken: usize,
}

impl<'a, R: Response> Reader<'a, R> {
    /// Returns a reader of `response` that has read nothing.
    const fn new(response: &'a mut R, deadline: Instant, cancel: &'a Cancel) -> Self {
        Self {
            response,
            deadline,
            cancel,
            buffered: Vec::new(),
            taken: 0,
        }
    }

    /// Reads at most `most` more bytes, which is more than zero, onto the
    /// buffered bytes, and returns how many it read, or 0 once the server
    /// has closed the connection. Before each read it limits the read to
    /// [`READ_SLICE`] or the time left, whichever is shorter, so it notices
    /// a cancel in good time and never reads past the deadline. A read
    /// that times out is tried again until the deadline.
    fn more(&mut self, most: usize) -> Result<usize, Failure> {
        let mut piece = [0_u8; READ_PIECE];
        let into = piece
            .get_mut(..most.clamp(1, READ_PIECE))
            .ok_or(Failure::BadResponse)?;
        loop {
            if self.cancel.is_cancelled() {
                return Err(Failure::Cancelled);
            }
            let slice = READ_SLICE.min(left(self.deadline)?);
            self.response.set_slice(slice).map_err(exchange)?;
            match self.response.read(into) {
                Ok(count) => {
                    self.buffered.extend(into.iter().take(count));
                    return Ok(count);
                }
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

    /// Takes the first `count` buffered bytes.
    fn take(&mut self, count: usize) -> Vec<u8> {
        self.taken += count;
        self.buffered.drain(..count).collect()
    }

    /// Takes the next line, which ends with CRLF or a bare LF within `most`
    /// bytes, and returns it without its end. It fails with `failure` when
    /// no line end comes within `most` bytes or the server closes first.
    fn line(&mut self, most: usize, failure: Failure) -> Result<Vec<u8>, Failure> {
        loop {
            if let Some(end) = self.buffered.iter().position(|byte| *byte == b'\n') {
                if end >= most {
                    return Err(failure);
                }
                let mut line = self.take(end + 1);
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(line);
            }
            if self.buffered.len() >= most || self.more(most - self.buffered.len())? == 0 {
                return Err(failure);
            }
        }
    }

    /// Takes exactly `count` bytes of body, reading no further than them,
    /// and fails with [`Failure::BadBody`] when the server closes first.
    fn exactly(&mut self, count: usize) -> Result<Vec<u8>, Failure> {
        while self.buffered.len() < count {
            if self.more(count - self.buffered.len())? == 0 {
                return Err(Failure::BadBody);
            }
        }
        Ok(self.take(count))
    }

    /// Reads the headers up to the blank line that ends them, within what
    /// is left of [`MOST_RESPONSE_BYTES`] after the status line, and
    /// returns how the body ends. A `Transfer-Encoding` whose last coding
    /// is `chunked` wins over `Content-Length`.
    fn framing(&mut self) -> Result<Framing, Failure> {
        let mut length = None;
        let mut coded = None;
        loop {
            let room = MOST_RESPONSE_BYTES.saturating_sub(self.taken);
            let line = self.line(room, Failure::BadBody)?;
            if line.is_empty() {
                break;
            }
            let text = std::str::from_utf8(&line).map_err(|_| Failure::BadBody)?;
            let (name, value) = text.split_once(':').ok_or(Failure::BadBody)?;
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                if !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(Failure::BadBody);
                }
                let read: usize = value.parse().map_err(|_| Failure::BadBody)?;
                if length.is_some_and(|length| length != read) {
                    return Err(Failure::BadBody);
                }
                length = Some(read);
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                let last = value.rsplit(',').next().unwrap_or_default().trim();
                coded = Some(last.eq_ignore_ascii_case("chunked"));
            }
        }
        Ok(match (coded, length) {
            (Some(true), _) => Framing::Chunked,
            (Some(false), _) | (None, None) => Framing::Close,
            (None, Some(length)) => Framing::Length(length),
        })
    }

    /// Reads the body that `framing` describes, taking at most
    /// [`MOST_BODY_BYTES`] bytes of it as sent, which for a chunked body
    /// counts each chunk-size line and line end. A `Content-Length` or a
    /// chunk that would pass the limit fails the attempt before the bytes
    /// are read, and a body that ends at the close reads at most one byte
    /// past the limit before it fails.
    fn body(&mut self, framing: Framing) -> Result<Vec<u8>, Failure> {
        match framing {
            Framing::Length(length) if length > MOST_BODY_BYTES => Err(Failure::BodyTooLong),
            Framing::Length(length) => self.exactly(length),
            Framing::Close => loop {
                if self.buffered.len() > MOST_BODY_BYTES {
                    return Err(Failure::BodyTooLong);
                }
                if self.more(MOST_BODY_BYTES + 1 - self.buffered.len())? == 0 {
                    let all = self.buffered.len();
                    return Ok(self.take(all));
                }
            },
            Framing::Chunked => self.chunks(),
        }
    }

    /// Decodes a chunked body up to its last, empty chunk. It reads no
    /// trailer, since the connection closes after the attempt. Each
    /// chunk-size line, chunk and line end counts against
    /// [`MOST_BODY_BYTES`], so a stream of small chunks with long
    /// extensions fails with [`Failure::BodyTooLong`] once it has taken the
    /// limit.
    fn chunks(&mut self) -> Result<Vec<u8>, Failure> {
        let start = self.taken;
        let mut body = Vec::new();
        loop {
            let room = MOST_BODY_BYTES.saturating_sub(self.taken - start);
            let most = MOST_CHUNK_LINE_BYTES.min(room);
            let line = match self.line(most, Failure::BadBody) {
                Err(Failure::BadBody)
                    if most < MOST_CHUNK_LINE_BYTES && self.buffered.len() >= most =>
                {
                    return Err(Failure::BodyTooLong);
                }
                read => read?,
            };
            let size = chunk_size(&line).ok_or(Failure::BadBody)?;
            if size == 0 {
                return Ok(body);
            }
            let room = MOST_BODY_BYTES.saturating_sub(self.taken - start);
            if size.checked_add(2).is_none_or(|sent| sent > room) {
                return Err(Failure::BodyTooLong);
            }
            body.extend(self.exactly(size)?);
            if !self.line(2, Failure::BadBody)?.is_empty() {
                return Err(Failure::BadBody);
            }
        }
    }
}

/// Returns the size of a chunk-size line such as `1a` or `1a;name=value`,
/// or `None` when the size is not hexadecimal digits that fit `usize`.
fn chunk_size(line: &[u8]) -> Option<usize> {
    let size = line.split(|byte| *byte == b';').next()?.trim_ascii();
    if size.is_empty() || !size.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    usize::from_str_radix(std::str::from_utf8(size).ok()?, 16).ok()
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

    use super::{
        Failure, MOST_BODY_BYTES, MOST_RESPONSE_BYTES, READ_SLICE, Response, chunk_size,
        judge_response, status, status_line,
    };
    use crate::cancel::Cancel;
    use crate::plan::probe::{JsonExpectation, JsonPointer};

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
        assert!(!response.slices.is_empty());
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

    /// A response of `head` followed by endless `x`, which counts the bytes
    /// read from it.
    struct Headed {
        head: &'static [u8],
        read: usize,
    }

    impl Read for Headed {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            for byte in buffer.iter_mut() {
                *byte = self.head.get(self.read).copied().unwrap_or(b'x');
                self.read += 1;
            }
            Ok(buffer.len())
        }
    }

    impl Response for Headed {
        fn set_slice(&mut self, _: Duration) -> io::Result<()> {
            Ok(())
        }
    }

    fn judge(response: &mut Headed) -> Result<(), Failure> {
        let json = [JsonExpectation::present(JsonPointer::new("/ok".to_owned()))];
        let deadline = Instant::now() + Duration::from_secs(5);
        judge_response(response, None, &json, deadline, &Cancel::new())
    }

    #[test]
    fn a_body_read_takes_at_most_one_byte_past_its_limit_from_the_response() {
        let head = b"HTTP/1.1 200 OK\r\n\r\n";
        let mut response = Headed { head, read: 0 };

        assert_eq!(judge(&mut response), Err(Failure::BodyTooLong));
        assert!(
            response.read <= head.len().max(MOST_RESPONSE_BYTES) + MOST_BODY_BYTES + 1,
            "read {}",
            response.read
        );
    }

    #[test]
    fn a_chunked_body_read_stops_at_a_chunk_that_passes_its_limit() {
        let mut response = Headed {
            head: b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n10001\r\n",
            read: 0,
        };

        assert_eq!(judge(&mut response), Err(Failure::BodyTooLong));
        assert!(
            response.read <= MOST_RESPONSE_BYTES,
            "read {}",
            response.read
        );
    }

    /// A response of `head` followed by `chunk` again and again, which
    /// counts the bytes read from it.
    struct Repeating {
        head: &'static [u8],
        chunk: Vec<u8>,
        read: usize,
    }

    impl Read for Repeating {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            for byte in buffer.iter_mut() {
                *byte = match self.read.checked_sub(self.head.len()) {
                    None => self.head.get(self.read).copied().unwrap_or_default(),
                    Some(at) => self
                        .chunk
                        .get(at % self.chunk.len())
                        .copied()
                        .unwrap_or_default(),
                };
                self.read += 1;
            }
            Ok(buffer.len())
        }
    }

    impl Response for Repeating {
        fn set_slice(&mut self, _: Duration) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_chunked_body_read_counts_chunk_lines_against_its_limit() {
        let head = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        let extension = "e".repeat(1000);
        let mut response = Repeating {
            head,
            chunk: format!("1;{extension}\r\nx\r\n").into_bytes(),
            read: 0,
        };
        let json = [JsonExpectation::present(JsonPointer::new("/ok".to_owned()))];
        let deadline = Instant::now() + Duration::from_secs(60);

        let judged = judge_response(&mut response, None, &json, deadline, &Cancel::new());

        assert_eq!(judged, Err(Failure::BodyTooLong));
        assert!(
            response.read <= MOST_RESPONSE_BYTES + MOST_BODY_BYTES,
            "read {}",
            response.read
        );
    }

    /// Judges a chunked response of one chunk that holds `{"ok":1}` and
    /// spaces, which with its framing takes `sent` bytes after the headers.
    fn judge_one_chunk_of(sent: usize) -> Result<(), Failure> {
        // The size line, the chunk's line end and the last chunk's line.
        let framing = "xxxx\r\n".len() + "\r\n".len() + "0\r\n".len();
        let data = format!("{{\"ok\":1{}}}", " ".repeat(sent - framing - 8));
        let wire = format!("{:04x}\r\n{data}\r\n0\r\n\r\n", data.len());
        let response = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{wire}");
        let json = [JsonExpectation::present(JsonPointer::new("/ok".to_owned()))];
        let deadline = Instant::now() + Duration::from_secs(5);
        judge_response(
            &mut response.as_bytes(),
            None,
            &json,
            deadline,
            &Cancel::new(),
        )
    }

    #[test]
    fn a_chunked_body_passes_at_its_limit_as_sent_and_fails_one_byte_past_it() {
        assert_eq!(judge_one_chunk_of(MOST_BODY_BYTES), Ok(()));
        assert_eq!(
            judge_one_chunk_of(MOST_BODY_BYTES + 1),
            Err(Failure::BodyTooLong)
        );
    }

    #[test]
    fn headers_longer_than_their_bound_fail_as_unreadable_headers() {
        let mut response = Headed {
            head: b"HTTP/1.1 200 OK\r\nX-Long: ",
            read: 0,
        };

        assert_eq!(judge(&mut response), Err(Failure::BadBody));
        assert!(
            response.read <= MOST_RESPONSE_BYTES,
            "read {}",
            response.read
        );
    }

    #[test]
    fn a_close_before_the_headers_end_fails_as_unreadable_headers() {
        let json = [JsonExpectation::present(JsonPointer::new("/ok".to_owned()))];
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut response = &b"HTTP/1.1 200 OK\r\nServer: x\r\n"[..];

        let judged = judge_response(&mut response, None, &json, deadline, &Cancel::new());

        assert_eq!(judged, Err(Failure::BadBody));
    }

    #[test]
    fn a_chunk_size_is_hexadecimal_digits_before_any_extension() {
        assert_eq!(chunk_size(b"1a"), Some(26));
        assert_eq!(chunk_size(b"1A;name=value"), Some(26));
        assert_eq!(chunk_size(b"0"), Some(0));
        assert_eq!(chunk_size(b""), None);
        assert_eq!(chunk_size(b"+1"), None);
        assert_eq!(chunk_size(b"zz"), None);
        assert_eq!(chunk_size(b"ffffffffffffffffff"), None);
    }
}
