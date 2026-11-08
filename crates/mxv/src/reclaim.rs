//! The verb `reclaim`, which stops idle workspaces to free memory, or
//! cleans the build output of idle workspaces to free disk, and the verbs
//! `pin` and `unpin`, which keep a workspace from being stopped.
//!
//! A load manager calls `mxv reclaim --need memory=<SIZE>` when the machine
//! runs short of memory. A running workspace is idle when it is not
//! pinned, no process outside phase has its working directory inside the
//! workspace, and neither its checkouts, commits and staged changes nor an
//! mxv command that named it changed within the idle age. That age is
//! `--idle`, or else the config file's `stop_idle`, or else
//! [`DEFAULT_STOP_IDLE`]. Reclaim stops the idle workspaces that were
//! active least recently, until the resident memory of their processes
//! and containers covers the need, through the same stop as `mxv down`,
//! and marks each one so that `mxv status` says so and `mxv up` starts the
//! same processes again.
//!
//! `mxv reclaim --need disk=<SIZE>` runs the clean of `mxv clean
//! --over-limit` with a target of freeing the need, and keeps each path
//! that only a repository's own `mxv.toml` declares disposable, because no
//! person confirms it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crumbkit::{Age, Batch, Document, Output, Prose, Row};
use multiverse::phase::host::TableEntry;
use multiverse::{Footprint, Git, Runs, Workspace, Workspaces};
use schemars::JsonSchema;
use serde::Serialize;

use crate::clean::{self, CleanState, Limit, Run};
use crate::cli::{PinArgs, ReclaimArgs};
use crate::environment::Environment;
use crate::failures::MxvFailure;
use crate::processes;
use crate::units::{Size, parse_age, parse_size};
use crate::verbs::configured;

#[cfg(test)]
mod tests;

/// The idle age of a running workspace when neither `--idle` nor the
/// config file's `stop_idle` sets one: two hours.
pub(crate) const DEFAULT_STOP_IDLE: Duration = Duration::from_secs(2 * 60 * 60);

/// The shortest idle age that `--idle` accepts: ten minutes. A shorter
/// one would stop a workspace that a person left for a few minutes. The
/// config file's `stop_idle` and `clean_idle` may set a shorter age, for
/// tests.
pub(crate) const MIN_IDLE: Duration = Duration::from_secs(10 * 60);

/// Whether stopping a container returns its resident memory to the host
/// at once. On macOS containers run in Docker Desktop's virtual machine,
/// which keeps the memory that they free for a while, so mxv reports
/// their memory and does not count it towards a need.
const CONTAINERS_RETURN_MEMORY: bool = !cfg!(target_os = "macos");

/// Returns the idle age that `text` names for `--idle`, which is at least
/// [`MIN_IDLE`].
///
/// # Errors
///
/// Returns the reason that `text` is not an age or is shorter than
/// [`MIN_IDLE`].
pub(crate) fn parse_idle(text: &str) -> Result<Duration, String> {
    let age = parse_age(text)?;
    if age < MIN_IDLE {
        return Err(format!(
            "{text:?} is shorter than 10m, the shortest idle age that --idle accepts; stop_idle and clean_idle in the config file can set a shorter one"
        ));
    }
    Ok(age)
}

/// Returns the bytes that stopping a workspace frees towards a need: the
/// resident memory of its processes, `processes`, and of its containers,
/// `containers`, when `containers_count` says that stopping a container
/// returns its memory to the host.
pub(crate) const fn counted(processes: u64, containers: u64, containers_count: bool) -> u64 {
    if containers_count {
        processes.saturating_add(containers)
    } else {
        processes
    }
}

/// The document that `mxv reclaim` prints.
pub(crate) const RECLAIM: Document<Batch<ReclaimRow, crumbkit::Error>> = Document::new(
    "mxv",
    "reclaim",
    1,
    "Each workspace that the run considered, with what it stopped or cleaned, or would on a dry run, the bytes that frees, and why the workspace counts as idle or why the run kept it, and a failure for each workspace that could not be measured, stopped or cleaned.",
);

/// The document that `mxv pin` prints.
pub(crate) const PIN: Document<Prose<PinResult>> = Document::new(
    "mxv",
    "pin",
    1,
    "The workspace and whether it is pinned, which it is after the run.",
);

