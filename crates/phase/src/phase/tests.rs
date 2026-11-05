use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{DEFAULT_KILL_WAIT, DEFAULT_LOCK_WAIT, DEFAULT_START_GRACE, Phase, UpDeadlines};
use crate::AppliedEnvironment;
use crate::cancel::Cancel;
use crate::env::{EnvironmentError, EnvironmentSource};
use crate::error::{Operation, PhaseError};
use crate::logs::{LineSink, LogLine, LogRequest};
use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::plan::Plan;
use crate::record::store::RunStore;
use crate::select::Selection;

struct Unused;

impl EnvironmentSource for Unused {
    fn resolve(
        &self,
        app: &AppName,
        _process: &ProcessName,
        _deadline: Instant,
    ) -> Result<Box<dyn AppliedEnvironment>, EnvironmentError> {
        Err(EnvironmentError::new(
            app.to_string(),
            "test".into(),
            "the test resolves nothing".into(),
        ))
    }
}

impl LineSink for Unused {
    fn line(&mut self, _line: &LogLine<'_>) -> ControlFlow<()> {
        ControlFlow::Break(())
    }
}

fn not_built<T: std::fmt::Debug>(result: Result<T, PhaseError>) -> Operation {
    match result {
        Err(PhaseError::NotBuilt { operation }) => operation,
        other => panic!("expected not built, got {other:?}"),
    }
}

fn id() -> WorkspaceId {
    "0b9f3c8e".parse().unwrap()
}

fn plan() -> Plan {
    Plan::new("reading-list".parse().unwrap(), id(), Vec::new()).unwrap()
}

#[test]
fn a_new_phase_keeps_its_state_directory_and_the_default_waits() {
    let phase = Phase::new(PathBuf::from("/state/phase"));

    assert_eq!(phase.state_dir(), Path::new("/state/phase"));
    assert_eq!(phase.lock_wait(), DEFAULT_LOCK_WAIT);
    assert_eq!(phase.start_grace(), DEFAULT_START_GRACE);
    assert_eq!(phase.kill_wait(), DEFAULT_KILL_WAIT);
}

#[test]
fn a_workspaces_run_record_is_the_path_that_the_run_store_keeps_it_at() {
    let phase = Phase::new(PathBuf::from("/state/phase"));

    assert_eq!(
        phase.run_record(&id()),
        Path::new("/state/phase/runs/0b9f3c8e.json")
    );
    assert_eq!(
        phase.run_record(&id()),
        RunStore::new(phase.state_dir()).path(&id())
    );
}

#[test]
fn a_phase_takes_other_waits_from_its_builder() {
    let phase = Phase::new(PathBuf::from("/state/phase"))
        .with_lock_wait(Duration::from_millis(1))
        .with_start_grace(Duration::from_millis(2))
        .with_kill_wait(Duration::from_millis(3));

    assert_eq!(phase.lock_wait(), Duration::from_millis(1));
    assert_eq!(phase.start_grace(), Duration::from_millis(2));
    assert_eq!(phase.kill_wait(), Duration::from_millis(3));
}

#[test]
fn exec_reports_that_it_is_not_built_yet() {
    let phase = Phase::new(PathBuf::from("/state/phase"));
    let soon = Instant::now() + Duration::from_secs(1);
    let cancel = Cancel::new();
    let api: AppName = "api".parse().unwrap();

    let exec = phase.exec(&plan(), &api, &["true".into()], &Unused, soon, &cancel);
    assert_eq!(not_built(exec), Operation::Exec);
}

#[test]
fn a_workspace_without_a_record_is_stopped_and_has_no_log() {
    let state = tempfile::tempdir().unwrap();
    let phase = Phase::new(state.path().to_path_buf());

    let status = phase.status(&id(), None).unwrap();
    assert_eq!(status.state, crate::WorkspaceState::Stopped);
    assert!(status.rows.is_empty());
    let request = LogRequest::new("api".parse().unwrap(), "server".parse().unwrap());
    assert!(matches!(
        phase.logs(&id(), &request, &mut Unused),
        Err(PhaseError::UnknownTarget { .. })
    ));
}

#[test]
fn up_and_down_of_an_empty_plan_start_and_stop_nothing() {
    let state = tempfile::tempdir().unwrap();
    let phase = Phase::new(state.path().to_path_buf());
    let soon = Instant::now() + Duration::from_secs(5);

    let up = phase
        .up(
            &plan(),
            &Selection::All,
            &Unused,
            UpDeadlines::new(soon, soon),
            &Cancel::new(),
        )
        .unwrap();
    let down = phase.down(&id(), &Selection::All, soon).unwrap();

    assert!(up.apps.is_empty());
    assert!(down.apps.is_empty());
}

/// The full name of the keeper tests' test that runs a keeper when this
/// test binary is started as one.
const KEEPER_ENTRY: &str =
    "engine::keeper::tests::the_test_binary_runs_a_keeper_when_it_is_started_as_one";

/// An environment source that gives each process `GREETING`.
struct Greeting;

