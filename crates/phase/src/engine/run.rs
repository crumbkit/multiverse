//! `up`, `down` and `restart`: the engine's operations on a workspace.
//!
//! Each operation takes the workspace's lifecycle lock through the run
//! record's [`Journal`], and holds it until it returns. `up` writes every
//! object that it will create into the record before it starts anything,
//! then starts apps in dependency order. For each host process it checks
//! the process's port, resolves the process's environment, starts its
//! keeper through the [`HostRunner`], records the keeper and the process
//! from the handshake, and waits for the process to be ready. A process
//! without a readiness probe is watched for the start grace instead, and a
//! process that ends meanwhile fails to start. Container apps go through
//! the [`ContainerRunner`].
//!
//! Before each launch, `up` writes the process's keeper and entry back to
//! planned with no process, so a record that names a stopped or stale
//! process never stands for the new one. It pushes each process it
//! launches onto its list of what it started before it records the
//! process.
//!
//! When a process fails to start, `up` stops what it started of that app,
//! starts no further app, and reports which apps started. When the caller
//! cancels, or any failure ends the operation, `up` stops everything it
//! started, and goes on stopping after a stop fails. Those stops each get
//! their process's grace and twice the kill wait from the moment they
//! start. `up` then removes the record's entries of apps that it never
//! tried to start, and removes the record when nothing in it may run.
//!
//! `restart` stops each running process with the caller's deadline and
//! starts it again. When the caller cancels, `restart` stops what it
//! started of the app it was restarting, leaves the apps it already
//! restarted running, and reports the apps it had not reached as
//! untouched.
//!
//! `down` and `restart` stop processes through [`HostRunner::stop`], which
//! never signals a process that phase cannot prove is its own, and the
//! record keeps such a process.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::cancel::Cancel;
use crate::clock::{POLL_INTERVAL, deadline_in, now, pause};
use crate::engine::containers::{ContainerRunner, ContainerStart};
use crate::engine::host::{Holder, HostRunner, is_gone};
use crate::engine::keeper::{AppEnd, KeeperArgs, KeeperCommand, KeeperSpec, Launch};
use crate::engine::probe::{self, Check, Goal, HostExec, LogLines, LogWatch, Outcome};
use crate::env::{AppliedEnvironment, EnvironmentSource};
use crate::error::{NotRunnable, PhaseError};
use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::phase::UpDeadlines;
use crate::plan::probe::{LogPattern, ProbeCheck};
use crate::plan::restart::DEFAULT_STOP_GRACE;
use crate::plan::{AppPlan, Plan, ProcessKind, ProcessPlan, RunnerKind};
use crate::record::process::{Identity, vouched};
use crate::record::run::{
    ContainerEntry, Ended, EntryState, Exit, HostProcess, Launching, Listen, ObjectKey, PlanHash,
    RunRecord, Started,
};
use crate::record::store::{Journal, RunStore};
use crate::report::{AppOutcome, AppReport, Note, Report, StartProblem};
use crate::select::Selection;
use crate::source::Source;

pub(crate) mod order;
mod stop;
#[cfg(test)]
mod tests;

use order::{Chosen, choose};
use stop::{Stopped, Target};

/// The most lines of a process's log that a report holds when the process
/// ends before it is ready. Older lines are left out.
const TAIL_LINES: usize = 20;

/// How long `up` waits for a keeper's end line once it has seen the
/// keeper's app gone. The keeper writes the line as soon as it reaps the
/// app, so the wait is short. When the line has not come by then, the
/// report of the failed start holds no code and no signal.
const END_LINE_WAIT: Duration = Duration::from_secs(2);

/// What the engine's operations are given besides their arguments.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Settings<'a> {
    /// The directory of phase's state, which holds run records and logs.
    pub(crate) state_dir: &'a Path,
    /// How long an operation waits for the lifecycle lock.
    pub(crate) lock_wait: Duration,
    /// How long `up` watches a process without a readiness probe.
    pub(crate) start_grace: Duration,
    /// How long phase waits after `KILL`.
    pub(crate) kill_wait: Duration,
    /// The program that runs keepers, or `None` when the host gave none.
    pub(crate) keeper: Option<&'a KeeperCommand>,
    /// The variables that keepers and apps inherit, `PATH` and `HOME`.
    pub(crate) inherited: &'a BTreeMap<String, String>,
}

/// The engine of one operation: its settings and what it reaches the
/// machine through.
pub(crate) struct Engine<'a> {
    /// The settings.
    pub(crate) settings: Settings<'a>,
    /// The runner of host processes.
    pub(crate) host: &'a dyn HostRunner,
    /// The runner of container apps.
    pub(crate) containers: &'a dyn ContainerRunner,
}

