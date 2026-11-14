//! The failures of `mxv archive`, `mxv restore` and `mxv remove --archived`,
//! each with a stable code and a next step (CLI-3).

use std::fmt;

use crumbkit::{Call, Code, Failure, FieldValue, Recovery};
use multiverse::{
    ArchiveRefusal, ArchiveState, FileStatus, MirrorLoss, Operation, RestoreRefusal, Unkept,
    UnkeptKind, WorkspaceName,
};

/// The code of a name that no workspace or archive has.
pub const ARCHIVE_NOT_FOUND: Code = Code::new("mxv", "archive", "not_found");
/// The code of an archive whose name another archive holds.
pub const ARCHIVE_NAME_TAKEN: Code = Code::new("mxv", "archive", "name_taken");
/// The code of a checkout that differs from what the archive admitted.
pub const ARCHIVE_SOURCE_CHANGED: Code = Code::new("mxv", "archive", "source_changed");
/// The code of a clone whose mirror is gone, so nothing could hold its pin.
pub const ARCHIVE_MIRROR_MISSING: Code = Code::new("mxv", "archive", "mirror_missing");
/// The code of a workspace with work that an archive cannot keep.
pub const ARCHIVE_UNKEEPABLE: Code = Code::new("mxv", "archive", "unkeepable");
/// The code of a workspace that a process outside phase works in.
pub const ARCHIVE_IN_USE: Code = Code::new("mxv", "archive", "in_use");
/// The code of a checkout that an archive cannot read.
pub const ARCHIVE_CHECKOUT_UNUSABLE: Code = Code::new("mxv", "archive", "checkout_unusable");
/// The code of an archive that has not finished, for a command that needs
/// a finished one.
pub const ARCHIVE_UNFINISHED: Code = Code::new("mxv", "archive", "unfinished");
/// The code of an archive that has not finished and whose pins are gone,
/// so it cannot be finished.
pub const ARCHIVE_PINS_GONE: Code = Code::new("mxv", "archive", "pins_gone");
/// The code of an archive that stopped before it retired every checkout and
/// whose workspace was removed, so it cannot be finished.
pub const ARCHIVE_WORKSPACE_REMOVED: Code = Code::new("mxv", "archive", "workspace_removed");
/// The code of an archive whose move to a dated name a repair started and
/// did not finish.
pub const ARCHIVE_RENAME_UNFINISHED: Code = Code::new("mxv", "archive", "rename_unfinished");
/// The code of `--yes` with paths that only a repository's own `mxv.toml`
/// declares disposable.
pub const ARCHIVE_MANIFEST_DISPOSABLE: Code = Code::new("mxv", "archive", "manifest_disposable");
/// The code of an archive whose record would be larger than a record may be.
pub const ARCHIVE_RECORD_TOO_LARGE: Code = Code::new("mxv", "archive", "record_too_large");
/// The code of `--delete-volumes` for volumes that a live workspace uses.
pub const ARCHIVE_VOLUMES_IN_USE: Code = Code::new("mxv", "archive", "volumes_in_use");
/// The code of a restore of a name that no archive has.
pub const RESTORE_NOT_FOUND: Code = Code::new("mxv", "restore", "not_found");
/// The code of a restore whose name or directory something else holds.
pub const RESTORE_NAME_TAKEN: Code = Code::new("mxv", "restore", "name_taken");
/// The code of a worktree's branch that moved after the archive.
pub const RESTORE_BRANCH_MOVED: Code = Code::new("mxv", "restore", "branch_moved");
/// The code of an archived commit that nothing holds any more.
pub const RESTORE_COMMIT_MISSING: Code = Code::new("mxv", "restore", "commit_missing");
/// The code of a restore of an archive that has not finished.
pub const RESTORE_UNFINISHED: Code = Code::new("mxv", "restore", "unfinished");
/// The code of a file of an archive that differs from its digest.
pub const RESTORE_ROOT_FILE_CHANGED: Code = Code::new("mxv", "restore", "root_file_changed");
/// The code of an ignored file whose copy in the archive is missing or
/// differs from its digest.
pub const RESTORE_IGNORED_DAMAGED: Code = Code::new("mxv", "restore", "ignored_damaged");
/// The code of a restore of a clone whose mirror is gone, which only
/// `--from-origin` rebuilds.
pub const RESTORE_MIRROR_MISSING: Code = Code::new("mxv", "restore", "mirror_missing");
/// The code of an ignored file whose path the rebuilt workspace already
/// holds.
pub const RESTORE_IGNORED_TAKEN: Code = Code::new("mxv", "restore", "ignored_taken");

