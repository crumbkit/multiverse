//! The process verbs `up`, `down`, `restart`, `status` and `logs`, which
//! run a workspace's app from the `mxv.toml` of its checkout, and the
//! documents they print.
//!
//! Each verb works on the workspace that `-w/--workspace` names, or else on
//! the one that holds the current directory. Its positional words name
//! processes.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crumbkit::{Batch, Document, Listing, Output, Stream};
use multiverse::phase::WorkspaceId;
use multiverse::{
    BuildEnd, BuildStop, BuildsPassed, DownOutcome, Error, Git, Lock, Manifest, ManifestError,
    ProcessStatus, Reclaimed, Runs, Store, Unstoppable, UpOutcome, Volumes, Workspace,
    WorkspaceName,
};
use std::time::Duration;

use crate::admission::Deadband;
use crate::cli::{LogsArgs, PortsResetArgs, ProcessesArgs, StatusArgs};
use crate::environment::Environment;
use crate::failures::MxvFailure;
use crate::rows::{CheckoutStateRow, LogLine, PortRow, ProcessRow, StatusSummary};
use crate::run_failures::RunFailure;
use crate::verbs;

/// The document that `mxv up` prints.
pub(crate) const UP: Document<Vec<ProcessRow>> = Document::new(
    "mxv",
    "up",
    1,
    "The selected units and their dependencies, in the order they started, each running.",
);

/// The document that `mxv down` prints.
pub(crate) const DOWN: Document<Batch<ProcessRow, RunFailure>> = Document::new(
    "mxv",
    "down",
    1,
    "The units that the run stopped, in the order it stopped them, and a failure for each process group that outlived SIGKILL.",
);

/// The document that `mxv restart` prints.
pub(crate) const RESTART: Document<Vec<ProcessRow>> = Document::new(
    "mxv",
    "restart",
    1,
    "The restarted units and their dependencies, in the order they started, each running.",
);

/// The document that `mxv status` prints.
pub(crate) const STATUS: Document<Listing<ProcessRow, StatusSummary>> = Document::new(
    "mxv",
    "status",
    1,
    "Each unit of the workspace's manifest in start order, then each other unit that its run record holds, with its state, and the manifest's error when it does not parse.",
)
.when_empty("the workspace has no manifest that declares units, and its run record holds no unit");

/// The document that `mxv logs` prints.
pub(crate) const LOGS: Document<Stream<LogLine>> = Document::new(
    "mxv",
    "logs",
    1,
    "The last lines of a unit's log, then with --follow each line that the unit writes.",
);

/// The document that `mxv ports reset` prints.
pub(crate) const PORTS_RESET: Document<Vec<PortRow>> = Document::new(
    "mxv",
    "ports-reset",
    1,
    "Each port of the workspace's manifest and the host port that it holds after the reset.",
);

/// A workspace that a process verb works on, with the store that holds its
/// record and the run files of its processes.
pub(crate) struct Context {
    /// The store of the workspace records.
    store: Store,
    /// The run files of every workspace's processes.
    pub(crate) runs: Runs,
    /// The workspace.
    pub(crate) workspace: Workspace,
    /// The processes that the command line names after the workspace.
    processes: Vec<String>,
}

impl Context {
    /// Narrows the mark that `mxv reclaim` left on the workspace once
    /// `up`, `restart` or `down` has started or stopped the processes
    /// `names`, or every process when `names` is empty, while the command
    /// holds the workspace's lock. The mark keeps the processes that are
    /// still to start.
    fn forget_reclaim(&mut self, names: &[String]) -> Result<(), MxvFailure> {
        let Some(mark) = &self.workspace.reclaimed else {
            return Ok(());
        };
        let left = mark.without(names);
        if left.as_ref() != Some(mark) {
            self.workspace.reclaimed = left;
            self.store.update(&mut self.workspace)?;
        }
        Ok(())
    }

