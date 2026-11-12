//! The verb `repair`, which repairs the broken records of one name that
//! `mxv list` names `mxv repair NAME` for, or with `--orphans` deletes what
//! no record names, and the document it prints.
//!
//! The library's [`Workspaces::repair`](multiverse::Workspaces::repair)
//! decides what each broken record needs and carries out the move, the
//! rename or the deletion. [`Workspaces::orphans`] finds what no record
//! names, and [`Workspaces::sweep_orphan`] deletes one item. This module
//! holds the workspace's lock and finishes a renamed archive as `mxv
//! archive -w` finishes one.

use std::fmt;

use clap::Args;
use crumbkit::{Document, DryRun, Prose, Run, Yes};
use multiverse::health::{Orphan, OrphanKind, Repaired, Swept};
use multiverse::{Error, Lock, Mode, WorkspaceName, Workspaces};
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

    /// Delete what no record names, instead of repairing one name's records.
    #[arg(long)]
    pub(crate) orphans: bool,

    /// With --orphans, also delete the docker volumes labelled with each workspace id that no record holds.
    #[arg(long, requires = "orphans")]
    pub(crate) delete_volumes: bool,

    /// Print what the command would do, and change nothing.
    #[command(flatten)]
    pub(crate) dry_run: DryRun,

    /// Repair the records without asking, and with --orphans also delete the archives that a repair moved aside and the ignored files that no archive names.
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
    fn new(name: Option<&WorkspaceName>, state: RepairState) -> Self {
        Self {
            name: name.map(ToString::to_string),
            state,
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
    /// A directory under the workspace root that no workspace record
    /// names.
    Directory,
    /// The run files of a name that no workspace record holds.
    RunFiles,
    /// phase's run record and logs of a workspace id that no record holds.
    RunRecord,
    /// An archive that `mxv repair NAME` moved aside, with its ignored
    /// files.
    MovedAside,
    /// A directory of ignored files whose archive no record names.
    IgnoredFiles,
    /// A directory of the archives that holds no record.
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
}

/// One item that no record names, as `mxv repair --orphans` prints it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct OrphanRow {
    /// What the item is.
    kind: OrphanRowKind,
    /// Where it is: the directory, or the run record or the logs of a
    /// workspace id.
    path: String,
    /// What the sweep did with it, or would do on a dry run.
    state: OrphanState,
    /// The bytes that deleting it frees, measured for a moved-aside archive
    /// and for ignored files, or null.
    size: Option<Size>,
    /// The archive whose restore writes a directory again, or null.
    archive: Option<String>,
    /// The workspace id of a run record, or null.
    id: Option<String>,
    /// Why it stays, one sentence for each reason, such as a file of a
    /// directory that no archive writes again. Empty when it is deleted.
    reasons: Vec<String>,
    /// The docker volumes that the sweep deleted with a run record.
    volumes: Vec<String>,
}

impl OrphanRow {
    /// Returns the row of `orphan` in `state`, kept for `reasons`.
    fn new(orphan: &Orphan, state: OrphanState, reasons: Vec<String>) -> Self {
        let (kind, archive, id) = match &orphan.kind {
            OrphanKind::Directory { archive, .. } => (
                OrphanRowKind::Directory,
                archive.as_ref().map(ToString::to_string),
                None,
            ),
            OrphanKind::RunFiles { .. } => (OrphanRowKind::RunFiles, None, None),
            OrphanKind::RunRecord { id, .. } => {
                (OrphanRowKind::RunRecord, None, Some(id.to_string()))
            }
            OrphanKind::MovedAside { .. } => (OrphanRowKind::MovedAside, None, None),
            OrphanKind::IgnoredFiles => (OrphanRowKind::IgnoredFiles, None, None),
            OrphanKind::Unrecorded => (OrphanRowKind::Unrecorded, None, None),
        };
        Self {
            kind,
            path: orphan.path.display().to_string(),
            state,
            size: orphan.bytes.map(Size),
            archive,
            id,
            reasons,
            volumes: Vec::new(),
        }
    }

