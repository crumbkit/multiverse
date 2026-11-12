//! The verb `repair`, which repairs the broken records of one name that
//! `mxv list` names `mxv repair NAME` for, or with `--orphans` deletes what
//! no record names, and the document it prints.
//!
//! The library's [`Workspaces::repair`](multiverse::Workspaces::repair)
//! decides what each broken record needs and carries out the move, the
//! rename or the deletion.
//! [`Workspaces::orphans`](multiverse::Workspaces::orphans) finds what no
//! record names, and
//! [`Workspaces::sweep_orphan`](multiverse::Workspaces::sweep_orphan)
//! deletes one item. This module holds the workspace's lock and finishes a
//! renamed archive as `mxv archive -w` finishes one.

use std::fmt;

use clap::Args;
use crumbkit::{Document, DryRun, Prose, Run, Yes};
use multiverse::health::{Orphan, OrphanKind, Repaired, Swept, UNRECORDED};
use multiverse::{Mode, WorkspaceName};
use schemars::JsonSchema;
use serde::Serialize;

use crate::archive::{archives, finish};
use crate::environment::Environment;
use crate::failures::MxvFailure;
use crate::processes;
use crate::rows::repair_text;
use crate::units::Size;
use crate::verbs::{configured, parse_name};

/// The document that `mxv repair` prints.
pub(crate) const REPAIR: Document<Prose<RepairResult>> = Document::new(
    "mxv",
    "repair",
    1,
    "What the repair of one name's broken records, or the sweep of what no record names, did, or would do on a dry run.",
);

/// The arguments of `mxv repair`.
#[derive(Debug, Args)]
pub(crate) struct RepairArgs {
    /// The name of the workspace or archive whose records to repair
    #[arg(required_unless_present = "orphans", conflicts_with = "orphans")]
    pub(crate) name: Option<String>,

    /// List what no record names and delete its run files and run records. phase's sweep first stops the processes and removes the containers, networks and run volumes that a run record names.
    #[arg(long)]
    pub(crate) orphans: bool,

    /// With --orphans, delete the archives that a repair moved aside and the ignored files that no archive names, which hold the only copies of their files.
    #[arg(long, requires = "orphans")]
    pub(crate) delete_aside: bool,

    /// With --orphans, also delete the docker volumes that hold the data of each workspace id that no record holds.
    #[arg(long, requires = "orphans")]
    pub(crate) delete_volumes: bool,

    /// Print what the command would do, and change nothing.
    #[command(flatten)]
    pub(crate) dry_run: DryRun,

    /// Repair the records, or delete what --orphans lists, without asking.
    #[command(flatten)]
    pub(crate) yes: Yes,
}

/// What the repair did with the records of the name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RepairState {
    /// Nothing of the name is broken, or what is broken is another
    /// command's to repair, which `repair` names. With `--orphans`, the
    /// sweep deleted nothing.
    Unchanged,
    /// The directory of an archive whose pins are gone moved aside.
    MovedAside,
    /// A dry run: the directory would move aside.
    WouldMoveAside,
    /// An older archive beside a newer workspace was finished under the
    /// dated name in `archive`.
    Finished,
    /// A dry run: the older archive would be finished under that name.
    WouldFinish,
    /// The archive that a restore had already rebuilt was deleted, with its
    /// pins.
    Deleted,
    /// A dry run: the archive would be deleted.
    WouldDelete,
    /// With `--orphans`, the sweep deleted at least one item.
    Swept,
    /// With `--orphans`, a dry run: the sweep would delete at least one
    /// item.
    WouldSweep,
}

/// What `mxv repair` did, or would do on a dry run.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct RepairResult {
    /// The name whose records the command repaired, or null with
    /// `--orphans`.
    name: Option<String>,
    /// What the repair did, or would do.
    state: RepairState,
    /// The name that the older archive had, such as `docs`, or null when
    /// no archive was renamed. It differs from `name` when `name` is the
    /// dated name of a rename that an earlier repair started.
    older: Option<String>,
    /// The dated name that the older archive takes, such as
    /// `docs-2026-11-02`, or null when no archive was renamed.
    archive: Option<String>,
    /// The paths that hold what moved aside: the archive's directory, and
    /// its private tar of ignored files when it has one. Empty when nothing
    /// moved aside.
    kept: Vec<String>,
    /// The command that repairs what is broken when `mxv repair` does not,
    /// such as `mxv archive -w docs`, or null.
    repair: Option<String>,
    /// The references of the pins that the deletion deleted, or would
    /// delete.
    pins: Vec<String>,
    /// With `--orphans`, one row for each item that no record names. Empty
    /// without `--orphans`.
    orphans: Vec<OrphanRow>,
    /// What the repair could not finish, one sentence for each, such as a
    /// pin that it could not delete.
    warnings: Vec<String>,
}

