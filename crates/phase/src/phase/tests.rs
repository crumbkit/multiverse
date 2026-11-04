use std::ops::ControlFlow;
use std::path::Path;
use std::time::{Duration, Instant};

use super::{Phase, UpDeadlines};
use crate::AppliedEnvironment;
use crate::env::{EnvironmentError, EnvironmentSource};
use crate::error::{Operation, PhaseError};
use crate::logs::{LineSink, LogLine, LogRequest};
use crate::name::AppName;
use crate::plan::Plan;
use crate::select::Selection;

struct Unused;

impl EnvironmentSource for Unused {
    fn resolve(
        &self,
        app: &AppName,
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

fn plan() -> Plan {
    Plan::new(
        "reading-list".parse().unwrap(),
        "0b9f3c8e".parse().unwrap(),
        Vec::new(),
    )
    .unwrap()
}

#[test]
fn every_operation_reports_that_it_is_not_built_yet() {
    let phase = Phase::new();
    let workspace = Path::new("/workspaces/reading-list");
    let soon = Instant::now() + Duration::from_secs(1);
    let deadlines = UpDeadlines {
        build: soon,
        ready: soon,
    };
    let api: AppName = "api".parse().unwrap();

    assert_eq!(
        not_built(phase.up(&plan(), &Selection::All, &Unused, deadlines)),
        Operation::Up
    );
    assert_eq!(
        not_built(phase.restart(&plan(), &Selection::All, &Unused, soon)),
        Operation::Restart
    );
    assert_eq!(
        not_built(phase.down(workspace, &Selection::All, soon)),
        Operation::Down
    );
    assert_eq!(not_built(phase.status(workspace)), Operation::Status);
    let request = LogRequest::new(api.clone(), "server".parse().unwrap());
    assert_eq!(
        not_built(phase.logs(workspace, &request, &mut Unused)),
        Operation::Logs
    );
    assert_eq!(not_built(phase.sweep(workspace, true)), Operation::Sweep);
    assert_eq!(
        not_built(phase.exec(&plan(), &api, &["true".into()], &Unused, soon)),
        Operation::Exec
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
