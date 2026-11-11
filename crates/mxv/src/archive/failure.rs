//! The failures of `mxv archive`, `mxv restore` and `mxv remove --archived`,
//! each with a stable code and a next step (CLI-3).

use std::fmt;

use crumbkit::{Call, Code, Failure, Field, FieldValue, Recovery};
use multiverse::{
    ArchiveRefusal, ArchiveState, Operation, RestoreRefusal, Unkept, UnkeptKind, WorkspaceName,
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
];

/// Why an archive or a restore stopped.
#[derive(Debug)]
pub(crate) enum ArchiveFailure {
    /// No workspace or archive has the name.
    NotFound {
        /// The name.
        name: String,
    },
    /// An archive holds the name.
    NameTaken {
        /// The name.
        name: String,
        /// Whether the archive that holds the name has not finished.
        retiring: bool,
    },
    /// A checkout differs from what the archive admitted.
    SourceChanged {
        /// The workspace's name.
        name: String,
        /// The checkout's folder.
        folder: String,
        /// What differs.
        differences: Vec<String>,
    },
    /// A clone's mirror is gone.
    MirrorMissing {
        /// The checkout's folder.
        folder: String,
        /// The mirror.
        mirror: String,
    },
    /// The workspace holds work that the archive cannot keep.
    Unkeepable {
        /// The workspace's name.
        name: String,
        /// Each thing that the archive cannot keep.
        items: Vec<Unkept>,
    },
    /// A process outside phase works in the workspace.
    InUse {
        /// The workspace's name.
        name: String,
        /// Which process, in a clause.
        reason: String,
    },
    /// A checkout cannot be read.
    Unusable {
        /// The checkout's folder.
        folder: String,
        /// Why, in a clause.
        reason: String,
    },
    /// The archive has not finished.
    Unfinished {
        /// The name.
        name: String,
    },
    /// `--yes` with paths that only a repository's `mxv.toml` declares
    /// disposable.
    ManifestDisposable {
        /// The workspace's name.
        name: String,
        /// The paths.
        paths: Vec<String>,
    },
    /// The record would be larger than a record may be.
    RecordTooLarge {
        /// The workspace's name.
        name: String,
        /// The size that the record would have, in bytes.
        bytes: u64,
        /// The largest record, in bytes.
        most: u64,
    },
    /// A live workspace has the stable id of the archive's volumes.
    VolumesInUse {
        /// The archive's name.
        name: String,
        /// The live workspace.
        workspace: String,
    },
    /// No archive has the name.
    RestoreNotFound {
        /// The name.
        name: String,
    },
    /// The archive has not finished.
    RestoreUnfinished {
        /// The name.
        name: String,
    },
    /// Something holds the name or the directory.
    RestoreNameTaken {
        /// The name.
        name: String,
        /// The directory.
        path: String,
        /// Whether a workspace record holds the name.
        record: bool,
    },
    /// A worktree's branch moved.
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
    CommitMissing {
        /// The archive's name.
        name: String,
        /// The checkout's folder.
        folder: String,
        /// The commit.
        commit: String,
    },
    /// A file of the archive changed.
    RootFileChanged {
        /// The archive's name.
        name: String,
        /// The file.
        path: String,
    },
    /// The archive's copy of an ignored file is missing or differs from
    /// its digest.
    IgnoredDamaged {
        /// The archive's name.
        name: String,
        /// Each archived path, such as `app/.env`.
        paths: Vec<String>,
    },
    /// The rebuilt workspace already holds something at an ignored file's
    /// path.
    IgnoredTaken {
        /// The archive's name.
        name: String,
        /// Each archived path, such as `app/.env`.
        paths: Vec<String>,
    },
}

