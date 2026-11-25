//! Tests of the rules by which the tests' docker sweep finds a temp root
//! and decides that its test run has ended. They need no docker daemon.

#![expect(clippy::disallowed_methods, reason = "the tests read the real clock")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::test_docker::{
    MOST_UNOWNED_AGE, OWNER_FILE, OwnerLock, has_ended, run_record, temp_root_of,
};

fn bases() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/var/folders/ab/T"),
        PathBuf::from("/private/var/folders/ab/T"),
    ]
}

#[test]
fn a_run_record_below_the_temp_directory_lies_in_the_tempfile_directory_below_it() {
    assert_eq!(
        temp_root_of(
            Path::new("/var/folders/ab/T/.tmpA1b2/state/mxv/phase/runs/x.json"),
            &bases()
        ),
        Some(PathBuf::from("/var/folders/ab/T/.tmpA1b2"))
    );
    assert_eq!(
        temp_root_of(
            Path::new("/private/var/folders/ab/T/.tmpA1b2/runs/x.json"),
            &bases()
        ),
        Some(PathBuf::from("/private/var/folders/ab/T/.tmpA1b2"))
    );
}

#[test]
fn a_run_record_below_a_target_tmp_lies_in_the_tempfile_directory_below_it() {
    assert_eq!(
        temp_root_of(
            Path::new("/work/multiverse.worktrees/a/target/tmp/.tmpQ9/state/mxv/phase/runs/x.json"),
            &bases()
        ),
        Some(PathBuf::from(
            "/work/multiverse.worktrees/a/target/tmp/.tmpQ9"
        ))
    );
}

#[test]
fn a_run_record_outside_a_tempfile_directory_lies_in_no_temp_root() {
    for path in [
        "/home/someone/.local/state/mxv/phase/runs/x.json",
        "/var/folders/ab/T/mxv-test-ports/runs/x.json",
        "/var/folders/ab/T/.tmpA1b2",
        "/work/target/tmp/.tmpQ9",
        "/work/target/tmp/other/runs/x.json",
    ] {
        assert_eq!(temp_root_of(Path::new(path), &bases()), None, "{path}");
    }
}

#[test]
fn the_run_record_label_is_found_under_any_prefix_that_phase_accepts() {
    let labels = |key: &str, value: &str| BTreeMap::from([(key.to_owned(), value.to_owned())]);
    assert_eq!(
        run_record(&labels("mxv.run-record", "/t/.tmp1/runs/x.json")),
        Some(Path::new("/t/.tmp1/runs/x.json"))
    );
    assert_eq!(
        run_record(&labels("example.run-record", "/t/.tmp1/runs/x.json")),
        Some(Path::new("/t/.tmp1/runs/x.json"))
    );
    assert_eq!(run_record(&labels("Mxv.run-record", "/t/x.json")), None);
    assert_eq!(run_record(&labels(".run-record", "/t/x.json")), None);
    assert_eq!(
        run_record(&labels("mxv.run-record", "relative/x.json")),
        None
    );
    assert_eq!(run_record(&labels("mxv.workspace-id", "/t/x.json")), None);
}

#[test]
fn a_temp_root_that_no_longer_exists_has_ended() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(".tmpgone");
    assert!(has_ended(&root, SystemTime::now()));
}

#[test]
fn a_temp_root_whose_owner_file_is_locked_has_not_ended() {
    let root = tempfile::tempdir().unwrap();
    let _owner = OwnerLock::take(root.path());
    assert!(!has_ended(root.path(), SystemTime::now()));
}

#[test]
fn a_temp_root_whose_owner_file_nobody_locks_has_ended() {
    let root = tempfile::tempdir().unwrap();
    let owner = OwnerLock::take(root.path());
    owner.release();
    assert!(root.path().join(OWNER_FILE).exists());
    assert!(has_ended(root.path(), SystemTime::now()));
}

#[test]
fn a_temp_root_without_an_owner_file_ends_only_once_it_is_older_than_the_limit() {
    let root = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    assert!(!has_ended(root.path(), now));
    assert!(!has_ended(
        root.path(),
        now + MOST_UNOWNED_AGE - Duration::from_secs(60)
    ));
    assert!(has_ended(
        root.path(),
        now + MOST_UNOWNED_AGE + Duration::from_secs(60)
    ));
}