/// Every failure code of this module.
pub(crate) const CODES: &[&Code] = &[
    &ARCHIVE_NOT_FOUND,
    &ARCHIVE_NAME_TAKEN,
    &ARCHIVE_SOURCE_CHANGED,
    &ARCHIVE_MIRROR_MISSING,
    &ARCHIVE_UNKEEPABLE,
    &ARCHIVE_IN_USE,
    &ARCHIVE_CHECKOUT_UNUSABLE,
    &ARCHIVE_UNFINISHED,
    &ARCHIVE_PINS_GONE,
    &ARCHIVE_WORKSPACE_REMOVED,
    &ARCHIVE_RENAME_UNFINISHED,
    &ARCHIVE_MANIFEST_DISPOSABLE,
    &ARCHIVE_RECORD_TOO_LARGE,
    &ARCHIVE_VOLUMES_IN_USE,
    &RESTORE_NOT_FOUND,
    &RESTORE_NAME_TAKEN,
    &RESTORE_BRANCH_MOVED,
    &RESTORE_COMMIT_MISSING,
    &RESTORE_UNFINISHED,
    &RESTORE_ROOT_FILE_CHANGED,
    &RESTORE_IGNORED_DAMAGED,
    &RESTORE_IGNORED_TAKEN,
    &RESTORE_MIRROR_MISSING,
];

/// How far the archive that holds a name got, which decides what a
/// refusal of the name names as the way out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Holder {
    /// The archive finished, so `mxv restore` or `mxv remove --archived`
    /// frees the name.
    Archived,
    /// The archive retired every checkout and did not finish, or an older
    /// mxv left it retiring, so beside a newer workspace `mxv repair`
    /// finishes it under a dated name, or moves it aside when a pin is
    /// gone.
    Retired,
    /// The archive stopped before it retired every checkout, so beside a
    /// newer workspace `mxv repair` moves it aside.
    Retiring,
}

impl Holder {
    /// Returns how far `archive` got.
    pub(crate) fn of(archive: &multiverse::Archive) -> Self {
        if archive.state == ArchiveState::Archived {
            Self::Archived
        } else if archive.counts_as_retired() {
            Self::Retired
        } else {
            Self::Retiring
        }
    }
}

/// The command that finishes an unfinished archive, which `mxv list
/// --archived` names as its repair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Finisher {
    /// `mxv archive -w NAME` finishes or resumes the archive.
    Archive,
    /// Only `mxv repair NAME` deals with the archive: it moves it aside, or
    /// finishes it under a dated name beside a newer workspace.
    Repair,
}

