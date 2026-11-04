use std::path::Path;

use crate::docker::{Run, Scope, message, run_args};
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
fn docker_run_publishes_on_the_loopback_address_labels_the_container_and_mounts_named_volumes() {
    let manifest = Manifest::parse(
        r#"
ports = ["dynamodb"]

[containers.dynamodb]
image = "amazon/dynamodb-local:2.5.4"
command = ["-jar", "DynamoDBLocal.jar", "-dbPath", "/data"]
env = { AWS_REGION = "eu-west-1" }
ports = { 8000 = "dynamodb" }
volumes = { data = "/data" }
"#,
        Path::new("/repo/mxv.toml"),
    )
    .unwrap();
    let args = run_args(&Run {
        scope: &scope("reading-list"),
        name: "dynamodb",
        container: &manifest.containers["dynamodb"],
        host_ports: &[24001],
    });
    assert_eq!(
        args,
        [
            "run",
            "--detach",
            "--name",
            "mxv-01234567-reading-list.dynamodb",
            "--label",
            "mxv.store=0123456789abcdef0123456789abcdef",
            "--label",
            "mxv.workspace=reading-list",
            "--label",
            "mxv.container=dynamodb",
            "--publish",
            "127.0.0.1:24001:8000/tcp",
            "--env",
            "AWS_REGION=eu-west-1",
            "--mount",
            "type=volume,source=mxv-01234567-reading-list.data,target=/data",
            "--",
            "amazon/dynamodb-local:2.5.4",
            "-jar",
            "DynamoDBLocal.jar",
            "-dbPath",
            "/data",
        ]
    );
}

#[test]
fn a_workspace_name_with_a_dot_still_splits_from_the_container_name_at_the_last_dot() {
    // A container name never holds `.`, so `mxv-01234567-release.2.db` is
    // only the workspace `release.2` with the container `db`.
    assert_eq!(
        scope("release.2").container_name("db"),
        "mxv-01234567-release.2.db"
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
