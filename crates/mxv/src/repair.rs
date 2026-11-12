//! The verb `repair`, which repairs the broken records of one name that
//! `mxv list` names `mxv repair NAME` for, and the document it prints.
//!
//! The library's [`Workspaces::repair`](multiverse::Workspaces::repair)
//! decides what each broken record needs and carries out the move, the
//! rename or the deletion. This module holds the workspace's lock and
//! finishes a renamed archive as `mxv archive -w` finishes one.

use std::fmt;

use clap::Args;
use crumbkit::{Document, DryRun, Prose, Run, Yes};
use multiverse::health::Repaired;
use multiverse::{Mode, WorkspaceName};
use schemars::JsonSchema;
use serde::Serialize;

use crate::archive::{ArchiveFailure, archives, finish};
use crate::environment::Environment;
use crate::failures::MxvFailure;
use crate::processes;
use crate::rows::repair_text;
use crate::verbs::{configured, parse_name};

/// The document that `mxv repair` prints.
pub(crate) const REPAIR: Document<Prose<RepairResult>> = Document::new(
    "mxv",
    "repair",
    1,
    "What the repair of one name's broken records did, or would do on a dry run.",
);

/// The arguments of `mxv repair`.
#[derive(Debug, Args)]
pub(crate) struct RepairArgs {
    /// The name of the workspace or archive whose records to repair
    pub(crate) name: String,

    /// Print what the command would do, and change nothing.
    #[command(flatten)]
    pub(crate) dry_run: DryRun,

    /// Repair the records without asking.
    #[command(flatten)]
    pub(crate) yes: Yes,
}

/// What the repair did with the records of the name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RepairState {
    /// Nothing of the name is broken, or what is broken is another
    /// command's to repair, which `repair` names.
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
}

/// What `mxv repair` did, or would do on a dry run.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct RepairResult {
    /// The name whose records the command repaired.
    name: String,
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
    /// What the repair could not finish, one sentence for each, such as a
    /// pin that it could not delete.
    warnings: Vec<String>,
}

impl RepairResult {
    /// Returns the result for `name` in `state`, with no other field set.
    fn new(name: &WorkspaceName, state: RepairState) -> Self {
        Self {
            name: name.to_string(),
            state,
            older: None,
            archive: None,
            kept: Vec::new(),
            repair: None,
            pins: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

impl fmt::Display for RepairResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for warning in &self.warnings {
            writeln!(formatter, "Warning: {warning}.")?;
        }
        let name = &self.name;
        match self.state {
            RepairState::Unchanged => match &self.repair {
                Some(repair) => write!(
                    formatter,
                    "mxv repair changed nothing, because `{repair}` repairs the records of {name}."
                ),
                None => write!(
                    formatter,
                    "Nothing of {name} is broken, so mxv changed nothing."
                ),
            },
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
                    "Nothing is deleted, and nothing in mxv deletes these paths later, so delete them yourself once you no longer need them."
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

/// Repairs the records of the name that `args` names.
pub(crate) fn repair(
    output: &mut Run,
    environment: &Environment,
    args: &RepairArgs,
) -> Result<Prose<RepairResult>, crumbkit::Error> {
    let name = parse_name(&args.name)?;
    let workspaces = configured(environment)?;
    let records = archives(environment)?;
    let dry = output.is_dry_run();
    let planned = workspaces
        .repair(&records, &name, Mode::DryRun)
        .map_err(MxvFailure::from)?;
    let (repaired, lock) = match question(&name, &planned) {
        Some(text) if !dry => {
            output.confirm(args.yes, &text)?;
            // The person confirmed one repair, so the records are planned
            // again under the lock, and a plan that asks a different
            // question refuses before anything changes.
            let lock = processes::lock(environment, &name)?;
            let again = workspaces
                .repair(&records, &name, Mode::DryRun)
                .map_err(MxvFailure::from)?;
            if question(&name, &again).as_deref() != Some(text.as_str()) {
                return Err(ArchiveFailure::RepairChanged {
                    name: name.to_string(),
                }
                .into());
            }
            let repaired = workspaces
                .repair(&records, &name, Mode::Change)
                .map_err(MxvFailure::from)?;
            (repaired, Some(lock))
        }
        // A dry run, or a name with nothing to repair, changes nothing, so
        // it takes no lock.
        _ => (planned, None),
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
            if !dry {
                // Soft limit: the dated name was free when the library
                // chose it, and this command holds only the lock of
                // `name`. When an `mxv new` of the dated name runs in the
                // moment between them, `finish` still marks the archive
                // archived, beside a workspace that has another stable id.
                // `mxv list --archived` then names no problem, the archive
                // and its pins stay whole, and `mxv restore` of the dated
                // name refuses with `mxv::restore::name_taken` until that
                // workspace is gone, so nothing is lost. Taking the lock of
                // the dated name inside the library's choice would close
                // the gap.
                finish(environment, &workspaces, &records, &mut archive, None)?;
            }
            RepairResult {
                older: Some(from.to_string()),
                archive: Some(archive.name.to_string()),
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
    drop(lock);
    Ok(Prose::new(result))
}
