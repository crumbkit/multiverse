use std::path::Path;

use std::collections::BTreeMap;

use serde_json::json;

use crate::docker::{
    ContainerState, Scope, UpFailure, compose_env, compose_file, message, up_failure,
};
use crate::manifest::Manifest;
use crate::name::WorkspaceName;

/// A store id of the tests.
const STORE: &str = "0123456789abcdef0123456789abcdef";

fn scope(workspace: &str) -> Scope {
    Scope::new(
        STORE,
        &WorkspaceName::try_from(workspace.to_owned()).unwrap(),
    )
}

#[test]
fn two_workspaces_whose_names_join_alike_never_share_a_container_or_volume_name() {
    assert_ne!(
        scope("reading-list").container_name("db"),
        scope("reading").container_name("list-db")
    );
    assert_eq!(
        scope("reading-list").container_name("db"),
        "mxv-01234567-reading-list.db"
    );
    assert_eq!(
        scope("reading-list").volume_name("data"),
        "mxv-01234567-reading-list.data"
    );
}

#[test]
fn two_stores_give_one_workspace_name_different_container_and_volume_names() {
    let other = Scope::new(
        "fedcba9876543210fedcba9876543210",
        &WorkspaceName::try_from("reading-list".to_owned()).unwrap(),
    );
    assert_ne!(
        other.container_name("db"),
        scope("reading-list").container_name("db")
    );
    assert_ne!(
        other.volume_name("data"),
        scope("reading-list").volume_name("data")
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
    let manifest = Manifest::parse(
        r#"
ports = ["dynamodb", "unused"]

[containers.dynamodb]
image = "amazon/dynamodb-local:2.5.4"
command = ["sh", "-c", "echo $HOME"]
env = { AWS_REGION = "eu-west-1", TOKEN = "s3cret$x" }
ports = { 8000 = "dynamodb" }
volumes = { data = "/data" }

[containers.cache]
image = "busybox:1.37.0"
env = { TOKEN = "other" }
"#,
        Path::new("/repo/mxv.toml"),
    )
    .unwrap();
    let scope = scope("reading-list");
    let ports = BTreeMap::from([("dynamodb".to_owned(), 24001)]);
    let file = compose_file(&scope, &manifest.containers, &ports);
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
        compose_env(&manifest.containers, "dynamodb"),
        [
            ("MXV_ENV_1_0".to_owned(), "eu-west-1".to_owned()),
            ("MXV_ENV_1_1".to_owned(), "s3cret$x".to_owned()),
        ]
    );
    assert_eq!(
        compose_env(&manifest.containers, "cache"),
        [("MXV_ENV_0_0".to_owned(), "other".to_owned())]
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
        scope("release.2").container_name("db"),
        "mxv-01234567-release_d2.db"
    );
    assert_ne!(
        scope("release.2").container_name("db"),
        scope("release").container_name("2-db")
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
