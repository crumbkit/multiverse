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
use multiverse::{DisposablePath, Error, Git, KeptPath, Workspace, Workspaces};
use schemars::JsonSchema;
use serde::Serialize;

use crate::cli::CleanArgs;
use crate::environment::Environment;
use crate::failures::MxvFailure;
use crate::processes;
use crate::rows::joined;
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
    /// The bytes that the run freed, measured before and after each
    /// deletion, or the bytes that the disposable directories take on a dry
    /// run, which is 0 for a workspace that the run kept. A file with a
    /// hard link outside the deleted directory frees nothing.
    freed: Size,
    /// The bytes that the workspace's disposable directories took on disk
    /// before the run.
    disposable: Size,
    /// When a file or directory of the workspace's checkouts outside the
    /// disposable directories, or a commit or staged change, last changed,
    /// or null when none exists.
    modified: Option<Age>,
    /// Why the run kept the workspace's disposable directories, or some of
    /// them, or null.
    reason: Option<String>,
    /// Why a checkout's disposable set holds fewer globs than its
    /// `mxv.toml` declares, such as an invalid manifest, or null.
    warnings: Option<String>,
    /// Each disposable path that the run deleted, or would delete, with its
    /// checkout's name in front, such as `app: target/`.
    paths: Vec<String>,
    /// Each path that the disposable set matches and that the run kept,
    /// with the reason, such as a directory that holds a `.git` entry.
    skipped: Vec<String>,
}

impl Row for CleanRow {
    const COLUMNS: &'static [&'static str] = &[
        "name",
        "state",
        "freed",
        "disposable",
        "modified",
        "reason",
        "warnings",
    ];
    const LONG_COLUMNS: &'static [&'static str] = &["paths", "skipped"];
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
        let (frees, deletes) = if self.planned {
            ("would free", "would delete")
        } else {
            ("freed", "deleted")
        };
        write!(
            formatter,
            "mxv {frees} {}. The disposable directories took {} before the run",
            self.freed, self.disposable
        )?;
        match self.limit {
            Some(limit) if self.disposable > limit => {
                write!(formatter, ", over the limit of {limit}.")
            }
            Some(limit) => write!(
                formatter,
                ", within the limit of {limit}, so mxv {deletes} nothing."
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
    /// The paths that the disposable set matches and that mxv keeps.
    kept: Vec<KeptPath>,
    /// Why a checkout's disposable set holds fewer globs than declared.
    warnings: Vec<String>,
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
            reason: reason.or_else(|| skipped_reason(&self.kept)),
            warnings: joined(&self.warnings),
            paths: if deletes {
                self.paths.iter().map(named).collect()
            } else {
                Vec::new()
            },
            skipped: self.kept.iter().map(kept_text).collect(),
        }
    }
}

/// Returns `found` with its checkout's name in front, such as
/// `app: target/`.
fn named(found: &DisposablePath) -> String {
    format!("{}: {}", found.checkout, found.path)
}

/// Returns `kept` with its checkout's name in front and its reason, such
/// as `app: node_modules/, because it holds the git entry ...`.
fn kept_text(kept: &KeptPath) -> String {
    format!("{}: {}, because {}", kept.checkout, kept.path, kept.reason)
}

