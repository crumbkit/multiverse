use std::path::Path;

use crate::docker::{Run, container_name, run_args, volume_name};
use crate::manifest::Manifest;
use crate::name::WorkspaceName;

fn name(text: &str) -> WorkspaceName {
    WorkspaceName::try_from(text.to_owned()).unwrap()
}

#[test]
fn two_workspaces_whose_names_join_alike_never_share_a_container_or_volume_name() {
    assert_ne!(
        container_name(&name("reading-list"), "db"),
        container_name(&name("login"), "fix-db")
    );
    assert_eq!(
        container_name(&name("reading-list"), "db"),
        "mxv-reading-list.db"
    );
    assert_eq!(
        volume_name(&name("reading-list"), "data"),
        "mxv-reading-list.data"
    );
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
    let workspace = name("reading-list");
    let args = run_args(&Run {
        workspace: &workspace,
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
            "mxv-reading-list.dynamodb",
            "--label",
            "mxv.workspace=reading-list",
            "--label",
            "mxv.container=dynamodb",
            "--publish",
            "127.0.0.1:24001:8000/tcp",
            "--env",
            "AWS_REGION=eu-west-1",
            "--mount",
            "type=volume,source=mxv-reading-list.data,target=/data",
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
    // A container name never holds `.`, so `mxv-release.2.db` is only the
    // workspace `release.2` with the container `db`.
    assert_eq!(container_name(&name("release.2"), "db"), "mxv-release.2.db");
    assert_ne!(
        container_name(&name("release.2"), "db"),
        container_name(&name("release"), "2-db")
    );
}
