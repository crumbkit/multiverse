//! Tests of phase's compose project of one app.

use std::num::NonZeroU16;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;

use super::{
    compose_file, file_secrets, for_environment, keys_sorted, objects, plan_hash, reference,
    references, values_digest,
};
use crate::engine::container::names::{Identity, Scope};
use crate::engine::container::secrets::{ImageConfig, entrypoint};
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
    app.permissions.get_or_insert_default().secrets =
        ["PGPASSWORD".to_owned(), "TOKEN".to_owned()].into();
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
    assert_eq!(server["restart"], "no");
    assert_eq!(server["labels"]["mxv.secrets"], "files");
    assert!(server.get("x-phase-secret-owner").is_none());
    let worker = &file["services"]["worker"];
    assert!(worker["labels"].get("mxv.secrets").is_none());
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
    let image = ImageConfig::new(&["docker-entrypoint.sh"], &["postgres"], "");
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
fn a_new_secret_owner_changes_the_plan_hash_of_its_process_alone() {
    let mut app = with_secrets();
    let scope = scope("db");
    let before = plan_hash(&scope, &app, &name("server"), None).unwrap();
    let worker = plan_hash(&scope, &app, &name("worker"), None).unwrap();

    app.processes[0].secret_owner = Some("999:999".to_owned());

    let file = compose_file(&scope, &app, None).unwrap();
    assert_eq!(
        file["services"]["server"]["x-phase-secret-owner"],
        "999:999"
    );
    assert_ne!(
        plan_hash(&scope, &app, &name("server"), None).unwrap(),
        before
    );
    assert_eq!(
        plan_hash(&scope, &app, &name("worker"), None).unwrap(),
        worker
    );
}

#[test]
fn a_new_volume_owner_changes_the_plan_hash_of_each_process_that_mounts_a_volume() {
    let mut app = db();
    app.container.caches.clear();
    let scope = scope("db");
    let server = plan_hash(&scope, &app, &name("server"), None).unwrap();
    let worker = plan_hash(&scope, &app, &name("worker"), None).unwrap();
    let file = compose_file(&scope, &app, None).unwrap();
    assert!(
        file["services"]["server"]
            .get("x-phase-volume-owner")
            .is_none(),
        "{file}"
    );

    app.container.owner = Some("999:999".to_owned());

    let file = compose_file(&scope, &app, None).unwrap();
    assert_eq!(
        file["services"]["server"]["x-phase-volume-owner"],
        "999:999"
    );
    assert_ne!(
        plan_hash(&scope, &app, &name("server"), None).unwrap(),
        server
    );
    assert_eq!(
        plan_hash(&scope, &app, &name("worker"), None).unwrap(),
        worker
    );
}

