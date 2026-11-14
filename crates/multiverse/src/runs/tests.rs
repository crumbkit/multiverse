use std::fs;

use crate::testing::{unmade_clone, write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::Error;
use crate::manifest::Manifest;
use crate::name::WorkspaceName;
use crate::record::{Checkout, ID_RECORD_VERSION, Workspace};
use crate::runs::phased::saved_workspace_id;
use crate::runs::{DownOutcome, ProcessState, Runs};
use crate::store::{Created, Store};

/// A temporary directory with a store, run files and a checkout.
struct Setup {
    dir: tempfile::TempDir,
}

impl Setup {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn store(&self) -> Store {
        Store::new(self.dir.path().join("workspaces"))
    }

    fn runs(&self) -> Runs {
        Runs::new(self.dir.path()).with_grace(Duration::from_millis(500))
    }

    fn workspace(&self, name: &str) -> Workspace {
        let name: WorkspaceName = name.parse().unwrap();
        let path = self.dir.path().join(name.as_str());
        fs::create_dir_all(path.join("app")).unwrap();
        let checkout = Checkout::new(
            "app".to_owned(),
            &PathBuf::from("/nowhere/app"),
            path.join("app"),
            name.to_string(),
            "origin/main".to_owned(),
            unmade_clone(),
        );
        let mut workspace = Workspace::new(name, path, vec![checkout]);
        assert_eq!(
            self.store().create(&mut workspace).unwrap(),
            Created::Written
        );
        workspace
    }
}

fn manifest(text: &str) -> Manifest {
    Manifest::parse(text, &PathBuf::from("mxv.toml")).unwrap()
}

#[test]
fn a_workspace_that_never_started_reports_its_manifest_processes_as_stopped() {
    let setup = Setup::new();
    let workspace = setup.workspace("triage");
    let manifest = manifest("[processes.a]\nargv = [\"true\"]\n");
    let statuses = setup.runs().status(&workspace, Some(&manifest)).unwrap();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].state, ProcessState::Stopped);
    assert!(setup.runs().status(&workspace, None).unwrap().is_empty());
}

#[test]
fn down_of_a_name_that_no_process_has_returns_the_name() {
    let setup = Setup::new();
    let workspace = setup.workspace("triage");
    let runs = setup.runs();
    let outcome = runs
        .down(
            &runs.lock(&workspace.name).unwrap(),
            &workspace,
            None,
            &["web".to_owned()],
            crate::runs::Needed::Keep,
        )
        .unwrap();
    assert!(matches!(outcome, DownOutcome::UnknownProcess(name) if name == "web"));
}

#[test]
fn a_second_lock_of_a_workspace_fails_with_busy_while_the_first_is_held() {
    let setup = Setup::new();
    let workspace = setup.workspace("triage");
    let runs = setup.runs().with_lock_wait(Duration::from_millis(100));
    let held = runs.lock(&workspace.name).unwrap();
    assert!(matches!(
        runs.lock(&workspace.name),
        Err(crate::error::Error::Busy { .. })
    ));
    let other = setup.workspace("review");
    assert!(runs.lock(&other.name).is_ok());
    drop(held);
    assert!(runs.lock(&workspace.name).is_ok());
}

#[test]
fn the_run_files_sit_in_the_runs_directory_of_mxvs_state_directory() {
    let state = Path::new("/state/mxv");
    let name: WorkspaceName = "triage".parse().unwrap();

    assert_eq!(
        Runs::new(state).workspace_dir(&name),
        Path::new("/state/mxv/runs/triage")
    );
}

#[test]
fn up_saves_a_stable_id_in_a_record_without_one_and_then_reads_the_saved_id() {
    let setup = Setup::new();
    let store = setup.store();
    let mut workspace = setup.workspace("triage");
    workspace.id = None;
    // A record that an earlier mxv wrote holds no stable id.
    let path = setup.dir.path().join("workspaces/triage.json");
    let mut record: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    record.as_object_mut().unwrap().remove("id");
    record["version"] = 2.into();
    write(&path, record.to_string());
    let id = saved_workspace_id(&store, &workspace.name).unwrap();
    let saved = store.read(&workspace.name).unwrap().unwrap();
    assert_eq!(saved.id.as_ref(), Some(&id));
    assert_eq!(saved.version, ID_RECORD_VERSION);
    assert_eq!(saved_workspace_id(&store, &workspace.name).unwrap(), id);
}

#[test]
fn a_workspace_without_a_record_gives_no_stable_id_and_says_what_mxv_cannot_do() {
    let setup = Setup::new();
    let store = setup.store();
    let name: WorkspaceName = "triage".parse().unwrap();
    let saved = saved_workspace_id(&store, &name);
    assert!(
        matches!(
            saved,
            Err(Error::DockerFailed { ref operation, .. }) if operation == "workspace id"
        ),
        "{saved:?}"
    );
    let text = saved.unwrap_err().to_string();
    assert!(
        text.starts_with("mxv cannot name the workspace's containers, because "),
        "{text}"
    );
}

#[test]
fn a_touch_records_when_an_mxv_command_last_named_the_workspace() {
    let setup = Setup::new();
    let runs = setup.runs();
    let name: WorkspaceName = "triage".parse().unwrap();
    assert_eq!(runs.touched(&name), None);
    let before = std::time::SystemTime::now() - Duration::from_secs(1);
    runs.touch(&name).unwrap();
    let touched = runs.touched(&name).unwrap();
    assert!(touched >= before, "{touched:?}");
}

#[test]
fn a_keeper_touches_the_workspace_again_until_it_is_dropped() {
    let setup = Setup::new();
    let runs = setup.runs();
    let name: WorkspaceName = "triage".parse().unwrap();
    let keeper = runs.keep_touched(&name, Duration::from_millis(20));
    let first = runs.touched(&name).unwrap();
    let later = (0..250).find_map(|_| {
        #[expect(
            clippy::disallowed_methods,
            reason = "the test lets real time pass, because the keeper touches on a real clock"
        )]
        std::thread::sleep(Duration::from_millis(20));
        runs.touched(&name).filter(|touched| *touched > first)
    });
    assert!(
        later.is_some(),
        "the keeper did not touch the workspace again"
    );
    drop(keeper);
}
