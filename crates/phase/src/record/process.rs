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
use crate::record::run::HostProcess;

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
        let Some(group) = pid_of(group) else {
            return false;
        };
        match rustix::process::test_kill_process_group(group) {
            Ok(()) => true,
            Err(error) => error != rustix::io::Errno::SRCH,
        }
    }

    fn reap(&self, pid: u32) {
        if let Some(process) = pid_of(pid) {
            let _ = rustix::process::waitpid(Some(process), rustix::process::WaitOptions::NOHANG);
        }
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
            Err(error) => Err(table(format!(
                "cannot send {signal:?} to the process {}: {error}",
                host.pid
            ))),
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
        .map_err(|error| table(format!("cannot read {path}: {error}")))?;
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
