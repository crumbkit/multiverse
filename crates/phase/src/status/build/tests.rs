use std::num::NonZeroU16;
use std::path::{Path, PathBuf};

use super::{Sources, status};
use crate::engine::container::ContainerState;
use crate::name::WorkspaceId;
use crate::plan::{AppPlan, Plan, ProcessPlan, RunnerKind};
use crate::record::docker::{Docker, DockerKind};
use crate::record::process::{Processes, Start};
use crate::record::run::{EntryState, HostProcess, Listen, RunRecord};
use crate::record::store::RunStore;
use crate::record::testing::{
    FakeDocker, FakeProcesses, Spawned, Unreachable, app, empty_record, host_at, process,
    workspace, write_file,
};
use crate::record::{ContainerId, Handle};
use crate::source::Source;
use crate::status::{ProcessState, SourceSummary, UnknownReason, WorkspaceState};

/// Writes `record` as the run record of the tests' workspace under
/// `state`, as phase would have left it.
fn write_record(state: &Path, record: &RunRecord) {
    let store = RunStore::new(state);
    std::fs::create_dir_all(state.join("runs")).unwrap();
    write_file(
        &store.path(&workspace()),
        &serde_json::to_string(record).unwrap(),
    );
}

/// Returns a record whose `api/server` keeper is `keeper` and whose process
/// is `server`, each in `state` when it has a process.
fn host_record(
    keeper: Option<(EntryState, HostProcess)>,
    server: Option<(EntryState, HostProcess)>,
) -> RunRecord {
    let mut record = empty_record().host_process(&app("api"), &process("server"));
    for (entry, given) in [
        (&mut record.keepers[0], keeper),
        (&mut record.processes[0], server),
    ] {
        if let Some((state, host)) = given {
            entry.state = state;
            entry.host = Some(host);
        }
    }
    record
}

/// Returns the status that `processes` and `docker` give of the record in
/// `state`, with `plan`.
fn read(
    state: &Path,
    plan: Option<&Plan>,
    processes: &dyn Processes,
    docker: &dyn Docker,
) -> crate::status::Status {
    let sources = Sources {
        state_dir: state,
        processes,
        docker,
    };
    status(&RunStore::new(state), &workspace(), plan, sources).unwrap()
}

/// Returns the only row's state of the record in `state`.
fn only_state(state: &Path, processes: &dyn Processes) -> ProcessState {
    let status = read(state, None, processes, &Unreachable);
    assert_eq!(status.rows.len(), 1, "{status:?}");
    status.rows[0].state
}

/// Returns a plan of the tests' workspace with the host app `api`, whose
/// process `server` listens on port 41000, and the container app `db`.
fn plan() -> Plan {
    let mut api = AppPlan::new(
        app("api"),
        RunnerKind::Host,
        Source::checkout(PathBuf::from("/workspaces/reading-list/api"), None),
    );
    let mut server = ProcessPlan::new(process("server"), vec!["./serve".into()]);
    api.ports
        .insert("http".parse().unwrap(), NonZeroU16::new(41_000).unwrap());
    server.port = Some("http".parse().unwrap());
    api.processes.push(server);
    let mut db = AppPlan::new(
        app("db"),
        RunnerKind::Container,
        Source::image("postgres:17".parse().unwrap()),
    );
    db.processes
        .push(ProcessPlan::new(process("server"), Vec::new()));
    Plan::new("reading-list".parse().unwrap(), workspace(), vec![api, db]).unwrap()
}

#[test]
fn a_running_process_is_reported_from_the_record_and_the_process_table_without_a_plan() {
    let state = tempfile::tempdir().unwrap();
    let sleeper = Spawned::sleeper();
    let mut record = host_record(None, Some((EntryState::Running, sleeper.host())));
    record.processes[0].restarts = 2;
    record.processes[0].listen = Some(Listen {
        name: "http".parse().unwrap(),
        port: 41_000,
    });
    write_record(state.path(), &record);

    let status = read(
        state.path(),
        None,
        &crate::record::process::System,
        &Unreachable,
    );

    assert_eq!(status.workspace, workspace());
    assert_eq!(status.name, None);
    assert_eq!(status.state, WorkspaceState::Running);
    let row = &status.rows[0];
    assert_eq!(row.state, ProcessState::Running);
    assert_eq!(
        row.handle,
        Some(Handle::pid(sleeper.pid(), sleeper.host().started_at_us))
    );
    assert_eq!(row.source, None);
    assert_eq!(row.restarts, 2);
    assert_eq!(row.address, Some("127.0.0.1:41000".parse().unwrap()));
    assert_eq!(
        row.log,
        Some(
            state
                .path()
                .join("logs")
                .join(workspace().as_str())
                .join("api.server.log")
        )
    );
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        let started = row.started_at_ms.unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let now = u64::try_from(now).unwrap();
        assert!(
            started <= now + 1000 && now - started < 60_000,
            "{started} {now}"
        );
    }
}

