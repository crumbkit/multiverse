//! Tests of the Docker of `sweep` against a fake `docker` program, which
//! answers from files in a directory of the test's own, and one test
//! against a real daemon, which skips when none answers.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;

use crate::engine::container::ContainerState;
use crate::engine::container::cli::DockerCli;
use crate::engine::container::names::{Scope, StoreId};
use crate::name::{WorkspaceId, WorkspaceName};
use crate::record::DockerName;
use crate::record::docker::{
    Docker, DockerCause, DockerError, DockerKind, DockerObject, SeenContainer,
};

/// The fake `docker` program. It appends each call's arguments to
/// `calls`. While the file `down` exists it fails every call as docker does
/// when its daemon does not answer, and while the file `nocontext` exists
/// it fails every call as docker does for an unknown context. `inspect`
/// prints the file `objects/<target>`, which holds what the real format
/// prints, or fails as docker's daemon does for a missing object of that
/// kind. An inspection whose format starts with the container's status
/// prints the file `states/<target>`, or `running 0` when there is none.
/// `update` succeeds for an object that exists, or fails as docker does for
/// a stopped container while the file `noupdate` exists. A removal deletes
/// that file, or fails as a refusal while the file `refuse` exists.
const FAKE_DOCKER: &str = r#"#!/bin/sh
dir=$(dirname "$0")
echo "$*" >> "$dir/calls"
if [ -e "$dir/down" ]; then
    echo "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?" >&2
    exit 1
fi
if [ -e "$dir/nocontext" ]; then
    echo "Failed to initialize: unable to resolve docker endpoint: context \"phase-test\": context not found: open $dir/meta.json: no such file or directory" >&2
    exit 1
fi
for last; do :; done
missing() {
    case "$1" in
        network) echo "Error response from daemon: network $last not found" >&2 ;;
        volume) echo "Error response from daemon: get $last: no such volume" >&2 ;;
        *) echo "Error response from daemon: No such container: $last" >&2 ;;
    esac
    exit 1
}
case "$1 $2" in
    "version --format") echo "29.0.0"; exit 0 ;;
    "container inspect")
        if [ -e "$dir/objects/$last" ]; then
            case "$4" in
                "{{.State.Status}}"*)
                    if [ -e "$dir/states/$last" ]; then cat "$dir/states/$last"; else echo "running 0"; fi ;;
                *) cat "$dir/objects/$last" ;;
            esac
            exit 0
        fi
        missing "$1" ;;
    "update --restart")
        if [ ! -e "$dir/objects/$last" ]; then missing container; fi
        if [ -e "$dir/noupdate" ]; then echo "Error response from daemon: Cannot update container $last: cannot update a stopped container" >&2; exit 1; fi
        exit 0 ;;
    "network inspect"|"network inspect"|"volume inspect")
        if [ -e "$dir/objects/$last" ]; then cat "$dir/objects/$last"; exit 0; fi
        missing "$1" ;;
esac
case "$1" in
    stop|rm|network|volume)
        if [ -e "$dir/refuse" ]; then echo "Error response from daemon: conflict: $last is in use" >&2; exit 1; fi
        if [ "$1" = stop ]; then
            if [ -e "$dir/objects/$last" ]; then exit 0; fi
        elif [ -e "$dir/objects/$last" ]; then rm "$dir/objects/$last"; exit 0; fi
        missing "$1" ;;
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
    /// workspace id, store and workspace labels are `fields`. An empty
    /// label is left out, and an object without labels prints `null`, as
    /// docker does.
    fn object(&self, targets: &[&str], fields: [&str; 4]) {
        let [id, workspace_id, store, workspace] = fields;
        let labels: serde_json::Map<String, serde_json::Value> = [
            ("mxv.workspace-id", workspace_id),
            ("mxv.store", store),
            ("mxv.workspace", workspace),
        ]
        .into_iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| (key.to_owned(), value.into()))
        .collect();
        let labels = if labels.is_empty() {
            "null".to_owned()
        } else {
            serde_json::Value::Object(labels).to_string()
        };
        self.labelled(targets, id, &labels);
    }

    /// Adds an object that docker finds by each of `targets`, whose id is
    /// `id` and whose labels print as the JSON `labels`.
    fn labelled(&self, targets: &[&str], id: &str, labels: &str) {
        for target in targets {
            write(
                &self.dir.path().join("objects").join(target),
                &format!("{id}\n{labels}\n"),
            );
        }
    }

    /// Sets the status and exit code that docker prints for the container
    /// `target`, such as `restarting 1`.
    fn state(&self, target: &str, printed: &str) {
        let states = self.dir.path().join("states");
        fs::create_dir_all(&states).unwrap();
        write(&states.join(target), &format!("{printed}\n"));
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
    assert!(
        calls.contains(&format!("rm --force --volumes -- {id}")),
        "{calls}"
    );
}