/// Why an archive or a restore stopped.
#[derive(Debug, Failure)]
pub(crate) enum ArchiveFailure {
    /// No workspace or archive has the name.
    #[failure(
        code = ARCHIVE_NOT_FOUND,
        recovery = Recovery::Run(Call::new(&["list"])),
        field(name = name)
    )]
    NotFound {
        /// The name.
        name: String,
    },
    /// An archive holds the name.
    #[failure(
        code = ARCHIVE_NAME_TAKEN,
        recovery = name_taken(name, *holder),
        field(name = name)
    )]
    NameTaken {
        /// The name.
        name: String,
        /// How far the archive that holds the name got.
        holder: Holder,
    },
    /// A checkout differs from what the archive admitted.
    #[failure(
        code = ARCHIVE_SOURCE_CHANGED,
        hint = "look at what changed, then run `mxv archive -w {name}` again to finish the archive",
        field(
            name = name,
            folder = folder,
            differences = FieldValue::list(differences.iter())
        )
    )]
    SourceChanged {
        /// The workspace's name.
        name: String,
        /// The checkout's folder.
        folder: String,
        /// What differs.
        differences: Vec<String>,
    },
    /// The mirrors of an archive's clones are gone, and the restore may not
    /// fetch.
    #[failure(
        code = RESTORE_MIRROR_MISSING,
        hint = "`mxv restore {name} --from-origin` fetches origin again and restores only what origin holds, and `mxv remove --archived {name}` deletes the archive",
        field(name = name, mirrors = mirrors(checkouts), lost = lost(checkouts))
    )]
    RestoreMirrorMissing {
        /// The archive's name.
        name: String,
        /// Each checkout whose mirror is gone.
        checkouts: Vec<MirrorLoss>,
    },
    /// A clone's mirror is gone.
    #[failure(
        code = ARCHIVE_MIRROR_MISSING,
        hint = "run `mxv new` for the same repository, which clones the mirror again, then archive the workspace again",
        field(folder = folder, mirror = mirror)
    )]
    MirrorMissing {
        /// The checkout's folder.
        folder: String,
        /// The mirror.
        mirror: String,
    },
    /// The workspace holds work that the archive cannot keep.
    #[failure(
        code = ARCHIVE_UNKEEPABLE,
        hint = "Everything else in the workspace would be kept. `mxv archive -w {name} --force` archives it and loses only the items above.",
        field(name = name, items = FieldValue::list(items.iter().map(item_line)))
    )]
    Unkeepable {
        /// The workspace's name.
        name: String,
        /// Each thing that the archive cannot keep.
        items: Vec<Unkept>,
    },
    /// A process outside phase works in the workspace.
    #[failure(
        code = ARCHIVE_IN_USE,
        hint = "stop that process or move it out of the workspace, then run `mxv archive -w {name}` again",
        field(name = name, reason = reason)
    )]
    InUse {
        /// The workspace's name.
        name: String,
        /// Which process, in a clause.
        reason: String,
    },
    /// A checkout cannot be read.
    #[failure(
        code = ARCHIVE_CHECKOUT_UNUSABLE,
        hint = "repair the checkout with git or `mxv add`, then archive the workspace again",
        field(folder = folder, reason = reason)
    )]
    Unusable {
        /// The checkout's folder.
        folder: String,
        /// Why, in a clause.
        reason: String,
    },
    /// The archive has not finished.
    #[failure(
        code = ARCHIVE_UNFINISHED,
        recovery = unfinished(name, *finisher),
        field(name = name)
    )]
    Unfinished {
        /// The name.
        name: String,
        /// The command that finishes the archive.
        finisher: Finisher,
    },
    /// The archive has not finished, and the pin of no checkout names its
    /// commit any more.
    #[failure(
        code = ARCHIVE_PINS_GONE,
        hint = "run `mxv repair {name}`, which moves the archive aside so that it no longer claims a restore brings the workspace back",
        field(name = name)
    )]
    PinsGone {
        /// The name.
        name: String,
    },
    /// The archive stopped before it retired every checkout, and its
    /// workspace was removed.
    #[failure(
        code = ARCHIVE_WORKSPACE_REMOVED,
        hint = "run `mxv repair {name}`, which moves the archive aside, because nothing says that it holds every checkout",
        field(name = name)
    )]
    WorkspaceRemoved {
        /// The name.
        name: String,
    },
    /// A repair started to move the archive to a dated name and did not
    /// finish, so another record holds the same archive.
    #[failure(
        code = ARCHIVE_RENAME_UNFINISHED,
        hint = "run `mxv repair {name}`, which finishes the move and then the archive",
        field(name = name, other = other)
    )]
    RenameUnfinished {
        /// The name.
        name: String,
        /// The name of the other record.
        other: String,
    },
    /// `--yes` with paths that only a repository's `mxv.toml` declares
    /// disposable.
    #[failure(
        code = ARCHIVE_MANIFEST_DISPOSABLE,
        hint = "run `mxv archive -w {name}` without --yes and answer the prompt, or pass --force",
        field(name = name, paths = FieldValue::list(paths.iter()))
    )]
    ManifestDisposable {
        /// The workspace's name.
        name: String,
        /// The paths.
        paths: Vec<String>,
    },
    /// The record would be larger than a record may be.
    #[failure(
        code = ARCHIVE_RECORD_TOO_LARGE,
        hint = "move or delete some of the extra files in the workspace's directory, then archive again",
        field(name = name, bytes = bytes.to_string(), most = most.to_string())
    )]
    RecordTooLarge {
        /// The workspace's name.
        name: String,
        /// The size that the record would have, in bytes.
        bytes: u64,
        /// The largest record, in bytes.
        most: u64,
    },
    /// A live workspace has the stable id of the archive's volumes.
    #[failure(
        code = ARCHIVE_VOLUMES_IN_USE,
        hint = "run `mxv remove --archived {name}` without --delete-volumes, because the volumes belong to the workspace {workspace}",
        field(name = name, workspace = workspace)
    )]
    VolumesInUse {
        /// The archive's name.
        name: String,
        /// The live workspace.
        workspace: String,
    },
    /// No archive has the name.
    #[failure(
        code = RESTORE_NOT_FOUND,
        recovery = Recovery::Run(Call::new(&["list"])),
        field(name = name)
    )]
    RestoreNotFound {
        /// The name.
        name: String,
    },
    /// The archive has not finished.
    #[failure(
        code = RESTORE_UNFINISHED,
        recovery = unfinished(name, *finisher),
        field(name = name)
    )]
    RestoreUnfinished {
        /// The name.
        name: String,
        /// The command that finishes the archive.
        finisher: Finisher,
    },
    /// Something holds the name or the directory.
    #[failure(
        code = RESTORE_NAME_TAKEN,
        recovery = restore_name_taken(name, path, *record),
        field(name = name, path = path)
    )]
    RestoreNameTaken {
        /// The name.
        name: String,
        /// The directory.
        path: String,
        /// Whether a workspace record holds the name.
        record: bool,
    },
    /// A worktree's branch moved.
    #[failure(
        code = RESTORE_BRANCH_MOVED,
        hint = "point the branch back with `git branch --force {branch} {pinned}`, or rename it, then restore again",
        field(
            name = name,
            folder = folder,
            branch = branch,
            pinned = pinned,
            tip = tip
        )
    )]
    BranchMoved {
        /// The archive's name.
        name: String,
        /// The checkout's folder.
        folder: String,
        /// The branch.
        branch: String,
        /// The pinned commit.
        pinned: String,
        /// The branch's commit now.
        tip: String,
    },
    /// The archived commit is gone.
    #[failure(
        code = RESTORE_COMMIT_MISSING,
        hint = "`mxv remove --archived {name}` deletes the archive, because nothing can rebuild this checkout",
        field(name = name, folder = folder, commit = commit)
    )]
    CommitMissing {
        /// The archive's name.
        name: String,
        /// The checkout's folder.
        folder: String,
        /// The commit.
        commit: String,
    },
    /// A file of the archive changed.
    #[failure(
        code = RESTORE_ROOT_FILE_CHANGED,
        hint = "put the file back, or run `mxv remove --archived {name}` to delete the archive",
        field(name = name, path = path)
    )]
    RootFileChanged {
        /// The archive's name.
        name: String,
        /// The file.
        path: String,
    },
    /// The archive's copy of an ignored file is missing or differs from
    /// its digest.
    #[failure(
        code = RESTORE_IGNORED_DAMAGED,
        hint = "`mxv restore {name} --force` restores the workspace without these files and lists them as lost",
        field(name = name, paths = FieldValue::list(paths.iter()))
    )]
    IgnoredDamaged {
        /// The archive's name.
        name: String,
        /// Each archived path, such as `app/.env`.
        paths: Vec<String>,
    },
    /// The rebuilt workspace already holds something at an ignored file's
    /// path.
    #[failure(
        code = RESTORE_IGNORED_TAKEN,
        hint = "look at what wrote these paths, then restore again",
        field(name = name, paths = FieldValue::list(paths.iter()))
    )]
    IgnoredTaken {
        /// The archive's name.
        name: String,
        /// Each archived path, such as `app/.env`.
        paths: Vec<String>,
    },
}

