//! `up` of a host process that runs while its part of the plan has
//! changed, with the fake host and container runners of the run tests.

use super::{Env, World, container_app, host_app, name, notes, outcome, plan};
use crate::cancel::Cancel;
use crate::plan::{AppPlan, Permissions};
use crate::report::{AppOutcome, Note};
use crate::select::Selection;

/// Returns the pid of the app process that the fake host last launched for
/// `app/process`.
fn pid(world: &World, target: &str) -> u32 {
    world.host.state.borrow().pids[target].1
}

/// Returns `app` with permissions that declare nothing, so its processes
/// run under the sandbox where the machine has one.
fn with_permissions(mut app: AppPlan) -> AppPlan {
    app.permissions = Some(Permissions::default());
    app
}

#[test]
fn up_restarts_a_running_process_whose_app_gained_permissions_and_its_pid_changes() {
    let world = World::outside_temp();
    world
        .up(
            &plan(vec![host_app("api", None, &[])]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();
    let before = pid(&world, "api/server");

    let changed = plan(vec![with_permissions(host_app("api", None, &[]))]);
    let report = world
        .up(&changed, &Selection::All, &Env::default())
        .unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::Started, "{report:?}");
    assert_eq!(
        notes(&report, "api"),
        &[Note::plan_changed(&name("api"), name("server"))]
    );
    assert_eq!(world.host.stops(), vec!["api/server"]);
    assert_eq!(world.host.launches(), vec!["api/server", "api/server"]);
    assert_ne!(pid(&world, "api/server"), before);
    // The new keeper and its app run, and the old ones are gone.
    assert_eq!(world.host.running(), 2);
}

#[test]
fn up_leaves_a_running_process_whose_part_of_the_plan_is_unchanged_alone() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    let before = pid(&world, "api/server");

    let report = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::AlreadyRunning);
    assert!(notes(&report, "api").is_empty());
    assert!(world.host.stops().is_empty());
    assert_eq!(pid(&world, "api/server"), before);
}

#[test]
fn up_restarts_once_a_running_process_whose_entry_records_no_part_of_the_plan() {
    let world = World::new();
    let plan = plan(vec![host_app("api", None, &[])]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    // A record that an older phase wrote holds no part hash.
    world.edit(|record| {
        for entry in &mut record.processes {
            entry.part_hash = None;
        }
    });
    let before = pid(&world, "api/server");

    let first = world.up(&plan, &Selection::All, &Env::default()).unwrap();
    let restarted = pid(&world, "api/server");
    let second = world.up(&plan, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&first, "api"), &AppOutcome::Started, "{first:?}");
    assert_eq!(
        notes(&first, "api"),
        &[Note::plan_unrecorded(&name("api"), name("server"))]
    );
    assert_ne!(restarted, before);
    assert_eq!(outcome(&second, "api"), &AppOutcome::AlreadyRunning);
    assert_eq!(pid(&world, "api/server"), restarted);
    assert_eq!(world.host.launches().len(), 2);
}

#[test]
fn up_restarts_a_running_process_whose_command_changed() {
    let world = World::new();
    world
        .up(
            &plan(vec![host_app("api", None, &[])]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();
    let before = pid(&world, "api/server");
    let mut api = host_app("api", None, &[]);
    api.processes[0].run = vec!["./serve".into(), "--verbose".into()];

    let report = world
        .up(&plan(vec![api]), &Selection::All, &Env::default())
        .unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::Started, "{report:?}");
    assert_eq!(world.host.stops(), vec!["api/server"]);
    assert_ne!(pid(&world, "api/server"), before);
    let record = world.record().unwrap();
    let (_, entry) = record.host_pair(&name("api"), &name("server"));
    assert_eq!(
        entry.unwrap().host.as_ref().map(|host| host.pid),
        Some(pid(&world, "api/server"))
    );
}

#[test]
fn a_dependent_of_a_restarted_process_keeps_running_as_a_dependent_of_a_replaced_container_does() {
    let world = World::new();
    let containers = plan(vec![
        container_app("store"),
        host_app("web", None, &["store"]),
    ]);
    world
        .up(&containers, &Selection::All, &Env::default())
        .unwrap();
    // The fake runner replaces every container that it does not report
    // as running, as the real one replaces a container whose plan hash
    // changed.
    let replaced = world
        .up(&containers, &Selection::All, &Env::default())
        .unwrap();
    assert_eq!(outcome(&replaced, "store"), &AppOutcome::Started);
    assert_eq!(outcome(&replaced, "web"), &AppOutcome::AlreadyRunning);

    let hosts = plan(vec![
        host_app("api", None, &["db"]),
        host_app("db", None, &[]),
    ]);
    world.up(&hosts, &Selection::All, &Env::default()).unwrap();
    let api = pid(&world, "api/server");
    let mut db = host_app("db", None, &[]);
    db.processes[0].run = vec!["./serve".into(), "--verbose".into()];
    let changed = plan(vec![host_app("api", None, &["db"]), db]);

    let report = world
        .up(&changed, &Selection::All, &Env::default())
        .unwrap();

    assert_eq!(outcome(&report, "db"), &AppOutcome::Started, "{report:?}");
    assert_eq!(outcome(&report, "api"), &AppOutcome::AlreadyRunning);
    assert_eq!(pid(&world, "api/server"), api);
    assert!(!world.host.stops().contains(&"api/server".to_owned()));
}

#[test]
fn an_environment_that_cannot_be_resolved_leaves_a_changed_process_running() {
    let world = World::new();
    world
        .up(
            &plan(vec![host_app("api", None, &[])]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();
    let before = pid(&world, "api/server");
    let env = Env {
        failing: ["api".to_owned()].into(),
        ..Env::default()
    };

    let error = world
        .engine()
        .up(
            &plan(vec![with_permissions(host_app("api", None, &[]))]),
            &Selection::All,
            &env,
            super::deadlines(),
            &Cancel::new(),
        )
        .unwrap_err();

    assert!(
        matches!(error, crate::error::PhaseError::Environment { .. }),
        "{error:?}"
    );
    assert!(world.host.stops().is_empty());
    assert_eq!(pid(&world, "api/server"), before);
    assert_eq!(world.host.running(), 2);
}

#[test]
fn a_changed_process_that_survives_its_stop_is_left_and_nothing_starts_in_its_place() {
    let world = World::new();
    world
        .up(
            &plan(vec![host_app("api", None, &[])]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();
    world
        .host
        .state
        .borrow_mut()
        .unstoppable
        .insert("api/server".to_owned());

    let report = world
        .up(
            &plan(vec![with_permissions(host_app("api", None, &[]))]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();

    assert_eq!(outcome(&report, "api"), &AppOutcome::Left, "{report:?}");
    assert_eq!(world.host.launches(), vec!["api/server"]);
}
