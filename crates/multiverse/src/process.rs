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
//! [`start_time`] reads `/proc/<pid>/stat` on Linux and calls
//! `proc_pidinfo` on macOS. Elsewhere it runs `ps -o lstart= -p <pid>`.
//!
//! [`group_listens`] tells whether a member of a process group listens on
//! a TCP port. On Linux it reads the listening sockets of
//! `/proc/net/tcp` and `/proc/net/tcp6`, and the file descriptors of the
//! group's members under `/proc/<pid>/fd`. On macOS it lists the group's
//! members with `proc_listpids`, their descriptors with `proc_pidinfo`, and
//! reads each socket with `proc_pidfdinfo`. The calls into libproc are in
//! the `darwin` module, which holds mxv's only unsafe code.
//!
//! `ps` is found on the caller's `PATH`. `ps` runs with
//! `LC_ALL=C` and `TZ=UTC0`, so what they print does not depend on the
//! caller's time zone or locale, an empty standard input, the first
//! [`MOST_PS_BYTES`] bytes of standard output kept and standard error
//! discarded, and a deadline of [`PS_DEADLINE`], after which mxv kills and
//! reaps them.

use std::fs::File;
use std::io::{self, Read as _};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use rustix::process::{Pid, Signal};

use crate::error::Error;
use crate::store;

#[cfg(target_os = "macos")]
mod darwin;
#[cfg(test)]
mod tests;

/// The shell that runs [`SCRIPT`].
const SHELL: &str = "/bin/sh";

/// The script that runs a manifest's `argv` and records its exit code. The
/// shell's `$1` is the exit file, and the rest are the `argv`.
pub(crate) const SCRIPT: &str = r#"exit_file=$1; shift; "$@"; printf '%s\n' "$?" > "$exit_file""#;

/// The deadline of one `ps` run. Linux reads `/proc` and macOS calls
/// libproc, so neither runs `ps`.
#[cfg_attr(
    any(target_os = "linux", target_os = "macos"),
    expect(dead_code, reason = "Linux reads /proc and macOS calls libproc")
)]
pub(crate) const PS_DEADLINE: Duration = Duration::from_secs(5);

/// The most bytes of `ps` output that this module keeps, and of
/// a `/proc` file that it reads. A busy host's socket table may need
/// several megabytes.
#[cfg_attr(
    target_os = "macos",
    expect(
        dead_code,
        reason = "macOS calls libproc and reads neither ps nor /proc"
    )
)]
const MOST_PS_BYTES: u64 = 16 * 1024 * 1024;

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
/// to the process's start. On macOS it is the start that `proc_pidinfo`
/// reports, as seconds and microseconds since the epoch, such as
/// `1793809711.139785`. A process of another user, whose start mxv may not
/// read, has the start time `unreadable`, which never matches a recorded
/// one, because every process that mxv records is the caller's. Elsewhere
/// it is what `ps -o lstart=` prints with `TZ=UTC0` and `LC_ALL=C`, such as
/// `Wed Nov  4 10:00:00 2026`.
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
    #[cfg(target_os = "macos")]
    {
        macos_start_time(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        ps_start_time(pid)
    }
}

/// Returns the start that `proc_pidinfo(PROC_PIDTBSDINFO)` reports for
/// `pid`, or `None` when no process has the id.
#[cfg(target_os = "macos")]
fn macos_start_time(pid: u32) -> Result<Option<String>, Error> {
    let Ok(id) = i32::try_from(pid) else {
        return Ok(None);
    };
    match darwin::bsd_info(id) {
        Ok(info) => Ok(bsd_info_start(&info)),
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(None),
        Err(error) if error.raw_os_error() == Some(libc::EPERM) => Ok(Some(UNREADABLE.to_owned())),
        Err(error) => Err(Error::ProcessTable {
            message: format!("proc_pidinfo cannot read the process {pid}: {error}"),
        }),
    }
}

/// Returns the start time in `info`, the bytes of `struct proc_bsdinfo`,
/// whose `pbi_start_tvsec` and `pbi_start_tvusec` are 64-bit fields at
/// offsets 120 and 128.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn bsd_info_start(info: &[u8]) -> Option<String> {
    Some(bsd_start(read_u64(info, 120)?, read_u64(info, 128)?))
}

/// Returns the native-endian `u64` at `offset` of `bytes`, or `None` when
/// `bytes` ends first.
#[cfg(any(target_os = "macos", test))]
fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let field = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_ne_bytes(field.try_into().ok()?))
}

/// Returns the native-endian `i32` at `offset` of `bytes`, or `None` when
/// `bytes` ends first.
#[cfg(any(target_os = "macos", test))]
fn read_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    let field = bytes.get(offset..offset.checked_add(4)?)?;
    Some(i32::from_ne_bytes(field.try_into().ok()?))
}

/// The start time of a process whose start mxv may not read.
#[cfg(any(target_os = "macos", test))]
pub(crate) const UNREADABLE: &str = "unreadable";

/// Returns the start time of a process that started `seconds` and
/// `microseconds` after the epoch.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn bsd_start(seconds: u64, microseconds: u64) -> String {
    format!("{seconds}.{microseconds:06}")
}

/// Returns field 22 of `/proc/<pid>/stat`, or `None` when no process has
/// the id. The fields follow the last `)`, which ends the command name, and
/// the first of them is field 3.
#[cfg(target_os = "linux")]
fn linux_start_time(pid: u32) -> Result<Option<String>, Error> {
    Ok(read_proc(&format!("/proc/{pid}/stat"))?.and_then(|text| stat_start_time(&text)))
}