impl Engine<'_> {
    /// Starts the selected apps of `plan` and what they depend on, as the
    /// module docs say.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::UnknownTarget`] when `select` names what the
    /// plan lacks, the errors of taking the lock and of reading and
    /// writing the record, and [`PhaseError::Environment`] after stopping
    /// what this `up` started when an environment cannot be resolved.
    pub(crate) fn up(
        &self,
        plan: &Plan,
        select: &Selection,
        env: &dyn EnvironmentSource,
        deadlines: UpDeadlines,
        cancel: &Cancel,
    ) -> Result<Report, PhaseError> {
        let chosen = choose(plan, select, true)?;
        let store = RunStore::new(self.settings.state_dir);
        let journal = Journal::open(&store, plan.id(), self.settings.lock_wait)?;
        let mut op = Op {
            engine: self,
            workspace: plan.id().clone(),
            plan_hash: PlanHash::new(plan.digest()),
            journal,
            env,
            deadlines,
            cancel,
            started: Vec::new(),
        };
        op.up(plan, &chosen)
    }

    /// Resolves the environment of each selected process that runs again,
    /// then stops and starts it, as the module docs say. A selected app
    /// with no running process is reported as not running, and `restart`
    /// starts nothing that it depends on.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Engine::up`]. When an environment cannot be
    /// resolved, `restart` has stopped nothing.
    pub(crate) fn restart(
        &self,
        plan: &Plan,
        select: &Selection,
        env: &dyn EnvironmentSource,
        deadline: Instant,
        cancel: &Cancel,
    ) -> Result<Report, PhaseError> {
        let chosen = choose(plan, select, false)?;
        let store = RunStore::new(self.settings.state_dir);
        let journal = Journal::open(&store, plan.id(), self.settings.lock_wait)?;
        let mut op = Op {
            engine: self,
            workspace: plan.id().clone(),
            plan_hash: PlanHash::new(plan.digest()),
            journal,
            env,
            deadlines: UpDeadlines::new(deadline, deadline),
            cancel,
            started: Vec::new(),
        };
        op.restart(&chosen)
    }

    /// Stops the selected apps of `workspace` in reverse dependency order,
    /// as the `stop` module says.
    ///
    /// # Errors
    ///
    /// Returns the errors of taking the lock and of reading and writing the
    /// record.
    pub(crate) fn down(
        &self,
        workspace: &WorkspaceId,
        select: &Selection,
        deadline: Instant,
    ) -> Result<Report, PhaseError> {
        stop::down(self, workspace, select, deadline)
    }

    /// Returns the log file of `process` of `app` in `workspace`.
    fn log_path(&self, workspace: &WorkspaceId, app: &AppName, process: &ProcessName) -> PathBuf {
        crate::logs::log_path(self.settings.state_dir, workspace, app, process)
    }
}

/// One `up` or `restart` in progress, which holds the lifecycle lock.
struct Op<'e, 's> {
    /// The engine.
    engine: &'e Engine<'e>,
    /// The workspace.
    workspace: WorkspaceId,
    /// The hash of the plan that this operation starts processes from.
    plan_hash: PlanHash,
    /// The record's journal, which holds the lock.
    journal: Journal<'s>,
    /// The source of each process's environment.
    env: &'e dyn EnvironmentSource,
    /// The deadlines.
    deadlines: UpDeadlines,
    /// The caller's flag.
    cancel: &'e Cancel,
    /// What this operation started, in order.
    started: Vec<Begun>,
}

/// Something that an operation started, which it stops again when the
/// caller cancels or a failure ends the operation.
enum Begun {
    /// A host process and its keeper.
    Host(Target),
    /// The containers of an app.
    Containers(AppName),
}

impl Begun {
    /// Returns the app that the started object belongs to.
    const fn app(&self) -> &AppName {
        match self {
            Self::Host(target) => &target.app,
            Self::Containers(app) => app,
        }
    }
}

/// How one app's start ended.
enum AppStep {
    /// The app has this outcome.
    Done(AppOutcome, Vec<Note>),
    /// The caller cancelled the operation while the app started.
    Cancelled(Vec<Note>),
}

/// How one process's start ended.
enum Step {
    /// The process is ready.
    Ready,
    /// The process was already running and is left alone.
    AlreadyRunning,
    /// A process that phase started survived an earlier stop, so phase
    /// leaves it and starts nothing in its place.
    Left,
    /// The process failed to start for this reason.
    Failed(StartProblem),
    /// A deadline passed, so phase does not know whether it is running.
    Uncertain,
    /// The caller cancelled the start.
    Cancelled,
    /// A failure that is not a start problem kept the process from
    /// starting.
    Errored(PhaseError),
}

/// What the run record says of a process that `up` is about to start.
enum Recorded {
    /// No keeper or process of it may still run.
    Nothing,
    /// Its keeper or its process is still the one phase started.
    Ours,
    /// A keeper or process of it that phase tried to stop is still there.
    Left,
    /// The process that led the group `0` has ended and the group still
    /// has members, which nothing proves are phase's own, so phase leaves
    /// the group and starts nothing in its place.
    Unverified(u32),
    /// phase may not read a recorded keeper or process of it.
    Unknown,
}

/// The stops of one cancelled or failed operation, by app: the notes they
/// added, and whether each stop left nothing behind.
type StopsByApp = BTreeMap<AppName, (Vec<Note>, bool)>;

