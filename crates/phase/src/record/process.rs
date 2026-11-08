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

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustix::process::{Pid, Signal};

use crate::clock::{POLL_INTERVAL, now, pause};
use crate::engine::host::{HostStop, HostStopped};
use crate::error::PhaseError;
use crate::record::run::HostProcess;

#[cfg(target_os = "macos")]
pub(crate) mod darwin;
mod table;

pub use table::{TableEntry, process_table};
#[cfg(test)]
pub(crate) use table::{bsd_info_entry, stat_entry, status_resident, task_resident, vnode_dir};

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
        darwin::Failure::Growing { call, tries } => ProcessError::table(format!(
            "{call} filled every buffer in {tries} tries, because the list kept growing"
        )),
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

/// A process id that phase may signal. It is never 0, which `kill(2)` reads
/// as the caller's own group, never 1, which is init and which as a group
/// id makes `kill(2)` signal every process the caller may signal, and never
/// an id past `i32::MAX`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProcessId(Pid);

/// A process group id that may be signalled, with the same limits as
/// [`ProcessId`]: a group of 0 names the caller's own group and a group of
/// 1 makes `kill(2)` signal every process the caller may signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProcessGroup(Pid);

/// The refusal of an id that [`ProcessId`] or [`ProcessGroup`] does not
/// accept: 0, 1, or an id past `i32::MAX`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadProcessId {
    /// The refused id.
    pub id: u32,
}

impl fmt::Display for BadProcessId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "phase refuses to signal the id {}, because 0, 1 and ids past {} name no single process or group",
            self.id,
            i32::MAX
        )
    }
}

impl std::error::Error for BadProcessId {}

impl From<BadProcessId> for io::Error {
    fn from(refused: BadProcessId) -> Self {
        Self::new(io::ErrorKind::InvalidInput, refused)
    }
}

impl ProcessId {
    /// Returns `pid` as an id that may be signalled.
    ///
    /// # Errors
    ///
    /// Returns [`BadProcessId`] for 0, 1 and any id past `i32::MAX`.
    pub fn new(pid: u32) -> Result<Self, BadProcessId> {
        checked(pid).map(Self)
    }

    /// Returns the id as a number.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0.as_raw_nonzero().get().unsigned_abs()
    }
}

impl ProcessGroup {
    /// Returns `group` as a group id that may be signalled.
    ///
    /// # Errors
    ///
    /// Returns [`BadProcessId`] for 0, 1 and any id past `i32::MAX`.
    pub fn new(group: u32) -> Result<Self, BadProcessId> {
        checked(group).map(Self)
    }

    /// Returns the id as a number.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0.as_raw_nonzero().get().unsigned_abs()
    }
}

/// Returns `id` as rustix's process id when it is above 1 and at most
/// `i32::MAX`.
fn checked(id: u32) -> Result<Pid, BadProcessId> {
    i32::try_from(id)
        .ok()
        .filter(|raw| *raw > 1)
        .and_then(Pid::from_raw)
        .ok_or(BadProcessId { id })
}

/// Sends `signal` to the process group `group`.
///
/// # Errors
///
/// Returns the failure of `kill(2)`, such as ESRCH when the group is gone.
pub fn signal_group(group: ProcessGroup, signal: Signal) -> io::Result<()> {
    rustix::process::kill_process_group(group.0, signal).map_err(io::Error::from)
}

/// Sends `signal` to the process `pid` alone.
///
/// # Errors
///
/// Returns the failure of `kill(2)`, such as ESRCH when no process has the
/// id.
pub fn signal_pid(pid: ProcessId, signal: Signal) -> io::Result<()> {
    rustix::process::kill_process(pid.0, signal).map_err(io::Error::from)
}

/// Reports whether the process group `group` still has a member that has
/// not ended. A group that the caller may not signal still exists. A
/// member that has ended and that its parent has not reaped yet, a zombie,
/// runs nothing and no signal ends it, so a group of only such members
/// counts as gone. When the members cannot be listed, a group that
/// `kill(2)` finds counts as existing.
#[must_use]
pub fn group_exists(group: ProcessGroup) -> bool {
    match rustix::process::test_kill_process_group(group.0) {
        Err(error) if error == rustix::io::Errno::SRCH => false,
        Ok(()) | Err(_) => live_members(group).is_none_or(|members| !members.is_empty()),
    }
}

/// A member of a process group that has not ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Member {
    /// The member's pid.
    pub pid: u32,
    /// Its start, as [`raw_start`] gives it.
    pub start: RawStart,
}