/// The document that `mxv unpin` prints.
pub(crate) const UNPIN: Document<Prose<PinResult>> = Document::new(
    "mxv",
    "unpin",
    1,
    "The workspace and whether it is pinned, which it is not after the run.",
);

/// What a reclaim must free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Need {
    /// Bytes of resident memory, which stopping idle workspaces frees.
    Memory(u64),
    /// Bytes on disk, which cleaning idle workspaces' build output frees.
    Disk(u64),
}

/// Returns the need that `text` names: `memory=<SIZE>` or `disk=<SIZE>`,
/// with a size as `mxv clean` reads one, such as `memory=2GiB`.
///
/// # Errors
///
/// Returns the reason that `text` is not a need.
pub(crate) fn parse_need(text: &str) -> Result<Need, String> {
    let shape = || {
        format!(
            "{text:?} is not a need, which is memory=<SIZE> or disk=<SIZE>, such as memory=2GiB"
        )
    };
    let (resource, size) = text.split_once('=').ok_or_else(shape)?;
    let resource = match resource.trim() {
        "memory" => Need::Memory,
        "disk" => Need::Disk,
        _ => return Err(shape()),
    };
    Ok(resource(parse_size(size)?))
}

/// What `mxv reclaim` does with one workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReclaimState {
    /// The run stopped the workspace's processes and containers.
    Stopped,
    /// A dry run would stop the workspace's processes and containers.
    WouldStop,
    /// The run deleted the workspace's disposable directories.
    Cleaned,
    /// A dry run would delete the workspace's disposable directories.
    WouldClean,
    /// The run left the workspace as it was, for the reason that the row
    /// gives.
    Kept,
}

/// One workspace, as `mxv reclaim` prints it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ReclaimRow {
    /// The workspace's name.
    name: String,
    /// `stopped`, `would_stop`, `cleaned`, `would_clean` or `kept`.
    state: ReclaimState,
    /// The bytes that the run freed, or would free on a dry run, which is
    /// 0 for a workspace that it kept. For memory it is the resident
    /// memory of the workspace's processes, measured before the stop, and
    /// on Linux of its containers too.
    freed: Size,
    /// For memory, the resident memory of the workspace's containers,
    /// measured before the stop, and 0 for a workspace that the run kept
    /// or a disk need. On macOS Docker Desktop's virtual machine keeps the
    /// memory that stopped containers free for a while, so freed leaves it
    /// out there.
    containers: Size,
    /// How long ago the workspace was last active: its checkouts changed
    /// or an mxv command named it. It is null when neither happened, and
    /// for a workspace that the run kept without measuring.
    active: Option<Age>,
    /// Why the workspace counts as idle, for one that the run stopped or
    /// cleaned, or why the run kept it.
    reason: Option<String>,
}

impl Row for ReclaimRow {
    const COLUMNS: &'static [&'static str] =
        &["name", "state", "freed", "containers", "active", "reason"];
    // A person copies the name into the next command.
    const UNCUT_COLUMNS: &'static [&'static str] = &["name"];
}

/// The facts about one running workspace that decide whether it is idle.
#[derive(Debug)]
pub(crate) struct Facts {
    /// Whether `mxv pin` pinned the workspace.
    pub(crate) pinned: bool,
    /// When a file of its checkouts, a commit or a staged change last
    /// changed, or `None` when none did or the run did not measure it.
    pub(crate) modified: Option<SystemTime>,
    /// When an mxv command last named the workspace, or `None`.
    pub(crate) touched: Option<SystemTime>,
    /// Each process outside phase whose working directory is inside the
    /// workspace, with that directory.
    pub(crate) users: Vec<(u32, PathBuf)>,
}

/// Whether a running workspace is idle.
#[derive(Debug)]
pub(crate) enum Verdict {
    /// The workspace is idle.
    Idle {
        /// When it was last active, or `None` when nothing it records says.
        active: Option<SystemTime>,
        /// Why it counts as idle, in a sentence.
        why: String,
    },
    /// The workspace is in use, for this reason.
    InUse(String),
}

