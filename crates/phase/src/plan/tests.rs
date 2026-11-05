use std::num::NonZeroU16;
use std::path::PathBuf;
use std::time::Duration;

use super::container::BindMount;
use super::probe::{Probe, ProbeCheck};
use super::restart::{Restart, RestartLimits};
use super::{
    AppPlan, ContainerProblem, MOST_READY_TIMEOUT, PermissionProblem, Plan, PlanError, ProcessKind,
    ProcessPlan, ProcessProblem, RunnerKind, SecretDelivery, SourceProblem,
};
use crate::source::{BuildSpec, ReleaseSpec, Source, Verification};

fn checkout() -> Source {
    Source::Checkout {
        dir: PathBuf::from("/workspaces/reading-list/api"),
        build: Some(BuildSpec::new(
            vec!["cargo".into(), "build".into(), "--release".into()],
            PathBuf::from("target/release/api"),
        )),
    }
}

fn host_app(name: &str) -> AppPlan {
    let mut app = AppPlan::new(name.parse().unwrap(), RunnerKind::Host, checkout());
    app.ports
        .insert("http".parse().unwrap(), NonZeroU16::new(41_000).unwrap());
    let mut server = ProcessPlan::new(
        "server".parse().unwrap(),
        vec!["{program}".into(), "serve".into()],
    );
    server.port = Some("http".parse().unwrap());
    server.ready = Some(Probe::new(ProbeCheck::Http {
        path: "/healthz".into(),
    }));
    server.restart = Restart::OnFailure(RestartLimits::default());
    app.processes.push(server);
    app
}

fn container_app(name: &str) -> AppPlan {
    let mut app = AppPlan::new(
        name.parse().unwrap(),
        RunnerKind::Container,
        Source::Image {
            image: "postgres:17".parse().unwrap(),
        },
    );
    app.ports
        .insert("pg".parse().unwrap(), NonZeroU16::new(41_001).unwrap());
    let mut server = ProcessPlan::new("server".parse().unwrap(), Vec::new());
    server
        .publish
        .insert("pg".parse().unwrap(), NonZeroU16::new(5432).unwrap());
    server.volumes.insert(
        "data".parse().unwrap(),
        PathBuf::from("/var/lib/postgresql/data"),
    );
    app.processes.push(server);
    app
}

fn plan(apps: Vec<AppPlan>) -> Result<Plan, PlanError> {
    Plan::new(
        "reading-list".parse().unwrap(),
        "0b9f3c8e-6a1d-4f7b-9c2e-5d8a7b6c4e21".parse().unwrap(),
        apps,
    )
}

fn process_problem(app: AppPlan) -> ProcessProblem {
    match plan(vec![app]) {
        Err(PlanError::BadProcess { problem, .. }) => problem,
        other => panic!("expected a bad process, got {other:?}"),
    }
}

#[test]
fn a_plan_survives_a_trip_through_json() {
    let mut api = host_app("api");
    api.depends.push("auth".parse().unwrap());
    let mut migrate = ProcessPlan::new("migrate".parse().unwrap(), vec!["{program}".into()]);
    migrate.kind = ProcessKind::Oneshot { once: true };
    api.processes.push(migrate);
    api.permissions
        .get_or_insert_default()
        .listen
        .insert("http".parse().unwrap());
    api.permissions
        .get_or_insert_default()
        .secrets
        .insert("STRIPE_KEY".into());
    let original = plan(vec![api, host_app("auth")]).unwrap();

    let json = serde_json::to_string(&original).unwrap();
    let read: Plan = serde_json::from_str(&json).unwrap();

    assert_eq!(read, original);
    assert_eq!(
        serde_json::to_value(&original).unwrap()["schema"],
        "phase/plan/v1"
    );
}

#[test]
fn a_plan_refuses_two_apps_with_the_same_name() {
    assert_eq!(
        plan(vec![host_app("api"), host_app("api")]),
        Err(PlanError::DuplicateApp {
            app: "api".parse().unwrap()
        })
    );
}

#[test]
fn a_plan_read_from_json_refuses_two_apps_with_the_same_name() {
    let one = plan(vec![host_app("api")]).unwrap();
    let mut json = serde_json::to_value(&one).unwrap();
    let app = json["apps"][0].clone();
    json["apps"].as_array_mut().unwrap().push(app);

    let read = serde_json::from_value::<Plan>(json);

    assert!(
        read.unwrap_err()
            .to_string()
            .contains("two apps are named api")
    );
}

#[test]
fn a_plan_refuses_a_schema_it_does_not_know() {
    let one = plan(vec![host_app("api")]).unwrap();
    let mut json = serde_json::to_value(&one).unwrap();
    json["schema"] = "phase/plan/v2".into();

    assert!(serde_json::from_value::<Plan>(json).is_err());
}

#[test]
fn a_plan_refuses_two_processes_of_an_app_with_the_same_name() {
    let mut api = host_app("api");
    let again = api.processes[0].clone();
    api.processes.push(again);

    assert_eq!(
        plan(vec![api]),
        Err(PlanError::DuplicateProcess {
            app: "api".parse().unwrap(),
            process: "server".parse().unwrap(),
        })
    );
}