    /// Takes the workspace's lock, which `up`, `down` and `restart` hold
    /// while they start or stop its processes, then reads the workspace's
    /// record again, so the ports are the ones that a command which held
    /// the lock before stored.
    pub(crate) fn lock(&mut self) -> Result<Lock, MxvFailure> {
        let lock = self.runs.lock(&self.workspace.name)?;
        if let Some(fresh) = self.store.read(&self.workspace.name)? {
            self.workspace = fresh;
        }
        Ok(lock)
    }
}

/// Returns the context of the workspace that `-w` names, or else of the
/// one that holds the current directory, and the processes `processes`.
pub(crate) fn context(
    environment: &Environment,
    workspace: Option<&String>,
    processes: &[String],
) -> Result<Context, crumbkit::Error> {
    let store = Store::new(environment.records()?);
    let runs = environment
        .runs()?
        .with_port_range(environment.port_range()?);
    let workspace = match workspace {
        Some(text) => {
            let name = verbs::parse_name(text)?;
            store
                .read(&name)
                .map_err(MxvFailure::from)?
                .ok_or_else(|| MxvFailure::NotFound { name: text.clone() })?
        }
        None => current(&store, environment)?.ok_or(RunFailure::NotCurrent)?,
    };
    touch(&runs, &workspace.name);
    Ok(Context {
        store,
        runs,
        workspace,
        processes: processes.to_vec(),
    })
}

/// How often `mxv exec` and `mxv logs --follow` touch the workspace again
/// while they run, well inside the shortest idle age that `mxv reclaim`
/// accepts.
pub(crate) const TOUCH_EVERY: Duration = Duration::from_secs(5 * 60);

/// Records that an mxv command named the workspace `name` now, which
/// `mxv reclaim` reads as activity. A touch that cannot be written changes
/// nothing the command does, so its failure is ignored, and the workspace
/// then reads as last named earlier.
fn touch(runs: &Runs, name: &WorkspaceName) {
    let _ = runs.touch(name);
}

/// Records that an mxv command named the workspace `name` now, as
/// [`touch`] does, for the verbs that read a workspace without a process
/// context, such as `show` and `cd`.
pub(crate) fn touch_named(environment: &Environment, name: &WorkspaceName) {
    if let Ok(runs) = environment.runs() {
        touch(&runs, name);
    }
}

/// Returns the workspace whose directory holds the current directory, or
/// `None`.
pub(crate) fn current(
    store: &Store,
    environment: &Environment,
) -> Result<Option<Workspace>, MxvFailure> {
    let Some(dir) = environment.current_dir() else {
        return Ok(None);
    };
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    Ok(store.list()?.workspaces.into_iter().find(|workspace| {
        let path = workspace
            .path
            .canonicalize()
            .unwrap_or_else(|_| workspace.path.clone());
        dir.starts_with(path)
    }))
}

/// Returns the checkout that holds the workspace's manifest. A workspace
/// holds one checkout for now.
fn checkout(workspace: &Workspace) -> &Path {
    workspace
        .checkouts
        .first()
        .map_or(workspace.path.as_path(), |checkout| checkout.path.as_path())
}

/// Returns the one manifest that the workspace reads, the workspace
/// manifest or else the checkout's `mxv.toml`, or `None` when it has
/// neither.
fn manifest(workspace: &Workspace) -> Result<Option<Manifest>, RunFailure> {
    workspace.manifest().map_err(RunFailure::Manifest)
}

/// Returns the workspace's manifest, or `None` when it has none or it does
/// not parse, for `down` and `status`, which then work from the run records
/// so a broken manifest never keeps processes running.
fn lenient_manifest(workspace: &Workspace) -> Option<Manifest> {
    workspace.manifest().ok().flatten()
}

