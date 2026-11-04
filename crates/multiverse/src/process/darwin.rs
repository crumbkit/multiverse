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

/// `PROC_PGRP_ONLY` of `<sys/proc_info.h>`: list the members of a process
/// group.
const PROC_PGRP_ONLY: u32 = 2;

/// `PROC_PIDFDSOCKETINFO` of `<sys/proc_info.h>`: describe a socket.
const PROC_PIDFDSOCKETINFO: i32 = 3;

/// The size of `struct proc_bsdinfo`.
pub(super) const BSDINFO_BYTES: usize = 136;

/// The size of `struct socket_fdinfo`.
pub(super) const SOCKET_FDINFO_BYTES: usize = 792;

/// Returns `struct proc_bsdinfo` of `pid` as bytes, from
/// `proc_pidinfo(PROC_PIDTBSDINFO)`.
pub(super) fn bsd_info(pid: i32) -> io::Result<[u8; BSDINFO_BYTES]> {
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
    if usize::try_from(written).ok() == Some(BSDINFO_BYTES) {
        Ok(info)
    } else {
        Err(error_or_short("proc_pidinfo(PROC_PIDTBSDINFO)"))
    }
}

/// Returns the ids of the members of the process group `group`.
pub(super) fn group_members(group: i32) -> io::Result<Vec<i32>> {
    let bytes = listing("proc_listpids(PROC_PGRP_ONLY)", |buffer, size| {
        // SAFETY: `buffer` is null with `size` 0, or a live, writable buffer
        // of `size` bytes, and the kernel writes at most `size` bytes.
        unsafe { libc::proc_listpids(PROC_PGRP_ONLY, group.cast_unsigned(), buffer, size) }
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
pub(super) fn descriptors(pid: i32) -> io::Result<Vec<u8>> {
    listing("proc_pidinfo(PROC_PIDLISTFDS)", |buffer, size| {
        // SAFETY: `buffer` is null with `size` 0, or a live, writable buffer
        // of `size` bytes, and the kernel writes at most `size` bytes.
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, buffer, size) }
    })
}

/// Returns `struct socket_fdinfo` of the descriptor `fd` of `pid` as
/// bytes, from `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)`.
pub(super) fn socket_info(pid: i32, fd: i32) -> io::Result<Vec<u8>> {
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
    if usize::try_from(written).ok() == Some(SOCKET_FDINFO_BYTES) {
        Ok(info)
    } else {
        Err(error_or_short("proc_pidfdinfo(PROC_PIDFDSOCKETINFO)"))
    }
}

/// Runs `call` once with a null buffer, which returns the bytes the list
/// needs now, and again with a buffer of that size and room for 32 more
/// entries, and returns the bytes that the second call wrote.
fn listing(name: &str, call: impl Fn(*mut c_void, i32) -> i32) -> io::Result<Vec<u8>> {
    let needed = call(std::ptr::null_mut(), 0);
    let Ok(needed) = usize::try_from(needed) else {
        return Err(error_or_short(name));
    };
    let mut buffer = vec![0_u8; needed + 32 * 8];
    let written = call(buffer.as_mut_ptr().cast::<c_void>(), c_int(buffer.len()));
    let Ok(written) = usize::try_from(written) else {
        return Err(error_or_short(name));
    };
    buffer.truncate(written.min(buffer.len()));
    Ok(buffer)
}

/// Returns `size` as the C `int` that libproc takes. Every size here is a
/// struct's size or a process's descriptor list, far below `i32::MAX`.
fn c_int(size: usize) -> i32 {
    i32::try_from(size).unwrap_or(i32::MAX)
}

/// Returns the error of a libproc call that failed or wrote less than a
/// whole struct, with the call's name.
fn error_or_short(name: &str) -> io::Error {
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code) if code != 0 => error,
        _ => io::Error::other(format!("{name} wrote less than a whole struct")),
    }
}
