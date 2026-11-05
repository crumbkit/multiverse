use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;

use crate::cancel::Cancel;
use crate::clock::now;
use crate::engine::container::ContainerError as Error;
use crate::engine::container::command::{
    ChildEnv as _, Referenced, message, run_program_until, says_missing,
};
use crate::engine::container::compose::{
    ComposeValue, ContainerSpec, ContainerSpecs, PublishedPort, UpFailure, compose_env,
    compose_file, up_failure,
};
use crate::engine::container::names::{
    ContainerId, Identity, Scope, ServiceName, StoreId, VolumeKey, VolumeName,
};
use crate::engine::container::objects::{
    ContainerState, STOP_TIMEOUT_FORMAT, mint_anonymous, mint_container, mint_volume,
    stop_timeout_of,
};
use crate::name::WorkspaceName;

/// A store id of the tests.
const STORE: &str = "0123456789abcdef0123456789abcdef";

/// A container id of the tests.
const ID: &str = "4f1c0a8e9b7d6c5a4f1c0a8e9b7d6c5a4f1c0a8e9b7d6c5a4f1c0a8e9b7d6c5a";

fn service(name: &str) -> ServiceName {
    ServiceName::new(name).unwrap()
}

fn key(name: &str) -> VolumeKey {
    VolumeKey::new(name).unwrap()
}

/// Returns an object's labels as `docker inspect` prints them with
/// `{{json .Labels}}`: the store, workspace and `label` labels, where an
/// empty value leaves its label out.
fn printed_labels(store: &str, workspace: &str, label: &str, object: &str) -> String {
    let mut labels = serde_json::Map::new();
    for (key, value) in [
        ("mxv.store", store),
        ("mxv.workspace", workspace),
        (label, object),
    ] {
        if !value.is_empty() {
            labels.insert(key.to_owned(), json!(value));
        }
    }
    serde_json::Value::Object(labels).to_string()
}

/// Returns the labels of the scope's container `service`.
fn container_labels(store: &str, workspace: &str, service: &str) -> String {
    printed_labels(store, workspace, "mxv.container", service)
}

/// Returns the labels of the scope's volume `volume`.
fn volume_labels(store: &str, workspace: &str, volume: &str) -> String {
    printed_labels(store, workspace, "mxv.volume", volume)
}

fn scope(workspace: &str) -> Scope {
    Scope::new(
        &StoreId::new(STORE).unwrap(),
        &WorkspaceName::try_from(workspace.to_owned()).unwrap(),
    )
}

fn variables(pairs: &[(&str, &str)]) -> BTreeMap<String, ComposeValue> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), ComposeValue::new((*value).to_owned())))
        .collect()
}

/// Returns the services of the tests: `dynamodb`, which publishes a port
/// and mounts a volume, and `cache`.
fn specs() -> ContainerSpecs {
    let mut dynamodb = ContainerSpec::new("amazon/dynamodb-local:2.5.4".to_owned());
    dynamodb.command = Some(vec![
        "sh".to_owned(),
        "-c".to_owned(),
        "echo $HOME".to_owned(),
    ]);
    dynamodb.env = variables(&[("AWS_REGION", "eu-west-1"), ("TOKEN", "s3cret$x")]);
    dynamodb.ports = vec![
        PublishedPort::new(8000, "dynamodb".to_owned()),
        PublishedPort::new(9000, "unused".to_owned()),
    ];
    dynamodb.volumes = BTreeMap::from([(key("data"), "/data".to_owned())]);
    let mut cache = ContainerSpec::new("busybox:1.37.0".to_owned());
    cache.env = variables(&[("TOKEN", "other")]);
    BTreeMap::from([(service("dynamodb"), dynamodb), (service("cache"), cache)])
}