impl RepairResult {
    /// Returns the result for `name` in `state`, with no other field set.
    fn new(name: &WorkspaceName, state: RepairState) -> Self {
        Self {
            name: Some(name.to_string()),
            state,
            ..Self::sweep()
        }
    }

    /// Returns the result of `--orphans` that deleted nothing, with no
    /// row.
    const fn sweep() -> Self {
        Self {
            name: None,
            state: RepairState::Unchanged,
            older: None,
            archive: None,
            kept: Vec::new(),
            repair: None,
            pins: Vec::new(),
            orphans: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

/// What one item that no record names is, in an [`OrphanRow`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OrphanRowKind {
    /// The run files of a name that no workspace has.
    RunFiles,
    /// phase's run record and logs of a workspace id that no record holds.
    RunRecord,
    /// The docker data volumes of a workspace id that no record holds and
    /// phase keeps nothing of, which only --delete-volumes lists.
    Volumes,
    /// An archive that `mxv repair NAME` moved aside, with its ignored
    /// files.
    MovedAside,
    /// A directory of ignored files whose archive no record names.
    IgnoredFiles,
    /// A directory of the archives without its own record, which mxv never
    /// deletes.
    Unrecorded,
}

/// What the sweep did with one item, in an [`OrphanRow`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OrphanState {
    /// The sweep deleted it.
    Deleted,
    /// A dry run: the sweep would delete it.
    WouldDelete,
    /// It stays, for the reasons that the row gives.
    Kept,
    /// The sweep began to delete it and failed, so part of it may be gone.
    /// `deleted` names the paths that are gone, and `reasons` the failure.
    PartlyDeleted,
}

/// One item that no record names, as `mxv repair --orphans` prints it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct OrphanRow {
    /// What the item is.
    kind: OrphanRowKind,
    /// The directory, the first of a run record's paths, or the first
    /// volume's name.
    path: String,
    /// The workspace name of run files, or null.
    name: Option<String>,
    /// The workspace id of a run record or of volumes, or null.
    id: Option<String>,
    /// What the sweep did with it, or would do on a dry run.
    state: OrphanState,
    /// The bytes that deleting it frees.
    size: Size,
    /// Why it stays, or why its deletion stopped, one sentence for each
    /// reason, such as that only --delete-aside deletes it. Empty when it
    /// is deleted.
    reasons: Vec<String>,
    /// The item's paths that a deletion that stopped deleted. Empty unless
    /// the state is `partly_deleted`.
    deleted: Vec<String>,
    /// The docker volumes of a `volumes` item, or those that
    /// `--delete-volumes` deleted with a run record.
    volumes: Vec<String>,
}

impl OrphanRow {
    /// Returns the row of `orphan` in `state`, kept for `reasons`.
    fn new(orphan: &Orphan, state: OrphanState, reasons: Vec<String>) -> Self {
        let mut volumes = Vec::new();
        let (kind, name, id) = match &orphan.kind {
            OrphanKind::RunFiles { name } => {
                (OrphanRowKind::RunFiles, Some(name.to_string()), None)
            }
            OrphanKind::RunRecord { id, .. } => {
                (OrphanRowKind::RunRecord, None, Some(id.as_str().to_owned()))
            }
            OrphanKind::Volumes { id, names } => {
                volumes.clone_from(names);
                (OrphanRowKind::Volumes, None, Some(id.as_str().to_owned()))
            }
            OrphanKind::MovedAside { .. } => (OrphanRowKind::MovedAside, None, None),
            OrphanKind::IgnoredFiles => (OrphanRowKind::IgnoredFiles, None, None),
            OrphanKind::Unrecorded => (OrphanRowKind::Unrecorded, None, None),
        };
        Self {
            kind,
            path: orphan.path.display().to_string(),
            name,
            id,
            state,
            size: Size(orphan.bytes),
            reasons,
            deleted: Vec::new(),
            volumes,
        }
    }
}

