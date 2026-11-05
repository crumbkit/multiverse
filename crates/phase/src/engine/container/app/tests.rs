//! Tests of phase's compose project of one app.

use std::num::NonZeroU16;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;

use super::{compose_file, file_secrets, objects, plan_hash, reference, references};
use crate::engine::container::names::{Identity, Scope};
use crate::engine::container::secrets::{ImageCommand, entrypoint};
use crate::error::{NotRunnable, PhaseError};
use crate::plan::container::BindMount;
use crate::plan::restart::{Restart, RestartLimits, Stop, StopSignal};
use crate::plan::{AppPlan, ProcessKind, ProcessPlan, RunnerKind, SecretDelivery, digest};
use crate::record::run::{PlanHash, VolumeLifetime};
use crate::source::Source;

/// The workspace id of the tests.
const WORKSPACE_ID: &str = "fedcba9876543210fedcba9876543210";

fn name<T: std::str::FromStr>(text: &str) -> T
where
    T::Err: std::fmt::Debug,
{
    text.parse().unwrap()
}

fn scope(app: &str) -> Scope {
    Scope::app(
        Identity {
            id: name(WORKSPACE_ID),
            run_record: PathBuf::from(format!("/state/runs/{WORKSPACE_ID}.json")),
        },
        &name(app),
    )
}

/// Returns the app `db`: a `server` that publishes the port `pg`, mounts
/// the volume `data` and receives `PGPORT` and the secret-free `MODE`, and
/// a `worker` that receives `MODE`, with a cache volume, a read-only bind
/// mount and a run-as user.
fn db() -> AppPlan {
    let mut app = AppPlan::new(
        name("db"),
        RunnerKind::Container,
        Source::image(name("postgres:17")),
    );
    app.ports
        .insert(name("pg"), NonZeroU16::new(25432).unwrap());
    let mut server = ProcessPlan::new(name("server"), Vec::new());
    server.env = ["PGPORT".to_owned(), "MODE".to_owned()].into();
    server
        .publish
        .insert(name("pg"), NonZeroU16::new(5432).unwrap());
    server
        .volumes
        .insert(name("data"), PathBuf::from("/var/lib/postgresql/data"));
    server.restart = Restart::on_failure(RestartLimits::default());
    server.stop = Stop {
        signal: StopSignal::Int,
        grace: Duration::from_millis(2500),
    };
    let mut worker = ProcessPlan::new(
        name("worker"),
        vec!["sh".to_owned(), "-c".to_owned(), "echo $HOME".to_owned()],
    );
    worker.env = ["MODE".to_owned()].into();
    app.processes = vec![server, worker];
    app.container
        .caches
        .insert(name("cache"), PathBuf::from("/cache"));
    app.container.mounts.push(BindMount::new(
        PathBuf::from("/src/db"),
        PathBuf::from("/app"),
        true,
    ));
    app.container.user = Some("1000:1000".to_owned());
    app
}

