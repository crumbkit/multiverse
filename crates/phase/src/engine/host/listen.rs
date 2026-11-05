//! Which process holds the socket that listens on a TCP port.
//!
//! On Linux it reads the listening sockets of `/proc/net/tcp` and
//! `/proc/net/tcp6`, the file descriptors under `/proc/<pid>/fd`, and each
//! holder's parent and group from `/proc/<pid>/stat`. On macOS it lists the
//! processes with `proc_listpids`, their descriptors with `proc_pidinfo`,
//! each socket with `proc_pidfdinfo`, and each holder's parent and group
//! with `proc_pidinfo`, through the process table's `darwin` module.

use crate::record::process::ProcessError;
#[cfg(any(target_os = "macos", test))]
use crate::record::process::read_i32;
#[cfg(target_os = "linux")]
use crate::record::process::{ProcRead, read_proc, stat_field};
#[cfg(target_os = "macos")]
use crate::record::process::{darwin, darwin_error};

/// The process that holds the socket that listens on a port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holder {
    /// A member of the process group.
    Group,
    /// A process in another group that descends from the group's leader,
    /// such as a daemon that started a session of its own.
    Descendant(u32),
    /// Another process, or `None` when no process whose descriptors the
    /// caller may read holds the socket.
    Other(Option<u32>),
}

/// Returns which process holds the socket that listens on the TCP port
/// `port`, as seen from the process group `group`, whose leader's id is
/// `group`. It returns `None` when this system gives no way to tell.
///
/// # Errors
///
/// Returns [`ProcessError::Table`] when the process table or the socket
/// table cannot be read, and [`ProcessError::Unsupported`] when the
/// operating system's structs differ from the ones phase reads.
pub fn listen_holder(group: u32, port: u16) -> Result<Option<Holder>, ProcessError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let mut others = Vec::new();
        for pid in listening_processes(port)? {
            match table_entry(pid)? {
                Some(entry) if entry.group == group => return Ok(Some(Holder::Group)),
                Some(_) => others.push(pid),
                None => {}
            }
        }
        for pid in &others {
            if descends_from(*pid, group)? {
                return Ok(Some(Holder::Descendant(*pid)));
            }
        }
        Ok(Some(Holder::Other(others.first().copied())))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (group, port);
        Ok(None)
    }
}

/// The parent and the process group of one process.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone, Copy, Debug)]
struct Entry {
    /// The parent's process id.
    parent: u32,
    /// The process group's id.
    group: u32,
}

/// The most parents that [`descends_from`] follows, which bounds a walk
/// through a process table that changes while it is read.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const MOST_ANCESTORS: usize = 256;

/// Reports whether `leader` is an ancestor of the process `pid`. A process
/// whose parent ended has the init process as its parent, so a descendant
/// whose parent between it and `leader` ended no longer counts.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn descends_from(pid: u32, leader: u32) -> Result<bool, ProcessError> {
    let mut current = pid;
    for _ in 0..MOST_ANCESTORS {
        let Some(entry) = table_entry(current)? else {
            return Ok(false);
        };
        if entry.parent == leader {
            return Ok(true);
        }
        if entry.parent <= 1 {
            return Ok(false);
        }
        current = entry.parent;
    }
    Ok(false)
}

/// Returns the parent and the group of `pid`, from fields 4 and 5 of
/// `/proc/<pid>/stat`, or `None` when no process has the id or its file
/// may not be read.
#[cfg(target_os = "linux")]
fn table_entry(pid: u32) -> Result<Option<Entry>, ProcessError> {
    Ok(match read_proc(&format!("/proc/{pid}/stat"))? {
        ProcRead::Text(text) => stat_field(&text, 4)
            .zip(stat_field(&text, 5))
            .map(|(parent, group)| Entry { parent, group }),
        ProcRead::Absent | ProcRead::Denied => None,
    })
}

/// Returns the parent and the group of `pid`, from `pbi_ppid` at offset 16
/// and `pbi_pgid` at offset 100 of `struct proc_bsdinfo`, or `None` when no
/// process has the id or the caller may not read it.
#[cfg(target_os = "macos")]
fn table_entry(pid: u32) -> Result<Option<Entry>, ProcessError> {
    let Ok(id) = i32::try_from(pid) else {
        return Ok(None);
    };
    match darwin::bsd_info(id) {
        Ok(info) => Ok(bsd_info_entry(&info)),
        Err(failure) if matches!(failure.errno(), Some(libc::ESRCH | libc::EPERM)) => Ok(None),
        Err(failure) => Err(darwin_error(&failure, pid)),
    }
}

/// Returns the parent and the group in `info`, the bytes of
/// `struct proc_bsdinfo`.
#[cfg(target_os = "macos")]
fn bsd_info_entry(info: &[u8]) -> Option<Entry> {
    Some(Entry {
        parent: read_i32(info, 16)?.cast_unsigned(),
        group: read_i32(info, 100)?.cast_unsigned(),
    })
}