/// Returns the reason of a row that kept the paths `kept`, or `None` when
/// it kept none.
fn skipped_reason(kept: &[KeptPath]) -> Option<String> {
    (!kept.is_empty()).then(|| {
        let texts: Vec<String> = kept.iter().map(kept_text).collect();
        format!("mxv kept {}", texts.join("; "))
    })
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
    let Ok(elapsed) = now.duration_since(modified?) else {
        return Some(
            "a file of its checkout changed after the current time, so mxv cannot tell how long the workspace has been idle".to_owned(),
        );
    };
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

/// Returns why `--over-limit` kept an idle workspace with disposable
/// directories: the total was within `limit`, or cleaning workspaces that
/// were unchanged longer brought it there.
pub(crate) fn within_limit_reason(total: u64, limit: u64) -> String {
    if total <= limit {
        format!(
            "the disposable directories of every workspace take {}, within the limit of {}",
            Size(total),
            Size(limit)
        )
    } else {
        format!(
            "cleaning the workspaces that were unchanged longer brings the total within the limit of {}",
            Size(limit)
        )
    }
}

/// A workspace that `clean` selected, by its key, or the failure to find or
/// read it.
type Selected = (String, Result<Workspace, MxvFailure>);

/// Returns the key of each workspace that `args` selects, with the
/// workspace or the failure to find or read it: the named ones, in the
/// order given, or every workspace and every record that mxv cannot read.
fn selected(workspaces: &Workspaces<Git>, args: &CleanArgs) -> Result<Vec<Selected>, MxvFailure> {
    if args.names.is_empty() {
        let records = workspaces.list()?;
        let mut found: Vec<Selected> = records
            .workspaces
            .into_iter()
            .map(|workspace| (workspace.name.to_string(), Ok(workspace)))
            .collect();
        found.extend(records.problems.into_iter().map(|problem| {
            let key = record_name(&problem);
            (key, Err(MxvFailure::from(problem)))
        }));
        return Ok(found);
    }
    Ok(args
        .names
        .iter()
        .map(|text| {
            let found = parse_name(text).and_then(|name| {
                workspaces.find(&name)?.ok_or(MxvFailure::NotFound {
                    name: name.to_string(),
                })
            });
            (text.clone(), found)
        })
        .collect())
}

/// Returns the name of the record that `problem` names, without `.json`.
fn record_name(problem: &Error) -> String {
    match problem {
        Error::CorruptRecord { path, .. }
        | Error::NewerRecord { path, .. }
        | Error::MismatchedRecord { path, .. }
        | Error::StateUnreadable { path, .. } => path
            .file_stem()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Measures `workspace` and decides whether it is idle at `now`.
fn measure(
    environment: &Environment,
    workspaces: &Workspaces<Git>,
    workspace: Workspace,
    now: SystemTime,
    idle: Duration,
) -> Result<Candidate, MxvFailure> {
    let usage = workspaces.disk_usage(&workspace)?;
    let running = processes::running(environment, &workspace)?;
    Ok(Candidate {
        busy: busy(&running, usage.modified, now, idle),
        disposable: usage.disposable_bytes,
        modified: usage.modified,
        paths: usage.disposable,
        kept: usage.kept,
        warnings: usage.warnings,
        workspace,
    })
}

/// Takes the lock of `candidate`'s workspace, checks again that it is
/// idle, deletes its planned paths, and returns its row, or the failure of
/// a workspace whose paths could not all be deleted, with the bytes freed.
fn clean_one(
    environment: &Environment,
    workspaces: &Workspaces<Git>,
    candidate: &Candidate,
    idle: Duration,
    now: SystemTime,
) -> (Result<CleanRow, MxvFailure>, u64) {
    let name = candidate.workspace.name.to_string();
    let lock = match processes::lock(environment, &candidate.workspace.name) {
        Ok(lock) => lock,
        Err(MxvFailure::Library(Error::Busy { .. })) => {
            let reason = "another mxv command holds the workspace's lock".to_owned();
            return (Ok(candidate.row(CleanState::Kept, Some(reason), now)), 0);
        }
        Err(failure) => return (Err(failure), 0),
    };
    let result = workspaces.clean(&candidate.workspace, &candidate.paths, |fresh| {
        let running = processes::running(environment, &candidate.workspace)?;
        Ok::<_, MxvFailure>(busy(&running, fresh.modified, SystemTime::now(), idle))
    });
    drop(lock);
    let cleaned = match result {
        Ok(cleaned) => cleaned,
        Err(failure) => return (Err(failure), 0),
    };
    if let Some(reason) = cleaned.busy {
        let reason = format!("the workspace became busy before mxv deleted anything: {reason}");
        return (Ok(candidate.row(CleanState::Kept, Some(reason), now)), 0);
    }
    let freed = cleaned
        .deleted
        .iter()
        .map(|found| found.bytes)
        .chain(cleaned.failed.iter().map(|failed| failed.bytes))
        .fold(0_u64, u64::saturating_add);
    let deleted: Vec<String> = cleaned.deleted.iter().map(named).collect();
    if let Some(first) = cleaned.failed.first() {
        let failure = MxvFailure::PartlyCleaned {
            name,
            path: first.failed.clone(),
            deleted,
            failed: cleaned
                .failed
                .iter()
                .map(|failed| {
                    format!(
                        "{}: {}, at {}: {}",
                        failed.checkout,
                        failed.path,
                        failed.failed.display(),
                        failed.error
                    )
                })
                .collect(),
            freed,
        };
        return (Err(failure), freed);
    }
    let mut row = candidate.row(CleanState::Cleaned, None, now);
    row.freed = Size(freed);
    row.paths = deleted;
    row.warnings = joined(&cleaned.warnings);
    let mut kept = candidate.kept.clone();
    kept.extend(
        cleaned
            .kept
            .into_iter()
            .filter(|path| !candidate.kept.contains(path)),
    );
    row.reason = skipped_reason(&kept);
    row.skipped = kept.iter().map(kept_text).collect();
    (Ok(row), freed)
}

/// Deletes the disposable directories of the idle workspaces that `args`
/// selects, after the person confirms, or reports what it would delete on
/// a dry run. A workspace that mxv cannot find, read or clean becomes a
/// failure of the batch, and the run goes on with the others.
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
    let mut keys = Vec::new();
    let mut entries = Vec::new();
    for (key, found) in selected(&workspaces, args)? {
        keys.push(key);
        entries.push(
            found.and_then(|workspace| measure(environment, &workspaces, workspace, now, idle)),
        );
    }
    let total = entries.iter().flatten().fold(0_u64, |sum, candidate| {
        sum.saturating_add(candidate.disposable)
    });
    let picked: Vec<usize> = match limit {
        Some(limit) => {
            let measured: Vec<usize> = (0..entries.len())
                .filter(|index| matches!(entries.get(*index), Some(Ok(_))))
                .collect();
            let shapes: Vec<_> = measured
                .iter()
                .filter_map(|index| match entries.get(*index) {
                    Some(Ok(candidate)) => Some((
                        candidate.modified,
                        candidate.disposable,
                        candidate.busy.is_none(),
                    )),
                    _ => None,
                })
                .collect();
            over_limit(&shapes, total, limit)
                .into_iter()
                .filter_map(|at| measured.get(at).copied())
                .collect()
        }
        None => (0..entries.len())
            .filter(|index| match entries.get(*index) {
                Some(Ok(candidate)) => candidate.busy.is_none() && !candidate.paths.is_empty(),
                _ => false,
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
    let mut batch = Batch::new(&["clean"], "names", &keys);
    let mut freed: u64 = 0;
    for (index, entry) in entries.into_iter().enumerate() {
        let candidate = match entry {
            Ok(candidate) => candidate,
            Err(failure) => {
                batch.push(Err(failure));
                continue;
            }
        };
        if !picked.contains(&index) {
            let reason = candidate.busy.clone().unwrap_or_else(|| {
                if candidate.paths.is_empty() {
                    "it holds no disposable directory".to_owned()
                } else {
                    within_limit_reason(total, limit.unwrap_or(u64::MAX))
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
        let (result, bytes) = clean_one(environment, &workspaces, &candidate, idle, now);
        freed = freed.saturating_add(bytes);
        batch.push(result);
    }
    let summary = CleanSummary {
        freed: Size(freed),
        disposable: Size(total),
        limit: limit.map(Size),
        planned,
    };
    Ok(batch.note(summary.to_string()))
}
