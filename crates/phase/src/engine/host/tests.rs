use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use rustix::process::Signal;

use super::follow::{LogFollower, MOST_FOLLOW_BYTES};
use super::listen::{listening_inodes, socket_descriptors, socket_listens_on};
use super::{Holder, HostRunner as _, HostStop, HostStopped, listen_holder};
use crate::clock::now;
use crate::engine::probe::LogLines as _;
use crate::plan::restart::StopSignal;
use crate::record::process::{Identity, Processes as _, Start, System};
use crate::record::testing::{FakeProcesses, Spawned, host_at, write_file};

/// Appends `text` to the log at `path`, creating it when it is missing.
fn append(path: &Path, text: &str) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes the log that a follower reads"
    )]
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

/// Replaces the log at `path` with `text`, as a cap that empties it does.
fn replace(path: &Path, text: &str) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test empties the log that a follower reads, as a cap does"
    )]
    OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

#[test]
fn a_follower_at_the_end_returns_each_complete_line_written_after_it_opened() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.log");
    append(&path, "before\n");
    let mut follower = LogFollower::at_end(&path).unwrap().unwrap();
    assert_eq!(follower.path(), path);
    append(&path, "first\nsec");
    assert_eq!(follower.lines().unwrap(), ["first"]);
    append(&path, "ond\n");
    assert_eq!(follower.lines().unwrap(), ["second"]);
    assert!(follower.lines().unwrap().is_empty());
    assert!(
        LogFollower::at_end(&dir.path().join("missing.log"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_follower_from_the_start_reads_what_the_log_already_holds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.log");
    append(&path, "one\ntwo\n");
    let mut follower = LogFollower::from_start(&path).unwrap();
    assert_eq!(follower.lines().unwrap(), ["one", "two"]);
    assert!(LogFollower::from_start(&dir.path().join("missing.log")).is_err());
}

#[test]
fn a_log_that_does_not_exist_yet_counts_as_empty_and_is_read_from_its_start_once_it_appears() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("later.log");
    let mut follower = LogFollower::at_end_or_empty(&path).unwrap();
    assert!(follower.new_lines().unwrap().is_empty());
    append(&path, "ready\n");
    assert_eq!(follower.new_lines().unwrap(), ["ready"]);
}

/// Renames the log at `path` to `older`, as a restart's rotation does.
fn rotate(path: &Path, older: &Path) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test renames the log that a follower reads, as a rotation does"
    )]
    fs::rename(path, older).unwrap();
}

#[test]
fn a_follower_starts_again_at_the_start_of_a_log_that_became_shorter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.log");
    append(&path, "a long first line\n");
    let mut follower = LogFollower::at_end_or_empty(&path).unwrap();
    append(&path, "half");
    assert!(follower.lines().unwrap().is_empty());
    // A cap empties the log, and the held partial line belongs to the old
    // contents.
    replace(&path, "new\n");
    assert_eq!(follower.lines().unwrap(), ["new"]);
}

#[test]
fn a_line_longer_than_one_read_comes_back_in_parts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.log");
    append(&path, "");
    let mut follower = LogFollower::at_end_or_empty(&path).unwrap();
    let most = usize::try_from(MOST_FOLLOW_BYTES).unwrap();
    append(&path, &"x".repeat(most + 10));
    let parts = follower.lines().unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].len(), most);
    assert!(follower.lines().unwrap().is_empty());
    append(&path, "\n");
    assert_eq!(follower.lines().unwrap(), ["x".repeat(10)]);
    fs::remove_file(&path).unwrap();
}

#[test]
fn an_unfinished_line_and_a_read_never_hold_more_than_the_bound() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.log");
    append(&path, "");
    let mut follower = LogFollower::at_end_or_empty(&path).unwrap();
    let most = usize::try_from(MOST_FOLLOW_BYTES).unwrap();
    append(&path, &"x".repeat(most - 5));
    assert!(follower.lines().unwrap().is_empty());
    append(&path, &format!("{}\n", "y".repeat(20)));
    let parts = follower.lines().unwrap();
    assert_eq!(
        parts,
        [format!("{}{}", "x".repeat(most - 5), "y".repeat(5))]
    );
    assert_eq!(follower.lines().unwrap(), ["y".repeat(15)]);
}

