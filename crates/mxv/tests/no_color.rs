//! The `NO_COLOR` test of the `mxv` command (CLI-14). Each run puts
//! standard input and standard output on one pseudo-terminal and standard
//! error on another, so mxv sees a terminal as it does in a person's shell.
//! Help on standard output carries clap's styles at a terminal, and with
//! `NO_COLOR=1` it carries no escape code. Help writes nothing to standard
//! error, so the check there only guards against a future warning that
//! carries colour.
//!
//! The test opens the pseudo-terminals with rustix's safe functions, so it
//! runs on macOS and Linux and needs no `script` command. Each terminal is
//! in raw mode, so it passes mxv's bytes through unchanged.

// clippy allows `unwrap` only in code under `cfg(test)`, and an integration
// test's helper functions are not under it without this attribute.
#![cfg(test)]
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs::File;
use std::io::{self, ErrorKind, Read as _};
use std::os::fd::OwnedFd;
use std::process::{Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};

use rustix::fs::{Mode, OFlags};
use rustix::io::{Errno, FdFlags, fcntl_setfd};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{OptionalActions, Winsize, tcgetattr, tcsetattr, tcsetwinsize};
use tempfile::TempDir;

/// The terminal type each run sees. clap styles its output only when
/// `TERM` names a terminal that supports colour.
const TERM: &str = "xterm-256color";

/// The byte that starts every ANSI escape code.
const ESCAPE: u8 = 0x1b;

/// The most times [`pty`] calls `openpt` before it gives up. On a busy Mac
/// `openpt` sometimes fails with the unknown error -6 when many processes
/// open pseudo-terminals at once: 21 of 1,000 runs with 24 at once failed
/// that way, and none of 300 runs one after another did. The test retries
/// that one call, which allocates a shared resource, and never the test
/// itself (BASE-20).
const OPENPT_ATTEMPTS: usize = 5;

/// One pseudo-terminal: the side the test reads and the side mxv writes to.
struct Pty {
    /// The side that the test reads mxv's bytes from.
    main: OwnedFd,
    /// The terminal that mxv receives as a standard stream.
    terminal: OwnedFd,
}

/// Opens a pseudo-terminal in raw mode, 80 columns wide, with both sides
/// closed in any program that the test starts.
fn pty() -> io::Result<Pty> {
    let main = open_main()?;
    fcntl_setfd(&main, FdFlags::CLOEXEC)?;
    grantpt(&main)?;
    unlockpt(&main)?;
    let name = ptsname(&main, Vec::new())?;
    let terminal = rustix::fs::open(
        name.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut termios = tcgetattr(&terminal)?;
    termios.make_raw();
    tcsetattr(&terminal, OptionalActions::Now, &termios)?;
    let size = Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    tcsetwinsize(&terminal, size)?;
    Ok(Pty { main, terminal })
}

/// Opens the main side of a new pseudo-terminal, calling `openpt` up to
/// [`OPENPT_ATTEMPTS`] times and returning the last failure.
fn open_main() -> io::Result<OwnedFd> {
    let mut failure = None;
    for _ in 0..OPENPT_ATTEMPTS {
        match openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY) {
            Ok(main) => return Ok(main),
            Err(error) => failure = Some(error),
        }
    }
    Err(failure.map_or_else(
        || io::Error::other("openpt was never called"),
        io::Error::from,
    ))
}

/// Reads everything mxv writes to `main` on a thread of its own, so a run
/// that fills a terminal's buffer does not wait on the test. Reading ends
/// when every copy of the terminal side is closed, which Linux reports as
/// an `EIO` error and macOS as the end of the file.
fn drain(main: OwnedFd) -> JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut file = File::from(main);
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            match file.read(&mut buffer) {
                Ok(0) => return Ok(bytes),
                Ok(read) => bytes.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) if error.raw_os_error() == Some(Errno::IO.raw_os_error()) => {
                    return Ok(bytes);
                }
                Err(error) => return Err(error),
            }
        }
    })
}

/// Returns the bytes that the thread of [`drain`] read.
fn collect(reader: JoinHandle<io::Result<Vec<u8>>>) -> io::Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| io::Error::other("the thread that reads a pseudo-terminal panicked"))?
}

/// What one run printed on standard output and standard error, and how it
/// ended.
struct Run {
    /// The bytes on standard output.
    stdout: Vec<u8>,
    /// The bytes on standard error.
    stderr: Vec<u8>,
    /// How the run ended.
    status: ExitStatus,
}

/// Runs the built `mxv` binary with `args` at pseudo-terminals, with a
/// cleared environment that holds `TERM`, `HOME` and the XDG directories in
/// a fresh temporary directory, and the variables `env` added.
fn mxv(args: &[&str], env: &[(&str, &str)]) -> io::Result<Run> {
    let home = TempDir::new()?;
    let out = pty()?;
    let err = pty()?;
    let input = out.terminal.try_clone()?;
    #[expect(
        clippy::disallowed_methods,
        reason = "the test runs the built binary with its standard streams on pseudo-terminals, which snapbox cannot set"
    )]
    let mut command = Command::new(env!("CARGO_BIN_EXE_mxv"));
    command
        .args(args)
        .current_dir(home.path())
        .env_clear()
        .env("TERM", TERM)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("XDG_STATE_HOME", home.path().join("state"))
        .envs(env.iter().copied())
        .stdin(Stdio::from(input))
        .stdout(Stdio::from(out.terminal))
        .stderr(Stdio::from(err.terminal));
    let stdout = drain(out.main);
    let stderr = drain(err.main);
    #[expect(
        clippy::disallowed_methods,
        reason = "the test waits for the built binary, which prints help and reads no input"
    )]
    let status = command.status()?;
    // The command holds the test's copies of the terminal sides, and
    // dropping it closes them, so reading ends now that mxv has exited.
    drop(command);
    Ok(Run {
        stdout: collect(stdout)?,
        stderr: collect(stderr)?,
        status,
    })
}

/// Reports whether `bytes` hold an ANSI escape code.
fn has_escape(bytes: &[u8]) -> bool {
    bytes.contains(&ESCAPE)
}

#[test]
fn help_at_a_terminal_carries_colour_so_the_test_reaches_a_real_terminal() {
    let help = mxv(&["--help"], &[]).unwrap();

    assert!(
        help.status.success(),
        "mxv --help ends with {}",
        help.status
    );
    assert!(
        has_escape(&help.stdout),
        "help at a terminal has no colour, so mxv does not see a terminal: {:?}",
        String::from_utf8_lossy(&help.stdout)
    );
}

#[test]
fn help_at_a_terminal_under_no_color_carries_no_escape_code() {
    let help = mxv(&["--help"], &[("NO_COLOR", "1")]).unwrap();

    assert!(
        help.status.success(),
        "mxv --help ends with {}",
        help.status
    );
    assert!(!help.stdout.is_empty(), "mxv --help prints nothing");
    assert!(
        !has_escape(&help.stdout) && !has_escape(&help.stderr),
        "help at a terminal under NO_COLOR has an escape code: {:?} {:?}",
        String::from_utf8_lossy(&help.stdout),
        String::from_utf8_lossy(&help.stderr)
    );
}
