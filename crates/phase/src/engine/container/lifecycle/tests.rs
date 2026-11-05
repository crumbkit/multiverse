use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::engine::container::ContainerError as Error;
use crate::engine::container::host::{Follow, Host, Versioned};
use crate::engine::container::lifecycle::{
    Lifecycle, Prepared, RUN_VERSION, STORE_ID_FILE, StoreIdRecord, WorkspaceVolumes,
    app_volume_name, delete_app_volumes,
};
use crate::engine::container::names::{ServiceName, StoreId, VolumeName};
use crate::engine::container::objects::{Volume, mint_anonymous};
use crate::name::{WorkspaceId, WorkspaceName};
use crate::phase::Phase;

/// A host that keeps the run files as plain JSON files, takes no lock and
/// starts no follower.
#[derive(Debug)]
struct Files;

impl Host for Files {
    type Error = Error;
    type Lock = ();

    fn read_json<T: DeserializeOwned + Versioned>(&self, path: &Path) -> Result<Option<T>, Error> {
        match fs::read(path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).unwrap())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Error::Unreadable {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    fn write_json<T: Serialize>(&self, path: &Path, value: &T) -> Result<(), Error> {
        #[expect(
            clippy::disallowed_methods,
            reason = "the test's host writes its run files directly"
        )]
        fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> Result<(), Error> {
        let _ = fs::remove_file(path);
        Ok(())
    }

    fn rotate_log(&self, _log: &Path, _older: &Path) -> Result<(), Error> {
        Ok(())
    }

    fn lock(&self, _path: &Path, _wait: Duration) -> Result<(), Error> {
        Ok(())
    }

    fn saved_workspace_id(&self, _workspace: &WorkspaceName) -> Result<WorkspaceId, Error> {
        Ok("0123456789abcdef0123456789abcdef".parse().unwrap())
    }

    fn port_taken(&self, _port: u16) -> bool {
        false
    }

    fn follower_alive(&self, _record: &Path) -> Result<bool, Error> {
        Ok(false)
    }

    fn follow(&self, _follow: &Follow<'_>) -> Result<(), Error> {
        Ok(())
    }
}

/// Writes the store id file of the runs directory `runs`.
fn write_store_id(runs: &Path) {
    Files
        .write_json(
            &runs.join(STORE_ID_FILE),
            &StoreIdRecord {
                version: RUN_VERSION,
                id: StoreId::new("0123456789abcdef0123456789abcdef").unwrap(),
            },
        )
        .unwrap();
}

fn lifecycle(runs: &Path, workspace: &str) -> Lifecycle<'static, Files> {
    Lifecycle::new(
        &Files,
        runs,
        &WorkspaceName::try_from(workspace.to_owned()).unwrap(),
        Duration::ZERO,
        Duration::ZERO,
    )
}

/// Returns the `Prepared` of `lifecycle`, from its runs directory's store
/// id.
fn prepared(lifecycle: &Lifecycle<'_, Files>) -> Prepared {
    lifecycle.existing().unwrap().unwrap()
}

fn refused_scope<T: std::fmt::Debug>(result: Result<T, Error>) {
    assert!(
        matches!(
            &result,
            Err(Error::Failed {
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
    // The stable id that the host saved for one workspace never labels
    // the containers of another.
    let saved = reading.saved_id().unwrap();
    assert_eq!(saved.id().as_str(), "0123456789abcdef0123456789abcdef");
    refused_scope(other.prepare(&Phase::new(runs.path().join("phase")), &saved));
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
        app: false,
    };
    let refused = other.delete_volumes(&prepared(&other), volumes);
    assert!(
        matches!(
            &refused,
            Err(Error::Foreign { kind: "volume", name })
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

#[test]
fn the_volumes_of_apps_and_of_the_older_project_are_deleted_only_by_their_own_function() {
    let runs = tempfile::tempdir().unwrap();
    write_store_id(runs.path());
    let reading = lifecycle(runs.path(), "reading-list");
    let name = VolumeName::new("0f9e8d7c6b5a49382716051f0e9d8c7b6a594837261504f3e2d1c0b9a8978675")
        .unwrap();
    let anonymous = || {
        vec![Volume::Anonymous(
            mint_anonymous(&name, "anonymous\n").unwrap(),
        )]
    };
    let older = WorkspaceVolumes {
        scope: Some(prepared(&reading).scope),
        volumes: anonymous(),
        app: false,
    };
    let refused = delete_app_volumes(older);
    assert!(
        matches!(&refused, Err(Error::Foreign { kind: "volume", .. })),
        "{refused:?}"
    );
    let apps = WorkspaceVolumes {
        scope: Some(prepared(&reading).scope),
        volumes: anonymous(),
        app: true,
    };
    let refused = reading.delete_volumes(&prepared(&reading), apps);
    assert!(
        matches!(&refused, Err(Error::Foreign { kind: "volume", .. })),
        "{refused:?}"
    );
    delete_app_volumes(WorkspaceVolumes::default()).unwrap();
}

#[test]
fn the_data_volumes_of_a_workspaces_apps_are_named_after_its_stable_id() {
    let phase = Phase::new("/state/phase".into());
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let key = crate::engine::container::names::VolumeKey::new("data").unwrap();
    let name = app_volume_name(&phase, &id, &key).unwrap();
    assert!(
        name.starts_with("phase-") && name.ends_with(".data"),
        "{name}"
    );
    assert_eq!(name.len(), "phase-".len() + 16 + ".data".len());
}
