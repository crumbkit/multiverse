//! Tests of the container runner through `Phase::up`, `restart` and
//! `down` against a real Docker daemon. Each test skips when no daemon
//! answers or when `busybox:1.37.0` is not pulled, which the tests do not
//! pull. Each test uses a workspace id of its own and removes what it
//! created.

use std::collections::BTreeSet;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use crate::cancel::Cancel;
use crate::env::{AppliedEnvironment, EnvironmentError, EnvironmentSource};
use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::phase::{Phase, UpDeadlines};
use crate::plan::probe::{Probe, ProbeCheck};
use crate::plan::{AppPlan, Plan, ProcessPlan, RunnerKind};
use crate::record::run::EntryState;
use crate::record::store::RunStore;
use crate::report::{AppOutcome, Report, StartProblem};
use crate::select::Selection;
use crate::source::Source;

/// The image of the tests.
const IMAGE: &str = "busybox:1.37.0";

/// The value of `MODE` that the tests' environment source gives, which is
/// not a secret.
const MODE: &str = "test-mode-7c1e";

/// Runs the real `docker <args>` and returns its standard output when it
/// succeeded.
fn docker(args: &[&str]) -> Option<String> {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test reads and removes objects of its own in a real docker daemon"
    )]
    let output = Command::new("docker").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Reports whether the tests can run, and prints why when they cannot.
fn docker_ready() -> bool {
    if docker(&["version", "--format", "{{.Server.Version}}"]).is_none() {
        println!("skipping: no docker daemon answers");
        return false;
    }
    if docker(&["image", "inspect", IMAGE]).is_none() {
        println!("skipping: {IMAGE} is not pulled");
        return false;
    }
    true
}

/// An environment source that gives every process `MODE`.
struct Mode;

impl EnvironmentSource for Mode {
    fn resolve(
        &self,
        _app: &AppName,
        _process: &ProcessName,
        _deadline: Instant,
    ) -> Result<Box<dyn AppliedEnvironment>, EnvironmentError> {
        Ok(Box::new(ModeValue))
    }
}

/// The environment that holds `MODE`.
struct ModeValue;

impl AppliedEnvironment for ModeValue {
    fn names(&self) -> BTreeSet<String> {
        BTreeSet::from(["MODE".to_owned()])
    }

    fn expires_at(&self) -> Option<std::time::SystemTime> {
        None
    }

    fn digest(&self, key: &[u8; 32]) -> [u8; 32] {
        *key
    }

    fn apply_as(&self, command: &mut Command, reference: &dyn Fn(&str) -> Option<String>) {
        if let Some(name) = reference("MODE") {
            command.env(name, MODE);
        }
    }
}

/// A workspace of the tests: its state directory, its id and its phase.
struct World {
    state: TempDir,
    id: WorkspaceId,
    phase: Phase,
}

impl World {
    fn new() -> Self {
        let state = tempfile::tempdir().unwrap();
        let phase =
            Phase::new(state.path().to_path_buf()).with_start_grace(Duration::from_millis(500));
        Self {
            id: crate::random::hex_id().unwrap().parse().unwrap(),
            phase,
            state,
        }
    }

    fn plan(&self, apps: Vec<AppPlan>) -> Plan {
        Plan::new("phase-test".parse().unwrap(), self.id.clone(), apps).unwrap()
    }

    fn up(&self, plan: &Plan) -> Report {
        let ready = Instant::now() + Duration::from_secs(60);
        self.phase
            .up(
                plan,
                &Selection::All,
                &Mode,
                UpDeadlines::new(ready, ready),
                &Cancel::new(),
            )
            .unwrap()
    }

    fn down(&self) -> Report {
        self.phase
            .down(
                &self.id,
                &Selection::All,
                Instant::now() + Duration::from_secs(30),
            )
            .unwrap()
    }

    /// Returns the recorded id of the container of `process` of `app`.
    fn container(&self, app: &str, process: &str) -> Option<String> {
        let record = RunStore::new(self.state.path()).read(&self.id).unwrap()?;
        record
            .containers
            .iter()
            .find(|entry| entry.app.as_str() == app && entry.process.as_str() == process)
            .filter(|entry| entry.state == EntryState::Running)
            .and_then(|entry| entry.id.as_ref())
            .map(|id| id.as_str().to_owned())
    }

    /// Removes every container and volume that carries this workspace's
    /// id label.
    fn clean(&self) {
        let label = format!("label=mxv.workspace-id={}", self.id);
        for kind in ["container", "volume"] {
            let listed = docker(&[kind, "ls", "--all", "--quiet", "--filter", &label])
                .or_else(|| docker(&[kind, "ls", "--quiet", "--filter", &label]))
                .unwrap_or_default();
            for object in listed.split_whitespace() {
                let _ = match kind {
                    "container" => docker(&["rm", "--force", "--", object]),
                    _ => docker(&["volume", "rm", "--", object]),
                };
            }
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        self.clean();
    }
}

/// Returns a TCP port on 127.0.0.1 that nothing held a moment ago.
fn free_port() -> NonZeroU16 {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    NonZeroU16::new(listener.local_addr().unwrap().port()).unwrap()
}

/// Returns the container app `app` with one process, `server`, which runs
/// `script` in `sh` and receives `MODE`.
fn app(name: &str, script: &str) -> AppPlan {
    let mut app = AppPlan::new(
        name.parse().unwrap(),
        RunnerKind::Container,
        Source::image(IMAGE.parse().unwrap()),
    );
    let mut server = ProcessPlan::new(
        "server".parse().unwrap(),
        vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
    );
    server.env.insert("MODE".to_owned());
    server.stop.grace = Duration::from_secs(1);
    app.processes.push(server);
    app
}

fn outcome<'r>(report: &'r Report, app: &str) -> &'r AppOutcome {
    &report
        .apps
        .iter()
        .find(|report| report.app.as_str() == app)
        .unwrap()
        .outcome
}

