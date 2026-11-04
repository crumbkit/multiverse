//! A tiny TCP server that the end-to-end tests of `mxv up` run as an app's
//! process. It listens on 127.0.0.1 at the port that the variable `PORT`
//! names, prints `listening on <port>`, and accepts connections until a
//! signal stops it. With `PORT` unset or not a port it exits with code 2.

use std::io::Write as _;
use std::net::{Ipv4Addr, TcpListener};
use std::process::ExitCode;

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
        drop(stream);
    }
    ExitCode::SUCCESS
}
