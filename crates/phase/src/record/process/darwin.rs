//! The calls into macOS's `libproc` that read the process table, which are
//! the only unsafe code in phase and in mxv (BASE-3). Each function copies
//! what the kernel writes into a byte buffer that it owns, and the safe
//! code in [`super`] and in the host runner reads fields from those bytes
//! at the offsets that `<sys/proc_info.h>` gives.
#![expect(
    unsafe_code,
    reason = "crates/phase/src/record/process/darwin.rs is the one module that calls libproc, which has no safe interface"
)]

use std::ffi::c_void;
use std::io;

/// `PROC_ALL_PIDS` of `<sys/proc_info.h>`: list every process.
const PROC_ALL_PIDS: u32 = 1;

/// `PROC_PIDFDSOCKETINFO` of `<sys/proc_info.h>`: describe a socket.
const PROC_PIDFDSOCKETINFO: i32 = 3;

/// The size of `struct proc_bsdinfo`.
pub(crate) const BSDINFO_BYTES: usize = 136;

/// The size of `struct socket_fdinfo`.
pub(crate) const SOCKET_FDINFO_BYTES: usize = 792;

/// Why a libproc call gave no struct.
#[derive(Debug)]
pub(crate) enum Failure {
    /// The call failed with this error, such as ESRCH for a process that
    /// has ended.
    Os(io::Error),
    /// The call succeeded and wrote `written` bytes where the struct that
    /// phase reads has `expected`, so this macOS's struct differs from it.
    Size {
        /// The call.
        call: &'static str,
        /// The bytes that the call wrote.
        written: usize,
        /// The bytes of the struct that phase reads.
        expected: usize,
    },
    /// The list filled every buffer that phase gave it, because it kept
    /// growing between the call that sized it and the call that read it.
    Growing {
        /// The call.
        call: &'static str,
        /// The times phase sized a buffer and read the list into it.
        tries: usize,
    },
}

impl Failure {
    /// Returns the errno of a failed call, or `None` for a struct of
    /// another size.
    pub(crate) fn errno(&self) -> Option<i32> {
        match self {
            Self::Os(error) => error.raw_os_error(),
            Self::Size { .. } | Self::Growing { .. } => None,
        }
    }
}

/// Returns `struct proc_bsdinfo` of `pid` as bytes, from
/// `proc_pidinfo(PROC_PIDTBSDINFO)`.
pub(crate) fn bsd_info(pid: i32) -> Result<[u8; BSDINFO_BYTES], Failure> {
    let mut info = [0_u8; BSDINFO_BYTES];
    clear_errno();
    // SAFETY: `info` is a live, writable buffer of `BSDINFO_BYTES` bytes,
    // and the kernel writes at most the size it is given.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast::<c_void>(),
            c_int(BSDINFO_BYTES),
        )
    };
    whole("proc_pidinfo(PROC_PIDTBSDINFO)", written, BSDINFO_BYTES)?;
    Ok(info)
}

/// Returns the ids of every process.
pub(crate) fn all_processes() -> Result<Vec<i32>, Failure> {
    let bytes = listing("proc_listpids(PROC_ALL_PIDS)", |buffer, size| {
        // SAFETY: `buffer` is null with `size` 0, or a live, writable buffer
        // of `size` bytes, and the kernel writes at most `size` bytes.
        unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, buffer, size) }
    })?;
    let (ids, _) = bytes.as_chunks::<4>();
    Ok(ids
        .iter()
        .map(|id| i32::from_ne_bytes(*id))
        .filter(|id| *id > 0)
        .collect())
}

/// Returns the descriptor list of `pid` as bytes: `struct proc_fdinfo`
/// entries of 8 bytes each, from `proc_pidinfo(PROC_PIDLISTFDS)`.
pub(crate) fn descriptors(pid: i32) -> Result<Vec<u8>, Failure> {
    listing("proc_pidinfo(PROC_PIDLISTFDS)", |buffer, size| {
        // SAFETY: `buffer` is null with `size` 0, or a live, writable buffer
        // of `size` bytes, and the kernel writes at most `size` bytes.
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, buffer, size) }
    })
}