#[test]
fn a_follower_moves_to_the_new_log_after_the_old_one_is_renamed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.log");
    append(&path, "one\nhal");
    let mut follower = LogFollower::from_start(&path).unwrap();
    assert_eq!(follower.lines().unwrap(), ["one"]);
    append(&path, "f\nend");
    rotate(&path, &dir.path().join("a.log.1"));
    // The old file stays open while no new log exists.
    assert_eq!(follower.lines().unwrap(), ["half"]);
    append(&path, "new\n");
    assert_eq!(follower.lines().unwrap(), ["end"]);
    assert_eq!(follower.lines().unwrap(), ["new"]);
}

#[test]
fn the_listening_sockets_of_a_port_come_from_the_lines_in_state_0a() {
    let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:5DC1 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41234 1 0000000000000000 100 0 0 10 0\n   1: 0100007F:5DC1 0100007F:9C40 01 00000000:00000000 00:00000000 00000000  1000        0 41999 1 0000000000000000 20 4 30 10 -1\n   2: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 17000 1 0000000000000000 100 0 0 10 0\n";
    assert_eq!(listening_inodes(table, 24001), ["41234"]);
    assert_eq!(listening_inodes(table, 8080), ["17000"]);
    assert!(listening_inodes(table, 9).is_empty());
}

#[test]
fn the_socket_descriptors_of_a_descriptor_list_are_the_entries_of_type_socket() {
    let mut list = Vec::new();
    for (fd, kind) in [(0_i32, 1_i32), (3, 2), (4, 3), (7, 2)] {
        list.extend_from_slice(&fd.to_ne_bytes());
        list.extend_from_slice(&kind.to_ne_bytes());
    }
    assert_eq!(socket_descriptors(&list), [3, 7]);
}

#[test]
fn a_socket_info_listens_on_a_port_only_as_a_tcp_socket_in_the_listen_state() {
    let info = |kind: i32, port: u16, state: i32| {
        let mut info = vec![0_u8; 792];
        info[256..260].copy_from_slice(&kind.to_ne_bytes());
        info[268..270].copy_from_slice(&port.to_be_bytes());
        info[344..348].copy_from_slice(&state.to_ne_bytes());
        info
    };
    assert!(socket_listens_on(&info(2, 24_001, 1), 24_001));
    assert!(!socket_listens_on(&info(2, 24_001, 1), 24_002));
    assert!(!socket_listens_on(&info(2, 24_001, 4), 24_001));
    assert!(!socket_listens_on(&info(1, 24_001, 1), 24_001));
    assert!(!socket_listens_on(&info(2, 24_001, 1)[..300], 24_001));
}

#[test]
fn this_process_holds_its_own_listening_socket_as_a_member_of_its_group() {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let own = u32::try_from(rustix::process::getpgrp().as_raw_nonzero().get()).unwrap();
    let held = listen_holder(own, port).unwrap();
    // A group that no process leads sees this process as another holder.
    let other = listen_holder(0x7fff_fff0, port).unwrap();
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        assert_eq!(held, Some(Holder::Group));
        assert_eq!(other, Some(Holder::Other(Some(std::process::id()))));
    } else {
        assert_eq!((held, other), (None, None));
    }
    drop(listener);
}

fn stop<'a>(
    keeper: Option<&'a crate::record::run::HostProcess>,
    app: Option<&'a crate::record::run::HostProcess>,
) -> HostStop<'a> {
    HostStop {
        keeper,
        app,
        signal: StopSignal::Term,
        grace: Duration::from_millis(20),
        kill_wait: Duration::from_millis(20),
    }
}

#[test]
fn stopping_a_keeper_and_an_app_that_are_both_gone_signals_nothing() {
    let processes = FakeProcesses::default();
    let keeper = host_at(100, 1);
    let app = host_at(200, 2);

    let stopped = processes
        .stop(
            &stop(Some(&keeper), Some(&app)),
            now() + Duration::from_secs(1),
        )
        .unwrap();

    assert_eq!(stopped, HostStopped::AlreadyGone);
    assert!(processes.signals.borrow().is_empty());
}

