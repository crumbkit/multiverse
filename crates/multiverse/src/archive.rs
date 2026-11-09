//! Putting a workspace away and rebuilding it: the archive records that
//! `mxv archive` writes, and the operations that write and read them.
//!
//! An archive lives in `<archives>/<name>`, in the state directory, private
//! to the person. `archive.json` holds its [`Archive`] record, `manifest`
//! the bytes of the workspace manifest when the workspace has one, and
//! `root/` the extra files of the workspace's directory. Each checkout's
//! commit stays reachable through a pin, a reference
//! `refs/mxv/archives/<archive id>/<folder>` in the worktree's repository or
//! the clone's mirror. Nothing else reads the pins, and mxv never pushes
//! them.
//!
//! [`Workspaces::plan_archive`] classifies the workspace as `mxv remove`
//! classifies it, with [`Workspaces::plan_removal`], and names everything
//! that an archive could not keep. [`Workspaces::archive`] then writes the
//! record, pins each `HEAD`, copies the root files, and retires each
//! checkout. Retiring moves the checkout aside by one rename, reads its git
//! state and path list again in the new place, and puts it back when
//! anything differs from what the archive admitted. The stages are
//! idempotent, so the same command finishes an archive that stopped partway.
//!
//! Restoring lives in [`restore`].

use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::mirror::directory;

mod admit;
mod record;
mod restore;
mod retire;
mod root;
#[cfg(test)]
mod tests;

pub use admit::{ArchiveOptions, ArchivePlan, ArchivePlanned, ArchiveRefusal, Unsaved};
pub use record::{Archive, ArchiveRecords, ArchiveState, ArchivedCheckout, Archives};
pub use restore::{
    ArchivedRemoval, PortChange, RestoreOutcome, RestorePlan, RestorePlanned, RestoreRefusal,
    Restored,
};
pub use retire::ArchiveOutcome;
pub use root::{RootEntry, RootKind};

/// The highest version of the archive record that this version of
/// multiverse reads. A record with a higher version is refused (DUR-3).
pub const ARCHIVE_VERSION: u32 = 1;

/// The message of the commit that `mxv archive --wip` makes.
pub const WIP_MESSAGE: &str = "wip: archived by mxv";

/// The name of an archive's record in its directory.
const RECORD_FILE: &str = "archive.json";

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

/// Returns the name of the pin of the checkout in `folder` of the archive
/// `id`. The folder is encoded as one reference component, because a
/// repository's name can hold bytes that a reference may not.
fn pin_ref(id: &str, folder: &str) -> String {
    format!(
        "{PIN_NAMESPACE}{id}/{}",
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