/// Returns the workspace's manifest as [`lenient_manifest`] does, and the
/// manifest's error when it cannot be read or does not parse.
fn manifest_or_error(workspace: &Workspace) -> (Option<Manifest>, Option<ManifestError>) {
    match workspace.manifest() {
        Ok(manifest) => (manifest, None),
        Err(error) => (None, Some(error)),
    }
}

/// Returns the sentence for each process of `manifest` that compiles
/// inside its `argv`, which would hold a place on the machine for the
/// server's whole life if mxv gave it one, so mxv gives it none.
fn cargo_run_warnings(manifest: &Manifest) -> impl Iterator<Item = String> + '_ {
    manifest
        .processes
        .iter()
        .filter(|(_, process)| process.argv.get(..2) == Some(&["cargo".to_owned(), "run".to_owned()]))
        .map(|(name, _)| {
            format!(
                "the process `{name}` compiles inside its argv with `cargo run`, which deadband does not limit, so run the built program in argv and put `cargo build` in a `build` step"
            )
        })
}

/// Returns the warnings that `up` and `restart` print for `manifest`, the
/// manifest of `workspace`: each table that mxv ignores although its name
/// is close to one of its own, each process whose `argv` is `cargo run`,
/// and a manifest that declares nothing to start. A manifest of services
/// alone starts their backends.
fn start_warnings(workspace: &Workspace, manifest: &Manifest) -> Vec<String> {
    let mut warnings = manifest.warnings.clone();
    warnings.extend(cargo_run_warnings(manifest));
    if manifest.processes.is_empty()
        && manifest.containers.is_empty()
        && manifest.services.is_empty()
    {
        let path = workspace
            .manifest_inputs()
            .chosen()
            .map_or_else(|| workspace.manifest_file(), Path::to_path_buf);
        warnings.push(format!(
            "the manifest {} declares no units and no services, so mxv started nothing",
            path.display()
        ));
    }
    warnings
}

/// Returns the failure of a workspace whose record names a checkout that
/// an `add` stopped before it finished, for the commands that act on the
/// manifest. Its manifest may count the unfinished checkout, which never
/// ran, so they refuse until the `add` resumes.
pub(crate) fn refuse_unfinished(workspace: &Workspace) -> Result<(), MxvFailure> {
    match workspace.unfinished_checkout() {
        Some(checkout) => Err(MxvFailure::Incomplete {
            name: workspace.name.to_string(),
            folder: checkout.name.clone(),
            repository: checkout.source.clone(),
        }),
        None => Ok(()),
    }
}

/// Returns the workspace's manifest, or the failure of a workspace with an
/// unfinished checkout or of a checkout without a manifest.
pub(crate) fn required_manifest(workspace: &Workspace) -> Result<Manifest, crumbkit::Error> {
    refuse_unfinished(workspace)?;
    Ok(
        manifest(workspace)?.ok_or_else(|| RunFailure::ManifestMissing {
            checkout: checkout(workspace).to_path_buf(),
            workspace: workspace.manifest_file(),
        })?,
    )
}

/// The signal that interrupted a command's builds, which ends mxv with
/// 128 plus its number and prints no document.
#[derive(Debug)]
pub(crate) struct Interrupted {
    /// The signal's number.
    signal: i32,
}

/// Prints `result` as [`crumbkit::finish`] does, or ends with 128 plus the
/// signal that interrupted the command's builds, and returns the exit code.
pub(crate) fn finish_interruptible(
    output: Output,
    document: Document<Vec<ProcessRow>>,
    result: Result<Result<Vec<ProcessRow>, Interrupted>, crumbkit::Error>,
) -> ExitCode {
    match result {
        Ok(Err(Interrupted { signal })) => {
            crumbkit::events::interrupted();
            crumbkit::events::ended(u8::try_from(128 + signal).unwrap_or(u8::MAX), None)
        }
        Ok(Ok(rows)) => crumbkit::finish(output, document, Ok::<_, crumbkit::Error>(rows)),
        Err(error) => crumbkit::finish(output, document, Err::<Vec<ProcessRow>, _>(error)),
    }
}

