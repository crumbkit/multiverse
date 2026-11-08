use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use tempfile::TempDir;

use super::{Engine, Settings};

mod drift;
mod oneshot;
use crate::cancel::Cancel;
use crate::clock::now;
use crate::engine::containers::{
    Before, ContainerObjects, ContainerRewrite, ContainerRunner, ContainerStart, ContainerStop,
    ContainerStopped, ContainersStarted, Rewritten, StartedContainer,
};
use crate::engine::host::{Holder, HostRunner, HostStop, HostStopped};
use crate::engine::keeper::{AppEnd, KeeperCommand, Kept, Launch};
use crate::engine::probe::LogLines;
use crate::env::{AppliedEnvironment, EnvironmentError, EnvironmentSource};
use crate::error::PhaseError;
use crate::name::{AppName, ProcessName};
use crate::phase::UpDeadlines;
use crate::plan::probe::{Probe, ProbeCheck};
use crate::plan::{AppPlan, Plan, ProcessPlan, RunnerKind};
use crate::record::DockerName;
use crate::record::docker::DockerError;
use crate::record::process::Identity;
use crate::record::run::ContainerEntry;
use crate::record::run::PlanHash;
use crate::record::run::{EntryState, Exit, HostProcess, ObjectKey, RunRecord};
use crate::record::store::RunStore;
use crate::record::testing::workspace;
use crate::report::{AppOutcome, Note, Report, StartProblem};
use crate::select::{Selection, Target};
use crate::source::Source;

/// The value of the secret that the tests' environment source resolves.
const SECRET: &str = "test-secret-value-7f3a";

/// What the fake listener of a port reports.
#[derive(Clone, Copy, Debug)]
enum Listener {
    /// The process `pid`, in its own group, holds the port.
    Held(u32),
    /// The process `pid` holds the port once something has been launched.
    HeldAfterLaunch(u32),
    /// The started app's own group holds the port.
    Own,
    /// The listener cannot be found.
    Unknown,
    /// The listener cannot be found, and a bind finds the port in use.
    UnknownHeld,
    /// A process that phase may not read holds the port.
    Unreadable,
    /// A process that phase may not read holds the port once something
    /// has been launched.
    UnreadableAfterLaunch,
    /// The listener cannot be found once something has been launched, and
    /// a bind then finds the port in use.
    UnknownHeldAfterLaunch,
}

/// A fake host runner, which keeps its processes in memory.
#[derive(Default)]
struct FakeHost {
    /// The state, which the runner's methods change.
    state: RefCell<HostState>,
}

#[derive(Default)]
struct HostState {
    /// The last pid handed out.
    last_pid: u32,
    /// The pids that are running.
    alive: BTreeSet<u32>,
    /// The `app/process` of each pid.
    names: BTreeMap<u32, String>,
    /// Each `app/process` launched, in order.
    launches: Vec<String>,
    /// Each `app/process` stopped, in order.
    stops: Vec<String>,
    /// Each launched keeper's specification as JSON.
    specs: Vec<String>,
    /// The `app/process` whose app ends right after it starts.
    ends: BTreeSet<String>,
    /// How the keeper of each `app/process` reports that its app ended.
    exits: BTreeMap<String, Exit>,
    /// The `app/process` that cannot start.
    unstartable: BTreeSet<String>,
    /// The `app/process` whose stop finds a process phase may not read.
    not_ours: BTreeSet<String>,
    /// The `app/process` whose app survives `KILL`.
    unstoppable: BTreeSet<String>,
    /// The `app/process` whose stop finds a group whose leader has gone
    /// and that nothing proves phase's own.
    unverified: BTreeSet<String>,
    /// The pids whose process has ended and left members in its group.
    leaderless: BTreeSet<u32>,
    /// The log lines that each log file's follower hands over, by file name.
    logs: BTreeMap<String, Vec<String>>,
    /// What listens on each port.
    listeners: BTreeMap<u16, Listener>,
    /// The run record as it was at each launch.
    records_at_launch: Vec<Option<RunRecord>>,
    /// The state directory, whose record a launch reads.
    state_dir: Option<PathBuf>,
    /// The `app/process` whose launch cancels the up, and its cancel.
    cancels_at_launch: Option<(String, Cancel)>,
    /// The `app/process` whose launch makes the run record unwritable.
    breaks_record_at_launch: Option<String>,
    /// The keeper and the app pid of each `app/process`, as last launched.
    pids: BTreeMap<String, (u32, u32)>,
    /// When each stop was asked for, and its deadline.
    stop_calls: Vec<(Instant, Instant)>,
}

impl FakeHost {
    /// Ends the app of `name` and leaves its keeper running, as a keeper
    /// in restart backoff is.
    fn end_app_and_keep_keeper(&self, name: &str) {
        let mut state = self.state.borrow_mut();
        let (_, app) = state.pids[name];
        state.alive.remove(&app);
    }

    /// Ends the app of `name` and leaves members in its group, and ends
    /// its keeper too unless `keep_keeper`.
    fn end_leader(&self, name: &str, keep_keeper: bool) {
        let mut state = self.state.borrow_mut();
        let (keeper, app) = state.pids[name];
        state.alive.remove(&app);
        state.leaderless.insert(app);
        if !keep_keeper {
            state.alive.remove(&keeper);
        }
    }

    fn breaks_record_at_launch(&self, name: &str) {
        self.state.borrow_mut().breaks_record_at_launch = Some(name.to_owned());
    }

    fn stop_calls(&self) -> Vec<(Instant, Instant)> {
        self.state.borrow().stop_calls.clone()
    }

    fn cancels_at_launch(&self, name: &str, cancel: &Cancel) {
        self.state.borrow_mut().cancels_at_launch = Some((name.to_owned(), cancel.clone()));
    }

    fn ends(&self, name: &str) {
        self.state.borrow_mut().ends.insert(name.to_owned());
    }

    /// Ends the app of `name` right after it starts, and has its keeper
    /// report that it ended as `exit`.
    fn ends_as(&self, name: &str, exit: Exit) {
        self.ends(name);
        self.state.borrow_mut().exits.insert(name.to_owned(), exit);
    }

    fn launches(&self) -> Vec<String> {
        self.state.borrow().launches.clone()
    }

    fn stops(&self) -> Vec<String> {
        self.state.borrow().stops.clone()
    }

    fn listen(&self, port: u16, listener: Listener) {
        self.state.borrow_mut().listeners.insert(port, listener);
    }

    fn running(&self) -> usize {
        self.state.borrow().alive.len()
    }
}

impl HostRunner for FakeHost {
    fn launch(&self, launch: &Launch<'_>) -> Result<Kept, PhaseError> {
        let mut state = self.state.borrow_mut();
        let name = format!("{}/{}", launch.args.app(), launch.args.process());
        if let Some(dir) = &state.state_dir {
            let record = RunStore::new(dir).read(launch.args.workspace()).unwrap();
            state.records_at_launch.push(record);
        }
        if let Some((at, cancel)) = &state.cancels_at_launch
            && *at == name
        {
            cancel.cancel();
        }
        state.launches.push(name.clone());
        event(format!("host start {name}"));
        state
            .specs
            .push(serde_json::to_string(launch.spec).unwrap());
        if state.unstartable.contains(&name) {
            return Err(PhaseError::ProcessUnstartable {
                app: launch.args.app().clone(),
                process: launch.args.process().clone(),
                reason: "the program does not exist".to_owned(),
            });
        }
        let keeper = state.last_pid.max(1000) + 1;
        let app = keeper + 1;
        state.last_pid = app;
        state.alive.insert(keeper);
        if !state.ends.contains(&name) {
            state.alive.insert(app);
        }
        state.names.insert(keeper, name.clone());
        state.names.insert(app, name.clone());
        state.pids.insert(name.clone(), (keeper, app));
        if state.breaks_record_at_launch.as_ref() == Some(&name)
            && let Some(dir) = &state.state_dir
        {
            set_mode(&dir.join("runs"), 0o500);
        }
        let host = |pid| HostProcess {
            pid,
            started_at_us: u64::from(pid),
            group: pid,
            boot_id: None,
        };
        let kept = Kept::detached(host(keeper), host(app));
        Ok(match state.exits.get(&name) {
            Some(exit) => kept.ending(AppEnd::known(*exit)),
            None => kept,
        })
    }

    fn stop(&self, stop: &HostStop<'_>, _deadline: Instant) -> Result<HostStopped, PhaseError> {
        let mut state = self.state.borrow_mut();
        state.stop_calls.push((now(), _deadline));
        let pid = stop.app.or(stop.keeper).map_or(0, |host| host.pid);
        let name = state.names.get(&pid).cloned().unwrap_or_default();
        state.stops.push(name.clone());
        event(format!("host stop {name}"));
        if state.not_ours.contains(&name) {
            return Ok(HostStopped::NotOurs);
        }
        if state.unstoppable.contains(&name) {
            return Ok(HostStopped::Unstoppable { group: pid });
        }
        if state.unverified.contains(&name) {
            return Ok(HostStopped::GroupUnverified { group: pid });
        }
        let mut any = false;
        for host in [stop.keeper, stop.app].into_iter().flatten() {
            any |= state.alive.remove(&host.pid);
            state
                .listeners
                .retain(|_, holder| !matches!(holder, Listener::Held(pid) if *pid == host.pid));
        }
        Ok(if any {
            HostStopped::Stopped
        } else {
            HostStopped::AlreadyGone
        })
    }

    fn identity(&self, process: &HostProcess) -> Result<Identity, PhaseError> {
        let state = self.state.borrow();
        Ok(if state.alive.contains(&process.pid) {
            Identity::Ours
        } else if state.leaderless.contains(&process.pid) {
            Identity::LeaderGone
        } else {
            Identity::Gone
        })
    }

    fn follow_log(&self, log: &Path) -> Result<Box<dyn LogLines + Send>, PhaseError> {
        let name = log.file_name().unwrap().to_string_lossy().into_owned();
        let lines = self
            .state
            .borrow_mut()
            .logs
            .remove(&name)
            .unwrap_or_default();
        Ok(Box::new(FakeLines(Some(lines))))
    }

