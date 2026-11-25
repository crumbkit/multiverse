//! The test that each example of the `mxv` command prints the same bytes at a
//! terminal as in a pipe (CLI-11).
//!
//! The test runs each example twice in the fixture that the convention tests
//! build: once with standard output and standard error on pipes, and once
//! with each on a pseudo-terminal of its own. Standard input is `/dev/null`
//! in both runs, so no command asks a question. Both runs use the same
//! directories, so the paths that an example prints are the same. Both runs
//! set `TERM=xterm-256color`, as a person's shell does for a pipe too, so the
//! only difference between them is where the streams go, and clap styles its
//! help at the terminal as it does in a person's terminal.
//!
//! crumbkit's OUTPUT-1 lets colour and the width of a text table differ at a
//! terminal. The test therefore removes the colour escape codes from what the
//! terminal shows, and gives the terminal 1,000 columns, which is wider than
//! any table of the fixture. It also turns off the terminal's output
//! processing, so a newline reaches the test as one byte, as a pipe passes
//! it.

#![cfg(test)]
#![expect(
    clippy::disallowed_methods,
    reason = "the tests read the real clock and the files and directories that they and the code under test write"
)]

use std::fs::File;
use std::io::Read as _;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags};
use rustix::io::{Errno, FdFlags};
use rustix::process::{Pid, Signal};
use rustix::pty::OpenptFlags;
use rustix::termios::{OptionalActions, OutputModes, Winsize};

#[path = "support/fixture.rs"]
mod fixture;
#[path = "support/git.rs"]
mod support;

use crate::fixture::{ENV, Repositories, repositories};

/// The leaves whose examples the test does not run, each with the reason.
const SKIPPED: &[(&str, &str)] = &[(
    "proxy install",
    "Without a terminal the command still starts Caddy where Caddy is installed, and a test must not start a server on the machine that runs it.",
)];

/// The variables of every run beside [`ENV`] and the XDG directories: a
/// `TERM` that names a colour terminal, and a deadband socket that does not
/// exist, so `mxv exec` starts its command at once and never waits for a
/// place from the deadband daemon of the machine that runs the test.
const RUN_ENV: &[(&str, &str)] = &[
    ("TERM", "xterm-256color"),
    ("DEADBAND_SOCKET", "/nonexistent/deadband.sock"),
];

/// The XDG variables of every run, each with the directory of the run's
/// temporary directory that it names.
const XDG: &[(&str, &str)] = &[
    ("XDG_CONFIG_HOME", "config"),
    ("XDG_DATA_HOME", "data"),
    ("XDG_STATE_HOME", "state"),
    ("XDG_CACHE_HOME", "cache"),
];

/// How long one run may take, until both of its streams end, before the
/// test stops its process group.
const TIMEOUT: Duration = Duration::from_secs(30);

/// How long the test waits between two checks of whether a run whose
/// streams have ended has exited.
const POLL: Duration = Duration::from_millis(10);

/// How many times the test tries to open a pseudo-terminal before it fails.
const OPEN_ATTEMPTS: usize = 5;

/// What one run of an example printed, and how it ended.
#[derive(Debug, PartialEq, Eq)]
struct Ran {
    /// The exit status.
    status: ExitStatus,
    /// The bytes of standard output.
    stdout: Vec<u8>,
    /// The bytes of standard error.
    stderr: Vec<u8>,
}

#[test]
fn each_example_prints_the_same_bytes_at_a_terminal_as_in_a_pipe() {
    let repositories = repositories();
    let mut problems = Vec::new();
    let mut ran = 0;
    for leaf in mxv::TOOL.get_help().get_leaves() {
        let path = leaf.path().join(" ");
        if SKIPPED.iter().any(|(skipped, _)| *skipped == path) {
            continue;
        }
        for example in leaf.get_examples() {
            let words: Vec<&str> = leaf.path().iter().chain(example.words()).copied().collect();
            let line = format!("mxv {}", words.join(" "));
            let run = tempfile::tempdir().unwrap();
            let run_path = run.path().canonicalize().unwrap();
            let piped = run_once(&repositories, &run_path, &words, Streams::Pipes);
            let shown = run_once(&repositories, &run_path, &words, Streams::Terminals);
            let shown = Ran {
                status: shown.status,
                stdout: without_colour(&shown.stdout),
                stderr: without_colour(&shown.stderr),
            };
            ran += 1;
            if piped != shown {
                problems.push(format!(
                    "{line}\n  in a pipe: {}\n  standard output: {:?}\n  standard error: {:?}\n  at a terminal: {}\n  standard output: {:?}\n  standard error: {:?}",
                    piped.status,
                    String::from_utf8_lossy(&piped.stdout),
                    String::from_utf8_lossy(&piped.stderr),
                    shown.status,
                    String::from_utf8_lossy(&shown.stdout),
                    String::from_utf8_lossy(&shown.stderr),
                ));
            }
        }
    }
    assert!(ran > 0, "the test ran no example");
    assert!(
        problems.is_empty(),
        "these examples print other bytes at a terminal than in a pipe:\n{}",
        problems.join("\n")
    );
}

