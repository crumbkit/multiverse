//! The verb `clean`, which deletes the disposable directories of idle
//! workspaces, and the document it prints.
//!
//! A workspace is idle when none of its processes or containers run and
//! its checkouts have not changed for the idle age, as
//! [`multiverse::DiskUsage::modified`] measures it. The idle age is
//! `--idle`, or else the config file's `clean_idle`, or else
//! [`DEFAULT_IDLE`]. `clean` measures every workspace it considers, which
//! reads each file of each one, so it is meant for an occasional run, such
//! as from a cron job. Nothing runs in the background.

use std::fmt;
use std::time::{Duration, SystemTime};

use crumbkit::{Age, Batch, Document, Output, Row};
use multiverse::{DisposablePath, Git, Workspace, Workspaces};
use schemars::JsonSchema;
use serde::Serialize;

use crate::cli::CleanArgs;
use crate::environment::Environment;
use crate::failures::MxvFailure;
use crate::processes;
use crate::units::Size;
use crate::verbs::{configured, parse_name};

#[cfg(test)]
mod tests;

/// The idle age when neither `--idle` nor the config file's `clean_idle`
/// sets one: seven days.
pub(crate) const DEFAULT_IDLE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The document that `mxv clean` prints.
pub(crate) const CLEAN: Document<Batch<CleanRow, MxvFailure>> = Document::new(
    "mxv",
    "clean",
    1,
    "Each workspace that the run considered, with the disposable directories it deleted, or would delete on a dry run, and the bytes they took, or why it kept them, and a failure for each workspace whose directories could not be deleted.",
);

/// What `mxv clean` does with one workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CleanState {
    /// The run deleted the workspace's disposable directories.
    Cleaned,
    /// A dry run would delete the workspace's disposable directories.
    WouldClean,
    /// The run keeps the workspace's disposable directories, for the
    /// reason that the row gives.
    Kept,
}

/// One workspace, as `mxv clean` prints it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct CleanRow {
    /// The workspace's name.
    name: String,
    /// `cleaned` when the run deleted the workspace's disposable
    /// directories, `would_clean` on a dry run, or `kept`.
    state: CleanState,
    /// The bytes that the deleted directories took on disk, or would free
    /// on a dry run, which is 0 for a workspace that the run kept.
    freed: Size,
    /// The bytes that the workspace's disposable directories took on disk
    /// before the run.
    disposable: Size,
    /// When a file of the workspace's checkouts outside the disposable
    /// directories, or a commit or staged change, last changed, or null when
    /// none exists.
    modified: Option<Age>,
    /// Why the run kept the workspace's disposable directories, or null.
    reason: Option<String>,
    /// Each disposable path that the run deleted, or would delete, with its
    /// checkout's name in front, such as `app: target/`.
    paths: Vec<String>,
}

impl Row for CleanRow {
    const COLUMNS: &'static [&'static str] =
        &["name", "state", "freed", "disposable", "modified", "reason"];
    const LONG_COLUMNS: &'static [&'static str] = &["paths"];
    // A person copies the name into the next command.
    const UNCUT_COLUMNS: &'static [&'static str] = &["name"];
}

/// The note that `mxv clean` prints after its rows: the bytes freed and
/// how the total compares with the limit.
struct CleanSummary {
    /// The bytes that the run freed, or would free on a dry run.
    freed: Size,
    /// The bytes that the disposable directories of the workspaces that the
    /// run considered took before it.
    disposable: Size,
    /// The config file's `max_disposable`, which `--over-limit` cleans
    /// down to, or `None` without `--over-limit`.
    limit: Option<Size>,
    /// Whether the run is a dry run, which deleted nothing.
    planned: bool,
}

