//! The verb `sweep`, which archives the workspaces whose change has
//! merged, and the sweep that `mxv new` runs first.
//!
//! The library in [`multiverse`] decides whether a change has merged
//! ([`Workspaces::merge_verdict`]) and advances a board mount. This module
//! archives each merged workspace without a question, because the verdict
//! proved the change merged, measures the space that the archive freed, and
//! prints one row for each workspace that it considered.

use std::fmt;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crumbkit::{Document, Listing, Row, Run};
use multiverse::{GitHub, Verdict, Workspace, WorkspaceState, update_board_mount};
use schemars::JsonSchema;
use serde::Serialize;

use crate::archive::{self, ArchiveArgs};
use crate::cli::{NewArgs, SweepArgs};
use crate::environment::Environment;
use crate::failures::MxvFailure;
use crate::proxy::{self, Served};
use crate::units::Size;
use crate::verbs::{self, configured};

/// The document that `mxv sweep` prints.
pub(crate) const SWEEP: Document<Listing<SweepRow>> = Document::new(
    "mxv",
    "sweep",
    1,
    "Each live workspace that the run considered, whether it archived the workspace, would archive it on a dry run, or kept it for the reason that the row gives, and the bytes that each archive freed.",
)
.when_empty("no live workspace exists");

/// What `mxv sweep` does with one workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SweepState {
    /// The run archived the workspace.
    Archived,
    /// A dry run would archive the workspace.
    WouldArchive,
    /// The run keeps the workspace, for the reason that the row gives.
    Kept,
}

/// One workspace, as `mxv sweep` prints it.
#[derive(Debug, Serialize, JsonSchema, Row)]
pub(crate) struct SweepRow {
    /// The workspace's name.
    // A person copies the name into the next command.
    #[row(column, uncut)]
    pub(crate) name: String,
    /// `archived` when the run archived the workspace, `would_archive` on
    /// a dry run, or `kept`.
    #[row(column)]
    pub(crate) state: SweepState,
    /// The bytes that the workspace's directory took before the archive
    /// deleted it, or take on a dry run, and 0 for a workspace that the run
    /// kept. The archive keeps the ignored files of its checkouts in a tar
    /// outside the directory, so the disk frees a little less.
    #[row(column)]
    pub(crate) freed: Size,
    /// Why the run kept the workspace, or null.
    #[row(column)]
    pub(crate) reason: Option<String>,
    /// Each board mount whose update the run ran, such as
    /// `/repos/multiverse in /Users/you/board`.
    #[row(long)]
    pub(crate) board_mounts: Vec<String>,
    /// Each board mount update that failed, with the command that runs it
    /// again.
    #[row(long)]
    pub(crate) mount_failures: Vec<String>,
}

impl SweepRow {
    /// Returns the row of a workspace that the run keeps.
    fn kept(workspace: &Workspace, reason: String) -> Self {
        Self {
            name: workspace.name.to_string(),
            state: SweepState::Kept,
            freed: Size(0),
            reason: Some(reason),
            board_mounts: Vec::new(),
            mount_failures: Vec::new(),
        }
    }
}

/// The note that `mxv sweep` prints after its rows.
struct Summary {
    /// The workspaces that the run archived, or would archive.
    archived: usize,
    /// The bytes that they took.
    freed: u64,
    /// Whether the run is a dry run.
    planned: bool,
}

impl fmt::Display for Summary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (verb, noun) = if self.planned {
            ("would archive", "would free")
        } else {
            ("archived", "freed")
        };
        let workspaces = if self.archived == 1 {
            "1 workspace".to_owned()
        } else {
            format!("{} workspaces", self.archived)
        };
        write!(
            formatter,
            "mxv {verb} {workspaces} and {noun} {}.",
            Size(self.freed)
        )
    }
}

/// Runs `mxv new`, which archives the merged workspaces first unless
/// `--no-sweep` or a dry run says otherwise, and prints its result.
pub(crate) fn new(output: Run, environment: &Environment, args: &NewArgs) -> ExitCode {
    proxy::serving(
        output,
        |output, served| {
            if !args.no_sweep && !output.is_dry_run() {
                match before_new(output, environment, &args.name, served) {
                    Ok(note) => note.iter().for_each(|text| crate::notice(text)),
                    Err(error) => {
                        crate::warn(&[format!("mxv could not sweep merged workspaces: {error}")])
                    }
                }
            }
            verbs::new(output, environment, args)
        },
        |output, result| crumbkit::finish(output, verbs::NEW, result),
    )
}

/// Runs `mxv sweep` and prints its result.
pub(crate) fn run(output: Run, environment: &Environment, args: &SweepArgs) -> ExitCode {
    proxy::serving(
        output,
        |output, served| sweep(output, environment, args, served),
        |output, result| crumbkit::finish(output, SWEEP, result),
    )
}

/// Archives the merged workspaces, or lists them on a dry run, and prints a
/// row for each live workspace.
pub(crate) fn sweep(
    output: &mut Run,
    environment: &Environment,
    args: &SweepArgs,
    served: &mut Served,
) -> Result<Listing<SweepRow>, crumbkit::Error> {
    let planned = output.is_dry_run();
    let verdicts = verdicts(environment, None, None)?;
    let merged: Vec<&str> = verdicts
        .iter()
        .filter(|(_, verdict)| *verdict == Verdict::Merged)
        .map(|(workspace, _)| workspace.name.as_str())
        .collect();
    if !planned && !merged.is_empty() {
        output.confirm(
            args.yes,
            &format!(
                "Archive the merged workspaces {}? Each archive deletes the workspace's directory, build output and docker volumes.",
                merged.join(", ")
            ),
        )?;
    }
    let (rows, summary) = archive_merged(output, environment, verdicts, planned, served)?;
    Ok(Listing::from_all(rows, None).note(summary.to_string()))
}

