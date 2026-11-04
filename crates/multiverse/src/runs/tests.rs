use std::fs;

use crate::testing::write;
use std::io::Write as _;
use std::net::{Ipv4Addr, TcpListener};
use std::path::PathBuf;
use std::time::Duration;

use crate::manifest::Manifest;
use crate::name::WorkspaceName;
use crate::record::{Checkout, Workspace};
use crate::runs::{
    DownOutcome, EXIT_FILE_WAIT, ExitReason, Follower, ProcessState, Runs, UpOutcome, tail,
};
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
        Runs::new(self.dir.path().join("runs")).with_grace(Duration::from_millis(500))
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
        );
        let workspace = Workspace::new(name, path, vec![checkout]);
        assert_eq!(self.store().create(&workspace).unwrap(), Created::Written);
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
fn up_allocates_each_port_once_and_writes_it_to_the_record() {
    let setup = Setup::new();
    let mut workspace = setup.workspace("triage");
    let manifest = manifest(
        "ports = [\"api\"]\n[processes.a]\nargv = [\"sleep\", \"30\"]\nports = { PORT = \"api\" }\n",
    );
    let checkout = workspace.checkouts[0].path.clone();
    let runs = setup.runs();
    assert!(matches!(
        runs.up(
            &runs.lock(&workspace.name).unwrap(),
            &setup.store(),
            &mut workspace,
            &checkout,
            &manifest,
            &[]
        )
        .unwrap(),
        UpOutcome::Up(_)
    ));
    let port = workspace.ports["api"];
    let stored = setup.store().read(&workspace.name).unwrap().unwrap();
    assert_eq!(stored.ports["api"], port);
    assert!(matches!(
        runs.down(
            &runs.lock(&workspace.name).unwrap(),
            &workspace,
            Some(&manifest),
            &[]
        )
        .unwrap(),
        DownOutcome::Down(_)
    ));
}

#[test]
fn a_process_that_only_receives_its_readiness_port_is_ready_when_that_port_accepts() {
    let setup = Setup::new();
    let mut workspace = setup.workspace("triage");
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    workspace
        .ports
        .insert("api".to_owned(), listener.local_addr().unwrap().port());
    let manifest = manifest(
        "ports = [\"api\"]\n[processes.a]\nargv = [\"sleep\", \"30\"]\nports = { PORT = \"api\" }\nready = { port = \"api\", timeout_seconds = 5 }\n",
    );
    let checkout = workspace.checkouts[0].path.clone();
    let runs = setup.runs();
    let outcome = runs
        .up(
            &runs.lock(&workspace.name).unwrap(),
            &setup.store(),
            &mut workspace,
            &checkout,
            &manifest,
            &[],
        )
        .unwrap();
    let UpOutcome::Up(statuses) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(matches!(statuses[0].state, ProcessState::Running { .. }));
    runs.down(
        &runs.lock(&workspace.name).unwrap(),
        &workspace,
        Some(&manifest),
        &[],
    )
    .unwrap();
}

#[test]
fn down_kills_a_process_that_ignores_sigterm_after_the_grace() {
    let setup = Setup::new();
    let mut workspace = setup.workspace("triage");
    let manifest = manifest(
        "[processes.stubborn]\nargv = [\"sh\", \"-c\", \"trap '' TERM; while :; do sleep 1; done\"]\n",
    );
    let checkout = workspace.checkouts[0].path.clone();
    let runs = setup.runs();
    runs.up(
        &runs.lock(&workspace.name).unwrap(),
        &setup.store(),
        &mut workspace,
        &checkout,
        &manifest,
        &[],
    )
    .unwrap();
    let DownOutcome::Down(stopped) = runs
        .down(
            &runs.lock(&workspace.name).unwrap(),
            &workspace,
            Some(&manifest),
            &[],
        )
        .unwrap()
    else {
        panic!("down did not stop the process");
    };
    assert_eq!(stopped[0].as_ref().unwrap().state, ProcessState::Stopped);
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
        )
        .unwrap();
    assert!(matches!(outcome, DownOutcome::UnknownProcess(name) if name == "web"));
}