/// Prints how many bytes the run freed, and how the total compares with
/// the limit.
impl fmt::Display for CleanSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let frees = if self.planned { "would free" } else { "freed" };
        write!(
            formatter,
            "mxv {frees} {}. The disposable directories took {}",
            self.freed, self.disposable
        )?;
        match self.limit {
            Some(limit) if self.disposable > limit => {
                write!(formatter, ", over the limit of {limit}.")
            }
            Some(limit) => write!(
                formatter,
                ", within the limit of {limit}, so mxv deleted nothing."
            ),
            None => formatter.write_str("."),
        }
    }
}

/// One workspace that `clean` considers, measured.
struct Candidate {
    /// The workspace.
    workspace: Workspace,
    /// The disposable paths of its checkouts and their sizes.
    paths: Vec<DisposablePath>,
    /// The bytes that its disposable paths take.
    disposable: u64,
    /// When its checkouts last changed.
    modified: Option<SystemTime>,
    /// Why it is not idle, or `None` when it is.
    busy: Option<String>,
}

impl Candidate {
    /// Returns the row of the candidate in `state`, with `reason`.
    fn row(&self, state: CleanState, reason: Option<String>, now: SystemTime) -> CleanRow {
        let deletes = state != CleanState::Kept;
        CleanRow {
            name: self.workspace.name.to_string(),
            state,
            freed: Size(if deletes { self.disposable } else { 0 }),
            disposable: Size(self.disposable),
            modified: self.modified.map(|time| Age::since(time, now)),
            reason,
            paths: if deletes {
                self.paths.iter().map(named).collect()
            } else {
                Vec::new()
            },
        }
    }
}

/// Returns `found` with its checkout's name in front, such as
/// `app: target/`.
fn named(found: &DisposablePath) -> String {
    format!("{}: {}", found.checkout, found.path)
}

/// Returns `count` workspaces as words.
fn workspaces_text(count: usize) -> String {
    if count == 1 {
        "1 workspace".to_owned()
    } else {
        format!("{count} workspaces")
    }
}

/// Returns why the workspace whose processes `running` names and whose
/// checkouts last changed at `modified` is not idle at `now` for the idle
/// age `idle`, or `None` when it is idle.
pub(crate) fn busy(
    running: &[String],
    modified: Option<SystemTime>,
    now: SystemTime,
    idle: Duration,
) -> Option<String> {
    if !running.is_empty() {
        return Some(format!(
            "its processes or containers run: {}",
            running.join(", ")
        ));
    }
    let elapsed = now.duration_since(modified?).unwrap_or_default();
    (elapsed < idle).then(|| {
        format!(
            "its checkout changed {} ago, within the idle age of {}",
            Age::compact(elapsed),
            Age::compact(idle)
        )
    })
}

/// Returns the indexes of the idle `candidates` that `--over-limit`
/// cleans: the oldest first, one by one, until the disposable bytes of
/// every workspace, `total`, minus the bytes of those picked, are no more
/// than `limit`.
pub(crate) fn over_limit(
    candidates: &[(Option<SystemTime>, u64, bool)],
    total: u64,
    limit: u64,
) -> Vec<usize> {
    let mut order: Vec<usize> = (0..candidates.len())
        .filter(|index| {
            candidates
                .get(*index)
                .is_some_and(|(_, bytes, idle)| *idle && *bytes > 0)
        })
        .collect();
    order.sort_by_key(|index| candidates.get(*index).map(|(modified, _, _)| *modified));
    let mut remaining = total;
    let mut picked = Vec::new();
    for index in order {
        if remaining <= limit {
            break;
        }
        if let Some((_, bytes, _)) = candidates.get(index) {
            remaining = remaining.saturating_sub(*bytes);
            picked.push(index);
        }
    }
    picked
}

/// Returns the workspaces that `args` selects: the named ones, in the
/// order given, or every workspace whose record mxv can read.
fn selected(workspaces: &Workspaces<Git>, args: &CleanArgs) -> Result<Vec<Workspace>, MxvFailure> {
    if args.names.is_empty() {
        return Ok(workspaces.list()?.workspaces);
    }
    args.names
        .iter()
        .map(|text| {
            let name = parse_name(text)?;
            workspaces.find(&name)?.ok_or(MxvFailure::NotFound {
                name: name.to_string(),
            })
        })
        .collect()
}