impl EnvironmentSource for Greeting {
    fn resolve(
        &self,
        _app: &AppName,
        _process: &ProcessName,
        _deadline: Instant,
    ) -> Result<Box<dyn AppliedEnvironment>, EnvironmentError> {
        Ok(Box::new(GreetingValue))
    }
}

/// The environment that holds `GREETING`.
struct GreetingValue;

impl AppliedEnvironment for GreetingValue {
    fn names(&self) -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::from(["GREETING".to_owned()])
    }

    fn expires_at(&self) -> Option<std::time::SystemTime> {
        None
    }

    fn digest(&self, key: &[u8; 32]) -> [u8; 32] {
        *key
    }

    fn apply_as(
        &self,
        command: &mut std::process::Command,
        reference: &dyn Fn(&str) -> Option<String>,
    ) {
        if let Some(name) = reference("GREETING") {
            command.env(name, "hello");
        }
    }
}

/// A phase whose keeper is this test binary, and the plan of one app,
/// `api`, whose process `server` writes `ready hello` and then sleeps.
fn real_keeper(state: &std::path::Path, checkout: &std::path::Path) -> (Phase, Plan) {
    use crate::plan::probe::{Probe, ProbeCheck};
    use crate::plan::{AppPlan, ProcessPlan, RunnerKind};
    use crate::source::Source;

    let phase = Phase::new(state.to_path_buf())
        .with_keeper(
            std::env::current_exe().unwrap(),
            ["--exact", "--nocapture", "--", KEEPER_ENTRY, "keep"]
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect(),
        )
        .with_inherited(std::collections::BTreeMap::from([
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("HOME".to_owned(), state.display().to_string()),
        ]))
        .with_kill_wait(Duration::from_secs(2));
    let mut api = AppPlan::new(
        "api".parse().unwrap(),
        RunnerKind::Host,
        Source::checkout(checkout.to_path_buf(), None),
    );
    let mut server = ProcessPlan::new(
        "server".parse().unwrap(),
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo \"ready $GREETING\"; exec sleep 30".into(),
        ],
    );
    server.env.insert("GREETING".to_owned());
    server.ready = Some(Probe::new(ProbeCheck::log("^ready hello$".to_owned())));
    api.processes.push(server);
    let plan = Plan::new("reading-list".parse().unwrap(), id(), vec![api]).unwrap();
    (phase, plan)
}

#[test]
fn up_starts_a_host_process_under_a_real_keeper_and_down_stops_it() {
    use crate::engine::host::HostRunner as _;
    use crate::record::process::{Identity, System};
    use crate::report::AppOutcome;

    let state = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let (phase, plan) = real_keeper(state.path(), checkout.path());
    let soon = Instant::now() + Duration::from_secs(30);

    let up = phase
        .up(
            &plan,
            &Selection::All,
            &Greeting,
            UpDeadlines::new(soon, soon),
            &Cancel::new(),
        )
        .unwrap();

    assert_eq!(up.apps[0].outcome, AppOutcome::Started, "{up:?}");
    let record = RunStore::new(state.path()).read(&id()).unwrap().unwrap();
    let app = record.processes[0].host.clone().unwrap();
    assert_eq!(System.identity(&app).unwrap(), Identity::Ours);

    let down = phase.down(&id(), &Selection::All, soon).unwrap();

    assert_eq!(down.apps[0].outcome, AppOutcome::Stopped, "{down:?}");
    assert_eq!(RunStore::new(state.path()).read(&id()).unwrap(), None);
    assert!(matches!(
        System.identity(&app).unwrap(),
        Identity::Gone | Identity::Reused
    ));
}

#[test]
fn down_of_a_real_keeper_does_not_wait_for_the_keepers_record_writes() {
    use crate::report::AppOutcome;

    let state = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let (phase, plan) = real_keeper(state.path(), checkout.path());
    let soon = Instant::now() + Duration::from_secs(30);
    phase
        .up(
            &plan,
            &Selection::All,
            &Greeting,
            UpDeadlines::new(soon, soon),
            &Cancel::new(),
        )
        .unwrap();

    let down = phase.down(&id(), &Selection::All, soon).unwrap();

    assert_eq!(down.apps[0].outcome, AppOutcome::Stopped, "{down:?}");
    // The keeper asked for the lock once while `down` held it, and dropped
    // its writes instead of waiting.
    let diagnostics = std::fs::read_to_string(
        state
            .path()
            .join("logs")
            .join(id().as_str())
            .join("api.server.keeper.log"),
    )
    .unwrap();
    assert!(
        diagnostics.contains("another operation holds the lifecycle lock"),
        "{diagnostics}"
    );
}

#[test]
fn a_not_built_error_names_the_operation() {
    let error = PhaseError::NotBuilt {
        operation: Operation::Sweep,
    };
    assert_eq!(
        error.to_string(),
        "phase cannot sweep yet, because it is not built"
    );
}

#[test]
fn sweeping_a_workspace_with_no_run_record_finds_nothing() {
    let state = tempfile::tempdir().unwrap();
    let phase = Phase::new(state.path().to_path_buf());

    let swept = phase.sweep(&id(), false).unwrap();

    assert!(swept.items.is_empty());
    assert!(swept.objects.is_empty());
}