/// Returns the error of `refusal` for the workspace `name`: the failure of
/// a workspace that `mxv new` did not finish, which every verb that needs
/// a finished workspace shares, or else [`archive_refusal`].
pub(super) fn refused(name: &WorkspaceName, refusal: ArchiveRefusal) -> crumbkit::Error {
    match refusal {
        ArchiveRefusal::Creating => crate::failures::MxvFailure::Creating {
            name: name.to_string(),
        }
        .into(),
        refusal => archive_refusal(name, refusal).into(),
    }
}

/// Returns the failure of `refusal` for the workspace `name`.
fn archive_refusal(name: &WorkspaceName, refusal: ArchiveRefusal) -> ArchiveFailure {
    let name = name.to_string();
    match refusal {
        ArchiveRefusal::NameTaken(archive) => ArchiveFailure::NameTaken {
            name,
            holder: Holder::of(&archive),
        },
        ArchiveRefusal::PinsGone(_) => ArchiveFailure::PinsGone { name },
        ArchiveRefusal::WorkspaceRemoved(_) => ArchiveFailure::WorkspaceRemoved { name },
        ArchiveRefusal::RenameUnfinished(other) => ArchiveFailure::RenameUnfinished {
            name,
            other: other.name.to_string(),
        },
        ArchiveRefusal::MirrorMissing { folder, mirror } => ArchiveFailure::MirrorMissing {
            folder,
            mirror: mirror.display().to_string(),
        },
        ArchiveRefusal::Unusable { folder, reason } => ArchiveFailure::Unusable { folder, reason },
        ArchiveRefusal::Unkeepable(found) => ArchiveFailure::Unkeepable {
            name,
            items: found.items,
        },
        ArchiveRefusal::SourceChanged {
            folder,
            differences,
        } => ArchiveFailure::SourceChanged {
            name,
            folder,
            differences,
        },
        ArchiveRefusal::RecordTooLarge { bytes, most } => {
            ArchiveFailure::RecordTooLarge { name, bytes, most }
        }
        _ => ArchiveFailure::NotFound { name },
    }
}

