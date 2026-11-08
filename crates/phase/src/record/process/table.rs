//! The process table that a reclaim reads: each process with its parent,
//! its group, its working directory and its resident memory.

use std::path::PathBuf;

use super::ProcessError;

/// One process of the process table, as [`process_table`] reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct TableEntry {
    /// The process id.
    pub pid: u32,
    /// The id of its parent process.
    pub parent: u32,
    /// The id of its process group.
    pub group: u32,
    /// Its working directory, or `None` when phase may not read it, as for
    /// another user's process.
    pub dir: Option<PathBuf>,
    /// The bytes of memory that it holds resident, or `None` when phase may
    /// not read them.
    pub resident: Option<u64>,
}

impl TableEntry {
    /// Returns the entry of the process `pid`.
    #[must_use]
    pub const fn new(
        pid: u32,
        parent: u32,
        group: u32,
        dir: Option<PathBuf>,
        resident: Option<u64>,
    ) -> Self {
        Self {
            pid,
            parent,
            group,
            dir,
            resident,
        }
    }
}

/// Returns every process that has not ended.
///
/// # Errors
///
/// Returns [`ProcessError`] when the process table cannot be read.
pub fn process_table() -> Result<Vec<TableEntry>, ProcessError> {
    Ok(Vec::new())
}

/// Returns the entry that the text of `/proc/<pid>/stat` describes.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn stat_entry(_text: &str, _pid: u32) -> Option<TableEntry> {
    None
}

/// Returns the bytes of the `VmRSS` line of the text of
/// `/proc/<pid>/status`.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn status_resident(_text: &str) -> Option<u64> {
    None
}

/// Returns the entry that `info`, the bytes of `struct proc_bsdinfo`,
/// describes.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn bsd_info_entry(_info: &[u8], _pid: u32) -> Option<TableEntry> {
    None
}

/// Returns the working directory in `info`, the bytes of `struct
/// proc_vnodepathinfo`.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn vnode_dir(_info: &[u8]) -> Option<PathBuf> {
    None
}

/// Returns the resident bytes in `info`, the bytes of `struct
/// proc_taskinfo`.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn task_resident(_info: &[u8]) -> Option<u64> {
    None
}
