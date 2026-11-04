use std::num::NonZeroU16;
use std::path::PathBuf;

use super::probe::{Probe, ProbeCheck};
use super::restart::{Restart, RestartLimits};
use super::{AppPlan, Plan, PlanError, ProcessKind, ProcessPlan, ProcessProblem, RunnerKind};
use crate::source::{BuildSpec, Source};

fn checkout() -> Source {
    Source::Checkout {
        dir: PathBuf::from("/workspaces/reading-list/api"),
        build: BuildSpec::new(
            vec!["cargo".into(), "build".into(), "--release".into()],
            PathBuf::from("target/release/api"),
        ),
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
    api.permissions.listen.insert("http".parse().unwrap());
    api.permissions.secrets.insert("STRIPE_KEY".into());
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
    api.permissions.listen.insert("admin".parse().unwrap());

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
fn a_plan_refuses_a_oneshot_process_that_restarts_always() {
    let mut api = host_app("api");
    api.processes[0].kind = ProcessKind::Oneshot { once: false };
    api.processes[0].ready = None;
    api.processes[0].restart = Restart::Always(RestartLimits::default());

    assert_eq!(process_problem(api), ProcessProblem::OneshotRestartsAlways);
}