#[test]
fn up_runs_a_container_app_that_a_command_probe_finds_ready_and_down_removes_it() {
    if !docker_ready() {
        return;
    }
    let world = World::new();
    let mut db = app("db", "touch /tmp/ready; exec sleep 60");
    db.processes[0].ready = Some(Probe::new(ProbeCheck::command(vec![
        "test".to_owned(),
        "-e".to_owned(),
        "/tmp/ready".to_owned(),
    ])));
    db.processes[0]
        .volumes
        .insert("data".parse().unwrap(), PathBuf::from("/data"));
    let plan = world.plan(vec![db]);

    let report = world.up(&plan);

    assert_eq!(outcome(&report, "db"), &AppOutcome::Started, "{report:?}");
    let id = world.container("db", "server").unwrap();
    let printed = docker(&["exec", "--", &id, "printenv", "MODE"]).unwrap();
    assert_eq!(printed.trim(), MODE);
    let labels = docker(&[
        "container",
        "inspect",
        "--format",
        "{{json .Config.Labels}}",
        &id,
    ])
    .unwrap();
    assert!(
        labels.contains(&format!("\"mxv.workspace-id\":\"{}\"", world.id)),
        "{labels}"
    );
    assert!(labels.contains("\"mxv.app\":\"db\""), "{labels}");
    assert!(!labels.contains(MODE), "{labels}");
    let mounts = docker(&["container", "inspect", "--format", "{{json .Mounts}}", &id]).unwrap();
    assert!(mounts.contains(".data"), "{mounts}");

    let report = world.down();

    assert_eq!(outcome(&report, "db"), &AppOutcome::Stopped, "{report:?}");
    assert!(docker(&["container", "inspect", &id]).is_none());
}

#[test]
fn a_container_that_exits_before_it_is_ready_fails_with_its_exit_code_and_output_and_is_removed() {
    if !docker_ready() {
        return;
    }
    let world = World::new();
    let plan = world.plan(vec![app("db", "echo going away; exit 3")]);

    let report = world.up(&plan);

    match outcome(&report, "db") {
        AppOutcome::Failed {
            problem: StartProblem::Exited { code, log_tail, .. },
            ..
        } => {
            assert_eq!(*code, Some(3));
            assert!(
                log_tail.iter().any(|line| line == "going away"),
                "{log_tail:?}"
            );
        }
        other => panic!("{other:?}"),
    }
    let label = format!("label=mxv.workspace-id={}", world.id);
    let left = docker(&["container", "ls", "--all", "--quiet", "--filter", &label]).unwrap();
    assert_eq!(left.trim(), "");
}

#[test]
fn a_log_probe_reads_the_containers_output_and_a_tcp_probe_waits_for_a_listener_inside() {
    if !docker_ready() {
        return;
    }
    let world = World::new();
    let port = free_port();
    let mut db = app(
        "db",
        "sleep 1; echo \"ready $MODE\"; exec nc -lk -p 8080 -e cat",
    );
    db.processes[0].ready = Some(Probe::new(ProbeCheck::log(format!("^ready {MODE}$"))));
    let mut web = app("web", "sleep 1; exec nc -lk -p 8080 -e cat");
    web.ports.insert("http".parse().unwrap(), port);
    web.processes[0]
        .publish
        .insert("http".parse().unwrap(), NonZeroU16::new(8080).unwrap());
    web.processes[0].ready = Some(Probe::new(ProbeCheck::tcp("http".parse().unwrap())));
    let plan = world.plan(vec![db, web]);

    let began = Instant::now();
    let report = world.up(&plan);

    assert_eq!(outcome(&report, "db"), &AppOutcome::Started, "{report:?}");
    assert_eq!(outcome(&report, "web"), &AppOutcome::Started, "{report:?}");
    assert!(began.elapsed() >= Duration::from_secs(2));
    world.down();
}

#[test]
fn a_published_port_that_something_holds_fails_the_start_before_docker_runs() {
    if !docker_ready() {
        return;
    }
    let world = World::new();
    let holder = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    let port = NonZeroU16::new(holder.local_addr().unwrap().port()).unwrap();
    let mut web = app("web", "exec sleep 60");
    web.ports.insert("http".parse().unwrap(), port);
    web.processes[0]
        .publish
        .insert("http".parse().unwrap(), NonZeroU16::new(8080).unwrap());
    let plan = world.plan(vec![web]);

    let report = world.up(&plan);

    assert!(
        matches!(
            outcome(&report, "web"),
            AppOutcome::Failed { problem: StartProblem::PortTaken { port, .. }, .. } if port.as_str() == "http"
        ),
        "{report:?}"
    );
    drop(holder);
}

#[test]
fn restart_recreates_the_container_and_up_replaces_a_running_one() {
    if !docker_ready() {
        return;
    }
    let world = World::new();
    let plan = world.plan(vec![app("db", "exec sleep 60")]);
    world.up(&plan);
    let first = world.container("db", "server").unwrap();

    let report = world
        .phase
        .restart(
            &plan,
            &Selection::All,
            &Mode,
            Instant::now() + Duration::from_secs(60),
            &Cancel::new(),
        )
        .unwrap();

    assert_eq!(outcome(&report, "db"), &AppOutcome::Restarted, "{report:?}");
    let second = world.container("db", "server").unwrap();
    assert_ne!(first, second);
    assert!(docker(&["container", "inspect", &first]).is_none());

    world.up(&plan);
    let third = world.container("db", "server").unwrap();
    assert_ne!(second, third);
    assert!(docker(&["container", "inspect", &second]).is_none());
    world.down();
}