#[test]
fn a_plan_refuses_a_dependency_on_an_app_it_lacks() {
    let mut api = host_app("api");
    api.depends.push("auth".parse().unwrap());

    assert_eq!(
        plan(vec![api]),
        Err(PlanError::UnknownDependency {
            app: "api".parse().unwrap(),
            depends: "auth".parse().unwrap(),
        })
    );
}

#[test]
fn a_plan_refuses_a_container_app_that_runs_from_a_checkout() {
    let mut api = host_app("api");
    api.runner = RunnerKind::Container;

    assert_eq!(
        plan(vec![api]),
        Err(PlanError::RunnerMismatch {
            app: "api".parse().unwrap(),
            runner: RunnerKind::Container,
        })
    );
}

#[test]
fn a_plan_refuses_a_host_app_that_runs_from_an_image() {
    let mut api = host_app("api");
    api.source = Source::Image {
        image: "postgres:17".parse().unwrap(),
    };

    assert!(matches!(
        plan(vec![api]),
        Err(PlanError::RunnerMismatch { .. })
    ));
}

#[test]
fn a_plan_refuses_permission_to_listen_on_a_port_the_app_lacks() {
    let mut api = host_app("api");
    api.permissions
        .get_or_insert_default()
        .listen
        .insert("admin".parse().unwrap());

    assert_eq!(
        plan(vec![api]),
        Err(PlanError::UnknownPort {
            app: "api".parse().unwrap(),
            port: "admin".parse().unwrap(),
        })
    );
}

#[test]
fn a_plan_refuses_a_host_process_with_an_empty_command() {
    let mut api = host_app("api");
    api.processes[0].run.clear();

    assert_eq!(process_problem(api), ProcessProblem::EmptyCommand);
}

#[test]
fn a_plan_refuses_an_http_probe_on_a_process_without_a_port() {
    let mut api = host_app("api");
    api.processes[0].port = None;

    assert_eq!(process_problem(api), ProcessProblem::HttpProbeWithoutPort);
}

#[test]
fn a_plan_refuses_an_http_probe_path_without_a_leading_slash() {
    let mut api = host_app("api");
    api.processes[0].ready = Some(Probe::new(ProbeCheck::Http {
        path: "healthz".into(),
    }));

    assert_eq!(process_problem(api), ProcessProblem::HttpPathWithoutSlash);
}

#[test]
fn a_plan_refuses_an_http_probe_path_that_cannot_be_sent_as_written() {
    for path in ["/a b", "/a\r\nX: 1", "/caf\u{e9}", "/page#top", "/\u{7f}"] {
        let mut api = host_app("api");
        api.processes[0].ready = Some(Probe::new(ProbeCheck::http(path.to_owned())));

        assert_eq!(
            process_problem(api),
            ProcessProblem::HttpPathUnsendable,
            "{path:?}"
        );
    }
}

#[test]
fn a_plan_accepts_an_http_probe_path_with_a_query() {
    let mut api = host_app("api");
    api.processes[0].ready = Some(Probe::new(ProbeCheck::http(
        "/health?deep=1&x=%20".to_owned(),
    )));

    assert!(plan(vec![api]).is_ok());
}

#[test]
fn a_plan_refuses_a_tcp_probe_on_a_port_the_app_lacks() {
    let mut api = host_app("api");
    api.processes[0].ready = Some(Probe::new(ProbeCheck::Tcp {
        port: "pg".parse().unwrap(),
    }));

    assert_eq!(
        process_problem(api),
        ProcessProblem::UnknownPort("pg".parse().unwrap())
    );
}

#[test]
fn a_plan_refuses_a_oneshot_process_with_a_probe() {
    let mut api = host_app("api");
    api.processes[0].kind = ProcessKind::Oneshot { once: false };

    assert_eq!(process_problem(api), ProcessProblem::OneshotWithProbe);
}

#[test]
fn a_plan_refuses_a_oneshot_process_with_any_restart_policy_but_no() {
    for restart in [
        Restart::Always(RestartLimits::default()),
        Restart::OnFailure(RestartLimits::default()),
    ] {
        let mut api = host_app("api");
        api.processes[0].kind = ProcessKind::Oneshot { once: false };
        api.processes[0].ready = None;
        api.processes[0].port = None;
        api.processes[0].restart = restart;

        assert_eq!(process_problem(api), ProcessProblem::OneshotRestarts);
    }
}

#[test]
fn a_plan_refuses_a_oneshot_process_that_listens_on_a_port() {
    let mut api = host_app("api");
    api.processes[0].kind = ProcessKind::Oneshot { once: true };
    api.processes[0].ready = None;
    api.processes[0].restart = Restart::No;

    assert_eq!(process_problem(api), ProcessProblem::OneshotWithPort);
}

#[test]
fn a_plan_accepts_a_oneshot_without_a_probe_a_port_or_a_restart_policy() {
    let mut api = host_app("api");
    api.processes[0].kind = ProcessKind::Oneshot { once: true };
    api.processes[0].ready = None;
    api.processes[0].restart = Restart::No;
    api.processes[0].port = None;

    assert!(plan(vec![api]).is_ok());
}