#[test]
fn removing_a_container_turns_off_its_restart_policy_before_it_stops_it() {
    let fake = Fake::new();
    let id = container_id('2');
    fake.object(&[&id], [&id, WORKSPACE_ID, "", ""]);
    fake.docker()
        .remove(DockerKind::Container, &id, Duration::from_secs(7))
        .unwrap();
    let calls = fake.calls();
    let update = calls.find(&format!("update --restart no -- {id}"));
    let stop = calls.find(&format!("stop --time 7 -- {id}"));
    assert!(update.is_some() && update < stop, "{calls}");
}

#[test]
fn removing_a_container_that_docker_is_restarting_removes_it_without_a_stop() {
    let fake = Fake::new();
    let id = container_id('2');
    fake.object(&[&id], [&id, WORKSPACE_ID, "", ""]);
    fake.state(&id, "restarting 1");
    fake.docker()
        .remove(DockerKind::Container, &id, Duration::from_secs(7))
        .unwrap();
    assert!(!fake.has(&id));
    let calls = fake.calls();
    assert!(!calls.contains("stop "), "{calls}");
    assert!(
        calls.contains(&format!("rm --force --volumes -- {id}")),
        "{calls}"
    );
}

#[test]
fn removing_a_stopped_container_whose_update_docker_refuses_still_removes_it() {
    let fake = Fake::new();
    let id = container_id('2');
    fake.object(&[&id], [&id, WORKSPACE_ID, "", ""]);
    fake.state(&id, "exited 1");
    fake.mark("noupdate");
    fake.docker()
        .remove(DockerKind::Container, &id, Duration::from_secs(7))
        .unwrap();
    assert!(!fake.has(&id));
    assert!(!fake.calls().contains("stop "), "{}", fake.calls());
}