    fn listener(&self, group: u32, port: u16) -> Result<Option<Holder>, PhaseError> {
        let state = self.state.borrow();
        let held = |pid: Option<u32>| match pid {
            Some(pid) if pid == group => Holder::Group,
            pid => Holder::Other(pid),
        };
        match state.listeners.get(&port) {
            None | Some(Listener::Unreadable) => Ok(Some(held(None))),
            Some(Listener::Held(pid)) => Ok(Some(held(Some(*pid)))),
            Some(Listener::HeldAfterLaunch(pid)) if !state.launches.is_empty() => {
                Ok(Some(held(Some(*pid))))
            }
            Some(
                Listener::HeldAfterLaunch(_)
                | Listener::UnreadableAfterLaunch
                | Listener::UnknownHeldAfterLaunch,
            ) if state.launches.is_empty() => Ok(Some(held(None))),
            Some(Listener::HeldAfterLaunch(_) | Listener::UnreadableAfterLaunch) => {
                Ok(Some(held(None)))
            }
            Some(Listener::Own) => Ok(Some(held(state.alive.last().copied()))),
            Some(Listener::Unknown | Listener::UnknownHeld | Listener::UnknownHeldAfterLaunch) => {
                Err(PhaseError::ProcessTable {
                    message: "the test cannot find listeners".to_owned(),
                })
            }
        }
    }

    fn port_in_use(&self, port: u16) -> Result<bool, PhaseError> {
        let state = self.state.borrow();
        Ok(match state.listeners.get(&port) {
            Some(Listener::Held(_) | Listener::UnknownHeld | Listener::Unreadable) => true,
            Some(Listener::UnreadableAfterLaunch | Listener::UnknownHeldAfterLaunch) => {
                !state.launches.is_empty()
            }
            _ => false,
        })
    }
}

/// Sets the mode of `path`.
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Log lines that a follower hands over on its first call.
struct FakeLines(Option<Vec<String>>);

impl LogLines for FakeLines {
    fn new_lines(&mut self) -> io::Result<Vec<String>> {
        Ok(self.0.take().unwrap_or_default())
    }
}

std::thread_local! {
    /// What the fake runners did, in order, across host and container
    /// apps.
    static EVENTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Notes that a fake runner did `what`.
fn event(what: String) {
    EVENTS.with(|events| events.borrow_mut().push(what));
}

/// Returns what the fake runners did, in order, and forgets it.
fn events() -> Vec<String> {
    EVENTS.with(|events| std::mem::take(&mut *events.borrow_mut()))
}

/// A fake container runner.
#[derive(Default)]
struct FakeContainers {
    /// Each app started, in order.
    started: RefCell<Vec<String>>,
    /// Each app stopped, in order.
    stopped: RefCell<Vec<String>>,
    /// Whether a stop leaves each container in place.
    stubborn: bool,
    /// The state directory, whose run record each start reads.
    state_dir: RefCell<Option<PathBuf>>,
    /// The state and id of each process's container entry, as the run
    /// record held them when the runner was asked to start it.
    entries_at_start: RefCell<Vec<(EntryState, bool)>>,
    /// The `app/process` containers that the runner reports as still
    /// running.
    running: RefCell<BTreeSet<String>>,
    /// The processes of each start, as `app/process`, in order.
    starts: RefCell<Vec<Vec<String>>>,
    /// The process whose container stop fails with a Docker error.
    stop_failing: Option<&'static str>,
    /// How a start ends after it creates every container, or `None` when
    /// every container becomes ready.
    start_ending: RefCell<Option<StartEnding>>,
    /// The processes whose containers each stop was asked for, in order.
    stop_order: RefCell<Vec<String>>,
    /// How the rewrite of the secret files of each `app/process` container
    /// ends, after it resolves the process's environment. A container that
    /// it does not name has no secret files.
    rewrite_ending: RefCell<BTreeMap<String, Rewritten>>,
    /// Each `app/process` whose secret files a rewrite was asked for, in
    /// order.
    rewrites: RefCell<Vec<String>>,
    /// The readiness that each container the runner starts reports.
    ready: RefCell<Option<crate::record::run::Ready>>,
    /// The `app/process` containers whose rewrite finds the Docker daemon
    /// unavailable.
    rewrite_unreachable: RefCell<BTreeSet<String>>,
}

/// How a fake container start ends after it creates every container.
#[derive(Clone, Copy)]
enum StartEnding {
    /// Docker fails, and the start removes the first container only.
    Failure,
    /// A deadline cuts the wait for the last container short.
    Uncertain,
}

/// The plan hash that the fake container runner gives every container.
fn fake_plan_hash() -> PlanHash {
    PlanHash::new([1; 32])
}

/// Returns the Docker failure of the fake container runner.
fn docker_refused() -> PhaseError {
    PhaseError::Docker {
        source: DockerError::refused("the fake daemon said no".to_owned()),
    }
}

impl ContainerRunner for FakeContainers {
    fn objects(
        &self,
        workspace: &crate::name::WorkspaceId,
        app: &AppPlan,
    ) -> Result<ContainerObjects, PhaseError> {
        Ok(ContainerObjects {
            containers: app
                .processes
                .iter()
                .map(|process| {
                    let name: DockerName = format!("mxv-{workspace}-{}-{}", app.name, process.name)
                        .parse()
                        .unwrap();
                    (process.name.clone(), name)
                })
                .collect(),
            ..ContainerObjects::default()
        })
    }

    fn start(
        &self,
        start: &ContainerStart<'_>,
        before: &mut dyn FnMut(Before<'_>) -> Result<(), PhaseError>,
    ) -> ContainersStarted {
        assert_eq!(start.workspace, &workspace());
        assert!(start.deadline > now());
        if start.cancel.is_cancelled() {
            return ContainersStarted::default();
        }
        for process in start.processes {
            if let Err(source) = start.env.resolve(&start.app.name, process, start.deadline) {
                return ContainersStarted {
                    failure: Some(PhaseError::Environment {
                        app: start.app.name.clone(),
                        source,
                    }),
                    ..ContainersStarted::default()
                };
            }
        }
        for process in start.processes {
            if let Err(error) = before(Before::Container(process)) {
                return ContainersStarted {
                    failure: Some(error),
                    ..ContainersStarted::default()
                };
            }
            if let Some(state_dir) = self.state_dir.borrow().as_ref() {
                let record = RunStore::new(state_dir)
                    .read(start.workspace)
                    .unwrap()
                    .unwrap();
                let entry = record
                    .containers
                    .iter()
                    .find(|entry| entry.app == start.app.name && &entry.process == process)
                    .unwrap();
                self.entries_at_start
                    .borrow_mut()
                    .push((entry.state, entry.id.is_some()));
            }
        }
        self.started.borrow_mut().push(start.app.name.to_string());
        for process in start.processes {
            event(format!("container start {}/{process}", start.app.name));
        }
        self.starts.borrow_mut().push(
            start
                .processes
                .iter()
                .map(|process| format!("{}/{process}", start.app.name))
                .collect(),
        );
        let mut started = ContainersStarted {
            containers: start
                .processes
                .iter()
                .map(|process| StartedContainer {
                    process: process.clone(),
                    id: "c0".repeat(32).try_into().unwrap(),
                    plan_hash: fake_plan_hash(),
                    ready: self.ready.borrow().clone(),
                })
                .collect(),
            ..ContainersStarted::default()
        };
        match *self.start_ending.borrow() {
            None => {}
            Some(StartEnding::Failure) => {
                started.failure = Some(docker_refused());
                started.removed = start.processes.iter().take(1).cloned().collect();
            }
            Some(StartEnding::Uncertain) => started.uncertain = start.processes.last().cloned(),
        }
        started
    }

    fn running(
        &self,
        workspace: &crate::name::WorkspaceId,
        app: &AppPlan,
        containers: &[ContainerEntry],
    ) -> Result<Vec<ProcessName>, PhaseError> {
        assert_eq!(workspace, &self::workspace());
        assert!(containers.iter().all(|entry| entry.id.is_some()));
        Ok(containers
            .iter()
            .filter(|entry| {
                entry.plan_hash == Some(fake_plan_hash())
                    && self
                        .running
                        .borrow()
                        .contains(&format!("{}/{}", app.name, entry.process))
            })
            .map(|entry| entry.process.clone())
            .collect())
    }

    fn started_at(
        &self,
        workspace: &crate::name::WorkspaceId,
        app: &AppName,
        containers: &[ContainerEntry],
    ) -> Result<Vec<(ProcessName, String)>, PhaseError> {
        assert_eq!(workspace, &self::workspace());
        // Each start of the app's containers is a new run of them.
        let starts = self
            .starts
            .borrow()
            .iter()
            .filter(|start| {
                start
                    .iter()
                    .any(|process| process.starts_with(&format!("{app}/")))
            })
            .count();
        Ok(containers
            .iter()
            .filter(|entry| entry.id.is_some())
            .map(|entry| (entry.process.clone(), format!("start-{starts}")))
            .collect())
    }

    fn rewrite(&self, rewrite: &ContainerRewrite<'_>) -> Result<Rewritten, PhaseError> {
        assert_eq!(rewrite.workspace, &workspace());
        assert!(rewrite.entry.id.is_some());
        let process = format!("{}/{}", rewrite.app.name, rewrite.entry.process);
        self.rewrites.borrow_mut().push(process.clone());
        if self.rewrite_unreachable.borrow().contains(&process) {
            return Err(PhaseError::Docker {
                source: DockerError::unavailable("the fake daemon does not answer".to_owned()),
            });
        }
        let Some(ending) = self.rewrite_ending.borrow().get(&process).cloned() else {
            return Ok(Rewritten::NoFiles);
        };
        match rewrite
            .env
            .resolve(&rewrite.app.name, &rewrite.entry.process, rewrite.deadline)
        {
            Ok(_) => Ok(ending),
            Err(source) => Ok(Rewritten::Kept {
                reason: source.to_string(),
            }),
        }
    }

    fn stop(
        &self,
        stop: &ContainerStop<'_>,
    ) -> Vec<(ProcessName, Result<ContainerStopped, PhaseError>)> {
        assert_eq!(stop.workspace, &workspace());
        assert!(stop.deadline > now());
        self.stopped.borrow_mut().push(stop.app.to_string());
        stop.containers
            .iter()
            .rev()
            .map(|entry| {
                self.stop_order
                    .borrow_mut()
                    .push(format!("{}/{}", stop.app, entry.process));
                event(format!("container stop {}/{}", stop.app, entry.process));
                let how = match (&entry.id, self.stubborn) {
                    _ if self.stop_failing == Some(entry.process.as_str()) => Err(docker_refused()),
                    (None, _) => Ok(ContainerStopped::AlreadyGone),
                    (Some(_), true) => Ok(ContainerStopped::Unstoppable),
                    (Some(_), false) => Ok(ContainerStopped::Stopped),
                };
                (entry.process.clone(), how)
            })
            .collect()
    }
}

/// An environment source that gives every process `API_TOKEN`, and fails
/// for the apps that `failing` names.
#[derive(Default)]
struct Env {
    /// The apps whose environment cannot be resolved.
    failing: BTreeSet<String>,
    /// The `app/process` processes whose environment cannot be resolved.
    failing_processes: BTreeSet<String>,
    /// Each `app/process` resolved, in order.
    resolved: RefCell<Vec<String>>,
    /// The apps whose environment the source resolves without any
    /// variable.
    empty: BTreeSet<String>,
}

impl EnvironmentSource for Env {
    fn resolve(
        &self,
        app: &AppName,
        process: &ProcessName,
        _deadline: Instant,
    ) -> Result<Box<dyn AppliedEnvironment>, EnvironmentError> {
        self.resolved.borrow_mut().push(format!("{app}/{process}"));
        if self.failing.contains(app.as_str())
            || self.failing_processes.contains(&format!("{app}/{process}"))
        {
            return Err(EnvironmentError::new(
                "API_TOKEN".to_owned(),
                "vault".to_owned(),
                "the vault is locked".into(),
            ));
        }
        if self.empty.contains(app.as_str()) {
            return Ok(Box::new(Nothing));
        }
        Ok(Box::new(Secret))
    }
}

/// An environment that holds no variable.
struct Nothing;

impl AppliedEnvironment for Nothing {
    fn names(&self) -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn expires_at(&self) -> Option<SystemTime> {
        None
    }

    fn digest(&self, key: &[u8; 32]) -> [u8; 32] {
        *key
    }

    fn apply_as(&self, _command: &mut Command, _reference: &dyn Fn(&str) -> Option<String>) {}
}

/// An environment that holds `API_TOKEN`.
struct Secret;

impl AppliedEnvironment for Secret {
    fn names(&self) -> BTreeSet<String> {
        BTreeSet::from(["API_TOKEN".to_owned()])
    }

    fn expires_at(&self) -> Option<SystemTime> {
        None
    }

    fn digest(&self, key: &[u8; 32]) -> [u8; 32] {
        *key
    }

    fn apply_as(&self, command: &mut Command, reference: &dyn Fn(&str) -> Option<String>) {
        if let Some(name) = reference("API_TOKEN") {
            command.env(name, SECRET);
        }
    }
}

/// A state directory, the fakes and the settings of an engine.
struct World {
    state: TempDir,
    host: FakeHost,
    containers: FakeContainers,
    keeper: KeeperCommand,
    inherited: BTreeMap<String, String>,
}

impl World {
    fn new() -> Self {
        Self::in_state(tempfile::tempdir().unwrap())
    }

    /// Returns a world whose state directory lies in the build's target
    /// directory, outside the per-user temporary directory, which the
    /// sandbox always denies, so a sandboxed app's spec can be built.
    fn outside_temp() -> Self {
        let target = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("sandbox-fixtures");
        std::fs::create_dir_all(&target).unwrap();
        Self::in_state(
            tempfile::Builder::new()
                .prefix("phase-run-")
                .tempdir_in(target)
                .unwrap(),
        )
    }

    fn in_state(state: TempDir) -> Self {
        let host = FakeHost::default();
        host.state.borrow_mut().state_dir = Some(state.path().to_path_buf());
        let containers = FakeContainers::default();
        *containers.state_dir.borrow_mut() = Some(state.path().to_path_buf());
        Self {
            state,
            host,
            containers,
            keeper: KeeperCommand::new(PathBuf::from("/usr/local/bin/mxv"), vec!["keep".into()]),
            inherited: BTreeMap::from([("PATH".to_owned(), "/usr/bin:/bin".to_owned())]),
        }
    }

    fn engine(&self) -> Engine<'_> {
        Engine {
            settings: Settings {
                state_dir: self.state.path(),
                lock_wait: Duration::from_millis(50),
                start_grace: Duration::from_millis(30),
                kill_wait: Duration::from_millis(20),
                keeper: Some(&self.keeper),
                inherited: &self.inherited,
                protected: &[],
            },
            host: &self.host,
            containers: &self.containers,
        }
    }

    fn up(&self, plan: &Plan, select: &Selection, env: &Env) -> Result<Report, PhaseError> {
        self.engine()
            .up(plan, select, env, deadlines(), &Cancel::new())
    }

    fn down(&self, select: &Selection) -> Report {
        self.engine().down(&workspace(), select, soon()).unwrap()
    }

    fn record(&self) -> Option<RunRecord> {
        RunStore::new(self.state.path()).read(&workspace()).unwrap()
    }

    /// Gives the owner back the write permission on the directory of run
    /// records, which [`FakeHost::breaks_record_at_launch`] took away.
    fn restore_runs_directory(&self) {
        set_mode(&self.state.path().join("runs"), 0o700);
    }

    /// Changes the run record as `change` says, under the lifecycle lock.
    fn edit(&self, change: impl FnOnce(&mut RunRecord)) {
        let store = RunStore::new(self.state.path());
        let lock = store.lock(&workspace(), Duration::from_secs(1)).unwrap();
        let mut record = store.read(&workspace()).unwrap().unwrap();
        change(&mut record);
        store.write(&record, &lock).unwrap();
    }

    /// Marks the process entry of `app` stopped, as a keeper does when its
    /// app ends and it waits to restart it.
    fn record_app_stopped(&self, app: &str) {
        self.edit(|record| {
            record
                .set_state(
                    &ObjectKey::Process(name(app), name("server")),
                    EntryState::Stopped,
                )
                .unwrap();
        });
    }
}

fn soon() -> Instant {
    now() + Duration::from_secs(5)
}

fn deadlines() -> UpDeadlines {
    UpDeadlines::new(soon(), soon())
}

fn name<T: std::str::FromStr>(text: &str) -> T
where
    T::Err: std::fmt::Debug,
{
    text.parse().unwrap()
}

/// Returns a host app named `app` from a checkout, with one process
/// `server` that listens on `port` when it is given, and depends on
/// `depends`.
fn host_app(app: &str, port: Option<u16>, depends: &[&str]) -> AppPlan {
    let mut plan = AppPlan::new(
        name(app),
        RunnerKind::Host,
        Source::checkout(PathBuf::from("/workspaces/reading-list").join(app), None),
    );
    let mut server = ProcessPlan::new(name("server"), vec!["./serve".into()]);
    server.env.insert("API_TOKEN".to_owned());
    if let Some(port) = port {
        plan.ports
            .insert(name("http"), NonZeroU16::new(port).unwrap());
        server.port = Some(name("http"));
    }
    plan.processes.push(server);
    plan.depends = depends.iter().map(|depend| name(depend)).collect();
    plan
}

fn plan(apps: Vec<AppPlan>) -> Plan {
    Plan::new(name("reading-list"), workspace(), apps).unwrap()
}

fn outcome<'r>(report: &'r Report, app: &str) -> &'r AppOutcome {
    &report
        .apps
        .iter()
        .find(|entry| entry.app.as_str() == app)
        .unwrap()
        .outcome
}

