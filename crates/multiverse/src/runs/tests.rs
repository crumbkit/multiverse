use std::fs;

use crate::testing::write;
use std::io::Write as _;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::Error;
use crate::manifest::Manifest;
use crate::name::WorkspaceName;
use crate::record::{Checkout, RECORD_VERSION, Workspace};
use crate::run_files::RunHost;
use crate::runs::{DownOutcome, ExitReason, Follower, ProcessState, Runs, UpOutcome, tail};
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
    let mut workspace = setup.workspace("triage");
    let manifest = manifest("[processes.a]\nargv = [\"sleep\", \"30\"]\n");
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
    let ProcessState::Running { pid: Some(pid) } = statuses[0].state else {
        panic!("{statuses:?}");
    };
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
fn the_manifests_containers_become_the_container_engines_specs_with_every_field() {
    let manifest = Manifest::parse(
        r#"
ports = ["dynamodb"]

[containers.dynamodb]
image = "amazon/dynamodb-local:2.5.4"
command = ["sh", "-c", "echo $HOME"]
env = { AWS_REGION = "eu-west-1", TOKEN = "s3cret$x" }
ports = { 8000 = "dynamodb" }
volumes = { data = "/data" }

[containers.cache]
image = "busybox:1.37.0"
"#,
        std::path::Path::new("/repo/mxv.toml"),
    )
    .unwrap();
    let specs = crate::runs::container_specs(&manifest.containers);
    let names: Vec<&str> = specs.keys().map(|service| service.as_str()).collect();
    assert_eq!(names, ["cache", "dynamodb"]);
    let dynamodb = specs
        .iter()
        .find(|(service, _)| service.as_str() == "dynamodb")
        .map(|(_, spec)| spec)
        .unwrap();
    assert_eq!(dynamodb.image, "amazon/dynamodb-local:2.5.4");
    assert_eq!(
        dynamodb.command.as_deref(),
        Some(["sh".to_owned(), "-c".to_owned(), "echo $HOME".to_owned()].as_slice())
    );
    let variables: Vec<&str> = dynamodb.env.keys().map(String::as_str).collect();
    assert_eq!(variables, ["AWS_REGION", "TOKEN"]);
    assert_eq!(
        dynamodb.env["TOKEN"],
        phase::container::ComposeValue::new("s3cret$x".to_owned())
    );
    assert_eq!(
        dynamodb.ports,
        [phase::container::PublishedPort::new(
            8000,
            "dynamodb".to_owned()
        )]
    );
    let volumes: Vec<(&str, &str)> = dynamodb
        .volumes
        .iter()
        .map(|(key, path)| (key.as_str(), path.as_str()))
        .collect();
    assert_eq!(volumes, [("data", "/data")]);
    assert!(!format!("{dynamodb:?}").contains("s3cret"));
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
    assert_eq!(saved.version, RECORD_VERSION);
    assert_eq!(host.saved_workspace_id(&workspace.name).unwrap(), id);
}

#[test]
fn the_run_host_without_a_store_or_without_the_record_gives_no_stable_id() {
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
                Err(Error::DockerFailed {
                    operation: "workspace id",
                    ..
                })
            ),
            "{saved:?}"
        );
    }
}