impl Op<'_, '_> {
    /// Carries out `up` for `chosen`, then removes the record's entries of
    /// the apps that it never tried to start, and the record itself when
    /// nothing in it is left running.
    fn up(&mut self, plan: &Plan, chosen: &[Chosen<'_>]) -> Result<Report, PhaseError> {
        let mut attempted = BTreeSet::new();
        let report = self.start_all(plan, chosen, &mut attempted)?;
        let unattempted: Vec<AppName> = chosen
            .iter()
            .map(|choice| choice.app.name.clone())
            .filter(|name| !attempted.contains(name))
            .collect();
        self.journal.forget_unstarted(&unattempted)?;
        self.journal.finish_keeping_exits()?;
        Ok(report)
    }

    /// Starts the apps of `chosen` in order, adding each app that it tries
    /// to start to `attempted`.
    fn start_all(
        &mut self,
        plan: &Plan,
        chosen: &[Chosen<'_>],
        attempted: &mut BTreeSet<AppName>,
    ) -> Result<Report, PhaseError> {
        let mut refused = BTreeMap::new();
        let mut planned = RunRecord::new(self.workspace.clone(), PlanHash::new(plan.digest()));
        for choice in chosen {
            match self.plan_app(planned.clone(), choice) {
                Ok(record) => planned = record,
                Err(error) => {
                    refused.insert(choice.app.name.clone(), error);
                }
            }
        }
        self.journal.plan(planned)?;
        let mut reports: Vec<AppReport> = Vec::new();
        let mut unready: BTreeSet<AppName> = BTreeSet::new();
        let mut halted = false;
        for (at, choice) in chosen.iter().enumerate() {
            let name = &choice.app.name;
            if let Some(error) = refused.remove(name) {
                unready.insert(name.clone());
                reports.push(AppReport::new(name.clone(), AppOutcome::errored(&error)));
                continue;
            }
            if halted
                || choice
                    .app
                    .depends
                    .iter()
                    .any(|depend| unready.contains(depend))
            {
                unready.insert(name.clone());
                reports.push(AppReport::new(name.clone(), AppOutcome::NotStarted));
                continue;
            }
            let rest = chosen.get(at..).unwrap_or_default();
            if self.cancel.is_cancelled() {
                return self.cancelled(reports, Vec::new(), rest);
            }
            attempted.insert(name.clone());
            let step = match self.start_app(choice, None) {
                Ok(step) => step,
                Err(error) => return Err(self.abort(error)),
            };
            match step {
                AppStep::Done(outcome, notes) => {
                    if !matches!(outcome, AppOutcome::Started | AppOutcome::AlreadyRunning) {
                        unready.insert(name.clone());
                        halted = true;
                    }
                    reports.push(AppReport::new(name.clone(), outcome).with_notes(notes));
                }
                AppStep::Cancelled(notes) => return self.cancelled(reports, notes, rest),
            }
        }
        Ok(Report::new(reports))
    }

    /// Returns `planned` with the objects that starting `choice` creates,
    /// or the failure that keeps phase from running it.
    fn plan_app(&self, planned: RunRecord, choice: &Chosen<'_>) -> Result<RunRecord, PhaseError> {
        let app = choice.app;
        let mut planned = planned.app(&app.name, &app.depends);
        match app.runner {
            RunnerKind::Host => {
                runnable(app, &choice.processes)?;
                for process in &choice.processes {
                    planned = planned.planned_host_process(
                        &app.name,
                        &process.name,
                        Some(process.stop),
                        listen(app, process),
                    );
                }
            }
            RunnerKind::Container => {
                let objects = self.engine.containers.objects(&self.workspace, app)?;
                for (process, name) in objects.containers {
                    planned = planned.container(&app.name, &process, name);
                }
                for name in objects.networks {
                    planned = planned.network(name);
                }
                for (name, lifetime) in objects.volumes {
                    planned = planned.volume(name, lifetime);
                }
            }
        }
        Ok(planned)
    }

    /// Starts the processes of `choice`, each with the environment that
    /// `fresh` holds for it, or resolving it when `fresh` holds none.
    ///
    /// # Errors
    ///
    /// Returns the failures that end the whole operation:
    /// [`PhaseError::Environment`] and the errors of writing the record.
    fn start_app(
        &mut self,
        choice: &Chosen<'_>,
        mut fresh: Option<&mut BTreeMap<ProcessName, Box<dyn AppliedEnvironment>>>,
    ) -> Result<AppStep, PhaseError> {
        let app = choice.app;
        if app.runner == RunnerKind::Container {
            return self.start_containers(choice);
        }
        let mark = self.started.len();
        let mut notes = Vec::new();
        let mut any = false;
        for process in &choice.processes {
            let environment = fresh
                .as_deref_mut()
                .and_then(|fresh| fresh.remove(&process.name));
            let step = self.start_process(app, process, environment, &mut notes)?;
            let outcome = match step {
                Step::Ready => {
                    any = true;
                    continue;
                }
                Step::AlreadyRunning => continue,
                Step::Cancelled => return Ok(AppStep::Cancelled(notes)),
                Step::Uncertain => return Ok(AppStep::Done(AppOutcome::StartUncertain, notes)),
                Step::Left => AppOutcome::Left,
                Step::Failed(problem) => AppOutcome::failed(process.name.clone(), problem),
                Step::Errored(error) => AppOutcome::errored(&error),
            };
            self.stop_since(mark, &mut StopsByApp::new(), &mut notes)?;
            return Ok(AppStep::Done(outcome, notes));
        }
        let outcome = if any {
            AppOutcome::Started
        } else {
            AppOutcome::AlreadyRunning
        };
        Ok(AppStep::Done(outcome, notes))
    }

    /// Starts the processes of the container app `choice` through the
    /// container runner. A process whose recorded container still runs, as
    /// the runner checks it, is left alone, and an app whose processes all
    /// still run is already running. When the caller cancels or a container
    /// fails to start, the operation stops the containers it started.
    fn start_containers(&mut self, choice: &Chosen<'_>) -> Result<AppStep, PhaseError> {
        let app = choice.app;
        let mark = self.started.len();
        let chosen: BTreeSet<&ProcessName> = choice
            .processes
            .iter()
            .map(|process| &process.name)
            .collect();
        let recorded: Vec<ContainerEntry> = self
            .recorded_containers(&app.name, Some(&chosen))
            .into_iter()
            .filter(|entry| entry.state == EntryState::Running && entry.id.is_some())
            .collect();
        let running: BTreeSet<ProcessName> = if recorded.is_empty() {
            BTreeSet::new()
        } else {
            match self
                .engine
                .containers
                .running(&self.workspace, app, &recorded)
            {
                Ok(running) => running.into_iter().collect(),
                Err(error) => return Ok(AppStep::Done(AppOutcome::errored(&error), Vec::new())),
            }
        };
        let processes: Vec<ProcessName> = choice
            .processes
            .iter()
            .map(|process| process.name.clone())
            .filter(|process| !running.contains(process))
            .collect();
        if processes.is_empty() {
            return Ok(AppStep::Done(AppOutcome::AlreadyRunning, Vec::new()));
        }
        let start = ContainerStart {
            workspace: &self.workspace,
            app,
            processes: &processes,
            env: self.env,
            build: self.deadlines.build,
            deadline: self.deadlines.ready,
            cancel: self.cancel,
        };
        let journal = &mut self.journal;
        let mut launching = |process: &ProcessName| {
            journal.containers_launching(&app.name, std::slice::from_ref(process))
        };
        let started = self.engine.containers.start(&start, &mut launching);
        if started.containers.len() > started.removed.len() {
            self.started.push(Begun::Containers(app.name.clone()));
        }
        for (name, id) in started.networks {
            self.journal
                .started(&ObjectKey::Network(name), Started::Network(id))?;
        }
        for name in started.volumes {
            self.journal
                .started(&ObjectKey::Volume(name), Started::Volume)?;
        }
        for container in started.containers {
            let key = ObjectKey::Container(app.name.clone(), container.process);
            self.journal
                .started(&key, Started::Container(container.id, container.plan_hash))?;
        }
        for process in started.removed {
            self.journal
                .stopped(&ObjectKey::Container(app.name.clone(), process))?;
        }
        if let Some(error) = started.failure {
            return Ok(AppStep::Done(AppOutcome::errored(&error), Vec::new()));
        }
        if self.cancel.is_cancelled() {
            return Ok(AppStep::Cancelled(Vec::new()));
        }
        if started.uncertain.is_some() {
            return Ok(AppStep::Done(AppOutcome::StartUncertain, Vec::new()));
        }
        let Some((process, problem)) = started.problem else {
            return Ok(AppStep::Done(AppOutcome::Started, Vec::new()));
        };
        let mut notes = Vec::new();
        self.stop_since(mark, &mut StopsByApp::new(), &mut notes)?;
        Ok(AppStep::Done(AppOutcome::failed(process, problem), notes))
    }

    /// Starts `process` of `app` with `environment`, or with an
    /// environment it resolves, and waits until it is ready.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::Environment`] when the environment cannot be
    /// resolved, and the errors of writing the record.
    fn start_process(
        &mut self,
        app: &AppPlan,
        process: &ProcessPlan,
        environment: Option<Box<dyn AppliedEnvironment>>,
        notes: &mut Vec<Note>,
    ) -> Result<Step, PhaseError> {
        let host = self.engine.host;
        match self.recorded(app, process) {
            Ok(Recorded::Nothing) => {}
            Ok(Recorded::Ours) => return Ok(Step::AlreadyRunning),
            Ok(Recorded::Left) => return Ok(Step::Left),
            Ok(Recorded::Unverified(group)) => {
                notes.push(Note::group_unverified(process.name.clone(), group));
                return Ok(Step::Left);
            }
            Ok(Recorded::Unknown) => {
                return Ok(Step::Errored(PhaseError::ProcessTable {
                    message: format!(
                        "phase may not read the process that the run record names for {}/{}, so it cannot tell whether it runs",
                        app.name, process.name
                    ),
                }));
            }
            Err(error) => return Ok(Step::Errored(error)),
        }
        let listen = listen(app, process);
        if let Some(listen) = &listen
            && let Some(problem) = self.port_before_launch(process, listen, notes)
        {
            return Ok(Step::Failed(problem));
        }
        let environment = match environment {
            Some(environment) => environment,
            None => self
                .env
                .resolve(&app.name, &process.name, self.deadlines.ready)
                .map_err(|source| PhaseError::Environment {
                    app: app.name.clone(),
                    source,
                })?,
        };
        self.journal.launching(
            &app.name,
            &process.name,
            &Launching {
                plan_hash: self.plan_hash.clone(),
                stop: process.stop,
                listen: listen.clone(),
                restart: process.restart.clone(),
            },
        )?;
        let (keeper, started, end, lines) = match self.launch(app, process, environment.as_ref()) {
            Ok(launched) => launched,
            Err(step) => return Ok(step),
        };
        let target = Target {
            app: app.name.clone(),
            process: process.name.clone(),
            keeper: Some(keeper.clone()),
            host: Some(started.clone()),
            stop: process.stop,
            listen: listen.clone(),
        };
        self.started.push(Begun::Host(target.clone()));
        self.journal.started(
            &ObjectKey::Keeper(app.name.clone(), process.name.clone()),
            Started::Host(keeper),
        )?;
        self.journal.started(
            &ObjectKey::Process(app.name.clone(), process.name.clone()),
            Started::Host(started.clone()),
        )?;
        let step = self.await_ready(app, process, &target, environment.as_ref(), end, lines);
        if let Step::Failed(StartProblem::Exited { code, signal, .. }) = &step {
            // The keeper cannot record the end while this operation holds
            // the lifecycle lock, so the engine records what the keeper's
            // end line said. The stop that follows keeps it, and `status`
            // shows the process as exited.
            let exit = code.map(Exit::Code).or(signal.map(Exit::Signal));
            self.journal.ending(
                &ObjectKey::Process(app.name.clone(), process.name.clone()),
                Ended::by_itself(exit),
            )?;
        }
        if let (Step::Ready, Some(listen)) = (&step, &listen) {
            match port_holder(host, started.group, listen.port, LookupFailed::Report) {
                PortHolder::Free | PortHolder::Group(_) => {}
                PortHolder::Other(Some(pid)) => {
                    return Ok(Step::Failed(StartProblem::foreign_listener(
                        listen.name.clone(),
                        vec![pid],
                    )));
                }
                PortHolder::Other(None) => notes.push(Note::listener_unknown(
                    process.name.clone(),
                    listen.name.clone(),
                    UNREADABLE_HOLDER.to_owned(),
                )),
                PortHolder::Unknown(error) => notes.push(listener_unknown(process, listen, &error)),
            }
        }
        Ok(step)
    }

    /// Returns the start problem of a port that something already holds
    /// before `process` launches, or `None` when it is free or phase cannot
    /// tell, which it adds to `notes`.
    fn port_before_launch(
        &self,
        process: &ProcessPlan,
        listen: &Listen,
        notes: &mut Vec<Note>,
    ) -> Option<StartProblem> {
        // No process group is 0, so any holder is another process.
        match port_holder(self.engine.host, 0, listen.port, LookupFailed::Bind) {
            PortHolder::Free => None,
            PortHolder::Group(pid) | PortHolder::Other(pid) => Some(StartProblem::port_taken(
                listen.name.clone(),
                pid.into_iter().collect(),
            )),
            PortHolder::Unknown(error) => {
                notes.push(listener_unknown(process, listen, &error));
                None
            }
        }
    }

    /// Returns what the run record says of `process` of `app`: whether
    /// its keeper or its process, as the record names them, still runs.
    fn recorded(&self, app: &AppPlan, process: &ProcessPlan) -> Result<Recorded, PhaseError> {
        let Some(record) = self.journal.record() else {
            return Ok(Recorded::Nothing);
        };
        let (keeper, entry) = record.host_pair(&app.name, &process.name);
        let mut ours = false;
        let mut left = false;
        let mut unverified = None;
        let mut keeper_runs = false;
        let mut keeper_identity = Identity::Gone;
        for (is_keeper, entry) in [(true, keeper), (false, entry)] {
            let Some(entry) = entry else {
                continue;
            };
            let Some(host) = entry.host.as_ref().filter(|_| entry.live()) else {
                continue;
            };
            let mut identity = self.engine.host.identity(host)?;
            if is_keeper {
                keeper_identity = identity;
            } else {
                identity = vouched(keeper_identity, identity);
            }
            match identity {
                Identity::Ours if entry.state == EntryState::Stopping => left = true,
                Identity::Ours => {
                    ours = true;
                    keeper_runs |= is_keeper;
                }
                // A group whose leader has gone and that nothing proves
                // ours may be a stranger's, so `up` starts nothing in its
                // place and reports it as left.
                Identity::LeaderGone => unverified = Some(host.group),
                Identity::Unknown => return Ok(Recorded::Unknown),
                Identity::Reused | Identity::Gone => {}
            }
        }
        // A running keeper still supervises its app, even when the app's
        // entry reads `stopping`, so the process counts as running.
        Ok(if keeper_runs {
            Recorded::Ours
        } else if let Some(group) = unverified {
            Recorded::Unverified(group)
        } else if left {
            Recorded::Left
        } else if ours {
            Recorded::Ours
        } else {
            Recorded::Nothing
        })
    }

    /// Starts the keeper of `process` of `app` with `environment`, after
    /// opening a follower of its log, and returns the keeper, the started
    /// process, the reader of how it ended and the follower, or the step
    /// that ends the start.
    fn launch(
        &self,
        app: &AppPlan,
        process: &ProcessPlan,
        environment: &dyn AppliedEnvironment,
    ) -> Result<(HostProcess, HostProcess, AppEnd, Box<dyn LogLines + Send>), Step> {
        let engine = self.engine;
        let unstartable = |reason: String| {
            Step::Errored(PhaseError::KeeperUnstartable {
                app: app.name.clone(),
                process: process.name.clone(),
                reason,
            })
        };
        let Some(command) = engine.settings.keeper else {
            return Err(unstartable(
                "the program that links phase gave no keeper program".to_owned(),
            ));
        };
        let log = engine.log_path(&self.workspace, &app.name, &process.name);
        let spec = KeeperSpec::new(
            process.run.clone(),
            process_dir(app, process),
            log.clone(),
            process.restart.clone(),
            process.stop,
            engine.settings.kill_wait,
        )
        .expiring(environment.expires_at())
        .inheriting(engine.settings.inherited.clone())
        .map_err(|error| unstartable(error.to_string()))?;
        let lines = engine.host.follow_log(&log).map_err(Step::Errored)?;
        let args = KeeperArgs::new(
            self.workspace.clone(),
            app.name.clone(),
            process.name.clone(),
            engine.settings.state_dir.to_path_buf(),
        );
        let launched = engine.host.launch(&Launch {
            command,
            args: &args,
            spec: &spec,
            environment,
            deadline: self.deadlines.ready,
        });
        match launched {
            Ok(kept) => {
                let (keeper, app, end) = kept.detach();
                Ok((keeper, app, end, lines))
            }
            Err(_) if now() >= self.deadlines.ready => Err(Step::Uncertain),
            Err(error) => Err(Step::Errored(error)),
        }
    }

    /// Waits until the started `target` is ready, as `process` says. When
    /// the process ends first, the failed start holds the code or signal
    /// that `end` reads from the keeper within [`END_LINE_WAIT`].
    fn await_ready(
        &self,
        app: &AppPlan,
        process: &ProcessPlan,
        target: &Target,
        environment: &dyn AppliedEnvironment,
        end: AppEnd,
        lines: Box<dyn LogLines + Send>,
    ) -> Step {
        let host = self.engine.host;
        let began = now();
        let own_deadline = began + process.ready_timeout;
        let deadline = own_deadline.min(self.deadlines.ready);
        let mut tail = Tail::new(lines);
        let mut ended = || {
            target
                .host
                .as_ref()
                .is_some_and(|app| host.identity(app).is_ok_and(is_gone))
        };
        let outcome = match &process.ready {
            None => {
                let grace_end = began + self.engine.settings.start_grace;
                match watch(grace_end.min(deadline), &mut ended, self.cancel) {
                    // The readiness deadline cut the watch short, so the
                    // process may still end within its start grace.
                    Outcome::Ready if self.deadlines.ready < grace_end => return Step::Uncertain,
                    outcome => outcome,
                }
            }
            Some(probe) => {
                let inherited = &self.engine.settings.inherited;
                let exec = HostExec {
                    environment,
                    path: inherited
                        .get("PATH")
                        .map(OsString::from)
                        .unwrap_or_default(),
                    home: inherited
                        .get("HOME")
                        .map(OsString::from)
                        .unwrap_or_default(),
                    dir: process_dir(app, process),
                };
                let mut check = match check(app, process, &probe.check, &exec, &mut tail) {
                    Ok(check) => check,
                    Err(error) => return Step::Errored(error),
                };
                probe::wait(
                    probe,
                    &mut check,
                    Goal::Ready,
                    deadline,
                    &mut ended,
                    self.cancel,
                )
            }
        };
        match outcome {
            Outcome::Ready => Step::Ready,
            Outcome::Cancelled => Step::Cancelled,
            Outcome::Ended => {
                let exit = end.wait(now() + END_LINE_WAIT, self.cancel);
                Step::Failed(StartProblem::exited(
                    exit.and_then(Exit::code),
                    exit.and_then(Exit::signal),
                    tail.finish(),
                ))
            }
            Outcome::NotReady { .. } | Outcome::Failing { .. } if own_deadline > deadline => {
                Step::Uncertain
            }
            Outcome::NotReady { waited, .. } => Step::Failed(StartProblem::not_ready(waited)),
            Outcome::Failing { .. } => Step::Failed(StartProblem::not_ready(now() - began)),
        }
    }

    /// Stops what this operation started from position `mark` on, in
    /// reverse order, adds the notes of each app's stops to `by_app` and
    /// to `notes`, and reports whether every stop left nothing behind. It
    /// goes on stopping after a failure, and returns the first failure
    /// once it has tried every stop.
    fn stop_since(
        &mut self,
        mark: usize,
        by_app: &mut StopsByApp,
        notes: &mut Vec<Note>,
    ) -> Result<bool, PhaseError> {
        let begun: Vec<Begun> = self.started.drain(mark..).rev().collect();
        let mut all = true;
        let mut first_error = None;
        for begun in begun {
            let (app_notes, app_all) = by_app
                .entry(begun.app().clone())
                .or_insert_with(|| (Vec::new(), true));
            let mut found = Vec::new();
            let stopped = self.stop_begun(&begun, &mut found);
            notes.extend(found.iter().cloned());
            app_notes.extend(found);
            match stopped {
                Ok(stopped) => {
                    *app_all &= stopped;
                    all &= stopped;
                }
                Err(error) => {
                    *app_all = false;
                    all = false;
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(all),
        }
    }

    /// Stops `begun` with a deadline that gives it its grace and twice the
    /// kill wait from now, and reports whether nothing of it is left.
    fn stop_begun(&mut self, begun: &Begun, notes: &mut Vec<Note>) -> Result<bool, PhaseError> {
        let kill_wait = self.engine.settings.kill_wait;
        match begun {
            Begun::Host(target) => {
                let deadline = deadline_in(
                    target
                        .stop
                        .grace
                        .saturating_add(kill_wait.saturating_mul(2)),
                );
                let stopped =
                    stop::stop_target(self.engine, &mut self.journal, target, deadline, notes)?;
                Ok(stopped == Stopped::Stopped)
            }
            Begun::Containers(app) => {
                let deadline =
                    deadline_in(DEFAULT_STOP_GRACE.saturating_add(kill_wait.saturating_mul(2)));
                let containers = self.recorded_containers(app, None);
                stop::stop_containers(self.engine, &mut self.journal, app, &containers, deadline)
            }
        }
    }

    /// Returns the containers of `app` that the record names as not
    /// stopped, of the processes in `only` when it is given.
    fn recorded_containers(
        &self,
        app: &AppName,
        only: Option<&BTreeSet<&ProcessName>>,
    ) -> Vec<ContainerEntry> {
        self.journal
            .record()
            .map(|record| {
                record
                    .containers
                    .iter()
                    .filter(|entry| {
                        &entry.app == app
                            && only.is_none_or(|only| only.contains(&entry.process))
                            && entry.state != EntryState::Stopped
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Stops everything this operation started, and returns the report of
    /// a cancelled operation. The apps it reported started are stopped,
    /// the app it was starting, the first of `rest`, is stopped with the
    /// `notes` it collected, and the rest did not start. Each report keeps
    /// the notes its app collected while it started.
    ///
    /// # Errors
    ///
    /// Returns the first failure of the stops, once every stop is tried.
    fn cancelled(
        &mut self,
        reports: Vec<AppReport>,
        notes: Vec<Note>,
        rest: &[Chosen<'_>],
    ) -> Result<Report, PhaseError> {
        let mut by_app = StopsByApp::new();
        self.stop_since(0, &mut by_app, &mut Vec::new())?;
        let stopped = |by_app: &mut StopsByApp, app: &AppName, mut kept: Vec<Note>| {
            by_app.remove(app).map(|(found, all)| {
                kept.extend(found);
                let outcome = if all {
                    AppOutcome::Stopped
                } else {
                    AppOutcome::Left
                };
                AppReport::new(app.clone(), outcome).with_notes(kept)
            })
        };
        let mut apps: Vec<AppReport> = reports
            .into_iter()
            .map(|report| match report.outcome {
                AppOutcome::Started | AppOutcome::Restarted => {
                    stopped(&mut by_app, &report.app, report.notes.clone()).unwrap_or(report)
                }
                _ => report,
            })
            .collect();
        let mut current = Some(notes);
        for choice in rest {
            let name = choice.app.name.clone();
            let kept = current.take().unwrap_or_default();
            let report = stopped(&mut by_app, &name, kept.clone())
                .unwrap_or_else(|| AppReport::new(name, AppOutcome::NotStarted).with_notes(kept));
            apps.push(report);
        }
        Ok(Report::new(apps))
    }

    /// Stops everything this operation started after `error`, which ends
    /// the operation, and returns `error`. A failure of the stops is not
    /// returned, because `error` ended the operation first and the record
    /// keeps what was not stopped.
    fn abort(&mut self, error: PhaseError) -> PhaseError {
        let stops = self.stop_since(0, &mut StopsByApp::new(), &mut Vec::new());
        drop(stops);
        error
    }

    /// Carries out `restart` for `chosen`.
    fn restart(&mut self, chosen: &[Chosen<'_>]) -> Result<Report, PhaseError> {
        let mut running: Vec<(&Chosen<'_>, Vec<Target>)> = Vec::new();
        let mut reports = Vec::new();
        for choice in chosen {
            let targets = stop::running_targets(
                self.engine,
                self.journal.record(),
                &choice.app.name,
                &choice.processes,
            )?;
            if targets.is_empty() && choice.app.runner == RunnerKind::Host {
                reports.push(AppReport::new(
                    choice.app.name.clone(),
                    AppOutcome::NotRunning,
                ));
            } else {
                running.push((choice, targets));
            }
        }
        let mut fresh: BTreeMap<AppName, BTreeMap<ProcessName, Box<dyn AppliedEnvironment>>> =
            BTreeMap::new();
        for (choice, targets) in &running {
            for target in targets {
                let environment = self
                    .env
                    .resolve(&target.app, &target.process, self.deadlines.ready)
                    .map_err(|source| PhaseError::Environment {
                        app: choice.app.name.clone(),
                        source,
                    })?;
                fresh
                    .entry(target.app.clone())
                    .or_default()
                    .insert(target.process.clone(), environment);
            }
        }
        for (choice, targets) in &running {
            if self.cancel.is_cancelled() {
                reports.push(AppReport::new(
                    choice.app.name.clone(),
                    AppOutcome::Untouched,
                ));
                continue;
            }
            let report = match self.restart_app(choice, targets, fresh.remove(&choice.app.name)) {
                Ok(report) => report,
                Err(error) => return Err(self.abort(error)),
            };
            reports.push(report);
        }
        Ok(Report::new(reports))
    }

    /// Returns the outcome of a restart's stop that left something behind:
    /// uncertain when the caller's deadline has passed, and left otherwise.
    fn left_by_stop(&self) -> AppOutcome {
        if now() >= self.deadlines.ready {
            AppOutcome::StopUncertain
        } else {
            AppOutcome::Left
        }
    }

    /// Stops the running `targets` of `choice` and starts them again with
    /// the environments of `fresh`. The stops give up at the caller's
    /// deadline. When the caller cancels while the app starts, the restart
    /// stops what it started of this app and leaves the apps it already
    /// restarted running.
    fn restart_app(
        &mut self,
        choice: &Chosen<'_>,
        targets: &[Target],
        fresh: Option<BTreeMap<ProcessName, Box<dyn AppliedEnvironment>>>,
    ) -> Result<AppReport, PhaseError> {
        let name = choice.app.name.clone();
        let mut notes = Vec::new();
        if choice.app.runner == RunnerKind::Container {
            return self.restart_containers(choice);
        }
        let deadline = self.deadlines.ready;
        for target in targets.iter().rev() {
            let stopped =
                stop::stop_target(self.engine, &mut self.journal, target, deadline, &mut notes)?;
            if stopped != Stopped::Stopped {
                return Ok(AppReport::new(name, self.left_by_stop()).with_notes(notes));
            }
        }
        let only: BTreeSet<&ProcessName> = targets.iter().map(|target| &target.process).collect();
        let again = Chosen {
            app: choice.app,
            processes: choice
                .processes
                .iter()
                .copied()
                .filter(|process| only.contains(&process.name))
                .collect(),
        };
        let mut fresh = fresh.unwrap_or_default();
        let mark = self.started.len();
        let step = self.start_app(&again, Some(&mut fresh))?;
        Ok(match step {
            AppStep::Done(AppOutcome::Started | AppOutcome::AlreadyRunning, more) => {
                notes.extend(more);
                AppReport::new(name, AppOutcome::Restarted).with_notes(notes)
            }
            AppStep::Done(outcome, more) => {
                notes.extend(more);
                AppReport::new(name, outcome).with_notes(notes)
            }
            AppStep::Cancelled(more) => {
                notes.extend(more);
                self.cancelled_restart(name, mark, notes)?
            }
        })
    }

    /// Stops what a cancelled restart started of `app` since `mark`, and
    /// reports the app with `notes`.
    fn cancelled_restart(
        &mut self,
        app: AppName,
        mark: usize,
        mut notes: Vec<Note>,
    ) -> Result<AppReport, PhaseError> {
        let all = self.stop_since(mark, &mut StopsByApp::new(), &mut notes)?;
        let outcome = if all {
            AppOutcome::Stopped
        } else {
            AppOutcome::Left
        };
        Ok(AppReport::new(app, outcome).with_notes(notes))
    }

    /// Stops the recorded containers of the container app `choice` and
    /// starts them again through the container runner.
    fn restart_containers(&mut self, choice: &Chosen<'_>) -> Result<AppReport, PhaseError> {
        let name = choice.app.name.clone();
        let only: BTreeSet<&ProcessName> = choice
            .processes
            .iter()
            .map(|process| &process.name)
            .collect();
        let containers = self.recorded_containers(&name, Some(&only));
        if containers.is_empty() {
            return Ok(AppReport::new(name, AppOutcome::NotRunning));
        }
        let deadline = self.deadlines.ready;
        match stop::stop_containers(self.engine, &mut self.journal, &name, &containers, deadline) {
            Ok(true) => {}
            Ok(false) => return Ok(AppReport::new(name, self.left_by_stop())),
            Err(error) => return Ok(AppReport::new(name, AppOutcome::errored(&error))),
        }
        let mark = self.started.len();
        Ok(match self.start_containers(choice)? {
            AppStep::Done(AppOutcome::Started, notes) => {
                AppReport::new(name, AppOutcome::Restarted).with_notes(notes)
            }
            AppStep::Done(outcome, notes) => AppReport::new(name, outcome).with_notes(notes),
            AppStep::Cancelled(notes) => self.cancelled_restart(name, mark, notes)?,
        })
    }
}

/// Returns why phase cannot run the host processes `processes` of `app`
/// yet, if it cannot. A release and a checkout with a build need their
/// program settled first, which a later release does, and a oneshot needs
/// its exit status.
fn runnable(app: &AppPlan, processes: &[&ProcessPlan]) -> Result<(), PhaseError> {
    let refuse = |reason| {
        Err(PhaseError::NotRunnable {
            app: app.name.clone(),
            reason,
        })
    };
    match &app.source {
        Source::Release { .. } => return refuse(NotRunnable::Release),
        Source::Checkout { build: Some(_), .. } => return refuse(NotRunnable::CheckoutBuild),
        Source::Checkout { build: None, .. } | Source::Image { .. } => {}
    }
    if processes
        .iter()
        .any(|process| matches!(process.kind, ProcessKind::Oneshot { .. }))
    {
        return refuse(NotRunnable::Oneshot);
    }
    Ok(())
}

/// Returns the port that `process` of `app` listens on, or `None`.
fn listen(app: &AppPlan, process: &ProcessPlan) -> Option<Listen> {
    let name = process.port.as_ref()?;
    let port = app.ports.get(name)?;
    Some(Listen {
        name: name.clone(),
        port: port.get(),
    })
}

/// Returns the directory that `process` of `app` starts in: the app's
/// checkout, or the process's directory within it.
fn process_dir(app: &AppPlan, process: &ProcessPlan) -> PathBuf {
    let base = match &app.source {
        Source::Checkout { dir, .. } => dir.clone(),
        Source::Release { .. } | Source::Image { .. } => PathBuf::from("/"),
    };
    process
        .cwd
        .as_ref()
        .map_or(base.clone(), |cwd| base.join(cwd))
}

/// The reason of the note on a port whose holder phase may not read.
const UNREADABLE_HOLDER: &str = "a process that phase may not read holds the port";

/// What phase can tell of the process that holds a port.
#[derive(Debug)]
pub(super) enum PortHolder {
    /// Nothing listens on the port.
    Free,
    /// A member of the process group, or a process that descends from its
    /// leader, with its pid when it is not a member.
    Group(Option<u32>),
    /// Another process, with its pid when phase may read it.
    Other(Option<u32>),
    /// Phase cannot tell, for this reason.
    Unknown(PhaseError),
}

/// What [`port_holder`] does when the listener lookup fails.
#[derive(Clone, Copy, Debug)]
pub(super) enum LookupFailed {
    /// A bind and a connect stand in for the lookup, as before a launch,
    /// where a port in use must block the start.
    Bind,
    /// The holder is unknown, with the lookup's error as the reason, as
    /// after readiness and after a stop, where only a note follows.
    Report,
}

/// Returns what holds `port`, as seen from the process group `group`. The
/// listener lookup cannot tell a port that nothing holds from one that a
/// process phase may not read holds, so a bind and a connect decide
/// between them. When the lookup fails, `failed` says whether they stand
/// in for it.
pub(super) fn port_holder(
    host: &dyn HostRunner,
    group: u32,
    port: u16,
    failed: LookupFailed,
) -> PortHolder {
    let failed = match host.listener(group, port) {
        Ok(Some(Holder::Group)) => return PortHolder::Group(None),
        Ok(Some(Holder::Descendant(pid))) => return PortHolder::Group(Some(pid)),
        Ok(Some(Holder::Other(Some(pid)))) => return PortHolder::Other(Some(pid)),
        Ok(Some(Holder::Other(None)) | None) => None,
        Err(error) => match failed {
            LookupFailed::Bind => Some(error),
            LookupFailed::Report => return PortHolder::Unknown(error),
        },
    };
    match (host.port_in_use(port), failed) {
        (Ok(true), _) => PortHolder::Other(None),
        (Ok(false), None) => PortHolder::Free,
        (Ok(false), Some(error)) | (Err(error), _) => PortHolder::Unknown(error),
    }
}

/// Returns the note that phase could not find the listener of `listen`.
fn listener_unknown(process: &ProcessPlan, listen: &Listen, error: &PhaseError) -> Note {
    Note::listener_unknown(process.name.clone(), listen.name.clone(), error.to_string())
}

/// Returns the check of `probe` against `process` of `app`.
fn check<'a>(
    app: &AppPlan,
    process: &ProcessPlan,
    probe: &'a ProbeCheck,
    exec: &'a HostExec<'a>,
    lines: &'a mut Tail,
) -> Result<Check<'a>, PhaseError> {
    let unusable = |reason: String| PhaseError::ProcessUnstartable {
        app: app.name.clone(),
        process: process.name.clone(),
        reason,
    };
    let port = |name: Option<&crate::name::PortName>| {
        name.and_then(|name| app.ports.get(name))
            .map(|port| port.get())
            .ok_or_else(|| {
                unusable("the readiness probe's port is not one of the app's".to_owned())
            })
    };
    Ok(match probe {
        ProbeCheck::Http { path } => Check::Http {
            port: port(process.port.as_ref())?,
            path,
        },
        ProbeCheck::Tcp { port: name } => Check::Tcp {
            port: port(Some(name))?,
        },
        ProbeCheck::Command { run } => Check::Command { run, exec },
        ProbeCheck::Log { pattern } => Check::Log {
            watch: LogWatch::new(
                LogPattern::new(pattern).map_err(|error| unusable(error.to_string()))?,
            ),
            lines,
        },
    })
}

/// Watches a process that has no readiness probe until `until`, and
/// returns [`Outcome::Ended`] when it ends first.
pub(crate) fn watch(until: Instant, ended: &mut dyn FnMut() -> bool, cancel: &Cancel) -> Outcome {
    loop {
        if cancel.is_cancelled() {
            return Outcome::Cancelled;
        }
        if ended() {
            return Outcome::Ended;
        }
        let left = until.saturating_duration_since(now());
        if left.is_zero() {
            return Outcome::Ready;
        }
        pause(POLL_INTERVAL.min(left));
    }
}

/// A follower of a process's log that keeps its last [`TAIL_LINES`]
/// lines, for the report of a process that ends before it is ready.
struct Tail {
    /// The follower.
    lines: Box<dyn LogLines + Send>,
    /// The last lines read.
    last: VecDeque<String>,
}

impl Tail {
    /// Returns a tail of `lines` that has read nothing.
    fn new(lines: Box<dyn LogLines + Send>) -> Self {
        Self {
            lines,
            last: VecDeque::new(),
        }
    }

    /// Reads the last part of the log up to its end, and returns the last
    /// lines. A log that cannot be read leaves the lines read so far.
    fn finish(&mut self) -> Vec<String> {
        if let Ok(lines) = self.lines.rest() {
            self.keep(&lines);
        }
        self.last.drain(..).collect()
    }

    /// Keeps the last [`TAIL_LINES`] of the lines read and `lines`.
    fn keep(&mut self, lines: &[String]) {
        for line in lines {
            if self.last.len() == TAIL_LINES {
                self.last.pop_front();
            }
            self.last.push_back(line.clone());
        }
    }
}

impl LogLines for Tail {
    fn new_lines(&mut self) -> io::Result<Vec<String>> {
        let lines = self.lines.new_lines()?;
        self.keep(&lines);
        Ok(lines)
    }
}