/// Runs the builds of the context's processes through deadband, and
/// returns the proof that they passed, the signal that interrupted them,
/// or the failure of the first one that did not pass. Nothing has stopped
/// or started when it returns.
fn build<'lock>(
    environment: &Environment,
    context: &mut Context,
    lock: &'lock Lock,
    manifest: &Manifest,
) -> Result<Result<BuildsPassed<'lock>, Interrupted>, crumbkit::Error> {
    // The warnings come first, so a long or failing build cannot hide them.
    crate::warn(&start_warnings(&context.workspace, manifest));
    let workspace = context.workspace.name.to_string();
    let mut admission = Deadband::new(environment);
    let end = context
        .runs
        .build(
            lock,
            &context.store,
            &mut context.workspace,
            manifest,
            &context.processes,
            &mut admission,
        )
        .map_err(MxvFailure::from)?;
    match end {
        BuildEnd::Passed(passed) => Ok(Ok(passed)),
        BuildEnd::Stopped { process, why } => match why {
            BuildStop::Failed { code, signal } => Err(RunFailure::BuildFailed {
                workspace,
                process,
                code,
                signal,
            }
            .into()),
            BuildStop::Unstartable { reason } => Err(RunFailure::BuildUnstartable {
                workspace,
                process,
                reason,
            }
            .into()),
            BuildStop::TimedOut { waited } => Err(RunFailure::BuildTimedOut {
                workspace,
                process,
                seconds: waited.as_secs(),
            }
            .into()),
            BuildStop::Interrupted => Ok(Err(Interrupted {
                // Only the signal thread sets the flag that ended the build,
                // so a signal was caught. SIGINT stands in for an answer that
                // cannot be missing.
                signal: admission.caught().unwrap_or(2),
            })),
        },
        BuildEnd::UnknownProcess(process) => {
            Err(RunFailure::UnknownProcess { workspace, process }.into())
        }
        BuildEnd::Unrunnable(unrunnable) => Err(RunFailure::Unrunnable {
            workspace,
            unit: unrunnable.unit,
            message: unrunnable.message,
        }
        .into()),
    }
}

/// Starts the processes that `args` names, and their dependencies, after
/// their builds pass.
pub(crate) fn up(
    environment: &Environment,
    args: &ProcessesArgs,
) -> Result<Result<Vec<ProcessRow>, Interrupted>, crumbkit::Error> {
    let mut context = context(environment, args.workspace.as_ref(), &args.processes)?;
    let lock = context.lock()?;
    if context.processes.is_empty()
        && let Some(reclaimed) = &context.workspace.reclaimed
        && let Ok(Some(manifest)) = context.workspace.manifest()
    {
        // `mxv reclaim` recorded what ran, so `up` starts the same
        // processes again. A name that the manifest no longer declares is
        // left out.
        context.processes = reclaimed
            .processes
            .iter()
            .filter(|name| manifest.unit(name).is_some())
            .cloned()
            .collect();
    }
    let manifest = required_manifest(&context.workspace)?;
    let passed = match build(environment, &mut context, &lock, &manifest)? {
        Ok(passed) => passed,
        Err(interrupted) => return Ok(Err(interrupted)),
    };
    start(&mut context, passed, &manifest, &args.processes).map(Ok)
}

