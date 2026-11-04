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
fn every_operation_but_sweep_reports_that_it_is_not_built_yet() {
    let phase = Phase::new(PathBuf::from("/state/phase"));
    let soon = Instant::now() + Duration::from_secs(1);
    let cancel = Cancel::new();
    let api: AppName = "api".parse().unwrap();

    let up = phase.up(
        &plan(),
        &Selection::All,
        &Unused,
        UpDeadlines::new(soon, soon),
        &cancel,
    );
    assert_eq!(not_built(up), Operation::Up);
    let restart = phase.restart(&plan(), &Selection::All, &Unused, soon, &cancel);
    assert_eq!(not_built(restart), Operation::Restart);
    assert_eq!(
        not_built(phase.down(&id(), &Selection::All, soon)),
        Operation::Down
    );
    assert_eq!(not_built(phase.status(&id())), Operation::Status);
    let request = LogRequest::new(api.clone(), "server".parse().unwrap());
    assert_eq!(
        not_built(phase.logs(&id(), &request, &mut Unused)),
        Operation::Logs
    );
    let exec = phase.exec(&plan(), &api, &["true".into()], &Unused, soon, &cancel);
    assert_eq!(not_built(exec), Operation::Exec);
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
