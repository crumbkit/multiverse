use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::json;

use crate::engine::container::ContainerError as Error;
use crate::engine::container::command::message;
use crate::engine::container::compose::{
    ComposeValue, ContainerSpec, ContainerSpecs, PublishedPort, UpFailure, compose_env,
    compose_file, up_failure,
};
use crate::engine::container::names::{
    ContainerId, Identity, Scope, ServiceName, StoreId, VolumeKey, VolumeName,
};
use crate::engine::container::objects::{
    ContainerState, mint_anonymous, mint_container, mint_volume,
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
    assert!(scope.owns(&format!("{STORE}\treading-list\tdb"), "db"));
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
    assert!(scope.owns(&format!("{STORE}\treading-list\tdb"), "db"));
    assert!(!scope.owns("\t\t", "db"));
    assert!(!scope.owns(&format!("{STORE}\treading-list\tcache"), "db"));
    assert!(!scope.owns(&format!("{STORE}\treading\tdb"), "db"));
    assert!(!scope.owns("fedcba9876543210fedcba9876543210\treading-list\tdb", "db"));
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
fn docker_states_parse_into_running_created_or_exited_with_dockers_code() {
    assert_eq!(
        ContainerState::parse("running 0\n"),
        ContainerState::Running
    );
    assert_eq!(ContainerState::parse("paused 0"), ContainerState::Running);
    assert_eq!(
        ContainerState::parse("restarting 1"),
        ContainerState::Running
    );
    assert_eq!(ContainerState::parse("created 0"), ContainerState::Created);
    assert_eq!(
        ContainerState::parse("exited 137"),
        ContainerState::Exited(137)
    );
    assert_eq!(ContainerState::parse("dead 3"), ContainerState::Exited(3));
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
        &printed_container(&format!("{STORE}\treading-list\tdb")),
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
        "\t\t".to_owned(),
        format!("{STORE}\treading-list\tcache"),
        format!("{STORE}\treading\tdb"),
        "fedcba9876543210fedcba9876543210\treading-list\tdb".to_owned(),
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
    let printed = format!("{STORE}\treading-list\tdb\nnot-an-id\nrunning 0\n\n\n");
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
    assert!(mint_volume(&scope, &name, &format!("{STORE}\treading-list\tdata\n")).is_ok());
    for labels in [
        "\t\t\n".to_owned(),
        format!("{STORE}\treading-list\tother\n"),
        format!("{STORE}\treading\tdata\n"),
        "fedcba9876543210fedcba9876543210\treading-list\tdata\n".to_owned(),
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
        let minted = mint_volume(&scope, &name, &format!("{STORE}\treading-list\tdata\n"));
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