/// Starts the context's processes, and returns their rows or the failure
/// of the first one that does not start. `passed` proves that their builds
/// passed. `named` is what the command line named, which narrows the
/// reclaim mark once they start.
fn start(
    context: &mut Context,
    passed: BuildsPassed<'_>,
    manifest: &Manifest,
    named: &[String],
) -> Result<Vec<ProcessRow>, crumbkit::Error> {
    let workspace = context.workspace.name.to_string();
    let mut earlier = Vec::new();
    let outcome = context.runs.up(
        passed,
        &context.store,
        &mut context.workspace,
        manifest,
        &context.processes,
        &mut earlier,
    );
    let messages: Vec<String> = earlier
        .iter()
        .map(|note| note.message().to_owned())
        .collect();
    crate::warn(&messages);
    match outcome.map_err(MxvFailure::from)? {
        UpOutcome::Up(statuses) => {
            context.forget_reclaim(named)?;
            Ok(statuses.iter().map(ProcessRow::from).collect())
        }
        UpOutcome::Left {
            unstoppable,
            not_started,
        } => Err(RunFailure::Unstoppable {
            process: unstoppable.process,
            pid: unstoppable.pid,
            unverified: unstoppable.unverified,
            not_started,
        }
        .into()),
        UpOutcome::Unrunnable(unrunnable) => Err(RunFailure::Unrunnable {
            workspace,
            unit: unrunnable.unit,
            message: unrunnable.message,
        }
        .into()),
        UpOutcome::UnknownProcess(process) => {
            Err(RunFailure::UnknownProcess { workspace, process }.into())
        }
        UpOutcome::PortTaken {
            process,
            port,
            number,
            holders,
        } => Err(RunFailure::PortTaken {
            workspace,
            process,
            port,
            number,
            holders,
        }
        .into()),
        UpOutcome::Exited {
            process,
            code,
            signal,
            log,
        } => Err(RunFailure::Exited {
            workspace,
            process,
            code,
            signal,
            log: log.map(PathBuf::into_boxed_path),
        }
        .into()),
        UpOutcome::ForeignListener {
            process,
            port,
            number,
            pid,
            log,
        } => Err(RunFailure::ForeignListener {
            workspace,
            process,
            port,
            number,
            pid,
            log: log.map(PathBuf::into_boxed_path),
        }
        .into()),
        UpOutcome::DetachedListener {
            process,
            port,
            number,
            pid,
            log,
        } => Err(RunFailure::DetachedListener {
            workspace,
            process,
            port,
            number,
            pid,
            log: log.map(PathBuf::into_boxed_path),
        }
        .into()),
        UpOutcome::NotReady {
            process,
            waited,
            timeout,
            log,
        } => Err(RunFailure::NotReady {
            workspace,
            process,
            waited: Box::new(waited),
            seconds: timeout.as_secs(),
            log: log.map(PathBuf::into_boxed_path),
        }
        .into()),
    }
}

/// Stops the context's processes, and returns one result for each, in
/// the order they stopped.
fn stop(
    context: &Context,
    lock: &Lock,
    manifest: Option<&Manifest>,
) -> Result<Vec<Result<ProcessStatus, Unstoppable>>, crumbkit::Error> {
    let outcome = context
        .runs
        .down(lock, &context.workspace, manifest, &context.processes)
        .map_err(MxvFailure::from)?;
    match outcome {
        DownOutcome::Down(results) => Ok(results),
        DownOutcome::UnknownProcess(process) => Err(RunFailure::UnknownProcess {
            workspace: context.workspace.name.to_string(),
            process,
        }
        .into()),
    }
}

/// Stops the context's processes, and fails with the first process group
/// that outlived SIGKILL.
fn stop_every(
    context: &Context,
    lock: &Lock,
    manifest: Option<&Manifest>,
) -> Result<(), crumbkit::Error> {
    for result in stop(context, lock, manifest)? {
        if let Err(unstoppable) = result {
            return Err(RunFailure::Unstoppable {
                process: unstoppable.process,
                pid: unstoppable.pid,
                unverified: unstoppable.unverified,
                not_started: Vec::new(),
            }
            .into());
        }
    }
    Ok(())
}

