use std::num::NonZeroU16;
use std::path::{Path, PathBuf};

use super::{Sources, status};
use crate::engine::container::ContainerState;
use crate::name::WorkspaceId;
use crate::plan::restart::{Restart, RestartLimits};
use crate::plan::{AppPlan, Plan, ProcessPlan, RunnerKind};
use crate::record::docker::{Docker, DockerKind};
use crate::record::process::{Processes, Start};
use crate::record::run::{Ended, EntryState, Exit, HostProcess, Listen, Ready, RunRecord};
use crate::record::store::RunStore;
use crate::record::testing::{
    FakeDocker, FakeProcesses, Spawned, Unreachable, app, empty_record, host_at, process,
    workspace, write_file,
};
use crate::record::{ContainerId, Handle};
use crate::sandbox::Sandboxing;
use crate::source::Source;
use crate::status::{
    ExitReason, LastExit, ProcessState, SourceSummary, UnknownReason, WorkspaceState,
};

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

// The record still names the process as running, as it does after
// something killed the keeper, or after an outside `TERM` stopped a keeper
// that dropped its notes. Nothing recorded how the process ended.
#[test]
fn a_process_that_the_record_names_as_running_and_that_has_gone_is_exited_unrecorded() {
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

    assert_eq!(status.rows[0].state, ProcessState::unrecorded());
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
fn a_group_whose_leader_has_gone_is_unknown_unless_its_keeper_runs() {
    let state = tempfile::tempdir().unwrap();
    let processes = FakeProcesses::default()
        .with(100, Start::At(1), None)
        .with_leaderless(200, None);

    write_record(
        state.path(),
        &host_record(None, Some((EntryState::Running, host_at(200, 2)))),
    );
    assert_eq!(
        only_state(state.path(), &processes),
        ProcessState::unknown(UnknownReason::LeaderGone)
    );
    let status = read(state.path(), None, &processes, &Unreachable);
    assert_eq!(status.state, WorkspaceState::Unknown);
    assert_eq!(status.rows[0].handle, Some(Handle::pid(200, 2)));

    write_record(
        state.path(),
        &host_record(
            Some((EntryState::Running, host_at(100, 1))),
            Some((EntryState::Running, host_at(200, 2))),
        ),
    );
    assert_eq!(only_state(state.path(), &processes), ProcessState::Running);
}

#[test]
fn a_workspace_is_unknown_only_when_its_unknown_processes_decide_its_state() {
    let state = tempfile::tempdir().unwrap();
    let processes =
        FakeProcesses::default()
            .with(200, Start::At(2), None)
            .with(4242, Start::Unreadable, None);
    let mut record = host_record(None, Some((EntryState::Running, host_at(4242, 2))));
    let other = empty_record().host_process(&app("web"), &process("server"));
    record.keepers.extend(other.keepers);
    record.processes.extend(other.processes);
    write_record(state.path(), &record);

    // The other process has no record of a start, so it is stopped, and
    // the unknown one decides whether anything runs.
    assert_eq!(
        read(state.path(), None, &processes, &Unreachable).state,
        WorkspaceState::Unknown
    );

    record.processes[1].state = EntryState::Running;
    record.processes[1].host = Some(host_at(200, 2));
    write_record(state.path(), &record);
    assert_eq!(
        read(state.path(), None, &processes, &Unreachable).state,
        WorkspaceState::Unknown
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
fn status_shows_a_host_process_whose_app_declares_no_permissions_as_unsandboxed() {
    let state = tempfile::tempdir().unwrap();

    let status = read(
        state.path(),
        Some(&plan()),
        &FakeProcesses::default(),
        &Unreachable,
    );

    assert_eq!(
        status.rows[0].sandbox,
        Some(Sandboxing::Unsandboxed {
            reason: "its app declares no permissions".to_owned()
        })
    );
    assert_eq!(status.rows[1].sandbox, None);
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(json["rows"][0]["sandbox"]["kind"], "unsandboxed");
    assert!(json["rows"][1].get("sandbox").is_none());
}

#[test]
fn status_shows_a_host_process_whose_app_declares_permissions_under_this_machines_sandbox() {
    let state = tempfile::tempdir().unwrap();
    let mut apps = plan().apps().to_vec();
    let mut permissions = crate::plan::Permissions::default();
    permissions.listen.insert("http".parse().unwrap());
    apps[0].permissions = Some(permissions);
    let plan = Plan::new("reading-list".parse().unwrap(), workspace(), apps).unwrap();

    let status = read(
        state.path(),
        Some(&plan),
        &FakeProcesses::default(),
        &Unreachable,
    );

    let expected = crate::sandbox::sandboxing(&plan.apps()[0], &crate::sandbox::platform());
    assert_eq!(status.rows[0].sandbox, expected);
    #[cfg(target_os = "macos")]
    assert_eq!(status.rows[0].sandbox, Some(Sandboxing::seatbelt()));
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
fn a_planned_container_that_docker_has_not_started_is_starting() {
    let state = tempfile::tempdir().unwrap();
    container_record(state.path(), EntryState::Planned);
    let id = container_id();
    let created = FakeDocker::default()
        .with(DockerKind::Container, "db-server", id.as_str())
        .in_state(id.as_str(), ContainerState::Created);

    assert_eq!(
        container_state(state.path(), &FakeDocker::default()),
        ProcessState::Starting
    );
    assert_eq!(
        container_state(state.path(), &created),
        ProcessState::Starting
    );
}

#[test]
fn a_container_that_docker_is_restarting_is_restarting_and_a_paused_one_is_paused() {
    let state = tempfile::tempdir().unwrap();
    container_record(state.path(), EntryState::Running);
    let id = container_id();
    let in_state = |docker_state| {
        FakeDocker::default()
            .with(DockerKind::Container, "db-server", id.as_str())
            .in_state(id.as_str(), docker_state)
    };

    assert_eq!(
        container_state(state.path(), &in_state(ContainerState::Restarting)),
        ProcessState::Restarting
    );
    assert_eq!(
        container_state(state.path(), &in_state(ContainerState::Paused)),
        ProcessState::Paused
    );
    let status = read(
        state.path(),
        None,
        &FakeProcesses::default(),
        &in_state(ContainerState::Paused),
    );
    assert_eq!(status.rows[0].handle, Some(Handle::container(id.clone())));
    assert_eq!(status.state, WorkspaceState::PartlyRunning);
}

#[test]
fn a_container_in_a_state_that_phase_does_not_know_is_unknown() {
    let state = tempfile::tempdir().unwrap();
    container_record(state.path(), EntryState::Running);
    let id = container_id();
    let removing = FakeDocker::default()
        .with(DockerKind::Container, "db-server", id.as_str())
        .in_state(id.as_str(), ContainerState::Unknown);

    assert_eq!(
        container_state(state.path(), &removing),
        ProcessState::unknown(UnknownReason::UnrecognisedState)
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

/// Returns a record whose `api/server` keeper has gone and whose process
/// ended as `ended`, with its group gone.
fn ended_record(ended: Ended) -> RunRecord {
    let mut record = host_record(
        Some((EntryState::Running, host_at(100, 1))),
        Some((EntryState::Stopped, host_at(200, 2))),
    );
    record.processes[0].ended = Some(ended);
    record
}

#[test]
fn a_process_that_ended_by_itself_is_exited_with_its_code_or_signal() {
    for (exit, state) in [
        (Exit::Code(3), ProcessState::exited(Some(3))),
        (Exit::Signal(9), ProcessState::killed(9)),
    ] {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), &ended_record(Ended::by_itself(Some(exit))));

        let status = read(dir.path(), None, &FakeProcesses::default(), &Unreachable);

        assert_eq!(status.rows[0].state, state);
        assert_eq!(status.state, WorkspaceState::Stopped);
    }
}

#[test]
fn a_process_that_a_stop_request_ended_is_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let ended = Ended::requested(Some(Exit::Signal(15)));
    write_record(dir.path(), &ended_record(ended));

    let status = read(dir.path(), None, &FakeProcesses::default(), &Unreachable);

    assert_eq!(status.rows[0].state, ProcessState::Stopped);
    assert_eq!(status.rows[0].last_exit, None);
}

#[test]
fn a_process_that_restarts_shows_how_it_last_ended() {
    let dir = tempfile::tempdir().unwrap();
    let processes =
        FakeProcesses::default()
            .with(100, Start::At(1), None)
            .with(300, Start::At(3), None);
    let mut record = ended_record(Ended::by_itself(Some(Exit::Code(1))));
    write_record(dir.path(), &record);

    let status = read(dir.path(), None, &processes, &Unreachable);
    assert_eq!(status.rows[0].state, ProcessState::Restarting);
    assert_eq!(status.rows[0].last_exit, Some(LastExit::new(Some(1), None)));

    record.processes[0].state = EntryState::Running;
    record.processes[0].host = Some(host_at(300, 3));
    record.processes[0].restarts = 1;
    write_record(dir.path(), &record);

    let status = read(dir.path(), None, &processes, &Unreachable);
    assert_eq!(status.rows[0].state, ProcessState::Running);
    assert_eq!(status.rows[0].last_exit, Some(LastExit::new(Some(1), None)));
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(json["rows"][0]["last_exit"], serde_json::json!({"code": 1}));
}

/// Returns a record whose `api/server` keeper is process 100, which runs,
/// and whose process 200 has gone, with `restart` as its policy and `state`
/// and `ended` as its entry holds them.
fn crashed_record(restart: Restart, state: EntryState, ended: Option<Ended>) -> RunRecord {
    let mut record = host_record(
        Some((EntryState::Running, host_at(100, 1))),
        Some((state, host_at(200, 2))),
    );
    record.processes[0].restart = Some(restart);
    record.processes[0].ended = ended;
    record
}

#[test]
fn a_crashed_process_shows_as_restarting_only_when_its_policy_restarts_it() {
    let keeper = FakeProcesses::default().with(100, Start::At(1), None);
    let limits = RestartLimits::default();
    let one = RestartLimits {
        max: std::num::NonZeroU32::new(1),
        ..RestartLimits::default()
    };
    let crash = Some(Ended::by_itself(Some(Exit::Code(3))));
    let clean = Some(Ended::by_itself(Some(Exit::Code(0))));
    let cases = [
        // The keeper has not recorded the end yet.
        (
            Restart::No,
            EntryState::Running,
            None,
            0,
            ProcessState::Stopping,
        ),
        (
            Restart::always(limits.clone()),
            EntryState::Running,
            None,
            0,
            ProcessState::Restarting,
        ),
        (
            Restart::on_failure(limits.clone()),
            EntryState::Running,
            None,
            0,
            ProcessState::Stopping,
        ),
        // The keeper recorded the end and is stopping what is left.
        (
            Restart::No,
            EntryState::Stopping,
            crash,
            0,
            ProcessState::Stopping,
        ),
        // The keeper recorded that the group is gone.
        (
            Restart::No,
            EntryState::Stopped,
            crash,
            0,
            ProcessState::exited(Some(3)),
        ),
        (
            Restart::on_failure(limits.clone()),
            EntryState::Stopped,
            clean,
            0,
            ProcessState::exited(Some(0)),
        ),
        (
            Restart::on_failure(limits.clone()),
            EntryState::Stopped,
            crash,
            0,
            ProcessState::Restarting,
        ),
        (
            Restart::on_failure(one),
            EntryState::Stopped,
            crash,
            1,
            ProcessState::exited(Some(3)),
        ),
        (
            Restart::always(limits.clone()),
            EntryState::Stopped,
            clean,
            0,
            ProcessState::Restarting,
        ),
    ];
    for (restart, entry_state, ended, restarts, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        let mut record = crashed_record(restart.clone(), entry_state, ended);
        record.processes[0].restarts = restarts;
        write_record(dir.path(), &record);

        assert_eq!(
            only_state(dir.path(), &keeper),
            expected,
            "{restart:?} {entry_state:?} {ended:?} {restarts}"
        );
    }
}

#[test]
fn a_running_process_whose_keeper_and_app_have_gone_is_exited_without_a_recorded_end() {
    for ended in [None, Some(Ended::by_itself(Some(Exit::Code(1))))] {
        let dir = tempfile::tempdir().unwrap();
        let mut record = host_record(
            Some((EntryState::Running, host_at(100, 1))),
            Some((EntryState::Running, host_at(200, 2))),
        );
        record.processes[0].ended = ended;
        write_record(dir.path(), &record);

        let state = only_state(dir.path(), &FakeProcesses::default());

        assert_eq!(state, ProcessState::unrecorded(), "{ended:?}");
        let ProcessState::Exited { reason, .. } = state else {
            panic!("{state:?}");
        };
        assert_eq!(reason, Some(ExitReason::Unrecorded));
    }
}

#[test]
fn a_stopping_process_whose_keeper_and_app_have_gone_shows_how_it_ended() {
    let cases = [
        // Something killed the keeper while it stopped what a crash left.
        (
            Some(Ended::by_itself(Some(Exit::Code(3)))),
            ProcessState::exited(Some(3)),
        ),
        (Some(Ended::requested(None)), ProcessState::Stopped),
        (None, ProcessState::unrecorded()),
    ];
    for (ended, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        let mut record = host_record(
            Some((EntryState::Running, host_at(100, 1))),
            Some((EntryState::Stopping, host_at(200, 2))),
        );
        record.processes[0].ended = ended;
        write_record(dir.path(), &record);

        assert_eq!(
            only_state(dir.path(), &FakeProcesses::default()),
            expected,
            "{ended:?}"
        );
    }
}

#[test]
fn a_running_process_that_phase_observed_ready_since_its_last_start_is_ready() {
    let state = tempfile::tempdir().unwrap();
    let sleeper = Spawned::sleeper();
    let mut record = host_record(None, Some((EntryState::Running, sleeper.host())));
    record.processes[0].restarts = 2;
    let system = crate::record::process::System;

    write_record(state.path(), &record);
    assert_eq!(only_state(state.path(), &system), ProcessState::Running);

    record.processes[0].ready = Some(Ready::new(1_793_000_000_123, 2));
    write_record(state.path(), &record);
    assert_eq!(only_state(state.path(), &system), ProcessState::Ready);
    assert_eq!(
        read(state.path(), None, &system, &Unreachable).state,
        WorkspaceState::Running
    );

    // Readiness from before the keeper's last restart no longer counts.
    record.processes[0].ready = Some(Ready::new(1_793_000_000_123, 1));
    write_record(state.path(), &record);
    assert_eq!(only_state(state.path(), &system), ProcessState::Running);
}

#[test]
fn a_process_that_was_ready_and_has_gone_is_not_ready() {
    let state = tempfile::tempdir().unwrap();
    let mut record = host_record(None, Some((EntryState::Running, host_at(4242, 1))));
    record.processes[0].ready = Some(Ready::new(1_793_000_000_123, 0));
    write_record(state.path(), &record);

    assert_eq!(
        only_state(state.path(), &FakeProcesses::default()),
        ProcessState::unrecorded()
    );
}

#[test]
fn a_running_container_is_ready_only_while_docker_reports_the_start_that_phase_saw_ready() {
    const STARTED: &str = "2026-11-05T10:20:30.123456789Z";
    let state = tempfile::tempdir().unwrap();
    let id = container_id();
    let mut record =
        empty_record().container(&app("db"), &process("server"), "db-server".parse().unwrap());
    record.containers[0].state = EntryState::Running;
    record.containers[0].id = Some(id.clone());
    let docker = |started: Option<&str>| {
        FakeDocker::default()
            .with(DockerKind::Container, "db-server", id.as_str())
            .started_at(id.as_str(), started)
    };

    write_record(state.path(), &record);
    assert_eq!(
        container_state(state.path(), &docker(Some(STARTED))),
        ProcessState::Running
    );

    // The readiness time comes from the host's clock, and Docker's clock
    // may run ahead of it, so only the start that Docker reports counts.
    record.containers[0].ready =
        Some(Ready::new(1_000, 0).with_container_start(STARTED.to_owned()));
    write_record(state.path(), &record);
    assert_eq!(
        container_state(state.path(), &docker(Some(STARTED))),
        ProcessState::Ready
    );
    // Docker started the container again, under its restart policy or
    // because something outside phase restarted it.
    assert_eq!(
        container_state(state.path(), &docker(Some("2026-11-05T10:20:31Z"))),
        ProcessState::Running
    );
    assert_eq!(
        container_state(state.path(), &docker(None)),
        ProcessState::Running
    );
    let exited = docker(Some(STARTED)).in_state(id.as_str(), ContainerState::Exited(3));
    assert_eq!(
        container_state(state.path(), &exited),
        ProcessState::exited(Some(3))
    );

    // Readiness without a recorded start, which no container entry should
    // hold, counts as not ready.
    record.containers[0].ready = Some(Ready::new(1_000, 0));
    write_record(state.path(), &record);
    assert_eq!(
        container_state(state.path(), &docker(Some(STARTED))),
        ProcessState::Running
    );
}
