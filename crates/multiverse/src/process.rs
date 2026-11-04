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
//! [`listen_holder`] tells which process holds the socket that listens on a
//! TCP port: a member of a process group, a descendant of the group's
//! leader in another group, or another process. On Linux it reads the
//! listening sockets of `/proc/net/tcp` and `/proc/net/tcp6`, the file
//! descriptors under `/proc/<pid>/fd`, and each holder's parent and group
//! from `/proc/<pid>/stat`. On macOS it lists the processes with
//! `proc_listpids`, their descriptors with `proc_pidinfo`, each socket with
//! `proc_pidfdinfo`, and each holder's parent and group with
//! `proc_pidinfo`. The calls into libproc are in the `darwin` module, which
//! holds mxv's only unsafe code.
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

/// The deadline of one `ps` run. Linux reads `/proc` and runs no `ps`.
#[cfg_attr(
    target_os = "linux",
    expect(dead_code, reason = "Linux reads the process table from /proc")
)]
pub(crate) const PS_DEADLINE: Duration = Duration::from_secs(5);

/// The most bytes of `ps` output that this module keeps, and of
/// a `/proc` file that it reads. A busy host's socket table may need
/// several megabytes.
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
    /// Whether the process stays running when this handle is dropped.
    kept: bool,
}

impl Drop for Launched {
    /// Sends SIGKILL to the process's group and reaps the shell, unless
    /// [`Launched::keep`] kept the process. So a process that `up` started
    /// stops on every path that leaves `up` before the process is ready,
    /// an error among them.
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
    pub(crate) fn kill(self) {
        drop(self);
    }

    /// Leaves the process running once this handle is gone, after it is
    /// ready, or after its shell was reaped and its group handled.
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
        Err(failure) if failure.errno() == Some(libc::ESRCH) => Ok(None),
        Err(failure) if failure.errno() == Some(libc::EPERM) => Ok(Some(UNREADABLE.to_owned())),
        Err(failure) => Err(darwin_error(&failure, pid)),
    }
}

/// Returns the error of a libproc call about `pid` that gave no struct.
#[cfg(target_os = "macos")]
fn darwin_error(failure: &darwin::Failure, pid: u32) -> Error {
    match failure {
        darwin::Failure::Size {
            call,
            written,
            expected,
        } => Error::UnsupportedOs {
            message: format!(
                "{call} for the process {pid} wrote {written} bytes, and mxv reads a struct of {expected}"
            ),
        },
        darwin::Failure::Os(error) => Error::ProcessTable {
            message: format!("libproc cannot read the process {pid}: {error}"),
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

/// Returns the text of the `/proc` file at `path`, or `None` when it has
/// nothing that mxv may read: it does not exist or gives ESRCH, as for a
/// process that ends meanwhile, or it gives EACCES, as for some files of
/// another user's process.
#[cfg(target_os = "linux")]
fn read_proc(path: &str) -> Result<Option<String>, Error> {
    let gone = |error: &io::Error| {
        error.kind() == io::ErrorKind::NotFound
            || error.kind() == io::ErrorKind::PermissionDenied
            || error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error())
    };
    let mut text = String::new();
    let read = File::open(path).and_then(|file| file.take(MOST_PS_BYTES).read_to_string(&mut text));
    match read {
        Ok(_) => Ok(Some(text)),
        Err(error) if gone(&error) => Ok(None),
        Err(error) => Err(Error::ProcessTable {
            message: format!("cannot read {path}: {error}"),
        }),
    }
}

/// The process that holds the socket that listens on a port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Holder {
    /// A member of the process group.
    Group,
    /// A process in another group that descends from the group's leader,
    /// such as a daemon that started a session of its own.
    Descendant(u32),
    /// Another process, or `None` when no process whose descriptors mxv may
    /// read holds the socket.
    Other(Option<u32>),
}

/// Returns which process holds the socket that listens on the TCP port
/// `port`, as seen from the process group `group`, whose leader's id is
/// `group`. It returns `None` when this system gives mxv no way to tell.
///
/// # Errors
///
/// Returns [`Error::ProcessTable`] when the process table or the socket
/// table cannot be read, and [`Error::UnsupportedOs`] when the operating
/// system's structs differ from the ones mxv reads.
pub(crate) fn listen_holder(group: u32, port: u16) -> Result<Option<Holder>, Error> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let mut others = Vec::new();
        for pid in listening_processes(port)? {
            match table_entry(pid)? {
                Some(entry) if entry.group == group => return Ok(Some(Holder::Group)),
                Some(_) => others.push(pid),
                None => {}
            }
        }
        for pid in &others {
            if descends_from(*pid, group)? {
                return Ok(Some(Holder::Descendant(*pid)));
            }
        }
        Ok(Some(Holder::Other(others.first().copied())))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (group, port);
        Ok(None)
    }
}

/// The parent and the process group of one process.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone, Copy, Debug)]
struct Entry {
    /// The parent's process id.
    parent: u32,
    /// The process group's id.
    group: u32,
}

/// The most parents that [`descends_from`] follows, which bounds a walk
/// through a process table that changes while mxv reads it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const MOST_ANCESTORS: usize = 256;

/// Reports whether `leader` is an ancestor of the process `pid`. A process
/// whose parent ended has the init process as its parent, so a descendant
/// whose parent between it and `leader` ended no longer counts.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn descends_from(pid: u32, leader: u32) -> Result<bool, Error> {
    let mut current = pid;
    for _ in 0..MOST_ANCESTORS {
        let Some(entry) = table_entry(current)? else {
            return Ok(false);
        };
        if entry.parent == leader {
            return Ok(true);
        }
        if entry.parent <= 1 {
            return Ok(false);
        }
        current = entry.parent;
    }
    Ok(false)
}

