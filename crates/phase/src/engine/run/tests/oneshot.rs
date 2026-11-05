//! Oneshot processes under `up`, `restart` and `down`, with the fake host
//! and container runners of the run tests.

use std::time::Duration;

use super::{Env, World, host_app, name, outcome, plan, soon};
use crate::cancel::Cancel;
use crate::plan::{AppPlan, ProcessKind, ProcessPlan, RunnerKind};
use crate::record::run::{Ended, EntryState, Exit, ObjectKey, RunRecord};
use crate::report::{AppOutcome, StartProblem};
use crate::select::Selection;
use crate::source::Source;

/// Returns a host app named `app` whose oneshot `migrate` runs before its
/// long-running `server`, which depends on it.
fn migrating_app(app: &str, once: bool, depends: &[&str]) -> AppPlan {
    let mut plan = host_app(app, None, depends);
    let mut migrate = ProcessPlan::new(name("migrate"), vec!["./migrate".into()]);
    migrate.kind = ProcessKind::oneshot(once);
    migrate.env.insert("API_TOKEN".to_owned());
    plan.processes[0].depends.push(name("migrate"));
    plan.processes.insert(0, migrate);
    plan
}

/// Returns a host app named `app` with the single oneshot `provision`,
/// which depends on the apps `depends`.
fn provisioning_app(app: &str, once: bool, depends: &[&str]) -> AppPlan {
    let mut plan = AppPlan::new(
        name(app),
        RunnerKind::Host,
        Source::checkout(
            std::path::PathBuf::from("/workspaces/reading-list").join(app),
            None,
        ),
    );
    let mut provision = ProcessPlan::new(name("provision"), vec!["./provision".into()]);
    provision.kind = ProcessKind::oneshot(once);
    plan.processes.push(provision);
    plan.depends = depends.iter().map(|depend| name(depend)).collect();
    plan
}

/// Returns the entry of `process` of `app` in `record`.
fn entry<'r>(record: &'r RunRecord, app: &str, process: &str) -> &'r crate::record::run::HostEntry {
    record.host_pair(&name(app), &name(process)).1.unwrap()
}

#[test]
fn a_oneshot_that_exits_zero_completes_before_the_process_that_depends_on_it_starts() {
    let world = World::new();
    world.host.ends_as("api/migrate", Exit::Code(0));
    let plan = plan(vec![migrating_app("api", false, &[])]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::Started, "{report:?}");
    assert_eq!(world.host.launches(), vec!["api/migrate", "api/server"]);
    let record = world.record().unwrap();
    let migrate = entry(&record, "api", "migrate");
    assert_eq!(migrate.state, EntryState::Stopped);
    assert_eq!(migrate.ended, Some(Ended::by_itself(Some(Exit::Code(0)))));
    assert!(migrate.completed.is_some());
    let keeper = record.state(&ObjectKey::Keeper(name("api"), name("migrate")));
    assert_eq!(keeper, Some(EntryState::Stopped));
    // Only the server and its keeper still run.
    assert_eq!(world.host.running(), 2);
}

#[test]
fn an_app_that_depends_on_an_app_with_a_oneshot_starts_after_the_oneshot_completes() {
    let world = World::new();
    world.host.ends_as("tables/provision", Exit::Code(0));
    let plan = plan(vec![
        host_app("api", None, &["tables"]),
        provisioning_app("tables", false, &[]),
    ]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "tables"), &AppOutcome::Started);
    assert_eq!(outcome(&report, "api"), &AppOutcome::Started);
    assert_eq!(
        world.host.launches(),
        vec!["tables/provision", "api/server"]
    );
}

