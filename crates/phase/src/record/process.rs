//! Finds and stops a host process that the run record names. A recorded
//! process is the process with its pid whose start time equals the
//! recorded one, so a pid that the system gave to another process is never
//! mistaken for it.
//!
//! [`raw_start`] reads `/proc/<pid>/stat` on Linux and calls
//! `proc_pidinfo` on macOS. It is the one place in phase and mxv that reads
//! a process's start: [`start_time`] gives it to `sweep` in microseconds,
//! and mxv keeps it in the form that its run records hold. The calls into
//! libproc are in the `darwin` module, which holds the only unsafe code of
//! phase and mxv. This module also signals process groups and processes
//! for `sweep` and for mxv.

use std::fmt;
use std::io;

use rustix::process::{Pid, Signal};

use crate::error::PhaseError;
use crate::record::run::HostProcess;

#[cfg(target_os = "macos")]
pub(crate) mod darwin;

/// The most bytes of a `/proc` file that this module and the host
/// runner's listener lookup read. A busy host's socket table may need
/// several megabytes.
#[cfg(target_os = "linux")]
pub(crate) const MOST_PROC_BYTES: u64 = 16 * 1024 * 1024;

/// A failure to read the process table.
#[derive(Debug)]
#[non_exhaustive]
pub enum ProcessError {
    /// The process table, or a file of it, cannot be read.
    #[non_exhaustive]
    Table {
        /// What failed.
        message: String,
    },
    /// A libproc call wrote a struct of another size than the one that
    /// phase reads, so this macOS's structs differ from those phase knows.
    #[non_exhaustive]
    Unsupported {
        /// The call, such as `proc_pidinfo(PROC_PIDTBSDINFO)`.
        call: &'static str,
        /// The process that the call was about, or 0 for a call about every
        /// process.
        pid: u32,
        /// The bytes that the call wrote.
        written: usize,
        /// The bytes of the struct that phase reads.
        expected: usize,
    },
}

impl ProcessError {
    /// Returns the failure of a table that cannot be read, which `message`
    /// describes.
    pub(crate) const fn table(message: String) -> Self {
        Self::Table { message }
    }
}

impl fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Table { message } => formatter.write_str(message),
            Self::Unsupported {
                call,
                pid,
                written,
                expected,
            } => write!(
                formatter,
                "{call} for the process {pid} wrote {written} bytes, and phase reads a struct of {expected}"
            ),
        }
    }
}

impl std::error::Error for ProcessError {}

impl From<ProcessError> for PhaseError {
    fn from(error: ProcessError) -> Self {
        Self::ProcessTable {
            message: error.to_string(),
        }
    }
}

/// Returns the failure of a libproc call about `pid` that gave no struct.
#[cfg(target_os = "macos")]
pub(crate) fn darwin_error(failure: &darwin::Failure, pid: u32) -> ProcessError {
    match failure {
        darwin::Failure::Size {
            call,
            written,
            expected,
        } => ProcessError::Unsupported {
            call,
            pid,
            written: *written,
            expected: *expected,
        },
        darwin::Failure::Os(error) => {
            ProcessError::table(format!("libproc cannot read the process {pid}: {error}"))
        }
    }
}

/// A process's start as the process table gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RawStart {
    /// No process has the pid.
    Absent,
    /// A process of another user has the pid, and its start may not be
    /// read. Every process that phase or mxv records is the caller's, so it
    /// is never a recorded process.
    Unreadable,
    /// The process started this many clock ticks after the machine booted,
    /// as field 22 of `/proc/<pid>/stat` gives on Linux.
    Ticks(u64),
    /// The process started this many seconds and microseconds after the
    /// Unix epoch, as `proc_pidinfo` gives on macOS.
    Since {
        /// The whole seconds.
        seconds: u64,
        /// The microseconds after them.
        microseconds: u64,
    },
}

/// Returns the start of the process `pid` as the process table gives it.
///
/// # Errors
///
/// Returns [`ProcessError::Table`] when the process table cannot be read,
/// or this system is neither Linux nor macOS, and
/// [`ProcessError::Unsupported`] when macOS's struct differs from the one
/// that phase reads.
pub fn raw_start(pid: u32) -> Result<RawStart, ProcessError> {
    #[cfg(target_os = "macos")]
    {
        macos_raw_start(pid)
    }
    #[cfg(target_os = "linux")]
    {
        linux_raw_start(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        Err(ProcessError::table(
            "phase reads process start times only on Linux and macOS".to_owned(),
        ))
    }
}

/// What the process table holds at a pid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Start {
    /// No process has the pid.
    Absent,
    /// A process has the pid and started at this time, in the unit that
    /// [`start_time`] gives.
    At(u64),
    /// A process of another user has the pid, and phase may not read its
    /// start. Every process that phase records is the caller's, so it is
    /// never a recorded process.
    Unreadable,
}

