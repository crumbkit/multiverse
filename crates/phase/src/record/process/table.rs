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

/// Returns every process that has not ended and that phase may read: on
/// Linux from `/proc/<pid>/stat`, `/proc/<pid>/status` and the link
/// `/proc/<pid>/cwd`, and on macOS from libproc. A process that ends while
/// phase reads the table is left out.
///
/// # Errors
///
/// Returns [`ProcessError`] when the process table cannot be read, and on
/// a system other than Linux and macOS.
pub fn process_table() -> Result<Vec<TableEntry>, ProcessError> {
    #[cfg(target_os = "linux")]
    {
        linux_table()
    }
    #[cfg(target_os = "macos")]
    {
        macos_table()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(ProcessError::table(
            "phase reads the process table only on Linux and macOS".to_owned(),
        ))
    }
}

/// Returns the process table from `/proc`.
#[cfg(target_os = "linux")]
fn linux_table() -> Result<Vec<TableEntry>, ProcessError> {
    use super::{ProcRead, read_proc};
    let entries = std::fs::read_dir("/proc")
        .map_err(|error| ProcessError::table(format!("/proc cannot be read: {error}")))?;
    let mut table = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let ProcRead::Text(stat) = read_proc(&format!("/proc/{pid}/stat"))? else {
            continue;
        };
        let Some(mut found) = stat_entry(&stat, pid) else {
            continue;
        };
        found.dir = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
        if let ProcRead::Text(status) = read_proc(&format!("/proc/{pid}/status"))? {
            found.resident = status_resident(&status);
        }
        table.push(found);
    }
    Ok(table)
}

/// Returns the process table from libproc.
#[cfg(target_os = "macos")]
fn macos_table() -> Result<Vec<TableEntry>, ProcessError> {
    use super::{darwin, darwin_error};
    let pids = darwin::all_processes().map_err(|failure| darwin_error(&failure, 0))?;
    let mut table = Vec::new();
    for pid in pids {
        let info = match darwin::bsd_info(pid) {
            Ok(info) => info,
            Err(failure) if matches!(failure.errno(), Some(libc::ESRCH | libc::EPERM)) => {
                continue;
            }
            Err(failure) => return Err(darwin_error(&failure, pid.cast_unsigned())),
        };
        let Some(mut found) = bsd_info_entry(&info, pid.cast_unsigned()) else {
            continue;
        };
        found.dir = darwin::vnode_path_info(pid)
            .ok()
            .and_then(|info| vnode_dir(&info));
        found.resident = darwin::task_info(pid)
            .ok()
            .and_then(|info| task_resident(&info));
        table.push(found);
    }
    Ok(table)
}

/// Returns the entry that the text of `/proc/<pid>/stat` describes, with
/// its parent in field 4 and its group in field 5, or `None` for a zombie
/// or a dead process, whose state in field 3 is `Z` or `X`. The entry has
/// no directory and no resident bytes yet.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn stat_entry(text: &str, pid: u32) -> Option<TableEntry> {
    use super::stat_field;
    let state: char = stat_field(text, 3)?;
    if matches!(state, 'Z' | 'X') {
        return None;
    }
    Some(TableEntry::new(
        pid,
        stat_field(text, 4)?,
        stat_field(text, 5)?,
        None,
        None,
    ))
}

/// Returns the bytes of the `VmRSS` line of the text of
/// `/proc/<pid>/status`, which counts kibibytes, or `None` when the text
/// has none, as for a kernel thread.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn status_resident(text: &str) -> Option<u64> {
    let line = text.lines().find_map(|line| line.strip_prefix("VmRSS:"))?;
    let kibibytes: u64 = line.split_whitespace().next()?.parse().ok()?;
    kibibytes.checked_mul(1024)
}

/// Returns the entry that `info`, the bytes of `struct proc_bsdinfo`,
/// describes, or `None` for a zombie: `pbi_status` is at offset 4,
/// `pbi_ppid` at 16 and `pbi_pgid` at 100. The entry has no directory and
/// no resident bytes yet.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn bsd_info_entry(info: &[u8], pid: u32) -> Option<TableEntry> {
    use super::{SZOMB, read_i32};
    if read_i32(info, 4)? == SZOMB {
        return None;
    }
    Some(TableEntry::new(
        pid,
        read_i32(info, 16)?.cast_unsigned(),
        read_i32(info, 100)?.cast_unsigned(),
        None,
        None,
    ))
}

/// The offset of `pvi_cdir.vip_path` in `struct proc_vnodepathinfo`, after
/// the 152 bytes of `struct vnode_info`.
#[cfg(any(target_os = "macos", test))]
const CDIR_PATH_OFFSET: usize = 152;

/// The bytes of `vip_path`, which is `MAXPATHLEN` long.
#[cfg(any(target_os = "macos", test))]
const MAXPATHLEN: usize = 1024;

/// Returns the working directory in `info`, the bytes of `struct
/// proc_vnodepathinfo`: the path of `pvi_cdir`, up to its NUL, or `None`
/// when it is empty.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn vnode_dir(info: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    let field = info.get(CDIR_PATH_OFFSET..CDIR_PATH_OFFSET.checked_add(MAXPATHLEN)?)?;
    let path = field.split(|byte| *byte == 0).next()?;
    (!path.is_empty()).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(path)))
}

/// Returns the resident bytes in `info`, the bytes of `struct
/// proc_taskinfo`, whose `pti_resident_size` is a 64-bit field at offset 8.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn task_resident(info: &[u8]) -> Option<u64> {
    super::read_u64(info, 8)
}