impl fmt::Display for OrphanRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = &self.path;
        let size = self.size;
        let what = match self.kind {
            OrphanRowKind::RunFiles => format!(
                "{path} ({size}), the run files of {}, which no workspace has",
                self.name.as_deref().unwrap_or_default()
            ),
            OrphanRowKind::RunRecord => format!(
                "{path} ({size}), phase's run record and logs of the workspace id {}, which no record holds",
                self.id.as_deref().unwrap_or_default()
            ),
            OrphanRowKind::Volumes => format!(
                "the docker volumes {} of the workspace id {}, which no record holds",
                self.volumes.join(", "),
                self.id.as_deref().unwrap_or_default()
            ),
            OrphanRowKind::MovedAside => format!(
                "{path} ({size}), an archive that mxv repair moved aside because its pins were gone, with its ignored files"
            ),
            OrphanRowKind::IgnoredFiles => {
                format!("{path} ({size}), the ignored files of an archive that no record names")
            }
            OrphanRowKind::Unrecorded => {
                format!("{path} ({size}), a directory of the archives without its own record")
            }
        };
        let reasons = self.reasons.join("; ");
        match self.state {
            OrphanState::Deleted
                if self.volumes.is_empty() || self.kind == OrphanRowKind::Volumes =>
            {
                write!(formatter, "mxv deleted {what}.")
            }
            OrphanState::Deleted => write!(
                formatter,
                "mxv deleted {what}, with the docker volumes {}.",
                self.volumes.join(", ")
            ),
            OrphanState::WouldDelete => write!(formatter, "mxv would delete {what}."),
            OrphanState::Kept => write!(formatter, "mxv kept {what}, because {reasons}."),
            OrphanState::PartlyDeleted if self.deleted.is_empty() => write!(
                formatter,
                "mxv began to delete {what} and stopped, because {reasons}. Part of it may be gone."
            ),
            OrphanState::PartlyDeleted => write!(
                formatter,
                "mxv deleted {} of {what} and then stopped, because {reasons}.",
                self.deleted.join(", ")
            ),
        }
    }
}

impl fmt::Display for RepairResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for warning in &self.warnings {
            writeln!(formatter, "Warning: {warning}.")?;
        }
        let name = self.name.as_deref().unwrap_or_default();
        match self.state {
            RepairState::Unchanged if self.name.is_some() => match &self.repair {
                Some(repair) => write!(
                    formatter,
                    "mxv repair changed nothing, because `{repair}` repairs the records of {name}."
                ),
                None => write!(
                    formatter,
                    "Nothing of {name} is broken, so mxv changed nothing."
                ),
            },
            // A sweep of `--orphans`, which names no workspace.
            RepairState::Unchanged | RepairState::Swept | RepairState::WouldSweep => {
                if self.orphans.is_empty() {
                    return write!(
                        formatter,
                        "mxv found nothing that no record names, so it changed nothing."
                    );
                }
                let lines: Vec<String> = self.orphans.iter().map(ToString::to_string).collect();
                write!(formatter, "{}", lines.join("\n"))
            }
            RepairState::MovedAside | RepairState::WouldMoveAside => {
                let moved = if self.state == RepairState::MovedAside {
                    "moved"
                } else {
                    "would move"
                };
                let kept = self.kept.first().map_or("", String::as_str);
                writeln!(
                    formatter,
                    "mxv {moved} the archive of {name} aside to {kept}, because its pins are gone and no restore could bring the workspace back."
                )?;
                if let Some(tar) = self.kept.get(1) {
                    writeln!(formatter, "Its ignored files stay in {tar}.")?;
                }
                write!(
                    formatter,
                    "Nothing is deleted. `mxv repair --orphans --delete-aside` deletes these paths once you no longer need them."
                )
            }
            RepairState::Finished | RepairState::WouldFinish => {
                let dated = self.archive.as_deref().unwrap_or_default();
                let name = self.older.as_deref().unwrap_or(name);
                if self.state == RepairState::Finished {
                    write!(
                        formatter,
                        "mxv finished the older archive of {name} as {dated}. `mxv restore {dated}` brings it back."
                    )
                } else {
                    write!(
                        formatter,
                        "mxv would finish the older archive of {name} as {dated}."
                    )
                }
            }
            RepairState::Deleted => write!(
                formatter,
                "mxv deleted the archive of {name}, which the restore had already rebuilt."
            ),
            RepairState::WouldDelete => write!(
                formatter,
                "mxv would delete the archive of {name}, which the restore had already rebuilt."
            ),
        }
    }
}

/// Returns the question that the repair `planned` of `name` asks, or
/// `None` when it changes nothing.
fn question(name: &WorkspaceName, planned: &Repaired) -> Option<String> {
    match planned {
        Repaired::MovedAside { kept, .. } => Some(format!(
            "Move the archive of {name} aside to {}? Its pins are gone, so no restore can bring the workspace back.",
            kept.display()
        )),
        Repaired::Renamed { from, archive } => Some(format!(
            "Finish the older archive of {from} as {}?",
            archive.name
        )),
        Repaired::Deleted(_) => Some(format!(
            "Delete the archive of {name} and its pins? The workspace {name} that the restore rebuilt stays."
        )),
        _ => None,
    }
}

