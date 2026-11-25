//! The test that each example of the `mxv` command prints the same bytes at a
//! terminal as in a pipe (CLI-11).
//!
//! The test runs each example twice in the fixture that the convention tests
//! build: once with standard output and standard error on pipes, and once
//! with each on a pseudo-terminal of its own. Standard input is an empty pipe
//! in both runs, so no command asks a question. Both runs use the same
//! directories, so the paths that an example prints are the same.
//!
//! crumbkit's OUTPUT-1 lets colour and the width of a text table differ at a
//! terminal. The test therefore removes the colour escape codes from what the
//! terminal shows, and gives the terminal 1,000 columns, which is wider than
//! any table of the fixture. It also turns off the terminal's output
//! processing, so a newline reaches the test as one byte, as a pipe passes
//! it.

#![cfg(test)]

use std::fs::File;
use std::io::Read as _;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::process::{Child, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;
use rustix::process::{Pid, Signal};
use rustix::pty::OpenptFlags;
use rustix::termios::{OptionalActions, OutputModes, Winsize};
use tempfile::TempDir;

#[path = "support/git.rs"]
mod support;

use crate::support::{git, repository};

/// The leaves whose examples the test does not run, each with the reason.
/// They are the leaves whose examples the convention tests do not run.
const SKIPPED: &[(&str, &str)] = &[
    ("up", NO_LIVE_CHECKOUT),
    ("restart", NO_LIVE_CHECKOUT),
    ("logs", NO_LIVE_CHECKOUT),
    ("exec", NO_LIVE_CHECKOUT),
    ("ports reset", NO_LIVE_CHECKOUT),
    (
        "add",
        "The example adds a checkout to a workspace of clones, and the fixture's only workspace holds a git worktree under /home/agent, which a test cannot create.",
    ),
    (
        "archive",
        "The example archives a live workspace, and the fixture's workspace names a checkout under /home/agent, which a test cannot create.",
    ),
    (
        "self usage",
        "The output names the day the report starts, so a run just after midnight would differ from the run before it.",
    ),
    (
        "self figures list",
        "The figures come from the usage log of the machine that runs the test, and the first run adds to it.",
    ),
    (
        "self runs list",
        "The runs come from the usage log of the machine that runs the test, and the first run adds to it.",
    ),
    (
        "proxy trust",
        "The command asks a person at a terminal and raises the system's password prompt.",
    ),
    (
        "proxy install",
        "The command needs a Caddy program and asks a person at a terminal.",
    ),
    (
        "proxy check",
        "The command exits with 1 while anything is missing, and the fixture's machine has no Caddy.",
    ),
];

/// The reason that a process verb's examples do not run.
const NO_LIVE_CHECKOUT: &str = "The example starts or reads the processes of a live checkout with an mxv.toml, and the fixture's workspace names a checkout under /home/agent, which a test cannot create.";

/// The variables of every run, which the convention tests also set.
const ENV: &[(&str, &str)] = &[
    ("PATH", support::PATH),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
];

/// The XDG variables of every run, each with the directory of the run's
/// temporary directory that it names.
const XDG: &[(&str, &str)] = &[
    ("XDG_CONFIG_HOME", "config"),
    ("XDG_DATA_HOME", "data"),
    ("XDG_STATE_HOME", "state"),
    ("XDG_CACHE_HOME", "cache"),
];

/// How long one run may take before the test stops it.
const TIMEOUT: Duration = Duration::from_secs(30);

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
    for (skipped, _) in SKIPPED {
        assert!(
            leaves
                .iter()
                .any(|leaf| leaf.path().join(" ") == *skipped && !leaf.get_examples().is_empty()),
            "the skipped leaf `{skipped}` names no leaf with examples"
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

/// Runs mxv with `words` in a fresh copy of `repositories` and of the XDG
/// fixture, both under `run`, which the run empties first, so two runs of
/// one example see the same paths.
fn run_once(repositories: &Repositories, run: &Path, words: &[&str], streams: Streams) -> Ran {
    for entry in std::fs::read_dir(run).unwrap() {
        std::fs::remove_dir_all(entry.unwrap().path()).unwrap();
    }
    let root = run.join("root");
    copy_dir(repositories.dir.path(), &root);
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
        .envs(
            XDG.iter()
                .map(|(variable, name)| (*variable, xdg.join(name))),
        )
        .current_dir(&root)
        .stdin(Stdio::piped());
    match streams {
        Streams::Pipes => {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
            #[expect(
                clippy::disallowed_methods,
                reason = "the test runs the built binary with its streams on pipes"
            )]
            let mut child = command.spawn().unwrap();
            drop(child.stdin.take());
            let stdout = reader(child.stdout.take().unwrap().into());
            let stderr = reader(child.stderr.take().unwrap().into());
            let status = wait(child, words);
            Ran {
                status,
                stdout: stdout.join().unwrap(),
                stderr: stderr.join().unwrap(),
            }
        }
        Streams::Terminals => {
            let (out_controller, out_terminal) = pseudo_terminal();
            let (err_controller, err_terminal) = pseudo_terminal();
            command
                .stdout(Stdio::from(out_terminal))
                .stderr(Stdio::from(err_terminal));
            #[expect(
                clippy::disallowed_methods,
                reason = "the test runs the built binary with its streams on pseudo-terminals"
            )]
            let mut child = command.spawn().unwrap();
            // The command holds the only copies of the terminal ends now, so
            // the reads end when it exits. Dropping the command closes the
            // copies that it kept for the spawn.
            drop(command);
            drop(child.stdin.take());
            let stdout = reader(out_controller);
            let stderr = reader(err_controller);
            let status = wait(child, words);
            Ran {
                status,
                stdout: stdout.join().unwrap(),
                stderr: stderr.join().unwrap(),
            }
        }
    }
}

