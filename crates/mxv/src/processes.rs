//! The process verbs `up`, `down`, `restart`, `status` and `logs`, which
//! run a workspace's app from the `mxv.toml` of its checkout, and the
//! documents they print.
//!
//! Each verb works on the workspace that `-w/--workspace` names, or else on
//! the one that holds the current directory. Its positional words name
//! processes.

use std::path::Path;
use std::process::ExitCode;

use crumbkit::{Batch, Document, Listing, Output, Stream};
use multiverse::{
    DownOutcome, Follower, Lock, Manifest, ProcessStatus, Runs, Store, Unstoppable, UpOutcome,
    Volumes, Workspace, WorkspaceName, tail,
};

use crate::cli::{LogsArgs, PortsResetArgs, ProcessesArgs, StatusArgs};
use crate::environment::Environment;
use crate::failures::MxvFailure;
use crate::rows::{LogLine, PortRow, ProcessRow, StatusSummary};
use crate::run_failures::{LogTail, RunFailure};
use crate::verbs;

/// The document that `mxv up` prints.
pub(crate) const UP: Document<Vec<ProcessRow>> = Document::new(
    "mxv",
    "up",
    1,
    "The selected processes and their dependencies, in the order they started, each running.",
);

/// The document that `mxv down` prints.
pub(crate) const DOWN: Document<Batch<ProcessRow, RunFailure>> = Document::new(
    "mxv",
    "down",
    1,
    "The processes that the run stopped, in the order it stopped them, and a failure for each process group that outlived SIGKILL.",
);

/// The document that `mxv restart` prints.
pub(crate) const RESTART: Document<Vec<ProcessRow>> = Document::new(
    "mxv",
    "restart",
    1,
    "The restarted processes and their dependencies, in the order they started, each running.",
);

/// The document that `mxv status` prints.
pub(crate) const STATUS: Document<Listing<ProcessRow, StatusSummary>> = Document::new(
    "mxv",
    "status",
    1,
    "Each process of the workspace's manifest in start order, then each other process that has run files, with its state, and the manifest's error when it does not parse.",
)
.when_empty("the workspace has no manifest that declares processes, and no process has run files");

