//! Tests of `Phase::exec` into containers against a real Docker daemon.
//! Each test skips when no daemon answers, when the compose plugin is
//! missing, or when `busybox:1.37.0` is not pulled, as the container
//! runner's tests do. Each test uses a workspace id of its own and removes
//! what it created.

use std::collections::BTreeSet;
use std::fs::File;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::exec_args;
use crate::cancel::Cancel;
use crate::engine::exec::{ExecIo, ExecRequest};
use crate::env::{AppliedEnvironment, EnvironmentError, EnvironmentSource};
use crate::error::PhaseError;
use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::phase::{Phase, UpDeadlines};
use crate::plan::{AppPlan, Plan, ProcessPlan, RunnerKind};
use crate::record::ContainerId;
use crate::record::store::RunStore;
use crate::report::AppOutcome;
use crate::select::Selection;
use crate::source::Source;

/// The image of the tests.
const IMAGE: &str = "busybox:1.37.0";

/// The value of `MODE` that the tests' environment source gives.
const MODE: &str = "exec-mode-4b2d";

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

/// An environment source that gives every process `MODE`.
struct Mode;

impl EnvironmentSource for Mode {
    fn resolve(
        &self,
        _app: &AppName,
        _process: &ProcessName,
        _deadline: Instant,
    ) -> Result<Box<dyn AppliedEnvironment>, EnvironmentError> {
        Ok(Box::new(Mode))
    }
}

impl AppliedEnvironment for Mode {
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

/// A workspace of the tests with one container app, `api`, whose process
/// `server` receives `MODE` and sleeps.
struct World {
    state: TempDir,
    id: WorkspaceId,
    phase: Phase,
    plan: Plan,
}

impl World {
    fn new() -> Self {
        let state = tempfile::tempdir().unwrap();
        let id: WorkspaceId = crate::random::hex_id().unwrap().parse().unwrap();
        let phase =
            Phase::new(state.path().to_path_buf()).with_start_grace(Duration::from_millis(500));
        let mut app = AppPlan::new(
            "api".parse().unwrap(),
            RunnerKind::Container,
            Source::image(IMAGE.parse().unwrap()),
        );
        let mut server = ProcessPlan::new(
            "server".parse().unwrap(),
            vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "exec sleep 300".to_owned(),
            ],
        );
        server.env.insert("MODE".to_owned());
        server.stop.grace = Duration::from_secs(1);
        app.processes.push(server);
        let plan = Plan::new("phase-test".parse().unwrap(), id.clone(), vec![app]).unwrap();
        Self {
            state,
            id,
            phase,
            plan,
        }
    }

    fn up(&self) {
        let ready = Instant::now() + Duration::from_secs(180);
        let report = self
            .phase
            .up(
                &self.plan,
                &Selection::All,
                &Mode,
                UpDeadlines::new(ready, ready),
                &Cancel::new(),
            )
            .unwrap();
        assert_eq!(report.apps[0].outcome, AppOutcome::Started, "{report:?}");
    }

    /// Runs `script` in `sh` in the container of `api/server`, with its
    /// output in a file, and returns its status and output.
    fn exec(&self, script: &str) -> (Result<ExitStatus, PhaseError>, String) {
        let out = self.state.path().join("exec.out");
        #[expect(
            clippy::disallowed_methods,
            reason = "the test gives the command a file of its own for its output"
        )]
        let file = File::create(&out).unwrap();
        let request = ExecRequest::new(
            "api".parse().unwrap(),
            "server".parse().unwrap(),
            vec!["sh".into(), "-c".into(), script.into()],
            ExecIo::Streams {
                stdin: Stdio::null(),
                stdout: Stdio::from(file.try_clone().unwrap()),
                stderr: Stdio::from(file),
            },
        );
        let result = self.phase.exec(
            &self.plan,
            request,
            &Mode,
            Instant::now() + Duration::from_secs(30),
            &Cancel::new(),
        );
        (result, std::fs::read_to_string(&out).unwrap_or_default())
    }

    /// Returns the recorded id of the container of `api/server`.
    fn container(&self) -> String {
        let record = RunStore::new(self.state.path())
            .read(&self.id)
            .unwrap()
            .unwrap();
        record.containers[0]
            .id
            .as_ref()
            .unwrap()
            .as_str()
            .to_owned()
    }

    /// Removes every container that carries this workspace's id label.
    fn clean(&self) {
        let label = format!("label=mxv.workspace-id={}", self.id);
        let listed = docker(&["container", "ls", "--all", "--quiet", "--filter", &label])
            .unwrap_or_default();
        for object in listed.split_whitespace() {
            let _ = docker(&["rm", "--force", "--", object]);
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        self.clean();
    }
}

#[test]
fn the_arguments_of_docker_exec_put_the_commands_words_after_the_container() {
    assert_eq!(
        exec_args("abc", &["sh".into(), "-c".into(), "--tty".into()], false),
        ["exec", "--interactive", "--", "abc", "sh", "-c", "--tty"]
    );
    assert_eq!(
        exec_args("abc", &["true".into()], true),
        ["exec", "--interactive", "--tty", "--", "abc", "true"]
    );
}

#[test]
fn exec_into_a_running_container_returns_the_commands_exit_code_and_sees_its_environment() {
    if !docker_ready() {
        return;
    }
    let world = World::new();
    world.up();

    let (status, output) = world.exec("echo \"mode=$MODE\"; exit 3");

    assert_eq!(status.unwrap().code(), Some(3), "{output}");
    assert_eq!(output, format!("mode={MODE}\n"));
}

#[test]
fn exec_into_a_stopped_container_is_refused() {
    if !docker_ready() {
        return;
    }
    let world = World::new();
    world.up();
    docker(&["stop", "--time", "1", "--", &world.container()]).unwrap();

    let (status, _) = world.exec("exit 0");

    let error = status.unwrap_err();
    assert_eq!(error.code(), "phase::exec::not_running");
    assert_eq!(
        error.to_string(),
        "the container of api/server is not running, so phase cannot run a command in it; start it with up first"
    );
}

#[test]
fn exec_into_an_app_that_is_not_up_is_refused() {
    if !docker_ready() {
        return;
    }
    let world = World::new();

    let (status, _) = world.exec("exit 0");

    assert_eq!(status.unwrap_err().code(), "phase::exec::not_running");
}

#[test]
fn exec_into_a_container_whose_labels_name_another_workspace_is_refused() {
    if !docker_ready() {
        return;
    }
    let world = World::new();
    world.up();
    let foreign = docker(&[
        "run",
        "--detach",
        "--label",
        &format!("mxv.workspace-id={}", world.id),
        IMAGE,
        "sleep",
        "300",
    ])
    .unwrap();
    let foreign = foreign.trim();
    // The record now names the foreign container for api/server.
    let store = RunStore::new(world.state.path());
    let lock = store.lock(&world.id, Duration::from_secs(5)).unwrap();
    let mut record = store.read(&world.id).unwrap().unwrap();
    record.containers[0].id = Some(ContainerId::try_from(foreign.to_owned()).unwrap());
    store.write(&record, &lock).unwrap();
    drop(lock);

    let (status, output) = world.exec("touch /ran");

    let error = status.unwrap_err();
    assert_eq!(error.code(), "phase::docker::refused", "{error}");
    assert!(output.is_empty(), "{output}");
    assert!(docker(&["exec", "--", foreign, "test", "-e", "/ran"]).is_none());
    let _ = docker(&["rm", "--force", "--", foreign]);
}