fn notes<'r>(report: &'r Report, app: &str) -> &'r [Note] {
    &report
        .apps
        .iter()
        .find(|entry| entry.app.as_str() == app)
        .unwrap()
        .notes
}

fn only(target: &str) -> Selection {
    Selection::only(vec![name::<Target>(target)])
}

fn state_of(record: &RunRecord, app: &str) -> Option<EntryState> {
    record.state(&ObjectKey::Process(name(app), name("server")))
}

#[test]
fn up_starts_the_apps_in_dependency_order_and_records_them_running() {
    let world = World::new();
    let plan = plan(vec![
        host_app("api", None, &["db"]),
        host_app("db", None, &[]),
    ]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(world.host.launches(), vec!["db/server", "api/server"]);
    assert_eq!(outcome(&report, "db"), &AppOutcome::Started);
    assert_eq!(outcome(&report, "api"), &AppOutcome::Started);
    let record = world.record().unwrap();
    assert_eq!(state_of(&record, "api"), Some(EntryState::Running));
    assert_eq!(state_of(&record, "db"), Some(EntryState::Running));
}

#[test]
fn up_records_every_process_it_will_start_before_it_starts_the_first() {
    let world = World::new();
    let plan = plan(vec![
        host_app("api", None, &["db"]),
        host_app("db", None, &[]),
    ]);

    world.up(&plan, &Selection::All, &Env::default()).unwrap();

    let first = world.host.state.borrow().records_at_launch[0]
        .clone()
        .unwrap();
    assert_eq!(state_of(&first, "db"), Some(EntryState::Planned));
    assert_eq!(state_of(&first, "api"), Some(EntryState::Planned));
}

#[test]
fn up_of_one_app_also_starts_the_apps_it_depends_on_and_no_other() {
    let world = World::new();
    let plan = plan(vec![
        host_app("api", None, &["db"]),
        host_app("db", None, &[]),
        host_app("web", None, &[]),
    ]);

    world.up(&plan, &only("api"), &Env::default()).unwrap();

    assert_eq!(world.host.launches(), vec!["db/server", "api/server"]);
}

#[test]
fn up_refuses_a_selection_that_names_an_app_the_plan_lacks() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);

    let error = world
        .up(&plan, &only("billing"), &Env::default())
        .unwrap_err();

    assert_eq!(error.code(), "phase::select::unknown");
    assert!(world.host.launches().is_empty());
}

#[test]
fn up_fails_as_busy_while_another_operation_holds_the_lock() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    let _held = RunStore::new(world.state.path())
        .lock(&workspace(), Duration::from_millis(50))
        .unwrap();

    let error = world
        .up(&plan, &Selection::All, &Env::default())
        .unwrap_err();

    assert_eq!(error.code(), "phase::lock::busy");
}

#[test]
fn a_process_that_ends_within_the_start_grace_fails_and_its_dependents_do_not_start() {
    let world = World::new();
    world.host.ends("db/server");
    let plan = plan(vec![
        host_app("api", None, &["db"]),
        host_app("db", None, &[]),
    ]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert!(matches!(
        outcome(&report, "db"),
        AppOutcome::Failed {
            problem: StartProblem::Exited { .. },
            ..
        }
    ));
    assert_eq!(outcome(&report, "api"), &AppOutcome::NotStarted);
    assert_eq!(world.host.launches(), vec!["db/server"]);
    assert_eq!(world.host.running(), 0);
}

#[test]
fn a_failed_app_stops_the_apps_after_it_from_starting_and_leaves_the_ones_before() {
    let world = World::new();
    world.host.ends("b/server");
    let plan = plan(vec![
        host_app("a", None, &[]),
        host_app("b", None, &[]),
        host_app("c", None, &[]),
    ]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "a"), &AppOutcome::Started);
    assert_eq!(outcome(&report, "c"), &AppOutcome::NotStarted);
    assert_eq!(world.host.running(), 2);
}

