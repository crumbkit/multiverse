//! Tests of the Docker of `sweep` against a fake `docker` program, which
//! answers from files in a directory of the test's own, and one test
//! against a real daemon, which skips when none answers.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;

use crate::engine::container::cli::DockerCli;
use crate::engine::container::names::{Scope, StoreId};
use crate::name::{WorkspaceId, WorkspaceName};
use crate::record::DockerName;
use crate::record::docker::{Docker, DockerError, DockerKind, DockerObject};

/// The fake `docker` program. It appends each call's arguments to
/// `calls`, and fails every call while the file `down` exists, as a
/// daemon that does not answer does. `inspect` prints the file
/// `objects/<target>`, which holds what the real format prints, or fails
/// as docker does for a missing object. A removal deletes that file, or
/// fails as a refusal while the file `refuse` exists.
const FAKE_DOCKER: &str = r#"#!/bin/sh
dir=$(dirname "$0")
echo "$*" >> "$dir/calls"
if [ -e "$dir/down" ]; then
    echo "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?" >&2
    exit 1
fi
for last; do :; done
case "$1 $2" in
    "version --format") echo "29.0.0"; exit 0 ;;
    "container inspect"|"network inspect"|"volume inspect")
        if [ -e "$dir/objects/$last" ]; then cat "$dir/objects/$last"; exit 0; fi
        echo "Error: No such object: $last" >&2; exit 1 ;;
esac
case "$1" in
    stop|rm|network|volume)
        if [ -e "$dir/refuse" ]; then echo "Error response from daemon: conflict: $last is in use" >&2; exit 1; fi
        if [ "$1" = stop ]; then
            if [ -e "$dir/objects/$last" ]; then exit 0; fi
        elif [ -e "$dir/objects/$last" ]; then rm "$dir/objects/$last"; exit 0; fi
        echo "Error response from daemon: No such container: $last" >&2; exit 1 ;;
esac
echo "unexpected call $*" >&2
exit 1
"#;

/// The tests' workspace id.
const WORKSPACE_ID: &str = "0123456789abcdef0123456789abcdef";

/// The tests' store id.
const STORE: &str = "fedcba9876543210fedcba9876543210";

/// A container id.
fn container_id(digit: char) -> String {
    std::iter::repeat_n(digit, 64).collect()
}

/// A directory with the fake `docker` program.
struct Fake {
    /// The directory.
    dir: TempDir,
}

impl Fake {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("docker");
        write(&program, FAKE_DOCKER);
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir(dir.path().join("objects")).unwrap();
        Self { dir }
    }

    fn program(&self) -> PathBuf {
        self.dir.path().join("docker")
    }

    /// Adds an object that docker finds by each of `targets`, whose id,
    /// workspace id, store and workspace labels are `fields`.
    fn object(&self, targets: &[&str], fields: [&str; 4]) {
        for target in targets {
            write(
                &self.dir.path().join("objects").join(target),
                &format!("{}\n", fields.join("\t")),
            );
        }
    }

    fn has(&self, target: &str) -> bool {
        self.dir.path().join("objects").join(target).exists()
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.dir.path().join("calls")).unwrap_or_default()
    }

    fn mark(&self, file: &str) {
        write(&self.dir.path().join(file), "");
    }

    fn docker(&self) -> DockerCli {
        DockerCli::with_program(self.program().into_os_string())
    }
}

fn write(path: &Path, text: &str) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes the files of a fake docker program"
    )]
    fs::write(path, text).unwrap();
}

fn workspace() -> WorkspaceId {
    WORKSPACE_ID.parse().unwrap()
}

fn name(text: &str) -> DockerName {
    text.parse().unwrap()
}

fn legacy_scope() -> Scope {
    Scope::new(
        &StoreId::new(STORE).unwrap(),
        &"reading-list".parse::<WorkspaceName>().unwrap(),
    )
}