#[test]
fn an_apps_compose_file_runs_each_process_as_a_labelled_service_and_holds_no_value_of_a_variable() {
    let app = db();
    let scope = scope("db");
    let project = scope.project();
    let workspace = project.as_str().strip_suffix("-db").unwrap().to_owned();
    let labels = |process: &str| {
        json!({
            "mxv.workspace-id": WORKSPACE_ID,
            "mxv.run-record": format!("/state/runs/{WORKSPACE_ID}.json"),
            "mxv.app": "db",
            "mxv.container": process,
        })
    };
    let mounts = |own: Vec<serde_json::Value>| {
        let mut mounts = own;
        mounts.push(json!({"type": "volume", "source": "cache", "target": "/cache"}));
        mounts.push(
            json!({"type": "bind", "source": "/src/db", "target": "/app", "read_only": true}),
        );
        mounts
    };
    assert_eq!(
        compose_file(&scope, &app, None).unwrap(),
        json!({
            "services": {
                "server": {
                    "image": "postgres:17",
                    "container_name": format!("{project}.server"),
                    "network_mode": "bridge",
                    "labels": labels("server"),
                    "user": "1000:1000",
                    "environment": {"MODE": "${MXV_ENV_0_0:-}", "PGPORT": "${MXV_ENV_0_1:-}"},
                    "ports": ["127.0.0.1:25432:5432/tcp"],
                    "volumes": mounts(vec![json!({"type": "volume", "source": "data", "target": "/var/lib/postgresql/data"})]),
                    "restart": "on-failure",
                    "stop_signal": "SIGINT",
                    "stop_grace_period": "3s",
                },
                "worker": {
                    "image": "postgres:17",
                    "container_name": format!("{project}.worker"),
                    "network_mode": "bridge",
                    "labels": labels("worker"),
                    "command": ["sh", "-c", "echo $$HOME"],
                    "user": "1000:1000",
                    "environment": {"MODE": "${MXV_ENV_1_0:-}"},
                    "ports": [],
                    "volumes": mounts(Vec::new()),
                    "restart": "no",
                    "stop_signal": "SIGTERM",
                    "stop_grace_period": "10s",
                },
            },
            "volumes": {
                "cache": {"name": format!("{workspace}.cache"), "external": true},
                "data": {"name": format!("{workspace}.data"), "external": true},
            },
        })
    );
}

#[test]
fn each_variable_refers_to_a_private_variable_of_its_own_services_compose_child() {
    let app = db();
    assert_eq!(
        reference(&app, &name("server"), "MODE").as_deref(),
        Some("MXV_ENV_0_0")
    );
    assert_eq!(
        reference(&app, &name("server"), "PGPORT").as_deref(),
        Some("MXV_ENV_0_1")
    );
    assert_eq!(
        reference(&app, &name("worker"), "MODE").as_deref(),
        Some("MXV_ENV_1_0")
    );
    assert_eq!(reference(&app, &name("worker"), "PGPORT"), None);
    assert_eq!(reference(&app, &name("worker"), "PATH"), None);
    assert_eq!(reference(&app, &name("other"), "MODE"), None);
}

#[test]
fn an_app_creates_a_container_per_process_and_the_workspace_volumes_it_mounts_which_outlive_the_run()
 {
    let scope = scope("db");
    let found = objects(&scope, &db()).unwrap();
    let project = scope.project();
    let workspace = project.as_str().strip_suffix("-db").unwrap().to_owned();
    assert_eq!(
        found
            .containers
            .iter()
            .map(|(process, name)| (process.as_str(), name.as_str().to_owned()))
            .collect::<Vec<_>>(),
        vec![
            ("server", format!("{project}.server")),
            ("worker", format!("{project}.worker")),
        ]
    );
    assert!(found.networks.is_empty());
    assert_eq!(
        found
            .volumes
            .iter()
            .map(|(name, lifetime)| (name.as_str().to_owned(), *lifetime))
            .collect::<Vec<_>>(),
        vec![
            (format!("{workspace}.cache"), VolumeLifetime::Workspace),
            (format!("{workspace}.data"), VolumeLifetime::Workspace),
        ]
    );
}

/// Returns the reason that `objects` and `compose_file` refuse `app`.
fn refusal(app: &AppPlan) -> NotRunnable {
    let scope = scope("db");
    let file = compose_file(&scope, app, None);
    match (objects(&scope, app), file) {
        (
            Err(PhaseError::NotRunnable { reason, .. }),
            Err(PhaseError::NotRunnable { reason: same, .. }),
        ) if reason == same => reason,
        other => panic!("{other:?}"),
    }
}

/// Returns the app `db` whose `server` receives `PGPASSWORD` and `TOKEN`,
/// which the app's permissions name as secrets.
fn with_secrets() -> AppPlan {
    let mut app = db();
    app.processes[0].env.insert("PGPASSWORD".to_owned());
    app.processes[0].env.insert("TOKEN".to_owned());
    app.permissions.secrets = ["PGPASSWORD".to_owned(), "TOKEN".to_owned()].into();
    app
}