/// Returns how long ago `time` was at `now`, or the reason that mxv cannot
/// tell when `time` is after `now`. `what` names the event.
fn elapsed(
    time: Option<SystemTime>,
    now: SystemTime,
    what: &str,
) -> Result<Option<Duration>, String> {
    let Some(time) = time else {
        return Ok(None);
    };
    now.duration_since(time).map(Some).map_err(|_| {
        format!(
            "{what} after the current time, so mxv cannot tell how long the workspace has been idle"
        )
    })
}

/// Returns whether the workspace that `facts` describes is idle at `now`
/// for the idle age `stop_idle`. A pinned workspace and one that a process
/// outside phase works in are in use whatever their times say.
pub(crate) fn idle(facts: &Facts, now: SystemTime, stop_idle: Duration) -> Verdict {
    if facts.pinned {
        return Verdict::InUse("it is pinned; mxv unpin lets reclaim stop it".to_owned());
    }
    if let Some(reason) = user_reason(&facts.users) {
        return Verdict::InUse(reason);
    }
    let changed = match elapsed(facts.modified, now, "a file of its checkouts changed") {
        Ok(changed) => changed,
        Err(reason) => return Verdict::InUse(reason),
    };
    let named = match elapsed(facts.touched, now, "an mxv command named it") {
        Ok(named) => named,
        Err(reason) => return Verdict::InUse(reason),
    };
    let idle_age = Age::compact(stop_idle);
    if let Some(changed) = changed.filter(|changed| *changed < stop_idle) {
        return Verdict::InUse(format!(
            "its checkouts changed {} ago, within the idle age of {idle_age}",
            Age::compact(changed)
        ));
    }
    if let Some(named) = named.filter(|named| *named < stop_idle) {
        return Verdict::InUse(format!(
            "an mxv command named it {} ago, within the idle age of {idle_age}",
            Age::compact(named)
        ));
    }
    let unchanged = changed.map_or_else(
        || "its checkouts, commits and staged changes have not changed".to_owned(),
        |changed| {
            format!(
                "its checkouts, commits and staged changes are unchanged for {}",
                Age::compact(changed)
            )
        },
    );
    let untouched = named.map_or_else(
        || "no mxv command has named it".to_owned(),
        |named| format!("no mxv command named it for {}", Age::compact(named)),
    );
    Verdict::Idle {
        active: facts.modified.max(facts.touched),
        why: format!("{unchanged}, {untouched}, and no process outside phase works in it"),
    }
}

/// Returns whether measuring the workspace that `facts` describes must
/// walk its checkouts for when they last changed, which reads each of
/// their files. A workspace that is pinned, that a process outside phase
/// works in, or that an mxv command named within the idle age `stop_idle`
/// at `now` is in use whatever the walk finds, so it skips the walk.
pub(crate) fn walks(facts: &Facts, now: SystemTime, stop_idle: Duration) -> bool {
    let named_recently = matches!(
        elapsed(facts.touched, now, "an mxv command named it"),
        Ok(Some(named)) if named < stop_idle
    );
    !facts.pinned && facts.users.is_empty() && !named_recently
}

/// Returns why a workspace that a run measured as idle must not stop, from
/// `facts`, which mxv read again under the workspace's lock, or `None`
/// when it is still idle. `facts` is `None` when nothing of the workspace
/// runs any longer. `touched` is when an mxv command last named it, as the
/// run read it, and `now` and `stop_idle` are the time and the idle age.
pub(crate) fn recheck(
    facts: Option<&Facts>,
    touched: Option<SystemTime>,
    now: SystemTime,
    stop_idle: Duration,
) -> Option<String> {
    let Some(facts) = facts else {
        return Some("its processes and containers stopped before mxv reached it".to_owned());
    };
    if facts.pinned {
        return Some("it was pinned after mxv measured it".to_owned());
    }
    if facts.touched > touched {
        return Some("an mxv command named it after mxv measured it".to_owned());
    }
    match idle(facts, now, stop_idle) {
        Verdict::Idle { .. } => None,
        Verdict::InUse(reason) => Some(format!(
            "{reason}, which mxv found when it checked again under the workspace's lock"
        )),
    }
}

