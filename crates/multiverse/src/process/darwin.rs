//! The calls into macOS's `libproc` that read the process table, which are
//! the only unsafe code in mxv (BASE-3). Each function copies what the
//! kernel writes into a byte buffer that it owns, and the safe code in
//! [`super`] reads fields from those bytes at the offsets that
//! `<sys/proc_info.h>` gives.
#![expect(
    unsafe_code,
    reason = "crates/multiverse/src/process/darwin.rs is the one module that calls libproc, which has no safe interface"
)]

use std::ffi::c_void;
use std::io;

/// `PROC_ALL_PIDS` of `<sys/proc_info.h>`: list every process.
const PROC_ALL_PIDS: u32 = 1;

/// `PROC_PIDFDSOCKETINFO` of `<sys/proc_info.h>`: describe a socket.
const PROC_PIDFDSOCKETINFO: i32 = 3;

/// The size of `struct proc_bsdinfo`.
pub(super) const BSDINFO_BYTES: usize = 136;

/// The size of `struct socket_fdinfo`.
pub(super) const SOCKET_FDINFO_BYTES: usize = 792;

/// Why a libproc call gave no struct.
#[derive(Debug)]
pub(super) enum Failure {
    /// The call failed with this error, such as ESRCH for a process that
    /// has ended.
    Os(io::Error),
    /// The call succeeded and wrote `written` bytes where the struct that
    /// mxv reads has `expected`, so this macOS's struct differs from it.
    Size {
        /// The call.
        call: &'static str,
        /// The bytes that the call wrote.
        written: usize,
        /// The bytes of the struct that mxv reads.
        expected: usize,
    },
}

impl Failure {
    /// Returns the errno of a failed call, or `None` for a struct of
    /// another size.
    pub(super) fn errno(&self) -> Option<i32> {
        match self {
            Self::Os(error) => error.raw_os_error(),
            Self::Size { .. } => None,
        }
    }
}

/// Returns `struct proc_bsdinfo` of `pid` as bytes, from
/// `proc_pidinfo(PROC_PIDTBSDINFO)`.
pub(super) fn bsd_info(pid: i32) -> Result<[u8; BSDINFO_BYTES], Failure> {
    let mut info = [0_u8; BSDINFO_BYTES];
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
pub(super) fn all_processes() -> Result<Vec<i32>, Failure> {
    let bytes = listing(|buffer, size| {
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
pub(super) fn descriptors(pid: i32) -> Result<Vec<u8>, Failure> {
    listing(|buffer, size| {
        // SAFETY: `buffer` is null with `size` 0, or a live, writable buffer
        // of `size` bytes, and the kernel writes at most `size` bytes.
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, buffer, size) }
    })
}

/// Returns `struct socket_fdinfo` of the descriptor `fd` of `pid` as
/// bytes, from `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)`.
pub(super) fn socket_info(pid: i32, fd: i32) -> Result<Vec<u8>, Failure> {
    let mut info = vec![0_u8; SOCKET_FDINFO_BYTES];
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

/// Runs `call` once with a null buffer, which returns the bytes the list
/// needs now, and again with a buffer of that size and room for 32 more
/// entries, and returns the bytes that the second call wrote.
fn listing(call: impl Fn(*mut c_void, i32) -> i32) -> Result<Vec<u8>, Failure> {
    let needed = call(std::ptr::null_mut(), 0);
    let Ok(needed) = usize::try_from(needed) else {
        return Err(Failure::Os(io::Error::last_os_error()));
    };
    let mut buffer = vec![0_u8; needed + 32 * 8];
    let written = call(buffer.as_mut_ptr().cast::<c_void>(), c_int(buffer.len()));
    let Ok(written) = usize::try_from(written) else {
        return Err(Failure::Os(io::Error::last_os_error()));
    };
    buffer.truncate(written.min(buffer.len()));
    Ok(buffer)
}

/// Returns `size` as the C `int` that libproc takes. Every size here is a
/// struct's size or a process list, far below `i32::MAX`.
fn c_int(size: usize) -> i32 {
    i32::try_from(size).unwrap_or(i32::MAX)
}
