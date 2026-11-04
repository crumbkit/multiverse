use std::fs;

use crate::backend::{Backend as _, Changes};
use crate::error::Error;
use crate::git::{Git, explanation, message};
use crate::testing::{advance_origin, fixture, git, write};

#[test]
fn a_directory_outside_any_repository_is_not_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    let error = Git.repository(dir.path()).unwrap_err();
    assert!(
        matches!(error, Error::RepositoryNotFound { .. }),
        "{error:?}"
    );
}

#[test]
fn a_path_that_does_not_exist_is_not_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    let error = Git.repository(&dir.path().join("missing")).unwrap_err();
    assert!(
        matches!(error, Error::RepositoryNotFound { .. }),
        "{error:?}"
    );
}

#[test]
fn a_repository_without_an_origin_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "--quiet"]);
    let error = Git.repository(dir.path()).unwrap_err();
    assert!(matches!(error, Error::NoOrigin { .. }), "{error:?}");
}

#[test]
fn a_subdirectory_of_a_clone_names_the_clone_and_its_directory_name() {
    let fixture = fixture();
    let sub = fixture.clone.join("sub");
    fs::create_dir(&sub).unwrap();
    let repository = Git.repository(&sub).unwrap();
    assert_eq!(repository.root, fixture.clone);
    assert_eq!(repository.name, "app");
}

#[test]
fn the_base_is_the_default_branch_of_origin() {
    let fixture = fixture();
    assert_eq!(
        Git.base(&fixture.clone).unwrap(),
        "refs/remotes/origin/main"
    );
}

#[test]
fn a_checkout_starts_at_the_fetched_base_on_a_branch_that_tracks_nothing() {
    let fixture = fixture();
    let latest = advance_origin(&fixture);
    Git.fetch(&fixture.clone).unwrap();
    let checkout = fixture.dir.path().join("work");
    let created = Git
        .create_branch(&fixture.clone, "reading-list", "refs/remotes/origin/main")
        .unwrap();
    assert_eq!(created.as_deref(), Some(latest.as_str()));
    let commit = Git
        .add_checkout(&fixture.clone, &checkout, "reading-list")
        .unwrap();
    assert_eq!(commit, latest);
    let listed = Git.listed_checkout(&fixture.clone, &checkout).unwrap();
    assert_eq!(listed.and_then(|listed| listed.head), Some(latest));
    let upstream = git(
        &checkout,
        &[
            "for-each-ref",
            "--format=%(upstream)",
            "refs/heads/reading-list",
        ],
    );
    assert_eq!(upstream.trim(), "");
}

#[test]
fn a_new_file_in_a_checkout_is_a_change() {
    let fixture = fixture();
    assert_eq!(Git.changes(&fixture.clone).unwrap(), Changes::default());
    write(fixture.clone.join("notes.md"), "notes");
    assert_eq!(
        Git.changes(&fixture.clone).unwrap().changed,
        ["?? notes.md"]
    );
}

#[test]
fn an_ignored_file_and_an_ignored_directory_are_listed_apart_from_the_changes() {
    let fixture = fixture();
    write(fixture.clone.join(".git/info/exclude"), ".env\ntarget/\n");
    write(fixture.clone.join(".env"), "TOKEN=x");
    fs::create_dir_all(fixture.clone.join("target/debug")).unwrap();
    write(fixture.clone.join("target/debug/mxv"), "binary");
    let changes = Git.changes(&fixture.clone).unwrap();
    assert!(changes.changed.is_empty(), "{changes:?}");
    assert_eq!(changes.ignored, [".env", "target/"]);
}

#[test]
fn a_branch_counts_the_commits_that_its_base_does_not_hold() {
    let fixture = fixture();
    let base = "refs/remotes/origin/main";
    let checkout = fixture.dir.path().join("work");
    work(&fixture, &checkout, "topic");
    let unmerged = || {
        Git.unmerged_commits(&fixture.clone, "refs/heads/topic", base)
            .unwrap()
    };
    assert_eq!(unmerged(), Some(0));
    for _ in 0..2 {
        git(
            &checkout,
            &["commit", "--quiet", "--allow-empty", "-m", "Work"],
        );
    }
    assert_eq!(unmerged(), Some(2));
    git(&fixture.clone, &["update-ref", "-d", base]);
    assert_eq!(unmerged(), None);
}