#[test]
fn a_tail_returns_the_last_lines_of_a_log() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.log");
    write(&path, "one\ntwo\nthree\n");
    assert_eq!(tail(&path, 2), ["two", "three"]);
    assert!(tail(&dir.path().join("missing.log"), 2).is_empty());
}

#[test]
fn a_follower_returns_each_complete_line_written_after_it_opened() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.log");
    write(&path, "before\n");
    let mut follower = Follower::at_end(&path).unwrap().unwrap();
    let mut file = crate::store::append_file(&path).unwrap();
    file.write_all(b"first\nsec").unwrap();
    assert_eq!(follower.lines().unwrap(), ["first"]);
    file.write_all(b"ond\n").unwrap();
    assert_eq!(follower.lines().unwrap(), ["second"]);
    assert!(
        Follower::at_end(&dir.path().join("missing.log"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_run_record_whose_process_id_now_names_another_process_reads_as_exited() {
    let setup = Setup::new();
    let workspace = setup.workspace("triage");
    let dir = setup.runs().workspace_dir(&workspace.name);
    fs::create_dir_all(&dir).unwrap();
    let record = serde_json::json!({
        "version": 1,
        "pid": std::process::id(),
        "started": "Thu Jan  1 00:00:00 1970",
        "ports": {},
    });
    write(dir.join("a.json"), record.to_string());
    let statuses = setup.runs().status(&workspace, None).unwrap();
    assert_eq!(
        statuses[0].state,
        ProcessState::Exited {
            code: None,
            signal: None,
            reason: Some(ExitReason::NoExitFile),
        }
    );
}

/// Writes the run record of the process `a` of `workspace`, naming this
/// test's own process, with the start time `started`, and returns the run
/// directory.
fn record_this_process(setup: &Setup, workspace: &Workspace, started: &str) -> PathBuf {
    let dir = setup.runs().workspace_dir(&workspace.name);
    fs::create_dir_all(&dir).unwrap();
    let record = serde_json::json!({
        "version": 1,
        "pid": std::process::id(),
        "started": started,
        "ports": {},
    });
    write(dir.join("a.json"), record.to_string());
    dir
}

#[test]
fn a_running_process_whose_exit_file_the_shell_has_opened_but_not_written_reads_as_running() {
    let setup = Setup::new();
    let workspace = setup.workspace("triage");
    let started = crate::process::start_time(std::process::id())
        .unwrap()
        .unwrap();
    let dir = record_this_process(&setup, &workspace, &started);
    // `printf ... > file` creates the file empty before it writes the code.
    write(dir.join("a.exit"), "");
    let statuses = setup.runs().status(&workspace, None).unwrap();
    assert_eq!(
        statuses[0].state,
        ProcessState::Running {
            pid: Some(std::process::id())
        }
    );
}

#[test]
fn status_waits_for_an_exit_file_that_the_shell_writes_after_its_group_is_gone() {
    let setup = Setup::new();
    let workspace = setup.workspace("triage");
    let dir = record_this_process(&setup, &workspace, "Thu Jan  1 00:00:00 1970");
    let exit_file = dir.join("a.exit");
    let writer = std::thread::spawn(move || {
        crate::clock::pause(Duration::from_millis(60));
        write(exit_file, "7\n");
    });
    let statuses = setup.runs().status(&workspace, None).unwrap();
    writer.join().unwrap();
    assert_eq!(
        statuses[0].state,
        ProcessState::Exited {
            code: Some(7),
            signal: None,
            reason: None,
        }
    );
}

#[test]
fn status_reports_an_unknown_code_with_a_reason_once_the_exit_file_stays_missing() {
    let setup = Setup::new();
    let workspace = setup.workspace("triage");
    record_this_process(&setup, &workspace, "Thu Jan  1 00:00:00 1970");
    let began = std::time::Instant::now();
    let statuses = setup.runs().status(&workspace, None).unwrap();
    assert!(began.elapsed() >= EXIT_FILE_WAIT, "{:?}", began.elapsed());
    assert_eq!(
        statuses[0].state,
        ProcessState::Exited {
            code: None,
            signal: None,
            reason: Some(ExitReason::NoExitFile),
        }
    );
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
