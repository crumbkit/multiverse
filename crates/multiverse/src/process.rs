//! The module that starts, inspects and signals processes outside phase
//! (PROC-1). [`launch`] starts the log follower of a container that an
//! older mxv started, [`start_time`] reads a process's start time from the
//! process table, and [`signal`] and [`group_exists`] reach a process's
//! group.
//!
//! [`launch`] runs one process as follows (PROC-2):
//!
//! - Program: `/bin/sh`, the system's shell at a fixed path, runs the fixed
//!   script [`SCRIPT`]. The script runs the `argv` as separate values
//!   through `"$@"`, so no shell parses them, and writes the program's exit
//!   code to the process's exit file when the program ends. The shell stays
//!   as the process's parent so that the code is recorded without a
//!   long-running mxv. The program is found on the caller's `PATH`.
//! - Arguments: `-c`, the script, `mxv-process`, the exit file and the
//!   `argv`, each a separate value.
//! - Environment: the caller's environment.
//! - Streams and deadline: standard input is empty, and standard output and
//!   standard error are appended to the process's log file. The process
//!   has no deadline, because it ends when the container that it follows
//!   stops.
//! - End: the process runs in a new process group whose id is the shell's
//!   process id, and it outlives the mxv that started it. The process
//!   leaves its log and its exit file. After the mxv that started it exits,
//!   the operating system reaps it.
//!
//! [`start_time`] gives a process's start in the form that run records
//! hold, from phase's process table, which reads `/proc/<pid>/stat` on
//! Linux and calls `proc_pidinfo` on macOS. Elsewhere it runs
//! `ps -o lstart= -p <pid>`. [`signal`] and [`group_exists`] reach a
//! process's group through phase. The calls into libproc are in phase,
//! which holds the only unsafe code.
//!
//! On macOS [`started_at`] also runs `ps -o lstart= -p <pid>` for a run
//! record that an mxv before libproc wrote, which holds what `ps` prints.
//! `ps` is found on the caller's `PATH`. `ps` runs with
//! `LC_ALL=C` and `TZ=UTC0`, so what they print does not depend on the
//! caller's time zone or locale, an empty standard input, the first
//! [`MOST_PS_BYTES`] bytes of standard output kept and standard error
//! discarded, and a deadline of [`PS_DEADLINE`], after which mxv kills and
//! reaps them.

use std::fs::File;
use std::io::{self, Read as _};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use phase::host::{ProcessError, ProcessGroup, ProcessId, RawStart};
use rustix::process::Signal;

use crate::error::Error;
use crate::store;

#[cfg(test)]
mod tests;

/// The shell that runs [`SCRIPT`].
const SHELL: &str = "/bin/sh";

/// The script that runs an `argv` and records its exit code. The shell's
/// `$1` is the exit file, and the rest are the `argv`.
pub(crate) const SCRIPT: &str = r#"exit_file=$1; shift; "$@"; printf '%s\n' "$?" > "$exit_file""#;

/// The deadline of one `ps` run. Linux reads `/proc` and runs no `ps`.
#[cfg_attr(
    target_os = "linux",
    expect(dead_code, reason = "Linux reads the process table from /proc")
)]
pub(crate) const PS_DEADLINE: Duration = Duration::from_secs(5);

/// The most bytes of `ps` output that this module keeps.
#[cfg(not(target_os = "linux"))]
const MOST_PS_BYTES: u64 = 16 * 1024 * 1024;

/// What [`launch`] needs to start one process.
#[derive(Debug)]
pub(crate) struct Launch<'a> {
    /// The process's name, for errors.
    pub(crate) name: &'a str,
    /// The program and its arguments.
    pub(crate) argv: &'a [String],
    /// The directory the process starts in.
    pub(crate) cwd: &'a Path,
    /// The log file that standard output and standard error append to.
    pub(crate) log: &'a Path,
    /// The file that the script writes the exit code to.
    pub(crate) exit_file: &'a Path,
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
    /// Whether the process stays running when this handle is dropped.
    kept: bool,
}

impl Drop for Launched {
    /// Sends SIGKILL to the process's group and reaps the shell, unless
    /// [`Launched::keep`] kept the process. So a follower stops on every
    /// path that leaves its start before its record is written, an error
    /// among them.
    fn drop(&mut self) {
        if !self.kept {
            let _ = signal(self.pid(), Signal::KILL);
            let _ = self.child.wait();
        }
    }
}

