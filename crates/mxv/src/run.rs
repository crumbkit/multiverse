//! `mxv run`, which runs a task of a workspace through deadband and passes
//! its exit code on, or lists the tasks when it names none.
//!
//! A task is declared in a manifest, or is a mise task of a checkout, which
//! [`TaskRef`] tells apart. The lookup reads every manifest and asks mise
//! about every checkout before anything runs, so a failure of the lookup
//! leaves nothing running.

use std::fmt;
use std::path::PathBuf;
use std::process::{ExitCode, ExitStatus};

use crumbkit::{Call, Document, Failure, FieldValue, Recovery, Row, Run, Sink};
use multiverse::{MiseListing, MiseTaskName, Task, TaskEnd, TaskHome, TaskName, TaskServices};
use multiverse::{Store, TaskStart, Workspace};
use schemars::JsonSchema;
use serde::Serialize;

use crate::admission::Deadband;
use crate::cli::RunArgs;
use crate::environment::Environment;
use crate::exec::{ExecFailure, exit_number};
use crate::failures::MxvFailure;
use crate::processes::{self, Context};
use crate::run_failures::{RunFailure, TASK_AMBIGUOUS, TASK_UNKNOWN};

#[cfg(test)]
mod tests;

/// The document that `mxv run` prints when it names no task.
pub(crate) const RUN: Document<Vec<TaskRow>> = Document::new(
    "mxv",
    "run",
    1,
    "Each task of the workspace: those that a manifest declares, then those that mise lists, by checkout.",
)
.when_empty("the workspace's manifests declare no task, and mise lists none");

/// A failure of `mxv run` to choose its task.
#[derive(Debug, Failure)]
pub(crate) enum TaskFailure {
    /// The workspace has no task with the name.
    #[failure(
        code = TASK_UNKNOWN,
        recovery = Recovery::Run(Call::new(&["run"]).value("workspace", workspace)),
        field(
            workspace = workspace,
            task = task,
            names = FieldValue::list(names.iter())
        )
    )]
    Unknown {
        /// The workspace's name.
        workspace: String,
        /// The name that the command line gave.
        task: String,
        /// The names of the tasks that the workspace has.
        names: Vec<String>,
        /// Whether mise could not be asked about a checkout, so the names
        /// may lack its tasks.
        mise_unasked: bool,
    },
    /// Several checkouts hold a task with the name.
    #[failure(
        code = TASK_AMBIGUOUS,
        recovery = Recovery::hint(format!(
            "choose a checkout with --repo, such as `mxv run -w {workspace} --repo {} {task}`",
            checkouts.first().map_or("CHECKOUT", String::as_str)
        )),
        field(
            workspace = workspace,
            task = task,
            checkouts = FieldValue::list(checkouts.iter())
        )
    )]
    Ambiguous {
        /// The workspace's name.
        workspace: String,
        /// The name that the command line gave.
        task: String,
        /// The folders of the checkouts that hold it.
        checkouts: Vec<String>,
    },
    /// The caller passed `--json`, and a task's output is the task's own.
    #[failure(
        code = crumbkit::codes::CLI_USAGE,
        hint = "run the command without --json to read the task's output",
        field(argument = "--json")
    )]
    JsonRefused,
}

impl fmt::Display for TaskFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown {
                workspace,
                task,
                names,
                mise_unasked,
            } => {
                write!(
                    formatter,
                    "the workspace {workspace} has no task named {task}"
                )?;
                if names.is_empty() {
                    formatter.write_str(", and it has no tasks")?;
                } else {
                    write!(formatter, "; its tasks are {}", names.join(", "))?;
                }
                if *mise_unasked {
                    formatter.write_str(
                        "; mxv could not ask mise for a checkout's tasks, because mise is not installed or the checkout's directory is gone",
                    )?;
                }
                Ok(())
            }
            Self::Ambiguous {
                task, checkouts, ..
            } => write!(
                formatter,
                "the task {task} is in several checkouts: {}",
                checkouts.join(", ")
            ),
            Self::JsonRefused => formatter.write_str(
                "--json is not available, because mxv run hands the terminal to the task",
            ),
        }
    }
}

impl std::error::Error for TaskFailure {}

/// Why `--fields` is refused for a task.
const TEXT_ONLY: &str = "mxv run hands the terminal to the task";