#[test]
fn a_plan_with_every_container_and_environment_field_survives_a_trip_through_json() {
    let mut db = container_app("db");
    db.container.mounts.push(BindMount {
        source: PathBuf::from("/workspaces/reading-list/api"),
        target: PathBuf::from("/app"),
        read_only: false,
    });
    db.container.caches.insert(
        "cargo".parse().unwrap(),
        PathBuf::from("/usr/local/cargo/registry"),
    );
    db.container.user = Some("1000:1000".into());
    let mut api = host_app("api");
    api.overrides.insert("LOG_LEVEL".into());
    api.processes[0].cwd = Some(PathBuf::from("server"));
    api.processes[0].env.insert("BIND".into());
    api.processes[0].ready = Some(Probe::new(ProbeCheck::Log {
        pattern: "listening on".into(),
    }));
    let original = plan(vec![api, db]).unwrap();

    let json = serde_json::to_string(&original).unwrap();

    assert_eq!(serde_json::from_str::<Plan>(&json).unwrap(), original);
}

#[test]
fn a_plan_refuses_a_dependency_cycle_and_names_it() {
    let mut api = host_app("api");
    api.depends.push("auth".parse().unwrap());
    let mut auth = host_app("auth");
    auth.depends.push("api".parse().unwrap());

    assert_eq!(
        plan(vec![api, auth]),
        Err(PlanError::DependencyCycle {
            apps: vec![
                "api".parse().unwrap(),
                "auth".parse().unwrap(),
                "api".parse().unwrap(),
            ],
        })
    );
}

#[test]
fn a_plan_refuses_an_app_that_depends_on_itself() {
    let mut api = host_app("api");
    api.depends.push("api".parse().unwrap());

    assert!(matches!(
        plan(vec![api]),
        Err(PlanError::DependencyCycle { .. })
    ));
}

#[test]
fn a_plan_accepts_a_dependency_shared_by_two_apps() {
    let mut api = host_app("api");
    api.depends.push("db".parse().unwrap());
    let mut auth = host_app("auth");
    auth.depends.push("db".parse().unwrap());
    auth.depends.push("api".parse().unwrap());

    assert!(plan(vec![auth, api, container_app("db")]).is_ok());
}

#[test]
fn a_plan_refuses_a_variable_name_that_a_process_cannot_receive() {
    let mut api = host_app("api");
    api.processes[0].env.insert("A=B".into());

    assert_eq!(
        process_problem(api),
        ProcessProblem::BadVariableName("A=B".into())
    );
}

#[test]
fn a_plan_refuses_an_override_name_that_a_process_cannot_receive() {
    let mut api = host_app("api");
    api.overrides.insert(String::new());

    assert_eq!(
        plan(vec![api]),
        Err(PlanError::BadOverride {
            app: "api".parse().unwrap(),
            name: String::new(),
        })
    );
}

#[test]
fn a_plan_refuses_a_working_directory_outside_the_app() {
    let mut api = host_app("api");
    api.processes[0].cwd = Some(PathBuf::from("../other"));
    assert_eq!(process_problem(api), ProcessProblem::CwdOutsideApp);

    let mut api = host_app("api");
    api.processes[0].cwd = Some(PathBuf::from("/tmp"));
    assert_eq!(process_problem(api), ProcessProblem::CwdOutsideApp);
}

#[test]
fn a_plan_refuses_a_log_probe_with_an_empty_pattern() {
    let mut api = host_app("api");
    api.processes[0].ready = Some(Probe::new(ProbeCheck::Log {
        pattern: String::new(),
    }));

    assert_eq!(process_problem(api), ProcessProblem::EmptyLogPattern);
}

#[test]
fn a_plan_refuses_a_log_probe_whose_pattern_is_not_a_regular_expression() {
    let mut api = host_app("api");
    api.processes[0].ready = Some(Probe::new(ProbeCheck::log("ready (".to_owned())));

    let ProcessProblem::BadLogPattern { reason } = process_problem(api) else {
        panic!("the plan was refused for another reason");
    };
    assert!(!reason.is_empty());
    assert!(!reason.contains('\n'), "{reason}");
}

#[test]
fn a_plan_read_from_json_refuses_a_log_probe_whose_pattern_is_not_a_regular_expression() {
    let mut api = host_app("api");
    api.processes[0].ready = Some(Probe::new(ProbeCheck::log("ready".to_owned())));
    let mut json = serde_json::to_value(plan(vec![api]).unwrap()).unwrap();
    json["apps"][0]["processes"][0]["ready"]["check"]["log"]["pattern"] = "ready (".into();

    let refused = serde_json::from_value::<Plan>(json).unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("its log probe's pattern is not a regular expression"),
        "{refused}"
    );
}

#[test]
fn a_plan_refuses_a_published_port_on_a_host_process() {
    let mut api = host_app("api");
    api.processes[0]
        .publish
        .insert("http".parse().unwrap(), NonZeroU16::new(8080).unwrap());

    assert_eq!(process_problem(api), ProcessProblem::ContainerFieldOnHost);
}

#[test]
fn a_plan_refuses_a_published_port_the_app_lacks() {
    let mut db = container_app("db");
    db.processes[0]
        .publish
        .insert("admin".parse().unwrap(), NonZeroU16::new(8080).unwrap());

    assert_eq!(
        process_problem(db),
        ProcessProblem::UnknownPort("admin".parse().unwrap())
    );
}

#[test]
fn a_plan_refuses_a_volume_mounted_at_a_relative_path() {
    let mut db = container_app("db");
    db.processes[0]
        .volumes
        .insert("data".parse().unwrap(), PathBuf::from("data"));

    assert_eq!(
        process_problem(db),
        ProcessProblem::RelativeContainerPath(PathBuf::from("data"))
    );
}