impl Launched {
    /// Returns the process id, which is also the process group's id.
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Leaves the process running once this handle is gone.
    pub(crate) fn keep(mut self) {
        self.kept = true;
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
    Ok(Launched { child, kept: false })
}

/// Returns the start time of the process `pid` in a form that does not
/// depend on the caller's time zone or locale, or `None` when no process
/// has the id. Run records store it as it is, and only compare it.
///
/// On Linux it is field 22 of `/proc/<pid>/stat`, the clock ticks from boot
/// to the process's start. On macOS it is the start that `proc_pidinfo`
/// reports, as seconds and microseconds since the epoch, such as
/// `1793809711.139785`. A process of another user, whose start mxv may not
/// read, has the start time `unreadable` on macOS and none on Linux, and
/// never matches a recorded one, because every process that mxv records is
/// the caller's. Elsewhere it is what `ps -o lstart=` prints with `TZ=UTC0`
/// and `LC_ALL=C`, such as `Wed Nov  4 10:00:00 2026`.
///
/// # Errors
///
/// Returns [`Error::ProcessTable`] when the process table cannot be read,
/// or `ps` cannot start or misses its deadline, and
/// [`Error::UnsupportedOs`] when macOS's struct differs from the one that
/// phase reads.
pub(crate) fn start_time(pid: u32) -> Result<Option<String>, Error> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        Ok(recorded_start(
            phase::host::raw_start(pid).map_err(process_error)?,
        ))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        ps_start_time(pid)
    }
}

/// Returns `start` in the form that run records hold, or `None` when no
/// process has the id.
pub(crate) fn recorded_start(start: RawStart) -> Option<String> {
    match start {
        RawStart::Ticks(ticks) => Some(ticks.to_string()),
        RawStart::Since {
            seconds,
            microseconds,
        } => Some(bsd_start(seconds, microseconds)),
        RawStart::Unreadable if cfg!(target_os = "macos") => Some(UNREADABLE.to_owned()),
        _ => None,
    }
}

/// Returns every process that has not ended, with its parent, its group,
/// its working directory and its resident memory, as phase reads them.
///
/// # Errors
///
/// Returns [`Error::ProcessTable`] when the process table cannot be read,
/// and [`Error::UnsupportedOs`] when this system's structs differ from
/// those phase reads.
pub fn process_table() -> Result<Vec<phase::host::TableEntry>, Error> {
    phase::host::process_table().map_err(process_error)
}

/// Returns mxv's error for `error`, a failure of phase's process table.
pub(crate) fn process_error(error: ProcessError) -> Error {
    match error {
        ProcessError::Unsupported {
            call,
            pid,
            written,
            expected,
            ..
        } => Error::UnsupportedOs {
            message: format!(
                "{call} for the process {pid} wrote {written} bytes, and mxv reads a struct of {expected}"
            ),
        },
        other => Error::ProcessTable {
            message: other.to_string(),
        },
    }
}

/// Reports whether the process `pid` is the one that started at
/// `recorded`, a start time that [`start_time`] gave when mxv recorded the
/// process, or `None` when no process has the id. On macOS a record that
/// an mxv before libproc wrote holds what `ps -o lstart=` prints, so a
/// recorded start that is not seconds and microseconds is compared with
/// what `ps` prints now.
///
/// # Errors
///
/// Returns [`Error::ProcessTable`] or [`Error::UnsupportedOs`] when the
/// process table cannot be read.
pub(crate) fn started_at(pid: u32, recorded: &str) -> Result<Option<bool>, Error> {
    let Some(started) = start_time(pid)? else {
        return Ok(None);
    };
    if started == recorded {
        return Ok(Some(true));
    }
    #[cfg(target_os = "macos")]
    if !is_bsd_start(recorded) && started != UNREADABLE {
        return Ok(Some(ps_start_time(pid)?.as_deref() == Some(recorded)));
    }
    Ok(Some(false))
}

/// Reports whether `started` has the form of [`bsd_start`]: seconds, a
/// dot and six digits of microseconds.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn is_bsd_start(started: &str) -> bool {
    started.split_once('.').is_some_and(|(seconds, micros)| {
        !seconds.is_empty()
            && micros.len() == 6
            && seconds
                .bytes()
                .chain(micros.bytes())
                .all(|byte| byte.is_ascii_digit())
    })
}

/// The start time of a process whose start mxv may not read.
pub(crate) const UNREADABLE: &str = "unreadable";