#[test]
fn stopping_sends_term_to_the_keeper_and_then_the_stop_signal_to_an_app_the_keeper_left() {
    let processes = FakeProcesses::default()
        .with(100, Start::At(1), Some(Signal::TERM))
        .with(200, Start::At(2), Some(Signal::TERM));
    let keeper = host_at(100, 1);
    let app = host_at(200, 2);

    let stopped = processes
        .stop(
            &stop(Some(&keeper), Some(&app)),
            now() + Duration::from_secs(1),
        )
        .unwrap();

    assert_eq!(stopped, HostStopped::Stopped);
    assert_eq!(
        *processes.signals.borrow(),
        vec![(100, Signal::TERM), (200, Signal::TERM)]
    );
}

#[test]
fn stopping_an_app_whose_keeper_is_gone_sends_its_stop_signal_to_the_app() {
    let processes = FakeProcesses::default().with(200, Start::At(2), Some(Signal::INT));
    let app = host_at(200, 2);
    let request = HostStop {
        signal: StopSignal::Int,
        ..stop(None, Some(&app))
    };

    let stopped = processes
        .stop(&request, now() + Duration::from_secs(1))
        .unwrap();

    assert_eq!(stopped, HostStopped::Stopped);
    assert_eq!(*processes.signals.borrow(), vec![(200, Signal::INT)]);
}

#[test]
fn an_app_that_survives_kill_is_unstoppable_and_names_its_group() {
    let processes = FakeProcesses::default().with(200, Start::At(2), None);
    let app = host_at(200, 2);

    let stopped = processes
        .stop(&stop(None, Some(&app)), now() + Duration::from_secs(1))
        .unwrap();

    assert_eq!(stopped, HostStopped::Unstoppable { group: 200 });
    assert_eq!(
        *processes.signals.borrow(),
        vec![(200, Signal::TERM), (200, Signal::KILL)]
    );
}

#[test]
fn stopping_leaves_a_process_that_phase_may_not_read_and_reports_it_as_not_ours() {
    let processes = FakeProcesses::default().with(200, Start::Unreadable, Some(Signal::TERM));
    let app = host_at(200, 2);

    let stopped = processes
        .stop(&stop(None, Some(&app)), now() + Duration::from_secs(1))
        .unwrap();

    assert_eq!(stopped, HostStopped::NotOurs);
    assert!(processes.signals.borrow().is_empty());
}

#[test]
fn stopping_leaves_a_process_whose_pid_was_reused() {
    let processes = FakeProcesses::default().with(200, Start::At(99), Some(Signal::TERM));
    let app = host_at(200, 2);

    let stopped = processes
        .stop(&stop(None, Some(&app)), now() + Duration::from_secs(1))
        .unwrap();

    assert_eq!(stopped, HostStopped::AlreadyGone);
    assert!(processes.signals.borrow().is_empty());
}

#[test]
fn the_system_runner_stops_a_real_process_group_and_reports_it_gone() {
    let mut sleeper = Spawned::sleeper();
    let app = sleeper.host();

    let stopped = System
        .stop(&stop(None, Some(&app)), now() + Duration::from_secs(5))
        .unwrap();

    assert_eq!(stopped, HostStopped::Stopped);
    assert!(!sleeper.running());
    assert_eq!(System.identity(&app).unwrap(), Identity::Gone);
}

#[test]
fn the_rest_of_a_long_log_is_its_last_lines_up_to_the_end_of_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("api.server.log");
    let mut lines = LogFollower::at_end_or_empty(&log).unwrap();
    let filler = "x".repeat(99) + "\n";
    let count = usize::try_from(MOST_FOLLOW_BYTES).unwrap() * 2 / filler.len();
    write_file(&log, &(filler.repeat(count) + "last words\n"));

    let rest = lines.rest().unwrap();

    assert_eq!(rest.last().map(String::as_str), Some("last words"));
    assert!(
        rest.iter()
            .all(|line| line == "last words" || line.len() == 99)
    );
}