#[test]
fn a_plan_refuses_container_options_on_a_host_app() {
    let mut api = host_app("api");
    api.container.user = Some("1000".into());

    assert_eq!(
        plan(vec![api]),
        Err(PlanError::BadContainer {
            app: "api".parse().unwrap(),
            problem: ContainerProblem::OnHostApp,
        })
    );
}

#[test]
fn a_plan_refuses_a_bind_mount_from_a_relative_host_path() {
    let mut db = container_app("db");
    db.container.mounts.push(BindMount {
        source: PathBuf::from("api"),
        target: PathBuf::from("/app"),
        read_only: true,
    });

    assert_eq!(
        plan(vec![db]),
        Err(PlanError::BadContainer {
            app: "db".parse().unwrap(),
            problem: ContainerProblem::RelativeHostPath(PathBuf::from("api")),
        })
    );
}

#[test]
fn a_plan_refuses_an_empty_run_as_user() {
    let mut db = container_app("db");
    db.container.user = Some(String::new());

    assert_eq!(
        plan(vec![db]),
        Err(PlanError::BadContainer {
            app: "db".parse().unwrap(),
            problem: ContainerProblem::EmptyUser,
        })
    );
}

fn release(spec: ReleaseSpec) -> Source {
    Source::Release {
        version: "v1.2.0".parse().unwrap(),
        spec,
    }
}

fn release_spec() -> ReleaseSpec {
    ReleaseSpec::new(
        "crumbkit/example-api".parse().unwrap(),
        "example-api-{target}.tar.gz".into(),
        PathBuf::from("bin/example-api"),
        Verification::Attestation,
    )
}

fn source_problem(app: AppPlan) -> SourceProblem {
    match plan(vec![app]) {
        Err(PlanError::BadSource { problem, .. }) => problem,
        other => panic!("expected a bad source, got {other:?}"),
    }
}

#[test]
fn a_checkout_app_runs_a_command_from_its_checkout_without_a_build() {
    let mut api = host_app("api");
    api.source = Source::Checkout {
        dir: PathBuf::from("/workspaces/reading-list/web"),
        build: None,
    };
    api.processes[0].run = vec!["npm".into(), "run".into(), "dev".into()];

    assert!(plan(vec![api]).is_ok());
}

#[test]
fn a_plan_refuses_the_program_placeholder_when_the_checkout_has_no_build() {
    let mut api = host_app("api");
    api.source = Source::Checkout {
        dir: PathBuf::from("/workspaces/reading-list/web"),
        build: None,
    };

    assert_eq!(process_problem(api), ProcessProblem::ProgramWithoutBuild);
}

#[test]
fn a_plan_refuses_a_relative_checkout_directory() {
    let mut api = host_app("api");
    api.source = Source::Checkout {
        dir: PathBuf::from("api"),
        build: None,
    };
    api.processes[0].run = vec!["npm".into(), "start".into()];

    assert_eq!(
        source_problem(api),
        SourceProblem::RelativeCheckout(PathBuf::from("api"))
    );
}

#[test]
fn a_plan_refuses_a_build_output_outside_the_checkout() {
    let mut api = host_app("api");
    api.source = Source::Checkout {
        dir: PathBuf::from("/workspaces/reading-list/api"),
        build: Some(BuildSpec::new(
            vec!["make".into()],
            PathBuf::from("../elsewhere/api"),
        )),
    };

    assert_eq!(
        source_problem(api),
        SourceProblem::OutputOutsideCheckout(PathBuf::from("../elsewhere/api"))
    );
}

#[test]
fn a_plan_accepts_a_release_whose_program_lies_inside_its_asset() {
    let mut api = host_app("api");
    api.source = release(release_spec());

    assert!(plan(vec![api]).is_ok());
}

#[test]
fn a_plan_refuses_a_release_program_at_an_absolute_path() {
    let mut spec = release_spec();
    spec.binary = PathBuf::from("/usr/bin/api");
    let mut api = host_app("api");
    api.source = release(spec);

    assert_eq!(
        source_problem(api),
        SourceProblem::BinaryOutsideAsset(PathBuf::from("/usr/bin/api"))
    );
}

#[test]
fn a_plan_refuses_a_release_asset_name_with_a_slash() {
    let mut spec = release_spec();
    spec.asset = "dist/api.tar.gz".into();
    let mut api = host_app("api");
    api.source = release(spec);

    assert_eq!(
        source_problem(api),
        SourceProblem::BadAssetName("dist/api.tar.gz".into())
    );
}

#[test]
fn a_server_may_wait_for_a_oneshot_migration_of_its_app() {
    let mut api = host_app("api");
    let mut migrate = ProcessPlan::new("migrate".parse().unwrap(), vec!["{program}".into()]);
    migrate.kind = ProcessKind::Oneshot { once: false };
    api.processes.insert(0, migrate);
    api.processes[1].depends.push("migrate".parse().unwrap());

    assert!(plan(vec![api]).is_ok());
}

#[test]
fn a_plan_refuses_a_process_dependency_on_a_process_its_app_lacks() {
    let mut api = host_app("api");
    api.processes[0].depends.push("migrate".parse().unwrap());

    assert_eq!(
        process_problem(api),
        ProcessProblem::UnknownDependency("migrate".parse().unwrap())
    );
}

