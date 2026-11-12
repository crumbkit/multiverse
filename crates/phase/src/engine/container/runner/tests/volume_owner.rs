//! Tests of the volume-owner step through `Phase::up`, `restart`, `down`
//! and `sweep` against a real Docker daemon. They skip as the runner's
//! other tests do, and also when the helper image, busybox pinned by
//! digest, is not pulled, which CI's lane pulls.

use std::path::PathBuf;
use std::time::Duration;

use super::{BuiltImage, IMAGE, Mode, World, app, docker, docker_ready, outcome, start_deadline};
use crate::cancel::Cancel;
use crate::engine::container::volume_owner::HELPER_IMAGE;
use crate::engine::container::volumes::{app_volumes, delete_app_volumes};
use crate::error::PhaseError;
use crate::phase::{Phase, UpDeadlines};
use crate::plan::AppPlan;
use crate::record::DockerCause;
use crate::record::run::{ObjectKey, Started, VolumeOwner};
use crate::record::store::{Journal, RunStore};
use crate::report::{AppOutcome, Note, Report};
use crate::select::Selection;
use crate::source::Source;

/// Reports whether the tests can run, and prints why when they cannot.
fn ready() -> bool {
    if !docker_ready() {
        return false;
    }
    if docker(&["image", "inspect", HELPER_IMAGE]).is_none() {
        println!("skipping: {HELPER_IMAGE} is not pulled");
        return false;
    }
    true
}

/// Returns the container app `name` whose `server` runs `script` and
/// mounts the workspace volume `data` at `/data`.
fn mounting(name: &str, script: &str) -> AppPlan {
    let mut app = app(name, script);
    app.processes[0]
        .volumes
        .insert("data".parse().unwrap(), PathBuf::from("/data"));
    app
}

/// The script of a process that fails unless it can write `/data`.
const WRITES: &str = "touch /data/written || exit 3; exec sleep 60";

impl World {
    /// Returns the Docker name of the workspace volume `data`.
    fn data_volume(&self) -> String {
        let record = RunStore::new(self.state.path())
            .read(&self.id)
            .unwrap()
            .unwrap();
        record
            .volumes
            .iter()
            .find(|entry| entry.name.as_str().ends_with(".data"))
            .unwrap()
            .name
            .as_str()
            .to_owned()
    }

    /// Returns the owner that the run record holds for the volume `data`.
    fn recorded_owner(&self) -> Option<VolumeOwner> {
        let record = RunStore::new(self.state.path()).read(&self.id).unwrap()?;
        record
            .volumes
            .iter()
            .find(|entry| entry.name.as_str().ends_with(".data"))?
            .owner
            .clone()
    }

    /// Returns the owner and mode of the root of the volume `data`, as
    /// `uid:gid`.
    fn data_owner(&self) -> String {
        let mount = format!("{}:/v", self.data_volume());
        docker(&[
            "run", "--rm", "--volume", &mount, IMAGE, "stat", "-c", "%u:%g", "/v",
        ])
        .unwrap()
        .trim()
        .to_owned()
    }

    /// Gives the root of the volume `data` to `owner` outside phase.
    fn chown_data(&self, owner: &str) {
        let mount = format!("{}:/v", self.data_volume());
        docker(&[
            "run", "--rm", "--volume", &mount, IMAGE, "chown", owner, "/v",
        ])
        .unwrap();
    }

    /// Returns the ids of the workspace's volume-owner helpers.
    fn helpers(&self) -> Vec<String> {
        let workspace = format!("label=mxv.workspace-id={}", self.id);
        docker(&[
            "container",
            "ls",
            "--all",
            "--quiet",
            "--filter",
            &workspace,
            "--filter",
            "label=mxv.role=volume-owner",
        ])
        .unwrap()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
    }

    /// Creates a volume-owner helper of the app `app` that mounts the
    /// volume `data` and never ran, as a run of `up` that ended between
    /// the helper's creation and its removal leaves one.
    fn leave_helper(&self, app: &str) {
        let mount = format!("{}:/volume", self.data_volume());
        let run_record = RunStore::new(self.state.path()).path(&self.id);
        docker(&[
            "create",
            "--label",
            &format!("mxv.workspace-id={}", self.id),
            "--label",
            &format!("mxv.run-record={}", run_record.display()),
            "--label",
            &format!("mxv.app={app}"),
            "--label",
            "mxv.role=volume-owner",
            "--label",
            "mxv.volume=data",
            "--volume",
            &mount,
            "--entrypoint",
            "true",
            "--",
            HELPER_IMAGE,
        ])
        .unwrap();
    }

    /// Returns a second state directory, with a phase over it, that holds
    /// this world's workspace id, as a copied state directory does.
    fn second_state(&self) -> (tempfile::TempDir, Phase) {
        let state = tempfile::tempdir().unwrap();
        let phase =
            Phase::new(state.path().to_path_buf()).with_start_grace(Duration::from_millis(500));
        (state, phase)
    }

