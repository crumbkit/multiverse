//! The module that starts, inspects and signals an app's processes
//! (PROC-1). [`launch`] starts one process of a manifest, [`start_time`]
//! reads a process's start time from the process table, and [`signal`]
//! and [`group_exists`] reach a process's group.
//!
//! [`launch`] runs one process as follows (PROC-2):
//!
//! - Program: `/bin/sh`, the system's shell at a fixed path, runs the fixed
//!   script [`SCRIPT`]. The script runs the manifest's `argv` as separate
//!   values through `"$@"`, so no shell parses the manifest's words, and
//!   writes the program's exit code to the process's exit file when the
//!   program ends. The shell stays as the process's parent so that the code
//!   is recorded without a long-running mxv. The program is found on the
//!   `PATH` that the process receives, and whoever controls the manifest
//!   and that `PATH` supplies its bytes. The manifest is part of the
//!   repository, so a person who runs `mxv up` trusts the repository's
//!   code as they would when they run it by hand.
//! - Arguments: `-c`, the script, `mxv-process`, the exit file and the
//!   manifest's `argv`, each a separate value.
//! - Environment: the caller's environment, plus the manifest's `env` and
//!   one variable for each port the process receives.
//! - Streams and deadline: standard input is empty, and standard output and
//!   standard error are appended to the process's log file. The process
//!   has no deadline, because it runs until `mxv down` stops it.
//! - End: the process runs in a new process group whose id is the shell's
//!   process id, and it outlives the mxv that started it. `mxv down` sends
//!   SIGTERM to the group, waits, then sends SIGKILL, so the signals reach
//!   the program and every descendant that stays in the group. A
//!   descendant that starts a group of its own is not reached. The process
//!   leaves its log and its exit file. `mxv down` deletes the exit file and
//!   keeps the log, and `mxv remove` deletes both. The mxv that started the process reaps it when it ends while
//!   mxv waits for its readiness; after that mxv exits, the operating
//!   system reaps it.
//!
//! [`start_time`] reads `/proc/<pid>/stat` on Linux. Elsewhere it runs
//! `ps -o lstart= -p <pid>`, found on the caller's `PATH`, with `LC_ALL=C`
//! and `TZ=UTC0`, so the time it prints does not depend on the caller's
//! time zone, an empty standard input, the first
//! [`MOST_PS_BYTES`] bytes of standard output kept and standard error
//! discarded, and a deadline of [`PS_DEADLINE`], after which it kills and
//! reaps `ps`.

use std::fs::File;
use std::io::{self, Read as _};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use rustix::process::{Pid, Signal};

use crate::error::Error;
use crate::store;

#[cfg(test)]
mod tests;

/// The shell that runs [`SCRIPT`].
const SHELL: &str = "/bin/sh";

/// The script that runs a manifest's `argv` and records its exit code. The
/// shell's `$1` is the exit file, and the rest are the `argv`.
pub(crate) const SCRIPT: &str = r#"exit_file=$1; shift; "$@"; printf '%s\n' "$?" > "$exit_file""#;

/// The deadline of one `ps` run. Linux reads the start time from `/proc`
/// and runs no `ps`.
#[cfg_attr(
    target_os = "linux",
    expect(dead_code, reason = "Linux reads the start time from /proc")
)]
pub(crate) const PS_DEADLINE: Duration = Duration::from_secs(5);

/// The most bytes of `ps` output that [`start_time`] keeps.
const MOST_PS_BYTES: u64 = 4096;

/// What [`launch`] needs to start one process.
#[derive(Debug)]
pub(crate) struct Launch<'a> {
    /// The process's name in the manifest, for errors.
    pub(crate) name: &'a str,
    /// The program and its arguments.
    pub(crate) argv: &'a [String],
    /// The directory the process starts in.
    pub(crate) cwd: &'a Path,
    /// The variables the process receives on top of the caller's
    /// environment.
    pub(crate) env: &'a [(String, String)],
    /// The log file that standard output and standard error append to.
    pub(crate) log: &'a Path,
    /// The file that the script writes the exit code to.
    pub(crate) exit_file: &'a Path,
}