    /// Returns the item as the sentence names it, such as `the run files
    /// /x of a name that no workspace record holds`.
    fn what(&self) -> String {
        let path = &self.path;
        let size = self
            .size
            .map(|size| format!(" ({size})"))
            .unwrap_or_default();
        match self.kind {
            OrphanRowKind::Directory => match (&self.archive, self.state) {
                (Some(archive), OrphanState::Deleted | OrphanState::WouldDelete) => format!(
                    "{path}, a directory with no record that holds only what a restore of the archive {archive} writes again"
                ),
                _ => format!("{path}, a directory with no record"),
            },
            OrphanRowKind::RunFiles => {
                format!("the run files {path}, which no workspace record holds")
            }
            OrphanRowKind::RunRecord => format!(
                "the run record and logs of the workspace id {}, which no record holds",
                self.id.as_deref().unwrap_or_default()
            ),
            OrphanRowKind::MovedAside => format!(
                "{path}{size}, an archive that mxv repair moved aside because its pins were gone, with its ignored files"
            ),
            OrphanRowKind::IgnoredFiles => {
                format!("{path}{size}, the ignored files of an archive that no record names")
            }
            OrphanRowKind::Unrecorded => format!("{path}, an archive directory with no record"),
        }
    }
}

impl fmt::Display for OrphanRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = self.what();
        match self.state {
            OrphanState::Deleted => write!(formatter, "mxv deleted {what}.")?,
            OrphanState::WouldDelete => write!(formatter, "mxv would delete {what}.")?,
            OrphanState::Kept => write!(
                formatter,
                "mxv kept {what}, because {}.",
                self.reasons.join("; ")
            )?,
        }
        if !self.volumes.is_empty() {
            write!(
                formatter,
                " It deleted the docker volumes {}.",
                self.volumes.join(", ")
            )?;
        }
        Ok(())
    }
}

impl fmt::Display for RepairResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for warning in &self.warnings {
            writeln!(formatter, "Warning: {warning}.")?;
        }
        let Some(name) = &self.name else {
            if self.orphans.is_empty() {
                return write!(
                    formatter,
                    "mxv found nothing that no record names, so it changed nothing."
                );
            }
            let lines: Vec<String> = self.orphans.iter().map(ToString::to_string).collect();
            return write!(formatter, "{}", lines.join("\n"));
        };
        match self.state {
            RepairState::Unchanged | RepairState::Swept | RepairState::WouldSweep => {
                match &self.repair {
                    Some(repair) => write!(
                        formatter,
                        "mxv repair changed nothing, because `{repair}` repairs the records of {name}."
                    ),
                    None => write!(
                        formatter,
                        "Nothing of {name} is broken, so mxv changed nothing."
                    ),
                }
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
                    "Nothing is deleted. `mxv repair --orphans --yes` deletes these paths once you no longer need them."
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
            ..RepairResult::new(Some(&name), RepairState::Unchanged)
        },
        Repaired::MovedAside { kept, ignored, .. } => RepairResult {
            kept: std::iter::once(kept)
                .chain(ignored)
                .map(|path| path.display().to_string())
                .collect(),
            ..RepairResult::new(
                Some(&name),
                if dry {
                    RepairState::WouldMoveAside
                } else {
                    RepairState::MovedAside
                },
            )
        },
        Repaired::Renamed { from, mut archive } => {
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
                finish(environment, &workspaces, &records, &mut archive, None)?;
            }
            RepairResult {
                older: Some(from.to_string()),
                archive: Some(archive.name.to_string()),
                ..RepairResult::new(
                    Some(&name),
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
                Some(&name),
                if dry {
                    RepairState::WouldDelete
                } else {
                    RepairState::Deleted
                },
            )
        },
        // `Repaired` is non-exhaustive, so a repair that a newer library
        // adds reports no change until this command learns it.
        _ => RepairResult::new(Some(&name), RepairState::Unchanged),
    };
    drop(locks);
    Ok(Prose::new(result))
}

/// Reports whether only `--yes` deletes an item of `kind`: an archive that
/// a repair moved aside and ignored files that no archive names, which the
/// person deletes only after seeing them listed.
const fn needs_yes(kind: &OrphanKind) -> bool {
    matches!(
        kind,
        OrphanKind::MovedAside { .. } | OrphanKind::IgnoredFiles
    )
}