fn object<'a>(
    workspace: &'a WorkspaceId,
    kind: DockerKind,
    name: &'a DockerName,
    id: Option<&'a str>,
) -> DockerObject<'a> {
    DockerObject {
        workspace,
        kind,
        name,
        id,
    }
}

#[test]
fn a_container_with_a_recorded_id_is_found_by_that_id_when_it_carries_this_workspaces_id() {
    let fake = Fake::new();
    let id = container_id('a');
    fake.object(&[&id], [&id, WORKSPACE_ID, STORE, "reading"]);
    let workspace = workspace();
    let name = name("mxv-fedcba98-reading.db");
    let found = fake
        .docker()
        .find(&object(&workspace, DockerKind::Container, &name, Some(&id)))
        .unwrap();
    assert_eq!(found, Some(id.clone()));
    assert!(fake.calls().contains("container inspect --format"));
    assert!(fake.calls().trim_end().ends_with(&format!("-- {id}")));
}

#[test]
fn a_container_found_by_its_name_is_matched_only_by_this_workspaces_id_label() {
    let fake = Fake::new();
    let ours = container_id('b');
    fake.object(&["ours.db"], [&ours, WORKSPACE_ID, STORE, "reading"]);
    fake.object(
        &["theirs.db"],
        [
            &container_id('c'),
            "99999999999999999999999999999999",
            STORE,
            "reading",
        ],
    );
    let workspace = workspace();
    let docker = fake.docker();
    let ours_name = name("ours.db");
    let theirs_name = name("theirs.db");
    assert_eq!(
        docker
            .find(&object(&workspace, DockerKind::Container, &ours_name, None))
            .unwrap(),
        Some(ours)
    );
    assert_eq!(
        docker
            .find(&object(
                &workspace,
                DockerKind::Container,
                &theirs_name,
                None
            ))
            .unwrap(),
        None
    );
}

#[test]
fn an_object_without_an_mxv_label_is_never_matched_even_by_a_recorded_id() {
    let fake = Fake::new();
    let id = container_id('d');
    fake.object(&[&id, "plain.db"], [&id, "", "", ""]);
    let workspace = workspace();
    let plain = name("plain.db");
    let docker = fake.docker().with_legacy(legacy_scope());
    assert_eq!(
        docker
            .find(&object(
                &workspace,
                DockerKind::Container,
                &plain,
                Some(&id)
            ))
            .unwrap(),
        None
    );
    assert_eq!(
        docker
            .find(&object(&workspace, DockerKind::Container, &plain, None))
            .unwrap(),
        None
    );
}

#[test]
fn an_object_with_only_the_older_labels_is_matched_by_its_store_and_workspace() {
    let fake = Fake::new();
    let id = container_id('e');
    let target = "mxv-fedcba98-reading-list.db";
    fake.object(&[target], [&id, "", STORE, "reading-list"]);
    let workspace = workspace();
    let container = name(target);
    let wanted = object(&workspace, DockerKind::Container, &container, None);
    assert_eq!(
        fake.docker()
            .with_legacy(legacy_scope())
            .find(&wanted)
            .unwrap(),
        Some(id)
    );
    // Without the scope of the older labels, the object is not this
    // workspace's.
    assert_eq!(fake.docker().find(&wanted).unwrap(), None);
    // The older labels of another store do not match.
    let other = Scope::new(
        &StoreId::new("00000000000000000000000000000000").unwrap(),
        &"reading-list".parse::<WorkspaceName>().unwrap(),
    );
    assert_eq!(
        fake.docker().with_legacy(other).find(&wanted).unwrap(),
        None
    );
}

#[test]
fn an_object_that_carries_another_workspaces_id_is_never_matched_by_the_older_labels() {
    let fake = Fake::new();
    let target = "mxv-fedcba98-reading-list.db";
    fake.object(
        &[target],
        [
            &container_id('f'),
            "99999999999999999999999999999999",
            STORE,
            "reading-list",
        ],
    );
    let workspace = workspace();
    let container = name(target);
    let wanted = object(&workspace, DockerKind::Container, &container, None);
    assert_eq!(
        fake.docker()
            .with_legacy(legacy_scope())
            .find(&wanted)
            .unwrap(),
        None
    );
}