/// The document that `mxv logs` prints.
pub(crate) const LOGS: Document<Stream<LogLine>> = Document::new(
    "mxv",
    "logs",
    1,
    "The last lines of a process's log, then with --follow each line that the process writes.",
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
struct Context {
    /// The store of the workspace records.
    store: Store,
    /// The run files of every workspace's processes.
    runs: Runs,
    /// The workspace.
    workspace: Workspace,
    /// The processes that the command line names after the workspace.
    processes: Vec<String>,
}

impl Context {
    /// Takes the workspace's lock, which `up`, `down` and `restart` hold
    /// while they start or stop its processes, then reads the workspace's
    /// record again, so the ports are the ones that a command which held
    /// the lock before stored.
    fn lock(&mut self) -> Result<Lock, MxvFailure> {
        let lock = self.runs.lock(&self.workspace.name)?;
        if let Some(fresh) = self.store.read(&self.workspace.name)? {
            self.workspace = fresh;
        }
        Ok(lock)
    }
}

/// Returns the context of the workspace that `-w` names, or else of the
/// one that holds the current directory, and the processes `processes`.
fn context(
    environment: &Environment,
    workspace: Option<&String>,
    processes: &[String],
) -> Result<Context, crumbkit::Error> {
    let store = Store::new(environment.records()?);
    let runs = Runs::new(environment.runs()?).with_port_range(environment.port_range()?);
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
    Ok(Context {
        store,
        runs,
        workspace,
        processes: processes.to_vec(),
    })
}

/// Returns the workspace whose directory holds the current directory, or
/// `None`.
fn current(store: &Store, environment: &Environment) -> Result<Option<Workspace>, MxvFailure> {
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

/// Returns the workspace's manifest, or `None` when its checkout has none.
fn manifest(workspace: &Workspace) -> Result<Option<Manifest>, RunFailure> {
    Manifest::read(checkout(workspace)).map_err(RunFailure::Manifest)
}

/// Returns the workspace's manifest, or `None` when its checkout has none
/// or it does not parse, for `down` and `status`, which then work from the
/// run records so a broken manifest never keeps processes running.
fn lenient_manifest(workspace: &Workspace) -> Option<Manifest> {
    Manifest::read(checkout(workspace)).ok().flatten()
}

/// Returns the workspace's manifest as [`lenient_manifest`] does, and the
/// manifest's error as text when it does not parse.
fn manifest_or_error(workspace: &Workspace) -> (Option<Manifest>, Option<String>) {
    match Manifest::read(checkout(workspace)) {
        Ok(manifest) => (manifest, None),
        Err(error) => (None, Some(error.to_string())),
    }
}

/// Returns the workspace's manifest, or the failure of a checkout without
/// one.
fn required_manifest(workspace: &Workspace) -> Result<Manifest, RunFailure> {
    manifest(workspace)?.ok_or_else(|| RunFailure::ManifestMissing {
        checkout: checkout(workspace).to_path_buf(),
    })
}

/// Starts the processes that `args` names, and their dependencies.
pub(crate) fn up(
    environment: &Environment,
    args: &ProcessesArgs,
) -> Result<Vec<ProcessRow>, crumbkit::Error> {
    let mut context = context(environment, args.workspace.as_ref(), &args.processes)?;
    let lock = context.lock()?;
    start(&mut context, &lock)
}

/// Starts the context's processes, and returns their rows or the failure
/// of the first one that does not start.
fn start(context: &mut Context, lock: &Lock) -> Result<Vec<ProcessRow>, crumbkit::Error> {
    let manifest = required_manifest(&context.workspace)?;
    let checkout = checkout(&context.workspace).to_path_buf();
    let workspace = context.workspace.name.to_string();
    let outcome = context
        .runs
        .up(
            lock,
            &context.store,
            &mut context.workspace,
            &checkout,
            &manifest,
            &context.processes,
        )
        .map_err(MxvFailure::from)?;
    match outcome {
        UpOutcome::Up(statuses) => Ok(statuses.iter().map(ProcessRow::from).collect()),
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
            tail,
        } => Err(RunFailure::Exited {
            workspace,
            process,
            code,
            signal,
            log: log.into_boxed_path(),
            tail: LogTail::new(tail),
        }
        .into()),
        UpOutcome::ForeignListener {
            process,
            port,
            number,
            pid,
            log,
            tail,
        } => Err(RunFailure::ForeignListener {
            workspace,
            process,
            port,
            number,
            pid,
            log: log.into_boxed_path(),
            tail: LogTail::new(tail),
        }
        .into()),
        UpOutcome::DetachedListener {
            process,
            port,
            number,
            pid,
            log,
            tail,
        } => Err(RunFailure::DetachedListener {
            workspace,
            process,
            port,
            number,
            pid,
            log: log.into_boxed_path(),
            tail: LogTail::new(tail),
        }
        .into()),
        UpOutcome::NotReady {
            process,
            waited,
            timeout,
            log,
            tail,
        } => Err(RunFailure::NotReady {
            workspace,
            process,
            waited: Box::new(waited),
            seconds: timeout.as_secs(),
            log: log.into_boxed_path(),
            tail: LogTail::new(tail),
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
            }),
        });
    }
    Ok(batch)
}

/// Stops the processes that `args` names, or every process, and starts
/// them again with their dependencies.
pub(crate) fn restart(
    environment: &Environment,
    args: &ProcessesArgs,
) -> Result<Vec<ProcessRow>, crumbkit::Error> {
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
    stop_every(&context, &lock, Some(&manifest))?;
    start(&mut context, &lock)
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
    Ok(Listing::from_all(rows, None).summary(StatusSummary::new(error)))
}