#[test]
fn a_log_probe_makes_a_process_ready_once_its_line_arrives() {
    let world = World::new();
    world
        .host
        .state
        .borrow_mut()
        .logs
        .insert("api.server.log".to_owned(), vec!["listening".to_owned()]);
    let mut api = host_app("api", None, &[]);
    api.processes[0].ready = Some(Probe::new(ProbeCheck::log("listening".to_owned())));

    let report = world
        .up(&plan(vec![api]), &Selection::All, &Env::default())
        .unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::Started);
}

#[test]
fn up_records_when_a_process_passed_its_readiness_probe() {
    let world = World::new();
    world
        .host
        .state
        .borrow_mut()
        .logs
        .insert("api.server.log".to_owned(), vec!["listening".to_owned()]);
    let mut api = host_app("api", None, &[]);
    api.processes[0].ready = Some(Probe::new(ProbeCheck::log("listening".to_owned())));
    let before = crate::record::run::Ready::now(0).at_ms;

    world
        .up(&plan(vec![api]), &Selection::All, &Env::default())
        .unwrap();

    let ready = world.record().unwrap().processes[0].ready.clone().unwrap();
    assert_eq!(ready.restarts, 0);
    assert!(ready.at_ms >= before, "{ready:?}");
}

#[test]
fn up_records_a_process_without_a_probe_ready_once_it_outlasts_its_start_grace() {
    let world = World::new();

    world
        .up(
            &plan(vec![host_app("api", None, &[])]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();

    assert!(world.record().unwrap().processes[0].ready.is_some());
    let specs = world.host.state.borrow().specs.join("\n");
    assert!(specs.contains(r#""ready_after_ms":30"#), "{specs}");
}

#[test]
fn the_keeper_of_a_process_with_a_probe_does_not_count_it_ready_on_start() {
    let world = World::new();
    world
        .host
        .state
        .borrow_mut()
        .logs
        .insert("api.server.log".to_owned(), vec!["listening".to_owned()]);
    let mut api = host_app("api", None, &[]);
    api.processes[0].ready = Some(Probe::new(ProbeCheck::log("listening".to_owned())));

    world
        .up(&plan(vec![api]), &Selection::All, &Env::default())
        .unwrap();

    let specs = world.host.state.borrow().specs.join("\n");
    assert!(!specs.contains("ready_after_ms"), "{specs}");
}

#[test]
fn a_process_whose_port_another_group_answers_after_readiness_records_no_readiness() {
    let world = World::new();
    world.host.listen(41_000, Listener::HeldAfterLaunch(4242));
    let plan = plan(vec![host_app("api", Some(41_000), &[])]);

    world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert!(
        world
            .record()
            .is_none_or(|record| record.processes.iter().all(|entry| entry.ready.is_none()))
    );
}

#[test]
fn up_records_the_readiness_that_the_container_runner_observed() {
    let world = World::new();
    world
        .containers
        .ready
        .replace(Some(crate::record::run::Ready::new(1_793_000_000_123, 2)));

    world
        .up(
            &plan(vec![container_app("db")]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();

    assert_eq!(
        world.record().unwrap().containers[0].ready,
        Some(crate::record::run::Ready::new(1_793_000_000_123, 2))
    );
}

#[test]
fn a_process_that_ends_before_its_probe_passes_reports_nothing_that_it_wrote() {
    let world = World::new();
    world.host.ends("api/server");
    world.host.state.borrow_mut().logs.insert(
        "api.server.log".to_owned(),
        vec!["password=hunter2 rejected".to_owned()],
    );
    let mut api = host_app("api", None, &[]);
    api.processes[0].ready = Some(Probe::new(ProbeCheck::log("never".to_owned())));

    let report = world
        .up(&plan(vec![api]), &Selection::All, &Env::default())
        .unwrap();

    assert_eq!(
        outcome(&report, "api"),
        &AppOutcome::failed(name("server"), StartProblem::exited(None, None))
    );
    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("hunter2"), "{json}");
}

#[test]
fn a_process_that_misses_its_ready_timeout_fails_as_not_ready_and_is_stopped() {
    let world = World::new();
    let mut api = host_app("api", None, &[]);
    api.processes[0].ready = Some(Probe::new(ProbeCheck::log("never".to_owned())));
    api.processes[0].ready_timeout = Duration::from_millis(50);

    let report = world
        .up(&plan(vec![api]), &Selection::All, &Env::default())
        .unwrap();

    assert!(matches!(
        outcome(&report, "api"),
        AppOutcome::Failed {
            problem: StartProblem::NotReady { .. },
            ..
        }
    ));
    assert_eq!(world.host.stops(), vec!["api/server"]);
    assert_eq!(world.record(), None);
}

#[test]
fn a_readiness_deadline_that_cuts_off_a_start_makes_its_outcome_uncertain() {
    let world = World::new();
    let mut api = host_app("api", None, &[]);
    api.processes[0].ready = Some(Probe::new(ProbeCheck::log("never".to_owned())));
    let short = now() + Duration::from_millis(50);

    let report = world
        .engine()
        .up(
            &plan(vec![api]),
            &Selection::All,
            &Env::default(),
            UpDeadlines::new(short, short),
            &Cancel::new(),
        )
        .unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::StartUncertain);
    assert!(world.host.stops().is_empty());
}

#[test]
fn up_does_not_start_a_process_whose_port_is_already_held_and_names_the_holder() {
    let world = World::new();
    world.host.listen(41_000, Listener::Held(4242));
    let plan = plan(vec![host_app("api", Some(41_000), &[])]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(
        outcome(&report, "api"),
        &AppOutcome::failed(
            name("server"),
            StartProblem::port_taken(name("http"), vec![4242])
        )
    );
    assert!(world.host.launches().is_empty());
}

#[test]
fn a_port_that_another_group_answers_after_readiness_fails_the_start_and_stops_the_app() {
    let world = World::new();
    world.host.listen(41_000, Listener::HeldAfterLaunch(4242));
    let plan = plan(vec![host_app("api", Some(41_000), &[])]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(
        outcome(&report, "api"),
        &AppOutcome::failed(
            name("server"),
            StartProblem::foreign_listener(name("http"), vec![4242])
        )
    );
    assert_eq!(world.host.stops(), vec!["api/server"]);
}

#[test]
fn a_port_that_the_app_itself_holds_after_readiness_is_fine() {
    let world = World::new();
    world.host.listen(41_000, Listener::Own);
    let plan = plan(vec![host_app("api", Some(41_000), &[])]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::Started);
}

#[test]
fn a_listener_that_cannot_be_found_is_noted_and_does_not_block_the_start() {
    let world = World::new();
    world.host.listen(41_000, Listener::Unknown);
    let plan = plan(vec![host_app("api", Some(41_000), &[])]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::Started);
    assert!(matches!(
        notes(&report, "api").first(),
        Some(Note::ListenerUnknown { .. })
    ));
}

#[test]
fn an_environment_that_cannot_be_resolved_fails_up_and_stops_what_it_started() {
    let world = World::new();
    let env = Env {
        failing: BTreeSet::from(["api".to_owned()]),
        ..Env::default()
    };
    let plan = plan(vec![
        host_app("api", None, &["db"]),
        host_app("db", None, &[]),
    ]);

    let error = world.up(&plan, &Selection::All, &env).unwrap_err();

    assert_eq!(error.code(), "phase::environment");
    assert_eq!(world.host.launches(), vec!["db/server"]);
    assert_eq!(world.host.stops(), vec!["db/server"]);
    assert_eq!(world.host.running(), 0);
}

#[test]
fn a_cancelled_up_stops_what_it_started_and_reports_the_rest_as_not_started() {
    let world = World::new();
    let cancel = Cancel::new();
    let mut api = host_app("api", None, &["db"]);
    api.processes[0].ready = Some(Probe::new(ProbeCheck::log("never".to_owned())));
    let plan = plan(vec![
        api,
        host_app("db", None, &[]),
        host_app("web", None, &["api"]),
    ]);
    cancel.cancel();

    let report = world
        .engine()
        .up(
            &plan,
            &Selection::All,
            &Env::default(),
            deadlines(),
            &cancel,
        )
        .unwrap();

    assert_eq!(world.host.running(), 0);
    assert_eq!(outcome(&report, "web"), &AppOutcome::NotStarted);
}

#[test]
fn a_cancel_during_a_readiness_wait_stops_every_process_this_up_started() {
    let world = World::new();
    let mut api = host_app("api", None, &["db"]);
    api.processes[0].ready = Some(Probe::new(ProbeCheck::log("never".to_owned())));
    let plan = plan(vec![api, host_app("db", None, &[])]);
    let cancel = Cancel::new();
    world.host.cancels_at_launch("api/server", &cancel);

    let report = world
        .engine()
        .up(
            &plan,
            &Selection::All,
            &Env::default(),
            deadlines(),
            &cancel,
        )
        .unwrap();

    assert_eq!(world.host.stops(), vec!["api/server", "db/server"]);
    assert_eq!(outcome(&report, "db"), &AppOutcome::Stopped);
    assert_eq!(outcome(&report, "api"), &AppOutcome::Stopped);
    assert_eq!(world.host.running(), 0);
}

#[test]
fn up_leaves_a_process_that_already_runs_alone() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::AlreadyRunning);
    assert_eq!(world.host.launches(), vec!["api/server"]);
}

#[test]
fn up_starts_nothing_in_place_of_a_group_whose_leader_has_gone_and_reports_it_left() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world.host.end_leader("api/server", false);
    let (_, app) = world.host.state.borrow().pids["api/server"];

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::Left);
    assert_eq!(
        notes(&report, "api"),
        &[Note::group_unverified(name("server"), app)]
    );
    assert_eq!(world.host.launches(), vec!["api/server"]);
    assert!(world.host.stops().is_empty());
}

#[test]
fn up_counts_a_group_whose_leader_has_gone_as_running_while_its_keeper_runs() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world.host.end_leader("api/server", true);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::AlreadyRunning);
    assert_eq!(world.host.launches(), vec!["api/server"]);
}

#[test]
fn up_starts_a_recorded_process_again_once_it_has_gone() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world.host.state.borrow_mut().alive.clear();

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::Started);
    assert_eq!(world.host.launches(), vec!["api/server", "api/server"]);
}