#[test]
fn a_process_that_receives_secrets_mounts_a_tmpfs_and_waits_for_their_files_through_the_wrapper() {
    let app = with_secrets();
    let scope = scope("db");

    let file = compose_file(&scope, &app, None).unwrap();

    let server = &file["services"]["server"];
    assert_eq!(
        server["environment"],
        json!({"MODE": "${MXV_ENV_0_0:-}", "PGPORT": "${MXV_ENV_0_1:-}"})
    );
    assert!(
        server["volumes"].as_array().unwrap().contains(
            &json!({"type": "tmpfs", "target": "/run/secrets", "tmpfs": {"mode": 0o700}})
        )
    );
    let names: Vec<String> = ["PGPASSWORD".to_owned(), "TOKEN".to_owned()].into();
    assert_eq!(server["entrypoint"], json!(entrypoint(names.iter())));
    assert!(server.get("command").is_none());
    let worker = &file["services"]["worker"];
    assert!(worker.get("entrypoint").is_none());
    assert!(!worker["volumes"].to_string().contains("tmpfs"));
    assert_eq!(reference(&app, &name("server"), "TOKEN"), None);
    assert_eq!(
        file_secrets(&app, &app.processes[0]).collect::<Vec<_>>(),
        ["PGPASSWORD", "TOKEN"]
    );
    assert_eq!(file_secrets(&app, &app.processes[1]).count(), 0);
}

#[test]
fn the_wrapper_runs_the_images_entrypoint_and_the_plans_command_or_else_the_images_command() {
    let mut app = with_secrets();
    let image = ImageCommand::new(&["docker-entrypoint.sh"], &["postgres"]);
    let scope = scope("db");

    let file = compose_file(&scope, &app, Some(&image)).unwrap();
    assert_eq!(
        file["services"]["server"]["command"],
        json!(["docker-entrypoint.sh", "postgres"])
    );
    assert_eq!(
        file["services"]["worker"]["command"],
        json!(["sh", "-c", "echo $$HOME"])
    );

    app.processes[0].run = vec!["postgres".to_owned(), "-c".to_owned(), "x=$1".to_owned()];
    let file = compose_file(&scope, &app, Some(&image)).unwrap();
    assert_eq!(
        file["services"]["server"]["command"],
        json!(["docker-entrypoint.sh", "postgres", "-c", "x=$$1"])
    );
}

#[test]
fn a_process_that_opts_into_environment_secrets_receives_them_through_private_references() {
    let mut app = with_secrets();
    app.processes[0].secret_delivery = SecretDelivery::Env;

    let file = compose_file(&scope("db"), &app, None).unwrap();

    let server = &file["services"]["server"];
    assert_eq!(
        server["environment"],
        json!({
            "MODE": "${MXV_ENV_0_0:-}",
            "PGPASSWORD": "${MXV_ENV_0_1:-}",
            "PGPORT": "${MXV_ENV_0_2:-}",
            "TOKEN": "${MXV_ENV_0_3:-}",
        })
    );
    assert!(server.get("entrypoint").is_none());
    assert!(!server["volumes"].to_string().contains("tmpfs"));
    assert_eq!(
        reference(&app, &name("server"), "TOKEN").as_deref(),
        Some("MXV_ENV_0_3")
    );
    assert_eq!(file_secrets(&app, &app.processes[0]).count(), 0);
}

#[test]
fn a_processs_plan_hash_does_not_depend_on_its_images_command() {
    let app = with_secrets();
    let scope = scope("db");
    let file = compose_file(&scope, &app, None).unwrap();

    let hash = plan_hash(&scope, &app, &name("server")).unwrap();

    assert_eq!(
        hash,
        PlanHash::new(digest(file["services"]["server"].to_string().as_bytes()))
    );
}

#[test]
fn a_container_oneshot_or_working_directory_is_not_runnable() {
    let mut app = db();
    app.processes[1].kind = ProcessKind::Oneshot { once: false };
    assert_eq!(refusal(&app), NotRunnable::Oneshot);
    let mut app = db();
    app.processes[1].cwd = Some(PathBuf::from("src"));
    assert_eq!(refusal(&app), NotRunnable::ContainerWorkingDirectory);
}

