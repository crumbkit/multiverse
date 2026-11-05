//! Tests of the container runner through `Phase::up`, `restart` and
//! `down` against a real Docker daemon. Each test skips when no daemon
//! answers, when the compose plugin is missing, or when `busybox:1.37.0`
//! is not pulled, which the tests do not pull. CI's lane pulls the image,
//! pinned by digest, so the tests run there. Each test uses a workspace id
//! of its own and removes what it created. The tests of the log reader at
//! the end need no daemon.

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
use crate::plan::restart::{Restart, RestartLimits};
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
    if docker(&["compose", "version"]).is_none() {
        println!("skipping: docker's compose plugin is not installed");
        return false;
    }
    if docker(&["image", "inspect", IMAGE]).is_none() {
        println!("skipping: {IMAGE} is not pulled");
        return false;
    }
    true
}

/// The value of `SERVICES` that the tests' environment source gives, a
/// literal that is not a secret.
const SERVICES: &str = "s3,sqs,kms";

/// An environment source that gives every process `MODE` and `SERVICES`.
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
        BTreeSet::from(["MODE".to_owned(), "SERVICES".to_owned()])
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
        if let Some(name) = reference("SERVICES") {
            command.env(name, SERVICES);
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

/// Returns a TCP port on 127.0.0.1 and the listener that holds it, which
/// the test drops just before phase publishes the port, so no other test
/// takes the port meanwhile.
fn free_port() -> (NonZeroU16, TcpListener) {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    let port = NonZeroU16::new(listener.local_addr().unwrap().port()).unwrap();
    (port, listener)
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
fn a_log_probe_reads_the_containers_output() {
    if !docker_ready() {
        return;
    }
    let world = World::new();
    let mut db = app("db", "sleep 1; echo \"ready $MODE\"; exec sleep 60");
    db.processes[0].ready = Some(Probe::new(ProbeCheck::log(format!("^ready {MODE}$"))));
    let plan = world.plan(vec![db]);

    let began = Instant::now();
    let report = world.up(&plan);

    assert_eq!(outcome(&report, "db"), &AppOutcome::Started, "{report:?}");
    assert!(began.elapsed() >= Duration::from_secs(1));
    world.down();
}

#[test]
fn a_tcp_probe_waits_for_a_listener_inside_the_container_and_not_for_dockers_proxy() {
    if !docker_ready() {
        return;
    }
    let world = World::new();
    let (port, holder) = free_port();
    let mut web = app("web", "sleep 4; exec nc -lk -p 8080 -e cat");
    web.ports.insert("http".parse().unwrap(), port);
    web.processes[0]
        .publish
        .insert("http".parse().unwrap(), NonZeroU16::new(8080).unwrap());
    // A fixed container port, published to the workspace's host port, with
    // the 60-second attempt timeout that mxv's manifests use.
    let mut probe = Probe::new(ProbeCheck::tcp("http".parse().unwrap()));
    probe.timeout = Duration::from_secs(60);
    web.processes[0].ready = Some(probe);
    let plan = world.plan(vec![web]);

    drop(holder);
    let began = Instant::now();
    let report = world.up(&plan);

    assert_eq!(outcome(&report, "web"), &AppOutcome::Started, "{report:?}");
    assert!(
        began.elapsed() >= Duration::from_secs(3),
        "{:?}",
        began.elapsed()
    );
    world.down();
}

#[test]
fn a_container_that_crashes_in_a_restart_loop_fails_to_start_with_its_output() {
    if !docker_ready() {
        return;
    }
    let world = World::new();
    let mut db = app("db", "echo crashing; exit 3");
    db.processes[0].restart = Restart::on_failure(RestartLimits::default());
    db.processes[0].ready = Some(Probe::new(ProbeCheck::command(vec!["false".to_owned()])));
    db.processes[0].ready_timeout = Duration::from_secs(300);
    let plan = world.plan(vec![db]);

    let began = Instant::now();
    let report = world.up(&plan);

    match outcome(&report, "db") {
        AppOutcome::Failed {
            problem: StartProblem::Exited { log_tail, .. },
            ..
        } => assert!(
            log_tail.iter().any(|line| line == "crashing"),
            "{log_tail:?}"
        ),
        other => panic!("{other:?}"),
    }
    // Without the restart check, the wait would last the 300-second
    // ready timeout, or the 60-second deadline of the test. The bound
    // leaves room for a slow pull and `compose up` on a busy runner.
    assert!(began.elapsed() < Duration::from_secs(45));
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
fn restart_replaces_the_container_with_a_new_one() {
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
    world.down();
}

#[test]
fn up_leaves_a_running_container_of_the_same_plan_alone_and_replaces_a_stopped_or_changed_one() {
    if !docker_ready() {
        return;
    }
    let world = World::new();
    let plan = world.plan(vec![app("db", "exec sleep 60")]);
    world.up(&plan);
    let first = world.container("db", "server").unwrap();

    let report = world.up(&plan);

    assert_eq!(
        outcome(&report, "db"),
        &AppOutcome::AlreadyRunning,
        "{report:?}"
    );
    assert_eq!(world.container("db", "server").unwrap(), first);

    docker(&["stop", "--time", "0", "--", &first]).unwrap();
    let report = world.up(&plan);

    assert_eq!(outcome(&report, "db"), &AppOutcome::Started, "{report:?}");
    let second = world.container("db", "server").unwrap();
    assert_ne!(first, second);
    assert!(docker(&["container", "inspect", &first]).is_none());

    let changed = world.plan(vec![app("db", "exec sleep 61")]);
    let report = world.up(&changed);

    assert_eq!(outcome(&report, "db"), &AppOutcome::Started, "{report:?}");
    let third = world.container("db", "server").unwrap();
    assert_ne!(second, third);
    assert!(docker(&["container", "inspect", &second]).is_none());
    world.down();
}

/// Returns what `docker logs --timestamps` prints for `text` at the
/// nanosecond `at` of a fixed second.
fn logged(at: u32, text: &str) -> String {
    format!("2026-11-05T10:00:00.{at:09}Z {text}\n")
}

/// Returns a log reader of a made-up container whose streams keep `cap`
/// bytes.
fn logs(cap: u64) -> super::DockerLogs {
    let mut logs = super::DockerLogs::new(
        crate::engine::container::names::ContainerId::new(&"a".repeat(64)).unwrap(),
    );
    logs.cap = cap;
    logs
}

#[test]
fn a_log_read_hands_each_line_over_once_in_time_order() {
    let mut reader = logs(1 << 20);
    let out = logged(1, "one") + &logged(3, "three");
    let err = logged(2, "two");
    assert_eq!(reader.after(&out, &err), ["one", "two", "three"]);
    // The next read, from the last line's time, gets that line again.
    let out = logged(3, "three") + &logged(4, "four");
    assert_eq!(reader.after(&out, ""), ["four"]);
}

#[test]
fn a_full_stream_holds_back_every_line_after_its_last_whole_line() {
    let out = logged(1, "one") + &logged(4, "four") + &logged(5, "fi");
    let cap = u64::try_from(out.len()).unwrap();
    let mut reader = logs(cap);
    let err = logged(2, "two") + &logged(6, "six");
    // stdout filled its cap, so `fi` may be cut and stdout lines after
    // `four` were not read; `six` waits for the next read.
    assert_eq!(reader.after(&out, &err), ["one", "two", "four"]);
    let mut reader2 = logs(1 << 20);
    reader2.since = reader.since.clone();
    let out = logged(4, "four") + &logged(5, "five");
    let err = logged(6, "six");
    assert_eq!(reader2.after(&out, &err), ["five", "six"]);
}

#[test]
fn a_line_longer_than_a_read_keeps_is_handed_over_cut_and_the_reads_move_past_it() {
    let long = logged(1, &"x".repeat(100));
    let cut = &long[..60];
    let mut reader = logs(60);
    let handed = reader.after(cut, "");
    assert_eq!(handed.len(), 1);
    assert!(handed[0].ends_with(super::CUT_LINE), "{handed:?}");
    assert_eq!(
        reader.since.as_ref().map(|(since, _)| since.as_str()),
        Some("2026-11-05T10:00:00.000000002Z")
    );
    assert_eq!(
        super::just_after("2026-11-05T10:00:00.000000999Z"),
        "2026-11-05T10:00:00.000001000Z"
    );
}

#[test]
fn a_refused_restart_policy_update_fails_only_a_container_that_still_runs() {
    use crate::engine::container::ContainerState;
    let refused = || {
        Err(crate::engine::container::ContainerError::Failed {
            operation: "update",
            container: Some("server".to_owned()),
            message: "cannot update a stopped container".to_owned(),
        })
    };
    let runs = |state: &ContainerState| state.runs();
    assert!(super::settled(refused(), Some(ContainerState::Running), runs).is_err());
    assert!(super::settled(refused(), Some(ContainerState::Paused), runs).is_err());
    for state in [
        ContainerState::Exited(1),
        ContainerState::Restarting,
        ContainerState::Created,
    ] {
        assert_eq!(
            super::settled(refused(), Some(state), runs).unwrap(),
            Some(state)
        );
    }
    assert_eq!(super::settled(refused(), None, runs).unwrap(), None);
    assert_eq!(
        super::settled(Ok(()), Some(ContainerState::Running), runs).unwrap(),
        Some(ContainerState::Running)
    );
}

#[test]
fn the_time_just_after_a_stamp_carries_into_the_seconds_and_the_date() {
    for (stamp, after) in [
        (
            "2026-11-05T10:00:59.999999999Z",
            "2026-11-05T10:01:00.000000000Z",
        ),
        (
            "2026-12-31T23:59:59.999999999Z",
            "2027-01-01T00:00:00.000000000Z",
        ),
        (
            "2028-02-28T23:59:59.999999999Z",
            "2028-02-29T00:00:00.000000000Z",
        ),
        (
            "2026-02-28T23:59:59.999999999Z",
            "2026-03-01T00:00:00.000000000Z",
        ),
        ("2026-11-05T10:00:00.5Z", "2026-11-05T10:00:00.500000001Z"),
    ] {
        assert_eq!(super::just_after(stamp), after, "{stamp}");
    }
    assert_eq!(super::just_after("not a time"), "not a time");
}

#[test]
fn a_read_moves_past_a_cut_line_even_when_another_line_shares_its_time() {
    let long = logged(1, &"x".repeat(100));
    let cut = &long[..60];
    let mut reader = logs(60);
    let handed = reader.after(cut, &logged(1, "beside"));
    assert_eq!(handed.len(), 2, "{handed:?}");
    assert_eq!(
        reader.since.as_ref().map(|(since, _)| since.as_str()),
        Some("2026-11-05T10:00:00.000000002Z")
    );
}

#[test]
fn a_container_runs_its_own_command_and_receives_its_literal_variables() {
    if !docker_ready() {
        return;
    }
    let world = World::new();
    let mut db = app("db", "echo overridden; exec sleep 60");
    db.processes[0].env.insert("SERVICES".to_owned());
    let plan = world.plan(vec![db]);

    let report = world.up(&plan);

    assert_eq!(outcome(&report, "db"), &AppOutcome::Started, "{report:?}");
    let id = world.container("db", "server").unwrap();
    let printed = docker(&["exec", "--", &id, "printenv", "SERVICES"]).unwrap();
    assert_eq!(printed.trim(), SERVICES);
    let command = docker(&[
        "container",
        "inspect",
        "--format",
        "{{json .Config.Cmd}}",
        &id,
    ])
    .unwrap();
    assert_eq!(
        command.trim(),
        r#"["sh","-c","echo overridden; exec sleep 60"]"#
    );
    let config = docker(&[
        "container",
        "inspect",
        "--format",
        "{{json .Config.Labels}}",
        &id,
    ])
    .unwrap();
    assert!(!config.contains(SERVICES), "{config}");
    world.down();
}