#[test]
fn a_processs_plan_hash_does_not_depend_on_its_images_command() {
    let app = with_secrets();
    let scope = scope("db");
    let file = compose_file(&scope, &app, None).unwrap();

    let hash = plan_hash(&scope, &app, &name("server"), None).unwrap();

    assert_eq!(
        hash,
        PlanHash::new(digest(
            keys_sorted(&file["services"]["server"])
                .to_string()
                .as_bytes()
        ))
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
    let server = plan_hash(&scope, &app, &name("server"), None).unwrap();
    let worker = plan_hash(&scope, &app, &name("worker"), None).unwrap();
    assert_ne!(server, worker);
    assert_eq!(
        plan_hash(&scope, &db(), &name("server"), None).unwrap(),
        server
    );

    let mut changed = db();
    changed.processes[1].run.push("again".to_owned());
    assert_eq!(
        plan_hash(&scope, &changed, &name("server"), None).unwrap(),
        server
    );
    assert_ne!(
        plan_hash(&scope, &changed, &name("worker"), None).unwrap(),
        worker
    );

    let mut image = db();
    image.source = Source::image(name("postgres:18"));
    assert_ne!(
        plan_hash(&scope, &image, &name("server"), None).unwrap(),
        server
    );
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

/// Returns the app of [`with_secrets`] whose `server` also receives the
/// optional `LANG` and treats the secret `TOKEN` as optional.
fn with_optional() -> AppPlan {
    let mut app = with_secrets();
    app.processes[0].env.insert("LANG".to_owned());
    app.processes[0].optional_env = ["LANG".to_owned(), "TOKEN".to_owned()].into();
    app
}

#[test]
fn an_optional_variable_is_named_in_the_service_so_the_plan_hash_covers_the_flag() {
    let optional = with_optional();
    let mut required = optional.clone();
    required.processes[0].optional_env.clear();
    let scope = scope("db");

    let file = compose_file(&scope, &optional, None).unwrap();

    assert_eq!(
        file["services"]["server"]["x-phase-optional-env"],
        json!(["LANG", "TOKEN"])
    );
    assert!(
        file["services"]["worker"]
            .get("x-phase-optional-env")
            .is_none()
    );
    assert!(
        compose_file(&scope, &db(), None).unwrap()["services"]["server"]
            .get("x-phase-optional-env")
            .is_none()
    );
    assert_ne!(
        plan_hash(&scope, &optional, &name("server"), None).unwrap(),
        plan_hash(&scope, &required, &name("server"), None).unwrap()
    );
}

#[test]
fn an_absent_optional_variable_is_left_out_of_the_service_and_a_required_one_stays() {
    let app = with_optional();
    let scope = scope("db");
    let file = compose_file(&scope, &app, None).unwrap();
    let all: std::collections::BTreeSet<String> = ["LANG", "MODE", "PGPASSWORD", "TOKEN"]
        .map(str::to_owned)
        .into();
    let without: std::collections::BTreeSet<String> =
        ["MODE", "PGPASSWORD"].map(str::to_owned).into();

    let present = for_environment(&scope, &app, None, &name("server"), &all).unwrap();
    let absent = for_environment(&scope, &app, None, &name("server"), &without).unwrap();

    assert_eq!(present, file);
    let server = &absent["services"]["server"];
    // PGPORT is required, and phase refuses an environment that lacks it
    // before it builds this file, so the service always refers to it.
    assert_eq!(
        server["environment"],
        json!({"MODE": "${MXV_ENV_0_1:-}", "PGPORT": "${MXV_ENV_0_2:-}"})
    );
    assert_eq!(
        file["services"]["server"]["environment"]["LANG"],
        "${MXV_ENV_0_0:-}"
    );
    assert_eq!(
        server["entrypoint"],
        json!(entrypoint(["PGPASSWORD".to_owned()].iter()))
    );
    assert_eq!(absent["services"]["worker"], file["services"]["worker"]);
    assert_eq!(
        plan_hash(&scope, &app, &name("server"), None).unwrap(),
        PlanHash::new(digest(
            keys_sorted(&file["services"]["server"])
                .to_string()
                .as_bytes()
        ))
    );
}

#[test]
fn a_process_whose_every_secret_file_is_optional_and_absent_starts_without_files() {
    let mut app = with_optional();
    app.processes[0]
        .optional_env
        .insert("PGPASSWORD".to_owned());
    app.processes[0].secret_owner = Some("999:999".to_owned());
    let scope = scope("db");
    let image = ImageConfig::new(&["docker-entrypoint.sh"], &["postgres"], "");
    let without: std::collections::BTreeSet<String> = ["MODE", "PGPORT"].map(str::to_owned).into();

    let file = for_environment(&scope, &app, Some(&image), &name("server"), &without).unwrap();

    let server = &file["services"]["server"];
    assert!(server.get("entrypoint").is_none(), "{server}");
    assert!(server.get("command").is_none(), "{server}");
    assert!(server["labels"].get("mxv.secrets").is_none(), "{server}");
    assert!(server.get("x-phase-secret-owner").is_none(), "{server}");
    assert!(!server["volumes"].to_string().contains("tmpfs"), "{server}");
    assert_eq!(server["restart"], "on-failure");
    let full = compose_file(&scope, &app, Some(&image)).unwrap();
    assert_eq!(full["services"]["server"]["restart"], "no");
    assert_eq!(
        plan_hash(&scope, &app, &name("server"), None).unwrap(),
        PlanHash::new(digest(
            keys_sorted(&compose_file(&scope, &app, None).unwrap()["services"]["server"])
                .to_string()
                .as_bytes()
        ))
    );
}

#[test]
fn a_processs_plan_hash_is_the_one_of_its_service_with_sorted_keys_whatever_order_serde_json_keeps()
{
    let hash = plan_hash(&scope("db"), &db(), &name("server"), None).unwrap();

    // The hash that phase recorded before a dependency turned on serde_json's
    // insertion order, so an upgrade replaces no container.
    assert_eq!(
        serde_json::to_value(&hash).unwrap(),
        json!("0693baefa96a4945e387c7b392c3d90651d1de37b9649a80a747e70aa2fdc1b0")
    );
}

/// An environment of the tests that holds its values in a list and writes
/// each value when asked.
struct Values(Vec<(&'static str, &'static str)>);

impl crate::env::AppliedEnvironment for Values {
    fn names(&self) -> std::collections::BTreeSet<String> {
        self.0.iter().map(|(name, _)| (*name).to_owned()).collect()
    }

    fn expires_at(&self) -> Option<std::time::SystemTime> {
        None
    }

    fn digest(&self, key: &[u8; 32]) -> [u8; 32] {
        *key
    }

    fn apply_as(
        &self,
        _command: &mut std::process::Command,
        _reference: &dyn Fn(&str) -> Option<String>,
    ) {
    }

    fn write_value(&self, name: &str, out: &mut dyn std::io::Write) -> std::io::Result<()> {
        let (_, value) = self
            .0
            .iter()
            .find(|(known, _)| *known == name)
            .ok_or(std::io::ErrorKind::NotFound)?;
        out.write_all(value.as_bytes())
    }
}

/// Returns the plan hash of `process` of `app` with the digest of the
/// values of `environment` under a fixed key.
fn hash_with(app: &AppPlan, process: &str, environment: &Values) -> PlanHash {
    let key = crate::env::DigestKey::from_bytes([9; 32]);
    let digest = values_digest(app, &app.processes[0], environment, &key).unwrap();
    assert!(digest.is_some());
    plan_hash(&scope("db"), app, &name(process), digest.as_ref()).unwrap()
}

#[test]
fn a_changed_value_changes_the_plan_hash_and_an_unchanged_value_keeps_it() {
    let app = db();
    let s3 = Values(vec![("MODE", "s3"), ("PGPORT", "5432")]);
    let again = Values(vec![("MODE", "s3"), ("PGPORT", "5432")]);
    let sqs = Values(vec![("MODE", "s3,sqs"), ("PGPORT", "5432")]);

    let before = hash_with(&app, "server", &s3);

    assert_eq!(hash_with(&app, "server", &again), before);
    assert_ne!(hash_with(&app, "server", &sqs), before);
    assert_ne!(
        before,
        plan_hash(&scope("db"), &app, &name("server"), None).unwrap()
    );
}

#[test]
fn a_secrets_value_never_changes_the_plan_hash_whatever_its_delivery() {
    for delivery in [SecretDelivery::File, SecretDelivery::Env] {
        let mut app = with_secrets();
        app.processes[0].secret_delivery = delivery;
        let one = Values(vec![
            ("MODE", "dev"),
            ("PGPORT", "5432"),
            ("PGPASSWORD", "first-password"),
            ("TOKEN", "first-token"),
        ]);
        let other = Values(vec![
            ("MODE", "dev"),
            ("PGPORT", "5432"),
            ("PGPASSWORD", "second-password"),
            ("TOKEN", "second-token"),
        ]);
        let changed = Values(vec![
            ("MODE", "prod"),
            ("PGPORT", "5432"),
            ("PGPASSWORD", "first-password"),
            ("TOKEN", "first-token"),
        ]);

        let hash = hash_with(&app, "server", &one);

        assert_eq!(hash_with(&app, "server", &other), hash, "{delivery:?}");
        assert_ne!(hash_with(&app, "server", &changed), hash, "{delivery:?}");
        let printed = format!("{hash:?} {}", serde_json::to_string(&hash).unwrap());
        for value in ["first-password", "first-token", "dev"] {
            assert!(!printed.contains(value), "{printed}");
        }
    }
}

#[test]
fn the_digest_covers_the_variables_that_are_not_secrets_and_an_absent_optional_one() {
    let app = with_optional();
    let with_lang = Values(vec![("MODE", "dev"), ("PGPORT", "5432"), ("LANG", "C")]);
    let without = Values(vec![("MODE", "dev"), ("PGPORT", "5432")]);
    let empty = Values(vec![("MODE", "dev"), ("PGPORT", "5432"), ("LANG", "")]);

    let hashes = [
        hash_with(&app, "server", &with_lang),
        hash_with(&app, "server", &without),
        hash_with(&app, "server", &empty),
    ];

    assert_ne!(hashes[0], hashes[1]);
    assert_ne!(hashes[1], hashes[2]);
    assert_ne!(hashes[0], hashes[2]);
}

#[test]
fn an_environment_that_cannot_write_a_value_gives_no_digest_and_another_failure_is_an_error() {
    struct Unwritable;
    impl crate::env::AppliedEnvironment for Unwritable {
        fn names(&self) -> std::collections::BTreeSet<String> {
            ["MODE".to_owned(), "PGPORT".to_owned()].into()
        }
        fn expires_at(&self) -> Option<std::time::SystemTime> {
            None
        }
        fn digest(&self, key: &[u8; 32]) -> [u8; 32] {
            *key
        }
        fn apply_as(
            &self,
            _command: &mut std::process::Command,
            _reference: &dyn Fn(&str) -> Option<String>,
        ) {
        }
    }
    let app = db();
    let key = crate::env::DigestKey::from_bytes([9; 32]);

    assert_eq!(
        values_digest(&app, &app.processes[0], &Unwritable, &key).unwrap(),
        None
    );
    let lacking = Values(vec![("MODE", "dev")]);
    struct Lying(Values);
    impl crate::env::AppliedEnvironment for Lying {
        fn names(&self) -> std::collections::BTreeSet<String> {
            ["MODE".to_owned(), "PGPORT".to_owned()].into()
        }
        fn expires_at(&self) -> Option<std::time::SystemTime> {
            None
        }
        fn digest(&self, key: &[u8; 32]) -> [u8; 32] {
            *key
        }
        fn apply_as(
            &self,
            _command: &mut std::process::Command,
            _reference: &dyn Fn(&str) -> Option<String>,
        ) {
        }
        fn write_value(&self, name: &str, out: &mut dyn std::io::Write) -> std::io::Result<()> {
            crate::env::AppliedEnvironment::write_value(&self.0, name, out)
        }
    }
    let error = values_digest(&app, &app.processes[0], &Lying(lacking), &key).unwrap_err();
    assert!(error.contains("PGPORT"), "{error}");
}
