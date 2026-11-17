//! A tiny TCP server that the end-to-end tests of `mxv up` run as an app's
//! process. It listens on 127.0.0.1 at the port that the variable `PORT`
//! names, prints `listening on <port>`, and accepts connections until a
//! signal stops it. With `PORT` unset or not a port it exits with code 2.
//!
//! With no argument it closes each connection at once. With an argument it
//! answers each connection with an HTTP 200 response whose body is the
//! argument. The proxy tests use it to serve a page because it looks up no
//! host name: Python's `http.server` looks up the name of its address after
//! it binds and before it listens, and on a macOS runner that lookup can
//! outlast a unit's readiness wait.

use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::process::ExitCode;
use std::time::Duration;

/// How long the server waits for the rest of a request before it gives up
/// on the connection.
const READ_WAIT: Duration = Duration::from_secs(2);

/// The most bytes of one request that the server reads before it answers.
const MOST_REQUEST: usize = 64 * 1024;

fn main() -> ExitCode {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test server reads the port that mxv gives it"
    )]
    let port = std::env::var("PORT")
        .ok()
        .and_then(|port| port.parse::<u16>().ok());
    let Some(port) = port else {
        return ExitCode::from(2);
    };
    let body = std::env::args().nth(1);
    let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) else {
        return ExitCode::from(3);
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "the test server prints one line to its log"
    )]
    let mut out = std::io::stdout();
    let _ = writeln!(out, "listening on {port}");
    let _ = out.flush();
    for stream in listener.incoming() {
        if let (Ok(stream), Some(body)) = (stream, &body) {
            answer(stream, body);
        }
    }
    ExitCode::SUCCESS
}

/// Reads the request on `stream` up to the end of its headers, and writes
/// an HTTP 200 response whose body is `body`. A failed read or write ends
/// only this connection, so the server goes on to the next one.
fn answer(mut stream: TcpStream, body: &str) {
    if stream.set_read_timeout(Some(READ_WAIT)).is_err() {
        return;
    }
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") && request.len() < MOST_REQUEST {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(read) => request.extend_from_slice(chunk.get(..read).unwrap_or_default()),
        }
    }
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}
