use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use tempfile::TempDir;

use super::{Engine, Settings};
use crate::cancel::Cancel;
use crate::clock::now;
use crate::engine::containers::{
    ContainerObjects, ContainerRunner, ContainerStart, ContainerStop, ContainerStopped,
    ContainersStarted,
};
use crate::engine::host::{Holder, HostRunner, HostStop, HostStopped};
use crate::engine::keeper::{KeeperCommand, Kept, Launch};
use crate::engine::probe::LogLines;
use crate::env::{AppliedEnvironment, EnvironmentError, EnvironmentSource};
use crate::error::PhaseError;
use crate::name::{AppName, ProcessName};
use crate::phase::UpDeadlines;
use crate::plan::probe::{Probe, ProbeCheck};
use crate::plan::{AppPlan, Plan, ProcessKind, ProcessPlan, RunnerKind};
use crate::record::process::Identity;
use crate::record::run::{EntryState, HostProcess, ObjectKey, RunRecord};
use crate::record::store::RunStore;
use crate::record::testing::workspace;
use crate::record::{ContainerId, DockerName};
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
        Ok(Kept::detached(host(keeper), host(app)))
    }

    fn stop(&self, stop: &HostStop<'_>, _deadline: Instant) -> Result<HostStopped, PhaseError> {
        let mut state = self.state.borrow_mut();
        state.stop_calls.push((now(), _deadline));
        let pid = stop.app.or(stop.keeper).map_or(0, |host| host.pid);
        let name = state.names.get(&pid).cloned().unwrap_or_default();
        state.stops.push(name.clone());
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

    fn start(&self, start: &ContainerStart<'_>) -> Result<ContainersStarted, PhaseError> {
        assert_eq!(start.workspace, &workspace());
        assert!(start.deadline > now());
        if start.cancel.is_cancelled() {
            return Ok(ContainersStarted::default());
        }
        if let Some(state_dir) = self.state_dir.borrow().as_ref() {
            let record = RunStore::new(state_dir)
                .read(start.workspace)
                .unwrap()
                .unwrap();
            for process in start.processes {
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
        for process in start.processes {
            start
                .env
                .resolve(&start.app.name, process, start.deadline)
                .map_err(|source| PhaseError::Environment {
                    app: start.app.name.clone(),
                    source,
                })?;
        }
        self.started.borrow_mut().push(start.app.name.to_string());
        Ok(ContainersStarted {
            containers: start
                .processes
                .iter()
                .map(|process| {
                    let id: ContainerId = "c0".repeat(32).try_into().unwrap();
                    (process.clone(), id)
                })
                .collect(),
            ..ContainersStarted::default()
        })
    }

    fn stop(
        &self,
        stop: &ContainerStop<'_>,
    ) -> Result<Vec<(ProcessName, ContainerStopped)>, PhaseError> {
        assert_eq!(stop.workspace, &workspace());
        assert!(stop.deadline > now());
        self.stopped.borrow_mut().push(stop.app.to_string());
        Ok(stop
            .containers
            .iter()
            .map(|entry| {
                let how = match (&entry.id, self.stubborn) {
                    (None, _) => ContainerStopped::AlreadyGone,
                    (Some(_), true) => ContainerStopped::Unstoppable,
                    (Some(_), false) => ContainerStopped::Stopped,
                };
                (entry.process.clone(), how)
            })
            .collect())
    }
}

/// An environment source that gives every process `API_TOKEN`, and fails
/// for the apps that `failing` names.
#[derive(Default)]
struct Env {
    /// The apps whose environment cannot be resolved.
    failing: BTreeSet<String>,
    /// Each `app/process` resolved, in order.
    resolved: RefCell<Vec<String>>,
}

impl EnvironmentSource for Env {
    fn resolve(
        &self,
        app: &AppName,
        process: &ProcessName,
        _deadline: Instant,
    ) -> Result<Box<dyn AppliedEnvironment>, EnvironmentError> {
        self.resolved.borrow_mut().push(format!("{app}/{process}"));
        if self.failing.contains(app.as_str()) {
            return Err(EnvironmentError::new(
                "API_TOKEN".to_owned(),
                "vault".to_owned(),
                "the vault is locked".into(),
            ));
        }
        Ok(Box::new(Secret))
    }
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
        let state = tempfile::tempdir().unwrap();
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
fn a_process_that_ends_before_its_probe_passes_reports_the_last_lines_of_its_log() {
    let world = World::new();
    world.host.ends("api/server");
    world.host.state.borrow_mut().logs.insert(
        "api.server.log".to_owned(),
        vec!["missing config".to_owned()],
    );
    let mut api = host_app("api", None, &[]);
    api.processes[0].ready = Some(Probe::new(ProbeCheck::log("never".to_owned())));

    let report = world
        .up(&plan(vec![api]), &Selection::All, &Env::default())
        .unwrap();

    assert_eq!(
        outcome(&report, "api"),
        &AppOutcome::failed(
            name("server"),
            StartProblem::exited(None, None, vec!["missing config".to_owned()])
        )
    );
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
fn an_app_with_a_oneshot_is_reported_as_not_runnable() {
    let world = World::new();
    let mut api = host_app("api", None, &[]);
    api.processes[0].kind = ProcessKind::oneshot(false);

    let report = world
        .up(&plan(vec![api]), &Selection::All, &Env::default())
        .unwrap();

    assert!(matches!(
        outcome(&report, "api"),
        AppOutcome::Errored { .. }
    ));
    assert!(world.host.launches().is_empty());
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
fn an_up_that_leaves_nothing_running_removes_the_record() {
    let world = World::new();
    world.host.ends("api/server");

    world
        .up(
            &plan(vec![host_app("api", None, &[])]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();

    assert_eq!(world.record(), None);
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