    /// Brings `plan` up under `phase`, which belongs to another state
    /// directory than this world's, and returns what `up` returns.
    fn up_under(&self, phase: &Phase, plan: &crate::plan::Plan) -> Result<Report, PhaseError> {
        let ready = start_deadline();
        phase.up(
            plan,
            &Selection::All,
            &Mode,
            UpDeadlines::new(ready, ready),
            &Cancel::new(),
        )
    }

    /// Asserts that `up` stopped the app `db` with a refusal whose cause is
    /// Docker's foreign object, and that the object's name ends with `suffix`.
    fn assert_foreign(report: &Result<Report, PhaseError>, suffix: &str) {
        let report = report.as_ref().unwrap();
        let AppOutcome::Errored {
            cause: Some(cause),
            context: Some(context),
            ..
        } = outcome(report, "db")
        else {
            panic!("{report:?}");
        };
        assert_eq!(*cause, DockerCause::Foreign, "{report:?}");
        assert!(
            context.foreign.as_ref().unwrap().name.ends_with(suffix),
            "{report:?}"
        );
    }

    /// Restarts every app of `plan`.
    fn restart(&self, plan: &crate::plan::Plan) -> Report {
        self.phase
            .restart(
                plan,
                &Selection::All,
                &Mode,
                start_deadline(),
                &Cancel::new(),
            )
            .unwrap()
    }
}

#[test]
fn a_container_that_runs_as_another_user_than_root_can_write_its_new_named_volume() {
    if !ready() {
        return;
    }
    let world = World::new();
    let mut db = mounting("db", WRITES);
    db.container.user = Some("1000:1000".to_owned());
    let plan = world.plan(vec![db]);

    let report = world.up(&plan);

    assert_eq!(outcome(&report, "db"), &AppOutcome::Started, "{report:?}");
    assert_eq!(world.data_owner(), "1000:1000");
    assert_eq!(world.recorded_owner(), Some("1000:1000".parse().unwrap()));
    assert!(world.helpers().is_empty(), "{:?}", world.helpers());
    world.down();
}

#[test]
fn an_owner_of_keep_leaves_the_volume_owned_by_root() {
    if !ready() {
        return;
    }
    let world = World::new();
    let mut db = mounting("db", "exec sleep 60");
    db.container.user = Some("1000:1000".to_owned());
    db.container.owner = Some("keep".to_owned());
    let plan = world.plan(vec![db]);

    let report = world.up(&plan);

    assert_eq!(outcome(&report, "db"), &AppOutcome::Started, "{report:?}");
    assert_eq!(world.data_owner(), "0:0");
    assert_eq!(world.recorded_owner(), None);
    world.down();
}

#[test]
fn a_plans_owner_of_root_gives_a_volume_that_another_user_owned_to_root() {
    if !ready() {
        return;
    }
    let world = World::new();
    let mut db = mounting("db", "exec sleep 60");
    db.container.user = Some("1000:1000".to_owned());
    world.up(&world.plan(vec![db.clone()]));
    assert_eq!(world.data_owner(), "1000:1000");
    db.container.owner = Some("0:0".to_owned());

    let report = world.up(&world.plan(vec![db]));

    assert!(
        !matches!(outcome(&report, "db"), AppOutcome::Errored { .. }),
        "{report:?}"
    );
    assert_eq!(world.data_owner(), "0:0");
    assert_eq!(world.recorded_owner(), Some("0:0".parse().unwrap()));
    world.down();
}

#[test]
fn an_images_user_that_is_a_name_resolves_to_its_numbers_in_the_image() {
    if !ready() {
        return;
    }
    let Some(image) = BuiltImage::build(&format!(
        "FROM {IMAGE}\nRUN adduser -D -u 1234 app\nUSER app\n"
    )) else {
        return;
    };
    let gid = docker(&["run", "--rm", "--entrypoint", "id", &image.tag, "-g"]).unwrap();
    let world = World::new();
    let mut db = mounting("db", WRITES);
    db.source = Source::image(image.tag.parse().unwrap());
    let plan = world.plan(vec![db]);

    let report = world.up(&plan);

    assert_eq!(outcome(&report, "db"), &AppOutcome::Started, "{report:?}");
    assert_eq!(world.data_owner(), format!("1234:{}", gid.trim()));
    assert!(world.helpers().is_empty(), "{:?}", world.helpers());
    world.down();
}

