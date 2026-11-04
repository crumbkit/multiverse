use std::path::Path;
use std::time::Duration;

use crate::docker::lifecycle::{
    Lifecycle, Prepared, STORE_ID_FILE, StoreIdRecord, WorkspaceVolumes,
};
use crate::docker::names::{ServiceName, StoreId, VolumeName};
use crate::docker::objects::{Volume, mint_anonymous};
use crate::error::Error;
use crate::name::WorkspaceName;
use crate::run_files::{RUN_VERSION, write_json};

/// Writes the store id file of the runs directory `runs`.
fn write_store_id(runs: &Path) {
    write_json(
        &runs.join(STORE_ID_FILE),
        &StoreIdRecord {
            version: RUN_VERSION,
            id: StoreId::new("0123456789abcdef0123456789abcdef").unwrap(),
        },
    )
    .unwrap();
}

fn lifecycle(runs: &Path, workspace: &str) -> Lifecycle {
    Lifecycle::new(
        runs,
        &WorkspaceName::try_from(workspace.to_owned()).unwrap(),
        Duration::ZERO,
        Duration::ZERO,
    )
}

/// Returns the `Prepared` of `lifecycle`, from its runs directory's store
/// id.
fn prepared(lifecycle: &Lifecycle) -> Prepared {
    lifecycle.existing().unwrap().unwrap()
}

fn refused_scope<T: std::fmt::Debug>(result: Result<T, Error>) {
    assert!(
        matches!(
            &result,
            Err(Error::DockerFailed {
                operation: "scope check",
                ..
            })
        ),
        "{result:?}"
    );
}

#[test]
fn a_lifecycle_refuses_the_prepared_scope_of_another_workspace_before_it_runs_docker() {
    let runs = tempfile::tempdir().unwrap();
    write_store_id(runs.path());
    let reading = lifecycle(runs.path(), "reading-list");
    let other = lifecycle(runs.path(), "reading");
    let foreign = prepared(&reading);
    let db = ServiceName::new("db").unwrap();
    refused_scope(other.state(&foreign, &db));
    refused_scope(other.status(&foreign, &db));
    refused_scope(other.writing(&foreign, &db));
    refused_scope(other.stop(&foreign, &db));
    refused_scope(other.stop_all(&foreign));
    refused_scope(other.volumes(&foreign));
    refused_scope(other.delete_volumes(&foreign, WorkspaceVolumes::default()));
    refused_scope(other.start(&foreign, &Default::default(), &db, &Default::default()));
    // The workspace's own scope reaches no docker command for a workspace
    // without records.
    assert_eq!(other.state(&prepared(&other), &db).unwrap(), None);
}

#[test]
fn a_lifecycle_refuses_the_prepared_scope_of_another_runs_directory() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    write_store_id(first.path());
    write_store_id(second.path());
    let foreign = prepared(&lifecycle(first.path(), "reading-list"));
    refused_scope(lifecycle(second.path(), "reading-list").stop_all(&foreign));
}

#[test]
fn a_lifecycle_refuses_to_delete_the_volumes_of_another_workspace_before_it_runs_docker() {
    let runs = tempfile::tempdir().unwrap();
    write_store_id(runs.path());
    let reading = lifecycle(runs.path(), "reading-list");
    let other = lifecycle(runs.path(), "reading");
    let name = VolumeName::new("0f9e8d7c6b5a49382716051f0e9d8c7b6a594837261504f3e2d1c0b9a8978675")
        .unwrap();
    let volumes = WorkspaceVolumes {
        scope: Some(prepared(&reading).scope),
        volumes: vec![Volume::Anonymous(
            mint_anonymous(&name, "anonymous\n").unwrap(),
        )],
    };
    let refused = other.delete_volumes(&prepared(&other), volumes);
    assert!(
        matches!(
            &refused,
            Err(Error::DockerForeign { kind: "volume", name })
                if name == "0f9e8d7c6b5a49382716051f0e9d8c7b6a594837261504f3e2d1c0b9a8978675"
        ),
        "{refused:?}"
    );
    // A workspace's own empty volumes delete nothing and run no docker
    // command.
    other
        .delete_volumes(&prepared(&other), WorkspaceVolumes::default())
        .unwrap();
}