/// Returns the start time of the process `pid`, in microseconds: since the
/// Unix epoch on macOS, and since the machine booted on Linux, where the
/// kernel counts the start in clock ticks. phase only compares it with a
/// start time that this function gave before.
///
/// # Errors
///
/// Returns [`PhaseError::ProcessTable`] when the process table cannot be
/// read.
pub(crate) fn start_time(pid: u32) -> Result<Start, PhaseError> {
    let overflow = || ProcessError::table(format!("the start time of the process {pid} overflows"));
    Ok(match raw_start(pid)? {
        RawStart::Absent => Start::Absent,
        RawStart::Unreadable => Start::Unreadable,
        RawStart::Ticks(ticks) => {
            Start::At(ticks_to_micros(ticks, clock_ticks_per_second()).ok_or_else(overflow)?)
        }
        RawStart::Since {
            seconds,
            microseconds,
        } => Start::At(
            seconds
                .checked_mul(1_000_000)
                .and_then(|micros| micros.checked_add(microseconds))
                .ok_or_else(overflow)?,
        ),
    })
}

/// Returns the clock ticks in a second, in which Linux counts a process's
/// start.
#[cfg(target_os = "linux")]
fn clock_ticks_per_second() -> u64 {
    rustix::param::clock_ticks_per_second()
}

/// Returns 0: only Linux counts a process's start in clock ticks.
#[cfg(not(target_os = "linux"))]
const fn clock_ticks_per_second() -> u64 {
    0
}

/// Returns the start that `proc_pidinfo(PROC_PIDTBSDINFO)` reports for
/// `pid`.
#[cfg(target_os = "macos")]
fn macos_raw_start(pid: u32) -> Result<RawStart, ProcessError> {
    let Ok(id) = i32::try_from(pid) else {
        return Ok(RawStart::Absent);
    };
    match darwin::bsd_info(id) {
        Ok(info) => bsd_info_since(&info).ok_or_else(|| {
            ProcessError::table(format!("libproc gave no start time for the process {pid}"))
        }),
        Err(failure) if failure.errno() == Some(libc::ESRCH) => Ok(RawStart::Absent),
        Err(failure) if failure.errno() == Some(libc::EPERM) => Ok(RawStart::Unreadable),
        Err(failure) => Err(darwin_error(&failure, pid)),
    }
}

/// Returns the start in `info`, the bytes of `struct proc_bsdinfo`, whose
/// `pbi_start_tvsec` and `pbi_start_tvusec` are 64-bit fields at offsets
/// 120 and 128.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn bsd_info_since(info: &[u8]) -> Option<RawStart> {
    Some(RawStart::Since {
        seconds: read_u64(info, 120)?,
        microseconds: read_u64(info, 128)?,
    })
}

/// Returns the start time in `info`, the bytes of `struct proc_bsdinfo`, as
/// microseconds since the epoch.
#[cfg(test)]
pub(crate) fn bsd_info_start(info: &[u8]) -> Option<u64> {
    match bsd_info_since(info)? {
        RawStart::Since {
            seconds,
            microseconds,
        } => seconds.checked_mul(1_000_000)?.checked_add(microseconds),
        _ => None,
    }
}

/// Returns the native-endian `u64` at `offset` of `bytes`, or `None` when
/// `bytes` ends first.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let field = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_ne_bytes(field.try_into().ok()?))
}

/// Returns the native-endian `i32` at `offset` of `bytes`, or `None` when
/// `bytes` ends first.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn read_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    let field = bytes.get(offset..offset.checked_add(4)?)?;
    Some(i32::from_ne_bytes(field.try_into().ok()?))
}

/// What a read of a `/proc` file gave.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) enum ProcRead {
    /// The file's text.
    Text(String),
    /// The file does not exist or gives ESRCH, as for a process that ends
    /// meanwhile.
    Absent,
    /// The file gives EACCES, as some files of another user's process do.
    Denied,
}

/// Reads up to [`MOST_PROC_BYTES`] of the `/proc` file at `path`.
///
/// # Errors
///
/// Returns [`ProcessError::Table`] for any other failure.
#[cfg(target_os = "linux")]
pub(crate) fn read_proc(path: &str) -> Result<ProcRead, ProcessError> {
    use std::io::Read as _;

    let mut text = String::new();
    let read = std::fs::File::open(path)
        .and_then(|file| file.take(MOST_PROC_BYTES).read_to_string(&mut text));
    match read {
        Ok(_) => Ok(ProcRead::Text(text)),
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) =>
        {
            Ok(ProcRead::Absent)
        }
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(ProcRead::Denied),
        Err(error) => Err(ProcessError::table(format!("cannot read {path}: {error}"))),
    }
}