/// Returns the failure of `refusal` for the archive `name`. `finisher`
/// says which command finishes an unfinished archive, and is called only
/// for one.
pub(super) fn restore_refusal(
    name: &WorkspaceName,
    refusal: RestoreRefusal,
    finisher: impl FnOnce() -> Finisher,
) -> ArchiveFailure {
    let name = name.to_string();
    match refusal {
        RestoreRefusal::MirrorMissing { checkouts } => {
            ArchiveFailure::RestoreMirrorMissing { name, checkouts }
        }
        RestoreRefusal::Unfinished(_) => ArchiveFailure::RestoreUnfinished {
            name,
            finisher: finisher(),
        },
        RestoreRefusal::NameTaken { path, record } => ArchiveFailure::RestoreNameTaken {
            name,
            path: path.display().to_string(),
            record,
        },
        RestoreRefusal::BranchMoved {
            folder,
            branch,
            pinned,
            tip,
        } => ArchiveFailure::BranchMoved {
            name,
            folder,
            branch,
            pinned,
            tip,
        },
        RestoreRefusal::CommitMissing { folder, commit } => ArchiveFailure::CommitMissing {
            name,
            folder,
            commit,
        },
        RestoreRefusal::RootFileChanged { path } => ArchiveFailure::RootFileChanged { name, path },
        RestoreRefusal::IgnoredDamaged { paths } => ArchiveFailure::IgnoredDamaged { name, paths },
        RestoreRefusal::IgnoredTaken { paths } => ArchiveFailure::IgnoredTaken { name, paths },
        _ => ArchiveFailure::RestoreNotFound { name },
    }
}

/// Returns the recovery of the name `name`, which an archive that got as
/// far as `holder` holds.
fn name_taken(name: &str, holder: Holder) -> Recovery {
    Recovery::hint(match holder {
        Holder::Retired => format!(
            "an earlier archive of {name} has not finished and a newer workspace holds the name; run `mxv repair {name}`, which finishes the earlier archive under a dated name, or moves it aside when one of its pins is gone, then archive {name} again"
        ),
        Holder::Retiring => format!(
            "an earlier archive of {name} stopped before it retired every checkout and a newer workspace holds the name; run `mxv repair {name}`, which moves the earlier archive aside, then archive {name} again"
        ),
        Holder::Archived => format!(
            "run `mxv restore {name}` to bring the archive back, or `mxv remove --archived {name}` to delete it"
        ),
    })
}

/// Returns the recovery of the unfinished archive `name`, which `finisher`
/// finishes.
fn unfinished(name: &str, finisher: Finisher) -> Recovery {
    Recovery::hint(match finisher {
        Finisher::Archive => format!("run `mxv archive -w {name}`, which finishes the archive"),
        Finisher::Repair => format!(
            "run `mxv repair {name}`, which moves the archive aside or finishes it under a dated name, as the problem that `mxv list --archived` shows for it says"
        ),
    })
}