#[test]
fn an_app_from_a_release_is_reported_as_not_runnable_and_its_dependents_do_not_start() {
    let world = World::new();
    let mut db = host_app("db", None, &[]);
    db.source = Source::release(
        name("v1.0.0"),
        crate::source::ReleaseSpec::new(
            name("crumbkit/example-db"),
            "db-{target}.tar.gz".to_owned(),
            PathBuf::from("db"),
            crate::source::Verification::Sha256,
        ),
    );
    db.processes[0].run = vec!["{program}".to_owned()];
    let plan = plan(vec![
        host_app("api", None, &["db"]),
        db,
        host_app("web", None, &[]),
    ]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert!(matches!(
        outcome(&report, "db"),
        AppOutcome::Errored { code, .. } if code == "phase::app::not_runnable"
    ));
    assert_eq!(outcome(&report, "api"), &AppOutcome::NotStarted);
    assert_eq!(outcome(&report, "web"), &AppOutcome::Started);
}

#[test]
fn a_process_that_cannot_start_reports_the_failure_as_errored() {
    let world = World::new();
    world
        .host
        .state
        .borrow_mut()
        .unstartable
        .insert("api/server".to_owned());
    let plan = plan(vec![host_app("api", None, &[])]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert!(matches!(
        outcome(&report, "api"),
        AppOutcome::Errored { code, .. } if code == "phase::process::unstartable"
    ));
}

#[test]
fn up_without_a_keeper_program_reports_the_host_app_as_errored() {
    let world = World::new();
    let mut engine = world.engine();
    engine.settings.keeper = None;
    let plan = plan(vec![host_app("api", None, &[])]);

    let report = engine
        .up(
            &plan,
            &Selection::All,
            &Env::default(),
            deadlines(),
            &Cancel::new(),
        )
        .unwrap();

    assert!(matches!(
        outcome(&report, "api"),
        AppOutcome::Errored { code, .. } if code == "phase::keeper::unstartable"
    ));
}

#[test]
fn no_environment_value_reaches_the_keepers_specification_the_record_or_the_report() {
    let world = World::new();
    world.host.ends("api/server");
    let plan = plan(vec![host_app("api", None, &[])]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    let specs = world.host.state.borrow().specs.join("\n");
    assert!(specs.contains("./serve"), "{specs}");
    assert!(!specs.contains(SECRET));
    let record = std::fs::read_to_string(RunStore::new(world.state.path()).path(&workspace()))
        .unwrap_or_default();
    assert!(!record.contains(SECRET));
    assert!(!serde_json::to_string(&report).unwrap().contains(SECRET));
    assert!(!format!("{report:?}").contains(SECRET));
}

#[test]
fn a_container_app_starts_through_the_container_runner_and_is_recorded() {
    let world = World::new();
    let mut db = AppPlan::new(
        name("db"),
        RunnerKind::Container,
        Source::image(name("postgres:17")),
    );
    db.processes
        .push(ProcessPlan::new(name("server"), Vec::new()));
    let plan = plan(vec![host_app("api", None, &["db"]), db]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(*world.containers.started.borrow(), vec!["db"]);
    assert_eq!(outcome(&report, "db"), &AppOutcome::Started);
    assert_eq!(outcome(&report, "api"), &AppOutcome::Started);
    let record = world.record().unwrap();
    assert_eq!(
        record.state(&ObjectKey::Container(name("db"), name("server"))),
        Some(EntryState::Running)
    );
}

#[test]
fn a_container_start_writes_its_entry_back_to_planned_without_an_id_before_the_runner_starts_it() {
    let world = World::new();
    let plan = plan(vec![container_app("db")]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world
        .engine()
        .restart(
            &plan,
            &Selection::All,
            &Env::default(),
            soon(),
            &Cancel::new(),
        )
        .unwrap();

    assert_eq!(
        *world.containers.entries_at_start.borrow(),
        vec![(EntryState::Planned, false); 3]
    );
    let record = world.record().unwrap();
    assert_eq!(
        record.state(&ObjectKey::Container(name("db"), name("server"))),
        Some(EntryState::Running)
    );
}

#[test]
fn up_leaves_a_container_that_still_runs_alone_and_starts_only_the_others() {
    let world = World::new();
    let mut db = container_app("db");
    db.processes
        .push(ProcessPlan::new(name("worker"), Vec::new()));
    let plan = plan(vec![db]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    let key = ObjectKey::Container(name("db"), name("server"));
    world
        .containers
        .running
        .borrow_mut()
        .extend(["db/server".to_owned(), "db/worker".to_owned()]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "db"), &AppOutcome::AlreadyRunning);
    assert_eq!(world.containers.starts.borrow().len(), 1);
    let record = world.record().unwrap();
    assert_eq!(record.state(&key), Some(EntryState::Running));
    assert!(
        record
            .containers
            .iter()
            .all(|entry| entry.id.is_some() && entry.state == EntryState::Running)
    );

    world.containers.running.borrow_mut().remove("db/worker");
    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "db"), &AppOutcome::Restarted);
    assert_eq!(
        world.containers.starts.borrow().last().unwrap(),
        &vec!["db/worker".to_owned()]
    );
    assert_eq!(
        world.record().unwrap().state(&key),
        Some(EntryState::Running)
    );
}

#[test]
fn up_rewrites_the_secret_files_of_a_container_that_still_runs_and_leaves_it_running() {
    let world = World::new();
    let mut db = container_app("db");
    db.processes
        .push(ProcessPlan::new(name("worker"), Vec::new()));
    let plan = plan(vec![db]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world
        .containers
        .running
        .borrow_mut()
        .extend(["db/server".to_owned(), "db/worker".to_owned()]);
    world.containers.rewrite_ending.borrow_mut().insert(
        "db/server".to_owned(),
        Rewritten::Written {
            files: 2,
            removed: 0,
        },
    );
    let env = Env::default();

    let report = world.up(&plan, &Selection::All, &env).unwrap();

    assert_eq!(outcome(&report, "db"), &AppOutcome::AlreadyRunning);
    assert_eq!(
        notes(&report, "db"),
        [Note::secrets_rewritten(&name("db"), name("server"), 2, 0)]
    );
    match &notes(&report, "db")[0] {
        Note::SecretsRewritten { message, .. } => {
            assert_eq!(message, "rewrote 2 secret files of db/server");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(world.containers.starts.borrow().len(), 1);
    assert_eq!(
        *world.containers.rewrites.borrow(),
        vec!["db/server".to_owned(), "db/worker".to_owned()]
    );
    assert_eq!(*env.resolved.borrow(), vec!["db/server".to_owned()]);
    assert!(world.containers.stopped.borrow().is_empty());
    assert_eq!(
        world
            .record()
            .unwrap()
            .state(&ObjectKey::Container(name("db"), name("server"))),
        Some(EntryState::Running)
    );
}

#[test]
fn a_secret_rewrite_that_cannot_reach_docker_fails_the_app_without_a_note_for_it() {
    let world = World::new();
    let plan = plan(vec![container_app("db"), host_app("api", None, &["db"])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world
        .containers
        .running
        .borrow_mut()
        .insert("db/server".to_owned());
    world
        .containers
        .rewrite_unreachable
        .borrow_mut()
        .insert("db/server".to_owned());

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    match outcome(&report, "db") {
        AppOutcome::Errored { code, cause, .. } => {
            assert_eq!(code, "phase::docker::unavailable");
            assert_eq!(*cause, Some(crate::record::DockerCause::DaemonUnavailable));
        }
        other => panic!("{other:?}"),
    }
    assert!(notes(&report, "db").is_empty());
    assert_ne!(outcome(&report, "api"), &AppOutcome::AlreadyRunning);
    assert_eq!(world.containers.starts.borrow().len(), 1);
    assert!(world.containers.stopped.borrow().is_empty());
}

#[test]
fn up_replaces_a_running_container_that_cannot_receive_its_secret_files() {
    let world = World::new();
    let mut db = container_app("db");
    db.processes
        .push(ProcessPlan::new(name("worker"), Vec::new()));
    let plan = plan(vec![db]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world
        .containers
        .running
        .borrow_mut()
        .extend(["db/server".to_owned(), "db/worker".to_owned()]);
    world
        .containers
        .rewrite_ending
        .borrow_mut()
        .insert("db/server".to_owned(), Rewritten::Replace);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "db"), &AppOutcome::Restarted);
    assert_eq!(
        world.containers.starts.borrow().last().unwrap(),
        &vec!["db/server".to_owned()]
    );
}

#[test]
fn a_secret_rewrite_that_fails_keeps_the_container_running_with_a_note_and_rewrites_the_others() {
    let world = World::new();
    let mut db = container_app("db");
    db.processes
        .push(ProcessPlan::new(name("worker"), Vec::new()));
    let plan = plan(vec![db, host_app("api", None, &["db"])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world
        .containers
        .running
        .borrow_mut()
        .extend(["db/server".to_owned(), "db/worker".to_owned()]);
    for process in ["db/server", "db/worker"] {
        world.containers.rewrite_ending.borrow_mut().insert(
            process.to_owned(),
            Rewritten::Written {
                files: 1,
                removed: 1,
            },
        );
    }
    let locked = Env {
        failing_processes: BTreeSet::from(["db/server".to_owned()]),
        ..Env::default()
    };

    let report = world.up(&plan, &Selection::All, &locked).unwrap();

    assert_eq!(outcome(&report, "db"), &AppOutcome::AlreadyRunning);
    assert_eq!(outcome(&report, "api"), &AppOutcome::AlreadyRunning);
    assert_eq!(
        *world.containers.rewrites.borrow(),
        vec!["db/server".to_owned(), "db/worker".to_owned()]
    );
    let messages: Vec<&str> = notes(&report, "db")
        .iter()
        .map(|note| match note {
            Note::SecretsKept { message, .. } | Note::SecretsRewritten { message, .. } => {
                message.as_str()
            }
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        messages,
        [
            "db/server still runs with its earlier secret files: vault could not supply the variable API_TOKEN",
            "rewrote 1 secret file of db/worker and removed 1 file of lapsed optional secrets",
        ]
    );
    assert!(messages.iter().all(|message| !message.contains(SECRET)));
    assert_eq!(world.containers.starts.borrow().len(), 1);
    assert!(world.containers.stopped.borrow().is_empty());
    assert_eq!(
        world
            .record()
            .unwrap()
            .state(&ObjectKey::Container(name("db"), name("server"))),
        Some(EntryState::Running)
    );
}

#[test]
fn down_stops_the_apps_in_reverse_dependency_order_and_removes_the_record() {
    let world = World::new();
    let plan = plan(vec![
        host_app("api", None, &["db"]),
        host_app("db", None, &[]),
    ]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();

    let report = world.down(&Selection::All);

    assert_eq!(world.host.stops(), vec!["api/server", "db/server"]);
    assert_eq!(outcome(&report, "api"), &AppOutcome::Stopped);
    assert_eq!(outcome(&report, "db"), &AppOutcome::Stopped);
    assert_eq!(world.record(), None);
    assert_eq!(world.host.running(), 0);
}

#[test]
fn down_of_one_app_stops_only_that_app_and_keeps_the_record() {
    let world = World::new();
    let plan = plan(vec![
        host_app("api", None, &["db"]),
        host_app("db", None, &[]),
    ]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();

    let report = world.down(&only("api"));

    assert_eq!(world.host.stops(), vec!["api/server"]);
    assert_eq!(report.apps.len(), 1);
    let record = world.record().unwrap();
    assert_eq!(state_of(&record, "db"), Some(EntryState::Running));
}

#[test]
fn down_of_a_workspace_with_no_record_reports_the_selected_apps_as_not_running() {
    let world = World::new();

    let report = world.down(&only("api"));

    assert_eq!(outcome(&report, "api"), &AppOutcome::NotRunning);
}

#[test]
fn down_keeps_a_process_it_cannot_prove_is_its_own_and_notes_it() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world
        .host
        .state
        .borrow_mut()
        .not_ours
        .insert("api/server".to_owned());

    let report = world.down(&Selection::All);

    assert_eq!(outcome(&report, "api"), &AppOutcome::Left);
    assert_eq!(notes(&report, "api"), &[Note::not_ours(name("server"))]);
    assert!(world.record().is_some());
}

#[test]
fn down_keeps_a_group_it_cannot_prove_is_its_own_and_notes_the_group() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world
        .host
        .state
        .borrow_mut()
        .unverified
        .insert("api/server".to_owned());
    let (_, app) = world.host.state.borrow().pids["api/server"];

    let report = world.down(&Selection::All);

    assert_eq!(outcome(&report, "api"), &AppOutcome::Left);
    assert_eq!(
        notes(&report, "api"),
        &[Note::group_unverified(name("server"), app)]
    );
    assert!(world.record().is_some());
}

#[test]
fn down_keeps_a_process_that_survives_kill_and_notes_its_group() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world
        .host
        .state
        .borrow_mut()
        .unstoppable
        .insert("api/server".to_owned());

    let report = world.down(&Selection::All);

    assert_eq!(outcome(&report, "api"), &AppOutcome::Left);
    assert!(matches!(notes(&report, "api"), [Note::Unstoppable { .. }]));
    assert_eq!(
        state_of(&world.record().unwrap(), "api"),
        Some(EntryState::Stopping)
    );
}

#[test]
fn down_notes_a_port_still_held_after_the_app_is_gone_and_signals_nothing_more() {
    let world = World::new();
    let plan = plan(vec![host_app("api", Some(41_000), &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world.host.listen(41_000, Listener::Held(4242));

    let report = world.down(&Selection::All);

    assert_eq!(outcome(&report, "api"), &AppOutcome::Stopped);
    assert_eq!(
        notes(&report, "api"),
        &[Note::detached_listener(
            name("server"),
            name("http"),
            vec![4242]
        )]
    );
    assert_eq!(world.host.stops(), vec!["api/server"]);
}

#[test]
fn down_stops_a_container_app_through_the_container_runner() {
    let world = World::new();
    let mut db = AppPlan::new(
        name("db"),
        RunnerKind::Container,
        Source::image(name("postgres:17")),
    );
    db.processes
        .push(ProcessPlan::new(name("server"), Vec::new()));
    world
        .up(&plan(vec![db]), &Selection::All, &Env::default())
        .unwrap();

    let report = world.down(&Selection::All);

    assert_eq!(*world.containers.stopped.borrow(), vec!["db"]);
    assert_eq!(outcome(&report, "db"), &AppOutcome::Stopped);
}

#[test]
fn restart_resolves_the_environment_again_then_stops_and_starts_the_process() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    let env = Env::default();
    world.up(&plan, &Selection::All, &env).unwrap();

    let report = world
        .engine()
        .restart(&plan, &Selection::All, &env, soon(), &Cancel::new())
        .unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::Restarted);
    assert_eq!(*env.resolved.borrow(), vec!["api/server", "api/server"]);
    assert_eq!(world.host.stops(), vec!["api/server"]);
    assert_eq!(world.host.launches(), vec!["api/server", "api/server"]);
    assert_eq!(world.host.running(), 2);
}

#[test]
fn restart_leaves_a_group_it_cannot_prove_is_its_own_and_starts_nothing() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world.host.end_leader("api/server", false);
    world
        .host
        .state
        .borrow_mut()
        .unverified
        .insert("api/server".to_owned());
    let (_, app) = world.host.state.borrow().pids["api/server"];

    let report = world
        .engine()
        .restart(
            &plan,
            &Selection::All,
            &Env::default(),
            soon(),
            &Cancel::new(),
        )
        .unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::Left);
    assert_eq!(
        notes(&report, "api"),
        &[Note::group_unverified(name("server"), app)]
    );
    assert_eq!(world.host.launches(), vec!["api/server"]);
}

#[test]
fn a_restart_whose_environment_cannot_be_resolved_stops_nothing() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    let failing = Env {
        failing: BTreeSet::from(["api".to_owned()]),
        ..Env::default()
    };

    let error = world
        .engine()
        .restart(&plan, &Selection::All, &failing, soon(), &Cancel::new())
        .unwrap_err();

    assert_eq!(error.code(), "phase::environment");
    assert!(world.host.stops().is_empty());
}

#[test]
fn restart_of_an_app_that_is_not_running_starts_nothing() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);

    let report = world
        .engine()
        .restart(
            &plan,
            &Selection::All,
            &Env::default(),
            soon(),
            &Cancel::new(),
        )
        .unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::NotRunning);
    assert!(world.host.launches().is_empty());
}

#[test]
fn down_keeps_a_container_that_docker_left_in_place() {
    let mut world = World::new();
    world.containers.stubborn = true;
    let mut db = AppPlan::new(
        name("db"),
        RunnerKind::Container,
        Source::image(name("postgres:17")),
    );
    db.processes
        .push(ProcessPlan::new(name("server"), Vec::new()));
    world
        .up(&plan(vec![db]), &Selection::All, &Env::default())
        .unwrap();

    let report = world.down(&Selection::All);

    assert_eq!(outcome(&report, "db"), &AppOutcome::Left);
    assert!(world.record().is_some());
}

fn container_app(app: &str) -> AppPlan {
    let mut plan = AppPlan::new(
        name(app),
        RunnerKind::Container,
        Source::image(name("postgres:17")),
    );
    plan.processes
        .push(ProcessPlan::new(name("server"), Vec::new()));
    plan
}

#[test]
fn down_stops_a_keeper_in_restart_backoff_whose_process_entry_reads_stopped() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world.host.end_app_and_keep_keeper("api/server");
    world.record_app_stopped("api");

    let report = world.down(&Selection::All);

    assert_eq!(world.host.stops(), vec!["api/server"]);
    assert_eq!(outcome(&report, "api"), &AppOutcome::Stopped);
    assert_eq!(world.host.running(), 0);
    assert_eq!(world.record(), None);
}

#[test]
fn up_reports_a_keeper_in_restart_backoff_as_already_running_and_starts_no_second_keeper() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    let keeper = world.record().unwrap().keepers[0].host.clone();
    world.host.end_app_and_keep_keeper("api/server");
    world.record_app_stopped("api");

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::AlreadyRunning);
    assert_eq!(world.host.launches(), vec!["api/server"]);
    assert_eq!(world.record().unwrap().keepers[0].host, keeper);
}

#[test]
fn restart_restarts_a_process_whose_keeper_is_in_restart_backoff() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world.host.end_app_and_keep_keeper("api/server");
    world.record_app_stopped("api");

    let report = world
        .engine()
        .restart(
            &plan,
            &Selection::All,
            &Env::default(),
            soon(),
            &Cancel::new(),
        )
        .unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::Restarted);
    assert_eq!(world.host.stops(), vec!["api/server"]);
    assert_eq!(world.host.launches(), vec!["api/server", "api/server"]);
    assert_eq!(world.host.running(), 2);
}