#[test]
fn a_scope_with_the_workspaces_identity_adds_its_stable_id_run_record_and_app_to_every_label() {
    let identity = Identity {
        id: "fedcba9876543210fedcba9876543210".parse().unwrap(),
        run_record: PathBuf::from("/state/runs/fedcba9876543210fedcba9876543210.json"),
    };
    let scope = scope("reading-list").identified(identity);
    let file = compose_file(&scope, &specs(), &BTreeMap::new());
    let expected = |label: &str, object: &str| {
        json!({
            "mxv.store": STORE,
            "mxv.workspace": "reading-list",
            label: object,
            "mxv.workspace-id": "fedcba9876543210fedcba9876543210",
            "mxv.run-record": "/state/runs/fedcba9876543210fedcba9876543210.json",
            "mxv.app": "reading-list",
        })
    };
    assert_eq!(
        file["services"]["cache"]["labels"],
        expected("mxv.container", "cache")
    );
    assert_eq!(
        file["volumes"]["data"]["labels"],
        expected("mxv.volume", "data")
    );
    // The identity labels the objects that the scope creates, and the
    // scope still owns the objects that carry only the older labels.
    assert!(scope.owns(
        &container_labels(STORE, "reading-list", "db"),
        "mxv.container",
        "db"
    ));
    assert_eq!(scope, self::scope("reading-list"));
}

#[test]
fn two_workspaces_whose_names_join_alike_never_share_a_container_or_volume_name() {
    assert_ne!(
        scope("reading-list").container_name(&service("db")),
        scope("reading").container_name(&service("list-db"))
    );
    assert_eq!(
        scope("reading-list").container_name(&service("db")),
        "mxv-01234567-reading-list.db"
    );
    assert_eq!(
        scope("reading-list").volume_name(&key("data")),
        "mxv-01234567-reading-list.data"
    );
}

#[test]
fn two_stores_give_one_workspace_name_different_container_and_volume_names() {
    let other = Scope::new(
        &StoreId::new("fedcba9876543210fedcba9876543210").unwrap(),
        &WorkspaceName::try_from("reading-list".to_owned()).unwrap(),
    );
    assert_ne!(
        other.container_name(&service("db")),
        scope("reading-list").container_name(&service("db"))
    );
    assert_ne!(
        other.volume_name(&key("data")),
        scope("reading-list").volume_name(&key("data"))
    );
}

#[test]
fn a_scope_owns_only_objects_whose_labels_name_its_store_workspace_and_object() {
    let scope = scope("reading-list");
    let owns = |labels: &str| scope.owns(labels, "mxv.container", "db");
    assert!(owns(&container_labels(STORE, "reading-list", "db")));
    assert!(!owns("null"));
    assert!(!owns("{}"));
    assert!(!owns(""));
    assert!(!owns(&container_labels(STORE, "reading-list", "cache")));
    assert!(!owns(&container_labels(STORE, "reading", "db")));
    assert!(!owns(&container_labels(
        "fedcba9876543210fedcba9876543210",
        "reading-list",
        "db"
    )));
    assert!(!owns(&printed_labels(
        STORE,
        "reading-list",
        "mxv.volume",
        "db"
    )));
}

#[test]
fn a_scope_owns_no_object_whose_label_values_differ_by_a_tab_or_a_space() {
    let scope = scope("reading-list");
    let owns = |labels: &str| scope.owns(labels, "mxv.container", "db");
    // A value that holds tabs cannot stand in for the other labels.
    assert!(!owns(&printed_labels(
        &format!("{STORE}\treading-list\tdb"),
        "",
        "mxv.container",
        ""
    )));
    assert!(!owns(&container_labels(
        &format!("{STORE} "),
        "reading-list",
        "db"
    )));
    assert!(!owns(&container_labels(STORE, " reading-list", "db")));
    assert!(!owns(&container_labels(STORE, "reading-list", "db\n")));
}