/// The source that a row names for a declared task.
const DECLARED: &str = "mxv.toml";

/// The source that a row names for a mise task.
const MISE: &str = "mise";

/// One task of a workspace, as `mxv run` lists it.
#[derive(Debug, Serialize, JsonSchema, PartialEq, Eq)]
pub(crate) struct TaskRow {
    /// The name that `mxv run` takes.
    name: String,
    /// Where the task comes from: `mxv.toml` or `mise`.
    source: String,
    /// The folder of the checkout that holds it, or null for the workspace
    /// manifest of a workspace with several checkouts.
    checkout: Option<String>,
    /// Whether it takes a place from deadband.
    gate: bool,
}

impl Row for TaskRow {
    const COLUMNS: &'static [&'static str] = &["name", "source", "checkout", "gate"];
    // A person copies the name into the next command.
    const UNCUT_COLUMNS: &'static [&'static str] = &["name"];
}

/// A task that the lookup found. It is the only key that the lookup
/// returns.
#[derive(Clone, Debug, PartialEq, Eq)]
enum TaskRef<'a> {
    /// A task under `[tasks.<name>]` of a manifest, with the task.
    Declared(TaskName, &'a Task),
    /// A task that mise listed.
    Mise(MiseTaskName),
}

/// One place that holds tasks, with what mise said about it.
#[derive(Debug)]
struct Home {
    /// The manifest's tasks and where they run.
    tasks: TaskHome,
    /// What mise listed. A workspace manifest of several checkouts has no
    /// directory of its own that mise reads, so it lists nothing.
    mise: MiseListing,
}

impl Home {
    /// Returns the name of the task in this place that `name` refers to, a
    /// declared one first, and whether mise lists the name as well.
    fn find(&self, name: &str) -> Option<(TaskRef<'_>, bool)> {
        let mise = MiseTaskName::new(name).filter(|name| match &self.mise {
            MiseListing::Listed(names) => names.contains(name),
            MiseListing::Missing | MiseListing::Refused { .. } => false,
        });
        match TaskName::new(name).and_then(|name| self.tasks.tasks.get_key_value(&name)) {
            Some((declared, task)) => {
                Some((TaskRef::Declared(declared.clone(), task), mise.is_some()))
            }
            None => mise.map(|mise| (TaskRef::Mise(mise), false)),
        }
    }

    /// Returns the rows of this place: its declared tasks, then each mise
    /// task that no declared task hides.
    fn rows(&self) -> Vec<TaskRow> {
        let checkout = &self.tasks.checkout;
        let declared = self.tasks.tasks.iter().map(|(name, task)| TaskRow {
            name: name.to_string(),
            source: DECLARED.to_owned(),
            checkout: checkout.clone(),
            gate: task.gate,
        });
        let listed: &[MiseTaskName] = match &self.mise {
            MiseListing::Listed(names) => names,
            MiseListing::Missing | MiseListing::Refused { .. } => &[],
        };
        let mise = listed
            .iter()
            .filter(|name| {
                TaskName::new(name.as_str())
                    .is_none_or(|name| !self.tasks.tasks.contains_key(&name))
            })
            .map(|name| TaskRow {
                name: name.to_string(),
                source: MISE.to_owned(),
                checkout: checkout.clone(),
                gate: true,
            });
        declared.chain(mise).collect()
    }
}

/// Returns the sentence that says mise refused to list the tasks of `home`,
/// or `None`.
fn refusal(home: &Home) -> Option<String> {
    let MiseListing::Refused { message } = &home.mise else {
        return None;
    };
    let place = home
        .tasks
        .checkout
        .as_deref()
        .unwrap_or("the workspace manifest");
    Some(format!(
        "mise did not list the tasks of {place}, so mxv offers none of them: {message}"
    ))
}

/// Returns every place that holds the workspace's tasks, or the checkout
/// that `repo` names when it names one, asks mise about each checkout among
/// them, and prints mise's refusals on standard error.
fn homes(context: &Context, repo: Option<&String>) -> Result<Vec<Home>, crumbkit::Error> {
    let tasks = context
        .workspace
        .task_homes()
        .map_err(RunFailure::Manifest)?;
    let homes: Vec<Home> = narrowed(&context.workspace, tasks, repo)?
        .into_iter()
        .map(|tasks| {
            let mise = if tasks.checkout.is_some() {
                context.runs.mise_tasks(&tasks.root)
            } else {
                MiseListing::Listed(Vec::new())
            };
            Home { tasks, mise }
        })
        .collect();
    let refusals: Vec<String> = homes.iter().filter_map(refusal).collect();
    crate::warn(&refusals);
    Ok(homes)
}