/// Returns `struct socket_fdinfo` of the descriptor `fd` of `pid` as
/// bytes, from `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)`.
pub(crate) fn socket_info(pid: i32, fd: i32) -> Result<Vec<u8>, Failure> {
    let mut info = vec![0_u8; SOCKET_FDINFO_BYTES];
    clear_errno();
    // SAFETY: `info` is a live, writable buffer of `SOCKET_FDINFO_BYTES`
    // bytes, and the kernel writes at most the size it is given.
    let written = unsafe {
        libc::proc_pidfdinfo(
            pid,
            fd,
            PROC_PIDFDSOCKETINFO,
            info.as_mut_ptr().cast::<c_void>(),
            c_int(SOCKET_FDINFO_BYTES),
        )
    };
    whole(
        "proc_pidfdinfo(PROC_PIDFDSOCKETINFO)",
        written,
        SOCKET_FDINFO_BYTES,
    )?;
    Ok(info)
}

/// Checks that a call that returned `written` wrote a whole struct of
/// `expected` bytes. A result of zero or less is a failure with errno.
fn whole(call: &'static str, written: i32, expected: usize) -> Result<(), Failure> {
    match usize::try_from(written) {
        Ok(written) if written == expected => Ok(()),
        Ok(written) if written > 0 => Err(Failure::Size {
            call,
            written,
            expected,
        }),
        _ => Err(Failure::Os(io::Error::last_os_error())),
    }
}

/// The times [`listing`] sizes a buffer and reads a list into it before it
/// gives up on a list that keeps growing.
const MOST_LISTING_TRIES: usize = 4;

/// The entries of room that [`listing`] adds to the size that a list needs
/// now, so that a list that grows a little between the two calls still
/// fits.
const LISTING_SLACK_ENTRIES: usize = 32;

/// The size of each entry of a list: a pid, or a `struct proc_fdinfo`.
const MOST_LISTING_ENTRY_BYTES: usize = 8;

/// Runs `call` with a null buffer, which returns the bytes the list needs
/// now, and again with a buffer of that size and room for
/// [`LISTING_SLACK_ENTRIES`] more entries, and returns the bytes that the
/// second call wrote. libproc writes no more than the buffer holds and does
/// not say whether the list had more, so a call that fills the buffer may
/// have cut the list short. Then `listing` tries again with a buffer at
/// least twice as large, [`MOST_LISTING_TRIES`] times in all.
///
/// errno is cleared before each call, so a failure that sets no errno reads
/// as errno 0 and never as a stale ESRCH or EPERM.
fn listing(name: &'static str, call: impl Fn(*mut c_void, i32) -> i32) -> Result<Vec<u8>, Failure> {
    let mut least = 0;
    for _ in 0..MOST_LISTING_TRIES {
        clear_errno();
        let needed = call(std::ptr::null_mut(), 0);
        let Ok(needed) = usize::try_from(needed) else {
            return Err(Failure::Os(io::Error::last_os_error()));
        };
        let size = (needed + LISTING_SLACK_ENTRIES * MOST_LISTING_ENTRY_BYTES).max(least);
        let mut buffer = vec![0_u8; size];
        clear_errno();
        let written = call(buffer.as_mut_ptr().cast::<c_void>(), c_int(buffer.len()));
        let Ok(written) = usize::try_from(written) else {
            return Err(Failure::Os(io::Error::last_os_error()));
        };
        if written < buffer.len() {
            buffer.truncate(written);
            return Ok(buffer);
        }
        least = size.saturating_mul(2);
    }
    Err(Failure::Growing {
        call: name,
        tries: MOST_LISTING_TRIES,
    })
}

/// Sets this thread's errno to 0.
fn clear_errno() {
    // SAFETY: `__error` returns a pointer to the calling thread's errno,
    // which this thread may write.
    unsafe {
        *libc::__error() = 0;
    }
}

/// Returns `size` as the C `int` that libproc takes. Every size here is a
/// struct's size or a process list, far below `i32::MAX`.
fn c_int(size: usize) -> i32 {
    i32::try_from(size).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests;