#[test]
fn a_container_name_longer_than_a_docker_name_may_be_refuses_its_process() {
    let mut app = db();
    app.name = name(&"d".repeat(64));
    app.processes[0].name = name(&"s".repeat(64));
    let refused = objects(&scope(app.name.as_str()), &app);
    assert!(
        matches!(&refused, Err(PhaseError::ProcessUnstartable { process, .. }) if process == &app.processes[0].name),
        "{refused:?}"
    );
}

#[test]
fn each_restart_policy_maps_to_dockers_own_policy() {
    let mut app = db();
    app.processes[0].restart = Restart::always(RestartLimits::default());
    app.processes[1].restart = Restart::on_failure(RestartLimits {
        max: std::num::NonZeroU32::new(3),
        ..RestartLimits::default()
    });
    let file = compose_file(&scope("db"), &app, None).unwrap();
    assert_eq!(file["services"]["server"]["restart"], "unless-stopped");
    assert_eq!(file["services"]["worker"]["restart"], "on-failure:3");
    app.processes[1].restart = Restart::No;
    let file = compose_file(&scope("db"), &app, None).unwrap();
    assert_eq!(file["services"]["worker"]["restart"], "no");
}

#[test]
fn a_stop_grace_is_written_in_whole_seconds_rounded_up_and_at_least_one() {
    let mut app = db();
    app.processes[0].stop.grace = Duration::from_millis(1);
    app.processes[1].stop.grace = Duration::ZERO;
    let file = compose_file(&scope("db"), &app, None).unwrap();
    assert_eq!(file["services"]["server"]["stop_grace_period"], "1s");
    assert_eq!(file["services"]["worker"]["stop_grace_period"], "1s");
    app.processes[0].stop.grace = Duration::from_millis(4001);
    let file = compose_file(&scope("db"), &app, None).unwrap();
    assert_eq!(file["services"]["server"]["stop_grace_period"], "5s");
}

#[test]
fn an_apps_containers_are_planned_in_dependency_order() {
    let mut app = db();
    app.processes[0].depends.push(name("worker"));
    let found = objects(&scope("db"), &app).unwrap();
    let order: Vec<&str> = found
        .containers
        .iter()
        .map(|(process, _)| process.as_str())
        .collect();
    assert_eq!(order, ["worker", "server"]);
}

#[test]
fn a_processs_plan_hash_changes_only_with_its_own_part_of_the_app() {
    let scope = scope("db");
    let app = db();
    let server = plan_hash(&scope, &app, &name("server")).unwrap();
    let worker = plan_hash(&scope, &app, &name("worker")).unwrap();
    assert_ne!(server, worker);
    assert_eq!(plan_hash(&scope, &db(), &name("server")).unwrap(), server);

    let mut changed = db();
    changed.processes[1].run.push("again".to_owned());
    assert_eq!(
        plan_hash(&scope, &changed, &name("server")).unwrap(),
        server
    );
    assert_ne!(
        plan_hash(&scope, &changed, &name("worker")).unwrap(),
        worker
    );

    let mut image = db();
    image.source = Source::image(name("postgres:18"));
    assert_ne!(plan_hash(&scope, &image, &name("server")).unwrap(), server);
}

#[test]
fn a_label_value_with_a_dollar_sign_reaches_the_compose_file_escaped() {
    let scope = Scope::app(
        Identity {
            id: name(WORKSPACE_ID),
            run_record: PathBuf::from("/state/$HOME/runs.json"),
        },
        &name("db"),
    );
    let file = compose_file(&scope, &db(), None).unwrap();
    assert_eq!(
        file["services"]["server"]["labels"]["mxv.run-record"],
        "/state/$$HOME/runs.json"
    );
}

#[test]
fn the_references_of_an_app_are_every_private_variable_its_compose_file_names() {
    let app = db();
    let file = compose_file(&scope("db"), &app, None).unwrap().to_string();
    let all = references(&app);
    assert_eq!(all, ["MXV_ENV_0_0", "MXV_ENV_0_1", "MXV_ENV_1_0"]);
    for name in &all {
        assert!(file.contains(&format!("${{{name}:-}}")), "{name}");
    }
}