#[test]
fn a_plan_refuses_a_cycle_among_an_apps_processes_and_names_it() {
    let mut api = host_app("api");
    let mut worker = ProcessPlan::new("worker".parse().unwrap(), vec!["{program}".into()]);
    worker.depends.push("server".parse().unwrap());
    api.processes.push(worker);
    api.processes[0].depends.push("worker".parse().unwrap());

    assert_eq!(
        plan(vec![api]),
        Err(PlanError::ProcessCycle {
            app: "api".parse().unwrap(),
            processes: vec![
                "server".parse().unwrap(),
                "worker".parse().unwrap(),
                "server".parse().unwrap(),
            ],
        })
    );
}

#[test]
fn a_plan_refuses_a_process_that_depends_on_itself() {
    let mut api = host_app("api");
    api.processes[0].depends.push("server".parse().unwrap());

    assert!(matches!(
        plan(vec![api]),
        Err(PlanError::ProcessCycle { .. })
    ));
}

#[test]
fn a_plan_refuses_an_override_of_a_secret() {
    let mut api = host_app("api");
    api.permissions
        .get_or_insert_default()
        .secrets
        .insert("STRIPE_KEY".into());
    api.overrides.insert("STRIPE_KEY".into());

    assert_eq!(
        plan(vec![api]),
        Err(PlanError::SecretOverride {
            app: "api".parse().unwrap(),
            name: "STRIPE_KEY".into(),
        })
    );
}

#[test]
fn a_process_waits_up_to_a_minute_to_become_ready_by_default() {
    let server = ProcessPlan::new("server".parse().unwrap(), Vec::new());

    assert_eq!(server.ready_timeout, Duration::from_secs(60));
}

#[test]
fn a_plan_refuses_a_ready_timeout_of_zero_or_longer_than_an_hour() {
    let mut api = host_app("api");
    api.processes[0].ready_timeout = Duration::ZERO;
    assert_eq!(process_problem(api), ProcessProblem::ReadyTimeoutOutOfRange);

    let mut api = host_app("api");
    api.processes[0].ready_timeout = MOST_READY_TIMEOUT + Duration::from_secs(1);
    assert_eq!(process_problem(api), ProcessProblem::ReadyTimeoutOutOfRange);

    let mut api = host_app("api");
    api.processes[0].ready_timeout = MOST_READY_TIMEOUT;
    assert!(plan(vec![api]).is_ok());
}