/// Stops the processes that `args` names, or every process, and returns
/// one row or failure for each.
pub(crate) fn down(
    environment: &Environment,
    args: &ProcessesArgs,
) -> Result<Batch<ProcessRow, RunFailure>, crumbkit::Error> {
    let mut context = context(environment, args.workspace.as_ref(), &args.processes)?;
    let lock = context.lock()?;
    let manifest = lenient_manifest(&context.workspace);
    let results = stop(&context, &lock, manifest.as_ref())?;
    context.forget_reclaim(&args.processes)?;
    let keys: Vec<&str> = results
        .iter()
        .map(|result| match result {
            Ok(status) => status.name.as_str(),
            Err(unstoppable) => unstoppable.process.as_str(),
        })
        .collect();
    let mut batch = Batch::new(&["down"], "processes", &keys);
    for result in &results {
        batch.push(match result {
            Ok(status) => Ok(ProcessRow::from(status)),
            Err(unstoppable) => Err(RunFailure::Unstoppable {
                process: unstoppable.process.clone(),
                pid: unstoppable.pid,
                unverified: unstoppable.unverified,
                not_started: Vec::new(),
            }),
        });
    }
    Ok(batch)
}

/// Stops the processes that `args` names, or every process, and starts
/// them again with their dependencies, once their builds have passed, so
/// a build that fails leaves them running.
pub(crate) fn restart(
    environment: &Environment,
    args: &ProcessesArgs,
) -> Result<Result<Vec<ProcessRow>, Interrupted>, crumbkit::Error> {
    let mut context = context(environment, args.workspace.as_ref(), &args.processes)?;
    let manifest = required_manifest(&context.workspace)?;
    if let Err(unknown) = manifest.start_order(&context.processes) {
        return Err(RunFailure::UnknownProcess {
            workspace: context.workspace.name.to_string(),
            process: unknown,
        }
        .into());
    }
    let lock = context.lock()?;
    let passed = match build(environment, &mut context, &lock, &manifest)? {
        Ok(passed) => passed,
        Err(interrupted) => return Ok(Err(interrupted)),
    };
    replace(&mut context, &lock, passed, &manifest, &args.processes).map(Ok)
}

/// Stops the context's processes and starts them again. It is the only
/// path that stops and then starts, and it takes `passed`, so a restart
/// cannot stop a process before the builds have passed.
fn replace(
    context: &mut Context,
    lock: &Lock,
    passed: BuildsPassed<'_>,
    manifest: &Manifest,
    named: &[String],
) -> Result<Vec<ProcessRow>, crumbkit::Error> {
    stop_every(context, lock, Some(manifest))?;
    start(context, passed, manifest, named)
}

/// Returns each process of the workspace that `args` names and its state.
pub(crate) fn status(
    environment: &Environment,
    args: &StatusArgs,
) -> Result<Listing<ProcessRow, StatusSummary>, crumbkit::Error> {
    let context = context(environment, args.workspace.as_ref(), &[])?;
    let (manifest, error) = manifest_or_error(&context.workspace);
    let statuses = context
        .runs
        .status(&context.workspace, manifest.as_ref())
        .map_err(MxvFailure::from)?;
    let rows = statuses.iter().map(ProcessRow::from).collect();
    let checkouts = context
        .workspace
        .checkouts
        .iter()
        .map(|checkout| CheckoutStateRow::read(&Git, checkout))
        .collect();
    Ok(Listing::from_all(rows, None).summary(StatusSummary::new(
        &context.workspace,
        manifest.as_ref(),
        error.map(RunFailure::Manifest).as_ref(),
        checkouts,
    )))
}

