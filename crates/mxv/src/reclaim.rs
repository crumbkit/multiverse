//! The verb `reclaim`, which stops idle workspaces to free memory or
//! cleans their build output to free disk.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use multiverse::phase::host::TableEntry;

#[cfg(test)]
mod tests;

/// What a reclaim must free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Need {
    /// Bytes of resident memory.
    Memory(u64),
    /// Bytes on disk.
    Disk(u64),
}

/// Returns the need that `text` names.
pub(crate) fn parse_need(text: &str) -> Result<Need, String> {
    let _ = text;
    Err("memory=<SIZE>".to_owned())
}

/// The facts about one running workspace that decide whether it is idle.
#[derive(Debug)]
pub(crate) struct Facts {
    /// Whether `mxv pin` pinned the workspace.
    pub(crate) pinned: bool,
    /// When a file of its checkouts, a commit or a staged change last
    /// changed.
    pub(crate) modified: Option<SystemTime>,
    /// When an mxv command last named the workspace.
    pub(crate) touched: Option<SystemTime>,
    /// Each process outside phase whose working directory is inside the
    /// workspace, with that directory.
    pub(crate) users: Vec<(u32, PathBuf)>,
}

/// Whether a running workspace is idle.
#[derive(Debug)]
pub(crate) enum Verdict {
    /// The workspace is idle.
    Idle {
        /// When it was last active.
        active: Option<SystemTime>,
        /// Why it counts as idle.
        why: String,
    },
    /// The workspace is in use, for this reason.
    InUse(String),
}

/// Returns whether the workspace that `facts` describes is idle at `now`
/// for the idle age `stop_idle`.
pub(crate) fn idle(facts: &Facts, now: SystemTime, stop_idle: Duration) -> Verdict {
    let _ = (facts, now, stop_idle);
    Verdict::InUse(String::new())
}

/// Returns the indexes of `idle` to stop.
pub(crate) fn choose(idle: &[(Option<SystemTime>, u64)], need: u64) -> Vec<usize> {
    let _ = (idle, need);
    Vec::new()
}

/// Returns the entries of `table` that phase owns.
pub(crate) fn owned<'t>(
    table: &'t [TableEntry],
    apps: &[u32],
) -> impl Iterator<Item = &'t TableEntry> {
    let _ = apps;
    table.iter().take(0)
}

/// Returns the users of `dir`.
pub(crate) fn users(
    table: &[TableEntry],
    dir: &Path,
    apps: &[u32],
    own: u32,
) -> Vec<(u32, PathBuf)> {
    let _ = (table, dir, apps, own);
    Vec::new()
}
