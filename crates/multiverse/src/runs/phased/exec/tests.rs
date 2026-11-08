use phase::{AppName, ProcessName, ProcessState as Phase, RunnerKind, StatusRow};

use super::running;

fn row(state: Phase) -> StatusRow {
    StatusRow::new(
        AppName::try_from("api".to_owned()).unwrap(),
        ProcessName::try_from("main".to_owned()).unwrap(),
        RunnerKind::Host,
        None,
        state,
    )
}

#[test]
fn a_command_can_run_in_the_context_of_a_unit_that_is_starting_running_ready_or_paused() {
    assert!(running(&row(Phase::Starting)));
    assert!(running(&row(Phase::Running)));
    assert!(running(&row(Phase::Ready)));
    assert!(running(&row(Phase::Paused)));
}

#[test]
fn no_command_runs_in_the_context_of_a_unit_that_is_stopped_stopping_completed_or_restarting() {
    assert!(!running(&row(Phase::Stopped)));
    assert!(!running(&row(Phase::Stopping)));
    assert!(!running(&row(Phase::Completed)));
    assert!(!running(&row(Phase::Restarting)));
}