#[test]
fn a_oneshot_that_exits_non_zero_fails_its_app_with_its_code_and_log_tail_and_stops_dependents() {
    let world = World::new();
    world.host.ends_as("tables/provision", Exit::Code(4));
    world.host.state.borrow_mut().logs.insert(
        "tables.provision.log".to_owned(),
        vec![
            "creating table orders".to_owned(),
            "table exists".to_owned(),
        ],
    );
    let plan = plan(vec![
        host_app("api", None, &["tables"]),
        provisioning_app("tables", true, &[]),
    ]);

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(
        outcome(&report, "tables"),
        &AppOutcome::failed(
            name("provision"),
            StartProblem::exited(
                Some(4),
                None,
                vec![
                    "creating table orders".to_owned(),
                    "table exists".to_owned()
                ]
            )
        )
    );
    assert_eq!(outcome(&report, "api"), &AppOutcome::NotStarted);
    assert_eq!(world.host.launches(), vec!["tables/provision"]);
    let record = world.record().unwrap();
    let provision = entry(&record, "tables", "provision");
    assert_eq!(provision.completed, None);
    assert_eq!(provision.ended, Some(Ended::by_itself(Some(Exit::Code(4)))));
}

#[test]
fn a_oneshot_that_does_not_exit_within_its_ready_timeout_fails_as_not_ready() {
    let world = World::new();
    let mut tables = provisioning_app("tables", false, &[]);
    tables.processes[0].ready_timeout = Duration::from_millis(60);

    let report = world
        .up(&plan(vec![tables]), &Selection::All, &Env::default())
        .unwrap();

    assert!(
        matches!(
            outcome(&report, "tables"),
            AppOutcome::Failed {
                problem: StartProblem::NotReady { .. },
                ..
            }
        ),
        "{report:?}"
    );
    assert_eq!(world.host.running(), 0);
}

#[test]
fn a_cancel_while_a_oneshot_runs_stops_it_and_starts_nothing_after_it() {
    let world = World::new();
    let cancel = Cancel::new();
    world.host.cancels_at_launch("api/migrate", &cancel);
    let plan = plan(vec![migrating_app("api", false, &[])]);

    let report = world
        .engine()
        .up(
            &plan,
            &Selection::All,
            &Env::default(),
            super::deadlines(),
            &cancel,
        )
        .unwrap();

    assert_eq!(world.host.launches(), vec!["api/migrate"]);
    assert_eq!(outcome(&report, "api"), &AppOutcome::Stopped);
    assert_eq!(world.host.running(), 0);
}

#[test]
fn a_once_oneshot_is_left_alone_by_a_later_up_and_runs_again_when_its_plan_changes() {
    let world = World::new();
    world.host.ends_as("tables/provision", Exit::Code(0));
    let first = plan(vec![provisioning_app("tables", true, &[])]);
    world.up(&first, &Selection::All, &Env::default()).unwrap();

    let again = world.up(&first, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&again, "tables"), &AppOutcome::AlreadyRunning);
    assert_eq!(world.host.launches(), vec!["tables/provision"]);

    let mut changed = provisioning_app("tables", true, &[]);
    changed.processes[0].run.push("--with-indexes".into());
    let report = world
        .up(&plan(vec![changed]), &Selection::All, &Env::default())
        .unwrap();

    assert_eq!(outcome(&report, "tables"), &AppOutcome::Started);
    assert_eq!(
        world.host.launches(),
        vec!["tables/provision", "tables/provision"]
    );
}

