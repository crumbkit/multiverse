//! Oneshot processes under real keepers that this test binary runs.

use std::time::{Duration, Instant};

use super::{Greeting, id, real_keeper_running};
use crate::cancel::Cancel;
use crate::phase::{Phase, UpDeadlines};
use crate::plan::{Plan, ProcessKind, ProcessPlan};
use crate::record::store::RunStore;
use crate::report::{AppOutcome, StartProblem};
use crate::select::Selection;
use crate::status::ProcessState;

/// Returns the phase and plan of [`real_keeper_running`] whose app `api`
/// first runs the oneshot `migrate`, which runs `script` in `sh`, and then
/// its server/// the file `migrated` that the oneshot writes.
fn migrating(state: &std::path::Path, checkout: &std::path::Path, script: &str) -> (Phase, Plan) {
    let (phase, plan) = real_keeper_running(
        state,
        checkout,
        "test -e migrated && echo \"ready $GREETING\"; exec sleep 30",
        Some("^ready hello$"),
        &[],
    );
    let mut apps = plan.apps().to_vec();
    let mut migrate = ProcessPlan::new(
        "migrate".parse().unwrap(),
        vec!["/bin/sh".into(), "-c".into(), script.into()],
    );
    migrate.kind = ProcessKind::oneshot(true);
    migrate.env.insert("GREETING".to_owned());
    apps[0].processes[0]
        .depends
        .push("migrate".parse().unwrap());
    apps[0].processes.insert(0, migrate);
    let plan = Plan::new(plan.workspace().clone(), id(), apps).unwrap();
    (phase, plan)
}

fn up(phase: &Phase, plan: &Plan) -> crate::report::Report {
    let soon = Instant::now() + Duration::from_secs(30);
    phase
        .up(
            plan,
            &Selection::All,
            &Greeting,
            UpDeadlines::new(soon, soon),
            &Cancel::new(),
        )
        .unwrap()
}