/// Creates the branch `branch` from `origin/main` and checks it out at
/// `checkout`.
fn work(fixture: &crate::testing::Fixture, checkout: &std::path::Path, branch: &str) {
    Git.create_branch(&fixture.clone, branch, "refs/remotes/origin/main")
        .unwrap();
    Git.add_checkout(&fixture.clone, checkout, branch).unwrap();
}

#[test]
fn creating_a_branch_that_exists_leaves_it_where_it_was() {
    let fixture = fixture();
    git(
        &fixture.clone,
        &["commit", "--quiet", "--allow-empty", "-m", "Mine"],
    );
    let mine = git(&fixture.clone, &["rev-parse", "HEAD"]);
    git(&fixture.clone, &["branch", "topic"]);
    let created = Git
        .create_branch(&fixture.clone, "topic", "refs/remotes/origin/main")
        .unwrap();
    assert_eq!(created, None);
    assert_eq!(git(&fixture.clone, &["rev-parse", "topic"]), mine);
}

#[test]
fn a_branch_is_deleted_at_a_commit_only_while_it_still_points_there() {
    let fixture = fixture();
    let start = Git
        .create_branch(&fixture.clone, "topic", "refs/remotes/origin/main")
        .unwrap()
        .unwrap();
    git(
        &fixture.clone,
        &["commit", "--quiet", "--allow-empty", "-m", "Moved"],
    );
    git(&fixture.clone, &["branch", "--force", "topic", "HEAD"]);
    assert!(
        Git.delete_branch_at(&fixture.clone, "topic", &start)
            .is_err()
    );
    let moved = git(&fixture.clone, &["rev-parse", "HEAD"]);
    assert_eq!(
        Git.branch_tip(&fixture.clone, "topic").unwrap(),
        Some(moved.trim().to_owned())
    );
}

#[test]
fn a_branch_that_does_not_exist_has_no_tip() {
    let fixture = fixture();
    assert_eq!(
        Git.branch_tip(&fixture.clone, "no-such-branch").unwrap(),
        None
    );
}

#[test]
fn a_listed_checkout_reports_its_lock_and_a_detached_head() {
    let fixture = fixture();
    let checkout = fixture.dir.path().join("work");
    work(&fixture, &checkout, "topic");
    let path = checkout.to_str().unwrap();
    git(
        &fixture.clone,
        &["worktree", "lock", "--reason", "initializing", path],
    );
    git(&checkout, &["switch", "--quiet", "--detach"]);
    let listed = Git
        .listed_checkout(&fixture.clone, &checkout)
        .unwrap()
        .unwrap();
    assert_eq!(listed.locked.as_deref(), Some(crate::backend::INITIALIZING));
    assert!(listed.detached);
    Git.unlock_checkout(&fixture.clone, &checkout).unwrap();
    let listed = Git
        .listed_checkout(&fixture.clone, &checkout)
        .unwrap()
        .unwrap();
    assert_eq!(listed.locked, None);
}

#[test]
fn commits_on_a_detached_head_count_as_stray_and_commits_on_any_branch_do_not() {
    let fixture = fixture();
    let checkout = fixture.dir.path().join("work");
    work(&fixture, &checkout, "topic");
    git(
        &checkout,
        &["commit", "--quiet", "--allow-empty", "-m", "Work"],
    );
    assert_eq!(Git.stray_commits(&checkout, "HEAD").unwrap(), 0);
    git(&checkout, &["switch", "--quiet", "--detach"]);
    git(
        &checkout,
        &["commit", "--quiet", "--allow-empty", "-m", "Lost"],
    );
    assert_eq!(Git.stray_commits(&checkout, "HEAD").unwrap(), 1);
    let head = git(&checkout, &["rev-parse", "HEAD"]);
    assert_eq!(Git.stray_commits(&fixture.clone, head.trim()).unwrap(), 1);
}