#[test]
fn each_skipped_leaf_names_a_leaf_with_examples() {
    let leaves = mxv::TOOL.get_help().get_leaves();
    for (skipped, reason) in SKIPPED {
        assert!(
            leaves
                .iter()
                .any(|leaf| leaf.path().join(" ") == *skipped && !leaf.get_examples().is_empty()),
            "the skipped leaf `{skipped}` names no leaf with examples, so its skip, whose reason is \"{reason}\", should go"
        );
    }
}

/// Where a run's standard output and standard error go.
#[derive(Debug, Clone, Copy)]
enum Streams {
    /// Each goes to a pipe.
    Pipes,
    /// Each goes to a pseudo-terminal of its own.
    Terminals,
}

/// Which stream a reader thread read.
#[derive(Debug, Clone, Copy)]
enum Stream {
    /// Standard output.
    Out,
    /// Standard error.
    Err,
}

/// Runs mxv with `words` in a fresh copy of `repositories` and of the XDG
/// fixture, both under `run`, which the run empties first, so two runs of
/// one example see the same paths.
fn run_once(repositories: &Repositories, run: &Path, words: &[&str], streams: Streams) -> Ran {
    for entry in std::fs::read_dir(run).unwrap() {
        std::fs::remove_dir_all(entry.unwrap().path()).unwrap();
    }
    let root = run.join("root");
    copy_dir(Path::new(&repositories.path), &root);
    let xdg = run.join("xdg");
    for (_, name) in XDG {
        let fixture = Path::new("tests/fixtures/xdg").join(name);
        if fixture.exists() {
            copy_dir(&fixture, &xdg.join(name));
        } else {
            std::fs::create_dir_all(xdg.join(name)).unwrap();
        }
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "the test runs the built binary with its streams on pseudo-terminals, which the snapshot harness cannot make"
    )]
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mxv"));
    command
        .args(words)
        .env_clear()
        .envs(ENV.iter().copied())
        .envs(RUN_ENV.iter().copied())
        .envs(
            XDG.iter()
                .map(|(variable, name)| (*variable, xdg.join(name))),
        )
        .current_dir(&root)
        .stdin(Stdio::null())
        // The run leads a process group of its own, so the test can stop
        // every process that it starts, which may hold its streams open.
        .process_group(0);
    let (out_read, out_write, err_read, err_write) = match streams {
        Streams::Pipes => {
            let (out_read, out_write) = std::io::pipe().unwrap();
            let (err_read, err_write) = std::io::pipe().unwrap();
            (
                OwnedFd::from(out_read),
                OwnedFd::from(out_write),
                OwnedFd::from(err_read),
                OwnedFd::from(err_write),
            )
        }
        Streams::Terminals => {
            let (out_controller, out_terminal) = pseudo_terminal();
            let (err_controller, err_terminal) = pseudo_terminal();
            (out_controller, out_terminal, err_controller, err_terminal)
        }
    };
    command
        .stdout(Stdio::from(out_write))
        .stderr(Stdio::from(err_write));
    #[expect(
        clippy::disallowed_methods,
        reason = "the test runs the built binary with its streams on pipes or pseudo-terminals"
    )]
    let child = command.spawn().unwrap();
    // The run's processes hold the only copies of the writing ends now, so
    // the reads end when they close them. Dropping the command closes the
    // copies that it kept for the spawn.
    drop(command);
    let (sender, receiver) = mpsc::channel();
    reader(out_read, Stream::Out, sender.clone());
    reader(err_read, Stream::Err, sender);
    let deadline = Instant::now() + TIMEOUT;
    let mut stdout = None;
    let mut stderr = None;
    while stdout.is_none() || stderr.is_none() {
        let left = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(left) {
            Ok((Stream::Out, bytes)) => stdout = Some(bytes),
            Ok((Stream::Err, bytes)) => stderr = Some(bytes),
            Err(RecvTimeoutError::Timeout) => stop(child, words),
            // A reader thread drops its sender without sending only when
            // it panics on a read error, which its panic names.
            Err(RecvTimeoutError::Disconnected) => {
                let killed =
                    rustix::process::kill_process_group(Pid::from_child(&child), Signal::KILL);
                panic!(
                    "the test could not read a stream of mxv {}, and stopped its process group: {killed:?}",
                    words.join(" ")
                );
            }
        }
    }
    let status = wait(child, deadline, words);
    Ran {
        status,
        stdout: stdout.unwrap_or_default(),
        stderr: stderr.unwrap_or_default(),
    }
}