/// Returns the indexes of the idle workspaces `idle`, each with when it
/// was last active and the bytes that stopping it frees, that a reclaim
/// of `need` bytes stops: the least recently active first, one by one,
/// until the bytes of those chosen cover the need. A workspace that frees
/// nothing is never chosen. When every workspace together frees less than
/// the need, all of them are chosen.
pub(crate) fn choose(idle: &[(Option<SystemTime>, u64)], need: u64) -> Vec<usize> {
    let mut order: Vec<usize> = (0..idle.len())
        .filter(|index| idle.get(*index).is_some_and(|(_, bytes)| *bytes > 0))
        .collect();
    order.sort_by_key(|index| idle.get(*index).map(|(active, _)| *active));
    let mut freed: u64 = 0;
    let mut chosen = Vec::new();
    for index in order {
        if freed >= need {
            break;
        }
        if let Some((_, bytes)) = idle.get(index) {
            freed = freed.saturating_add(*bytes);
            chosen.push(index);
        }
    }
    chosen
}

/// Returns the entries of `table` that phase runs for a workspace whose
/// host processes are `apps`: each app, its keeper, which is the app's
/// parent, and every process that descends from either or is in an app's
/// process group. A parent of 1 or 0 belongs to the system and never
/// counts as a keeper.
pub(crate) fn owned<'t>(
    table: &'t [TableEntry],
    apps: &[u32],
) -> impl Iterator<Item = &'t TableEntry> {
    let parents: BTreeMap<u32, u32> = table
        .iter()
        .map(|entry| (entry.pid, entry.parent))
        .collect();
    let mut roots: BTreeSet<u32> = apps.iter().copied().collect();
    roots.extend(
        apps.iter()
            .filter_map(|app| parents.get(app).copied())
            .filter(|parent| *parent > 1),
    );
    let groups: BTreeSet<u32> = apps.iter().copied().collect();
    table.iter().filter(move |entry| {
        if groups.contains(&entry.group) {
            return true;
        }
        // Each step goes to a parent, and the table holds each pid once, so
        // a walk longer than the table has met a loop and stops.
        let mut pid = entry.pid;
        for _ in 0..=parents.len() {
            if roots.contains(&pid) {
                return true;
            }
            match parents.get(&pid) {
                Some(parent) if *parent > 1 && *parent != pid => pid = *parent,
                _ => return false,
            }
        }
        false
    })
}

/// Returns each process of `table` whose working directory is inside
/// `dir`, a workspace's directory, which it canonicalizes when it can, and
/// that neither phase runs for the workspace's host processes `apps` nor is
/// mxv itself, `own`, with that directory.
pub(crate) fn users(
    table: &[TableEntry],
    dir: &Path,
    apps: &[u32],
    own: u32,
) -> Vec<(u32, PathBuf)> {
    let dir = &dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let phase: BTreeSet<u32> = owned(table, apps).map(|entry| entry.pid).collect();
    table
        .iter()
        .filter(|entry| entry.pid != own && !phase.contains(&entry.pid))
        .filter_map(|entry| {
            let inside = entry.dir.as_ref().filter(|found| found.starts_with(dir))?;
            Some((entry.pid, inside.clone()))
        })
        .collect()
}

/// Returns the sentence that names the first of `users`, the processes
/// outside phase that work in a workspace, or `None` when there are none.
pub(crate) fn user_reason(users: &[(u32, PathBuf)]) -> Option<String> {
    users.first().map(|(pid, dir)| {
        format!(
            "process {pid} outside phase works in it, in {}",
            dir.display()
        )
    })
}

/// Returns the bytes that the processes of `table` that phase runs for the
/// host processes `apps` hold resident.
fn resident(table: &[TableEntry], apps: &[u32]) -> u64 {
    owned(table, apps)
        .filter_map(|entry| entry.resident)
        .fold(0_u64, u64::saturating_add)
}

/// One workspace that a memory reclaim considered.
struct Measured {
    /// The workspace.
    workspace: Workspace,
    /// When an mxv command last named it, as the run read it.
    touched: Option<SystemTime>,
    /// Whether it is idle, or `None` when nothing of it runs.
    verdict: Option<Verdict>,
    /// The bytes that stopping it frees towards the need, measured for an
    /// idle one.
    bytes: u64,
    /// The resident memory of its containers, measured for an idle one.
    containers: u64,
}