#[test]
fn a_failure_message_prefers_the_last_fatal_or_error_line() {
    let stderr = "hint: try this\nerror: first\nwarning: noise\nfatal: the cause\nhint: more\n";
    assert_eq!(message(stderr), "fatal: the cause");
}

#[cfg(unix)]
#[test]
fn a_failure_of_a_command_that_a_signal_ended_names_the_signal() {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::ExitStatus;
    let killed = ExitStatus::from_raw(9);
    assert_eq!(explanation(killed, ""), "git was ended by signal 9");
    assert_eq!(
        explanation(killed, "fatal: the cause\n"),
        "fatal: the cause (git was ended by signal 9)"
    );
    assert_eq!(
        explanation(ExitStatus::from_raw(1 << 8), "fatal: the cause\n"),
        "fatal: the cause"
    );
}

#[test]
fn a_failure_message_without_a_fatal_line_quotes_the_last_lines_that_are_not_hints() {
    let stderr = "one\ntwo\nhint: skip\nthree\nfour\nfive\nsix\n\n";
    assert_eq!(message(stderr), "two / three / four / five / six");
}

#[test]
fn a_symbolic_link_in_place_of_a_checkout_is_not_the_checkout() {
    let fixture = fixture();
    let checkout = fixture.dir.path().join("work");
    work(&fixture, &checkout, "topic");
    let moved = fixture.dir.path().join("moved");
    git(
        &fixture.clone,
        &[
            "worktree",
            "move",
            checkout.to_str().unwrap(),
            moved.to_str().unwrap(),
        ],
    );
    std::os::unix::fs::symlink(&moved, &checkout).unwrap();
    assert_eq!(
        Git.listed_checkout(&fixture.clone, &checkout).unwrap(),
        None
    );
    assert!(
        Git.listed_checkout(&fixture.clone, &moved)
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_failed_command_names_its_operation_and_the_last_line_of_its_error() {
    let fixture = fixture();
    let error = Git
        .delete_branch_at(
            &fixture.clone,
            "no-such-branch",
            "0123456789012345678901234567890123456789",
        )
        .unwrap_err();
    let Error::CommandFailed {
        operation, message, ..
    } = error
    else {
        panic!("{error:?}");
    };
    assert_eq!(operation, "update-ref -d");
    assert!(message.contains("no-such-branch"), "{message}");
}

#[test]
fn a_fetch_from_an_origin_that_is_gone_fails() {
    let fixture = fixture();
    fs::remove_dir_all(&fixture.origin).unwrap();
    let error = Git.fetch(&fixture.clone).unwrap_err();
    assert!(
        matches!(
            error,
            Error::CommandFailed {
                operation: "fetch origin",
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_clone_of_an_origin_without_commits_has_an_empty_origin() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    git(
        &root,
        &[
            "init",
            "--quiet",
            "--bare",
            "--initial-branch=main",
            "origin.git",
        ],
    );
    git(&root, &["clone", "--quiet", "origin.git", "app"]);
    let clone = root.join("app");
    Git.fetch(&clone).unwrap();
    let error = Git.base(&clone).unwrap_err();
    assert!(matches!(error, Error::EmptyOrigin { .. }), "{error:?}");
}

#[test]
fn an_origin_with_branches_but_no_default_branch_has_no_default_branch() {
    let fixture = fixture();
    git(
        &fixture.clone,
        &["remote", "set-head", "origin", "--delete"],
    );
    git(
        &fixture.clone,
        &[
            "update-ref",
            "refs/remotes/origin/work",
            "refs/remotes/origin/main",
        ],
    );
    git(
        &fixture.clone,
        &["update-ref", "-d", "refs/remotes/origin/main"],
    );
    let error = Git.base(&fixture.clone).unwrap_err();
    assert!(matches!(error, Error::NoDefaultBranch { .. }), "{error:?}");
}