/// Deletes the disposable directories of the idle workspaces that `args`
/// selects, after the person confirms, or reports what it would delete on
/// a dry run.
pub(crate) fn clean(
    output: &mut Output,
    environment: &Environment,
    args: &CleanArgs,
) -> Result<Batch<CleanRow, MxvFailure>, crumbkit::Error> {
    let config = environment.config()?;
    let limit = if args.over_limit {
        Some(config.max_disposable.ok_or(MxvFailure::NoLimit {
            path: environment.config_file()?,
        })?)
    } else {
        None
    };
    let idle = args.idle.or(config.clean_idle).unwrap_or(DEFAULT_IDLE);
    let workspaces = configured(environment)?;
    let now = SystemTime::now();
    let mut candidates = Vec::new();
    for workspace in selected(&workspaces, args)? {
        let usage = workspaces
            .disk_usage(&workspace)
            .map_err(MxvFailure::from)?;
        let running = processes::running(environment, &workspace)?;
        candidates.push(Candidate {
            busy: busy(&running, usage.modified, now, idle),
            paths: usage.disposable,
            disposable: usage.disposable_bytes,
            modified: usage.modified,
            workspace,
        });
    }
    let total = candidates.iter().fold(0_u64, |sum, candidate| {
        sum.saturating_add(candidate.disposable)
    });
    let picked: Vec<usize> = match limit {
        Some(limit) => {
            let shapes: Vec<_> = candidates
                .iter()
                .map(|candidate| {
                    (
                        candidate.modified,
                        candidate.disposable,
                        candidate.busy.is_none(),
                    )
                })
                .collect();
            over_limit(&shapes, total, limit)
        }
        None => (0..candidates.len())
            .filter(|index| {
                candidates.get(*index).is_some_and(|candidate| {
                    candidate.busy.is_none() && !candidate.paths.is_empty()
                })
            })
            .collect(),
    };
    let planned = output.is_dry_run();
    if !planned && !picked.is_empty() {
        output.confirm(
            args.yes,
            &format!(
                "Delete the disposable directories of {}?",
                workspaces_text(picked.len())
            ),
        )?;
    }
    let names: Vec<String> = candidates
        .iter()
        .map(|candidate| candidate.workspace.name.to_string())
        .collect();
    let mut batch = Batch::new(&["clean"], "names", &names);
    let mut freed: u64 = 0;
    for (index, candidate) in candidates.iter().enumerate() {
        if !picked.contains(&index) {
            let reason = candidate.busy.clone().unwrap_or_else(|| {
                if candidate.paths.is_empty() {
                    "it holds no disposable directory".to_owned()
                } else {
                    "the disposable directories of every workspace take no more than the limit without cleaning it".to_owned()
                }
            });
            batch.push(Ok(candidate.row(CleanState::Kept, Some(reason), now)));
            continue;
        }
        if planned {
            freed = freed.saturating_add(candidate.disposable);
            batch.push(Ok(candidate.row(CleanState::WouldClean, None, now)));
            continue;
        }
        // A workspace whose directories cannot be deleted fails alone, and
        // the run goes on with the next one.
        match workspaces.clean(&candidate.workspace, &candidate.paths) {
            Ok(cleaned) => {
                let bytes = cleaned
                    .iter()
                    .fold(0_u64, |sum, found| sum.saturating_add(found.bytes));
                freed = freed.saturating_add(bytes);
                let mut row = candidate.row(CleanState::Cleaned, None, now);
                row.freed = Size(bytes);
                row.paths = cleaned.iter().map(named).collect();
                batch.push(Ok(row));
            }
            Err(error) => batch.push(Err(MxvFailure::from(error))),
        }
    }
    let summary = CleanSummary {
        freed: Size(freed),
        disposable: Size(total),
        limit: limit.map(Size),
        planned,
    };
    Ok(batch.note(summary.to_string()))
}
