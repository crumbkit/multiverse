#![expect(clippy::disallowed_methods, reason = "the tests read the real clock")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::manifest::{Manifest, Secret};
use crate::preflight::{Approvals, Outcome, Preflight};

fn secrets(text: &str) -> Vec<(String, Secret, PathBuf)> {
    Manifest::parse(text, Path::new("/repo/mxv.toml"))
        .unwrap()
        .secrets
        .into_iter()
        .map(|(name, secret)| (name, secret, PathBuf::from(".")))
        .collect()
}

/// Returns approvals that cover every secret of `list`, in a directory that
/// lives as long as the returned guard.
fn approved(list: &[(String, Secret, PathBuf)]) -> (tempfile::TempDir, Approvals) {
    let dir = tempfile::tempdir().unwrap();
    let approvals = Approvals::in_state(dir.path(), "/repo");
    let all: Vec<&Secret> = list.iter().map(|(_, secret, _)| secret).collect();
    approvals.approve(&all).unwrap();
    (dir, approvals)
}

fn run(text: &str) -> Preflight {
    let list = secrets(text);
    let (_dir, approvals) = approved(&list);
    Preflight::run(&list, &approvals, Duration::from_secs(5))
}

#[test]
fn a_check_that_succeeds_needs_no_sign_in() {
    let report = run("[secrets.ok]\ncheck = [\"true\"]\nsign_in = \"sign in\"\n");
    assert!(report.missing().is_empty());
    assert_eq!(report.message(), None);
}

#[test]
fn a_check_that_fails_needs_its_sign_in_command() {
    let report =
        run("[secrets.aws]\ncheck = [\"false\"]\nsign_in = \"aws sso login --profile work\"\n");
    let missing = report.missing();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].name, "aws");
    assert_eq!(missing[0].sign_in, "aws sso login --profile work");
}

#[test]
fn one_message_lists_every_sign_in_with_its_exact_command() {
    let report = run(concat!(
        "[secrets.aws]\ncheck = [\"false\"]\nsign_in = \"aws sso login --profile work\"\n",
        "[secrets.good]\ncheck = [\"true\"]\nsign_in = \"never shown\"\n",
        "[secrets.op]\ncheck = [\"false\"]\nsign_in = \"op signin --account my\"\n",
    ));
    let message = report.message().unwrap();
    assert!(message.contains("2 sign-ins"), "{message}");
    assert!(
        message.contains("aws sso login --profile work"),
        "{message}"
    );
    assert!(message.contains("op signin --account my"), "{message}");
    assert!(!message.contains("never shown"), "{message}");
}

#[test]
fn a_program_that_does_not_exist_counts_as_missing() {
    let report = run("[secrets.x]\ncheck = [\"/no/such/program\"]\nsign_in = \"install it\"\n");
    assert_eq!(report.missing().len(), 1);
    assert!(matches!(report.checked[0].outcome, Outcome::Unrunnable));
}

#[test]
fn a_check_that_hangs_is_stopped_and_counts_as_missing() {
    let started = std::time::Instant::now();
    let list = secrets("[secrets.slow]\ncheck = [\"sleep\", \"30\"]\nsign_in = \"wake up\"\n");
    let (_dir, approvals) = approved(&list);
    let report = Preflight::run(&list, &approvals, Duration::from_millis(200));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(matches!(report.checked[0].outcome, Outcome::TimedOut));
}

#[test]
fn the_message_wraps_each_sign_in_command_in_backticks() {
    let report = run("[secrets.aws]\ncheck = [\"false\"]\nsign_in = \"aws sso login\"\n");
    assert!(report.message().unwrap().contains("aws: `aws sso login`"));
}

#[test]
fn a_check_that_no_one_approved_does_not_run() {
    let marker = tempfile::tempdir().unwrap();
    let touched = marker.path().join("ran");
    let text = format!(
        "[secrets.evil]\ncheck = [\"touch\", \"{}\"]\nsign_in = \"sign in\"\n",
        touched.display()
    );
    let list = secrets(&text);
    let state = tempfile::tempdir().unwrap();
    let report = Preflight::run(
        &list,
        &Approvals::in_state(state.path(), "/repo"),
        Duration::from_secs(5),
    );
    assert!(matches!(report.checked[0].outcome, Outcome::NotApproved));
    assert!(!touched.exists());
    assert!(report.missing().is_empty());
    let message = report.message().unwrap();
    assert!(
        message.contains("did not run the check of evil"),
        "{message}"
    );
}

#[test]
fn an_approval_covers_the_same_command_line_in_any_manifest() {
    let state = tempfile::tempdir().unwrap();
    let approvals = Approvals::in_state(state.path(), "/repo");
    let one = secrets("[secrets.a]\ncheck = [\"true\"]\nsign_in = \"x\"\n");
    let same = secrets("[secrets.b]\ncheck = [\"true\"]\nsign_in = \"y\"\n");
    let other = secrets("[secrets.c]\ncheck = [\"true\", \"-v\"]\nsign_in = \"x\"\n");
    approvals.approve(&[&one[0].1]).unwrap();
    assert!(approvals.covers(&same[0].1));
    assert!(!approvals.covers(&other[0].1));
}

#[test]
fn a_workspace_with_no_secrets_has_nothing_to_report() {
    let report = Preflight::run(
        &[],
        &Approvals::in_state(Path::new("/no/such"), "/repo"),
        Duration::from_secs(1),
    );
    assert!(report.checked.is_empty());
    assert_eq!(report.message(), None);
}

#[test]
fn an_approval_does_not_cover_another_repository() {
    let state = tempfile::tempdir().unwrap();
    let one = Approvals::in_state(state.path(), "/repo/a");
    let other = Approvals::in_state(state.path(), "/repo/b");
    let list = secrets("[secrets.a]\ncheck = [\"./check\"]\nsign_in = \"x\"\n");
    one.approve(&[&list[0].1]).unwrap();
    assert!(one.covers(&list[0].1));
    assert!(!other.covers(&list[0].1));
}

#[test]
fn without_a_state_directory_nothing_is_approved_and_nothing_is_written() {
    let list = secrets("[secrets.a]\ncheck = [\"true\"]\nsign_in = \"x\"\n");
    let none = Approvals::none();
    assert!(!none.covers(&list[0].1));
    assert!(none.approve(&[&list[0].1]).is_err());
}

#[test]
fn an_approvals_file_that_cannot_be_read_keeps_its_lines() {
    let state = tempfile::tempdir().unwrap();
    let approvals = Approvals::in_state(state.path(), "/repo");
    let list = secrets("[secrets.a]\ncheck = [\"true\"]\nsign_in = \"x\"\n");
    // A directory where the file belongs cannot be read as a file.
    std::fs::create_dir(state.path().join("preflight-approved")).unwrap();
    assert!(approvals.approve(&[&list[0].1]).is_err());
    assert!(!approvals.covers(&list[0].1));
}