/// Keeps the home of the checkout that `--repo` names, when it names one.
pub(crate) fn narrowed(
    workspace: &Workspace,
    homes: Vec<TaskHome>,
    repo: Option<&String>,
) -> Result<Vec<TaskHome>, MxvFailure> {
    let Some(repo) = repo else {
        return Ok(homes);
    };
    let kept: Vec<TaskHome> = homes
        .into_iter()
        .filter(|home| home.checkout.as_ref() == Some(repo))
        .collect();
    if kept.is_empty() {
        return Err(MxvFailure::CheckoutNotFound {
            name: workspace.name.to_string(),
            checkout: repo.clone(),
            checkouts: workspace
                .checkouts
                .iter()
                .map(|checkout| checkout.name.clone())
                .collect(),
        });
    }
    Ok(kept)
}

/// A task that the lookup found, and the place that holds it.
#[derive(Debug)]
struct Found<'a> {
    /// The task.
    reference: TaskRef<'a>,
    /// The place.
    home: &'a Home,
    /// A sentence that says the lookup chose it over another.
    note: Option<String>,
}

/// Returns the task of `homes` that `name` refers to.
///
/// In one place a declared task wins a clash with a mise task, with a note.
/// The workspace manifest of a workspace with several checkouts holds tasks
/// for the whole workspace, so one of its tasks wins over a checkout's,
/// with a note. A name that several checkouts hold is refused.
fn resolve<'a>(workspace: &str, homes: &'a [Home], name: &str) -> Result<Found<'a>, TaskFailure> {
    let hits: Vec<(&Home, TaskRef<'a>, bool)> = homes
        .iter()
        .filter_map(|home| home.find(name).map(|(found, both)| (home, found, both)))
        .collect();
    let shared = hits.iter().find(|(home, ..)| home.tasks.checkout.is_none());
    let chosen = match (shared, hits.as_slice()) {
        (Some(shared), _) => shared,
        (None, [only]) => only,
        (None, []) => {
            return Err(TaskFailure::Unknown {
                workspace: workspace.to_owned(),
                task: name.to_owned(),
                names: names(homes),
                mise_unasked: homes
                    .iter()
                    .any(|home| matches!(home.mise, MiseListing::Missing)),
            });
        }
        (None, several) => {
            return Err(TaskFailure::Ambiguous {
                workspace: workspace.to_owned(),
                task: name.to_owned(),
                checkouts: several
                    .iter()
                    .filter_map(|(home, ..)| home.tasks.checkout.clone())
                    .collect(),
            });
        }
    };
    let (home, reference, both) = chosen;
    let hidden: Vec<&str> = hits
        .iter()
        .filter_map(|(home, ..)| home.tasks.checkout.as_deref())
        .collect();
    let note = if shared.is_some() && !hidden.is_empty() {
        Some(format!(
            "{name} is in the workspace manifest and in {}, so mxv runs the one in the workspace manifest; --repo runs a checkout's",
            hidden.join(", ")
        ))
    } else {
        both.then(|| {
            format!(
                "{name} is declared in {DECLARED} and listed by mise, so mxv runs the one in {DECLARED}"
            )
        })
    };
    Ok(Found {
        reference: reference.clone(),
        home,
        note,
    })
}

