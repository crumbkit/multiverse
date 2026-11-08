use std::fs;

use crate::testing::write;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::Error;
use crate::manifest::Manifest;
use crate::name::WorkspaceName;
use crate::record::{Checkout, ID_RECORD_VERSION, Workspace};
use crate::run_files::RunHost;
use crate::runs::{DownOutcome, ExitReason, Follower, ProcessState, Runs, tail};
use crate::store::{Created, Store};
use phase::container::Host as _;

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

/// Starts `argv` as the process `name` of `workspace` as an older mxv did,
/// with a shell that leads a process group of its own, writes its log and
/// its run record, and returns its process id.
fn start_as_an_older_mxv(setup: &Setup, workspace: &Workspace, name: &str, argv: &[&str]) -> u32 {
    let runs = setup.runs();
    let dir = runs.workspace_dir(&workspace.name);
    fs::create_dir_all(&dir).unwrap();
    let argv: Vec<String> = argv.iter().map(|word| (*word).to_owned()).collect();
    let launched = crate::process::launch(&crate::process::Launch {
        name,
        argv: &argv,
        cwd: &dir,
        log: &runs.log(&workspace.name, name),
        exit_file: &dir.join(format!("{name}.exit")),
    })
    .unwrap();
    let pid = launched.pid();
    launched.keep();
    let record = serde_json::json!({
        "version": 1,
        "pid": pid,
        "started": crate::process::start_time(pid).unwrap().unwrap(),
        "ports": {},
    });
    write(dir.join(format!("{name}.json")), record.to_string());
    pid
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
fn down_kills_a_process_that_ignores_sigterm_after_the_grace() {
    let setup = Setup::new();
    let workspace = setup.workspace("triage");
    let manifest = manifest("[processes.stubborn]\nargv = [\"true\"]\n");
    let runs = setup.runs();
    start_as_an_older_mxv(
        &setup,
        &workspace,
        "stubborn",
        &[
            "sh",
            "-c",
            "trap '' TERM; echo trapped; while :; do sleep 1; done",
        ],
    );
    // The process ignores SIGTERM only once its trap is set.
    let log = runs.log(&workspace.name, "stubborn");
    while fs::read_to_string(&log).unwrap_or_default().is_empty() {
        crate::clock::pause(crate::clock::POLL_INTERVAL);
    }
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
fn status_reports_an_unknown_code_with_a_reason_when_the_shell_ended_without_an_exit_file() {
    let setup = Setup::new();
    let workspace = setup.workspace("triage");
    record_this_process(&setup, &workspace, "Thu Jan  1 00:00:00 1970");
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

#[cfg(target_os = "macos")]
#[test]
fn a_record_with_the_start_time_that_ps_prints_still_owns_its_running_process_on_macos() {
    let setup = Setup::new();
    let workspace = setup.workspace("triage");
    let manifest = manifest("[processes.a]\nargv = [\"sleep\", \"30\"]\n");
    let runs = setup.runs();
    let pid = start_as_an_older_mxv(&setup, &workspace, "a", &["sleep", "30"]);
    // An mxv before libproc recorded what `ps -o lstart=` prints in UTC.
    let record_file = runs.workspace_dir(&workspace.name).join("a.json");
    let mut record: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&record_file).unwrap()).unwrap();
    let old = crate::process::ps_start_time(pid).unwrap().unwrap();
    assert!(!old.contains('.'), "{old}");
    record["started"] = serde_json::Value::String(old);
    write(&record_file, record.to_string());
    let statuses = runs.status(&workspace, None).unwrap();
    assert_eq!(statuses[0].state, ProcessState::Running { pid: Some(pid) });
    runs.down(
        &runs.lock(&workspace.name).unwrap(),
        &workspace,
        Some(&manifest),
        &[],
    )
    .unwrap();
    assert!(!crate::process::group_exists(pid));
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
fn the_run_host_saves_a_stable_id_in_a_record_without_one_and_then_returns_the_saved_id() {
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
    let host = RunHost {
        records: Some(&store),
    };
    let id = host.saved_workspace_id(&workspace.name).unwrap();
    let saved = store.read(&workspace.name).unwrap().unwrap();
    assert_eq!(saved.id.as_ref(), Some(&id));
    assert_eq!(saved.version, ID_RECORD_VERSION);
    assert_eq!(host.saved_workspace_id(&workspace.name).unwrap(), id);
}

#[test]
fn the_run_host_without_a_store_or_without_the_record_gives_no_stable_id_and_says_what_it_cannot_do()
 {
    let setup = Setup::new();
    let store = setup.store();
    let name: WorkspaceName = "triage".parse().unwrap();
    for host in [
        RunHost::WITHOUT_RECORDS,
        RunHost {
            records: Some(&store),
        },
    ] {
        let saved = host.saved_workspace_id(&name);
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
    assert!(runs.recorded(&name).unwrap().is_empty());
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