#[test]
fn a_network_and_a_volume_are_found_through_their_own_inspect_commands() {
    let fake = Fake::new();
    let network_id = container_id('1');
    fake.object(&["net"], [&network_id, WORKSPACE_ID, "", ""]);
    fake.object(&["data"], ["data", WORKSPACE_ID, "", ""]);
    let workspace = workspace();
    let docker = fake.docker();
    let net = name("net");
    let data = name("data");
    assert_eq!(
        docker
            .find(&object(&workspace, DockerKind::Network, &net, None))
            .unwrap(),
        Some(network_id)
    );
    assert_eq!(
        docker
            .find(&object(&workspace, DockerKind::Volume, &data, None))
            .unwrap(),
        Some("data".to_owned())
    );
    let calls = fake.calls();
    assert!(calls.contains("network inspect --format"), "{calls}");
    assert!(calls.contains("volume inspect --format"), "{calls}");
}

#[test]
fn a_missing_object_is_not_found() {
    let fake = Fake::new();
    let workspace = workspace();
    let gone = name("gone");
    assert_eq!(
        fake.docker()
            .find(&object(&workspace, DockerKind::Container, &gone, None))
            .unwrap(),
        None
    );
}

#[test]
fn a_docker_that_does_not_answer_fails_a_find_and_never_reports_an_object_gone() {
    let fake = Fake::new();
    fake.mark("down");
    let workspace = workspace();
    let db = name("db");
    let found = fake
        .docker()
        .find(&object(&workspace, DockerKind::Container, &db, None));
    assert!(
        matches!(found, Err(DockerError::Unavailable { .. })),
        "{found:?}"
    );
}

#[test]
fn a_missing_docker_program_fails_a_find_as_unavailable() {
    let docker = DockerCli::with_program("/nonexistent/docker".into());
    let workspace = workspace();
    let db = name("db");
    let found = docker.find(&object(&workspace, DockerKind::Container, &db, None));
    assert!(
        matches!(found, Err(DockerError::Unavailable { .. })),
        "{found:?}"
    );
}

#[test]
fn removing_a_container_stops_it_within_the_wait_and_then_removes_it() {
    let fake = Fake::new();
    let id = container_id('2');
    fake.object(&[&id], [&id, WORKSPACE_ID, "", ""]);
    fake.docker()
        .remove(DockerKind::Container, &id, Duration::from_secs(7))
        .unwrap();
    assert!(!fake.has(&id));
    let calls = fake.calls();
    assert!(calls.contains(&format!("stop --time 7 -- {id}")), "{calls}");
    assert!(calls.contains(&format!("rm --force -- {id}")), "{calls}");
}

#[test]
fn removing_a_network_or_a_volume_runs_their_own_remove_commands() {
    let fake = Fake::new();
    let network_id = container_id('3');
    fake.object(&[&network_id], [&network_id, WORKSPACE_ID, "", ""]);
    fake.object(&["data"], ["data", WORKSPACE_ID, "", ""]);
    let docker = fake.docker();
    docker
        .remove(DockerKind::Network, &network_id, Duration::ZERO)
        .unwrap();
    docker
        .remove(DockerKind::Volume, "data", Duration::ZERO)
        .unwrap();
    let calls = fake.calls();
    assert!(
        calls.contains(&format!("network rm -- {network_id}")),
        "{calls}"
    );
    assert!(calls.contains("volume rm -- data"), "{calls}");
    assert!(!fake.has("data"));
}

