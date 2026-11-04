use std::fs;

use crate::testing::write;
use std::path::Path;

use rustix::process::Signal;

use crate::clock::{POLL_INTERVAL, pause};
use crate::process::{
    Launch, exit_code, group_exists, launch, signal, start_time, stat_start_time,
};

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

fn started(dir: &Path, words: &[&str], env: &[(String, String)]) -> crate::process::Launched {
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
    let env = [("GREETING".to_owned(), "hello world".to_owned())];
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
fn the_start_time_of_a_process_id_that_no_process_has_is_none() {
    assert_eq!(start_time(i32::MAX as u32 - 7).unwrap(), None);
}

#[test]
fn the_start_time_in_a_proc_stat_line_is_field_22_even_when_the_name_holds_parentheses() {
    let line = "4242 (my (odd) app) S 1 4242 4242 0 -1 4194560 120 0 0 0 3 1 0 0 20 0 1 0 987654 12345678 300\n";
    assert_eq!(stat_start_time(line).as_deref(), Some("987654"));
    assert_eq!(stat_start_time("4242 (short) S 1"), None);
}