impl Measured {
    /// Returns the row of the workspace in `state`, with `freed` bytes and
    /// `reason`.
    fn row(&self, state: ReclaimState, freed: u64, reason: String, now: SystemTime) -> ReclaimRow {
        let containers = if matches!(state, ReclaimState::Stopped | ReclaimState::WouldStop) {
            self.containers
        } else {
            0
        };
        let active = match &self.verdict {
            Some(Verdict::Idle { active, .. }) => *active,
            _ => None,
        };
        ReclaimRow {
            name: self.workspace.name.to_string(),
            state,
            freed: Size(freed),
            containers: Size(containers),
            active: active.map(|time| Age::since(time, now)),
            reason: Some(reason),
        }
    }
}

/// What a memory reclaim reads once for every workspace.
struct Machine {
    /// The run files of every workspace.
    runs: Runs,
    /// The workspaces, to measure when a checkout last changed.
    workspaces: Workspaces<Git>,
    /// The process table.
    table: Vec<TableEntry>,
    /// The idle age.
    stop_idle: Duration,
    /// The time the run started.
    now: SystemTime,
}

impl Machine {
    /// Reads the facts that decide whether `workspace`, whose processes
    /// and containers are `footprint`, is idle at `now`, from the process
    /// table `table`.
    fn facts(
        &self,
        workspace: &Workspace,
        footprint: &Footprint,
        table: &[TableEntry],
        now: SystemTime,
    ) -> Result<Facts, MxvFailure> {
        let mut facts = Facts {
            pinned: workspace.pinned,
            modified: None,
            touched: self.runs.touched(&workspace.name),
            users: users(table, &workspace.path, &footprint.pids, std::process::id()),
        };
        if walks(&facts, now, self.stop_idle) {
            facts.modified = self.workspaces.disk_usage(workspace)?.modified;
        }
        Ok(facts)
    }

    /// Measures `workspace` and decides whether it is idle.
    fn measure(&self, workspace: Workspace) -> Result<Measured, MxvFailure> {
        let mut measured = Measured {
            touched: None,
            verdict: None,
            bytes: 0,
            containers: 0,
            workspace,
        };
        let workspace = &measured.workspace;
        if self.runs.running(workspace)?.is_empty() {
            return Ok(measured);
        }
        let footprint = self.runs.footprint(workspace)?;
        let facts = self.facts(workspace, &footprint, &self.table, self.now)?;
        let verdict = idle(&facts, self.now, self.stop_idle);
        if matches!(verdict, Verdict::Idle { .. }) {
            let processes = resident(&self.table, &footprint.pids);
            measured.containers = self.runs.container_memory(&footprint)?;
            measured.bytes = counted(processes, measured.containers, CONTAINERS_RETURN_MEMORY);
        }
        measured.touched = facts.touched;
        measured.verdict = Some(verdict);
        Ok(measured)
    }

    /// Reads again, under the workspace's lock, whether `workspace`, which
    /// the run measured as `measured` and which now runs `running`, is
    /// still idle, and returns why it is not, or `None`.
    fn recheck(
        &self,
        measured: &Measured,
        workspace: &Workspace,
        running: &[String],
    ) -> Result<Option<String>, MxvFailure> {
        let now = SystemTime::now();
        if running.is_empty() {
            return Ok(recheck(None, measured.touched, now, self.stop_idle));
        }
        let table = multiverse::process_table()?;
        let footprint = self.runs.footprint(workspace)?;
        let facts = self.facts(workspace, &footprint, &table, now)?;
        Ok(recheck(Some(&facts), measured.touched, now, self.stop_idle))
    }
}

/// The note that `mxv reclaim` prints after its rows.
struct ReclaimSummary {
    /// What the run had to free.
    need: Need,
    /// The workspaces that the run stopped or cleaned, or would.
    count: usize,
    /// The bytes that the run freed, or would free.
    freed: u64,
    /// Whether the run is a dry run.
    planned: bool,
}

/// Prints what the run stopped or cleaned and how the bytes freed compare
/// with the need.
impl fmt::Display for ReclaimSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (verb, frees, need, what) = match (self.need, self.planned) {
            (Need::Memory(need), false) => ("stopped", "freed", need, "of resident memory"),
            (Need::Memory(need), true) => ("would stop", "would free", need, "of resident memory"),
            (Need::Disk(need), false) => ("cleaned", "freed", need, "on disk"),
            (Need::Disk(need), true) => ("would clean", "would free", need, "on disk"),
        };
        let workspaces = if self.count == 1 {
            "1 workspace".to_owned()
        } else {
            format!("{} workspaces", self.count)
        };
        write!(
            formatter,
            "mxv {verb} {workspaces} and {frees} {} {what}, of {} needed.",
            Size(self.freed),
            Size(need)
        )?;
        if self.freed < need {
            formatter.write_str(" No other idle workspace frees more.")?;
        }
        Ok(())
    }
}

