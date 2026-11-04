//! Finds and stops a host process that the run record names. A recorded
//! process is the process with its pid whose start time equals the
//! recorded one, so a pid that the system gave to another process is never
//! mistaken for it.
//!
//! [`start_time`] reads `/proc/<pid>/stat` on Linux and calls
//! `proc_pidinfo` on macOS, as multiverse's process module does. The call
//! into libproc is in the `darwin` module, which holds phase's only unsafe
//! code.

use rustix::process::{Pid, Signal};

use crate::error::PhaseError;

#[cfg(target_os = "macos")]
mod darwin;

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
    #[cfg(target_os = "macos")]
    {
        macos_start_time(pid)
    }
    #[cfg(target_os = "linux")]
    {
        linux_start_time(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        Err(table(
            "phase reads process start times only on Linux and macOS".to_owned(),
        ))
    }
}

/// Returns the error of a process table that cannot be read.
fn table(message: String) -> PhaseError {
    PhaseError::ProcessTable { message }
}

/// Returns the start that `proc_pidinfo(PROC_PIDTBSDINFO)` reports for
/// `pid`.
#[cfg(target_os = "macos")]
fn macos_start_time(pid: u32) -> Result<Start, PhaseError> {
    let Ok(id) = i32::try_from(pid) else {
        return Ok(Start::Absent);
    };
    match darwin::bsd_info(id) {
        Ok(info) => bsd_info_start(&info)
            .map(Start::At)
            .ok_or_else(|| table(format!("the start time of the process {pid} overflows"))),
        Err(failure) if failure.errno() == Some(libc::ESRCH) => Ok(Start::Absent),
        Err(failure) if failure.errno() == Some(libc::EPERM) => Ok(Start::Unreadable),
        Err(darwin::Failure::Size { written }) => Err(table(format!(
            "proc_pidinfo(PROC_PIDTBSDINFO) for the process {pid} wrote {written} bytes, and phase reads a struct of {}",
            darwin::BSDINFO_BYTES
        ))),
        Err(darwin::Failure::Os(error)) => Err(table(format!(
            "libproc cannot read the process {pid}: {error}"
        ))),
    }
}

/// Returns the start time in `info`, the bytes of `struct proc_bsdinfo`,
/// whose `pbi_start_tvsec` and `pbi_start_tvusec` are 64-bit fields at
/// offsets 120 and 128, as microseconds since the epoch.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn bsd_info_start(info: &[u8]) -> Option<u64> {
    let read = |offset: usize| -> Option<u64> {
        let field = info.get(offset..offset.checked_add(8)?)?;
        Some(u64::from_ne_bytes(field.try_into().ok()?))
    };
    read(120)?.checked_mul(1_000_000)?.checked_add(read(128)?)
}

/// Returns the start in field 22 of `/proc/<pid>/stat`, converted from
/// clock ticks to microseconds.
#[cfg(target_os = "linux")]
fn linux_start_time(pid: u32) -> Result<Start, PhaseError> {
    use std::io::{self, Read as _};

    let path = format!("/proc/{pid}/stat");
    let mut text = String::new();
    let read =
        std::fs::File::open(&path).and_then(|file| file.take(4096).read_to_string(&mut text));
    match read {
        Ok(_) => {}
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) =>
        {
            return Ok(Start::Absent);
        }
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            return Ok(Start::Unreadable);
        }
        Err(error) => return Err(table(format!("cannot read {path}: {error}"))),
    }
    let ticks =
        stat_start_ticks(&text).ok_or_else(|| table(format!("{path} holds no start time")))?;
    ticks_to_micros(ticks, rustix::param::clock_ticks_per_second())
        .map(Start::At)
        .ok_or_else(|| table(format!("the start time in {path} overflows")))
}

/// Returns field 22 of `text`, the contents of `/proc/<pid>/stat`, which is
/// the process's start in clock ticks after boot. The command name in
/// field 2 may hold spaces and parentheses, so the fields are counted from
/// the last `)`, after which field 3 comes.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn stat_start_ticks(text: &str) -> Option<u64> {
    let fields = text.rfind(')').and_then(|end| text.get(end + 1..))?;
    fields.split_whitespace().nth(22 - 3)?.parse().ok()
}

/// Returns `ticks` clock ticks, of which a second holds `per_second`, as
/// microseconds, or `None` when the result overflows or `per_second` is 0.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn ticks_to_micros(ticks: u64, per_second: u64) -> Option<u64> {
    ticks.checked_mul(1_000_000)?.checked_div(per_second)
}

/// Returns `pid` as rustix's process id, or `None` for 0 or an id past
/// `i32::MAX`, which name no single process.
fn pid_of(pid: u32) -> Option<Pid> {
    i32::try_from(pid).ok().and_then(Pid::from_raw)
}

/// Reaps `pid` when it is an ended child of this process, because an
/// unreaped child keeps its pid and stays a member of its group. A pid that
/// is not this process's child is left alone.
pub(crate) fn reap(pid: u32) {
    if let Some(process) = pid_of(pid) {
        let _ = rustix::process::waitpid(Some(process), rustix::process::WaitOptions::NOHANG);
    }
}

/// Reports whether the process group `group` still has a member. A group
/// that phase may not signal still exists.
pub(crate) fn group_exists(group: u32) -> bool {
    let Some(group) = pid_of(group) else {
        return false;
    };
    match rustix::process::test_kill_process_group(group) {
        Ok(()) => true,
        Err(error) => error != rustix::io::Errno::SRCH,
    }
}

/// Sends `signal` to the process group `group` when `group` is `pid`, so
/// the recorded process leads the group, and to the process `pid` alone
/// otherwise, so phase never signals a group that it did not create. A
/// process that has already gone is no failure.
///
/// # Errors
///
/// Returns [`PhaseError::ProcessTable`] when `kill(2)` fails for another
/// reason, such as EPERM.
pub(crate) fn signal(pid: u32, group: u32, signal: Signal) -> Result<(), PhaseError> {
    let Some(process) = pid_of(pid) else {
        return Ok(());
    };
    let sent = if group == pid {
        rustix::process::kill_process_group(process, signal)
    } else {
        rustix::process::kill_process(process, signal)
    };
    match sent {
        Ok(()) => Ok(()),
        Err(error) if error == rustix::io::Errno::SRCH => Ok(()),
        Err(error) => Err(table(format!(
            "cannot send {signal:?} to the process {pid}: {error}"
        ))),
    }
}