/// How the shell of a process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ended {
    /// The shell's exit code, or `None` when a signal ended it.
    pub(crate) code: Option<i32>,
    /// The signal that ended the shell, or `None`.
    pub(crate) signal: Option<i32>,
}

/// A process that [`launch`] started and this mxv has not reaped.
#[derive(Debug)]
pub(crate) struct Launched {
    /// The shell, which leads the process group.
    #[expect(
        clippy::disallowed_types,
        reason = "crates/multiverse/src/process.rs holds the child and owns its cleanup (PROC-1)"
    )]
    child: std::process::Child,
}

impl Launched {
    /// Returns the process id, which is also the process group's id.
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Reaps the process when it has ended, and returns how the shell
    /// ended then. It returns `None` while the process runs.
    pub(crate) fn ended(&mut self) -> Option<Ended> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(Ended {
                code: status.code(),
                signal: status.signal(),
            }),
            Ok(None) | Err(_) => None,
        }
    }

    /// Sends SIGKILL to the process's group and reaps the shell.
    pub(crate) fn kill(mut self) {
        let _ = signal(self.pid(), Signal::KILL);
        let _ = self.child.wait();
    }
}

/// Starts the process that `launch` describes, as the module docs say.
///
/// # Errors
///
/// Returns [`Error::Unwritable`] when the log cannot be opened, and
/// [`Error::ProcessUnstartable`] when the shell cannot start, such as when
/// the directory does not exist.
pub(crate) fn launch(launch: &Launch<'_>) -> Result<Launched, Error> {
    let log = store::append_file(launch.log)?;
    let errors = log.try_clone().map_err(|source| Error::Unwritable {
        path: launch.log.to_path_buf(),
        source,
    })?;
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/process.rs is the module that starts an app's processes (PROC-1)"
    )]
    let mut command = Command::new(SHELL);
    command
        .arg("-c")
        .arg(SCRIPT)
        .arg("mxv-process")
        .arg(launch.exit_file)
        .args(launch.argv)
        .current_dir(launch.cwd)
        .envs(launch.env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(errors)
        .process_group(0);
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/process.rs is the module that starts an app's processes (PROC-1)"
    )]
    let spawned = command.spawn();
    let child = spawned.map_err(|source| Error::ProcessUnstartable {
        process: launch.name.to_owned(),
        source,
    })?;
    Ok(Launched { child })
}

/// Returns the start time of the process `pid` in a form that does not
/// depend on the caller's time zone or locale, or `None` when no process
/// has the id. Run records store it as it is, and only compare it.
///
/// On Linux it is field 22 of `/proc/<pid>/stat`, the clock ticks from boot
/// to the process's start. Elsewhere it is what `ps -o lstart=` prints
/// with `TZ=UTC0` and `LC_ALL=C`, such as `Wed Nov  4 10:00:00 2026`.
///
/// # Errors
///
/// Returns [`Error::ProcessTable`] when the process table cannot be read,
/// or `ps` cannot start or misses its deadline.
pub(crate) fn start_time(pid: u32) -> Result<Option<String>, Error> {
    #[cfg(target_os = "linux")]
    {
        linux_start_time(pid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        ps_start_time(pid)
    }
}

/// Returns field 22 of `/proc/<pid>/stat`, or `None` when no process has
/// the id. The fields follow the last `)`, which ends the command name, and
/// the first of them is field 3.
#[cfg(target_os = "linux")]
fn linux_start_time(pid: u32) -> Result<Option<String>, Error> {
    let path = format!("/proc/{pid}/stat");
    let mut text = String::new();
    match File::open(&path) {
        Ok(file) => file
            .take(MOST_PS_BYTES)
            .read_to_string(&mut text)
            .map_err(|error| Error::ProcessTable {
                message: format!("cannot read {path}: {error}"),
            })?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Error::ProcessTable {
                message: format!("cannot open {path}: {error}"),
            });
        }
    };
    Ok(stat_start_time(&text))
}