/// Returns the parent and the group of `pid`, from fields 4 and 5 of
/// `/proc/<pid>/stat`, or `None` when no process has the id.
#[cfg(target_os = "linux")]
fn table_entry(pid: u32) -> Result<Option<Entry>, Error> {
    Ok(read_proc(&format!("/proc/{pid}/stat"))?.and_then(|text| {
        Some(Entry {
            parent: stat_field(&text, 4)?,
            group: stat_field(&text, 5)?,
        })
    }))
}

/// Returns the parent and the group of `pid`, from `pbi_ppid` at offset 16
/// and `pbi_pgid` at offset 100 of `struct proc_bsdinfo`, or `None` when no
/// process has the id or mxv may not read it.
#[cfg(target_os = "macos")]
fn table_entry(pid: u32) -> Result<Option<Entry>, Error> {
    let Ok(id) = i32::try_from(pid) else {
        return Ok(None);
    };
    match darwin::bsd_info(id) {
        Ok(info) => Ok(bsd_info_entry(&info)),
        Err(failure) if matches!(failure.errno(), Some(libc::ESRCH | libc::EPERM)) => Ok(None),
        Err(failure) => Err(darwin_error(&failure, pid)),
    }
}

/// Returns the parent and the group in `info`, the bytes of
/// `struct proc_bsdinfo`.
#[cfg(target_os = "macos")]
fn bsd_info_entry(info: &[u8]) -> Option<Entry> {
    Some(Entry {
        parent: read_i32(info, 16)?.cast_unsigned(),
        group: read_i32(info, 100)?.cast_unsigned(),
    })
}

/// Returns the processes that hold a socket that listens on the TCP port
/// `port`, among those whose descriptors mxv may read.
#[cfg(target_os = "linux")]
fn listening_processes(port: u16) -> Result<Vec<u32>, Error> {
    let mut inodes = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Some(text) = read_proc(table)? {
            inodes.extend(listening_inodes(&text, port));
        }
    }
    if inodes.is_empty() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir("/proc").map_err(|error| Error::ProcessTable {
        message: format!("cannot read /proc: {error}"),
    })?;
    let mut holders = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        // A process that ends meanwhile, or whose descriptors mxv may not
        // read, holds none of the sockets that mxv can see.
        let Ok(descriptors) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        let holds = descriptors.flatten().any(|descriptor| {
            std::fs::read_link(descriptor.path()).is_ok_and(|target| {
                target
                    .to_str()
                    .and_then(|target| target.strip_prefix("socket:["))
                    .and_then(|rest| rest.strip_suffix(']'))
                    .is_some_and(|inode| inodes.iter().any(|listening| listening == inode))
            })
        });
        if holds {
            holders.push(pid);
        }
    }
    Ok(holders)
}

/// Returns the processes that hold a socket that listens on the TCP port
/// `port`, among those whose descriptors mxv may read.
#[cfg(target_os = "macos")]
fn listening_processes(port: u16) -> Result<Vec<u32>, Error> {
    // A process that ends meanwhile gives ESRCH, a descriptor that it
    // closes meanwhile gives EBADF, and a process of another user gives
    // EPERM: each holds none of the sockets that mxv can see.
    let skipped = |failure: &darwin::Failure| {
        matches!(
            failure.errno(),
            Some(libc::ESRCH | libc::EBADF | libc::EPERM | libc::EACCES)
        )
    };
    let processes = darwin::all_processes().map_err(|failure| darwin_error(&failure, 0))?;
    let mut holders = Vec::new();
    for pid in processes {
        let descriptors = match darwin::descriptors(pid) {
            Ok(descriptors) => descriptors,
            Err(failure) if skipped(&failure) => continue,
            Err(failure) => return Err(darwin_error(&failure, pid.cast_unsigned())),
        };
        for fd in socket_descriptors(&descriptors) {
            match darwin::socket_info(pid, fd) {
                Ok(info) if socket_listens_on(&info, port) => {
                    holders.push(pid.cast_unsigned());
                    break;
                }
                Ok(_) => {}
                Err(failure) if skipped(&failure) => {}
                Err(failure) => return Err(darwin_error(&failure, pid.cast_unsigned())),
            }
        }
    }
    Ok(holders)
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

/// Returns the numeric field `field` of `text`, the contents of
/// `/proc/<pid>/stat`, counted from the last `)` as [`stat_start_time`]
/// counts. Field 4 is the parent and field 5 the process group.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn stat_field(text: &str, field: usize) -> Option<u32> {
    let fields = text.rfind(')').and_then(|end| text.get(end + 1..))?;
    fields
        .split_whitespace()
        .nth(field.checked_sub(3)?)?
        .parse()
        .ok()
}

/// Returns the descriptors in `list`, the bytes of a `struct proc_fdinfo`
/// array, whose type is `PROX_FDTYPE_SOCKET`. Each entry holds the
/// descriptor at offset 0 and its type at offset 4.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn socket_descriptors(list: &[u8]) -> Vec<i32> {
    /// `PROX_FDTYPE_SOCKET` of `<sys/proc_info.h>`.
    const PROX_FDTYPE_SOCKET: i32 = 2;
    let (entries, _) = list.as_chunks::<8>();
    entries
        .iter()
        .filter(|entry| read_i32(*entry, 4) == Some(PROX_FDTYPE_SOCKET))
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

/// Sends `signal` to the process `pid` alone.
///
/// # Errors
///
/// Returns the error of `kill`, such as ESRCH when no process has the id.
pub(crate) fn signal_process(pid: u32, signal: Signal) -> io::Result<()> {
    let process = pid_of(pid).ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    rustix::process::kill_process(process, signal).map_err(io::Error::from)
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