#[test]
fn up_writes_a_stopped_entry_back_to_planned_with_no_process_before_it_launches_again() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[]), host_app("db", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world.down(&only("api"));

    world.up(&plan, &only("api"), &Env::default()).unwrap();

    let at_launch = world
        .host
        .state
        .borrow()
        .records_at_launch
        .last()
        .cloned()
        .unwrap()
        .unwrap();
    let (keeper, process) = at_launch.host_pair(&name("api"), &name("server"));
    for entry in [keeper.unwrap(), process.unwrap()] {
        assert_eq!(entry.state, EntryState::Planned);
        assert_eq!(entry.host, None);
    }
}

#[test]
fn a_record_that_cannot_be_written_after_a_launch_fails_up_and_stops_every_process_it_started() {
    let world = World::new();
    world.host.breaks_record_at_launch("api/server");
    let plan = plan(vec![
        host_app("api", None, &["db"]),
        host_app("db", None, &[]),
    ]);

    let error = world.up(&plan, &Selection::All, &Env::default());

    set_mode(&world.state.path().join("runs"), 0o700);
    assert_eq!(error.unwrap_err().code(), "phase::state::unwritable");
    assert_eq!(world.host.stops(), vec!["api/server", "db/server"]);
    assert_eq!(world.host.running(), 0);
}