/// Returns the members of `group` that have not ended and that phase may
/// read, or `None` when this system gives no way to list them or the
/// process table cannot be read. A member of another user that phase may
/// not read is left out.
#[must_use]
pub fn live_members(group: ProcessGroup) -> Option<Vec<Member>> {
    #[cfg(target_os = "linux")]
    {
        let mut members = Vec::new();
        for entry in std::fs::read_dir("/proc").ok()? {
            let Some(pid) = entry
                .ok()
                .and_then(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            else {
                continue;
            };
            if let ProcRead::Text(text) = read_proc(&format!("/proc/{pid}/stat")).ok()? {
                members.extend(stat_member(&text, pid, group.get()));
            }
        }
        Some(members)
    }
    #[cfg(target_os = "macos")]
    {
        let mut members = Vec::new();
        for pid in darwin::all_processes().ok()? {
            match darwin::bsd_info(pid) {
                Ok(info) => {
                    members.extend(bsd_info_member(&info, pid.cast_unsigned(), group.get()));
                }
                Err(failure) if matches!(failure.errno(), Some(libc::ESRCH | libc::EPERM)) => {}
                Err(_) => return None,
            }
        }
        Some(members)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = group;
        None
    }
}

/// Returns the member `pid` of `group` that the text of its
/// `/proc/<pid>/stat` describes, or `None` when it is in another group or
/// has ended: its state, field 3, is `Z` for a zombie or `X` for a dead
/// process.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn stat_member(text: &str, pid: u32, group: u32) -> Option<Member> {
    let state: char = stat_field(text, 3)?;
    if stat_field::<u32>(text, 5)? != group || matches!(state, 'Z' | 'X') {
        return None;
    }
    let start = stat_field(text, 22).map_or(RawStart::Unreadable, RawStart::Ticks);
    Some(Member { pid, start })
}

/// `SZOMB` of `<sys/proc.h>`: the `pbi_status` of a zombie.
#[cfg(any(target_os = "macos", test))]
const SZOMB: i32 = 5;

/// Returns the member `pid` of `group` that `info`, the bytes of its
/// `struct proc_bsdinfo`, describes, or `None` when it is in another group
/// or is a zombie: `pbi_status` at offset 4 is `SZOMB`, and `pbi_pgid` is
/// at offset 100.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn bsd_info_member(info: &[u8], pid: u32, group: u32) -> Option<Member> {
    let status = read_i32(info, 4)?;
    if read_i32(info, 100)?.cast_unsigned() != group || status == SZOMB {
        return None;
    }
    Some(Member {
        pid,
        start: bsd_info_since(info)?,
    })
}

/// Returns when a process whose start is `start` started, or `None` when
/// phase cannot tell. On macOS the start counts from the epoch. On Linux it
/// counts in clock ticks from the boot, whose time is the current time less
/// the uptime of `/proc/uptime`, so the result can be off by a few
/// hundredths of a second.
#[must_use]
pub fn started_at(start: RawStart) -> Option<SystemTime> {
    match start {
        RawStart::Since {
            seconds,
            microseconds,
        } => UNIX_EPOCH
            .checked_add(Duration::from_secs(seconds))?
            .checked_add(Duration::from_micros(microseconds)),
        RawStart::Ticks(ticks) => {
            let since_boot = ticks_to_micros(ticks, clock_ticks_per_second())?;
            boot_time()?.checked_add(Duration::from_micros(since_boot))
        }
        RawStart::Absent | RawStart::Unreadable => None,
    }
}