/// Returns the names whose locks the repair `planned` of `name` takes, in
/// the order it takes them: both names of a rename, so that a repair of
/// either name excludes the other, and otherwise `name` alone.
fn lock_names(name: &WorkspaceName, planned: &Repaired) -> Vec<WorkspaceName> {
    let mut names = vec![name.clone()];
    if let Repaired::Renamed { from, archive } = planned {
        names = vec![from.clone(), archive.name.clone()];
    }
    // Two repairs that take the same two locks take them in one order, so
    // neither waits for the other while it holds the first.
    names.sort();
    names.dedup();
    names
}

/// Repairs the records of the name that `args` names, or with
/// `--orphans` deletes what no record names.
pub(crate) fn repair(
    output: &mut Run,
    environment: &Environment,
    args: &RepairArgs,
) -> Result<Prose<RepairResult>, crumbkit::Error> {
    let Some(name) = &args.name else {
        return orphans(output, environment, args);
    };
    let name = parse_name(name)?;
    let workspaces = configured(environment)?;
    let records = archives(environment)?;
    let dry = output.is_dry_run();
    let planned = workspaces
        .repair(&records, &name, Mode::DryRun)
        .map_err(MxvFailure::from)?;
    let (repaired, locks) = match question(&name, &planned) {
        Some(text) if !dry => {
            output.confirm(args.yes, &text)?;
            // The person confirmed one repair, so the records are planned
            // again under the locks, and a plan that asks a different
            // question refuses before anything changes.
            let mut locks = Vec::new();
            for held in lock_names(&name, &planned) {
                locks.push(processes::lock(environment, &held)?);
            }
            let again = workspaces
                .repair(&records, &name, Mode::DryRun)
                .map_err(MxvFailure::from)?;
            if question(&name, &again).as_deref() != Some(text.as_str()) {
                return Err(MxvFailure::RepairChanged {
                    name: name.to_string(),
                }
                .into());
            }
            let repaired = workspaces
                .repair(&records, &name, Mode::Change)
                .map_err(MxvFailure::from)?;
            (repaired, locks)
        }
        // A dry run, or a name with nothing to repair, changes nothing, so
        // it takes no lock.
        _ => (planned, Vec::new()),
    };
    let result = match repaired {
        Repaired::Unchanged { repair } => RepairResult {
            repair: repair.as_ref().map(repair_text),
            ..RepairResult::new(&name, RepairState::Unchanged)
        },
        Repaired::MovedAside { kept, ignored, .. } => RepairResult {
            kept: std::iter::once(kept)
                .chain(ignored)
                .map(|path| path.display().to_string())
                .collect(),
            ..RepairResult::new(
                &name,
                if dry {
                    RepairState::WouldMoveAside
                } else {
                    RepairState::MovedAside
                },
            )
        },
        Repaired::Renamed { from, mut archive } => {
            let mut warnings = Vec::new();
            if !dry {
                // Soft limit: the command holds the locks of both names,
                // and `mxv new` takes no lock. When an `mxv new` of the
                // dated name runs in the moment between the choice of the
                // name and `finish`, `finish` still marks the archive
                // archived, beside a workspace that has another stable
                // id. `mxv list --archived` then names no problem, the
                // archive and its pins stay whole, and `mxv restore` of
                // the dated name refuses with `mxv::restore::name_taken`
                // until that workspace is gone, so nothing is lost.
                finish(
                    environment,
                    &workspaces,
                    &records,
                    &mut archive,
                    None,
                    &mut warnings,
                )?;
            }
            RepairResult {
                older: Some(from.to_string()),
                archive: Some(archive.name.to_string()),
                warnings,
                ..RepairResult::new(
                    &name,
                    if dry {
                        RepairState::WouldFinish
                    } else {
                        RepairState::Finished
                    },
                )
            }
        }
        Repaired::Deleted(removal) => RepairResult {
            pins: removal
                .archive
                .checkouts
                .iter()
                .map(|checkout| checkout.pin.clone())
                .collect(),
            warnings: removal.warnings,
            ..RepairResult::new(
                &name,
                if dry {
                    RepairState::WouldDelete
                } else {
                    RepairState::Deleted
                },
            )
        },
        // `Repaired` is non-exhaustive, so a repair that a newer library
        // adds reports no change until this command learns it.
        _ => RepairResult::new(&name, RepairState::Unchanged),
    };
    drop(locks);
    Ok(Prose::new(result))
}