#[test]
fn a_real_oneshot_completes_before_its_dependent_starts_and_shows_as_completed() {
    let state = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let (phase, plan) = migrating(
        state.path(),
        checkout.path(),
        "sleep 1; echo \"migrated $GREETING\" > migrated; echo done",
    );

    let report = up(&phase, &plan);

    assert_eq!(report.apps[0].outcome, AppOutcome::Started, "{report:?}");
    let status = phase.status(&id(), Some(&plan)).unwrap();
    assert_eq!(status.rows[0].state, ProcessState::Completed, "{status:?}");
    assert_eq!(status.rows[1].state, ProcessState::Ready, "{status:?}");
    assert_eq!(status.state, crate::WorkspaceState::Running);
    let log = std::fs::read_to_string(status.rows[0].log.as_ref().unwrap()).unwrap();
    assert!(log.contains("done"), "{log}");

    // A second `up` leaves the completed oneshot alone.
    let again = up(&phase, &plan);
    assert_eq!(
        again.apps[0].outcome,
        AppOutcome::AlreadyRunning,
        "{again:?}"
    );

    phase
        .down(
            &id(),
            &Selection::All,
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();
    assert_eq!(RunStore::new(state.path()).read(&id()).unwrap(), None);
}

#[test]
fn a_real_oneshot_that_fails_reports_its_code_and_nothing_it_wrote_and_its_dependent_never_starts()
{
    let state = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let (phase, plan) = migrating(
        state.path(),
        checkout.path(),
        "echo 'could not connect: password=hunter2'; exit 3",
    );

    let report = up(&phase, &plan);

    let problem = StartProblem::exited(Some(3), None);
    assert_eq!(
        report.apps[0].outcome,
        AppOutcome::failed("migrate".parse().unwrap(), problem.clone())
    );
    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("hunter2"), "{json}");
    assert!(!problem.to_string().contains("hunter2"));
    let status = phase.status(&id(), Some(&plan)).unwrap();
    let log = std::fs::read_to_string(status.rows[0].log.as_ref().unwrap()).unwrap();
    assert!(log.contains("password=hunter2"), "{log}");
    assert_eq!(status.rows[0].state, ProcessState::exited(Some(3)));
    assert_eq!(status.rows[1].state, ProcessState::Stopped, "{status:?}");
    assert!(!checkout.path().join("migrated").exists());
    phase
        .down(
            &id(),
            &Selection::All,
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();
}

#[test]
fn a_real_oneshot_that_up_stopped_waiting_for_completes_once_its_keeper_records_the_exit() {
    let state = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let (phase, plan) = migrating(
        state.path(),
        checkout.path(),
        "sleep 1; echo run >> runs; echo \"migrated $GREETING\" > migrated",
    );
    let short = Instant::now() + Duration::from_millis(300);

    let report = phase
        .up(
            &plan,
            &Selection::All,
            &Greeting,
            UpDeadlines::new(short, short),
            &Cancel::new(),
        )
        .unwrap();

    assert_eq!(
        report.apps[0].outcome,
        AppOutcome::StartUncertain,
        "{report:?}"
    );
    // The keeper records the exit once `up` has released the lifecycle
    // lock.
    let waited = Instant::now() + Duration::from_secs(20);
    loop {
        let status = phase.status(&id(), Some(&plan)).unwrap();
        if status.rows[0].state == ProcessState::Completed {
            break;
        }
        assert!(Instant::now() < waited, "{status:?}");
        crate::clock::pause(crate::clock::POLL_INTERVAL);
    }

    let again = up(&phase, &plan);

    assert_eq!(again.apps[0].outcome, AppOutcome::Started, "{again:?}");
    assert_eq!(
        std::fs::read_to_string(checkout.path().join("runs")).unwrap(),
        "run\n"
    );
    phase
        .down(
            &id(),
            &Selection::All,
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();
}

/// A sandboxed oneshot runs under the sandbox: it may write its own state
/// directory, and a write to the workspace outside its checkout fails.
#[cfg(target_os = "macos")]
#[test]
fn a_real_oneshot_of_a_sandboxed_app_runs_under_the_sandbox() {
    use crate::plan::Permissions;
    use crate::sandbox::{Platform, platform};

    let Platform::Available(sandboxing) = platform() else {
        return;
    };
    // The per-user temporary directory is protected, so the test's files
    // lie in the build's target directory.
    let target = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("sandbox-fixtures");
    std::fs::create_dir_all(&target).unwrap();
    let root = tempfile::Builder::new()
        .prefix("phase-")
        .tempdir_in(target)
        .unwrap();
    let root = root.path().canonicalize().unwrap();
    let state = root.join("state");
    let workspace = root.join("workspace");
    let checkout = workspace.join("api");
    std::fs::create_dir_all(&checkout).unwrap();
    // The checkout is read-only under the sandbox, so the server's file is
    // there already.
    crate::record::testing::write_file(&checkout.join("migrated"), "migrated\n");
    let outside = workspace.join("outside");
    let (phase, plan) = migrating(
        &state,
        &checkout,
        &format!(
            "echo state > \"$TMPDIR/../state/file\" && if echo no > {}; then exit 9; fi",
            outside.display()
        ),
    );
    let mut apps = plan.apps().to_vec();
    apps[0].permissions = Some(Permissions::default());
    let plan = Plan::new(plan.workspace().clone(), id(), apps)
        .unwrap()
        .with_workspace_dir(workspace.clone(), Vec::new());

    let report = up(&phase, &plan);

    assert_eq!(report.apps[0].outcome, AppOutcome::Started, "{report:?}");
    assert!(!outside.exists());
    let record = RunStore::new(&state).read(&id()).unwrap().unwrap();
    let migrate = record
        .processes
        .iter()
        .find(|entry| entry.process.as_str() == "migrate")
        .unwrap();
    assert_eq!(migrate.sandbox, Some(sandboxing));
    let dirs = crate::sandbox::AppDirs::of(&state, &id(), &"api".parse().unwrap());
    assert_eq!(
        std::fs::read_to_string(dirs.state.join("file")).unwrap(),
        "state\n"
    );
    phase
        .down(
            &id(),
            &Selection::All,
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();
}