/// Prints the last lines of the log of the process that `args` names, and
/// with `--follow` each later line until the reader stops.
pub(crate) fn logs(output: Output, environment: &Environment, args: &LogsArgs) -> ExitCode {
    let mut streaming = match crumbkit::stream(output, LOGS) {
        Ok(streaming) => streaming,
        Err(code) => return code,
    };
    let (context, log) = match log_path(environment, args) {
        Ok(found) => found,
        Err(error) => return streaming.end(Err(error)),
    };
    let mut follower = match Follower::at_end(&log) {
        Ok(follower) => follower,
        Err(error) => return streaming.end(Err(MxvFailure::from(error))),
    };
    for line in tail(&log, args.lines) {
        if streaming.item(&LogLine::new(line)).is_break() {
            return streaming.end(Ok::<(), crumbkit::Error>(()));
        }
    }
    if args.follow {
        while let Some(reader) = follower.as_mut() {
            let lines = match reader.lines() {
                Ok(lines) => lines,
                Err(error) => return streaming.end(Err(MxvFailure::from(error))),
            };
            let quiet = lines.is_empty();
            for line in lines {
                if streaming.item(&LogLine::new(line)).is_break() {
                    return streaming.end(Ok::<(), crumbkit::Error>(()));
                }
            }
            if quiet {
                // The follow ends once the log is gone or nothing can write
                // to it any more. A read that returned lines is followed by
                // another, so the last lines before the end are printed.
                let writing = context.runs.writing(&context.workspace.name, &args.process);
                match writing {
                    Ok(true) if log.exists() => {}
                    Ok(_) => break,
                    Err(error) => return streaming.end(Err(MxvFailure::from(error))),
                }
                reader.wait();
            }
        }
    }
    streaming.end(Ok::<(), crumbkit::Error>(()))
}

/// Returns the log of the process that `args` names, after it checks that
/// the workspace knows the process.
fn log_path(
    environment: &Environment,
    args: &LogsArgs,
) -> Result<(Context, std::path::PathBuf), crumbkit::Error> {
    let context = context(environment, args.workspace.as_ref(), &[])?;
    let process = &args.process;
    let manifest = manifest(&context.workspace)?;
    let known = context
        .runs
        .status(&context.workspace, manifest.as_ref())
        .map_err(MxvFailure::from)?;
    if !known.iter().any(|status| &status.name == process) {
        return Err(RunFailure::UnknownProcess {
            workspace: context.workspace.name.to_string(),
            process: process.clone(),
        }
        .into());
    }
    let log = context.runs.log(&context.workspace.name, process);
    Ok((context, log))
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
    Ok(Runs::new(environment.runs()?).running(workspace)?)
}

/// Stops every process of `workspace` and deletes its run files and logs,
/// for `mxv remove --force`.
pub(crate) fn stop_all(
    environment: &Environment,
    workspace: &Workspace,
) -> Result<(), crumbkit::Error> {
    let mut context = Context {
        store: Store::new(environment.records()?),
        runs: Runs::new(environment.runs()?),
        workspace: workspace.clone(),
        processes: Vec::new(),
    };
    let lock = context.lock()?;
    stop_every(&context, &lock, None)
}

/// Removes the stopped containers of the workspace `name` and deletes its
/// run files and logs.
pub(crate) fn forget(environment: &Environment, name: &WorkspaceName) -> Result<(), MxvFailure> {
    Ok(Runs::new(environment.runs()?).forget(name)?)
}

/// Returns the docker volumes of the workspace `name`, for the refusal of
/// `mxv remove`.
pub(crate) fn volumes(
    environment: &Environment,
    name: &WorkspaceName,
) -> Result<Volumes, MxvFailure> {
    Ok(Runs::new(environment.runs()?).volumes(name)?)
}

/// Returns the docker containers of the workspace `name`, running or
/// stopped, which `mxv remove` removes.
pub(crate) fn containers(
    environment: &Environment,
    name: &WorkspaceName,
) -> Result<Vec<String>, MxvFailure> {
    Ok(Runs::new(environment.runs()?).docker_containers(name)?)
}

/// Deletes the docker volumes `volumes` of the workspace `name`, for `mxv
/// remove --delete-volumes`.
pub(crate) fn delete_volumes(
    environment: &Environment,
    name: &WorkspaceName,
    volumes: Volumes,
) -> Result<(), MxvFailure> {
    Ok(Runs::new(environment.runs()?).delete_volumes(name, volumes)?)
}
