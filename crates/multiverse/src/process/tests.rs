use std::fs;

use crate::testing::write;
use std::path::Path;

use rustix::process::Signal;

use crate::clock::{POLL_INTERVAL, pause};
use phase::host::RawStart;

use crate::process::{
    Holder, Launch, UNREADABLE, bsd_start, exit_code, group_exists, is_bsd_start, launch,
    listen_holder, recorded_start, signal, start_time,
};
use crate::redacted::Redacted;

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

fn started(dir: &Path, words: &[&str], env: &[(String, Redacted)]) -> crate::process::Launched {
    let argv = argv(words);
    launch(&Launch {
        name: "test",
        argv: &argv,
        cwd: dir,
        env,
        log: &dir.join("test.log"),
        exit_file: &dir.join("test.exit"),
    })
    .unwrap()
}

fn wait_for_end(launched: &mut crate::process::Launched) -> Option<i32> {
    loop {
        if let Some(ended) = launched.ended() {
            return ended.code;
        }
        pause(POLL_INTERVAL);
    }
}

#[test]
fn a_launched_process_appends_its_output_to_its_log_and_records_its_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path().join("test.log"), "earlier\n");
    let env = [(
        "GREETING".to_owned(),
        Redacted::new("hello world".to_owned()),
    )];
    let mut launched = started(
        dir.path(),
        &["sh", "-c", "echo \"$GREETING\"; echo oops >&2; exit 3"],
        &env,
    );
    assert_eq!(wait_for_end(&mut launched), Some(0));
    let log = fs::read_to_string(dir.path().join("test.log")).unwrap();
    assert_eq!(log, "earlier\nhello world\noops\n");
    assert_eq!(exit_code(&dir.path().join("test.exit")), Some(Some(3)));
}

#[test]
fn a_launched_process_passes_each_argument_as_one_value() {
    let dir = tempfile::tempdir().unwrap();
    let mut launched = started(dir.path(), &["printf", "[%s]\\n", "a b", "$HOME", "*"], &[]);
    wait_for_end(&mut launched);
    let log = fs::read_to_string(dir.path().join("test.log")).unwrap();
    assert_eq!(log, "[a b]\n[$HOME]\n[*]\n");
}

#[test]
fn a_program_that_does_not_exist_records_exit_code_127() {
    let dir = tempfile::tempdir().unwrap();
    let mut launched = started(dir.path(), &["mxv-no-such-program"], &[]);
    wait_for_end(&mut launched);
    assert_eq!(exit_code(&dir.path().join("test.exit")), Some(Some(127)));
}

#[test]
fn a_launch_in_a_directory_that_does_not_exist_fails_as_unstartable() {
    let dir = tempfile::tempdir().unwrap();
    let argv = argv(&["true"]);
    let error = launch(&Launch {
        name: "web",
        argv: &argv,
        cwd: &dir.path().join("missing"),
        env: &[],
        log: &dir.path().join("web.log"),
        exit_file: &dir.path().join("web.exit"),
    })
    .unwrap_err();
    assert!(matches!(error, crate::Error::ProcessUnstartable { process, .. } if process == "web"));
}

#[test]
fn a_signal_to_the_group_stops_the_process_and_its_children() {
    let dir = tempfile::tempdir().unwrap();
    let mut launched = started(dir.path(), &["sh", "-c", "sleep 30 & sleep 30; wait"], &[]);
    let pid = launched.pid();
    assert!(group_exists(pid));
    assert!(start_time(pid).unwrap().is_some());
    signal(pid, Signal::TERM).unwrap();
    assert_eq!(wait_for_end(&mut launched), None);
    while group_exists(pid) {
        pause(POLL_INTERVAL);
    }
    assert_eq!(exit_code(&dir.path().join("test.exit")), None);
}

#[test]
fn an_exit_file_without_its_line_feed_holds_no_code_yet() {
    let dir = tempfile::tempdir().unwrap();
    let exit_file = dir.path().join("test.exit");
    for partial in ["", "7"] {
        crate::testing::write(&exit_file, partial);
        assert_eq!(exit_code(&exit_file), None, "{partial:?}");
    }
    crate::testing::write(&exit_file, "7\n");
    assert_eq!(exit_code(&exit_file), Some(Some(7)));
    crate::testing::write(&exit_file, "x\n");
    assert_eq!(exit_code(&exit_file), Some(None));
}