/// Returns every task name of `homes`, once each, in name order.
fn names(homes: &[Home]) -> Vec<String> {
    let mut names: Vec<String> = homes
        .iter()
        .flat_map(Home::rows)
        .map(|row| row.name)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Returns what starts the task `found` with `args`, and whether it takes
/// a place from deadband.
fn start_of(found: &Found<'_>, args: &[String]) -> (TaskStart, bool) {
    let root: PathBuf = found.home.tasks.root.clone();
    let (start, gate) = match &found.reference {
        TaskRef::Mise(name) => (TaskStart::mise(root, name, args), true),
        TaskRef::Declared(_, task) => {
            let mut argv = task.argv.words().to_vec();
            argv.extend_from_slice(args);
            (
                TaskStart::new(
                    root,
                    task.dir.clone(),
                    argv,
                    TaskServices::Needs(task.needs.clone()),
                ),
                task.gate,
            )
        }
    };
    (
        start.with_own_ports(found.home.tasks.own_ports.clone()),
        gate,
    )
}

/// Lists the tasks of the workspace that `args` names.
fn list(environment: &Environment, args: &RunArgs) -> Result<Vec<TaskRow>, crumbkit::Error> {
    let context = processes::context(environment, args.workspace.as_ref(), &[])?;
    processes::refuse_unfinished(&context.workspace)?;
    let homes = homes(&context, args.repo.as_ref())?;
    Ok(homes.iter().flat_map(Home::rows).collect())
}

/// Runs the task `name` that `args` names. It returns the task's exit
/// status and whether mxv caught a signal while it ran.
fn run_task(
    output: &Run,
    environment: &Environment,
    args: &RunArgs,
    name: &str,
) -> Result<(ExitStatus, bool), crumbkit::Error> {
    let mut context = processes::context(environment, args.workspace.as_ref(), &[])?;
    processes::refuse_unfinished(&context.workspace)?;
    let workspace = context.workspace.name.to_string();
    let homes = homes(&context, args.repo.as_ref())?;
    let found = resolve(&workspace, &homes, name)?;
    if output.is_json() {
        return Err(TaskFailure::JsonRefused.into());
    }
    crate::warn(&found.note.iter().cloned().collect::<Vec<_>>());
    let (start, gate) = start_of(&found, args.task_words());
    match run_start(&mut context, environment, start, gate)? {
        (TaskEnd::Exited(status), caught) => Ok((status, caught)),
        (TaskEnd::Unstartable { reason }, _) => Err(ExecFailure::Unstartable {
            workspace,
            process: name.to_owned(),
            reason,
        }
        .into()),
    }
}

/// Gives the workspace of `context` its ports, runs `start` on the host,
/// through deadband when `gate` is true, and returns how it ended and
/// whether mxv caught a signal while it ran.
pub(crate) fn run_start(
    context: &mut Context,
    environment: &Environment,
    start: TaskStart,
    gate: bool,
) -> Result<(TaskEnd, bool), crumbkit::Error> {
    // The lock covers the ports and the plan. It ends before the task
    // starts, so a long test run never stops `up` or `down`.
    let lock = context.lock_live()?;
    context.mark(&lock);
    let manifest = context
        .workspace
        .manifest()
        .map_err(RunFailure::Manifest)?
        .unwrap_or_default();
    let store = Store::new(environment.records()?);
    let runs = &context.runs;
    let record = &mut context.workspace;
    let prepared = runs
        .prepare_task(&lock, &store, record, &manifest, start)
        .map_err(MxvFailure::from)?;
    drop(lock);
    let prepared = prepared.map_err(|unrunnable| RunFailure::Unrunnable {
        workspace: record.name.to_string(),
        unit: unrunnable.unit,
        message: unrunnable.message,
    })?;
    // A person who runs a task in a workspace is using it, so mxv touches
    // it while the task runs.
    let _toucher = runs.keep_touched(&record.name, processes::TOUCH_EVERY);
    let mut admission = Deadband::for_task(environment, gate);
    let terminal = crate::stdin_is_terminal() && crate::stdout_is_terminal();
    let ended = prepared
        .run(&mut admission, terminal)
        .map_err(MxvFailure::from)?;
    Ok((ended, admission.caught().is_some()))
}

/// Runs the task that `args` names and returns its exit code, or prints
/// the workspace's tasks when `args` names none.
pub(crate) fn run(mut output: Run, environment: &Environment, args: &RunArgs) -> ExitCode {
    let Some(name) = args.task() else {
        return crumbkit::finish(output, RUN, list(environment, args));
    };
    let mut sink = Sink::standard();
    if let Some(code) = output.refuse_fields_for_text(&mut sink, TEXT_ONLY) {
        return code;
    }
    match run_task(&output, environment, args, name) {
        Ok((status, caught)) => {
            // Only a signal that mxv caught counts as an interruption, so a
            // task that crashed with a signal of its own does not.
            if caught {
                crumbkit::events::interrupted();
            }
            crumbkit::events::ended(exit_number(status), None)
        }
        Err(error) => output.fail(&mut sink, error.failure()),
    }
}
