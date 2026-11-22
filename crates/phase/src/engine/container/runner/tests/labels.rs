//! Tests of the label prefix that a host gives phase, against a real
//! Docker daemon. Each test skips as the runner's other tests do.

use std::path::PathBuf;

use super::{World, app, docker, docker_ready, outcome};
use crate::engine::container::LabelPrefix;
use crate::phase::Phase;
use crate::report::AppOutcome;

/// Returns the prefix `example.`, a host's prefix other than mxv's.
fn example() -> LabelPrefix {
    LabelPrefix::new("example").unwrap()
}

/// Returns the labels of the docker object `object` of kind `kind`, as
/// JSON.
fn labels_of(kind: &str, object: &str) -> String {
    let path = if kind == "volume" {
        "{{json .Labels}}"
    } else {
        "{{json .Config.Labels}}"
    };
    docker(&[kind, "inspect", "--format", path, "--", object]).unwrap()
}

#[test]
fn containers_and_volumes_carry_the_label_prefix_phase_was_given() {
    if !docker_ready() {
        return;
    }
    let world = World::with_labels(example());
    let mut db = app("db", "exec sleep 60");
    db.processes[0]
        .volumes
        .insert("data".parse().unwrap(), PathBuf::from("/data"));
    let plan = world.plan(vec![db]);

    let report = world.up(&plan);

    assert_eq!(outcome(&report, "db"), &AppOutcome::Started, "{report:?}");
    let id = world.container("db", "server").unwrap();
    let container = labels_of("container", &id);
    assert!(
        container.contains(&format!("\"example.workspace-id\":\"{}\"", world.id)),
        "{container}"
    );
    assert!(container.contains("\"example.app\":\"db\""), "{container}");
    assert!(
        container.contains("\"example.container\":\"server\""),
        "{container}"
    );
    assert!(container.contains("\"example.run-record\":"), "{container}");
    assert!(!container.contains("\"mxv."), "{container}");
    let volume = crate::engine::container::app_volume_name(
        &world.phase,
        &world.id,
        &crate::engine::container::VolumeKey::new("data").unwrap(),
    )
    .unwrap();
    let volume = labels_of("volume", &volume);
    assert!(volume.contains("\"example.volume\":\"data\""), "{volume}");
    assert!(!volume.contains("\"mxv."), "{volume}");
    let listed = crate::engine::container::labelled_volumes(&world.phase).unwrap();
    assert!(listed.iter().any(|(id, _)| id == &world.id), "{listed:?}");
    world.down();
}

#[test]
fn a_sweep_under_one_label_prefix_leaves_objects_of_another_untouched() {
    if !docker_ready() {
        return;
    }
    let world = World::with_labels(example());
    let mut db = app("db", "exec sleep 60");
    db.processes[0]
        .volumes
        .insert("data".parse().unwrap(), PathBuf::from("/data"));
    let plan = world.plan(vec![db]);
    let report = world.up(&plan);
    assert_eq!(outcome(&report, "db"), &AppOutcome::Started, "{report:?}");
    let id = world.container("db", "server").unwrap();

    // A phase over the same state directory under mxv's prefix finds the
    // record's container foreign, and stops and removes nothing.
    let mxv = Phase::new(world.state.path().to_path_buf());
    let _swept = mxv.sweep(&world.id, false);
    let volumes = crate::engine::container::labelled_volumes(&mxv).unwrap();

    let state = docker(&[
        "container",
        "inspect",
        "--format",
        "{{.State.Status}}",
        "--",
        &id,
    ]);
    assert_eq!(state.as_deref().map(str::trim), Some("running"));
    assert!(
        volumes.iter().all(|(found, _)| found != &world.id),
        "{volumes:?}"
    );
    world.down();
}
