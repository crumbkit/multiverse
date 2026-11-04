//! The call into macOS's `libproc` that reads a process's start time, which
//! is the only unsafe code in phase (BASE-3). It copies what the kernel
//! writes into a byte buffer that it owns, and the safe code in [`super`]
//! reads fields from those bytes at the offsets that `<sys/proc_info.h>`
//! gives. multiverse's `process/darwin.rs` makes the same call the same
//! way.
#![expect(
    unsafe_code,
    reason = "crates/phase/src/record/process/darwin.rs is the one module of phase that calls libproc, which has no safe interface"
)]

use std::ffi::c_void;
use std::io;

/// The size of `struct proc_bsdinfo`.
pub(super) const BSDINFO_BYTES: usize = 136;

/// Why a libproc call gave no struct.
#[derive(Debug)]
pub(super) enum Failure {
    /// The call failed with this error, such as ESRCH for a process that
    /// has ended.
    Os(io::Error),
    /// The call succeeded and wrote `written` bytes where the struct that
    /// phase reads has [`BSDINFO_BYTES`], so this macOS's struct differs
    /// from it.
    Size {
        /// The bytes that the call wrote.
        written: usize,
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
    let size = i32::try_from(BSDINFO_BYTES).unwrap_or(i32::MAX);
    // SAFETY: `info` is a live, writable buffer of `BSDINFO_BYTES` bytes,
    // and the kernel writes at most the size it is given.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast::<c_void>(),
            size,
        )
    };
    match usize::try_from(written) {
        Ok(written) if written == BSDINFO_BYTES => Ok(info),
        Ok(written) if written > 0 => Err(Failure::Size { written }),
        _ => Err(Failure::Os(io::Error::last_os_error())),
    }
}