/// Stops idle workspaces until they free the memory that `args` needs, or
/// cleans idle workspaces' build output until it frees the disk, or
/// reports what it would do on a dry run. A workspace that mxv cannot
/// measure, stop or clean becomes a failure of the batch, and the run goes
/// on with the others.
pub(crate) fn reclaim(
    output: &mut Output,
    environment: &Environment,
    args: &ReclaimArgs,
) -> Result<Batch<ReclaimRow, crumbkit::Error>, crumbkit::Error> {
    match args.need {
        Need::Memory(need) => memory(output, environment, args, need),
        Need::Disk(need) => disk(output, environment, args, need),
    }
}

/// Returns the indexes of the idle workspaces of `entries` that a reclaim
/// of `need` bytes stops.
fn chosen(entries: &[Result<Measured, MxvFailure>], need: u64) -> Vec<usize> {
    let idle: Vec<(usize, Option<SystemTime>, u64)> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| match entry {
            Ok(Measured {
                verdict: Some(Verdict::Idle { active, .. }),
                bytes,
                ..
            }) => Some((index, *active, *bytes)),
            _ => None,
        })
        .collect();
    let shapes: Vec<(Option<SystemTime>, u64)> = idle
        .iter()
        .map(|(_, active, bytes)| (*active, *bytes))
        .collect();
    choose(&shapes, need)
        .into_iter()
        .filter_map(|at| idle.get(at).map(|(index, _, _)| *index))
        .collect()
}

/// Stops idle workspaces until their resident memory covers `need`.
fn memory(
    output: &Output,
    environment: &Environment,
    args: &ReclaimArgs,
    need: u64,
) -> Result<Batch<ReclaimRow, crumbkit::Error>, crumbkit::Error> {
    let config = environment.config()?;
    let workspaces = configured(environment)?;
    let records = workspaces.list().map_err(MxvFailure::from)?;
    let machine = Machine {
        runs: environment.runs()?,
        table: multiverse::process_table().map_err(MxvFailure::from)?,
        stop_idle: args.idle.or(config.stop_idle).unwrap_or(DEFAULT_STOP_IDLE),
        now: SystemTime::now(),
        workspaces,
    };
    let mut keys: Vec<String> = Vec::new();
    let mut entries = Vec::new();
    for workspace in records.workspaces {
        keys.push(workspace.name.to_string());
        entries.push(machine.measure(workspace));
    }
    let chosen = chosen(&entries, need);
    let planned = output.is_dry_run();
    let mut batch = Batch::new(&["reclaim"], "names", &keys);
    let mut summary = ReclaimSummary {
        need: Need::Memory(need),
        count: 0,
        freed: 0,
        planned,
    };
    let now = machine.now;
    for (index, entry) in entries.into_iter().enumerate() {
        let measured = match entry {
            Ok(measured) => measured,
            Err(failure) => {
                batch.push(Err(failure.into()));
                continue;
            }
        };
        let row = match &measured.verdict {
            None => Ok(measured.row(
                ReclaimState::Kept,
                0,
                "none of its processes or containers runs".to_owned(),
                now,
            )),
            Some(Verdict::InUse(reason)) => {
                Ok(measured.row(ReclaimState::Kept, 0, reason.clone(), now))
            }
            Some(Verdict::Idle { why, .. }) if chosen.contains(&index) => {
                if planned {
                    summary.count += 1;
                    summary.freed = summary.freed.saturating_add(measured.bytes);
                    Ok(measured.row(ReclaimState::WouldStop, measured.bytes, why.clone(), now))
                } else {
                    stop(environment, &machine, &measured, &mut summary, why)
                }
            }
            Some(Verdict::Idle { .. }) => Ok(measured.row(
                ReclaimState::Kept,
                0,
                if measured.bytes == 0 && measured.containers > 0 {
                    "it is idle, and only its containers hold resident memory, which Docker Desktop's virtual machine does not return to macOS at once".to_owned()
                } else if measured.bytes == 0 {
                    "it is idle, and its processes and containers hold no resident memory that mxv can measure".to_owned()
                } else {
                    "it is idle, and stopping workspaces that were idle longer covers the need"
                        .to_owned()
                },
                now,
            )),
        };
        batch.push(row);
    }
    Ok(batch.note(summary.to_string()))
}