/// Returns the start time of a process that started `seconds` and
/// `microseconds` after the epoch.
pub(crate) fn bsd_start(seconds: u64, microseconds: u64) -> String {
    format!("{seconds}.{microseconds:06}")
}

/// Returns the start time that `ps -o lstart=` prints in UTC, or `None`
/// when no process has the id.
#[cfg(not(target_os = "linux"))]
pub(crate) fn ps_start_time(pid: u32) -> Result<Option<String>, Error> {
    let Some((success, text)) = tool("ps", &["-o", "lstart=", "-p", &pid.to_string()])? else {
        return Err(Error::ProcessTable {
            message: "cannot start ps: it is not on PATH".to_owned(),
        });
    };
    let started = text.trim();
    if success && !started.is_empty() {
        Ok(Some(started.to_owned()))
    } else {
        Ok(None)
    }
}

/// Runs `program` with `args` as the module docs say, and returns whether
/// it succeeded and what it printed, or `None` when `program` is not on
/// `PATH`.
#[cfg(not(target_os = "linux"))]
fn tool(program: &str, args: &[&str]) -> Result<Option<(bool, String)>, Error> {
    use wait_timeout::ChildExt as _;

    let table = |message: String| Error::ProcessTable { message };
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/process.rs is the module that starts an app's processes (PROC-1)"
    )]
    let mut command = Command::new(program);
    command
        .args(args)
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
    let mut child: std::process::Child = match spawned {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(table(format!("cannot start {program}: {error}"))),
    };
    let status = match child.wait_timeout(PS_DEADLINE) {
        Ok(Some(status)) => status,
        Ok(None) | Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(table(format!(
                "{program} did not finish within {} seconds",
                PS_DEADLINE.as_secs()
            )));
        }
    };
    // `ps -p` and `lsof -a -iTCP:<port>` print a few lines, which fit in a
    // pipe, so the tool never blocks on its output before the wait ends.
    let mut text = String::new();
    if let Some(stdout) = child.stdout.take() {
        stdout
            .take(MOST_PS_BYTES)
            .read_to_string(&mut text)
            .map_err(|error| table(format!("cannot read the output of {program}: {error}")))?;
    }
    Ok(Some((status.success(), text)))
}

/// Sends `signal` to the process group `pid`.
///
/// # Errors
///
/// Returns the failure of `kill(2)`, such as `ESRCH` when the group is
/// gone, and [`io::ErrorKind::InvalidInput`] for 0, 1 or an id past
/// `i32::MAX`, which mxv never signals.
pub(crate) fn signal(pid: u32, signal: Signal) -> io::Result<()> {
    phase::host::signal_group(ProcessGroup::new(pid)?, signal)
}

/// Reports whether the process group `pid` still has a member. A group
/// that mxv may not signal still exists. When the group's leader is a
/// child of this mxv that has ended, the check reaps it first, because an
/// unreaped child stays a member of its group. An id that mxv never
/// signals, such as 0 or 1, names no group of mxv's.
pub(crate) fn group_exists(pid: u32) -> bool {
    let (Ok(process), Ok(group)) = (ProcessId::new(pid), ProcessGroup::new(pid)) else {
        return false;
    };
    phase::host::reap(process);
    phase::host::group_exists(group)
}

/// Reports whether the process group `group` has a member that has not
/// ended and that started more than `slack` before `exit_file` was last
/// written. It is false when the file has no time, the members cannot be
/// listed, or a member's start cannot be turned into a wall-clock time.
pub(crate) fn member_started_before(group: u32, exit_file: &Path, slack: Duration) -> bool {
    let Some(bound) = std::fs::metadata(exit_file)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|written| written.checked_sub(slack))
    else {
        return false;
    };
    let Ok(group) = ProcessGroup::new(group) else {
        return false;
    };
    phase::host::live_members(group).is_some_and(|members| {
        members.iter().any(|member| {
            phase::host::started_at(member.start).is_some_and(|started| started < bound)
        })
    })
}

/// Returns the exit code that the script wrote to `exit_file`, or `None`
/// when the file does not exist or the script has not finished writing it.
/// The script's redirection creates the file empty before `printf` writes
/// the code and its line feed, so only a file that ends in a line feed is
/// written. A written file that holds no number gives `Some(None)`.
pub(crate) fn exit_code(exit_file: &Path) -> Option<Option<i32>> {
    let mut text = String::new();
    File::open(exit_file)
        .ok()?
        .take(64)
        .read_to_string(&mut text)
        .ok()?;
    text.ends_with('\n').then(|| text.trim().parse().ok())
}
