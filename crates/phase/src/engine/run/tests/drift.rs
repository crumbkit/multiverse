//! `up` of a host process that runs while its part of the plan has
//! changed, with the fake host and container runners of the run tests.

use super::{Env, World, container_app, host_app, name, notes, outcome, plan};
use crate::cancel::Cancel;
use crate::plan::{AppPlan, Permissions};
use crate::record::run::Exit;
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

/// Returns `app` with a sandbox that may connect to the ports of `target`.
fn connecting_to(mut app: AppPlan, target: &str) -> AppPlan {
    app.permissions = Some(Permissions {
        connect: [target.to_owned()].into(),
        ..Permissions::default()
    });
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

    assert_eq!(
        outcome(&report, "api"),
        &AppOutcome::Restarted,
        "{report:?}"
    );
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

    assert_eq!(outcome(&first, "api"), &AppOutcome::Restarted, "{first:?}");
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

    assert_eq!(
        outcome(&report, "api"),
        &AppOutcome::Restarted,
        "{report:?}"
    );
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
    assert_eq!(outcome(&replaced, "store"), &AppOutcome::Restarted);
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

    assert_eq!(outcome(&report, "db"), &AppOutcome::Restarted, "{report:?}");
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
    let world = World::outside_temp();
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

#[test]
fn up_restarts_a_sandboxed_dependent_whose_connect_names_a_dependency_that_moved_to_another_port() {
    // Without a sandbox, no profile names the dependency's port.
    if !sandbox_available() {
        return;
    }
    let world = World::outside_temp();
    let api = || connecting_to(host_app("api", None, &["db"]), "db");
    world
        .up(
            &plan(vec![api(), host_app("db", Some(5432), &[])]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();
    let before = pid(&world, "api/server");

    let moved = plan(vec![api(), host_app("db", Some(5433), &[])]);
    let report = world.up(&moved, &Selection::All, &Env::default()).unwrap();
    let again = world.up(&moved, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "db"), &AppOutcome::Restarted, "{report:?}");
    assert_eq!(
        outcome(&report, "api"),
        &AppOutcome::Restarted,
        "{report:?}"
    );
    assert_eq!(
        notes(&report, "api"),
        &[Note::plan_changed(&name("api"), name("server"))]
    );
    assert_ne!(pid(&world, "api/server"), before);
    assert_eq!(outcome(&again, "api"), &AppOutcome::AlreadyRunning);
}

#[test]
fn up_leaves_a_sandboxed_dependent_running_when_a_dependency_its_connect_leaves_out_moved_to_another_port()
 {
    if !sandbox_available() {
        return;
    }
    let world = World::outside_temp();
    let api = || with_permissions(host_app("api", None, &["db"]));
    world
        .up(
            &plan(vec![api(), host_app("db", Some(5432), &[])]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();
    let before = pid(&world, "api/server");

    let moved = plan(vec![api(), host_app("db", Some(5433), &[])]);
    let report = world.up(&moved, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "db"), &AppOutcome::Restarted, "{report:?}");
    assert_eq!(outcome(&report, "api"), &AppOutcome::AlreadyRunning);
    assert_eq!(pid(&world, "api/server"), before);
}

#[test]
fn up_leaves_an_unsandboxed_dependent_running_when_its_dependency_moved_to_another_port() {
    let world = World::new();
    world
        .up(
            &plan(vec![
                host_app("api", None, &["db"]),
                host_app("db", Some(5432), &[]),
            ]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();
    let before = pid(&world, "api/server");

    let moved = plan(vec![
        host_app("api", None, &["db"]),
        host_app("db", Some(5433), &[]),
    ]);
    let report = world.up(&moved, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "db"), &AppOutcome::Restarted, "{report:?}");
    assert_eq!(outcome(&report, "api"), &AppOutcome::AlreadyRunning);
    assert_eq!(pid(&world, "api/server"), before);
}

/// Reports whether this machine offers a sandbox. Without one, phase
/// builds no sandbox, so nothing that only the sandbox reads restarts a
/// process.
fn sandbox_available() -> bool {
    matches!(
        crate::sandbox::platform(),
        crate::sandbox::Platform::Available(_)
    )
}

#[test]
fn up_restarts_a_sandboxed_process_once_the_host_program_protects_another_path() {
    if !sandbox_available() {
        return;
    }
    let world = World::outside_temp();
    let plan = plan(vec![with_permissions(host_app("api", None, &[]))]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    let before = pid(&world, "api/server");
    let guarded = world.state.path().parent().unwrap().join(format!(
        "guarded-{}",
        world.state.path().file_name().unwrap().to_string_lossy()
    ));
    std::fs::create_dir_all(&guarded).unwrap();
    let protected = vec![guarded.clone()];
    let mut engine = world.engine();
    engine.settings.protected = &protected;

    let report = engine
        .up(
            &plan,
            &Selection::All,
            &Env::default(),
            super::deadlines(),
            &Cancel::new(),
        )
        .unwrap();
    let again = engine
        .up(
            &plan,
            &Selection::All,
            &Env::default(),
            super::deadlines(),
            &Cancel::new(),
        )
        .unwrap();
    std::fs::remove_dir(&guarded).unwrap();

    assert_eq!(
        outcome(&report, "api"),
        &AppOutcome::Restarted,
        "{report:?}"
    );
    assert_eq!(
        notes(&report, "api"),
        &[Note::plan_changed(&name("api"), name("server"))]
    );
    assert_ne!(pid(&world, "api/server"), before);
    assert_eq!(outcome(&again, "api"), &AppOutcome::AlreadyRunning);
}

#[test]
fn up_leaves_a_sandboxed_process_running_when_only_the_inherited_path_changed() {
    if !sandbox_available() {
        return;
    }
    let world = World::outside_temp();
    let plan = plan(vec![with_permissions(host_app("api", None, &[]))]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    let before = pid(&world, "api/server");
    let inherited = std::collections::BTreeMap::from([(
        "PATH".to_owned(),
        "/usr/bin:/bin:/usr/sbin:/sbin".to_owned(),
    )]);
    let mut engine = world.engine();
    engine.settings.inherited = &inherited;

    let report = engine
        .up(
            &plan,
            &Selection::All,
            &Env::default(),
            super::deadlines(),
            &Cancel::new(),
        )
        .unwrap();

    assert_eq!(
        outcome(&report, "api"),
        &AppOutcome::AlreadyRunning,
        "{report:?}"
    );
    assert_eq!(pid(&world, "api/server"), before);
}

/// Returns `app` with its first process's command changed.
fn changed(mut app: AppPlan) -> AppPlan {
    app.processes[0].run.push("--verbose".into());
    app
}

/// Returns how many times the fake host was asked to stop `target`.
fn stops_of(world: &World, target: &str) -> usize {
    world
        .host
        .stops()
        .iter()
        .filter(|stopped| stopped.as_str() == target)
        .count()
}

/// Reports whether the app process that the fake host last launched for
/// `target` still runs.
fn alive(world: &World, target: &str) -> bool {
    world
        .host
        .state
        .borrow()
        .alive
        .contains(&pid(world, target))
}

/// Starts `db` and `api`, which depends on it, and returns the plan that
/// changes both.
fn running_pair(world: &World) -> crate::plan::Plan {
    world
        .up(
            &plan(vec![
                host_app("db", None, &[]),
                host_app("api", None, &["db"]),
            ]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();
    plan(vec![
        changed(host_app("db", None, &[])),
        changed(host_app("api", None, &["db"])),
    ])
}

#[test]
fn a_failed_up_leaves_an_earlier_restart_running_and_reports_it_as_restarted() {
    let world = World::new();
    let both = running_pair(&world);
    world.host.ends("api/server");

    let report = world.up(&both, &Selection::All, &Env::default()).unwrap();

    assert_eq!(outcome(&report, "db"), &AppOutcome::Restarted, "{report:?}");
    assert!(
        matches!(outcome(&report, "api"), AppOutcome::Failed { .. }),
        "{report:?}"
    );
    assert_eq!(
        notes(&report, "db"),
        &[Note::plan_changed(&name("db"), name("server"))]
    );
    assert_eq!(stops_of(&world, "db/server"), 1);
    assert!(alive(&world, "db/server"));
}

#[test]
fn up_reports_a_replaced_container_as_restarted_and_leaves_it_when_a_later_app_fails() {
    let world = World::new();
    let first = plan(vec![
        container_app("store"),
        host_app("web", None, &["store"]),
    ]);
    world.up(&first, &Selection::All, &Env::default()).unwrap();
    world.host.ends("web/server");
    let second = plan(vec![
        container_app("store"),
        changed(host_app("web", None, &["store"])),
    ]);

    let report = world.up(&second, &Selection::All, &Env::default()).unwrap();

    assert_eq!(
        outcome(&report, "store"),
        &AppOutcome::Restarted,
        "{report:?}"
    );
    assert!(
        matches!(outcome(&report, "web"), AppOutcome::Failed { .. }),
        "{report:?}"
    );
    assert!(world.containers.stopped.borrow().is_empty());
}

#[test]
fn an_up_that_an_environment_failure_ends_leaves_an_earlier_restart_running() {
    let world = World::new();
    let both = running_pair(&world);
    let env = Env {
        failing: ["api".to_owned()].into(),
        ..Env::default()
    };

    let error = world.up(&both, &Selection::All, &env).unwrap_err();

    assert!(
        matches!(error, crate::error::PhaseError::Environment { .. }),
        "{error:?}"
    );
    assert_eq!(world.host.stops(), vec!["db/server"]);
    assert!(alive(&world, "db/server"));
}

#[test]
fn a_cancelled_up_leaves_an_earlier_restart_running_and_stops_the_app_it_was_starting() {
    let world = World::new();
    let both = running_pair(&world);
    let cancel = Cancel::new();
    world.host.cancels_at_launch("api/server", &cancel);

    let report = world
        .engine()
        .up(
            &both,
            &Selection::All,
            &Env::default(),
            super::deadlines(),
            &cancel,
        )
        .unwrap();

    assert_eq!(outcome(&report, "db"), &AppOutcome::Restarted, "{report:?}");
    assert_eq!(outcome(&report, "api"), &AppOutcome::Stopped, "{report:?}");
    assert_eq!(stops_of(&world, "db/server"), 1);
    assert!(alive(&world, "db/server"));
    assert!(!alive(&world, "api/server"));
}

#[test]
fn a_restart_that_a_failure_of_the_record_ends_leaves_the_apps_it_already_restarted() {
    let world = World::new();
    let plan = plan(vec![
        host_app("db", None, &[]),
        host_app("api", None, &["db"]),
    ]);
    world.up(&plan, &Selection::All, &Env::default()).unwrap();
    world.host.breaks_record_at_launch("api/server");

    let result = world.engine().restart(
        &plan,
        &Selection::All,
        &Env::default(),
        super::soon(),
        &Cancel::new(),
    );
    world.restore_runs_directory();

    assert!(result.is_err(), "{result:?}");
    assert_eq!(stops_of(&world, "db/server"), 1);
    assert!(alive(&world, "db/server"));
}

#[test]
fn a_changed_process_whose_new_launch_fails_is_not_noted_as_restarted() {
    let world = World::new();
    world
        .up(
            &plan(vec![host_app("api", None, &[])]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();
    world.host.ends("api/server");

    let report = world
        .up(
            &plan(vec![changed(host_app("api", None, &[]))]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();

    assert!(
        matches!(outcome(&report, "api"), AppOutcome::Failed { .. }),
        "{report:?}"
    );
    assert!(
        !notes(&report, "api")
            .iter()
            .any(|note| matches!(note, Note::PlanChanged { .. })),
        "{report:?}"
    );
}

#[test]
fn a_sandbox_that_cannot_be_built_leaves_the_running_process_alone() {
    if !sandbox_available() {
        return;
    }
    let world = World::outside_temp();
    world
        .up(
            &plan(vec![host_app("api", None, &[])]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();
    let before = pid(&world, "api/server");
    let protected = vec![std::path::PathBuf::from("/workspaces/reading-list")];
    let mut engine = world.engine();
    engine.settings.protected = &protected;

    let report = engine
        .up(
            &plan(vec![with_permissions(host_app("api", None, &[]))]),
            &Selection::All,
            &Env::default(),
            super::deadlines(),
            &Cancel::new(),
        )
        .unwrap();

    assert!(
        matches!(outcome(&report, "api"), AppOutcome::Errored { .. }),
        "{report:?}"
    );
    assert!(world.host.stops().is_empty());
    assert_eq!(pid(&world, "api/server"), before);
    assert!(alive(&world, "api/server"));
    assert!(notes(&report, "api").is_empty(), "{report:?}");
}

#[test]
fn a_new_port_that_another_process_holds_leaves_the_running_process_alone() {
    let world = World::new();
    world
        .up(
            &plan(vec![host_app("api", Some(5432), &[])]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();
    let before = pid(&world, "api/server");
    world.host.listen(5433, super::Listener::Held(999));

    let report = world
        .up(
            &plan(vec![host_app("api", Some(5433), &[])]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();

    assert!(
        matches!(outcome(&report, "api"), AppOutcome::Failed { .. }),
        "{report:?}"
    );
    assert!(world.host.stops().is_empty());
    assert_eq!(pid(&world, "api/server"), before);
    assert!(alive(&world, "api/server"));
}

#[test]
fn a_changed_process_that_holds_its_own_port_restarts_on_that_port() {
    let world = World::new();
    world
        .up(
            &plan(vec![host_app("api", Some(5432), &[])]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();
    let before = pid(&world, "api/server");
    world.host.listen(5432, super::Listener::Held(before));

    let report = world
        .up(
            &plan(vec![changed(host_app("api", Some(5432), &[]))]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();

    assert_eq!(
        outcome(&report, "api"),
        &AppOutcome::Restarted,
        "{report:?}"
    );
    assert_ne!(pid(&world, "api/server"), before);
}

#[test]
fn up_restarts_a_dependent_that_receives_a_port_that_moved() {
    let world = World::new();
    let api = |port: u16| {
        let mut api = host_app("api", None, &["db"]);
        api.ports
            .insert(name("db"), std::num::NonZeroU16::new(port).unwrap());
        api
    };
    world
        .up(
            &plan(vec![api(5432), host_app("db", Some(5432), &[])]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();
    let before = pid(&world, "api/server");

    let report = world
        .up(
            &plan(vec![api(5433), host_app("db", Some(5433), &[])]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();

    assert_eq!(
        outcome(&report, "api"),
        &AppOutcome::Restarted,
        "{report:?}"
    );
    assert_ne!(pid(&world, "api/server"), before);
}

#[test]
fn a_oneshot_without_once_runs_again_after_up_restarts_its_dependency() {
    let world = World::new();
    world.host.ends_as("tables/provision", Exit::Code(0));
    let tables = || super::oneshot::provisioning_app("tables", false, &["db"]);
    world
        .up(
            &plan(vec![host_app("db", None, &[]), tables()]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();

    let report = world
        .up(
            &plan(vec![changed(host_app("db", None, &[])), tables()]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();

    assert_eq!(outcome(&report, "db"), &AppOutcome::Restarted, "{report:?}");
    assert_eq!(
        outcome(&report, "tables"),
        &AppOutcome::Started,
        "{report:?}"
    );
    assert_eq!(
        world
            .host
            .launches()
            .iter()
            .filter(|launched| launched.as_str() == "tables/provision")
            .count(),
        2
    );
}

#[test]
fn a_stop_that_leaves_a_changed_process_at_the_deadline_reports_an_uncertain_start() {
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
    let passed = std::time::Instant::now();

    let report = world
        .engine()
        .up(
            &plan(vec![changed(host_app("api", None, &[]))]),
            &Selection::All,
            &Env::default(),
            crate::phase::UpDeadlines::new(passed, passed),
            &Cancel::new(),
        )
        .unwrap();

    assert_eq!(
        outcome(&report, "api"),
        &AppOutcome::StartUncertain,
        "{report:?}"
    );
    assert_eq!(world.host.launches(), vec!["api/server"]);
    assert!(
        !notes(&report, "api")
            .iter()
            .any(|note| matches!(note, Note::PlanChanged { .. })),
        "{report:?}"
    );
}

#[test]
fn a_restart_records_the_part_of_the_plan_so_the_next_up_leaves_the_process_alone() {
    let world = World::new();
    world
        .up(
            &plan(vec![host_app("api", None, &[])]),
            &Selection::All,
            &Env::default(),
        )
        .unwrap();
    let newer = plan(vec![changed(host_app("api", None, &[]))]);
    world
        .engine()
        .restart(
            &newer,
            &Selection::All,
            &Env::default(),
            super::soon(),
            &Cancel::new(),
        )
        .unwrap();
    let restarted = pid(&world, "api/server");

    let report = world.up(&newer, &Selection::All, &Env::default()).unwrap();

    assert_eq!(
        outcome(&report, "api"),
        &AppOutcome::AlreadyRunning,
        "{report:?}"
    );
    assert_eq!(pid(&world, "api/server"), restarted);
    assert_eq!(world.host.launches().len(), 2);
}