#[test]
fn the_compose_file_publishes_on_the_loopback_address_labels_every_object_and_holds_no_value_of_a_variable()
 {
    let specs = specs();
    let scope = scope("reading-list");
    let ports = BTreeMap::from([("dynamodb".to_owned(), 24001)]);
    let file = compose_file(&scope, &specs, &ports);
    let labels = |label: &str, object: &str| json!({"mxv.store": STORE, "mxv.workspace": "reading-list", label: object});
    assert_eq!(
        file,
        json!({
            "services": {
                "cache": {
                    "image": "busybox:1.37.0",
                    "container_name": "mxv-01234567-reading-list.cache",
                    "network_mode": "bridge",
                    "labels": labels("mxv.container", "cache"),
                    "environment": {"TOKEN": "${MXV_ENV_0_0:-}"},
                    "ports": [],
                    "volumes": [],
                },
                "dynamodb": {
                    "image": "amazon/dynamodb-local:2.5.4",
                    "container_name": "mxv-01234567-reading-list.dynamodb",
                    "network_mode": "bridge",
                    "labels": labels("mxv.container", "dynamodb"),
                    "command": ["sh", "-c", "echo $$HOME"],
                    "environment": {
                        "AWS_REGION": "${MXV_ENV_1_0:-}",
                        "TOKEN": "${MXV_ENV_1_1:-}",
                    },
                    "ports": ["127.0.0.1:24001:8000/tcp"],
                    "volumes": [{"type": "volume", "source": "data", "target": "/data"}],
                },
            },
            "volumes": {
                "data": {
                    "name": "mxv-01234567-reading-list.data",
                    "labels": labels("mxv.volume", "data"),
                },
            },
        })
    );
    assert!(!file.to_string().contains("s3cret"));
    assert_eq!(
        compose_env(&specs, &service("dynamodb")).exposed(),
        [("MXV_ENV_1_0", "eu-west-1"), ("MXV_ENV_1_1", "s3cret$x")]
    );
    assert_eq!(
        compose_env(&specs, &service("cache")).exposed(),
        [("MXV_ENV_0_0", "other")]
    );
}

#[test]
fn a_workspace_name_with_an_underscore_or_a_dot_gives_a_project_name_that_compose_accepts_and_no_other_workspace_shares()
 {
    assert_eq!(scope("reading-list").project(), "mxv-01234567-reading-list");
    assert_eq!(scope("release.2").project(), "mxv-01234567-release_d2");
    assert_eq!(scope("release_d2").project(), "mxv-01234567-release__d2");
    assert_eq!(scope("a_b").project(), "mxv-01234567-a__b");
}

#[test]
fn docker_states_parse_into_dockers_exact_status_or_exited_with_dockers_code() {
    assert_eq!(
        ContainerState::parse("running 0\n"),
        ContainerState::Running
    );
    assert_eq!(ContainerState::parse("paused 0"), ContainerState::Paused);
    assert_eq!(
        ContainerState::parse("restarting 1"),
        ContainerState::Restarting
    );
    assert_eq!(ContainerState::parse("created 0"), ContainerState::Created);
    assert_eq!(
        ContainerState::parse("exited 137"),
        ContainerState::Exited(137)
    );
    assert_eq!(ContainerState::parse("dead 3"), ContainerState::Exited(3));
    assert_eq!(ContainerState::parse("removing 0"), ContainerState::Unknown);
    assert_eq!(ContainerState::parse("exited"), ContainerState::Unknown);
    assert_eq!(ContainerState::parse("exited x"), ContainerState::Unknown);
    assert_eq!(ContainerState::parse("frozen 0"), ContainerState::Unknown);
    assert_eq!(ContainerState::parse(""), ContainerState::Unknown);
}

#[test]
fn only_a_running_or_paused_container_takes_a_stop_signal_and_a_restarting_one_is_up() {
    let states = [
        ContainerState::Running,
        ContainerState::Paused,
        ContainerState::Restarting,
        ContainerState::Created,
        ContainerState::Exited(0),
        ContainerState::Unknown,
    ];
    let runs: Vec<bool> = states.iter().map(|state| state.runs()).collect();
    assert_eq!(runs, [true, true, false, false, false, false]);
    let up: Vec<bool> = states.iter().map(|state| state.is_up()).collect();
    assert_eq!(up, [true, true, true, false, false, false]);
}