#[test]
fn a_cancelled_restart_stops_only_the_app_it_was_starting_and_leaves_the_rest_untouched() {
    let world = World::new();
    let plan = plan(vec![host_app("a", None, &[]), host_app("b", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    let cancel = Cancel::new();
    world.host.cancels_at_launch("a/server", &cancel);

    let report = world
        .engine()
        .restart(&plan, &Selection::All, &Env::default(), soon(), &cancel)
        .unwrap();

    assert_eq!(outcome(&report, "a"), &AppOutcome::Stopped);
    assert_eq!(outcome(&report, "b"), &AppOutcome::Untouched);
    assert_eq!(world.host.stops(), vec!["a/server", "a/server"]);
    assert_eq!(world.host.running(), 2);
}

#[test]
fn a_restart_does_not_stop_the_apps_it_already_restarted_when_a_later_one_is_cancelled() {
    let world = World::new();
    let plan = plan(vec![host_app("a", None, &[]), host_app("b", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    let cancel = Cancel::new();
    world.host.cancels_at_launch("b/server", &cancel);

    let report = world
        .engine()
        .restart(&plan, &Selection::All, &Env::default(), soon(), &cancel)
        .unwrap();

    assert_eq!(outcome(&report, "a"), &AppOutcome::Restarted);
    assert_eq!(outcome(&report, "b"), &AppOutcome::Stopped);
    assert_eq!(world.host.running(), 2);
}

#[test]
fn a_port_that_a_bind_finds_in_use_fails_the_start_as_taken_by_an_unknown_holder() {
    let world = World::new();
    world.host.listen(41_000, Listener::UnknownHeld);
    let plan = plan(vec![host_app("api", Some(41_000), &[])]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(
        outcome(&report, "api"),
        &AppOutcome::failed(
            name("server"),
            StartProblem::port_taken(name("http"), vec![])
        )
    );
    assert!(world.host.launches().is_empty());
}

#[test]
fn a_port_that_a_process_phase_may_not_read_holds_fails_the_start_as_taken_by_an_unknown_holder() {
    let world = World::new();
    world.host.listen(41_000, Listener::Unreadable);
    let plan = plan(vec![host_app("api", Some(41_000), &[])]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(
        outcome(&report, "api"),
        &AppOutcome::failed(
            name("server"),
            StartProblem::port_taken(name("http"), vec![])
        )
    );
    assert!(world.host.launches().is_empty());
}

#[test]
fn a_port_that_a_process_phase_may_not_read_holds_after_readiness_is_noted_and_kept_started() {
    let world = World::new();
    world.host.listen(41_000, Listener::UnreadableAfterLaunch);
    let plan = plan(vec![host_app("api", Some(41_000), &[])]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::Started);
    assert!(matches!(
        notes(&report, "api").first(),
        Some(Note::ListenerUnknown { .. })
    ));
}

#[test]
fn a_failed_lookup_after_readiness_is_noted_with_its_cause_even_when_the_port_is_in_use() {
    let world = World::new();
    world.host.listen(41_000, Listener::UnknownHeldAfterLaunch);
    let plan = plan(vec![host_app("api", Some(41_000), &[])]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::Started);
    assert!(
        matches!(
            notes(&report, "api").first(),
            Some(Note::ListenerUnknown { reason, .. })
                if reason.contains("the test cannot find listeners")
        ),
        "{:?}",
        notes(&report, "api")
    );
}

#[test]
fn a_deadline_that_cuts_off_the_start_grace_makes_the_start_uncertain() {
    let world = World::new();
    let mut engine = world.engine();
    engine.settings.start_grace = Duration::from_secs(5);
    let short = now() + Duration::from_millis(50);

    let report = engine
        .up(
            &plan(vec![host_app("api", None, &[])]),
            &Selection::All,
            &Env::default(),
            UpDeadlines::new(short, short),
            &Cancel::new(),
        )
        .unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::StartUncertain);
    assert!(world.host.stops().is_empty());
}

#[test]
fn a_restarts_stop_uses_the_callers_deadline_and_a_cut_off_stop_is_uncertain() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world
        .host
        .state
        .borrow_mut()
        .unstoppable
        .insert("api/server".to_owned());
    let deadline = now() + Duration::from_millis(1);

    let report = world
        .engine()
        .restart(
            &plan,
            &Selection::All,
            &Env::default(),
            deadline,
            &Cancel::new(),
        )
        .unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::StopUncertain);
    assert_eq!(world.host.stop_calls()[0].1, deadline);
}

#[test]
fn a_stop_after_a_failed_start_has_the_grace_and_twice_the_kill_wait() {
    let world = World::new();
    let mut engine = world.engine();
    engine.settings.kill_wait = Duration::from_millis(500);
    let mut api = host_app("api", None, &[]);
    api.processes[0].ready = Some(Probe::new(ProbeCheck::log("never".to_owned())));
    api.processes[0].ready_timeout = Duration::from_millis(50);
    api.processes[0].stop.grace = Duration::from_secs(1);
    let before = now();

    engine
        .up(
            &plan(vec![api]),
            &Selection::All,
            &Env::default(),
            deadlines(),
            &Cancel::new(),
        )
        .unwrap();

    // The engine sets the deadline after the up begins and before it
    // records the stop, which syncs a file, and the fake reads the clock
    // after that. So the deadline lies between these two bounds however
    // long the sync takes.
    let (asked, deadline) = world.host.stop_calls()[0];
    let budget = Duration::from_secs(2);
    assert!(deadline >= before + budget, "{deadline:?} {before:?}");
    assert!(deadline <= asked + budget, "{deadline:?} {asked:?}");
}

#[test]
fn up_reports_a_process_that_survived_kill_as_left_and_launches_nothing() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world
        .host
        .state
        .borrow_mut()
        .unstoppable
        .insert("api/server".to_owned());
    world.down(&Selection::All);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::Left);
    assert_eq!(world.host.launches(), vec!["api/server"]);
}

#[test]
fn a_cancelled_up_keeps_the_notes_each_app_collected_while_starting() {
    let world = World::new();
    world.host.listen(41_000, Listener::Unknown);
    world.host.listen(41_001, Listener::Unknown);
    let mut api = host_app("api", Some(41_001), &["db"]);
    api.processes[0].ready = Some(Probe::new(ProbeCheck::log("never".to_owned())));
    let plan = plan(vec![api, host_app("db", Some(41_000), &[])]);
    let cancel = Cancel::new();
    world.host.cancels_at_launch("api/server", &cancel);

    let report = world
        .engine()
        .up(
            &plan,
            &Selection::All,
            &Env::default(),
            deadlines(),
            &cancel,
        )
        .unwrap();

    for app in ["db", "api"] {
        assert_eq!(outcome(&report, app), &AppOutcome::Stopped);
        assert!(
            notes(&report, app)
                .iter()
                .any(|note| matches!(note, Note::ListenerUnknown { .. })),
            "{app}: {report:?}"
        );
    }
}

#[test]
fn a_cancelled_up_stops_the_containers_it_started() {
    let world = World::new();
    let mut api = host_app("api", None, &["db"]);
    api.processes[0].ready = Some(Probe::new(ProbeCheck::log("never".to_owned())));
    let plan = plan(vec![api, container_app("db")]);
    let cancel = Cancel::new();
    world.host.cancels_at_launch("api/server", &cancel);

    let report = world
        .engine()
        .up(
            &plan,
            &Selection::All,
            &Env::default(),
            deadlines(),
            &cancel,
        )
        .unwrap();

    assert_eq!(*world.containers.stopped.borrow(), vec!["db"]);
    assert_eq!(outcome(&report, "db"), &AppOutcome::Stopped);
}

#[test]
fn up_forgets_the_entries_of_apps_it_never_tried_to_start() {
    let world = World::new();
    world.host.ends("api/server");
    let plan = plan(vec![
        host_app("db", None, &[]),
        host_app("api", None, &["db"]),
        host_app("web", None, &["db"]),
    ]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "web"), &AppOutcome::NotStarted);
    let record = world.record().unwrap();
    assert_eq!(state_of(&record, "db"), Some(EntryState::Running));
    assert_eq!(state_of(&record, "api"), Some(EntryState::Stopped));
    assert_eq!(state_of(&record, "web"), None);
    assert!(record.apps.iter().all(|app| app.name.as_str() != "web"));
}

#[test]
fn an_up_that_leaves_nothing_running_and_no_exit_removes_the_record() {
    let world = World::new();
    let mut api = host_app("api", None, &[]);
    api.processes[0].ready = Some(Probe::new(ProbeCheck::log("never".to_owned())));
    api.processes[0].ready_timeout = Duration::from_millis(50);

    world
        .up(&plan(vec![api]), &Selection::All, &Env::default())
        .unwrap();

    assert_eq!(world.record(), None);
}

#[test]
fn an_up_whose_process_ended_by_itself_keeps_the_record_of_its_end() {
    let world = World::new();
    world.host.ends("api/server");

    world
        .up(
            &plan(vec![host_app("api", None, &[])]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();

    let record = world.record().unwrap();
    assert_eq!(record.processes[0].state, EntryState::Stopped);
    assert_eq!(
        record.processes[0].ended,
        Some(crate::record::run::Ended::by_itself(None))
    );
}

#[test]
fn up_reports_a_running_keeper_as_already_running_even_when_its_process_entry_reads_stopping() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world.edit(|record| {
        record
            .set_state(
                &ObjectKey::Process(name("api"), name("server")),
                EntryState::Stopping,
            )
            .unwrap();
    });

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::AlreadyRunning);
    assert_eq!(world.host.launches(), vec!["api/server"]);
}

/// Returns the container app `app` with the processes `server` and
/// `worker`, where `worker` depends on `server`.
fn two_container_app(app: &str) -> AppPlan {
    let mut plan = container_app(app);
    let mut worker = ProcessPlan::new(name("worker"), Vec::new());
    worker.depends.push(name("server"));
    plan.processes.push(worker);
    plan
}

#[test]
fn down_stops_the_other_containers_and_records_them_stopped_when_docker_fails_for_one() {
    let mut world = World::new();
    world.containers.stop_failing = Some("worker");
    let plan = plan(vec![two_container_app("db")]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();

    let report = world.down(&Selection::All);

    assert!(
        matches!(outcome(&report, "db"), AppOutcome::Errored { .. }),
        "{report:?}"
    );
    assert_eq!(
        *world.containers.stop_order.borrow(),
        ["db/worker", "db/server"]
    );
    let record = world.record().unwrap();
    assert_eq!(
        record.state(&ObjectKey::Container(name("db"), name("server"))),
        Some(EntryState::Stopped)
    );
    assert_ne!(
        record.state(&ObjectKey::Container(name("db"), name("worker"))),
        Some(EntryState::Stopped)
    );
}

#[test]
fn a_failed_container_start_records_every_container_it_created_and_those_it_removed_as_stopped() {
    let world = World::new();
    *world.containers.start_ending.borrow_mut() = Some(StartEnding::Failure);
    let plan = plan(vec![two_container_app("db")]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert!(
        matches!(outcome(&report, "db"), AppOutcome::Errored { .. }),
        "{report:?}"
    );
    let record = world.record().unwrap();
    let entry = |process: &str| {
        record
            .containers
            .iter()
            .find(|entry| entry.process.as_str() == process)
            .unwrap()
            .clone()
    };
    assert_eq!(entry("server").state, EntryState::Stopped);
    assert_eq!(entry("worker").state, EntryState::Running);
    assert!(entry("worker").id.is_some());
    assert_eq!(entry("worker").plan_hash, Some(fake_plan_hash()));
}

#[test]
fn a_container_start_that_a_deadline_cuts_short_is_uncertain_and_its_containers_stay() {
    let world = World::new();
    *world.containers.start_ending.borrow_mut() = Some(StartEnding::Uncertain);
    let plan = plan(vec![container_app("db")]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "db"), &AppOutcome::StartUncertain);
    assert!(world.containers.stopped.borrow().is_empty());
    assert_eq!(
        world
            .record()
            .unwrap()
            .state(&ObjectKey::Container(name("db"), name("server"))),
        Some(EntryState::Running)
    );
}

#[test]
fn up_replaces_a_running_container_that_phase_started_from_another_plan_of_its_process() {
    let world = World::new();
    let plan = plan(vec![container_app("db")]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world
        .containers
        .running
        .borrow_mut()
        .insert("db/server".to_owned());
    world.edit(|record| {
        for entry in &mut record.containers {
            entry.plan_hash = Some(PlanHash::new([9; 32]));
        }
    });

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "db"), &AppOutcome::Restarted);
    assert_eq!(world.containers.starts.borrow().len(), 2);
}

#[test]
fn a_container_start_that_fails_before_it_removes_anything_keeps_the_recorded_ids() {
    let world = World::new();
    let plan = plan(vec![container_app("db")]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    let failing = Env {
        failing: BTreeSet::from(["db".to_owned()]),
        ..Env::default()
    };

    let report = world.up(&plan, &Selection::All, &failing).unwrap();

    assert!(
        matches!(outcome(&report, "db"), AppOutcome::Errored { .. }),
        "{report:?}"
    );
    let record = world.record().unwrap();
    let entry = &record.containers[0];
    assert_eq!(entry.state, EntryState::Running);
    assert!(entry.id.is_some());
    assert_eq!(entry.plan_hash, Some(fake_plan_hash()));
}

#[test]
fn a_host_app_that_depends_on_two_container_apps_starts_after_both_and_stops_before_them() {
    let world = World::new();
    let plan = plan(vec![
        host_app("api", None, &["db", "cache"]),
        container_app("db"),
        container_app("cache"),
    ]);
    events();

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    for app in ["api", "db", "cache"] {
        assert_eq!(outcome(&report, app), &AppOutcome::Started, "{report:?}");
    }
    let started = events();
    let at = |what: &str| started.iter().position(|event| event == what).unwrap();
    assert!(
        at("host start api/server") > at("container start db/server"),
        "{started:?}"
    );
    assert!(
        at("host start api/server") > at("container start cache/server"),
        "{started:?}"
    );

    let report = world.down(&Selection::All);

    for app in ["api", "db", "cache"] {
        assert_eq!(outcome(&report, app), &AppOutcome::Stopped, "{report:?}");
    }
    let stopped = events();
    let at = |what: &str| stopped.iter().position(|event| event == what).unwrap();
    assert!(
        at("host stop api/server") < at("container stop db/server"),
        "{stopped:?}"
    );
    assert!(
        at("host stop api/server") < at("container stop cache/server"),
        "{stopped:?}"
    );
}

#[test]
fn a_container_process_with_literal_variables_is_runnable_and_reaches_its_runner() {
    let world = World::new();
    let mut db = container_app("db");
    db.processes[0].env.insert("SERVICES".to_owned());
    let plan = plan(vec![db]);
    let env = Env::default();

    let report = world.up(&plan, &Selection::All, &env).unwrap();

    assert_eq!(outcome(&report, "db"), &AppOutcome::Started, "{report:?}");
    assert_eq!(*env.resolved.borrow(), ["db/server"]);
}

#[test]
fn a_process_that_ends_during_readiness_reports_the_code_or_signal_its_keeper_read() {
    for (exit, code, signal) in [
        (Exit::Code(3), Some(3), None),
        (Exit::Signal(9), None, Some(9)),
    ] {
        let world = World::new();
        world.host.ends_as("api/server", exit);
        let mut api = host_app("api", None, &[]);
        api.processes[0].ready = Some(Probe::new(ProbeCheck::log("never".to_owned())));

        let report = world
            .up(&plan(vec![api]), &Selection::All, &Env::default())
            .unwrap();

        assert_eq!(
            outcome(&report, "api"),
            &AppOutcome::failed(name("server"), StartProblem::exited(code, signal)),
            "{exit:?}"
        );
    }
}

#[test]
fn a_process_without_a_probe_that_ends_within_its_start_grace_reports_its_exit_code() {
    let world = World::new();
    world.host.ends_as("api/server", Exit::Code(3));

    let report = world
        .up(
            &plan(vec![host_app("api", None, &[])]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();

    assert_eq!(
        outcome(&report, "api"),
        &AppOutcome::failed(name("server"), StartProblem::exited(Some(3), None))
    );
}

/// The full name of the keeper tests' entry, which runs this test binary as
/// a keeper when its arguments hold `keep`.
const KEEPER_ENTRY: &str =
    "engine::keeper::tests::the_test_binary_runs_a_keeper_when_it_is_started_as_one";

/// Starts the host app `api`, whose process runs `sh -c script` and waits
/// for a log line that never comes, under a real keeper that this test
/// binary runs, and returns the report of its start.
fn up_with_a_real_keeper(script: &str) -> Report {
    let state = tempfile::tempdir().unwrap();
    let keeper = KeeperCommand::new(
        std::env::current_exe().unwrap(),
        ["--exact", "--nocapture", "--", KEEPER_ENTRY, "keep"]
            .into_iter()
            .map(Into::into)
            .collect(),
    );
    let inherited = BTreeMap::from([
        ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
        ("HOME".to_owned(), state.path().display().to_string()),
    ]);
    let containers = FakeContainers::default();
    let engine = Engine {
        settings: Settings {
            state_dir: state.path(),
            lock_wait: Duration::from_secs(5),
            start_grace: Duration::from_millis(30),
            kill_wait: Duration::from_secs(1),
            keeper: Some(&keeper),
            inherited: &inherited,
            protected: &[],
        },
        host: &crate::record::process::System,
        containers: &containers,
    };
    let mut api = AppPlan::new(
        name("api"),
        RunnerKind::Host,
        Source::checkout(state.path().to_path_buf(), None),
    );
    let mut server = ProcessPlan::new(
        name("server"),
        vec!["/bin/sh".into(), "-c".into(), script.into()],
    );
    server.ready = Some(Probe::new(ProbeCheck::log("never".to_owned())));
    server.ready_timeout = Duration::from_secs(20);
    api.processes.push(server);
    let deadline = now() + Duration::from_secs(30);
    engine
        .up(
            &plan(vec![api]),
            &Selection::All,
            &Env::default(),
            UpDeadlines::new(deadline, deadline),
            &Cancel::new(),
        )
        .unwrap()
}

#[test]
fn a_real_keeper_reports_the_code_or_signal_of_a_process_that_ends_during_readiness() {
    for (script, code, signal) in [
        ("echo starting; sleep 0.2; exit 3", Some(3), None),
        ("echo starting; sleep 0.2; kill -9 $$", None, Some(9)),
    ] {
        let report = up_with_a_real_keeper(script);

        assert_eq!(
            outcome(&report, "api"),
            &AppOutcome::failed(name("server"), StartProblem::exited(code, signal)),
            "{script}"
        );
    }
}

#[test]
fn a_failed_up_leaves_the_process_that_ended_exited_with_its_code_until_down() {
    let world = World::new();
    world.host.ends_as("api/server", Exit::Code(3));
    let mut api = host_app("api", None, &[]);
    api.processes[0].ready = Some(Probe::new(ProbeCheck::log("never".to_owned())));
    let plan = plan(vec![api]);

    world.up(&plan, &Selection::All, &Env::default()).unwrap();

    let sources = crate::status::Sources {
        state_dir: world.state.path(),
        processes: &crate::record::testing::FakeProcesses::default(),
        docker: &crate::record::testing::Unreachable,
    };
    let store = RunStore::new(world.state.path());
    let status = crate::status::status(&store, &workspace(), Some(&plan), sources).unwrap();
    assert_eq!(
        status.rows[0].state,
        crate::status::ProcessState::exited(Some(3))
    );
    let status = crate::status::status(&store, &workspace(), None, sources).unwrap();
    assert_eq!(status.rows.len(), 1);
    assert_eq!(
        status.rows[0].state,
        crate::status::ProcessState::exited(Some(3))
    );

    world.down(&Selection::All);

    assert_eq!(world.record(), None);
}

#[test]
fn an_up_whose_source_leaves_out_a_required_variable_launches_nothing() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    let empty = Env {
        empty: BTreeSet::from(["api".to_owned()]),
        ..Env::default()
    };

    let error = world.up(&plan, &Selection::All, &empty).unwrap_err();

    assert_eq!(error.code(), "phase::environment");
    assert!(world.host.launches().is_empty());
}

#[test]
fn a_restart_whose_source_leaves_out_a_required_variable_stops_nothing() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    let empty = Env {
        empty: BTreeSet::from(["api".to_owned()]),
        ..Env::default()
    };

    let error = world
        .engine()
        .restart(&plan, &Selection::All, &empty, soon(), &Cancel::new())
        .unwrap_err();

    assert_eq!(error.code(), "phase::environment");
    assert!(world.host.stops().is_empty());
}

#[test]
fn a_down_of_other_apps_keeps_the_exit_of_a_process_it_did_not_select() {
    let world = World::new();
    world.host.ends_as("api/server", Exit::Code(3));
    let web = host_app("web", None, &[]);
    let mut api = host_app("api", None, &["web"]);
    api.processes[0].ready = Some(Probe::new(ProbeCheck::log("never".to_owned())));
    let plan = plan(vec![web, api]);

    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world.down(&only("web"));

    let sources = crate::status::Sources {
        state_dir: world.state.path(),
        processes: &crate::record::testing::FakeProcesses::default(),
        docker: &crate::record::testing::Unreachable,
    };
    let store = RunStore::new(world.state.path());
    let status = crate::status::status(&store, &workspace(), None, sources).unwrap();
    let api = status
        .rows
        .iter()
        .find(|row| row.app.as_str() == "api")
        .unwrap();
    assert_eq!(api.state, crate::status::ProcessState::exited(Some(3)));

    world.down(&Selection::All);

    assert_eq!(world.record(), None);
}

#[test]
fn a_down_of_one_process_keeps_the_exit_of_another_process_of_its_app_until_a_sweep() {
    let world = World::new();
    world.host.ends_as("api/worker", Exit::Code(3));
    let mut api = host_app("api", None, &[]);
    let mut worker = ProcessPlan::new(name("worker"), vec!["./work".into()]);
    worker.ready = Some(Probe::new(ProbeCheck::log("never".to_owned())));
    api.processes.push(worker);
    let plan = plan(vec![api]);

    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world.down(&Selection::only(vec![name::<Target>("api/server")]));

    let record = world.record().expect("the down removed the worker's exit");
    let worker = ObjectKey::Process(name("api"), name("worker"));
    assert_eq!(record.state(&worker), Some(EntryState::Stopped));

    let processes = crate::record::testing::FakeProcesses::default();
    crate::record::sweep::sweep(
        &RunStore::new(world.state.path()),
        &workspace(),
        false,
        crate::record::sweep::Reach {
            docker: &crate::record::testing::Unreachable,
            processes: &processes,
            host: &processes,
        },
        crate::record::sweep::Waits {
            lock: Duration::from_millis(50),
            kill: Duration::from_millis(20),
        },
    )
    .unwrap();

    assert_eq!(world.record(), None);
}