/// Returns field 22 of `text`, the contents of `/proc/<pid>/stat`, which is
/// the process's start in clock ticks after boot. The command name in
/// field 2 may hold spaces and parentheses, so the fields are counted from
/// the last `)`, after which field 3 comes.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn stat_start_time(text: &str) -> Option<String> {
    let fields = text.rfind(')').and_then(|end| text.get(end + 1..))?;
    fields.split_whitespace().nth(22 - 3).map(str::to_owned)
}

/// Returns the start time that `ps -o lstart=` prints in UTC, or `None`
/// when no process has the id.
#[cfg(not(target_os = "linux"))]
fn ps_start_time(pid: u32) -> Result<Option<String>, Error> {
    use wait_timeout::ChildExt as _;

    let table = |message: String| Error::ProcessTable { message };
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/process.rs is the module that starts an app's processes (PROC-1)"
    )]
    let mut command = Command::new("ps");
    command
        .args(["-o", "lstart=", "-p"])
        .arg(pid.to_string())
        .env("LC_ALL", "C")
        .env("TZ", "UTC0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/process.rs is the module that starts an app's processes (PROC-1)"
    )]
    let spawned = command.spawn();
    #[expect(
        clippy::disallowed_types,
        reason = "crates/multiverse/src/process.rs holds the child and owns its deadline and cleanup (PROC-1)"
    )]
    let mut child: std::process::Child =
        spawned.map_err(|error| table(format!("cannot start ps: {error}")))?;
    let status = match child.wait_timeout(PS_DEADLINE) {
        Ok(Some(status)) => status,
        Ok(None) | Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(table(format!(
                "ps did not finish within {} seconds",
                PS_DEADLINE.as_secs()
            )));
        }
    };
    let mut text = String::new();
    if let Some(stdout) = child.stdout.take() {
        stdout
            .take(MOST_PS_BYTES)
            .read_to_string(&mut text)
            .map_err(|error| table(format!("cannot read the output of ps: {error}")))?;
    }
    let started = text.trim();
    if status.success() && !started.is_empty() {
        Ok(Some(started.to_owned()))
    } else {
        Ok(None)
    }
}

/// Returns `pid` as rustix's process id, or `None` for 0 or an id past
/// `i32::MAX`, which name no single process.
fn pid_of(pid: u32) -> Option<Pid> {
    i32::try_from(pid).ok().and_then(Pid::from_raw)
}

/// Sends `signal` to the process group `pid`.
///
/// # Errors
///
/// Returns the failure of `kill(2)`, such as `ESRCH` when the group is
/// gone.
pub(crate) fn signal(pid: u32, signal: Signal) -> io::Result<()> {
    let group = pid_of(pid).ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    rustix::process::kill_process_group(group, signal).map_err(io::Error::from)
}

/// Reports whether the process group `pid` still has a member. A group
/// that mxv may not signal still exists. When the group's leader is a
/// child of this mxv that has ended, the check reaps it first, because an
/// unreaped child stays a member of its group.
pub(crate) fn group_exists(pid: u32) -> bool {
    let Some(group) = pid_of(pid) else {
        return false;
    };
    let _ = rustix::process::waitpid(Some(group), rustix::process::WaitOptions::NOHANG);
    match rustix::process::test_kill_process_group(group) {
        Ok(()) => true,
        Err(error) => error != rustix::io::Errno::SRCH,
    }
}

/// Returns the exit code that the script wrote to `exit_file`, or `None`
/// when the file does not exist or holds no code.
pub(crate) fn exit_code(exit_file: &Path) -> Option<Option<i32>> {
    let mut text = String::new();
    File::open(exit_file)
        .ok()?
        .take(64)
        .read_to_string(&mut text)
        .ok()?;
    Some(text.trim().parse().ok())
}