#[test]
fn the_system_runner_finds_a_port_in_use_on_the_loopback_address() {
    let held = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = held.local_addr().unwrap().port();

    assert!(System.port_in_use(port).unwrap());
    drop(held);
}

#[test]
fn the_system_runner_finds_a_port_in_use_on_the_wildcard_address() {
    let held = std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    let port = held.local_addr().unwrap().port();

    assert!(System.port_in_use(port).unwrap());
    drop(held);
}

#[test]
fn the_system_runner_finds_a_port_in_use_on_the_ipv6_wildcard_address() {
    // A machine without IPv6 cannot hold the port this way.
    let Ok(held) = std::net::TcpListener::bind((std::net::Ipv6Addr::UNSPECIFIED, 0)) else {
        return;
    };
    let port = held.local_addr().unwrap().port();

    assert!(System.port_in_use(port).unwrap());
    drop(held);
}

#[test]
fn the_rest_of_a_long_log_keeps_the_line_that_starts_where_the_last_part_begins() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("api.server.log");
    let mut lines = LogFollower::at_end_or_empty(&log).unwrap();
    // Each line takes 1024 bytes with its line ending, so the last
    // mebibyte begins exactly at the start of line 2048.
    let text: String = (0..3072).map(|line| format!("{line:0>1023}\n")).collect();
    write_file(&log, &text);

    let rest = lines.rest().unwrap();

    assert_eq!(rest.len(), 1024);
    assert_eq!(rest.first(), Some(&format!("{:0>1023}", 2048)));
}

#[test]
fn stopping_a_recorded_pid_or_group_of_0_or_1_signals_nothing_and_reports_not_ours() {
    let processes = FakeProcesses::default()
        .with(1, Start::At(1), Some(Signal::TERM))
        .with(200, Start::At(2), Some(Signal::TERM));
    let init = host_at(1, 1);
    let in_group_one = crate::record::run::HostProcess {
        group: 1,
        ..host_at(200, 2)
    };
    let in_group_zero = crate::record::run::HostProcess {
        group: 0,
        ..host_at(200, 2)
    };

    for (keeper, app) in [
        (Some(&init), None),
        (None, Some(&in_group_one)),
        (None, Some(&in_group_zero)),
    ] {
        let stopped = processes
            .stop(&stop(keeper, app), now() + Duration::from_secs(1))
            .unwrap();

        assert_eq!(stopped, HostStopped::NotOurs);
    }
    assert!(processes.signals.borrow().is_empty());
}

#[test]
fn the_system_process_table_refuses_to_signal_pid_or_group_0_or_1() {
    let init = host_at(1, 1);
    let in_group_one = crate::record::run::HostProcess {
        group: 1,
        ..host_at(4242, 2)
    };

    assert!(!crate::record::process::signallable(&init));
    assert!(!crate::record::process::signallable(&in_group_one));
    assert!(crate::record::process::signallable(&host_at(4242, 2)));
    assert!(!System.group_exists(1));
    assert!(!System.group_exists(0));
}

/// The real process table of this machine, which refuses every signal. A
/// stop that must signal nothing runs through it, so a mistake fails the
/// test instead of signalling a real process.
struct NoSignals;

impl crate::record::process::Processes for NoSignals {
    fn start_time(&self, pid: u32) -> Result<Start, crate::PhaseError> {
        System.start_time(pid)
    }

    fn boot_id(&self) -> Result<Option<String>, crate::PhaseError> {
        System.boot_id()
    }

    fn group_exists(&self, group: u32) -> bool {
        System.group_exists(group)
    }

    fn reap(&self, pid: u32) {
        System.reap(pid);
    }

    fn signal(
        &self,
        host: &crate::record::run::HostProcess,
        signal: Signal,
    ) -> Result<(), crate::PhaseError> {
        panic!("the stop sent {signal:?} to {host:?}, and it must signal nothing");
    }
}