/// Returns when this machine booted, from `/proc/uptime`, or `None` off
/// Linux.
fn boot_time() -> Option<SystemTime> {
    #[cfg(target_os = "linux")]
    {
        let ProcRead::Text(text) = read_proc("/proc/uptime").ok()? else {
            return None;
        };
        SystemTime::now().checked_sub(uptime(&text)?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Returns the uptime in the first field of the text of `/proc/uptime`,
/// such as `12345.67 54321.00`.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn uptime(text: &str) -> Option<Duration> {
    let seconds: f64 = text.split_whitespace().next()?.parse().ok()?;
    Duration::try_from_secs_f64(seconds).ok()
}

/// Reaps `pid` when it is an ended child of this process, because an
/// unreaped child keeps its pid and stays a member of its group. A pid
/// that is not this process's child is left alone.
///
/// A caller that holds a [`std::process::Child`] for the same pid loses its
/// exit status to this call: the child's own `wait` then fails with ECHILD.
pub fn reap(pid: ProcessId) {
    let _ = rustix::process::waitpid(Some(pid.0), rustix::process::WaitOptions::NOHANG);
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
        ProcessGroup::new(group).is_ok_and(group_exists)
    }

    fn reap(&self, pid: u32) {
        if let Ok(pid) = ProcessId::new(pid) {
            reap(pid);
        }
    }

    fn signal(&self, host: &HostProcess, signal: Signal) -> Result<(), PhaseError> {
        let process = checked(host.pid).map_err(|refused| PhaseError::ProcessTable {
            message: refused.to_string(),
        })?;
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
            // macOS refuses a signal to a group whose members have all
            // ended and wait to be reaped, with EPERM. Nothing in such a
            // group runs, so the signal has nothing left to do.
            Err(error)
                if error == rustix::io::Errno::PERM
                    && host.group == host.pid
                    && ProcessGroup::new(host.group)
                        .ok()
                        .and_then(live_members)
                        .is_some_and(|members| members.is_empty()) =>
            {
                Ok(())
            }
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

/// Returns when a process whose recorded start is `started_at_us`
/// started, in milliseconds since the Unix epoch, or `None` when phase
/// cannot tell, as [`started_at`] says.
pub(crate) fn started_at_ms(started_at_us: u64) -> Option<u64> {
    let since = Duration::from_micros(started_at_us);
    let started = if cfg!(target_os = "macos") {
        UNIX_EPOCH.checked_add(since)?
    } else {
        boot_time()?.checked_add(since)?
    };
    u64::try_from(started.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()
}

/// What a recorded host process is now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Identity {
    /// The recorded process, or a member of its group, is running.
    Ours,
    /// Another process has the pid.
    Reused,
    /// phase may not read the process that has the pid, so it cannot tell
    /// whether it is the recorded one.
    Unknown,
    /// The recorded process led its group and has gone, and a group with
    /// its id still has a live member. That group may be the one phase
    /// started, or a stranger's: another program may have taken the free
    /// pid and made it the leader of a new group with `setsid`. phase
    /// treats it as its own only when something proves it, such as its own
    /// keeper still running, or an earlier look in the same stop that found
    /// the leader.
    LeaderGone,
    /// Nothing of the recorded process is left.
    Gone,
}

/// Returns what the recorded process `host` is now, as `processes` shows
/// it. A leader whose pid has no process may have left members in its
/// group, and the group is then [`Identity::LeaderGone`], because nothing
/// here proves that the group was never empty since the leader ended. It
/// reaps nothing.
///
/// A start time counts from the boot on a system that has a boot id, so a
/// process recorded in another boot is gone, and a process recorded there
/// without a boot id cannot be matched and is unknown.
///
/// # Errors
///
/// Returns [`PhaseError::ProcessTable`] when the process table cannot be
/// read.
pub(crate) fn identify(
    processes: &dyn Processes,
    host: &HostProcess,
) -> Result<Identity, PhaseError> {
    match (&host.boot_id, processes.boot_id()?) {
        (Some(recorded), current) if current.as_ref() != Some(recorded) => {
            return Ok(Identity::Gone);
        }
        (None, Some(_)) => return Ok(Identity::Unknown),
        _ => {}
    }
    Ok(match processes.start_time(host.pid)? {
        Start::At(started) if started == host.started_at_us => Identity::Ours,
        Start::At(_) => Identity::Reused,
        Start::Unreadable => Identity::Unknown,
        Start::Absent if host.group == host.pid && processes.group_exists(host.group) => {
            Identity::LeaderGone
        }
        Start::Absent => Identity::Gone,
    })
}

/// Reports whether phase may signal the recorded process `host`: its pid
/// and its group are both ids that [`ProcessId`] and [`ProcessGroup`]
/// accept, which refuse 0, 1 and ids past `i32::MAX`. The stop checks this
/// first so that it reports such a record as not ours rather than failing
/// at the signal. A signal to group 0 reaches the sender's own group, and `kill(-1)` reaches every process the user may
/// signal, so phase never sends either.
pub(crate) fn signallable(host: &HostProcess) -> bool {
    ProcessId::new(host.pid).is_ok() && ProcessGroup::new(host.group).is_ok()
}

/// Stops the keeper and the app of `stop` through `processes`, as
/// [`HostStop`] describes, and gives up the first wait at `deadline`. The
/// app's grace and kill wait count from the signal that `stop_host` sends
/// it, so an app whose keeper ends late still has its whole grace, up to
/// `deadline`. After `KILL`, it waits the kill wait even when `deadline` has
/// passed, so it returns by `deadline` plus the kill wait. A wait that does
/// not fit in an [`Instant`] ends at `deadline`.
///
/// A recorded pid or group of 0 or 1 makes it signal nothing and report
/// [`HostStopped::NotOurs`]. A group whose leader has gone counts as ours
/// only when [`vouched`] or [`settle`] proves it. Otherwise `stop_host`
/// signals nothing and reports [`HostStopped::GroupUnverified`]. It reaps
/// each process when it is an ended child of this process, so a keeper
/// that this process started is not kept as a zombie.
///
/// # Errors
///
/// Returns [`PhaseError::ProcessTable`] when the process table cannot be
/// read or a signal cannot be sent.
pub(crate) fn stop_host(
    processes: &dyn Processes,
    stop: &HostStop<'_>,
    deadline: Instant,
) -> Result<HostStopped, PhaseError> {
    let look = |host: Option<&HostProcess>| -> Result<Identity, PhaseError> {
        match host {
            Some(host) => {
                processes.reap(host.pid);
                identify(processes, host)
            }
            None => Ok(Identity::Gone),
        }
    };
    if [stop.keeper, stop.app]
        .into_iter()
        .flatten()
        .any(|host| !signallable(host))
    {
        return Ok(HostStopped::NotOurs);
    }
    let keeper = look(stop.keeper)?;
    let app = vouched(keeper, look(stop.app)?);
    if keeper != Identity::Ours && app != Identity::Ours {
        return Ok(ended(stop, keeper, app).unwrap_or(HostStopped::AlreadyGone));
    }
    let signal = stop.signal.signal();
    let mut app_signalled = false;
    if let (Some(host), Identity::Ours) = (stop.keeper, keeper) {
        processes.signal(host, Signal::TERM)?;
    }
    let mut first = after(stop.grace.saturating_add(stop.kill_wait), deadline).min(deadline);
    let (mut keeper, mut app) = (keeper, app);
    loop {
        keeper = settle(keeper, look(stop.keeper)?);
        app = settle(app, vouched(keeper, look(stop.app)?));
        if keeper != Identity::Ours && app != Identity::Ours {
            return Ok(ended(stop, keeper, app).unwrap_or(HostStopped::Stopped));
        }
        if keeper != Identity::Ours && !app_signalled {
            if let Some(host) = stop.app.filter(|_| app == Identity::Ours) {
                processes.signal(host, signal)?;
                // The app has its whole grace from its own signal, so a
                // keeper that ended late does not shorten it.
                first = first
                    .max(after(stop.grace.saturating_add(stop.kill_wait), deadline).min(deadline));
            }
            app_signalled = true;
        }
        if now() >= first {
            break;
        }
        pause(POLL_INTERVAL);
    }
    for (host, identity) in [(stop.keeper, keeper), (stop.app, app)] {
        if let (Some(host), Identity::Ours) = (host, identity) {
            processes.signal(host, Signal::KILL)?;
        }
    }
    let last = after(stop.kill_wait, deadline);
    loop {
        keeper = settle(keeper, look(stop.keeper)?);
        app = settle(app, vouched(keeper, look(stop.app)?));
        if keeper != Identity::Ours && app != Identity::Ours {
            return Ok(ended(stop, keeper, app).unwrap_or(HostStopped::Stopped));
        }
        if now() >= last {
            let left = if app == Identity::Ours {
                stop.app
            } else {
                stop.keeper
            };
            return Ok(HostStopped::Unstoppable {
                group: left.map_or(0, |host| host.group),
            });
        }
        pause(POLL_INTERVAL);
    }
}

/// Returns the time `wait` from now, or `deadline` when that does not fit
/// in an [`Instant`], as for a wait of [`Duration::MAX`].
fn after(wait: Duration, deadline: Instant) -> Instant {
    now().checked_add(wait).unwrap_or(deadline)
}

/// Returns what the app is, given that its keeper is `keeper`. A keeper
/// that phase proved its own started the app, recorded the app's group,
/// and reaps the app when it ends, so while the keeper runs, a group of
/// the app's whose leader has gone is the app's own group.
pub(crate) fn vouched(keeper: Identity, app: Identity) -> Identity {
    if keeper == Identity::Ours && app == Identity::LeaderGone {
        Identity::Ours
    } else {
        app
    }
}

/// Returns what a process that was `before` is now that it reads as
/// `after`. A process that phase stopped stays stopped, even when its pid
/// is reused meanwhile. A group whose leader this stop found to be phase's
/// own stays its own after the leader ends, because a group keeps its id
/// while it has a member, so no stranger can take the id meanwhile.
const fn settle(before: Identity, after: Identity) -> Identity {
    match (before, after) {
        (Identity::Gone | Identity::Reused, _) => before,
        (Identity::Ours, Identity::LeaderGone) => Identity::Ours,
        _ => after,
    }
}

/// Returns how a stop ended once neither the keeper nor the app is ours,
/// or `None` when nothing of either is left. A group whose leader has gone
/// and that nothing proved ours is reported by its id, and phase never
/// signalled it.
fn ended(stop: &HostStop<'_>, keeper: Identity, app: Identity) -> Option<HostStopped> {
    for (host, identity) in [(stop.app, app), (stop.keeper, keeper)] {
        if let (Some(host), Identity::LeaderGone) = (host, identity) {
            return Some(HostStopped::GroupUnverified { group: host.group });
        }
    }
    (keeper == Identity::Unknown || app == Identity::Unknown).then_some(HostStopped::NotOurs)
}