/// Returns the start in field 22 of `/proc/<pid>/stat`, in clock ticks.
#[cfg(target_os = "linux")]
fn linux_raw_start(pid: u32) -> Result<RawStart, ProcessError> {
    let path = format!("/proc/{pid}/stat");
    match read_proc(&path)? {
        ProcRead::Text(text) => stat_start_ticks(&text)
            .map(RawStart::Ticks)
            .ok_or_else(|| ProcessError::table(format!("{path} holds no start time"))),
        ProcRead::Absent => Ok(RawStart::Absent),
        ProcRead::Denied => Ok(RawStart::Unreadable),
    }
}

/// Returns field 22 of `text`, the contents of `/proc/<pid>/stat`, which is
/// the process's start in clock ticks after boot. The command name in
/// field 2 may hold spaces and parentheses, so the fields are counted from
/// the last `)`, after which field 3 comes.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn stat_start_ticks(text: &str) -> Option<u64> {
    stat_field(text, 22)
}

/// Returns the numeric field `field` of `text`, the contents of
/// `/proc/<pid>/stat`, counted from the last `)` as [`stat_start_ticks`]
/// counts. Field 4 is the parent and field 5 the process group.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn stat_field<T: std::str::FromStr>(text: &str, field: usize) -> Option<T> {
    let fields = text.rfind(')').and_then(|end| text.get(end + 1..))?;
    fields
        .split_whitespace()
        .nth(field.checked_sub(3)?)?
        .parse()
        .ok()
}

/// Returns `ticks` clock ticks, of which a second holds `per_second`, as
/// microseconds, or `None` when the result overflows or `per_second` is 0.
pub(crate) fn ticks_to_micros(ticks: u64, per_second: u64) -> Option<u64> {
    ticks.checked_mul(1_000_000)?.checked_div(per_second)
}

/// Sends `signal` to the process group `group`.
///
/// # Errors
///
/// Returns the failure of `kill(2)`, such as ESRCH when the group is gone,
/// and [`io::ErrorKind::InvalidInput`] for 0 or an id past `i32::MAX`.
pub fn signal_group(group: u32, signal: Signal) -> io::Result<()> {
    let group = pid_of(group).ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    rustix::process::kill_process_group(group, signal).map_err(io::Error::from)
}

/// Sends `signal` to the process `pid` alone.
///
/// # Errors
///
/// Returns the failure of `kill(2)`, such as ESRCH when no process has the
/// id, and [`io::ErrorKind::InvalidInput`] for 0 or an id past `i32::MAX`.
pub fn signal_pid(pid: u32, signal: Signal) -> io::Result<()> {
    let process = pid_of(pid).ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    rustix::process::kill_process(process, signal).map_err(io::Error::from)
}

/// Reports whether the process group `group` still has a member. A group
/// that the caller may not signal still exists.
#[must_use]
pub fn group_exists(group: u32) -> bool {
    let Some(group) = pid_of(group) else {
        return false;
    };
    match rustix::process::test_kill_process_group(group) {
        Ok(()) => true,
        Err(error) => error != rustix::io::Errno::SRCH,
    }
}

/// Reaps `pid` when it is an ended child of this process, because an
/// unreaped child keeps its pid and stays a member of its group. A pid
/// that is not this process's child is left alone.
pub fn reap(pid: u32) {
    if let Some(process) = pid_of(pid) {
        let _ = rustix::process::waitpid(Some(process), rustix::process::WaitOptions::NOHANG);
    }
}

/// Returns `pid` as rustix's process id, or `None` for 0 or an id past
/// `i32::MAX`, which name no single process.
fn pid_of(pid: u32) -> Option<Pid> {
    i32::try_from(pid).ok().and_then(Pid::from_raw)
}

/// The calls that `sweep` makes on the process table, behind a trait so
/// that tests can supply processes that a real system cannot, such as one
/// that survives `KILL`. [`System`] is the real one.
pub(crate) trait Processes {
    /// Returns what the process table holds at `pid`, as [`start_time`]
    /// says.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::ProcessTable`] when the table cannot be read.
    fn start_time(&self, pid: u32) -> Result<Start, PhaseError>;

    /// Returns the id of the current boot on Linux, or `None` on a system
    /// whose start times do not depend on the boot.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::ProcessTable`] when the id cannot be read.
    fn boot_id(&self) -> Result<Option<String>, PhaseError>;