/// Opens a pseudo-terminal of 1,000 columns without output processing, and
/// returns its controlling end and its terminal end.
fn pseudo_terminal() -> (OwnedFd, OwnedFd) {
    let controller = open_controller();
    rustix::pty::grantpt(&controller).unwrap();
    rustix::pty::unlockpt(&controller).unwrap();
    let name = rustix::pty::ptsname(&controller, Vec::new()).unwrap();
    let terminal = rustix::fs::open(
        name.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .unwrap();
    let mut settings = rustix::termios::tcgetattr(&terminal).unwrap();
    settings.output_modes.remove(OutputModes::OPOST);
    rustix::termios::tcsetattr(&terminal, OptionalActions::Now, &settings).unwrap();
    rustix::termios::tcsetwinsize(
        &terminal,
        Winsize {
            ws_row: 50,
            ws_col: 1000,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();
    (controller, terminal)
}

/// Opens the controlling end of a new pseudo-terminal. On a Mac under load,
/// `posix_openpt` fails now and then, in about one call of 800 when 24
/// processes open pseudo-terminals at once, though a pseudo-terminal is
/// free. The test opens about 60 per run and nextest retries no test, so it
/// tries [`OPEN_ATTEMPTS`] times and fails only when every attempt fails.
fn open_controller() -> OwnedFd {
    let mut errors = Vec::new();
    for _ in 0..OPEN_ATTEMPTS {
        match rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY) {
            Ok(controller) => {
                // `posix_openpt` on macOS cannot set close-on-exec, so the
                // test sets it, and mxv and the processes it starts do not
                // inherit the controlling end.
                rustix::io::fcntl_setfd(&controller, FdFlags::CLOEXEC).unwrap();
                return controller;
            }
            Err(error) => errors.push(error),
        }
    }
    panic!("the test could not open a pseudo-terminal in {OPEN_ATTEMPTS} attempts: {errors:?}");
}

/// Starts a thread that reads `stream` to its end and sends the bytes on
/// `sender`, tagged with `which`. A pseudo-terminal's controlling end on
/// Linux reads as `EIO` once the terminal end has closed, which the thread
/// takes as the end.
fn reader(stream: OwnedFd, which: Stream, sender: Sender<(Stream, Vec<u8>)>) {
    thread::spawn(move || {
        let mut file = File::from(stream);
        let mut bytes = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            match file.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => bytes.extend_from_slice(&chunk[..count]),
                Err(error) if error.raw_os_error() == Some(Errno::IO.raw_os_error()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => panic!("the test could not read a stream: {error}"),
            }
        }
        // The receiver is gone only when the run already failed the test.
        sender.send((which, bytes)).ok();
    });
}

/// Waits for `child`, whose streams have ended, to exit, and stops its
/// process group and fails the test when it is still running at
/// `deadline`. A process exits a moment after it closes its streams, so the
/// test asks again after each short pause.
#[expect(
    clippy::disallowed_types,
    reason = "the test holds the child that it ran on pseudo-terminals, which the library's process module cannot make"
)]
fn wait(mut child: std::process::Child, deadline: Instant, words: &[&str]) -> ExitStatus {
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            stop(child, words);
        }
        #[expect(
            clippy::disallowed_methods,
            reason = "a child's exit gives no signal that the test can wait on without a second thread, so the test polls try_wait"
        )]
        thread::sleep(POLL);
    }
}

/// Stops the process group that `child` leads, which ran longer than
/// [`TIMEOUT`], and fails the test naming the example. The test has not yet
/// reaped `child`, so its process id still names its group.
#[expect(
    clippy::disallowed_types,
    reason = "the test holds the child that it ran on pseudo-terminals, which the library's process module cannot make"
)]
fn stop(mut child: std::process::Child, words: &[&str]) -> ! {
    let killed = rustix::process::kill_process_group(Pid::from_child(&child), Signal::KILL);
    let reaped = child.wait();
    panic!(
        "mxv {} ran longer than {TIMEOUT:?}, and the test stopped its process group: {killed:?}, {reaped:?}",
        words.join(" ")
    );
}

/// Returns `bytes` without the escape codes that set colours and styles,
/// which start with `ESC [`, hold digits, semicolons and colons, and end
/// with `m`, such as `ESC [1m` and `ESC [4:3m`.
fn without_colour(bytes: &[u8]) -> Vec<u8> {
    let mut kept = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some((&byte, after)) = rest.split_first() {
        if byte == 0x1b
            && let Some(after) = after.strip_prefix(b"[")
        {
            let length = after
                .iter()
                .take_while(|byte| byte.is_ascii_digit() || **byte == b';' || **byte == b':')
                .count();
            if after.get(length) == Some(&b'm') {
                rest = &after[length + 1..];
                continue;
            }
        }
        kept.push(byte);
        rest = after;
    }
    kept
}

/// Copies the directory tree at `from` to `to`, which must not exist.
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            #[expect(
                clippy::disallowed_methods,
                reason = "the test copies its fixture into a temporary directory"
            )]
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}