/// Returns the failure of `refusal` for the workspace `name`.
pub(super) fn archive_refusal(name: &WorkspaceName, refusal: ArchiveRefusal) -> ArchiveFailure {
    let name = name.to_string();
    match refusal {
        ArchiveRefusal::NameTaken(archive) => ArchiveFailure::NameTaken {
            name,
            retiring: archive.state == ArchiveState::Retiring,
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

/// Returns the failure of `refusal` for the archive `name`.
pub(super) fn restore_refusal(name: &WorkspaceName, refusal: RestoreRefusal) -> ArchiveFailure {
    let name = name.to_string();
    match refusal {
        RestoreRefusal::Unfinished(_) => ArchiveFailure::RestoreUnfinished { name },
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

impl Failure for ArchiveFailure {
    fn code(&self) -> &'static Code {
        match self {
            Self::NotFound { .. } => &ARCHIVE_NOT_FOUND,
            Self::NameTaken { .. } => &ARCHIVE_NAME_TAKEN,
            Self::SourceChanged { .. } => &ARCHIVE_SOURCE_CHANGED,
            Self::MirrorMissing { .. } => &ARCHIVE_MIRROR_MISSING,
            Self::Unkeepable { .. } => &ARCHIVE_UNKEEPABLE,
            Self::InUse { .. } => &ARCHIVE_IN_USE,
            Self::Unusable { .. } => &ARCHIVE_CHECKOUT_UNUSABLE,
            Self::Unfinished { .. } => &ARCHIVE_UNFINISHED,
            Self::ManifestDisposable { .. } => &ARCHIVE_MANIFEST_DISPOSABLE,
            Self::RecordTooLarge { .. } => &ARCHIVE_RECORD_TOO_LARGE,
            Self::VolumesInUse { .. } => &ARCHIVE_VOLUMES_IN_USE,
            Self::RestoreNotFound { .. } => &RESTORE_NOT_FOUND,
            Self::RestoreUnfinished { .. } => &RESTORE_UNFINISHED,
            Self::RestoreNameTaken { .. } => &RESTORE_NAME_TAKEN,
            Self::BranchMoved { .. } => &RESTORE_BRANCH_MOVED,
            Self::CommitMissing { .. } => &RESTORE_COMMIT_MISSING,
            Self::RootFileChanged { .. } => &RESTORE_ROOT_FILE_CHANGED,
            Self::IgnoredDamaged { .. } => &RESTORE_IGNORED_DAMAGED,
            Self::IgnoredTaken { .. } => &RESTORE_IGNORED_TAKEN,
        }
    }

    fn recovery(&self) -> Recovery {
        match self {
            Self::NotFound { .. } | Self::RestoreNotFound { .. } => {
                Recovery::Run(Call::new(&["list"]))
            }
            Self::NameTaken {
                name,
                retiring: true,
            } => Recovery::hint(format!(
                "an earlier archive of {name} has not finished and a newer workspace holds the name; run `mxv remove {name}` to remove the newer workspace, then `mxv archive -w {name}` finishes the earlier archive"
            )),
            Self::NameTaken { name, .. } => Recovery::hint(format!(
                "run `mxv restore {name}` to bring the archive back, or `mxv remove --archived {name}` to delete it"
            )),
            Self::SourceChanged { name, .. } => Recovery::hint(format!(
                "look at what changed, then run `mxv archive -w {name}` again to finish the archive"
            )),
            Self::MirrorMissing { .. } => Recovery::hint(
                "run `mxv new` for the same repository, which clones the mirror again, then archive the workspace again",
            ),
            Self::Unkeepable { name, .. } => Recovery::hint(format!(
                "Everything else in the workspace would be kept. `mxv archive -w {name} --force` archives it and loses only the items above."
            )),
            Self::InUse { name, .. } => Recovery::hint(format!(
                "stop that process or move it out of the workspace, then run `mxv archive -w {name}` again"
            )),
            Self::Unusable { .. } => Recovery::hint(
                "repair the checkout with git or `mxv add`, then archive the workspace again",
            ),
            Self::Unfinished { name } | Self::RestoreUnfinished { name } => Recovery::hint(
                format!("run `mxv archive -w {name}`, which finishes the archive"),
            ),
            Self::ManifestDisposable { name, .. } => Recovery::hint(format!(
                "run `mxv archive -w {name}` without --yes and answer the prompt, or pass --force"
            )),
            Self::RecordTooLarge { .. } => Recovery::hint(
                "move or delete some of the extra files in the workspace's directory, then archive again",
            ),
            Self::VolumesInUse { name, workspace } => Recovery::hint(format!(
                "run `mxv remove --archived {name}` without --delete-volumes, because the volumes belong to the workspace {workspace}"
            )),
            Self::RestoreNameTaken { name, path, record } => {
                if *record {
                    Recovery::hint(format!(
                        "remove the workspace {name} first, or run `mxv remove --archived {name}` to delete the archive"
                    ))
                } else {
                    Recovery::hint(format!("move or delete {path}, then restore again"))
                }
            }
            Self::BranchMoved { branch, pinned, .. } => Recovery::hint(format!(
                "point the branch back with `git branch --force {branch} {pinned}`, or rename it, then restore again"
            )),
            Self::CommitMissing { name, .. } => Recovery::hint(format!(
                "`mxv remove --archived {name}` deletes the archive, because nothing can rebuild this checkout"
            )),
            Self::RootFileChanged { name, .. } => Recovery::hint(format!(
                "put the file back, or run `mxv remove --archived {name}` to delete the archive"
            )),
            Self::IgnoredDamaged { name, .. } => Recovery::hint(format!(
                "`mxv restore {name} --force` restores the workspace without these files and lists them as lost"
            )),
            Self::IgnoredTaken { .. } => {
                Recovery::hint("look at what wrote these paths, then restore again")
            }
        }
    }

    fn fields(&self) -> Vec<Field<'_>> {
        match self {
            Self::NotFound { name }
            | Self::NameTaken { name, .. }
            | Self::Unfinished { name }
            | Self::RestoreNotFound { name }
            | Self::RestoreUnfinished { name } => vec![Field::new("name", name)],
            Self::SourceChanged {
                name,
                folder,
                differences,
            } => vec![
                Field::new("name", name),
                Field::new("folder", folder),
                Field::new("differences", FieldValue::list(differences.iter())),
            ],
            Self::MirrorMissing { folder, mirror } => {
                vec![Field::new("folder", folder), Field::new("mirror", mirror)]
            }
            Self::Unkeepable { name, items } => vec![
                Field::new("name", name),
                Field::new("items", FieldValue::list(items.iter().map(item_line))),
            ],
            Self::InUse { name, reason } => {
                vec![Field::new("name", name), Field::new("reason", reason)]
            }
            Self::Unusable { folder, reason } => {
                vec![Field::new("folder", folder), Field::new("reason", reason)]
            }
            Self::ManifestDisposable { name, paths }
            | Self::IgnoredDamaged { name, paths }
            | Self::IgnoredTaken { name, paths } => vec![
                Field::new("name", name),
                Field::new("paths", FieldValue::list(paths.iter())),
            ],
            Self::RecordTooLarge { name, bytes, most } => vec![
                Field::new("name", name),
                Field::new("bytes", bytes.to_string()),
                Field::new("most", most.to_string()),
            ],
            Self::VolumesInUse { name, workspace } => {
                vec![Field::new("name", name), Field::new("workspace", workspace)]
            }
            Self::RestoreNameTaken { name, path, .. } => {
                vec![Field::new("name", name), Field::new("path", path)]
            }
            Self::BranchMoved {
                name,
                folder,
                branch,
                pinned,
                tip,
            } => vec![
                Field::new("name", name),
                Field::new("folder", folder),
                Field::new("branch", branch),
                Field::new("pinned", pinned),
                Field::new("tip", tip),
            ],
            Self::CommitMissing {
                name,
                folder,
                commit,
            } => vec![
                Field::new("name", name),
                Field::new("folder", folder),
                Field::new("commit", commit),
            ],
            Self::RootFileChanged { name, path } => {
                vec![Field::new("name", name), Field::new("path", path)]
            }
        }
    }
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
            Self::Unfinished { name } | Self::RestoreUnfinished { name } => {
                write!(formatter, "the archive of {name} stopped partway")
            }
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
            untracked,
        } => {
            let what = if *untracked {
                "the untracked file"
            } else {
                "the file"
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