/// Stops `stop` through the real process table without signalling, and
/// returns how the stop ended.
fn stop_unsignalled(stop: &HostStop<'_>) -> HostStopped {
    crate::record::process::stop_host(&NoSignals, stop, now() + Duration::from_secs(5)).unwrap()
}

/// A shell that ignores `TERM`, as do the commands it runs, and prints a
/// line once it is ready.
const IGNORES_TERM: &str = "trap '' TERM; echo ready; while :; do sleep 0.05; done";

/// Returns a stop of `app` alone with `grace` and `kill_wait`.
fn app_stop(
    app: &crate::record::run::HostProcess,
    grace: Duration,
    kill_wait: Duration,
) -> HostStop<'_> {
    HostStop {
        keeper: None,
        app: Some(app),
        signal: StopSignal::Term,
        grace,
        kill_wait,
    }
}

#[test]
fn the_system_runner_leaves_a_running_process_whose_recorded_start_does_not_match() {
    let mut sleeper = Spawned::sleeper();
    let reused = crate::record::run::HostProcess {
        started_at_us: sleeper.host().started_at_us + 1,
        ..sleeper.host()
    };

    assert_eq!(System.identity(&reused).unwrap(), Identity::Reused);
    assert_eq!(
        stop_unsignalled(&stop(None, Some(&reused))),
        HostStopped::AlreadyGone
    );
    assert!(sleeper.running());
}

#[test]
fn the_system_runner_signals_nothing_for_a_child_that_has_exited_under_a_wrong_start() {
    let mut child = Spawned::shell("echo ready; exec sleep 0.2");
    let recorded = crate::record::run::HostProcess {
        started_at_us: child.host().started_at_us + 1,
        ..child.host()
    };
    child.wait_for_exit();

    assert_eq!(System.identity(&recorded).unwrap(), Identity::Gone);
    assert_eq!(
        stop_unsignalled(&stop(Some(&recorded), Some(&recorded))),
        HostStopped::AlreadyGone
    );
}

#[cfg(target_os = "linux")]
#[test]
fn the_system_runner_leaves_a_process_recorded_without_a_boot_id_as_not_ours() {
    let mut sleeper = Spawned::sleeper();
    let unknown = crate::record::run::HostProcess {
        boot_id: None,
        ..sleeper.host()
    };

    assert_eq!(System.identity(&unknown).unwrap(), Identity::Unknown);
    assert_eq!(
        stop_unsignalled(&stop(None, Some(&unknown))),
        HostStopped::NotOurs
    );
    assert!(sleeper.running());
}

// Each record names a running child of this test with a pid or group of
// 0 or 1. The stop runs through a table that panics on any signal, so a
// refusal that came after the signal would fail the test without
// signalling init or every process of the user.
#[test]
fn the_system_runner_refuses_a_recorded_pid_or_group_of_0_or_1_before_any_signal() {
    let mut sleeper = Spawned::sleeper();
    let real = sleeper.host();
    let cases = [
        crate::record::run::HostProcess {
            pid: 1,
            ..real.clone()
        },
        crate::record::run::HostProcess {
            pid: 0,
            ..real.clone()
        },
        crate::record::run::HostProcess {
            group: 1,
            ..real.clone()
        },
        crate::record::run::HostProcess {
            group: 0,
            ..real.clone()
        },
    ];

    for refused in &cases {
        assert_eq!(
            stop_unsignalled(&stop(Some(refused), Some(&real))),
            HostStopped::NotOurs
        );
        assert_eq!(
            stop_unsignalled(&stop(None, Some(refused))),
            HostStopped::NotOurs
        );
    }
    assert!(sleeper.running());
}

#[test]
fn the_system_runner_returns_as_soon_as_a_process_ends_on_its_stop_signal() {
    let mut sleeper = Spawned::sleeper();
    let app = sleeper.host();
    let started = now();

    let stopped = System
        .stop(
            &app_stop(&app, Duration::from_secs(10), Duration::from_secs(10)),
            now() + Duration::from_secs(30),
        )
        .unwrap();

    assert_eq!(stopped, HostStopped::Stopped);
    assert!(now() - started < Duration::from_secs(5));
    assert!(!sleeper.running());
}