/// Deletes what no record names, for `mxv repair --orphans`.
fn orphans(
    output: &mut Run,
    environment: &Environment,
    args: &RepairArgs,
) -> Result<Prose<RepairResult>, crumbkit::Error> {
    let workspaces = configured(environment)?;
    let records = archives(environment)?;
    let runs = environment.runs()?;
    let root = environment.root()?.path;
    let found = workspaces
        .orphans(&records, &runs, &root)
        .map_err(MxvFailure::from)?;
    let dry = output.is_dry_run();
    // `Yes` hides its flag, and only its default holds no `--yes`.
    let passed_yes = args.yes != Yes::default();
    let reasons = |orphan: &Orphan| {
        if !orphan.kept.is_empty() {
            orphan.kept.clone()
        } else if needs_yes(&orphan.kind) && !passed_yes {
            vec!["only --yes deletes it, once you have seen it listed".to_owned()]
        } else {
            Vec::new()
        }
    };
    let mut result = RepairResult::new(None, RepairState::Unchanged);
    if found.unreadable > 0 {
        result.warnings.push(format!(
            "mxv cannot read {} records, which `mxv list` names, and such a record may name what looks orphaned, so mxv looked for nothing; move each aside and run `mxv repair --orphans` again",
            found.unreadable
        ));
    }
    let planned: Vec<&Orphan> = found
        .items
        .iter()
        .filter(|orphan| reasons(orphan).is_empty())
        .collect();
    if dry || planned.is_empty() {
        let state = if dry {
            OrphanState::WouldDelete
        } else {
            OrphanState::Kept
        };
        for orphan in &found.items {
            let why = reasons(orphan);
            let shown = if why.is_empty() {
                state
            } else {
                OrphanState::Kept
            };
            result.orphans.push(OrphanRow::new(orphan, shown, why));
        }
        if !planned.is_empty() {
            result.state = RepairState::WouldSweep;
        }
        return Ok(Prose::new(result));
    }
    let paths: Vec<String> = planned
        .iter()
        .map(|orphan| orphan.path.display().to_string())
        .collect();
    output.confirm(
        args.yes,
        &format!("Delete {}, which no record names?", paths.join(", ")),
    )?;
    for orphan in &found.items {
        let why = reasons(orphan);
        if !why.is_empty() {
            result
                .orphans
                .push(OrphanRow::new(orphan, OrphanState::Kept, why));
            continue;
        }
        let row = sweep(&workspaces, &records, &runs, orphan, args)?;
        if row.state == OrphanState::Deleted {
            result.state = RepairState::Swept;
        }
        result.orphans.push(row);
    }
    Ok(Prose::new(result))
}

/// Deletes `orphan` under the lock of its name, when it has one, and
/// returns its row. A lock that another command holds keeps the item.
fn sweep(
    workspaces: &Workspaces<multiverse::Git>,
    records: &multiverse::Archives,
    runs: &multiverse::Runs,
    orphan: &Orphan,
    args: &RepairArgs,
) -> Result<OrphanRow, crumbkit::Error> {
    let name = match &orphan.kind {
        OrphanKind::Directory { name, .. } => name.as_ref(),
        OrphanKind::RunFiles { name } => Some(name),
        _ => None,
    };
    let _lock: Option<Lock> = match name.map(|name| runs.lock(name)) {
        None => None,
        Some(Ok(lock)) => Some(lock),
        Some(Err(Error::Busy { .. })) => {
            let name = name.map(ToString::to_string).unwrap_or_default();
            return Ok(OrphanRow::new(
                orphan,
                OrphanState::Kept,
                vec![format!("another mxv command holds the lock of {name}")],
            ));
        }
        Some(Err(error)) => return Err(MxvFailure::from(error).into()),
    };
    let swept = workspaces
        .sweep_orphan(records, runs, orphan, args.delete_volumes)
        .map_err(MxvFailure::from)?;
    Ok(match swept {
        Swept::Deleted { volumes } => OrphanRow {
            volumes,
            ..OrphanRow::new(orphan, OrphanState::Deleted, Vec::new())
        },
        Swept::Kept(reasons) => OrphanRow::new(orphan, OrphanState::Kept, reasons),
        // `Swept` is non-exhaustive, so an outcome that a newer library
        // adds reads as kept until this command learns it.
        _ => OrphanRow::new(
            orphan,
            OrphanState::Kept,
            vec!["this mxv does not know what the sweep did".to_owned()],
        ),
    })
}