/// Opens a pseudo-terminal of 1,000 columns without output processing, and
/// returns its controlling end and its terminal end.
fn pseudo_terminal() -> (OwnedFd, OwnedFd) {
    let controller = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
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

/// Starts a thread that reads `stream` to its end and returns the bytes. A
/// pseudo-terminal's controlling end on Linux reads as `EIO` once the
/// terminal end has closed, which the thread takes as the end.
fn reader(stream: OwnedFd) -> JoinHandle<Vec<u8>> {
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
        bytes
    })
}

/// Waits for `child` to exit, and stops it and fails the test when it runs
/// longer than [`TIMEOUT`].
#[expect(
    clippy::disallowed_types,
    reason = "the test holds the child that it ran on pseudo-terminals, which the library's process module cannot make"
)]
fn wait(mut child: Child, words: &[&str]) -> ExitStatus {
    let pid = Pid::from_child(&child);
    let (sender, receiver) = mpsc::channel::<()>();
    let waiter = thread::spawn(move || {
        let status = child.wait().unwrap();
        // Dropping the sender tells the receiver that the run has ended.
        drop(sender);
        status
    });
    if receiver.recv_timeout(TIMEOUT) == Err(RecvTimeoutError::Timeout) {
        let killed = rustix::process::kill_process(pid, Signal::KILL);
        panic!(
            "mxv {} ran longer than {TIMEOUT:?}, and the test stopped it: {killed:?}",
            words.join(" ")
        );
    }
    waiter.join().unwrap()
}

/// Returns `bytes` without the escape codes that set colours and styles,
/// which start with `ESC [`, hold digits and semicolons, and end with `m`.
fn without_colour(bytes: &[u8]) -> Vec<u8> {
    let mut kept = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some((&byte, after)) = rest.split_first() {
        if byte == 0x1b
            && let Some(after) = after.strip_prefix(b"[")
        {
            let length = after
                .iter()
                .take_while(|byte| byte.is_ascii_digit() || **byte == b';')
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

/// The directory of repositories that each run starts in a fresh copy of,
/// which the test deletes when it drops.
#[derive(Debug)]
struct Repositories {
    /// The temporary directory.
    dir: TempDir,
}

/// Returns a new directory of the repositories that the convention tests
/// build: a bare `origin.git`, its clones `app` and `web`, a repository
/// `plain` without an origin, and a clone `fresh` of an origin without
/// commits. The clone `app` also holds a branch named `taken`.
fn repositories() -> Repositories {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let app = repository(&root);
    git(&app, &["branch", "taken"]);
    git(&root, &["clone", "--quiet", "origin.git", "web"]);
    git(
        &root.join("web"),
        &["remote", "set-url", "origin", "../origin.git"],
    );
    let plain = root.join("plain");
    std::fs::create_dir(&plain).unwrap();
    git(&plain, &["init", "--quiet"]);
    git(
        &root,
        &[
            "init",
            "--quiet",
            "--bare",
            "--initial-branch=main",
            "empty.git",
        ],
    );
    git(&root, &["clone", "--quiet", "empty.git", "fresh"]);
    git(
        &root.join("fresh"),
        &["remote", "set-url", "origin", "../empty.git"],
    );
    Repositories { dir }
}