// The record still names the process as running, as it does after an
// outside `TERM` stopped a keeper that dropped its notes.
#[test]
fn a_process_that_the_record_names_as_running_and_that_has_gone_is_stopped() {
    let state = tempfile::tempdir().unwrap();
    let mut ended = Spawned::shell("echo ready; exec sleep 0.1");
    let host = ended.host();
    ended.wait_for_exit();
    write_record(
        state.path(),
        &host_record(
            Some((EntryState::Running, host.clone())),
            Some((EntryState::Running, host)),
        ),
    );

    let status = read(
        state.path(),
        None,
        &crate::record::process::System,
        &Unreachable,
    );

    assert_eq!(status.rows[0].state, ProcessState::Stopped);
    assert_eq!(status.rows[0].handle, None);
    assert_eq!(status.state, WorkspaceState::Stopped);
}

#[test]
fn an_entry_whose_group_is_1_is_reported_as_unknown_and_never_looked_up() {
    let state = tempfile::tempdir().unwrap();
    let in_group_one = HostProcess {
        group: 1,
        ..host_at(4242, 2)
    };
    write_record(
        state.path(),
        &host_record(None, Some((EntryState::Running, in_group_one))),
    );
    // The fake table holds no process, so a lookup would read the entry as
    // gone and hide it.
    let processes = FakeProcesses::default();

    assert_eq!(
        only_state(state.path(), &processes),
        ProcessState::unknown(UnknownReason::Unsignallable)
    );
    assert!(processes.signals.borrow().is_empty());
}

#[test]
fn a_process_that_phase_may_not_read_is_unknown() {
    let state = tempfile::tempdir().unwrap();
    write_record(
        state.path(),
        &host_record(None, Some((EntryState::Running, host_at(4242, 2)))),
    );
    let processes = FakeProcesses::default().with(4242, Start::Unreadable, None);

    assert_eq!(
        only_state(state.path(), &processes),
        ProcessState::unknown(UnknownReason::Unreadable)
    );
}

#[test]
fn a_running_keeper_makes_its_process_starting_before_its_first_start_and_restarting_after() {
    let state = tempfile::tempdir().unwrap();
    let processes = FakeProcesses::default().with(100, Start::At(1), None);
    let keeper = Some((EntryState::Running, host_at(100, 1)));

    write_record(state.path(), &host_record(keeper.clone(), None));
    assert_eq!(only_state(state.path(), &processes), ProcessState::Starting);
    let status = read(state.path(), None, &processes, &Unreachable);
    assert_eq!(status.state, WorkspaceState::Starting);

    write_record(
        state.path(),
        &host_record(keeper, Some((EntryState::Stopped, host_at(200, 2)))),
    );
    assert_eq!(
        only_state(state.path(), &processes),
        ProcessState::Restarting
    );
}

#[test]
fn a_process_that_phase_is_stopping_and_still_runs_is_stopping() {
    let state = tempfile::tempdir().unwrap();
    let processes = FakeProcesses::default().with(200, Start::At(2), None);
    write_record(
        state.path(),
        &host_record(None, Some((EntryState::Stopping, host_at(200, 2)))),
    );

    assert_eq!(only_state(state.path(), &processes), ProcessState::Stopping);
}