#[test]
fn a_probe_read_from_json_takes_the_defaults_it_omits() {
    let read: Probe = serde_json::from_str(r#"{"check":{"tcp":{"port":"pg"}}}"#).unwrap();

    assert_eq!(
        read,
        Probe::new(ProbeCheck::Tcp {
            port: "pg".parse().unwrap()
        })
    );
}

#[test]
fn a_plan_writes_its_waits_in_milliseconds_and_a_process_kind_as_plain_fields() {
    let mut api = host_app("api");
    let mut migrate = ProcessPlan::new("migrate".parse().unwrap(), vec!["{program}".into()]);
    migrate.kind = ProcessKind::oneshot(true);
    api.processes.push(migrate);
    let json = serde_json::to_value(plan(vec![api]).unwrap()).unwrap();
    let server = &json["apps"][0]["processes"][0];
    let migrate = &json["apps"][0]["processes"][1];

    assert_eq!(server["ready"]["period_ms"], 10_000);
    assert_eq!(server["ready"]["timeout_ms"], 1000);
    assert_eq!(server["ready_timeout_ms"], 60_000);
    assert_eq!(server["stop"]["grace_ms"], 10_000);
    assert_eq!(migrate["kind"], "oneshot");
    assert_eq!(migrate["once"], true);
    assert_eq!(server["kind"], "long-running");
    assert!(server.get("once").is_none());
}

#[test]
fn a_plan_refuses_an_asset_named_dot_dot_dot_or_nothing() {
    for asset in [".", "..", ""] {
        let mut spec = release_spec();
        spec.asset = asset.into();
        let mut api = host_app("api");
        api.source = release(spec);

        assert_eq!(
            source_problem(api),
            SourceProblem::BadAssetName(asset.into()),
            "{asset:?}"
        );
    }
}

#[test]
fn a_plan_refuses_an_empty_release_program_path() {
    let mut spec = release_spec();
    spec.binary = PathBuf::new();
    let mut api = host_app("api");
    api.source = release(spec);

    assert_eq!(
        source_problem(api),
        SourceProblem::BinaryOutsideAsset(PathBuf::new())
    );
}

#[test]
fn a_plan_refuses_an_empty_build_output_path() {
    let mut api = host_app("api");
    api.source = Source::checkout(
        PathBuf::from("/workspaces/reading-list/api"),
        Some(BuildSpec::new(vec!["make".into()], PathBuf::from("."))),
    );

    assert_eq!(
        source_problem(api),
        SourceProblem::OutputOutsideCheckout(PathBuf::from("."))
    );
}

#[test]
fn a_process_read_from_json_refuses_once_on_a_long_running_process() {
    let one = plan(vec![host_app("api")]).unwrap();
    let mut json = serde_json::to_value(&one).unwrap();
    json["apps"][0]["processes"][0]["once"] = true.into();

    assert!(serde_json::from_value::<Plan>(json).is_err());
}

#[test]
fn a_oneshot_read_from_json_without_once_runs_on_every_up() {
    let mut api = host_app("api");
    api.processes[0].ready = None;
    api.processes[0].port = None;
    api.processes[0].restart = Restart::No;
    let one = plan(vec![api]).unwrap();
    let mut json = serde_json::to_value(&one).unwrap();
    json["apps"][0]["processes"][0]["kind"] = "oneshot".into();
    json["apps"][0]["processes"][0]
        .as_object_mut()
        .unwrap()
        .remove("ready");

    let read: Plan = serde_json::from_value(json).unwrap();

    assert_eq!(
        read.apps()[0].processes[0].kind,
        ProcessKind::Oneshot { once: false }
    );
}

#[test]
fn a_plan_refuses_a_container_probe_on_a_port_the_process_does_not_publish() {
    let mut db = container_app("db");
    db.ports
        .insert("admin".parse().unwrap(), NonZeroU16::new(41_002).unwrap());
    db.processes[0].ready = Some(Probe::new(ProbeCheck::Tcp {
        port: "admin".parse().unwrap(),
    }));
    assert_eq!(
        process_problem(db.clone()),
        ProcessProblem::ProbePortUnpublished("admin".parse().unwrap())
    );

    db.processes[0].port = Some("admin".parse().unwrap());
    db.processes[0].ready = Some(Probe::new(ProbeCheck::http("/health".to_owned())));
    assert_eq!(
        process_problem(db.clone()),
        ProcessProblem::ProbePortUnpublished("admin".parse().unwrap())
    );

    db.processes[0].ready = Some(Probe::new(ProbeCheck::Tcp {
        port: "pg".parse().unwrap(),
    }));
    db.processes[0].port = None;
    assert!(plan(vec![db]).is_ok());
}

#[test]
fn a_plan_refuses_a_container_tcp_probe_whose_timeout_is_under_a_second() {
    let mut db = container_app("db");
    let mut probe = Probe::new(ProbeCheck::Tcp {
        port: "pg".parse().unwrap(),
    });
    probe.timeout = Duration::from_millis(999);
    db.processes[0].ready = Some(probe.clone());
    assert_eq!(
        process_problem(db.clone()),
        ProcessProblem::PublishedProbeTimeoutTooShort
    );
    probe.timeout = Duration::from_secs(60);
    db.processes[0].ready = Some(probe);
    assert!(plan(vec![db]).is_ok());

    let mut api = host_app("api");
    let mut probe = Probe::new(ProbeCheck::Tcp {
        port: "http".parse().unwrap(),
    });
    probe.timeout = Duration::from_millis(100);
    api.processes[0].ready = Some(probe);
    assert!(plan(vec![api]).is_ok());
}

#[test]
fn a_plan_refuses_a_stop_grace_longer_than_an_hour() {
    let mut api = host_app("api");
    api.processes[0].stop.grace = Duration::from_secs(3601);
    assert_eq!(
        process_problem(api.clone()),
        ProcessProblem::StopGraceOutOfRange
    );
    api.processes[0].stop.grace = Duration::from_secs(3600);
    assert!(plan(vec![api]).is_ok());
}

#[test]
fn a_process_receives_its_container_secrets_as_files_unless_it_opts_into_the_environment() {
    let mut db = container_app("db");
    assert_eq!(db.processes[0].secret_delivery, SecretDelivery::File);
    db.processes[0].secret_delivery = SecretDelivery::Env;
    let original = plan(vec![db, host_app("api")]).unwrap();

    let json = serde_json::to_value(&original).unwrap();

    assert_eq!(json["apps"][0]["processes"][0]["secret_delivery"], "env");
    assert!(
        json["apps"][1]["processes"][0]
            .get("secret_delivery")
            .is_none()
    );
    assert_eq!(serde_json::from_value::<Plan>(json).unwrap(), original);
}

#[test]
fn a_plan_refuses_environment_secret_delivery_on_a_host_process() {
    let mut api = host_app("api");
    api.processes[0].secret_delivery = SecretDelivery::Env;

    assert_eq!(process_problem(api), ProcessProblem::ContainerFieldOnHost);
}

#[test]
fn a_process_names_the_owner_of_its_secret_files_as_numeric_uid_and_gid() {
    let mut db = container_app("db");
    db.processes[0].secret_owner = Some("999:999".to_owned());
    let original = plan(vec![db, host_app("api")]).unwrap();

    let json = serde_json::to_value(&original).unwrap();

    assert_eq!(json["apps"][0]["processes"][0]["secret_owner"], "999:999");
    assert!(
        json["apps"][1]["processes"][0]
            .get("secret_owner")
            .is_none()
    );
    assert_eq!(serde_json::from_value::<Plan>(json).unwrap(), original);
}

#[test]
fn a_plan_refuses_a_secret_owner_that_is_not_a_numeric_uid_and_gid() {
    for owner in [
        "postgres",
        "999",
        "999:",
        ":999",
        "999:999:1",
        "-1:0",
        " 1:1",
    ] {
        let mut db = container_app("db");
        db.processes[0].secret_owner = Some(owner.to_owned());

        assert_eq!(
            process_problem(db),
            ProcessProblem::BadSecretOwner(owner.to_owned()),
            "{owner}"
        );
    }
}

#[test]
fn a_plan_refuses_a_secret_owner_on_a_host_process() {
    let mut api = host_app("api");
    api.processes[0].secret_owner = Some("1000:1000".to_owned());

    assert_eq!(process_problem(api), ProcessProblem::ContainerFieldOnHost);
}

#[test]
fn an_optional_variable_survives_a_trip_through_json_and_an_empty_set_is_left_out() {
    let mut api = host_app("api");
    api.processes[0].env.insert("LANG".into());
    api.processes[0].env.insert("PORT".into());
    api.processes[0].optional_env.insert("LANG".into());
    let original = plan(vec![api]).unwrap();

    let json = serde_json::to_value(&original).unwrap();

    assert_eq!(
        json["apps"][0]["processes"][0]["optional_env"],
        serde_json::json!(["LANG"])
    );
    assert_eq!(serde_json::from_value::<Plan>(json).unwrap(), original);
    let plain = serde_json::to_value(plan(vec![host_app("api")]).unwrap()).unwrap();
    assert!(
        plain["apps"][0]["processes"][0]
            .get("optional_env")
            .is_none(),
        "{plain}"
    );
}

#[test]
fn a_plan_refuses_an_optional_variable_that_the_process_does_not_receive() {
    let mut api = host_app("api");
    api.processes[0].optional_env.insert("LANG".into());

    assert_eq!(
        process_problem(api),
        ProcessProblem::UndeclaredOptional("LANG".into())
    );
}

#[test]
fn the_plan_digest_covers_the_optional_flag_and_holds_no_value() {
    let mut required = host_app("api");
    required.processes[0].env.insert("LANG".into());
    let mut optional = required.clone();
    optional.processes[0].optional_env.insert("LANG".into());

    let required = plan(vec![required]).unwrap();
    let optional = plan(vec![optional]).unwrap();

    assert_ne!(required.digest(), optional.digest());
    assert_eq!(optional.digest(), optional.clone().digest());
}

fn sandboxed(mut app: AppPlan) -> AppPlan {
    app.permissions
        .get_or_insert_default()
        .listen
        .insert("http".parse().unwrap());
    app
}

fn permission_problem(apps: Vec<AppPlan>) -> PermissionProblem {
    match plan(apps) {
        Err(PlanError::BadPermissions { problem, .. }) => problem,
        other => panic!("expected bad permissions, got {other:?}"),
    }
}

#[test]
fn an_app_without_permissions_writes_them_as_null_so_the_plan_hash_records_it_unsandboxed() {
    let unsandboxed = plan(vec![host_app("api")]).unwrap();
    let declared = plan(vec![sandboxed(host_app("api"))]).unwrap();

    let json = serde_json::to_value(&unsandboxed).unwrap();

    assert!(json["apps"][0]["permissions"].is_null());
    assert_ne!(unsandboxed.digest(), declared.digest());
}

#[test]
fn a_plan_refuses_an_app_that_may_connect_to_an_external_host_until_the_proxy_is_built() {
    let mut api = sandboxed(host_app("api"));
    api.permissions
        .get_or_insert_default()
        .connect
        .insert("api.stripe.com".into());

    let problem = permission_problem(vec![api]);

    assert_eq!(
        problem,
        PermissionProblem::ExternalHost("api.stripe.com".into())
    );
    assert!(problem.to_string().contains("proxy"), "{problem}");
}

#[test]
fn a_plan_refuses_an_app_that_may_connect_to_an_app_it_does_not_depend_on() {
    let mut api = sandboxed(host_app("api"));
    api.permissions
        .get_or_insert_default()
        .connect
        .insert("auth".into());

    assert_eq!(
        permission_problem(vec![api, host_app("auth")]),
        PermissionProblem::NotADependency("auth".parse().unwrap())
    );
}

#[test]
fn a_plan_accepts_an_app_that_may_connect_to_an_app_it_depends_on() {
    let mut api = sandboxed(host_app("api"));
    api.depends.push("auth".parse().unwrap());
    api.permissions
        .get_or_insert_default()
        .connect
        .insert("auth".into());

    assert!(plan(vec![api, host_app("auth")]).is_ok());
}

#[test]
fn a_plan_refuses_a_sandboxed_process_that_listens_on_a_port_its_app_may_not_listen_on() {
    let mut api = host_app("api");
    api.permissions = Some(super::Permissions::default());

    assert_eq!(
        permission_problem(vec![api]),
        PermissionProblem::ListenNotGranted {
            process: "server".parse().unwrap(),
            port: "http".parse().unwrap(),
        }
    );
}

#[test]
fn a_plan_refuses_a_write_path_that_leaves_the_checkout() {
    for path in ["../other", "", "$HOME/x", "target/../../x", "/srv/../etc"] {
        let mut api = sandboxed(host_app("api"));
        api.permissions
            .get_or_insert_default()
            .write
            .insert(path.into());

        assert_eq!(
            permission_problem(vec![api]),
            PermissionProblem::BadWrite(path.into()),
            "{path}"
        );
    }
}

#[test]
fn a_plan_accepts_the_state_directory_and_relative_and_absolute_paths_inside_the_checkout_as_writes()
 {
    let mut api = sandboxed(host_app("api"));
    api.permissions.get_or_insert_default().write =
        ["$STATE", "/workspaces/reading-list/api/cache", "target"]
            .into_iter()
            .map(String::from)
            .collect();

    assert!(plan(vec![api]).is_ok());
}

#[test]
fn a_plan_refuses_an_absolute_write_path_outside_the_checkout() {
    for path in [
        "/srv/cache",
        "/workspaces/reading-list/web",
        "/workspaces/reading-list/api-other",
    ] {
        let mut api = sandboxed(host_app("api"));
        api.permissions.get_or_insert_default().write = [path.to_owned()].into();

        assert_eq!(
            permission_problem(vec![api]),
            PermissionProblem::WriteOutsideCheckout(path.to_owned()),
            "{path}"
        );
    }
}

#[test]
fn a_plan_leaves_the_permissions_of_a_container_app_to_docker() {
    let mut db = container_app("db");
    db.permissions
        .get_or_insert_default()
        .connect
        .insert("api.stripe.com".into());

    assert!(plan(vec![db]).is_ok());
}

#[test]
fn a_plan_refuses_a_write_path_inside_another_write_path() {
    let mut api = sandboxed(host_app("api"));
    api.permissions.get_or_insert_default().write = ["target", "target/debug"]
        .into_iter()
        .map(String::from)
        .collect();

    assert_eq!(
        permission_problem(vec![api]),
        PermissionProblem::NestedWrite {
            outer: "target".to_owned(),
            inner: "target/debug".to_owned(),
        }
    );
}

#[test]
fn a_plan_refuses_an_absolute_write_path_inside_the_checkouts_write_path() {
    let mut api = sandboxed(host_app("api"));
    let crate::source::Source::Checkout { dir, .. } = &api.source else {
        panic!("a host app runs from its checkout");
    };
    let inner = dir.join("target/debug").display().to_string();
    api.permissions.get_or_insert_default().write = ["target".to_owned(), inner.clone()].into();

    assert_eq!(
        permission_problem(vec![api]),
        PermissionProblem::NestedWrite {
            outer: "target".to_owned(),
            inner,
        }
    );
}

#[test]
fn a_plan_refuses_two_apps_that_may_write_overlapping_paths() {
    // Both apps run from one checkout.
    let mut api = sandboxed(host_app("api"));
    api.permissions.get_or_insert_default().write = ["target".to_owned()].into();
    let mut web = sandboxed(host_app("web"));
    web.permissions.get_or_insert_default().write = ["target/web".to_owned()].into();

    assert_eq!(
        plan(vec![api, web]).unwrap_err(),
        PlanError::BadPermissions {
            app: "api".parse().unwrap(),
            problem: PermissionProblem::SharedWrite {
                path: "target".to_owned(),
                other: "web".parse().unwrap(),
            },
        }
    );
}

#[test]
fn a_plan_accepts_two_apps_of_one_checkout_that_both_write_the_same_path() {
    // Neither app may write the checkout that holds `target`, so neither
    // can replace `target` with a symbolic link.
    let mut api = sandboxed(host_app("api"));
    api.permissions.get_or_insert_default().write = ["target".to_owned()].into();
    let mut web = sandboxed(host_app("web"));
    web.permissions.get_or_insert_default().write = ["target".to_owned()].into();

    assert!(plan(vec![api, web]).is_ok());
}

#[test]
fn a_plan_refuses_two_apps_whose_writes_nest_through_a_symbolic_link() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("api");
    std::fs::create_dir_all(real.join("target")).unwrap();
    let link = root.path().join("web");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let with_checkout = |name: &str, dir: &std::path::Path, write: &str| {
        let mut app = sandboxed(host_app(name));
        if let Source::Checkout { dir: checkout, .. } = &mut app.source {
            *checkout = dir.to_path_buf();
        }
        app.permissions.get_or_insert_default().write = [write.to_owned()].into();
        app
    };

    assert_eq!(
        plan(vec![
            with_checkout("api", &real, "target"),
            with_checkout("web", &link, "target/web")
        ])
        .unwrap_err(),
        PlanError::BadPermissions {
            app: "api".parse().unwrap(),
            problem: PermissionProblem::SharedWrite {
                path: "target".to_owned(),
                other: "web".parse().unwrap(),
            },
        }
    );
}