/// Returns why `mxv repair --orphans` with `args` keeps `kind` without
/// trying to delete it, or `None` when the sweep deletes it. Run files and
/// run records are mxv's and phase's derived state, so the sweep deletes
/// them. The volumes of an id are listed only with `--delete-volumes`,
/// which deletes them. A moved-aside archive and ignored files hold the
/// only copies of their files, so only `--delete-aside` deletes them, and
/// `--yes` only skips the question.
fn kept_without_sweep(kind: &OrphanKind, args: &RepairArgs) -> Option<&'static str> {
    match kind {
        OrphanKind::RunFiles { .. } | OrphanKind::RunRecord { .. } => None,
        OrphanKind::Volumes { .. } if args.delete_volumes => None,
        OrphanKind::Volumes { .. } => Some("only --delete-volumes deletes docker volumes"),
        OrphanKind::MovedAside { .. } | OrphanKind::IgnoredFiles if args.delete_aside => None,
        OrphanKind::MovedAside { .. } | OrphanKind::IgnoredFiles => {
            Some("it holds the only copies of its files, so only --delete-aside deletes it")
        }
        OrphanKind::Unrecorded => Some(UNRECORDED),
    }
}

/// Lists what no record names for `mxv repair --orphans`, and deletes
/// what [`kept_without_sweep`] allows after the person confirms.
fn orphans(
    output: &mut Run,
    environment: &Environment,
    args: &RepairArgs,
) -> Result<Prose<RepairResult>, crumbkit::Error> {
    let workspaces = configured(environment)?;
    let records = archives(environment)?;
    let runs = environment.runs()?;
    let found = workspaces
        .orphans(&records, &runs, args.delete_volumes)
        .map_err(MxvFailure::from)?;
    let mut result = RepairResult::sweep();
    let swept: Vec<&Orphan> = found
        .iter()
        .filter(|orphan| kept_without_sweep(&orphan.kind, args).is_none())
        .collect();
    let dry = output.is_dry_run();
    if !dry && !swept.is_empty() {
        let paths: Vec<String> = swept
            .iter()
            .map(|orphan| orphan.path.display().to_string())
            .collect();
        let mut question = format!("Delete {}, which no record names?", paths.join(", "));
        if swept
            .iter()
            .any(|orphan| matches!(orphan.kind, OrphanKind::RunRecord { .. }))
        {
            question.push_str(
                " phase's sweep first stops the processes and removes the containers, networks and run volumes that each run record names.",
            );
        }
        output.confirm(args.yes, &question)?;
    }
    // A failure keeps its item, or names what of it is gone, with the error
    // as the reason, and the sweep goes on, so the result still names each
    // item already deleted. The command then exits with the code of the
    // first failure.
    let mut failures = Vec::new();
    for orphan in &found {
        let row = if let Some(why) = kept_without_sweep(&orphan.kind, args) {
            OrphanRow::new(orphan, OrphanState::Kept, vec![why.to_owned()])
        } else if dry {
            OrphanRow::new(orphan, OrphanState::WouldDelete, Vec::new())
        } else {
            match workspaces.sweep_orphan(&records, &runs, orphan, args.delete_volumes) {
                Ok(Swept::Deleted { volumes }) => OrphanRow {
                    volumes,
                    ..OrphanRow::new(orphan, OrphanState::Deleted, Vec::new())
                },
                Ok(Swept::Kept(reasons)) => OrphanRow::new(orphan, OrphanState::Kept, reasons),
                Ok(Swept::PartlyDeleted { deleted, error }) => {
                    let failure = MxvFailure::from(error);
                    let row = OrphanRow {
                        deleted: deleted
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect(),
                        ..OrphanRow::new(
                            orphan,
                            OrphanState::PartlyDeleted,
                            vec![failure.to_string()],
                        )
                    };
                    failures.push(failure.into());
                    row
                }
                Err(error) => {
                    let failure = MxvFailure::from(error);
                    let row = OrphanRow::new(orphan, OrphanState::Kept, vec![failure.to_string()]);
                    failures.push(failure.into());
                    row
                }
            }
        };
        match row.state {
            OrphanState::Deleted | OrphanState::PartlyDeleted => {
                result.state = RepairState::Swept;
            }
            OrphanState::WouldDelete => result.state = RepairState::WouldSweep,
            OrphanState::Kept => {}
        }
        result.orphans.push(row);
    }
    Ok(Prose::with_failures(result, failures))
}