#[test]
fn a_plan_orders_the_rows_and_gives_their_sources_and_addresses_and_unplanned_entries_stay() {
    let state = tempfile::tempdir().unwrap();
    let processes = FakeProcesses::default().with(300, Start::At(3), None);
    let mut record = empty_record().host_process(&app("old"), &process("worker"));
    record.processes[0].state = EntryState::Running;
    record.processes[0].host = Some(host_at(300, 3));
    write_record(state.path(), &record);

    let status = read(state.path(), Some(&plan()), &processes, &Unreachable);

    assert_eq!(status.name, Some("reading-list".parse().unwrap()));
    assert_eq!(status.state, WorkspaceState::PartlyRunning);
    let rows: Vec<(String, ProcessState)> = status
        .rows
        .iter()
        .map(|row| (format!("{}/{}", row.app, row.process), row.state))
        .collect();
    assert_eq!(
        rows,
        [
            ("api/server".to_owned(), ProcessState::Stopped),
            ("db/server".to_owned(), ProcessState::Stopped),
            ("old/worker".to_owned(), ProcessState::Running),
        ]
    );
    assert_eq!(
        status.rows[0].source,
        Some(SourceSummary::from(&Source::checkout(
            PathBuf::from("/workspaces/reading-list/api"),
            None
        )))
    );
    assert_eq!(
        status.rows[0].address,
        Some("127.0.0.1:41000".parse().unwrap())
    );
    assert_eq!(status.rows[1].runner, RunnerKind::Container);
    assert_eq!(status.rows[1].log, None);
    assert_eq!(status.rows[2].source, None);
}

#[test]
fn a_plan_of_another_workspace_is_not_used() {
    let state = tempfile::tempdir().unwrap();
    let other: WorkspaceId = "1c2d3e4f".parse().unwrap();
    let plan = Plan::new("other".parse().unwrap(), other, Vec::new()).unwrap();

    let status = read(
        state.path(),
        Some(&plan),
        &FakeProcesses::default(),
        &Unreachable,
    );

    assert_eq!(status.name, None);
    assert!(status.rows.is_empty());
}

/// Writes a record whose container `db/server` is running with the id
/// `container_id` and the name `db-server`.
fn container_record(state: &Path, entry_state: EntryState) {
    let mut record =
        empty_record().container(&app("db"), &process("server"), "db-server".parse().unwrap());
    record.containers[0].state = entry_state;
    record.containers[0].id = Some(container_id());
    write_record(state, &record);
}

fn container_id() -> ContainerId {
    "c0".repeat(32).try_into().unwrap()
}

/// Returns the state of the container row with `docker`.
fn container_state(state: &Path, docker: &dyn Docker) -> ProcessState {
    let status = read(state, None, &FakeProcesses::default(), docker);
    status.rows[0].state
}

#[test]
fn a_container_state_comes_from_docker() {
    let state = tempfile::tempdir().unwrap();
    container_record(state.path(), EntryState::Running);
    let id = container_id();
    let running = FakeDocker::default().with(DockerKind::Container, "db-server", id.as_str());
    let exited = FakeDocker::default()
        .with(DockerKind::Container, "db-server", id.as_str())
        .in_state(id.as_str(), ContainerState::Exited(3));

    assert_eq!(
        container_state(state.path(), &running),
        ProcessState::Running
    );
    let status = read(state.path(), None, &FakeProcesses::default(), &running);
    assert_eq!(status.rows[0].handle, Some(Handle::container(id)));
    assert_eq!(
        container_state(state.path(), &exited),
        ProcessState::exited(Some(3))
    );
    assert_eq!(
        container_state(state.path(), &FakeDocker::default()),
        ProcessState::Stopped
    );
}

#[test]
fn a_container_is_unknown_when_docker_does_not_answer_and_never_stopped() {
    let state = tempfile::tempdir().unwrap();
    container_record(state.path(), EntryState::Running);

    assert_eq!(
        container_state(state.path(), &Unreachable),
        ProcessState::unknown(UnknownReason::DockerUnavailable)
    );
    let down = FakeDocker {
        down: true,
        ..FakeDocker::default()
    };
    assert_eq!(
        container_state(state.path(), &down),
        ProcessState::unknown(UnknownReason::DockerUnavailable)
    );
}

#[test]
fn stopped_entries_are_left_out_without_a_plan() {
    let state = tempfile::tempdir().unwrap();
    container_record(state.path(), EntryState::Stopped);

    let status = read(state.path(), None, &FakeProcesses::default(), &Unreachable);

    assert!(status.rows.is_empty());
    assert_eq!(status.state, WorkspaceState::Stopped);
}