#[test]
fn a_plan_accepts_an_absolute_write_path_spelled_through_the_real_path_of_a_linked_checkout() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("api");
    std::fs::create_dir_all(real.join("cache")).unwrap();
    let link = root.path().join("api-link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let mut api = sandboxed(host_app("api"));
    if let Source::Checkout { dir, .. } = &mut api.source {
        *dir = link.clone();
    }
    let entry = real.canonicalize().unwrap().join("cache");
    api.permissions.get_or_insert_default().write = [entry.display().to_string()].into();

    assert!(plan(vec![api]).is_ok());
}

#[test]
fn a_process_digest_changes_with_its_command_and_not_with_another_process() {
    let mut api = host_app("api");
    let mut migrate = ProcessPlan::new("migrate".parse().unwrap(), vec!["./migrate".into()]);
    migrate.kind = ProcessKind::oneshot(true);
    api.processes.push(migrate);
    let before = api.process_digest(&api.processes[1]);

    let mut other = api.clone();
    other.processes[0].run.push("--verbose".into());
    assert_eq!(other.process_digest(&other.processes[1]), before);

    let mut changed = api.clone();
    changed.processes[1].run.push("--seed".into());
    assert_ne!(changed.process_digest(&changed.processes[1]), before);

    let mut sandboxed = api;
    sandboxed.permissions = Some(super::Permissions::default());
    assert_ne!(sandboxed.process_digest(&sandboxed.processes[1]), before);
}