#[test]
fn the_start_time_of_a_process_id_that_no_process_has_is_none() {
    assert_eq!(start_time(i32::MAX as u32 - 7).unwrap(), None);
}

#[test]
fn two_processes_started_20_ms_apart_have_different_start_times() {
    let dir = tempfile::tempdir().unwrap();
    let first = started(dir.path(), &["sh", "-c", "exec sleep 30"], &[]);
    // Linux counts a start in clock ticks, which are 10 ms at the usual
    // 100 Hz, and macOS counts it in microseconds.
    crate::clock::pause(std::time::Duration::from_millis(20));
    let second = started(dir.path(), &["sh", "-c", "exec sleep 30"], &[]);
    let times = (
        start_time(first.pid()).unwrap().unwrap(),
        start_time(second.pid()).unwrap().unwrap(),
    );
    first.kill();
    second.kill();
    assert_ne!(times.0, times.1);
}

#[test]
fn a_bsd_start_time_keeps_its_microseconds_and_never_equals_an_unreadable_one() {
    assert_eq!(bsd_start(1_793_809_711, 139_785), "1793809711.139785");
    assert_eq!(bsd_start(1_793_809_711, 5), "1793809711.000005");
    assert_ne!(bsd_start(0, 0), UNREADABLE);
}

#[test]
fn a_start_time_of_seconds_and_microseconds_is_libprocs_and_what_ps_prints_is_not() {
    assert!(is_bsd_start("1793809711.139785"));
    assert!(!is_bsd_start("Wed Nov  4 10:00:00 2026"));
    assert!(!is_bsd_start("1793809711"));
    assert!(!is_bsd_start("1793809711.13"));
    assert!(!is_bsd_start(".139785"));
}

#[test]
fn the_holder_of_a_listening_socket_is_the_group_or_names_the_other_process() {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let own = u32::try_from(rustix::process::getpgrp().as_raw_nonzero().get()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let other = started(dir.path(), &["sh", "-c", "exec sleep 30"], &[]);
    let answers = (
        listen_holder(own, port).unwrap(),
        listen_holder(other.pid(), port).unwrap(),
    );
    other.kill();
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        assert_eq!(
            answers,
            (
                Some(Holder::Group),
                Some(Holder::Other(Some(std::process::id())))
            )
        );
    } else {
        assert_eq!(answers, (None, None));
    }
}

#[test]
fn a_launched_process_that_is_dropped_before_it_is_kept_is_killed_with_its_group() {
    let dir = tempfile::tempdir().unwrap();
    let launched = started(dir.path(), &["sh", "-c", "sleep 30 & exec sleep 30"], &[]);
    let pid = launched.pid();
    assert!(group_exists(pid));
    drop(launched);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while group_exists(pid) && std::time::Instant::now() < deadline {
        pause(POLL_INTERVAL);
    }
    assert!(!group_exists(pid));
}

#[test]
fn a_launched_process_that_is_kept_outlives_its_handle() {
    let dir = tempfile::tempdir().unwrap();
    let launched = started(dir.path(), &["sh", "-c", "exec sleep 30"], &[]);
    let pid = launched.pid();
    launched.keep();
    assert!(group_exists(pid));
    signal(pid, Signal::KILL).unwrap();
}

#[test]
fn a_raw_start_becomes_the_start_that_run_records_hold() {
    assert_eq!(
        recorded_start(RawStart::Ticks(987_654)).as_deref(),
        Some("987654")
    );
    assert_eq!(
        recorded_start(RawStart::Since {
            seconds: 1_793_809_711,
            microseconds: 5
        })
        .as_deref(),
        Some("1793809711.000005")
    );
    assert_eq!(recorded_start(RawStart::Absent), None);
    // A process of another user has the start `unreadable` on macOS, and
    // none on Linux, as before phase read the process table.
    assert_eq!(
        recorded_start(RawStart::Unreadable).as_deref(),
        cfg!(target_os = "macos").then_some(UNREADABLE)
    );
}