/// Archives each merged workspace without a question, as `mxv new` does
/// before it makes a workspace, and returns the note that names what it
/// freed, or `None` when it archived nothing. It keeps the workspace
/// `named`, the name that `mxv new` was given, so that `mxv new` refuses a
/// name that a workspace holds as it does without the sweep.
pub(crate) fn before_new(
    output: &mut Run,
    environment: &Environment,
    named: &str,
    served: &mut Served,
) -> Result<Option<String>, crumbkit::Error> {
    let verdicts = verdicts(environment, Some(BEFORE_NEW_BUDGET), Some(named))?;
    let (rows, summary) = archive_merged(output, environment, verdicts, false, served)?;
    let archived = rows.iter().any(|row| row.state == SweepState::Archived);
    Ok(archived.then(|| summary.to_string()))
}

/// The time after which the sweep before `mxv new` starts no new decision.
/// A decision that has started runs to the end, within the deadlines of its
/// fetch and its `gh` call. A workspace that the sweep reaches after this
/// time stays, and `mxv sweep` decides it later.
const BEFORE_NEW_BUDGET: Duration = Duration::from_secs(45);

/// Returns each live workspace with whether its change has merged. It
/// starts no new decision once `budget` has passed, and it keeps the
/// workspace `named` without deciding it.
fn verdicts(
    environment: &Environment,
    budget: Option<Duration>,
    named: Option<&str>,
) -> Result<Vec<(Workspace, Verdict)>, crumbkit::Error> {
    let started = Instant::now();
    let workspaces = configured(environment)?;
    let records = workspaces.list().map_err(MxvFailure::from)?;
    let mut found = Vec::new();
    for workspace in records
        .workspaces
        .into_iter()
        .filter(|workspace| workspace.state() == WorkspaceState::Live)
    {
        if named == Some(workspace.name.as_str()) {
            found.push((workspace, Verdict::Kept("`mxv new` names it".to_owned())));
            continue;
        }
        if budget.is_some_and(|budget| started.elapsed() > budget) {
            found.push((
                workspace,
                Verdict::Kept("mxv ran out of time to check it; run `mxv sweep`".to_owned()),
            ));
            continue;
        }
        let verdict = workspaces
            .merge_verdict(&workspace, &GitHub)
            .unwrap_or_else(|error| Verdict::Kept(format!("mxv could not read it: {error}")));
        found.push((workspace, verdict));
    }
    Ok(found)
}

/// Archives each merged workspace of `verdicts`, or reports it on a dry
/// run, and returns the rows with their summary.
fn archive_merged(
    output: &mut Run,
    environment: &Environment,
    verdicts: Vec<(Workspace, Verdict)>,
    planned: bool,
    served: &mut Served,
) -> Result<(Vec<SweepRow>, Summary), crumbkit::Error> {
    let workspaces = configured(environment)?;
    let mut rows = Vec::new();
    let mut freed: u64 = 0;
    let mut archived = 0;
    for (workspace, verdict) in verdicts {
        match verdict {
            Verdict::Merged => {}
            Verdict::Kept(reason) => {
                rows.push(SweepRow::kept(&workspace, reason));
                continue;
            }
            _ => {
                rows.push(SweepRow::kept(
                    &workspace,
                    "its merge verdict is unknown".to_owned(),
                ));
                continue;
            }
        }
        let bytes = workspaces
            .disk_usage(&workspace)
            .map_or(0, |usage| usage.bytes);
        if planned {
            freed = freed.saturating_add(bytes);
            archived += 1;
            rows.push(SweepRow {
                name: workspace.name.to_string(),
                state: SweepState::WouldArchive,
                freed: Size(bytes),
                reason: None,
                board_mounts: Vec::new(),
                mount_failures: Vec::new(),
            });
            continue;
        }
        let args = ArchiveArgs::unattended(workspace.name.as_str());
        if let Err(error) = archive::archive(output, environment, &args, served) {
            rows.push(SweepRow::kept(
                &workspace,
                format!("the archive refused: {error}"),
            ));
            continue;
        }
        freed = freed.saturating_add(bytes);
        archived += 1;
        let mut row = SweepRow {
            name: workspace.name.to_string(),
            state: SweepState::Archived,
            freed: Size(bytes),
            reason: None,
            board_mounts: Vec::new(),
            mount_failures: Vec::new(),
        };
        for mount in &workspace.board_mounts {
            let text = format!("{} in {}", mount.path, mount.board.display());
            match update_board_mount(mount) {
                Ok(()) => row.board_mounts.push(text),
                Err(why) => row.mount_failures.push(format!(
                    "{text}: {why}; run `board -C {} mount update {}` to retry",
                    mount.board.display(),
                    mount.path
                )),
            }
        }
        crate::warn(&row.mount_failures);
        rows.push(row);
    }
    Ok((
        rows,
        Summary {
            archived,
            freed,
            planned,
        },
    ))
}
