//! The verb `clean`, which deletes the disposable directories of idle
//! workspaces, and the document it prints.
//!
//! A workspace is idle when none of its units run, no
//! other process, such as a `cargo build`, a shell or an editor, has its
//! working directory inside the workspace, and its checkouts have not
//! changed for the idle age, as
//! [`multiverse::DiskUsage::modified`] measures it. The idle age is
//! `--idle`, or else the config file's `clean_idle`, or else
//! [`DEFAULT_IDLE`]. `clean` measures every workspace it considers, which
//! reads each file of each one, so it is meant for an occasional run, such
//! as from a cron job. Nothing runs in the background.

use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crumbkit::{Age, Batch, Document, Output, Row};
use multiverse::phase::host::TableEntry;
use multiverse::{DisposablePath, DisposableSource, Error, Git, KeptPath, Workspace, Workspaces};
use schemars::JsonSchema;
use serde::Serialize;

use crate::cli::CleanArgs;
use crate::environment::Environment;
use crate::failures::MxvFailure;
use crate::processes;
use crate::reclaim::{user_reason, users};
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
    pub(crate) name: String,
    /// `cleaned` when the run deleted the workspace's disposable
    /// directories, `would_clean` on a dry run, or `kept`.
    pub(crate) state: CleanState,
    /// The bytes that the run freed, measured before and after each
    /// deletion, or the bytes that the disposable directories take on a dry
    /// run, which is 0 for a workspace that the run kept. A file with a
    /// hard link outside the deleted directory frees nothing.
    pub(crate) freed: Size,
    /// The bytes that the workspace's disposable directories took on disk
    /// before the run.
    disposable: Size,
    /// When a file or directory of the workspace's checkouts outside the
    /// disposable directories, or a commit or staged change, last changed,
    /// or null when none exists.
    pub(crate) modified: Option<Age>,
    /// Why the run kept the workspace's disposable directories, or some of
    /// them, or null.
    pub(crate) reason: Option<String>,
    /// Why a checkout's disposable set holds fewer globs than its
    /// `mxv.toml` declares, such as an invalid manifest, or null.
    warnings: Option<String>,
    /// Each disposable path that the run deleted, or would delete, with its
    /// checkout's name in front, such as `app: target/`.
    paths: Vec<String>,
    /// Each path that the disposable set matches and that the run kept,
    /// with the reason, such as a directory that holds a `.git` entry.
    skipped: Vec<String>,
    /// The paths of `paths` that only the repository's own `mxv.toml`
    /// declares disposable, which the prompt named.
    manifest_paths: Vec<String>,
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
    const LONG_COLUMNS: &'static [&'static str] = &["paths", "skipped", "manifest_paths"];
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
    /// Keeps each path that only the repository's own `mxv.toml` makes
    /// disposable, because `--yes` without `--force` gave no person a
    /// chance to confirm it.
    fn hold_manifest_paths(&mut self) {
        let (held, paths): (Vec<DisposablePath>, Vec<DisposablePath>) = self
            .paths
            .drain(..)
            .partition(|found| found.source == DisposableSource::Manifest);
        self.paths = paths;
        for found in held {
            self.disposable = self.disposable.saturating_sub(found.bytes);
            self.kept.push(KeptPath::new(
                found.checkout,
                found.path,
                "only the repository's mxv.toml declares it disposable, and --yes alone does not delete such paths; answer the prompt without --yes, or pass --force".to_owned(),
            ));
        }
    }

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
            manifest_paths: if deletes {
                from_manifest(&self.paths)
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

/// Returns each path of `paths` that only the repository's own `mxv.toml`
/// makes disposable, with its checkout's name in front.
fn from_manifest(paths: &[DisposablePath]) -> Vec<String> {
    paths
        .iter()
        .filter(|found| found.source == DisposableSource::Manifest)
        .map(named)
        .collect()
}

/// Returns the question that `clean` asks before it deletes the paths of
/// `picked`, which names each path that only a repository's `mxv.toml`
/// makes disposable, with its workspace.
fn question(picked: &[&Candidate]) -> String {
    let mut question = format!(
        "Delete the disposable directories of {}?",
        workspaces_text(picked.len())
    );
    let declared: Vec<String> = picked
        .iter()
        .flat_map(|candidate| {
            from_manifest(&candidate.paths)
                .into_iter()
                .map(|path| format!("{path} in {}", candidate.workspace.name))
        })
        .collect();
    if !declared.is_empty() {
        question.push_str(&format!(
            " This deletes {}, which only the repository's mxv.toml declares disposable.",
            declared.join(", ")
        ));
    }
    question
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

/// Returns why the workspace whose processes `running` names, in which the
/// processes outside phase `users` work, and whose checkouts last changed at
/// `modified` is not idle at `now` for the idle age `idle`, or `None` when it
/// is idle.
pub(crate) fn busy(
    running: &[String],
    users: &[(u32, PathBuf)],
    modified: Option<SystemTime>,
    now: SystemTime,
    idle: Duration,
) -> Option<String> {
    if !running.is_empty() {
        return Some(format!("its units run: {}", running.join(", ")));
    }
    if let Some(reason) = user_reason(users) {
        return Some(reason);
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

/// Returns the key of each workspace that `names` selects, with the
/// workspace or the failure to find or read it: the named ones, in the
/// order given, or every workspace and every record that mxv cannot read.
fn selected(workspaces: &Workspaces<Git>, names: &[String]) -> Result<Vec<Selected>, MxvFailure> {
    if names.is_empty() {
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
    Ok(names
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
    table: &[TableEntry],
    now: SystemTime,
    idle: Duration,
) -> Result<Candidate, MxvFailure> {
    processes::refuse_unfinished(&workspace)?;
    let usage = workspaces.disk_usage(&workspace)?;
    let running = processes::running(environment, &workspace)?;
    // No phase pids: `busy` checks `running` first, so a workspace with
    // running phase processes never reaches the outside-process reason.
    let outside = users(table, &workspace.path, &[], std::process::id());
    Ok(Candidate {
        busy: busy(&running, &outside, usage.modified, now, idle),
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
        let table = multiverse::process_table().map_err(MxvFailure::from)?;
        let users = users(&table, &candidate.workspace.path, &[], std::process::id());
        Ok::<_, MxvFailure>(busy(
            &running,
            &users,
            fresh.modified,
            SystemTime::now(),
            idle,
        ))
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
    row.manifest_paths = from_manifest(&cleaned.deleted);
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

/// How much of the disposable directories a run of [`run`] deletes.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Limit {
    /// Every idle workspace's.
    Every,
    /// Those of idle workspaces, oldest first, until the disposable
    /// directories of every workspace take no more than these bytes, as
    /// `--over-limit` cleans down to the config file's `max_disposable`.
    Total(u64),
    /// Those of idle workspaces, oldest first, until they free these bytes,
    /// as `mxv reclaim --need disk=<SIZE>` asks.
    Free(u64),
}

/// What a run of [`run`] cleans, and how it asks.
pub(crate) struct Run<'a> {
    /// The names of the workspaces to clean, or none for every workspace.
    pub(crate) names: &'a [String],
    /// How much it deletes.
    pub(crate) limit: Limit,
    /// The idle age, or `None` for the config file's `clean_idle`, or else
    /// [`DEFAULT_IDLE`].
    pub(crate) idle: Option<Duration>,
    /// Whether it keeps each path that only a repository's own `mxv.toml`
    /// declares disposable, because no person confirms it.
    pub(crate) hold: bool,
    /// The `--yes` that answers the question before it deletes, or `None`
    /// when it asks no question.
    pub(crate) ask: Option<crumbkit::Yes>,
}

/// What a run of [`run`] did with each workspace it considered.
pub(crate) struct Cleaned {
    /// The key of each workspace, in the order that the run considered
    /// them.
    pub(crate) keys: Vec<String>,
    /// The row or the failure of each workspace, in the same order.
    pub(crate) results: Vec<Result<CleanRow, MxvFailure>>,
    /// The bytes that the run freed, or would free on a dry run.
    pub(crate) freed: u64,
    /// What the run prints after its rows.
    summary: CleanSummary,
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
    let limit = if args.over_limit {
        let config = environment.config()?;
        Limit::Total(config.max_disposable.ok_or(MxvFailure::NoLimit {
            path: environment.config_file()?,
        })?)
    } else {
        Limit::Every
    };
    let cleaned = run(
        output,
        environment,
        &Run {
            names: &args.names,
            limit,
            idle: args.idle,
            // `Yes` hides its flag, and only its default holds no `--yes`.
            hold: args.yes != crumbkit::Yes::default() && !args.force,
            ask: Some(args.yes),
        },
    )?;
    let mut batch = Batch::new(&["clean"], "names", &cleaned.keys);
    for result in cleaned.results {
        batch.push(result);
    }
    Ok(batch.note(cleaned.summary.to_string()))
}

/// Deletes the disposable directories of the idle workspaces that `plan`
/// selects, or reports what it would delete on a dry run, and returns what
/// it did with each workspace it considered.
pub(crate) fn run(
    output: &mut Output,
    environment: &Environment,
    plan: &Run<'_>,
) -> Result<Cleaned, crumbkit::Error> {
    let config = environment.config()?;
    let idle = plan.idle.or(config.clean_idle).unwrap_or(DEFAULT_IDLE);
    let workspaces = configured(environment)?;
    let now = SystemTime::now();
    let table = multiverse::process_table().map_err(MxvFailure::from)?;
    let mut keys = Vec::new();
    let mut entries = Vec::new();
    for (key, found) in selected(&workspaces, plan.names)? {
        keys.push(key);
        let mut entry = found
            .and_then(|workspace| measure(environment, &workspaces, workspace, &table, now, idle));
        if plan.hold
            && let Ok(candidate) = &mut entry
        {
            candidate.hold_manifest_paths();
        }
        entries.push(entry);
    }
    let total = entries.iter().flatten().fold(0_u64, |sum, candidate| {
        sum.saturating_add(candidate.disposable)
    });
    let limit = match plan.limit {
        Limit::Every => None,
        Limit::Total(limit) => Some(limit),
        Limit::Free(need) => Some(total.saturating_sub(need)),
    };
    let picked = picked(&entries, total, limit);
    let planned = output.is_dry_run();
    if !planned
        && !picked.is_empty()
        && let Some(yes) = plan.ask
    {
        let chosen: Vec<&Candidate> = picked
            .iter()
            .filter_map(|index| match entries.get(*index) {
                Some(Ok(candidate)) => Some(candidate),
                _ => None,
            })
            .collect();
        output.confirm(yes, &question(&chosen))?;
    }
    let mut results = Vec::new();
    let mut freed: u64 = 0;
    for (index, entry) in entries.into_iter().enumerate() {
        let candidate = match entry {
            Ok(candidate) => candidate,
            Err(failure) => {
                results.push(Err(failure));
                continue;
            }
        };
        if !picked.contains(&index) {
            let reason = candidate.busy.clone().unwrap_or_else(|| {
                if candidate.paths.is_empty() {
                    skipped_reason(&candidate.kept)
                        .unwrap_or_else(|| "it holds no disposable directory".to_owned())
                } else {
                    within_limit_reason(total, limit.unwrap_or(u64::MAX))
                }
            });
            results.push(Ok(candidate.row(CleanState::Kept, Some(reason), now)));
            continue;
        }
        if planned {
            freed = freed.saturating_add(candidate.disposable);
            results.push(Ok(candidate.row(CleanState::WouldClean, None, now)));
            continue;
        }
        let (result, bytes) = clean_one(environment, &workspaces, &candidate, idle, now);
        freed = freed.saturating_add(bytes);
        results.push(result);
    }
    let summary = CleanSummary {
        freed: Size(freed),
        disposable: Size(total),
        limit: limit.map(Size),
        planned,
    };
    Ok(Cleaned {
        keys,
        results,
        freed,
        summary,
    })
}

/// Returns the indexes of the measured `entries` that a run deletes from:
/// with a `limit`, the idle ones that [`over_limit`] picks, and without
/// one, every idle one with a disposable directory.
fn picked(entries: &[Result<Candidate, MxvFailure>], total: u64, limit: Option<u64>) -> Vec<usize> {
    let Some(limit) = limit else {
        return (0..entries.len())
            .filter(|index| match entries.get(*index) {
                Some(Ok(candidate)) => candidate.busy.is_none() && !candidate.paths.is_empty(),
                _ => false,
            })
            .collect();
    };
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