#[test]
fn removing_a_running_container_whose_update_docker_refuses_fails_as_refused() {
    let fake = Fake::new();
    let id = container_id('2');
    fake.object(&[&id], [&id, WORKSPACE_ID, "", ""]);
    fake.mark("noupdate");
    let removed = fake
        .docker()
        .remove(DockerKind::Container, &id, Duration::from_secs(7));
    assert!(
        matches!(&removed, Err(DockerError::Refused { message, .. }) if message.contains("cannot update")),
        "{removed:?}"
    );
    assert!(fake.has(&id));
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
        matches!(&removed, Err(DockerError::Refused { message, .. }) if message.contains("in use")),
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

#[test]
fn the_failures_of_sweeps_docker_carry_the_cause_of_what_failed() {
    let workspace = workspace();
    let db = name("db");
    let missing = DockerCli::with_program("/nonexistent/docker".into())
        .find(&object(&workspace, DockerKind::Container, &db, None))
        .unwrap_err();
    assert_eq!(missing.cause(), DockerCause::DockerMissing);
    assert!(
        matches!(missing, DockerError::Unavailable { .. }),
        "{missing:?}"
    );

    let fake = Fake::new();
    fake.mark("down");
    let down = fake
        .docker()
        .remove(DockerKind::Volume, "data", Duration::ZERO)
        .unwrap_err();
    assert_eq!(down.cause(), DockerCause::DaemonUnavailable);

    let fake = Fake::new();
    fake.object(&["data"], ["data", WORKSPACE_ID, "", ""]);
    fake.mark("refuse");
    let refused = fake
        .docker()
        .remove(DockerKind::Volume, "data", Duration::ZERO)
        .unwrap_err();
    assert_eq!(refused.cause(), DockerCause::Refused);
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

#[test]
fn a_real_daemon_removes_a_containers_anonymous_volume_with_it_and_keeps_its_named_volume() {
    if !real_docker(&["version", "--format", "{{.Server.Version}}"]) {
        println!("skipping: no docker daemon answers");
        return;
    }
    if !real_docker(&["image", "inspect", "busybox:1.37.0"]) {
        println!("skipping: busybox:1.37.0 is not pulled");
        return;
    }
    let id = crate::random::hex_id().unwrap();
    let label = format!("mxv.workspace-id={id}");
    let named = format!("phase-test-{id}.keep");
    let container = format!("phase-test-{id}.db");
    assert!(real_docker(&[
        "volume", "create", "--label", &label, &named
    ]));
    assert!(real_docker(&[
        "container",
        "create",
        "--name",
        &container,
        "--label",
        &label,
        "--volume",
        &format!("{named}:/keep"),
        "--volume",
        "/data",
        "busybox:1.37.0"
    ]));
    let anonymous = real_output(&[
        "container",
        "inspect",
        "--format",
        "{{range .Mounts}}{{if eq .Destination \"/data\"}}{{.Name}}{{end}}{{end}}",
        "--",
        &container,
    ])
    .unwrap_or_default();

    let removed = DockerCli::new().remove(DockerKind::Container, &container, Duration::ZERO);

    let anonymous_left =
        !anonymous.is_empty() && real_docker(&["volume", "inspect", "--", &anonymous]);
    let named_left = real_docker(&["volume", "inspect", "--", &named]);
    let _ = real_docker(&["rm", "--force", "--", &container]);
    let _ = real_docker(&["volume", "rm", "--force", "--", &anonymous]);
    let _ = real_docker(&["volume", "rm", "--force", "--", &named]);
    removed.unwrap();
    assert_eq!(anonymous.len(), 64, "{anonymous}");
    assert!(!anonymous_left, "{anonymous} was left behind");
    assert!(named_left, "{named} was removed");
}

/// Runs the real `docker <args>` and returns its standard output, trimmed,
/// when it succeeded.
fn real_output(args: &[&str]) -> Option<String> {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test reads objects of its own in a real docker daemon"
    )]
    let output = std::process::Command::new("docker")
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[test]
fn a_label_value_that_holds_tabs_or_spaces_never_matches() {
    let fake = Fake::new();
    let workspace = workspace();
    let target = "mxv-fedcba98-reading-list.db";
    // A workspace id label that holds the older labels' values after tabs
    // is a workspace id of its own, so the object is not this workspace's.
    fake.labelled(
        &[target],
        &container_id('5'),
        &serde_json::json!({ "mxv.workspace-id": format!("\t{STORE}\treading-list") }).to_string(),
    );
    let container = name(target);
    let wanted = object(&workspace, DockerKind::Container, &container, None);
    let docker = fake.docker().with_legacy(legacy_scope());
    assert_eq!(docker.find(&wanted).unwrap(), None);
    for labels in [
        serde_json::json!({ "mxv.workspace-id": " " }),
        serde_json::json!({ "mxv.workspace-id": format!("{WORKSPACE_ID} ") }),
        serde_json::json!({ "mxv.store": format!("{STORE} "), "mxv.workspace": "reading-list" }),
        serde_json::json!({ "mxv.store": STORE, "mxv.workspace": "reading-list\n" }),
    ] {
        fake.labelled(&[target], &container_id('5'), &labels.to_string());
        assert_eq!(docker.find(&wanted).unwrap(), None, "{labels}");
    }
}

#[test]
fn an_unknown_docker_context_fails_a_find_and_a_removal_and_never_reports_an_object_gone() {
    let fake = Fake::new();
    let id = container_id('6');
    fake.object(&[&id, "data"], [&id, WORKSPACE_ID, "", ""]);
    fake.mark("nocontext");
    let workspace = workspace();
    let docker = fake.docker();
    for kind in [
        DockerKind::Container,
        DockerKind::Network,
        DockerKind::Volume,
    ] {
        let db = name("db");
        let found = docker.find(&object(&workspace, kind, &db, Some(&id)));
        assert!(
            matches!(&found, Err(DockerError::Unavailable { message, .. }) if message.contains("context not found")),
            "{kind:?}: {found:?}"
        );
        let removed = docker.remove(kind, &id, Duration::ZERO);
        assert!(
            matches!(removed, Err(DockerError::Unavailable { .. })),
            "{kind:?}: {removed:?}"
        );
    }
    assert!(fake.has(&id));
}

#[test]
fn a_removal_of_every_kind_while_docker_does_not_answer_fails_as_unavailable() {
    let fake = Fake::new();
    fake.mark("down");
    for kind in [
        DockerKind::Container,
        DockerKind::Network,
        DockerKind::Volume,
    ] {
        let removed = fake.docker().remove(kind, "data", Duration::ZERO);
        assert!(
            matches!(removed, Err(DockerError::Unavailable { .. })),
            "{kind:?}: {removed:?}"
        );
    }
}

#[test]
fn a_find_of_every_kind_while_docker_does_not_answer_fails_as_unavailable() {
    let fake = Fake::new();
    fake.mark("down");
    let workspace = workspace();
    let db = name("db");
    for kind in [
        DockerKind::Container,
        DockerKind::Network,
        DockerKind::Volume,
    ] {
        let found = fake.docker().find(&object(&workspace, kind, &db, None));
        assert!(
            matches!(found, Err(DockerError::Unavailable { .. })),
            "{kind:?}: {found:?}"
        );
    }
}

#[test]
fn a_missing_network_or_volume_is_not_found_from_the_daemons_own_answer() {
    let fake = Fake::new();
    let workspace = workspace();
    let gone = name("gone");
    for kind in [DockerKind::Network, DockerKind::Volume] {
        assert_eq!(
            fake.docker()
                .find(&object(&workspace, kind, &gone, None))
                .unwrap(),
            None,
            "{kind:?}"
        );
    }
    fake.docker()
        .remove(DockerKind::Network, "gone", Duration::ZERO)
        .unwrap();
}

#[test]
fn a_container_removal_waits_at_least_a_second_and_rounds_a_part_second_up() {
    let id = container_id('7');
    for (wait, seconds) in [
        (Duration::ZERO, 1),
        (Duration::from_millis(300), 1),
        (Duration::from_millis(1500), 2),
        (Duration::from_secs(3), 3),
    ] {
        let fake = Fake::new();
        fake.object(&[&id], [&id, WORKSPACE_ID, "", ""]);
        fake.docker()
            .remove(DockerKind::Container, &id, wait)
            .unwrap();
        assert!(
            fake.calls()
                .contains(&format!("stop --time {seconds} -- {id}")),
            "{wait:?}: {}",
            fake.calls()
        );
    }
}

#[test]
fn a_running_container_with_secret_files_that_docker_restarted_is_not_running() {
    let fake = Fake::new();
    let (waiting, restarted, fresh) = (container_id('a'), container_id('b'), container_id('c'));
    let ours = format!(r#"{{"mxv.workspace-id":"{WORKSPACE_ID}"}}"#);
    fake.labelled(
        &[&waiting],
        &waiting,
        &format!("{ours}\nrunning 0\n2 files"),
    );
    fake.labelled(&[&restarted], &restarted, &format!("{ours}\nrunning 0\n2 "));
    fake.labelled(&[&fresh], &fresh, &format!("{ours}\nrunning 0\n0 files"));
    let workspace = workspace();
    let db = name("db");
    let state = |id: &str| {
        fake.docker()
            .container_seen(&object(&workspace, DockerKind::Container, &db, Some(id)))
            .unwrap()
            .map(|seen| seen.state)
    };

    assert_eq!(state(&waiting), Some(ContainerState::Restarting));
    assert_eq!(state(&restarted), Some(ContainerState::Running));
    assert_eq!(state(&fresh), Some(ContainerState::Running));
    assert!(
        fake.calls().contains("{{.RestartCount}}"),
        "{}",
        fake.calls()
    );
}

#[test]
fn a_containers_restarts_and_start_time_come_from_its_inspection_with_its_state() {
    let fake = Fake::new();
    let (fresh, restarted, unreadable) = (container_id('a'), container_id('b'), container_id('c'));
    let ours = format!(r#"{{"mxv.workspace-id":"{WORKSPACE_ID}"}}"#);
    fake.labelled(
        &[&fresh],
        &fresh,
        &format!("{ours}\nrunning 0\n0 \n2026-11-05T10:20:30.123456789Z"),
    );
    fake.labelled(
        &[&restarted],
        &restarted,
        &format!("{ours}\nrunning 0\n3 \n1970-01-01T00:00:01.5Z"),
    );
    fake.labelled(
        &[&unreadable],
        &unreadable,
        &format!("{ours}\nrunning 0\nmany \n"),
    );
    let workspace = workspace();
    let db = name("db");
    let seen = |id: &str| {
        fake.docker()
            .container_seen(&object(&workspace, DockerKind::Container, &db, Some(id)))
            .unwrap()
            .unwrap()
    };

    assert_eq!(
        seen(&fresh),
        SeenContainer::new(
            ContainerState::Running,
            Some(0),
            Some("2026-11-05T10:20:30.123456789Z".to_owned())
        )
    );
    assert_eq!(
        seen(&restarted),
        SeenContainer::new(
            ContainerState::Running,
            Some(3),
            Some("1970-01-01T00:00:01.5Z".to_owned())
        )
    );
    assert_eq!(
        seen(&unreadable),
        SeenContainer::new(ContainerState::Running, None, None)
    );
    assert!(
        fake.calls().contains("{{.State.StartedAt}}"),
        "{}",
        fake.calls()
    );
}

#[test]
fn a_containers_state_comes_from_its_inspection_when_it_carries_this_workspaces_id() {
    let fake = Fake::new();
    let (running, exited, foreign) = (container_id('a'), container_id('b'), container_id('c'));
    let ours = format!(r#"{{"mxv.workspace-id":"{WORKSPACE_ID}"}}"#);
    fake.labelled(&[&running], &running, &format!("{ours}\nrunning 0"));
    fake.labelled(&[&exited], &exited, &format!("{ours}\nexited 137"));
    fake.labelled(
        &[&foreign],
        &foreign,
        r#"{"mxv.workspace-id":"another"}
running 0"#,
    );
    let workspace = workspace();
    let db = name("db");
    let state = |id: &str| {
        fake.docker()
            .container_seen(&object(&workspace, DockerKind::Container, &db, Some(id)))
            .unwrap()
            .map(|seen| seen.state)
    };

    assert_eq!(state(&running), Some(ContainerState::Running));
    assert_eq!(state(&exited), Some(ContainerState::Exited(137)));
    assert_eq!(state(&foreign), None);
    assert_eq!(state(&container_id('d')), None);
    assert!(
        fake.calls().contains("{{.State.Status}}"),
        "{}",
        fake.calls()
    );
    fake.mark("down");
    assert!(matches!(
        fake.docker().container_seen(&object(
            &workspace,
            DockerKind::Container,
            &db,
            Some(&running)
        )),
        Err(DockerError::Unavailable { .. })
    ));
}

#[test]
fn a_refused_removal_names_the_operation_and_the_containers_id_and_no_service() {
    let fake = Fake::new();
    let id = container_id('2');
    fake.object(&[&id], [&id, WORKSPACE_ID, "", ""]);
    fake.mark("noupdate");

    let error = fake
        .docker()
        .remove(DockerKind::Container, &id, Duration::from_secs(7))
        .unwrap_err();

    assert_eq!(error.operation(), Some("update"));
    assert_eq!(error.container_id(), Some(id.as_str()));
    assert_eq!(error.service(), None);
}

#[test]
fn a_find_that_docker_fails_names_the_inspect_command_of_the_objects_kind() {
    let fake = Fake::new();
    fake.mark("down");
    let id = container_id('5');
    let volume = name("data");
    let workspace = workspace();

    let container = fake
        .docker()
        .find(&object(
            &workspace,
            DockerKind::Container,
            &name("phase-db"),
            Some(&id),
        ))
        .unwrap_err();
    let volume = fake
        .docker()
        .find(&object(&workspace, DockerKind::Volume, &volume, None))
        .unwrap_err();

    assert_eq!(container.operation(), Some("container inspect"));
    assert_eq!(volume.operation(), Some("volume inspect"));
    assert_eq!(volume.container_id(), None);
}