#[test]
fn a_workspace_name_with_a_dot_still_splits_from_the_container_name_at_the_dot() {
    // A project name never holds `.`, so `mxv-01234567-release_d2.db` is
    // only the workspace `release.2` with the container `db`.
    assert_eq!(
        scope("release.2").container_name(&service("db")),
        "mxv-01234567-release_d2.db"
    );
    assert_ne!(
        scope("release.2").container_name(&service("db")),
        scope("release").container_name(&service("2-db"))
    );
}

#[test]
fn only_the_daemons_answer_that_an_object_does_not_exist_counts_as_missing() {
    // What docker 29 prints for each kind of object that its daemon does
    // not have.
    for said in [
        "Error response from daemon: No such container: phase-nope",
        "Error response from daemon: get phase-nope: no such volume",
        "Error response from daemon: network phase-nope not found",
        "error: no such object: phase-nope",
        "Error: No such object: phase-nope",
        // `docker image inspect` of a tag that docker 29.8.2 does not have.
        "Error response from daemon: No such image: busybox:0.0.0-nope",
        // Other docker CLI versions print a missing image this way.
        "Error: No such image: busybox:0.0.0-nope",
    ] {
        assert!(says_missing(said), "{said}");
    }
    // A failure that is no answer about the object never counts as
    // missing, even when it says "not found".
    for said in [
        "Failed to initialize: unable to resolve docker endpoint: context \"nope\": context not found: open /home/agent/.docker/contexts/meta/0/meta.json: no such file or directory",
        "context \"nope\": context not found",
        "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?",
        "error during connect: Get \"http://%2Fvar%2Frun%2Fdocker.sock/v1.47/containers/x/json\": dial unix /var/run/docker.sock: connect: no such file or directory",
        "Error response from daemon: conflict: phase-nope is in use",
        "docker: unknown command: docker volume inspect: not found",
        "",
    ] {
        assert!(!says_missing(said), "{said}");
    }
}

#[test]
fn the_message_of_a_failed_docker_command_is_its_first_docker_or_error_line() {
    assert_eq!(
        message(
            "Unable to find image 'busybox:0' locally\ndocker: Error response from daemon: manifest unknown\n\nRun 'docker run --help' for more information\n"
        ),
        "docker: Error response from daemon: manifest unknown"
    );
    assert_eq!(
        message("Error response from daemon: No such container: x\nmore\n"),
        "Error response from daemon: No such container: x"
    );
    assert_eq!(message("first\nlast\n\n"), "last");
    assert_eq!(message(""), "");
}

#[test]
fn a_compose_up_that_finds_its_host_port_allocated_names_the_port() {
    assert_eq!(
        up_failure(
            "Container mxv-01234567-a.web  Starting\nError response from daemon: driver failed programming external connectivity on endpoint mxv-01234567-a.web (abc): Bind for 127.0.0.1:31005 failed: port is already allocated\n"
        ),
        UpFailure::PortAllocated(Some(31005))
    );
    assert_eq!(
        up_failure("Error response from daemon: port is already allocated\n"),
        UpFailure::PortAllocated(None)
    );
}

#[test]
fn a_compose_up_that_finds_no_free_address_pool_says_so() {
    assert_eq!(
        up_failure(
            "Error response from daemon: all predefined address pools have been fully subnetted\n"
        ),
        UpFailure::NoAddressPool
    );
    assert_eq!(up_failure("Error manifest unknown\n"), UpFailure::Other);
}

#[test]
fn a_service_name_is_a_manifest_name_and_never_holds_a_dot() {
    for good in ["db", "web-1", "a_b", "0cache"] {
        assert_eq!(ServiceName::new(good).unwrap(), good);
    }
    for bad in ["", "-db", "_db", "a.b", "a b", "db/1", "Ä", &"a".repeat(65)] {
        assert!(ServiceName::new(bad).is_err(), "{bad:?}");
    }
    assert!(ServiceName::new(&"a".repeat(64)).is_ok());
}