    /// Reports whether the process group `group` still has a member. A
    /// group that phase may not signal still exists.
    fn group_exists(&self, group: u32) -> bool;

    /// Reaps `pid` when it is an ended child of this process, because an
    /// unreaped child keeps its pid and stays a member of its group. A pid
    /// that is not this process's child is left alone.
    fn reap(&self, pid: u32);

    /// Sends `signal` to the recorded process `host`: to its group when it
    /// leads its group, and to the process alone otherwise, so phase never
    /// signals a group that it did not create. A process that has already
    /// gone is no failure.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::ProcessTable`] when the signal cannot be sent
    /// for another reason, such as EPERM.
    fn signal(&self, host: &HostProcess, signal: Signal) -> Result<(), PhaseError>;
}

/// The process table of this machine.
#[derive(Clone, Copy, Debug)]
pub(crate) struct System;

impl Processes for System {
    fn start_time(&self, pid: u32) -> Result<Start, PhaseError> {
        start_time(pid)
    }

    fn boot_id(&self) -> Result<Option<String>, PhaseError> {
        boot_id()
    }

    fn group_exists(&self, group: u32) -> bool {
        group_exists(group)
    }

    fn reap(&self, pid: u32) {
        reap(pid);
    }

    fn signal(&self, host: &HostProcess, signal: Signal) -> Result<(), PhaseError> {
        let Some(process) = pid_of(host.pid) else {
            return Ok(());
        };
        // A group keeps its id while it has a member. Between sweep's check
        // and this signal, the id can name another group only if every
        // member ended and a new process took the id in that time.
        let sent = if host.group == host.pid {
            rustix::process::kill_process_group(process, signal)
        } else {
            signal_process(process, host, signal)
        };
        match sent {
            Ok(()) => Ok(()),
            Err(error) if error == rustix::io::Errno::SRCH => Ok(()),
            Err(error) => Err(PhaseError::ProcessTable {
                message: format!(
                    "cannot send {signal:?} to the process {}: {error}",
                    host.pid
                ),
            }),
        }
    }
}

/// Sends `signal` to `process` alone. On Linux it opens a pidfd, checks the
/// start time again through the open pidfd's process, and signals the
/// pidfd, so a pid that the system reuses after the check is never
/// signalled. A Linux kernel older than 5.3 has no `pidfd_open` and fails
/// it with ENOSYS, and there `kill(2)` signals the pid after the check.
/// Elsewhere `kill(2)` signals the pid. There, if the recorded
/// process ends and the system gives its pid to a new process between
/// sweep's check and the signal, the new process receives the signal. The
/// system hands out pids in order, so that needs the pid numbers to wrap
/// around within that time.
#[cfg(target_os = "linux")]
fn signal_process(process: Pid, host: &HostProcess, signal: Signal) -> rustix::io::Result<()> {
    let pidfd = match rustix::process::pidfd_open(process, rustix::process::PidfdFlags::empty()) {
        Ok(pidfd) => Some(pidfd),
        Err(rustix::io::Errno::NOSYS) => None,
        Err(error) => return Err(error),
    };
    match start_time(host.pid) {
        Ok(Start::At(started)) if started == host.started_at_us => match pidfd {
            Some(pidfd) => rustix::process::pidfd_send_signal(&pidfd, signal),
            None => rustix::process::kill_process(process, signal),
        },
        _ => Err(rustix::io::Errno::SRCH),
    }
}

/// Sends `signal` to `process` alone, as the Linux version says.
#[cfg(not(target_os = "linux"))]
fn signal_process(process: Pid, _host: &HostProcess, signal: Signal) -> rustix::io::Result<()> {
    rustix::process::kill_process(process, signal)
}

/// Returns the id of the current boot from
/// `/proc/sys/kernel/random/boot_id`, which changes at every boot, so a
/// start time since boot from an earlier boot is never matched.
#[cfg(target_os = "linux")]
fn boot_id() -> Result<Option<String>, PhaseError> {
    use std::io::Read as _;

    let path = "/proc/sys/kernel/random/boot_id";
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(128).read_to_string(&mut text))
        .map_err(|error| PhaseError::ProcessTable {
            message: format!("cannot read {path}: {error}"),
        })?;
    Ok(Some(text.trim().to_owned()))
}

/// Returns `None`: macOS gives start times since the epoch, which do not
/// depend on the boot.
#[cfg(not(target_os = "linux"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the Linux version of boot_id can fail, and both share one signature"
)]
const fn boot_id() -> Result<Option<String>, PhaseError> {
    Ok(None)
}