#[test]
fn removing_an_object_that_is_already_gone_succeeds() {
    let fake = Fake::new();
    fake.docker()
        .remove(DockerKind::Container, &container_id('4'), Duration::ZERO)
        .unwrap();
    fake.docker()
        .remove(DockerKind::Volume, "gone", Duration::ZERO)
        .unwrap();
}

#[test]
fn a_removal_that_docker_answers_and_refuses_fails_as_refused() {
    let fake = Fake::new();
    fake.object(&["data"], ["data", WORKSPACE_ID, "", ""]);
    fake.mark("refuse");
    let removed = fake
        .docker()
        .remove(DockerKind::Volume, "data", Duration::ZERO);
    assert!(
        matches!(&removed, Err(DockerError::Refused { message }) if message.contains("in use")),
        "{removed:?}"
    );
}

#[test]
fn a_removal_while_docker_does_not_answer_fails_as_unavailable() {
    let fake = Fake::new();
    fake.mark("down");
    let removed = fake
        .docker()
        .remove(DockerKind::Volume, "data", Duration::ZERO);
    assert!(
        matches!(removed, Err(DockerError::Unavailable { .. })),
        "{removed:?}"
    );
}

/// Runs the real `docker <args>` and reports whether it succeeded.
fn real_docker(args: &[&str]) -> bool {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test creates and removes objects of its own in a real docker daemon"
    )]
    let output = std::process::Command::new("docker").args(args).output();
    output.is_ok_and(|output| output.status.success())
}

#[test]
fn a_real_daemon_finds_and_removes_a_volume_and_a_container_by_this_workspaces_id_label() {
    if !real_docker(&["version", "--format", "{{.Server.Version}}"]) {
        println!("skipping: no docker daemon answers");
        return;
    }
    let id = crate::random::hex_id().unwrap();
    let workspace: WorkspaceId = id.parse().unwrap();
    let label = format!("mxv.workspace-id={id}");
    let other = format!("mxv.workspace-id={}", crate::random::hex_id().unwrap());
    let ours = format!("phase-test-{id}.data");
    let theirs = format!("phase-test-{id}.other");
    assert!(real_docker(&["volume", "create", "--label", &label, &ours]));
    assert!(real_docker(&[
        "volume", "create", "--label", &other, &theirs
    ]));
    let docker = DockerCli::new();
    let ours_name = name(&ours);
    let theirs_name = name(&theirs);
    let volume = object(&workspace, DockerKind::Volume, &ours_name, None);
    let found = docker.find(&volume);
    let foreign = docker.find(&object(&workspace, DockerKind::Volume, &theirs_name, None));
    let removed = docker.remove(DockerKind::Volume, &ours, Duration::ZERO);
    let after = docker.find(&volume);
    let _ = real_docker(&["volume", "rm", "--", &ours]);
    let _ = real_docker(&["volume", "rm", "--", &theirs]);
    assert_eq!(found.unwrap(), Some(ours.clone()));
    assert_eq!(foreign.unwrap(), None);
    removed.unwrap();
    assert_eq!(after.unwrap(), None);

    // A container needs the image of the containers tests, which the test
    // does not pull.
    if !real_docker(&["image", "inspect", "busybox:1.37.0"]) {
        println!("skipping the container: busybox:1.37.0 is not pulled");
        return;
    }
    let container = format!("phase-test-{id}.db");
    assert!(real_docker(&[
        "container",
        "create",
        "--name",
        &container,
        "--label",
        &label,
        "busybox:1.37.0"
    ]));
    let container_name = name(&container);
    let wanted = object(&workspace, DockerKind::Container, &container_name, None);
    let found = docker.find(&wanted);
    let removed = found
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .map(|found| docker.remove(DockerKind::Container, found, Duration::ZERO));
    let after = docker.find(&wanted);
    let _ = real_docker(&["rm", "--force", "--", &container]);
    let found = found.unwrap().unwrap();
    assert_eq!(found.len(), 64, "{found}");
    removed.unwrap().unwrap();
    assert_eq!(after.unwrap(), None);
}