#[test]
fn a_volume_name_is_a_name_that_docker_accepts_and_never_reads_as_an_option() {
    for good in [
        "mxv-01234567-reading-list.data",
        "0f9e8d7c6b5a49382716051f0e9d8c7b6a594837261504f3e2d1c0b9a8978675",
        "ab",
    ] {
        assert_eq!(VolumeName::new(good).unwrap(), good);
    }
    for bad in ["", "a", "-ab", "--help", ".ab", "a b", "a/b", "a:b"] {
        assert!(VolumeName::new(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_container_id_is_64_lowercase_hex_digits() {
    assert_eq!(ContainerId::new(ID).unwrap(), ID);
    for bad in [
        "",
        "4f1c0a8e9b7d",
        &ID.to_uppercase(),
        &format!("{ID}0"),
        &format!("-{}", &ID[1..]),
        &format!("g{}", &ID[1..]),
        "mxv-01234567-reading-list.db",
    ] {
        assert!(ContainerId::new(bad).is_err(), "{bad:?}");
    }
}

/// Returns what `docker container inspect` prints for a container whose
/// labels print as `labels`.
fn printed_container(labels: &str) -> String {
    format!(
        "{labels}\n{ID}\nexited 3\n2026-11-04T10:00:00.000000000Z\nmxv-01234567-reading-list.data 0f9e8d7c6b5a49382716051f0e9d8c7b6a594837261504f3e2d1c0b9a8978675 \n"
    )
}

#[test]
fn a_container_handle_is_minted_for_labels_that_name_its_store_workspace_and_service() {
    let scope = scope("reading-list");
    let inspected = mint_container(
        &scope,
        &service("db"),
        ID,
        &printed_container(&container_labels(STORE, "reading-list", "db")),
    )
    .unwrap();
    assert_eq!(inspected.container.service(), &service("db"));
    assert_eq!(inspected.container.id(), &ContainerId::new(ID).unwrap());
    assert_eq!(inspected.state, ContainerState::Exited(3));
    assert_eq!(inspected.started, "2026-11-04T10:00:00.000000000Z");
    let anonymous: Vec<&VolumeName> = inspected
        .anonymous
        .iter()
        .map(|volume| volume.name())
        .collect();
    assert_eq!(
        anonymous,
        [
            &VolumeName::new("0f9e8d7c6b5a49382716051f0e9d8c7b6a594837261504f3e2d1c0b9a8978675")
                .unwrap()
        ]
    );
}

#[test]
fn no_container_handle_is_minted_for_labels_of_another_store_workspace_or_service() {
    let scope = scope("reading-list");
    for labels in [
        "null".to_owned(),
        container_labels(STORE, "reading-list", "cache"),
        container_labels(STORE, "reading", "db"),
        container_labels("fedcba9876543210fedcba9876543210", "reading-list", "db"),
    ] {
        let minted = mint_container(
            &scope,
            &service("db"),
            "mxv-01234567-reading-list.db",
            &printed_container(&labels),
        );
        assert!(
            matches!(
                &minted,
                Err(Error::Foreign { kind: "container", name })
                    if name == "mxv-01234567-reading-list.db"
            ),
            "{labels:?}: {minted:?}"
        );
    }
}

#[test]
fn no_container_handle_is_minted_when_docker_prints_no_container_id() {
    let printed = format!(
        "{}\nnot-an-id\nrunning 0\n\n\n",
        container_labels(STORE, "reading-list", "db")
    );
    let minted = mint_container(&scope("reading-list"), &service("db"), ID, &printed);
    assert!(
        matches!(
            &minted,
            Err(Error::Failed {
                operation: "container inspect",
                ..
            })
        ),
        "{minted:?}"
    );
}

#[test]
fn a_named_volume_handle_is_minted_only_for_labels_that_name_its_store_workspace_and_volume() {
    let scope = scope("reading-list");
    let name = scope.volume_name(&key("data"));
    let printed = |labels: String| format!("{labels}\n");
    assert!(
        mint_volume(
            &scope,
            &name,
            &printed(volume_labels(STORE, "reading-list", "data"))
        )
        .is_ok()
    );
    for labels in [
        printed("null".to_owned()),
        printed(volume_labels(STORE, "reading-list", "other")),
        printed(volume_labels(STORE, "reading", "data")),
        printed(volume_labels(
            "fedcba9876543210fedcba9876543210",
            "reading-list",
            "data",
        )),
        printed(container_labels(STORE, "reading-list", "data")),
    ] {
        let minted = mint_volume(&scope, &name, &labels);
        assert!(
            matches!(
                &minted,
                Err(Error::Foreign { kind: "volume", name })
                    if name == "mxv-01234567-reading-list.data"
            ),
            "{labels:?}: {minted:?}"
        );
    }
}

#[test]
fn no_named_volume_handle_is_minted_for_a_volume_without_a_name_of_the_scope() {
    let scope = scope("reading-list");
    for name in [
        "0f9e8d7c6b5a49382716051f0e9d8c7b6a594837261504f3e2d1c0b9a8978675",
        "mxv-01234567-reading.data",
        "mxv-fedcba98-reading-list.data",
    ] {
        let name = VolumeName::new(name).unwrap();
        // Labels that would name the volume do not make another volume
        // the scope's.
        let minted = mint_volume(
            &scope,
            &name,
            &format!("{}\n", volume_labels(STORE, "reading-list", "data")),
        );
        assert!(
            matches!(&minted, Err(Error::Foreign { kind: "volume", .. })),
            "{name}: {minted:?}"
        );
    }
}

#[test]
fn an_anonymous_volume_handle_is_minted_only_for_a_volume_with_dockers_anonymous_label() {
    let name = VolumeName::new("0f9e8d7c6b5a49382716051f0e9d8c7b6a594837261504f3e2d1c0b9a8978675")
        .unwrap();
    assert_eq!(mint_anonymous(&name, "anonymous\n").unwrap().name(), &name);
    for printed in ["", "\n", "named\n"] {
        assert!(
            matches!(
                mint_anonymous(&name, printed),
                Err(Error::Foreign { kind: "volume", .. })
            ),
            "{printed:?}"
        );
    }
}

#[test]
fn a_store_id_is_32_lowercase_hex_digits() {
    assert_eq!(StoreId::new(STORE).unwrap(), STORE);
    for bad in [
        "",
        "01234567",
        "0123456789ABCDEF0123456789ABCDEF",
        "0123456789abcdef0123456789abcdef0",
        "-123456789abcdef0123456789abcdef",
        "../456789abcdef0123456789abcdef0",
    ] {
        assert!(StoreId::new(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_volume_key_follows_the_rule_of_a_service_name() {
    for good in ["data", "pg-data", "a_b"] {
        assert_eq!(VolumeKey::new(good).unwrap(), good);
    }
    for bad in ["", "-data", "a.b", "a b", "a/b", &"a".repeat(65)] {
        assert!(VolumeKey::new(bad).is_err(), "{bad:?}");
    }
}

/// The workspace id of the app scope's tests.
const WORKSPACE_ID: &str = "fedcba9876543210fedcba9876543210";

/// Returns the scope of the app `app` of the workspace [`WORKSPACE_ID`].
fn app_scope(app: &str) -> Scope {
    Scope::app(
        Identity {
            id: WORKSPACE_ID.parse().unwrap(),
            run_record: PathBuf::from(format!("/state/runs/{WORKSPACE_ID}.json")),
        },
        &app.parse().unwrap(),
    )
}

#[test]
fn an_app_scope_names_its_project_after_the_workspace_and_the_app_and_its_volumes_after_the_workspace()
 {
    let scope = app_scope("api");
    let project = scope.project();
    assert!(project.as_str().starts_with("phase-"), "{project}");
    assert!(project.as_str().ends_with("-api"), "{project}");
    assert_eq!(
        scope.container_name(&service("server")),
        format!("{project}.server").as_str()
    );
    let workspace = project.as_str().strip_suffix("-api").unwrap();
    assert_eq!(
        scope.volume_name(&key("data")),
        format!("{workspace}.data").as_str()
    );
    assert_eq!(
        app_scope("web").volume_name(&key("data")),
        scope.volume_name(&key("data"))
    );
    assert_ne!(app_scope("web").project(), project);
    let other = Scope::app(
        Identity {
            id: "0123456789abcdef0123456789abcdef".parse().unwrap(),
            run_record: PathBuf::from("/state/runs/other.json"),
        },
        &"api".parse().unwrap(),
    );
    assert_ne!(other.project(), project);
    assert_ne!(
        other.volume_name(&key("data")),
        scope.volume_name(&key("data"))
    );
}

#[test]
fn an_app_scope_labels_a_container_with_the_workspace_id_run_record_and_app_and_a_volume_without_the_app()
 {
    let scope = app_scope("api");
    let run_record = format!("/state/runs/{WORKSPACE_ID}.json");
    assert_eq!(
        scope.labels("mxv.container", "server"),
        json!({
            "mxv.workspace-id": WORKSPACE_ID,
            "mxv.run-record": run_record,
            "mxv.app": "api",
            "mxv.container": "server",
        })
    );
    assert_eq!(
        scope.labels("mxv.volume", "data"),
        json!({
            "mxv.workspace-id": WORKSPACE_ID,
            "mxv.run-record": run_record,
            "mxv.volume": "data",
        })
    );
}

#[test]
fn an_app_scope_owns_a_container_of_its_workspace_app_and_process_and_a_volume_of_its_workspace() {
    let scope = app_scope("api");
    let container = |workspace: &str, app: &str, process: &str| {
        json!({"mxv.workspace-id": workspace, "mxv.app": app, "mxv.container": process}).to_string()
    };
    assert!(scope.owns(
        &container(WORKSPACE_ID, "api", "server"),
        "mxv.container",
        "server"
    ));
    assert!(!scope.owns(
        &container(WORKSPACE_ID, "web", "server"),
        "mxv.container",
        "server"
    ));
    assert!(!scope.owns(
        &container(WORKSPACE_ID, "api", "worker"),
        "mxv.container",
        "server"
    ));
    assert!(!scope.owns(
        &container("0123456789abcdef0123456789abcdef", "api", "server"),
        "mxv.container",
        "server"
    ));
    assert!(!scope.owns(
        &container(&format!("{WORKSPACE_ID} "), "api", "server"),
        "mxv.container",
        "server"
    ));
    assert!(!scope.owns(
        &container_labels(STORE, "reading-list", "server"),
        "mxv.container",
        "server"
    ));
    let volume = json!({"mxv.workspace-id": WORKSPACE_ID, "mxv.volume": "data"}).to_string();
    assert!(scope.owns(&volume, "mxv.volume", "data"));
    assert!(app_scope("web").owns(&volume, "mxv.volume", "data"));
    assert!(!scope.owns(&volume, "mxv.volume", "cache"));
}

#[test]
fn a_containers_stop_timeout_is_read_alone_and_is_none_when_docker_omits_it() {
    assert_eq!(
        STOP_TIMEOUT_FORMAT,
        "{{json (index .Config \"StopTimeout\")}}"
    );
    assert_eq!(stop_timeout_of("3\n"), Some(3));
    assert_eq!(stop_timeout_of("null\n"), None);
    assert_eq!(stop_timeout_of("not json"), None);
}

#[test]
fn an_inspection_keeps_dockers_exact_status_and_restart_count() {
    let scope = scope("reading-list");
    let labels = container_labels(STORE, "reading-list", "db");
    let printed = format!("{labels}\n{ID}\nrestarting 1\n2026-11-04T10:00:00.000000000Z\n\n4\n");
    let inspected = mint_container(&scope, &service("db"), ID, &printed).unwrap();
    assert_eq!(inspected.state, ContainerState::Restarting);
    assert_eq!(inspected.restarts, 4);

    let printed = format!("{labels}\n{ID}\nrunning 0\n2026-11-04T10:00:00.000000000Z\n\n0\n");
    let inspected = mint_container(&scope, &service("db"), ID, &printed).unwrap();
    assert_eq!(inspected.state, ContainerState::Running);
    assert_eq!(inspected.restarts, 0);

    let printed = format!("{labels}\n{ID}\npaused 0\n2026-11-04T10:00:00.000000000Z\n\n0\n");
    let inspected = mint_container(&scope, &service("db"), ID, &printed).unwrap();
    assert_eq!(inspected.state, ContainerState::Paused);
}

#[test]
fn a_command_whose_caller_cancels_stops_at_once_as_cancelled() {
    let cancel = Cancel::new();
    cancel.cancel();
    let began = now();
    let ended = run_program_until(
        "sleep".as_ref(),
        ["30"],
        "sleep",
        Duration::from_secs(60),
        &cancel,
    );
    assert!(
        matches!(ended, Err(Error::Cancelled { operation: "sleep" })),
        "{:?}",
        ended.err()
    );
    assert!(now() - began < Duration::from_secs(5));
}

#[test]
fn a_command_that_misses_its_deadline_is_stopped_as_failed() {
    let began = now();
    let ended = run_program_until(
        "sleep".as_ref(),
        ["30"],
        "sleep",
        Duration::from_millis(200),
        &Cancel::new(),
    );
    assert!(
        matches!(ended, Err(Error::Failed { .. })),
        "{:?}",
        ended.err()
    );
    assert!(now() - began < Duration::from_secs(5));
}

/// An environment that holds `MODE`, with a test value.
struct ModeOnly;

impl crate::env::AppliedEnvironment for ModeOnly {
    fn names(&self) -> std::collections::BTreeSet<String> {
        ["MODE".to_owned()].into()
    }

    fn expires_at(&self) -> Option<std::time::SystemTime> {
        None
    }

    fn digest(&self, key: &[u8; 32]) -> [u8; 32] {
        *key
    }

    fn apply_as(
        &self,
        command: &mut std::process::Command,
        reference: &dyn Fn(&str) -> Option<String>,
    ) {
        if let Some(name) = reference("MODE") {
            command.env(name, "test-mode");
        }
    }
}

#[test]
fn a_compose_child_inherits_no_private_name_and_receives_only_its_own_values() {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test builds a command that it never starts, to read its variables"
    )]
    let mut command = std::process::Command::new("docker");
    let cleared = ["MXV_ENV_0_0".to_owned(), "MXV_ENV_1_0".to_owned()];
    let refer = |name: &str| (name == "MODE").then(|| "MXV_ENV_0_0".to_owned());
    Referenced {
        environment: &ModeOnly,
        reference: &refer,
        cleared: &cleared,
    }
    .set_on(&mut command);
    let envs: BTreeMap<String, Option<String>> = command
        .get_envs()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.map(|value| value.to_string_lossy().into_owned()),
            )
        })
        .collect();
    assert_eq!(envs["MXV_ENV_0_0"], Some("test-mode".to_owned()));
    assert_eq!(envs["MXV_ENV_1_0"], None);
}

#[test]
fn a_port_answers_only_when_its_connection_stays_open_for_the_full_hold_and_not_once_cancelled() {
    use std::net::{Ipv4Addr, SocketAddr, TcpListener};
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    let port = listener.local_addr().unwrap().port();
    let began = now();
    assert!(!crate::engine::container::answers_within(
        port,
        Duration::from_millis(250),
        &Cancel::new()
    ));
    assert!(now() - began < Duration::from_millis(300));
    let began = now();
    assert!(crate::engine::container::answers_within(
        port,
        crate::plan::probe::LEAST_PUBLISHED_PROBE_TIMEOUT,
        &Cancel::new()
    ));
    assert!(now() - began >= Duration::from_millis(300));
    let cancel = Cancel::new();
    cancel.cancel();
    assert!(!crate::engine::container::answers_within(
        port,
        Duration::from_secs(1),
        &cancel
    ));
    drop(listener);
}