/// Returns the processes that hold a socket that listens on the TCP port
/// `port`, among those whose descriptors the caller may read.
#[cfg(target_os = "linux")]
fn listening_processes(port: u16) -> Result<Vec<u32>, ProcessError> {
    let mut inodes = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let ProcRead::Text(text) = read_proc(table)? {
            inodes.extend(listening_inodes(&text, port));
        }
    }
    if inodes.is_empty() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir("/proc")
        .map_err(|error| ProcessError::table(format!("cannot read /proc: {error}")))?;
    let mut holders = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        // A process that ends meanwhile, or whose descriptors the caller may
        // not read, holds none of the sockets that it can see.
        let Ok(descriptors) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        let holds = descriptors.flatten().any(|descriptor| {
            std::fs::read_link(descriptor.path()).is_ok_and(|target| {
                target
                    .to_str()
                    .and_then(|target| target.strip_prefix("socket:["))
                    .and_then(|rest| rest.strip_suffix(']'))
                    .is_some_and(|inode| inodes.iter().any(|listening| listening == inode))
            })
        });
        if holds {
            holders.push(pid);
        }
    }
    Ok(holders)
}

/// Returns the processes that hold a socket that listens on the TCP port
/// `port`, among those whose descriptors the caller may read.
#[cfg(target_os = "macos")]
fn listening_processes(port: u16) -> Result<Vec<u32>, ProcessError> {
    // A process that ends meanwhile gives ESRCH, a descriptor that it
    // closes meanwhile gives EBADF, and a process of another user gives
    // EPERM: each holds none of the sockets that the caller can see.
    let skipped = |failure: &darwin::Failure| {
        matches!(
            failure.errno(),
            Some(libc::ESRCH | libc::EBADF | libc::EPERM | libc::EACCES)
        )
    };
    let processes = darwin::all_processes().map_err(|failure| darwin_error(&failure, 0))?;
    let mut holders = Vec::new();
    for pid in processes {
        let descriptors = match darwin::descriptors(pid) {
            Ok(descriptors) => descriptors,
            Err(failure) if skipped(&failure) => continue,
            Err(failure) => return Err(darwin_error(&failure, pid.cast_unsigned())),
        };
        for fd in socket_descriptors(&descriptors) {
            match darwin::socket_info(pid, fd) {
                Ok(info) if socket_listens_on(&info, port) => {
                    holders.push(pid.cast_unsigned());
                    break;
                }
                Ok(_) => {}
                Err(failure) if skipped(&failure) => {}
                Err(failure) => return Err(darwin_error(&failure, pid.cast_unsigned())),
            }
        }
    }
    Ok(holders)
}

/// Returns the inodes of the sockets in `text`, the contents of
/// `/proc/net/tcp` or `/proc/net/tcp6`, that listen on `port`. Each line
/// after the header holds the local address as `<hex address>:<hex port>`
/// in its second field, the state in its fourth, where `0A` is LISTEN, and
/// the inode in its tenth.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn listening_inodes(text: &str, port: u16) -> Vec<String> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let local_port = fields.get(1)?.rsplit_once(':')?.1;
            let listens = u16::from_str_radix(local_port, 16).ok() == Some(port)
                && fields.get(3) == Some(&"0A");
            listens.then(|| fields.get(9).map(|inode| (*inode).to_owned()))?
        })
        .collect()
}

/// Returns the descriptors in `list`, the bytes of a `struct proc_fdinfo`
/// array, whose type is `PROX_FDTYPE_SOCKET`. Each entry holds the
/// descriptor at offset 0 and its type at offset 4.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn socket_descriptors(list: &[u8]) -> Vec<i32> {
    /// `PROX_FDTYPE_SOCKET` of `<sys/proc_info.h>`.
    const PROX_FDTYPE_SOCKET: i32 = 2;
    let (entries, _) = list.as_chunks::<8>();
    entries
        .iter()
        .filter(|entry| read_i32(*entry, 4) == Some(PROX_FDTYPE_SOCKET))
        .filter_map(|entry| read_i32(entry, 0))
        .collect()
}

/// Reports whether `info`, the bytes of `struct socket_fdinfo`, is a TCP
/// socket in the LISTEN state on the local port `port`. Its `soi_kind` is
/// at offset 256, the local port of its `in_sockinfo` at offset 268 in
/// network byte order, and its `tcpsi_state` at offset 344.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn socket_listens_on(info: &[u8], port: u16) -> bool {
    /// `SOCKINFO_TCP` of `<sys/proc_info.h>`.
    const SOCKINFO_TCP: i32 = 2;
    /// `TSI_S_LISTEN` of `<sys/proc_info.h>`.
    const TSI_S_LISTEN: i32 = 1;
    let local_port = info
        .get(268..270)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u16::from_be_bytes);
    read_i32(info, 256) == Some(SOCKINFO_TCP)
        && read_i32(info, 344) == Some(TSI_S_LISTEN)
        && local_port == Some(port)
}