#[test]
fn a_once_oneshot_is_left_alone_even_when_its_app_starts_again() {
    let world = World::new();
    world.host.ends_as("api/migrate", Exit::Code(0));
    let plan = plan(vec![migrating_app("api", true, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world.down(&super::only("api/server"));

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::Started);
    assert_eq!(
        world.host.launches(),
        vec!["api/migrate", "api/server", "api/server"]
    );
}

#[test]
fn an_up_that_finds_everything_running_and_unchanged_leaves_a_oneshot_without_once_alone() {
    let world = World::new();
    world.host.ends_as("api/migrate", Exit::Code(0));
    let plan = plan(vec![migrating_app("api", false, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::AlreadyRunning);
    assert_eq!(world.host.launches(), vec!["api/migrate", "api/server"]);
}

#[test]
fn an_up_that_starts_the_app_again_runs_its_oneshot_without_once_again_first() {
    let world = World::new();
    world.host.ends_as("api/migrate", Exit::Code(0));
    let plan = plan(vec![migrating_app("api", false, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world.down(&super::only("api/server"));

    world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(
        world.host.launches(),
        vec!["api/migrate", "api/server", "api/migrate", "api/server"]
    );
}

#[test]
fn an_up_that_starts_an_app_it_depends_on_runs_a_oneshot_without_once_again() {
    let world = World::new();
    world.host.ends_as("tables/provision", Exit::Code(0));
    let plan = plan(vec![
        host_app("db", None, &[]),
        provisioning_app("tables", false, &["db"]),
    ]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    let unchanged = world.up(&plan, &Selection::All, &Env::default()).unwrap();
    assert_eq!(outcome(&unchanged, "tables"), &AppOutcome::AlreadyRunning);
    world.down(&super::only("db"));

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "tables"), &AppOutcome::Started);
    assert_eq!(
        world.host.launches(),
        vec![
            "db/server",
            "tables/provision",
            "db/server",
            "tables/provision"
        ]
    );
}

#[test]
fn down_forgets_a_oneshots_completion_so_the_next_up_runs_it_again() {
    let world = World::new();
    world.host.ends_as("tables/provision", Exit::Code(0));
    let plan = plan(vec![
        host_app("db", None, &[]),
        provisioning_app("tables", true, &["db"]),
    ]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();

    let down = world.down(&super::only("tables"));

    assert_eq!(outcome(&down, "tables"), &AppOutcome::Stopped);
    let record = world.record().unwrap();
    let provision = entry(&record, "tables", "provision");
    assert_eq!(provision.completed, None);
    assert_eq!(provision.ended, None);

    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    assert_eq!(
        world.host.launches(),
        vec!["db/server", "tables/provision", "tables/provision"]
    );

    world.down(&Selection::All);
    assert_eq!(world.record(), None);
}

#[test]
fn restart_runs_a_completed_oneshot_again_before_the_process_that_depends_on_it() {
    let world = World::new();
    world.host.ends_as("api/migrate", Exit::Code(0));
    let plan = plan(vec![migrating_app("api", true, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();

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

    assert_eq!(
        outcome(&report, "api"),
        &AppOutcome::Restarted,
        "{report:?}"
    );
    assert_eq!(
        world.host.launches(),
        vec!["api/migrate", "api/server", "api/migrate", "api/server"]
    );
    let record = world.record().unwrap();
    assert!(entry(&record, "api", "migrate").completed.is_some());
}

#[test]
fn restart_of_an_app_with_only_a_completed_oneshot_runs_it_again() {
    let world = World::new();
    world.host.ends_as("tables/provision", Exit::Code(0));
    let plan = plan(vec![provisioning_app("tables", true, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();

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

    assert_eq!(outcome(&report, "tables"), &AppOutcome::Restarted);
    assert_eq!(
        world.host.launches(),
        vec!["tables/provision", "tables/provision"]
    );
}

#[test]
fn a_oneshot_starts_with_no_restart_policy_and_no_start_grace() {
    let world = World::new();
    world.host.ends_as("tables/provision", Exit::Code(0));

    world
        .up(
            &plan(vec![provisioning_app("tables", false, &[])]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();

    let spec: serde_json::Value =
        serde_json::from_str(&world.host.state.borrow().specs[0]).unwrap();
    assert_eq!(
        spec["restart"],
        serde_json::to_value(crate::plan::restart::Restart::No).unwrap()
    );
    assert!(
        spec.get("ready_after_ms")
            .is_none_or(serde_json::Value::is_null),
        "{spec}"
    );
}
