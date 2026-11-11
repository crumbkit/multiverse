//! Putting a workspace away and rebuilding it: the archive records that
//! `mxv archive` writes, and the operations that write and read them.
//!
//! An archive lives in `<archives>/<name>`, in the state directory, private
//! to the person. `archive.json` holds its [`Archive`] record, `manifest`
//! the bytes of the workspace manifest when the workspace has one, and
//! `root/` the extra files of the workspace's directory. The ignored files
//! of its checkouts that lie outside the disposable set, such as `.env`,
//! are kept in one tar, `ignored.tar`, in the data directory under the
//! archive's id, which the record lists as [`IgnoredEntry`]. Each checkout's
//! git state stays reachable through pins, references below
//! `refs/mxv/archives/<archive id>/<folder>/` in the worktree's repository
//! or the clone's mirror: `head` keeps the commit of `HEAD`, and the others
//! keep the uncommitted work, the stashes, the other branches, the tags
//! that `origin` lacks, the per-worktree references and the commits that
//! no reference reaches, as [`Kept`] lists them. Nothing else reads the
//! pins, and mxv never pushes them.
//!
//! [`Workspaces::plan_archive`] classifies the workspace as `mxv remove`
//! classifies it, with [`Workspaces::plan_removal`], splits what it holds
//! into what an archive keeps and what it cannot represent, and refuses
//! for the second. [`Workspaces::archive`] then writes the record, commits
//! the uncommitted work through a temporary index, writes the pins, copies
//! the root files, and retires each checkout. Retiring moves the checkout
//! aside by one rename, reads its git state, its uncommitted work and its
//! path list again in the new place, and puts it back when anything
//! differs from what the archive kept. The stages are idempotent, so the
//! same command finishes an archive that stopped partway.
//!
//! Restoring lives in [`restore`].

use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::mirror::directory;

mod admit;
mod ignored;
mod record;
mod restore;
mod retire;
mod root;
#[cfg(test)]
mod tests;

pub use admit::{
    ArchiveOptions, ArchivePlan, ArchivePlanned, ArchiveRefusal, Uncommitted, Unkeepable, Unkept,
    UnkeptKind,
};
pub use ignored::{IgnoredEntry, IgnoredKind, IgnoredSource};
pub use record::{
    Archive, ArchiveRecords, ArchiveState, ArchivedCheckout, Archives, Kept, KeptStash, KeptWip,
};
pub use restore::{
    ArchivedRemoval, PortChange, PutBack, RestoreOutcome, RestorePlan, RestorePlanned,
    RestoreRefusal, Restored,
};
pub use retire::ArchiveOutcome;
pub use root::{RootEntry, RootKind};

/// The highest version of the archive record that this version of
/// multiverse reads. A record with a higher version is refused (DUR-3).
/// Version 2 adds [`ArchivedCheckout::kept`] and names the pin of `HEAD`
/// `<dir>/head`. A record of version 1 is still read, with nothing kept.
pub const ARCHIVE_VERSION: u32 = 2;

/// The message of the commits that keep a checkout's uncommitted work.
pub const WIP_MESSAGE: &str = "wip: archived by mxv";

/// The largest file of a checkout's uncommitted work, and the largest
/// ignored file, that an archive keeps unless the person sets another
/// limit, in bytes: 100 MiB. A larger file refuses the archive.
pub const ARCHIVE_MAX_FILE: u64 = 100 * 1024 * 1024;

/// The name of an archive's record in its directory.
const RECORD_FILE: &str = "archive.json";

/// The name of the temporary index file in an archive's directory, through
/// which the archive reads and commits a checkout's uncommitted work.
const WIP_INDEX: &str = "wip.index";

/// The number of hex digits of a SHA-256 digest.
const DIGEST_LENGTH: usize = 64;

/// The name of the copy of the workspace manifest in an archive's directory.
const MANIFEST_COPY: &str = "manifest";

/// The name of the directory that holds an archive's root files.
const ROOT_DIR: &str = "root";

/// The suffix of the name under which `mxv archive` moves a checkout while
/// it compares the checkout with what it admitted, such as
/// `.app.mxv-archiving` beside `app`. A checkout that passes moves on to
/// the name that [`aside`] returns, so what stands under this name is whole.
const QUARANTINE_SUFFIX: &str = ".mxv-archiving";

/// The start of every pin's reference.
const PIN_NAMESPACE: &str = "refs/mxv/archives/";

/// The end of the pin of a checkout's `HEAD` in a record of version 2.
const HEAD_PIN: &str = "/head";

/// The start of the reference that a restore gives each kept commit that
/// no other reference reaches, such as `refs/mxv/kept/<commit>`.
pub const KEPT_NAMESPACE: &str = "refs/mxv/kept/";

/// The checkpoint of a run outside the tests, which does nothing. A test
/// puts its own function in [`Workspaces`] to stop or change a run after the
/// stage that `_step` names.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the type of the checkpoint is shared with the test's function, which can fail"
)]
pub(crate) const fn no_checkpoint(_step: &str) -> Result<(), Error> {
    Ok(())
}

/// Returns the name of the pin of the `HEAD` of the checkout in `folder`
/// of the archive `id`. The folder is encoded as one reference component,
/// because a repository's name can hold bytes that a reference may not.
fn pin_ref(id: &str, folder: &str) -> String {
    format!(
        "{PIN_NAMESPACE}{id}/{}{HEAD_PIN}",
        directory(folder).replace("..", ".%2E")
    )
}

/// Returns the path beside `checkout` under which `archive` holds it while
/// it compares it with what it admitted, or `None` for a path without a
/// name.
fn quarantine(checkout: &Path) -> Option<PathBuf> {
    let name = checkout.file_name()?.to_str()?;
    Some(checkout.with_file_name(format!(".{name}{QUARANTINE_SUFFIX}")))
}