/// Prints the last lines of the log of the process that `args` names, and
/// with `--follow` each later line until the reader stops.
pub(crate) fn logs(output: Output, environment: &Environment, args: &LogsArgs) -> ExitCode {
    let mut streaming = match crumbkit::stream(output, LOGS) {
        Ok(streaming) => streaming,
        Err(code) => return code,
    };
    let context = match context(environment, args.workspace.as_ref(), &[]) {
        Ok(context) => context,
        Err(error) => return streaming.end(Err(error)),
    };
    // A person who follows a log is using the workspace, so mxv touches it
    // while it follows.
    let _toucher = args.follow.then(|| {
        context
            .runs
            .keep_touched(&context.workspace.name, TOUCH_EVERY)
    });
    let manifest = match manifest(&context.workspace) {
        Ok(manifest) => manifest,
        Err(error) => return streaming.end(Err(crumbkit::Error::from(error))),
    };
    let mut pass = |line: String| {
        if streaming.item(&LogLine::new(line)).is_break() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    // phase gives each note at most once for a read, and says in it why
    // the log passed no lines.
    let mut noted = |note: multiverse::phase::LogNote| {
        crate::warn(&[format!("`{}`: {note}", args.process)]);
    };
    let found = context.runs.logs(
        &context.workspace,
        manifest.as_ref(),
        &args.process,
        args.lines,
        args.follow,
        &mut multiverse::LogOutput {
            pass: &mut pass,
            noted: &mut noted,
        },
    );
    match found {
        Ok(true) => streaming.end(Ok::<(), crumbkit::Error>(())),
        Ok(false) => streaming.end(Err(crumbkit::Error::from(RunFailure::UnknownProcess {
            workspace: context.workspace.name.to_string(),
            process: args.process.clone(),
        }))),
        Err(error) => streaming.end(Err(MxvFailure::from(error))),
    }
}

/// Gives the workspace that `args` names new host ports for the ports that
/// it names, or for every port, while none of its processes runs.
pub(crate) fn ports_reset(
    environment: &Environment,
    args: &PortsResetArgs,
) -> Result<Vec<PortRow>, crumbkit::Error> {
    let mut context = context(environment, args.workspace.as_ref(), &[])?;
    let manifest = required_manifest(&context.workspace)?;
    let lock = context.lock()?;
    let running = context
        .runs
        .running(&context.workspace)
        .map_err(MxvFailure::from)?;
    if !running.is_empty() {
        return Err(RunFailure::Running {
            workspace: context.workspace.name.to_string(),
            processes: running,
        }
        .into());
    }
    let reset = context
        .runs
        .reset_ports(
            &lock,
            &context.store,
            &mut context.workspace,
            &manifest,
            &args.ports,
        )
        .map_err(MxvFailure::from)?;
    if let Err(port) = reset {
        return Err(RunFailure::UnknownPort {
            workspace: context.workspace.name.to_string(),
            port,
        }
        .into());
    }
    Ok(manifest
        .ports
        .iter()
        .filter_map(|name| {
            context
                .workspace
                .ports
                .get(name)
                .map(|port| PortRow::new(name.clone(), *port))
        })
        .collect())
}

/// Returns the names of the running processes of `workspace`, for the
/// refusal of `mxv remove`.
pub(crate) fn running(
    environment: &Environment,
    workspace: &Workspace,
) -> Result<Vec<String>, MxvFailure> {
    Ok(environment.runs()?.running(workspace)?)
}

/// Takes the lock of the workspace `name`, which `up` and `down` also
/// take, so `mxv clean` deletes nothing while they start or stop it.
pub(crate) fn lock(environment: &Environment, name: &WorkspaceName) -> Result<Lock, MxvFailure> {
    Ok(environment.runs()?.lock(name)?)
}

/// Stops every process of `workspace` and deletes its run files and logs,
/// for `mxv remove --force`.
pub(crate) fn stop_all(
    environment: &Environment,
    workspace: &Workspace,
) -> Result<(), crumbkit::Error> {
    let mut context = Context {
        store: Store::new(environment.records()?),
        runs: environment.runs()?,
        workspace: workspace.clone(),
        processes: Vec::new(),
    };
    let lock = context.lock()?;
    stop_every(&context, &lock, None)
}

/// Stops every process and container of `workspace`, for a caller that
/// already holds the workspace's lock as `lock`.
pub(crate) fn stop_all_locked(
    environment: &Environment,
    workspace: &Workspace,
    lock: &Lock,
) -> Result<(), crumbkit::Error> {
    let context = Context {
        store: Store::new(environment.records()?),
        runs: environment.runs()?,
        workspace: workspace.clone(),
        processes: Vec::new(),
    };
    stop_every(&context, lock, None)
}

/// Removes the stopped containers of the workspace `name`, whose stable id
/// is `id`, and deletes its run files and logs.
pub(crate) fn forget(
    environment: &Environment,
    name: &WorkspaceName,
    id: Option<&WorkspaceId>,
) -> Result<(), MxvFailure> {
    Ok(environment.runs()?.forget(name, id)?)
}

/// Returns the docker volumes of `workspace`, for the refusal of `mxv
/// remove`.
pub(crate) fn volumes(
    environment: &Environment,
    workspace: &Workspace,
) -> Result<Volumes, MxvFailure> {
    Ok(environment.runs()?.volumes(workspace)?)
}

/// Returns the docker containers of `workspace`, running or stopped, which
/// `mxv remove` removes.
pub(crate) fn containers(
    environment: &Environment,
    workspace: &Workspace,
) -> Result<Vec<String>, MxvFailure> {
    Ok(environment.runs()?.docker_containers(workspace)?)
}

/// Deletes the docker volumes `volumes`, which [`volumes`] returned for a
/// workspace, for `mxv remove --delete-volumes`.
pub(crate) fn delete_volumes(
    environment: &Environment,
    volumes: Volumes,
) -> Result<(), MxvFailure> {
    Ok(environment.runs()?.delete_volumes(volumes)?)
}

/// Stops every process and container of `workspace` through the stop of
/// `mxv down`, for `mxv reclaim`, and marks the workspace with what ran.
/// It holds the workspace's lock, reads the record again and the names of
/// what still runs, and passes both to `check`, which returns why the
/// workspace is no longer idle, or `None`. When `check` gives a reason,
/// or another mxv command holds the lock, it stops nothing and returns the
/// reason.
pub(crate) fn reclaim_stop(
    environment: &Environment,
    workspace: &Workspace,
    check: impl FnOnce(&Workspace, &[String]) -> Result<Option<String>, crumbkit::Error>,
) -> Result<Result<(), String>, crumbkit::Error> {
    let mut context = Context {
        store: Store::new(environment.records()?),
        runs: environment.runs()?,
        workspace: workspace.clone(),
        processes: Vec::new(),
    };
    let lock = match context.lock() {
        Ok(lock) => lock,
        Err(MxvFailure::Library(Error::Busy { .. })) => {
            return Ok(Err(
                "another mxv command holds the workspace's lock".to_owned()
            ));
        }
        Err(failure) => return Err(failure.into()),
    };
    let running = context
        .runs
        .running(&context.workspace)
        .map_err(MxvFailure::from)?;
    if let Some(reason) = check(&context.workspace, &running)? {
        return Ok(Err(reason));
    }
    let manifest = lenient_manifest(&context.workspace);
    stop_every(&context, &lock, manifest.as_ref())?;
    context.workspace.reclaimed = Some(Reclaimed::new(running));
    context
        .store
        .update(&mut context.workspace)
        .map_err(MxvFailure::from)?;
    Ok(Ok(()))
}

/// Pins or unpins, as `pinned` says, the workspace that `workspace` names,
/// or else the one that holds the current directory, while it holds the
/// workspace's lock, and returns its record.
pub(crate) fn set_pinned(
    environment: &Environment,
    workspace: Option<&String>,
    pinned: bool,
) -> Result<Workspace, crumbkit::Error> {
    let mut context = context(environment, workspace, &[])?;
    let _lock = context.lock()?;
    if context.workspace.pinned != pinned {
        context.workspace.pinned = pinned;
        context
            .store
            .update(&mut context.workspace)
            .map_err(MxvFailure::from)?;
    }
    Ok(context.workspace)
}