#[test]
fn the_system_runner_kills_a_process_that_ignores_its_stop_signal_after_the_grace_and_kill_wait() {
    let mut stubborn = Spawned::shell(IGNORES_TERM);
    let app = stubborn.host();
    let (grace, kill_wait) = (Duration::from_millis(300), Duration::from_millis(200));
    let started = now();

    let stopped = System
        .stop(
            &app_stop(&app, grace, kill_wait),
            now() + Duration::from_secs(30),
        )
        .unwrap();

    assert_eq!(stopped, HostStopped::Stopped);
    assert!(now() - started >= grace + kill_wait);
    assert!(!stubborn.running());
    assert!(!System.group_exists(app.group));
}

#[test]
fn the_system_runner_cuts_the_grace_short_at_the_deadline_and_returns_within_the_kill_wait() {
    let mut stubborn = Spawned::shell(IGNORES_TERM);
    let app = stubborn.host();
    let kill_wait = Duration::from_millis(200);
    let started = now();
    let deadline = started + Duration::from_millis(300);

    let stopped = System
        .stop(
            &app_stop(&app, Duration::from_secs(30), kill_wait),
            deadline,
        )
        .unwrap();

    let took = now() - started;
    assert_eq!(stopped, HostStopped::Stopped);
    assert!(took >= Duration::from_millis(300), "{took:?}");
    assert!(
        took < Duration::from_millis(300) + kill_wait + Duration::from_secs(2),
        "{took:?}"
    );
    assert!(!stubborn.running());
}

#[test]
fn the_system_runner_gives_an_app_its_whole_grace_when_its_keeper_ends_late() {
    let dir = tempfile::tempdir().unwrap();
    let graceful = dir.path().join("graceful");
    // The keeper takes a second to end on TERM, and the app takes 0.8
    // seconds to end on its own signal. The first wait of grace and kill
    // wait, 1.5 seconds, ends before the app would finish if it counted
    // from the keeper's signal.
    let mut keeper = Spawned::shell("trap 'sleep 1; exit 0' TERM; echo ready; sleep 30 & wait");
    let mut app = Spawned::shell(&format!(
        "trap 'sleep 0.8; echo done > {}; exit 0' TERM; echo ready; sleep 30 & wait",
        graceful.display()
    ));
    let (keeper_host, app_host) = (keeper.host(), app.host());
    let request = HostStop {
        keeper: Some(&keeper_host),
        app: Some(&app_host),
        signal: StopSignal::Term,
        grace: Duration::from_millis(1200),
        kill_wait: Duration::from_millis(300),
    };

    let stopped = System
        .stop(&request, now() + Duration::from_secs(30))
        .unwrap();

    assert_eq!(stopped, HostStopped::Stopped);
    assert!(
        graceful.exists(),
        "the app was killed before its grace ended"
    );
    assert!(!keeper.running());
    assert!(!app.running());
}

// A member of the app's group that has ended and that its parent, this
// test, has not reaped stays in the group, and `KILL` cannot end it. The
// stop signals only that group, which holds only this test's children.
// macOS refuses `KILL` to a group that holds only such members with EPERM,
// so there the stop fails as a process-table error, and the test runs on
// Linux alone.
#[cfg(target_os = "linux")]
#[test]
fn the_system_runner_reports_a_group_that_outlives_kill_and_the_kill_wait_as_unstoppable() {
    let leader = Spawned::sleeper();
    let app = leader.host();
    let mut zombie = Spawned::shell_in_group("echo ready", 1, app.group);
    let (grace, kill_wait) = (Duration::from_millis(100), Duration::from_millis(200));
    let started = now();

    let stopped = System
        .stop(
            &app_stop(&app, grace, kill_wait),
            now() + Duration::from_secs(30),
        )
        .unwrap();

    assert_eq!(stopped, HostStopped::Unstoppable { group: app.group });
    assert!(now() - started >= grace + kill_wait + kill_wait);
    zombie.wait_for_exit();
    assert!(!System.group_exists(app.group));
    drop(leader);
}