/// Stops the idle workspace `measured` through the stop of `mxv down`,
/// marks it as stopped by reclaim, counts it in `summary`, and returns its
/// row, which keeps it when it became active since the run measured it.
fn stop(
    environment: &Environment,
    machine: &Machine,
    measured: &Measured,
    summary: &mut ReclaimSummary,
    why: &str,
) -> Result<ReclaimRow, crumbkit::Error> {
    let now = SystemTime::now();
    let check = |workspace: &Workspace, running: &[String]| {
        machine
            .recheck(measured, workspace, running)
            .map_err(crumbkit::Error::from)
    };
    if let Err(reason) = processes::reclaim_stop(environment, &measured.workspace, check)? {
        return Ok(measured.row(ReclaimState::Kept, 0, reason, now));
    }
    summary.count += 1;
    summary.freed = summary.freed.saturating_add(measured.bytes);
    Ok(measured.row(ReclaimState::Stopped, measured.bytes, why.to_owned(), now))
}

/// Cleans the build output of idle workspaces until it frees `need` bytes
/// on disk, through the clean of `mxv clean --over-limit`.
fn disk(
    output: &mut Output,
    environment: &Environment,
    args: &ReclaimArgs,
    need: u64,
) -> Result<Batch<ReclaimRow, crumbkit::Error>, crumbkit::Error> {
    let cleaned = clean::run(
        output,
        environment,
        &Run {
            names: &[],
            limit: Limit::Free(need),
            idle: args.idle,
            hold: true,
            ask: None,
        },
    )?;
    let planned = output.is_dry_run();
    let mut batch = Batch::new(&["reclaim"], "names", &cleaned.keys);
    let mut count = 0;
    for result in cleaned.results {
        let row = result.map_err(crumbkit::Error::from).map(|row| {
            let state = match row.state {
                CleanState::Cleaned => ReclaimState::Cleaned,
                CleanState::WouldClean => ReclaimState::WouldClean,
                CleanState::Kept => ReclaimState::Kept,
            };
            if state != ReclaimState::Kept {
                count += 1;
            }
            ReclaimRow {
                name: row.name,
                state,
                freed: row.freed,
                containers: Size(0),
                active: row.modified,
                reason: row.reason.or_else(|| {
                    (state != ReclaimState::Kept).then(|| {
                        "none of its processes or containers runs, no other process works in it, and its checkouts are unchanged for the idle age of mxv clean".to_owned()
                    })
                }),
            }
        });
        batch.push(row);
    }
    let summary = ReclaimSummary {
        need: Need::Disk(need),
        count,
        freed: cleaned.freed,
        planned,
    };
    Ok(batch.note(summary.to_string()))
}

/// A workspace and whether it is pinned, as `mxv pin` and `mxv unpin`
/// print it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PinResult {
    /// The workspace's name.
    name: String,
    /// Whether `mxv reclaim` is kept from stopping the workspace.
    pinned: bool,
}

/// Prints whether reclaim may stop the workspace.
impl fmt::Display for PinResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.pinned {
            write!(
                formatter,
                "The workspace {} is pinned, so mxv reclaim never stops it.",
                self.name
            )
        } else {
            write!(
                formatter,
                "The workspace {} is not pinned, so mxv reclaim may stop it while it is idle.",
                self.name
            )
        }
    }
}

/// Pins or unpins, as `pinned` says, the workspace that `args` names, or
/// else the one that holds the current directory.
pub(crate) fn pin(
    environment: &Environment,
    args: &PinArgs,
    pinned: bool,
) -> Result<Prose<PinResult>, crumbkit::Error> {
    let workspace = processes::set_pinned(environment, args.workspace.as_ref(), pinned)?;
    Ok(Prose::new(PinResult {
        name: workspace.name.to_string(),
        pinned: workspace.pinned,
    }))
}