#[test]
fn a_user_whose_name_resolves_to_uid_0_is_refused_and_nothing_starts() {
    if !ready() {
        return;
    }
    let Some(image) = BuiltImage::build(&format!(
        "FROM {IMAGE}\nRUN echo 'toor:x:0:0:toor:/root:/bin/sh' >> /etc/passwd\nUSER toor\n"
    )) else {
        return;
    };
    let world = World::new();
    let mut db = mounting("db", WRITES);
    db.source = Source::image(image.tag.parse().unwrap());
    let plan = world.plan(vec![db]);

    let report = world.up(&plan);

    match outcome(&report, "db") {
        AppOutcome::Errored { code, message, .. } => {
            assert_eq!(code, "phase::container::volume_owner_root");
            assert!(message.contains("the image's USER"), "{message}");
            assert!(!message.contains("toor"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(!world.has_containers());
}

#[test]
fn an_up_that_finds_a_leftover_helper_removes_it_and_sets_the_owner_again() {
    if !ready() {
        return;
    }
    let world = World::new();
    let mut db = mounting("db", "exec sleep 60");
    db.container.user = Some("1000:1000".to_owned());
    let plan = world.plan(vec![db]);
    let report = world.up(&plan);
    assert_eq!(outcome(&report, "db"), &AppOutcome::Started, "{report:?}");
    // The run record still says that phase gave the volume to 1000:1000,
    // so only the leftover helper makes the next start set it again.
    world.chown_data("0:0");
    world.leave_helper("db");
    let id = world.container("db", "server").unwrap();
    docker(&["rm", "--force", "--", &id]).unwrap();

    let report = world.up(&plan);

    assert_eq!(outcome(&report, "db"), &AppOutcome::Restarted, "{report:?}");
    assert!(world.helpers().is_empty(), "{:?}", world.helpers());
    assert_eq!(world.data_owner(), "1000:1000");
    world.down();
}

#[test]
fn a_run_that_ended_while_a_helper_ran_leaves_no_recorded_owner_so_the_next_start_sets_it() {
    if !ready() {
        return;
    }
    let world = World::new();
    let mut db = mounting("db", "exec sleep 60");
    db.container.user = Some("1000:1000".to_owned());
    let plan = world.plan(vec![db]);
    world.up(&plan);
    // phase records the volume without an owner before a helper starts, so
    // a run that ends while the helper runs leaves this record and the
    // helper behind.
    let store = RunStore::new(world.state.path());
    let mut journal = Journal::open(&store, &world.id, std::time::Duration::from_secs(5)).unwrap();
    journal
        .started(
            &ObjectKey::Volume(world.data_volume().parse().unwrap()),
            Started::Volume(None),
        )
        .unwrap();
    drop(journal);
    world.chown_data("0:0");
    world.leave_helper("db");

    let report = world.restart(&plan);

    assert_eq!(outcome(&report, "db"), &AppOutcome::Restarted, "{report:?}");
    assert!(world.helpers().is_empty(), "{:?}", world.helpers());
    assert_eq!(world.data_owner(), "1000:1000");
    assert_eq!(world.recorded_owner(), Some("1000:1000".parse().unwrap()));
    world.down();
}

#[test]
fn down_and_sweep_remove_a_leftover_helper() {
    if !ready() {
        return;
    }
    let world = World::new();
    let mut db = mounting("db", WRITES);
    db.container.user = Some("1000:1000".to_owned());
    let plan = world.plan(vec![db]);
    world.up(&plan);
    world.leave_helper("db");

    world.phase.sweep(&world.id, false).unwrap();

    assert!(world.helpers().is_empty(), "{:?}", world.helpers());

    world.up(&plan);
    world.leave_helper("db");

    let report = world.down();

    assert_eq!(outcome(&report, "db"), &AppOutcome::Stopped, "{report:?}");
    assert!(world.helpers().is_empty(), "{:?}", world.helpers());
}

/// A Dockerfile whose image ships `/data` holding a file and owned by
/// `999:999`.
const SHIPS_DATA: &str = "RUN mkdir -p /data && echo seed > /data/seed && chown -R 999:999 /data\n";

/// A Dockerfile whose image ships an empty `/data` owned by `999:999`.
const SHIPS_EMPTY_DATA: &str = "RUN mkdir -p /data && chown 999:999 /data\n";

/// Returns an app `db` on an image built with `lines` after `FROM`, whose
/// `server` runs as `1000:1000`, mounts the volume `data` at `/data` and
/// runs `script`, or `None` when the image did not build.
fn on_built_image(lines: &str, script: &str) -> Option<(BuiltImage, AppPlan)> {
    let image = BuiltImage::build(&format!("FROM {IMAGE}\n{lines}"))?;
    let mut db = mounting("db", script);
    db.source = Source::image(image.tag.parse().unwrap());
    db.container.user = Some("1000:1000".to_owned());
    Some((image, db))
}

#[test]
fn the_first_up_of_an_image_with_a_directory_at_the_mount_path_gives_the_volume_the_wanted_owner() {
    if !ready() {
        return;
    }
    let Some((_image, db)) = on_built_image(SHIPS_DATA, WRITES) else {
        return;
    };
    let world = World::new();
    let plan = world.plan(vec![db]);

    let report = world.up(&plan);

    assert_eq!(outcome(&report, "db"), &AppOutcome::Started, "{report:?}");
    assert_eq!(world.data_owner(), "1000:1000");
    assert_eq!(world.recorded_owner(), Some("1000:1000".parse().unwrap()));
    assert!(world.helpers().is_empty(), "{:?}", world.helpers());
    world.down();
}

#[test]
fn an_owner_of_keep_runs_no_helper_and_leaves_the_owner_docker_copied_from_the_image() {
    if !ready() {
        return;
    }
    let Some((_image, mut db)) = on_built_image(SHIPS_DATA, "exec sleep 60") else {
        return;
    };
    db.container.owner = Some("keep".to_owned());
    let world = World::new();
    let plan = world.plan(vec![db]);

    let report = world.up(&plan);

    assert_eq!(outcome(&report, "db"), &AppOutcome::Started, "{report:?}");
    assert_eq!(world.data_owner(), "999:999");
    assert_eq!(world.recorded_owner(), None);
    world.down();
}

#[test]
fn a_second_process_that_mounts_a_still_empty_volume_leaves_it_with_the_wanted_owner() {
    if !ready() {
        return;
    }
    let Some((_image, mut db)) = on_built_image(SHIPS_EMPTY_DATA, "exec sleep 60") else {
        return;
    };
    let mut second = db.processes[0].clone();
    second.name = "worker".parse().unwrap();
    db.processes.push(second);
    let world = World::new();
    let plan = world.plan(vec![db]);

    let report = world.up(&plan);

    assert_eq!(outcome(&report, "db"), &AppOutcome::Started, "{report:?}");
    assert_eq!(world.data_owner(), "1000:1000");
    world.down();
}

#[test]
fn an_up_of_a_running_app_corrects_an_owner_changed_behind_phases_back() {
    if !ready() {
        return;
    }
    let world = World::new();
    let mut db = mounting("db", "exec sleep 60");
    db.container.user = Some("1000:1000".to_owned());
    let plan = world.plan(vec![db]);
    world.up(&plan);
    assert_eq!(world.recorded_owner(), Some("1000:1000".parse().unwrap()));
    world.chown_data("4321:4321");

    let report = world.up(&plan);

    assert_eq!(
        outcome(&report, "db"),
        &AppOutcome::AlreadyRunning,
        "{report:?}"
    );
    assert_eq!(world.data_owner(), "1000:1000");
    assert_eq!(world.recorded_owner(), Some("1000:1000".parse().unwrap()));
    assert!(world.helpers().is_empty(), "{:?}", world.helpers());

    world.chown_data("4321:4321");
    let report = world.restart(&plan);

    assert_eq!(outcome(&report, "db"), &AppOutcome::Restarted, "{report:?}");
    assert_eq!(world.data_owner(), "1000:1000");
    world.down();
}

#[test]
fn an_owner_of_keep_leaves_an_owner_changed_behind_phases_back_on_a_second_up() {
    if !ready() {
        return;
    }
    let world = World::new();
    let mut db = mounting("db", "exec sleep 60");
    db.container.user = Some("1000:1000".to_owned());
    db.container.owner = Some("keep".to_owned());
    let plan = world.plan(vec![db]);
    world.up(&plan);
    world.chown_data("4321:4321");

    let report = world.up(&plan);

    assert_eq!(
        outcome(&report, "db"),
        &AppOutcome::AlreadyRunning,
        "{report:?}"
    );
    assert_eq!(world.data_owner(), "4321:4321");
    assert_eq!(world.recorded_owner(), None);
    world.down();
}

#[test]
fn an_up_of_a_running_app_keeps_the_owner_of_its_container_when_the_images_user_moves() {
    if !ready() {
        return;
    }
    let Some(first) = BuiltImage::build(&format!("FROM {IMAGE}\nUSER 1000:1000\n")) else {
        return;
    };
    let Some(moved) = BuiltImage::build(&format!("FROM {IMAGE}\nUSER 2000:2000\n")) else {
        return;
    };
    let world = World::new();
    let tag = format!("phase-test-moving-{}:test", world.id);
    docker(&["tag", &first.tag, &tag]).unwrap();
    let mut db = mounting("db", "exec sleep 60");
    db.source = Source::image(tag.parse().unwrap());
    let plan = world.plan(vec![db]);
    world.up(&plan);
    assert_eq!(world.data_owner(), "1000:1000");
    docker(&["tag", &moved.tag, &tag]).unwrap();

    let report = world.up(&plan);

    assert_eq!(
        outcome(&report, "db"),
        &AppOutcome::AlreadyRunning,
        "{report:?}"
    );
    assert_eq!(world.data_owner(), "1000:1000");
    world.down();
    docker(&["image", "rm", &tag]).unwrap();
}

/// An image that no registry has, so a pull of it fails.
const UNPULLABLE: &str =
    "busybox@sha256:0000000000000000000000000000000000000000000000000000000000000000";

impl World {
    /// Makes every later `up` of this world fail to get the helper image.
    fn lose_helper_image(&mut self) {
        self.phase = Phase::new(self.state.path().to_path_buf())
            .with_start_grace(std::time::Duration::from_millis(500))
            .with_volume_helper_image(UNPULLABLE.to_owned());
    }

    /// Reports whether no file of the state directory holds `text`.
    fn state_lacks(&self, text: &str) -> bool {
        let path = RunStore::new(self.state.path()).path(&self.id);
        std::fs::read_to_string(path).map_or(true, |record| !record.contains(text))
    }
}

/// Returns the notes of the app `app` in `report`.
fn notes<'r>(report: &'r Report, app: &str) -> &'r [Note] {
    &report
        .apps
        .iter()
        .find(|report| report.app.as_str() == app)
        .unwrap()
        .notes
}

/// Asserts that `up` of `db` failed as unresolved, naming the two ways out
/// and nothing of the image's `USER`.
fn assert_unresolved(world: &World, report: &Report) {
    let json = serde_json::to_string(report).unwrap();
    assert!(!json.contains("[31m"), "{json}");
    assert!(!json.contains("\\u001b"), "{json}");
    assert!(!json.contains("no-such"), "{json}");
    assert!(world.state_lacks("no-such"));
    match outcome(report, "db") {
        AppOutcome::Errored { code, message, .. } => {
            assert_eq!(
                code, "phase::container::volume_owner_unresolved",
                "{message}"
            );
            assert!(message.contains("the image's USER"), "{message}");
            assert!(message.contains("numeric uid:gid"), "{message}");
            assert!(message.contains("keep"), "{message}");
            assert!(message.contains("refused to create or start"), "{message}");
            assert!(!message.contains("no id program"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(!world.has_containers());
    assert!(world.helpers().is_empty(), "{:?}", world.helpers());
}

/// Returns an app `db` whose image has the `USER` line `user` and which
/// mounts the volume `data`, with `owner` as its volume owner, or `None`
/// when the image did not build.
fn with_image_user(user: &str, owner: &str) -> Option<(BuiltImage, AppPlan)> {
    let image = BuiltImage::build(&format!("FROM {IMAGE}\nUSER {user}\n"))?;
    let mut db = mounting("db", WRITES);
    db.source = Source::image(image.tag.parse().unwrap());
    db.container.owner = Some(owner.to_owned());
    Some((image, db))
}

/// Asserts that `report` holds an error for `db` with no text from an
/// image `USER` that holds `MARK` and an escape sequence, and with no
/// failure class chosen by that text.
fn assert_image_user_hidden(report: &Report) {
    let json = serde_json::to_string(report).unwrap();
    assert!(!json.contains("MARK"), "{json}");
    assert!(!json.contains("[31m"), "{json}");
    assert!(!json.contains("\\u001b"), "{json}");
    assert!(!json.contains("no_address_pool"), "{json}");
    assert!(!json.contains("fully subnetted"), "{json}");
    assert!(!json.contains("already allocated"), "{json}");
    match outcome(report, "db") {
        AppOutcome::Errored { message, .. } => {
            assert!(message.contains("the image's USER"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_images_group_absent_from_its_group_file_never_reaches_the_report() {
    if !ready() {
        return;
    }
    for owner in ["keep", "1000:1000"] {
        let Some((_image, db)) = with_image_user("1000:MARK\u{1b}[31mgroup", owner) else {
            return;
        };
        let world = World::new();

        let report = world.up(&world.plan(vec![db]));

        assert_image_user_hidden(&report);
    }
}

#[test]
fn an_images_group_absent_from_its_group_file_never_reaches_the_report_without_a_volume() {
    if !ready() {
        return;
    }
    let Some((_image, mut db)) = with_image_user("1000:MARK\u{1b}[31mgroup", "keep") else {
        return;
    };
    db.processes[0].volumes.clear();
    let world = World::new();

    let report = world.up(&world.plan(vec![db]));

    assert_image_user_hidden(&report);
}

#[test]
fn an_images_user_that_holds_a_compose_failure_phrase_chooses_no_failure_class() {
    if !ready() {
        return;
    }
    for phrase in [
        "all predefined address pools have been fully subnetted",
        "port is already allocated",
    ] {
        let Some((_image, db)) = with_image_user(&format!("MARK\u{1b}[31m {phrase}"), "keep")
        else {
            return;
        };
        let world = World::new();

        let report = world.up(&world.plan(vec![db]));

        assert_image_user_hidden(&report);
    }
}

#[test]
fn an_images_user_with_a_third_field_never_reaches_the_report_or_chooses_a_failure_class() {
    if !ready() {
        return;
    }
    for group in [
        "MARK\u{1b}[31mgroup",
        "MARK\u{1b}[31m port is already allocated",
        "MARK\u{1b}[31m all predefined address pools have been fully subnetted",
    ] {
        let Some((_image, db)) = with_image_user(&format!("1000:{group}:tail"), "keep") else {
            return;
        };
        let world = World::new();

        let report = world.up(&world.plan(vec![db]));

        assert_image_user_hidden(&report);
    }
}

#[test]
fn an_images_user_that_is_a_word_of_dockers_sentence_leaves_the_sentence_whole() {
    if !ready() {
        return;
    }
    let Some((_image, db)) = with_image_user("daemon:nosuchgroup", "keep") else {
        return;
    };
    let world = World::new();

    let report = world.up(&world.plan(vec![db]));

    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("nosuchgroup"), "{json}");
    assert!(json.contains("docker_line"), "{json}");
    match outcome(&report, "db") {
        AppOutcome::Errored { message, .. } => {
            assert!(
                message.contains(
                    "Error response from daemon: unable to find group <the image's USER>:"
                ),
                "{message}"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_plans_run_as_user_docker_cannot_find_fails_as_unresolved_and_keeps_dockers_line() {
    if !ready() {
        return;
    }
    let world = World::new();
    let mut db = mounting("db", WRITES);
    db.container.user = Some("no-such-plan-user".to_owned());
    let plan = world.plan(vec![db]);

    let report = world.up(&plan);

    match outcome(&report, "db") {
        AppOutcome::Errored { code, message, .. } => {
            assert_eq!(
                code, "phase::container::volume_owner_unresolved",
                "{message}"
            );
            assert!(
                message.contains("unable to find user no-such-plan-user"),
                "{message}"
            );
            assert!(message.contains("keep"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(world.helpers().is_empty(), "{:?}", world.helpers());
}

#[test]
fn an_up_that_starts_one_process_and_cannot_check_the_volume_of_a_running_one_starts_it_with_a_note()
 {
    if !ready() {
        return;
    }
    let mut world = World::new();
    let mut db = mounting("db", "exec sleep 60");
    db.container.user = Some("1000:1000".to_owned());
    let mut worker = db.processes[0].clone();
    worker.name = "worker".parse().unwrap();
    worker.volumes.clear();
    db.processes.push(worker);
    let plan = world.plan(vec![db]);
    assert_eq!(outcome(&world.up(&plan), "db"), &AppOutcome::Started);
    let server = world.container("db", "server").unwrap();
    let worker = world.container("db", "worker").unwrap();
    docker(&["rm", "--force", "--", &worker]).unwrap();
    world.lose_helper_image();

    let report = world.up(&plan);

    assert_eq!(outcome(&report, "db"), &AppOutcome::Restarted, "{report:?}");
    assert!(
        matches!(notes(&report, "db"), [Note::VolumeOwnerUnchecked { .. }]),
        "{report:?}"
    );
    assert_eq!(world.container("db", "server").unwrap(), server);
    assert_ne!(world.container("db", "worker").unwrap(), worker);
    world.down();
}

#[test]
fn an_images_user_absent_from_its_passwd_file_never_reaches_the_report_or_the_record() {
    if !ready() {
        return;
    }
    let Some(image) = BuiltImage::build(&format!("FROM {IMAGE}\nUSER no-such\u{1b}[31muser\n"))
    else {
        return;
    };
    let world = World::new();
    let mut db = mounting("db", WRITES);
    db.source = Source::image(image.tag.parse().unwrap());
    let plan = world.plan(vec![db]);

    let report = world.up(&plan);

    assert_unresolved(&world, &report);
}

#[test]
fn an_images_user_absent_from_its_passwd_file_never_reaches_the_report_when_no_id_helper_runs() {
    if !ready() {
        return;
    }
    let Some(image) = BuiltImage::build(&format!("FROM {IMAGE}\nUSER no-such\u{1b}[31muser\n"))
    else {
        return;
    };
    let world = World::new();
    let mut db = mounting("db", WRITES);
    db.source = Source::image(image.tag.parse().unwrap());
    db.container.owner = Some("keep".to_owned());
    let plan = world.plan(vec![db]);

    let report = world.up(&plan);

    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("[31m"), "{json}");
    assert!(!json.contains("no-such"), "{json}");
    assert!(world.state_lacks("no-such"));
    match outcome(&report, "db") {
        AppOutcome::Errored { message, .. } => {
            assert!(message.contains("the image's USER"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_image_with_no_id_program_fails_as_unresolved_and_names_a_numeric_owner_or_keep() {
    if !ready() {
        return;
    }
    let Some(image) = BuiltImage::build(&format!(
        "FROM {IMAGE}\nRUN adduser -D -u 1234 app && rm /bin/id\nUSER app\n"
    )) else {
        return;
    };
    let world = World::new();
    let mut db = mounting("db", WRITES);
    db.source = Source::image(image.tag.parse().unwrap());
    let plan = world.plan(vec![db]);

    let report = world.up(&plan);

    assert_unresolved(&world, &report);
}

#[test]
fn a_helper_image_that_cannot_be_pulled_leaves_the_old_container_running() {
    if !ready() {
        return;
    }
    let mut world = World::new();
    let mut db = mounting("db", "exec sleep 60");
    db.container.user = Some("1000:1000".to_owned());
    db.container.owner = Some("keep".to_owned());
    assert_eq!(
        outcome(&world.up(&world.plan(vec![db.clone()])), "db"),
        &AppOutcome::Started
    );
    let before = world.container("db", "server").unwrap();
    db.container.owner = Some("1000:1000".to_owned());
    world.lose_helper_image();

    let report = world.up(&world.plan(vec![db]));

    assert!(
        matches!(outcome(&report, "db"), AppOutcome::Errored { .. }),
        "{report:?}"
    );
    assert_eq!(world.container("db", "server").unwrap(), before);
    assert_eq!(
        docker(&[
            "container",
            "inspect",
            "--format",
            "{{.State.Status}}",
            &before
        ])
        .unwrap()
        .trim(),
        "running"
    );
    world.down();
}

#[test]
fn an_up_of_a_running_app_that_cannot_get_the_helper_reports_already_running_with_a_note() {
    if !ready() {
        return;
    }
    let mut world = World::new();
    let mut db = mounting("db", "exec sleep 60");
    db.container.user = Some("1000:1000".to_owned());
    let plan = world.plan(vec![db]);
    world.up(&plan);
    world.lose_helper_image();

    let report = world.up(&plan);

    assert_eq!(
        outcome(&report, "db"),
        &AppOutcome::AlreadyRunning,
        "{report:?}"
    );
    assert!(
        matches!(notes(&report, "db"), [Note::VolumeOwnerUnchecked { .. }]),
        "{report:?}"
    );
    world.down();
}

#[test]
fn an_up_of_a_running_app_whose_image_is_gone_reports_already_running_with_a_note() {
    if !ready() {
        return;
    }
    let Some(image) = BuiltImage::build(&format!("FROM {IMAGE}\nLABEL gone=soon\n")) else {
        return;
    };
    let world = World::new();
    let mut db = mounting("db", "exec sleep 60");
    db.source = Source::image(image.tag.parse().unwrap());
    let plan = world.plan(vec![db]);
    world.up(&plan);
    docker(&["image", "rm", "--force", &image.tag]).unwrap();

    let report = world.up(&plan);

    assert_eq!(
        outcome(&report, "db"),
        &AppOutcome::AlreadyRunning,
        "{report:?}"
    );
    assert!(
        matches!(notes(&report, "db"), [Note::VolumeOwnerUnchecked { .. }]),
        "{report:?}"
    );
    world.down();
}

#[test]
fn an_up_of_a_running_app_whose_owner_it_can_check_adds_no_note() {
    if !ready() {
        return;
    }
    let world = World::new();
    let mut db = mounting("db", "exec sleep 60");
    db.container.user = Some("1000:1000".to_owned());
    let plan = world.plan(vec![db]);
    world.up(&plan);

    let report = world.up(&plan);

    assert_eq!(
        outcome(&report, "db"),
        &AppOutcome::AlreadyRunning,
        "{report:?}"
    );
    assert!(notes(&report, "db").is_empty(), "{report:?}");
    world.down();
}

#[test]
fn a_chown_helper_that_fails_after_the_container_is_created_ends_the_start_and_removes_it() {
    if !ready() {
        return;
    }
    let world = World::new();
    let mut db = mounting("db", "exec sleep 60");
    db.container.user = Some("1000:1000".to_owned());
    let plan = world.plan(vec![db]);
    world.up(&plan);
    let volume = world.data_volume();
    let labels = docker(&[
        "volume",
        "inspect",
        "--format",
        "{{range $k, $v := .Labels}}{{$k}}={{$v}}\n{{end}}",
        &volume,
    ])
    .unwrap();
    world.down();
    docker(&["volume", "rm", &volume]).unwrap();
    // A volume mounted read-only makes `chown` fail after phase has
    // created the app's container.
    let mut create = vec![
        "volume",
        "create",
        "--opt",
        "type=tmpfs",
        "--opt",
        "device=tmpfs",
        "--opt",
        "o=ro",
    ];
    for label in labels.lines().filter(|line| !line.is_empty()) {
        create.extend(["--label", label]);
    }
    create.push(&volume);
    docker(&create).unwrap();

    let report = world.up(&plan);

    match outcome(&report, "db") {
        AppOutcome::Errored { message, .. } => {
            assert!(message.contains("exited with status 1"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(!world.has_containers());
    assert!(world.helpers().is_empty(), "{:?}", world.helpers());
    docker(&["volume", "rm", &volume]).unwrap();
}

#[test]
fn deleting_a_workspaces_app_volumes_removes_a_leftover_helper_first() {
    if !ready() {
        return;
    }
    let world = World::new();
    let mut db = mounting("db", "exec sleep 60");
    db.container.user = Some("1000:1000".to_owned());
    let plan = world.plan(vec![db]);
    world.up(&plan);
    let volume = world.data_volume();
    let id = world.container("db", "server").unwrap();
    docker(&["rm", "--force", "--", &id]).unwrap();
    world.leave_helper("db");

    delete_app_volumes(app_volumes(&world.phase, &world.id).unwrap()).unwrap();

    assert!(world.helpers().is_empty(), "{:?}", world.helpers());
    assert!(docker(&["volume", "inspect", &volume]).is_none());
}

#[test]
fn what_id_prints_in_the_image_never_reaches_the_report() {
    if !ready() {
        return;
    }
    for (status, code) in [
        (3, "phase::container::volume_owner_unresolved"),
        (0, "phase::container::volume_owner_unresolved"),
    ] {
        let Some(image) = BuiltImage::build(&format!(
            "FROM {IMAGE}\n\
             RUN adduser -D -u 1234 app && rm /bin/id && \
             printf '#!/bin/sh\\necho password=made-up-4f2a\\necho password=made-up-4f2a >&2\\nexit {status}\\n' > /bin/id && \
             chmod 755 /bin/id\n\
             USER app\n"
        )) else {
            return;
        };
        let world = World::new();
        let mut db = mounting("db", WRITES);
        db.source = Source::image(image.tag.parse().unwrap());
        let plan = world.plan(vec![db]);

        let report = world.up(&plan);

        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("made-up"), "{json}");
        assert!(!json.contains("password"), "{json}");
        match outcome(&report, "db") {
            AppOutcome::Errored {
                code: found,
                message,
                ..
            } => {
                assert_eq!(found, code, "{message}");
                if status == 3 {
                    assert!(message.contains("status 3"), "{message}");
                }
            }
            other => panic!("{other:?}"),
        }
        assert!(world.helpers().is_empty(), "{:?}", world.helpers());
    }
}

#[test]
fn an_images_user_that_holds_an_escape_sequence_never_reaches_the_report() {
    if !ready() {
        return;
    }
    // Docker resolves the user in the image's /etc/passwd before `id`
    // runs, so the name with the escape sequence is a real user there,
    // and the image's `id` fails as one would for a name it cannot read.
    let Some(image) = BuiltImage::build(&format!(
        "FROM {IMAGE}\n\
         RUN printf 'ev\\033[31mil:x:1234:1234::/:/bin/sh\\n' >> /etc/passwd && \
         rm /bin/id && printf '#!/bin/sh\\nexit 3\\n' > /bin/id && chmod 755 /bin/id\n\
         USER ev\u{1b}[31mil\n"
    )) else {
        return;
    };
    let world = World::new();
    let mut db = mounting("db", WRITES);
    db.source = Source::image(image.tag.parse().unwrap());
    let plan = world.plan(vec![db]);

    let report = world.up(&plan);

    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("[31m"), "{json}");
    assert!(!json.contains("\\u001b"), "{json}");
    match outcome(&report, "db") {
        AppOutcome::Errored { code, message, .. } => {
            assert_eq!(code, "phase::container::volume_owner_unresolved");
            assert!(message.contains("the image's USER"), "{message}");
            assert!(message.contains("status 3"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(!world.has_containers());
    assert!(world.helpers().is_empty(), "{:?}", world.helpers());
}

#[test]
fn a_second_state_directory_with_the_same_id_neither_lists_deletes_nor_mounts_the_first_ones_volume()
 {
    if !ready() {
        return;
    }
    let world = World::new();
    let plan = world.plan(vec![mounting("db", "exec sleep 60")]);
    world.up(&plan);
    let volume = world.data_volume();
    let (_state, other) = world.second_state();

    let listed = app_volumes(&other, &world.id).unwrap();
    assert!(listed.names().is_empty(), "{listed:?}");
    delete_app_volumes(listed).unwrap();
    assert!(docker(&["volume", "inspect", &volume]).is_some());

    let halted = world.up_under(&other, &plan);
    World::assert_foreign(&halted, &volume);
    assert!(world.container("db", "server").is_some());

    let own = app_volumes(&world.phase, &world.id).unwrap();
    assert_eq!(own.names(), [volume.clone()]);
    world.down();
    delete_app_volumes(own).unwrap();
    assert!(docker(&["volume", "inspect", &volume]).is_none());
}

#[test]
fn a_second_state_directory_with_the_same_id_never_replaces_the_first_ones_container() {
    if !ready() {
        return;
    }
    let world = World::new();
    let plan = world.plan(vec![app("db", "exec sleep 60")]);
    world.up(&plan);
    let id = world.container("db", "server").unwrap();
    let (_state, other) = world.second_state();

    let halted = world.up_under(&other, &plan);

    World::assert_foreign(&halted, "-db.server");
    assert_eq!(world.container("db", "server"), Some(id.clone()));
    let state = docker(&[
        "container",
        "inspect",
        "--format",
        "{{.State.Running}}",
        &id,
    ]);
    assert_eq!(state.as_deref().map(str::trim), Some("true"));
    world.down();
}