/// Returns the text of the `/proc` file at `path`, or `None` when it does
/// not exist, such as for a process that has ended.
#[cfg(target_os = "linux")]
fn read_proc(path: &str) -> Result<Option<String>, Error> {
    let mut text = String::new();
    match File::open(path) {
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
    Ok(Some(text))
}

/// Reports whether a member of the process group `group` listens on the
/// TCP port `port`, or `None` when this system gives mxv no way to tell.
///
/// # Errors
///
/// Returns [`Error::ProcessTable`] when the process table or the socket
/// table cannot be read, or `lsof` misses its deadline.
pub(crate) fn group_listens(group: u32, port: u16) -> Result<Option<bool>, Error> {
    #[cfg(target_os = "linux")]
    {
        linux_group_listens(group, port).map(Some)
    }
    #[cfg(target_os = "macos")]
    {
        macos_group_listens(group, port).map(Some)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (group, port);
        Ok(None)
    }
}

/// Reports whether a member of the process group `group` holds a socket
/// that listens on the TCP port `port`.
#[cfg(target_os = "linux")]
fn linux_group_listens(group: u32, port: u16) -> Result<bool, Error> {
    let mut inodes = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Some(text) = read_proc(table)? {
            inodes.extend(listening_inodes(&text, port));
        }
    }
    if inodes.is_empty() {
        return Ok(false);
    }
    let entries = std::fs::read_dir("/proc").map_err(|error| Error::ProcessTable {
        message: format!("cannot read /proc: {error}"),
    })?;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let in_group = read_proc(&format!("/proc/{pid}/stat"))?.and_then(|text| stat_group(&text))
            == Some(group);
        if !in_group {
            continue;
        }
        // A member that ends meanwhile, or whose descriptors mxv may not
        // read, holds none of the group's sockets that mxv can see.
        let Ok(descriptors) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for descriptor in descriptors.flatten() {
            if let Ok(target) = std::fs::read_link(descriptor.path())
                && let Some(inode) = target
                    .to_str()
                    .and_then(|target| target.strip_prefix("socket:["))
                    .and_then(|rest| rest.strip_suffix(']'))
                && inodes.iter().any(|listening| listening == inode)
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Returns the inodes of the sockets in `text`, the contents of
/// `/proc/net/tcp` or `/proc/net/tcp6`, that listen on `port`. Each line
/// after the header holds the local address as `<hex address>:<hex port>`
/// in its second field, the state in its fourth, where `0A` is LISTEN, and
/// the inode in its tenth.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn listening_inodes(text: &str, port: u16) -> Vec<String> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let local_port = fields.get(1)?.rsplit_once(':')?.1;
            let listens = u16::from_str_radix(local_port, 16).ok() == Some(port)
                && fields.get(3) == Some(&"0A");
            listens.then(|| fields.get(9).map(|inode| (*inode).to_owned()))?
        })
        .collect()
}

/// Returns field 5 of `text`, the contents of `/proc/<pid>/stat`, which is
/// the process's group, counted from the last `)` as [`stat_start_time`]
/// counts.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn stat_group(text: &str) -> Option<u32> {
    let fields = text.rfind(')').and_then(|end| text.get(end + 1..))?;
    fields.split_whitespace().nth(5 - 3)?.parse().ok()
}

/// Reports whether a member of the process group `group` holds a socket
/// that listens on the TCP port `port`.
#[cfg(target_os = "macos")]
fn macos_group_listens(group: u32, port: u16) -> Result<bool, Error> {
    let Ok(group) = i32::try_from(group) else {
        return Ok(false);
    };
    let members = darwin::group_members(group).map_err(|error| Error::ProcessTable {
        message: format!("proc_listpids cannot list the process group {group}: {error}"),
    })?;
    for member in members {
        // A member that ends meanwhile, or whose descriptors mxv may not
        // read, holds none of the group's sockets that mxv can see.
        let Ok(descriptors) = darwin::descriptors(member) else {
            continue;
        };
        for fd in socket_descriptors(&descriptors) {
            if let Ok(info) = darwin::socket_info(member, fd)
                && socket_listens_on(&info, port)
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Returns the descriptors in `list`, the bytes of a `struct proc_fdinfo`
/// array, whose type is `PROX_FDTYPE_SOCKET`. Each entry holds the
/// descriptor at offset 0 and its type at offset 4.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn socket_descriptors(list: &[u8]) -> Vec<i32> {
    /// `PROX_FDTYPE_SOCKET` of `<sys/proc_info.h>`.
    const PROX_FDTYPE_SOCKET: i32 = 2;
    list.chunks_exact(8)
        .filter(|entry| read_i32(entry, 4) == Some(PROX_FDTYPE_SOCKET))
        .filter_map(|entry| read_i32(entry, 0))
        .collect()
}

/// Reports whether `info`, the bytes of `struct socket_fdinfo`, is a TCP
/// socket in the LISTEN state on the local port `port`. Its `soi_kind` is
/// at offset 256, the local port of its `in_sockinfo` at offset 268 in
/// network byte order, and its `tcpsi_state` at offset 344.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn socket_listens_on(info: &[u8], port: u16) -> bool {
    /// `SOCKINFO_TCP` of `<sys/proc_info.h>`.
    const SOCKINFO_TCP: i32 = 2;
    /// `TSI_S_LISTEN` of `<sys/proc_info.h>`.
    const TSI_S_LISTEN: i32 = 1;
    let local_port = info
        .get(268..270)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u16::from_be_bytes);
    read_i32(info, 256) == Some(SOCKINFO_TCP)
        && read_i32(info, 344) == Some(TSI_S_LISTEN)
        && local_port == Some(port)
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
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn ps_start_time(pid: u32) -> Result<Option<String>, Error> {
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
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