/// Returns the recovery of a restore of `name` whose name a workspace
/// record holds, when `record` is true, or whose directory `path` exists.
fn restore_name_taken(name: &str, path: &str, record: bool) -> Recovery {
    Recovery::hint(if record {
        format!(
            "run `mxv remove {name}` first, which keeps the archive, and then restore again; `mxv remove --archived {name}` deletes the archive instead, with any files that only it still holds"
        )
    } else {
        format!("move or delete {path}, then restore again")
    })
}

/// Returns the mirror of each checkout in `checkouts`.
fn mirrors(checkouts: &[MirrorLoss]) -> FieldValue<'static> {
    FieldValue::list(
        checkouts
            .iter()
            .map(|loss| loss.mirror.display().to_string()),
    )
}

/// Returns each thing that only the lost mirror of a checkout in
/// `checkouts` held, after the checkout's folder.
fn lost(checkouts: &[MirrorLoss]) -> FieldValue<'static> {
    FieldValue::list(checkouts.iter().flat_map(|loss| {
        loss.items
            .iter()
            .map(|item| format!("{}: {item}", loss.folder))
    }))
}

impl fmt::Display for ArchiveFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { name } => write!(formatter, "no workspace or archive is named {name}"),
            Self::NameTaken { name, .. } => {
                write!(formatter, "an archive already holds the name {name}")
            }
            Self::SourceChanged {
                folder,
                differences,
                ..
            } => write!(
                formatter,
                "the checkout {folder} changed after mxv admitted it, and mxv put it back: {}",
                differences.join("; ")
            ),
            Self::MirrorMissing { folder, mirror } => write!(
                formatter,
                "the mirror {mirror} of the checkout {folder} is gone, so nothing could keep its commit"
            ),
            Self::Unkeepable { name, items } => {
                write!(
                    formatter,
                    "mxv cannot archive {name}, because it holds work an archive cannot keep:"
                )?;
                for item in items {
                    write!(formatter, "\n- {}", item_line(item))?;
                }
                Ok(())
            }
            Self::InUse { name, reason } => {
                write!(formatter, "the workspace {name} is in use: {reason}")
            }
            Self::Unusable { folder, reason } => {
                write!(
                    formatter,
                    "mxv cannot archive the checkout {folder}: {reason}"
                )
            }
            Self::Unfinished { name, .. } | Self::RestoreUnfinished { name, .. } => {
                write!(formatter, "the archive of {name} stopped partway")
            }
            Self::RenameUnfinished { name, other } => write!(
                formatter,
                "a repair stopped while it moved the archive {name}, so the record {other} holds the same archive and its files are split between the two"
            ),
            Self::PinsGone { name } => write!(
                formatter,
                "the archive of {name} stopped partway, and the pins that keep its commits are gone, so mxv cannot finish it"
            ),
            Self::WorkspaceRemoved { name } => write!(
                formatter,
                "the archive of {name} stopped before it finished, and its workspace was removed, so mxv cannot finish it"
            ),
            Self::ManifestDisposable { name, paths } => write!(
                formatter,
                "archiving {name} would delete {}, which only the repository's mxv.toml declares disposable, and --yes alone does not delete such paths",
                paths.join(", ")
            ),
            Self::RecordTooLarge { name, bytes, most } => write!(
                formatter,
                "the archive of {name} would need a record of {bytes} bytes, and a record holds at most {most} bytes"
            ),
            Self::VolumesInUse { name, workspace } => write!(
                formatter,
                "the docker volumes of the archive {name} belong to the live workspace {workspace}"
            ),
            Self::RestoreNotFound { name } => write!(formatter, "no archive is named {name}"),
            Self::RestoreNameTaken { name, path, record } => {
                if *record {
                    write!(formatter, "a workspace already holds the name {name}")
                } else {
                    write!(formatter, "the directory {path} already exists")
                }
            }
            Self::BranchMoved {
                folder,
                branch,
                pinned,
                tip,
                ..
            } => write!(
                formatter,
                "the branch {branch} of {folder} points at {tip}, and the archive pinned {pinned}"
            ),
            Self::CommitMissing { folder, commit, .. } => write!(
                formatter,
                "the commit {commit} of {folder} is gone from every repository that mxv can read"
            ),
            Self::RootFileChanged { path, .. } => write!(
                formatter,
                "the archive's copy of {path} differs from its recorded digest"
            ),
            Self::IgnoredDamaged { paths, .. } => write!(
                formatter,
                "the archive's copy of {} is missing or differs from its recorded digest",
                paths.join(", ")
            ),
            Self::IgnoredTaken { paths, .. } => write!(
                formatter,
                "the rebuilt workspace already holds {}",
                paths.join(", ")
            ),
            Self::RestoreMirrorMissing { name, checkouts } => {
                write!(
                    formatter,
                    "mxv cannot restore {name}, because the mirror that held its pins is gone:"
                )?;
                for loss in checkouts {
                    write!(formatter, "\n- {}: {}", loss.folder, loss.mirror.display())?;
                }
                write!(
                    formatter,
                    "\nRestoring from origin would lose what only the mirror held, and it restores a commit only when origin holds it"
                )?;
                for loss in checkouts {
                    for item in &loss.items {
                        write!(formatter, "\n- {}: {item}", loss.folder)?;
                    }
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ArchiveFailure {}

/// Returns `bytes` in the largest binary unit that it reaches, such as
/// `412 MiB`, rounded down.
pub(crate) fn size(bytes: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")];
    UNITS.iter().find(|(unit, _)| bytes >= *unit).map_or_else(
        || format!("{bytes} bytes"),
        |(unit, name)| format!("{} {name}", bytes / unit),
    )
}

/// Returns the line of the refusal that names `item`, with what the
/// person can do about it.
fn item_line(item: &Unkept) -> String {
    let path = item.dir.display();
    let folder = &item.folder;
    match &item.kind {
        UnkeptKind::Paused { at, operation } => {
            let dir = if at.is_empty() {
                path.to_string()
            } else {
                item.dir
                    .join(at.trim_end_matches('/'))
                    .display()
                    .to_string()
            };
            let remedy = match operation {
                Operation::Merge => format!(
                    "Finish it with `git -C {dir} commit`, or abort it with `git -C {dir} merge --abort`."
                ),
                Operation::Rebase => format!(
                    "Finish it with `git -C {dir} rebase --continue`, or abort it with `git -C {dir} rebase --abort`."
                ),
                Operation::CherryPick | Operation::Sequence => format!(
                    "Finish it with `git -C {dir} cherry-pick --continue`, or abort it with `git -C {dir} cherry-pick --abort`."
                ),
                Operation::Revert => format!(
                    "Finish it with `git -C {dir} revert --continue`, or abort it with `git -C {dir} revert --abort`."
                ),
                Operation::Bisect => format!("End it with `git -C {dir} bisect reset`."),
                _ => format!("Finish or abort it in {dir}."),
            };
            format!("{item}. {remedy}")
        }
        UnkeptKind::TooLarge {
            path: file,
            bytes,
            most,
            status,
        } => {
            let what = match status {
                FileStatus::Untracked => "the untracked file",
                FileStatus::Ignored => "the ignored file",
                _ => "the file",
            };
            format!(
                "{folder}: {what} {file} is {}, over archive_max_file ({}). Move it out of the workspace or delete it.",
                size(*bytes),
                size(*most)
            )
        }
        UnkeptKind::Nested { .. } => format!(
            "{item}. Push it to a remote of its own and move it out of the workspace, or add it as a submodule."
        ),
        UnkeptKind::Submodule { .. } => {
            format!("{item}. Commit and push that work in the submodule, or discard it.")
        }
        UnkeptKind::Hidden {
            path: file,
            skip_worktree,
        } => {
            let flag = if *skip_worktree {
                "--no-skip-worktree"
            } else {
                "--no-assume-unchanged"
            };
            format!(
                "{item}. Clear the mark with `git -C {path} update-index {flag} {file}`, then archive again."
            )
        }
        UnkeptKind::Ignored { .. } | UnkeptKind::Unlisted { .. } => {
            format!("{item}. Move it out of the workspace or delete it.")
        }
        UnkeptKind::Root { entry } => format!(
            "root: {entry} is an entry that mxv cannot copy, such as a socket. Delete it, or stop what made it."
        ),
        _ => item.to_string(),
    }
}
