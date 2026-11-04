use std::fs;

use crate::backend::{Backend as _, Changes};
use crate::error::Error;
use crate::git::{Git, message};
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
    assert_eq!(repository.name, "inbox");
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
    let commit = Git
        .add_checkout(
            &fixture.clone,
            &checkout,
            "reading-list",
            "refs/remotes/origin/main",
        )
        .unwrap();
    assert_eq!(commit, latest);
    assert!(Git.owns_checkout(&fixture.clone, &checkout).unwrap());
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
    Git.add_checkout(&fixture.clone, &checkout, "topic", base)
        .unwrap();
    let unmerged = || {
        Git.unmerged_commits(&fixture.clone, "refs/heads/topic", base)
            .unwrap()
    };
    assert_eq!(unmerged(), 0);
    for _ in 0..2 {
        git(
            &checkout,
            &["commit", "--quiet", "--allow-empty", "-m", "Work"],
        );
    }
    assert_eq!(unmerged(), 2);
}

#[test]
fn commits_on_a_detached_head_count_as_stray_and_commits_on_any_branch_do_not() {
    let fixture = fixture();
    let checkout = fixture.dir.path().join("work");
    Git.add_checkout(
        &fixture.clone,
        &checkout,
        "topic",
        "refs/remotes/origin/main",
    )
    .unwrap();
    git(
        &checkout,
        &["commit", "--quiet", "--allow-empty", "-m", "Work"],
    );
    assert_eq!(Git.stray_commits(&checkout).unwrap(), 0);
    git(&checkout, &["switch", "--quiet", "--detach"]);
    git(
        &checkout,
        &["commit", "--quiet", "--allow-empty", "-m", "Lost"],
    );
    assert_eq!(Git.stray_commits(&checkout).unwrap(), 1);
}

#[test]
fn a_failure_message_prefers_the_last_fatal_or_error_line() {
    let stderr = "hint: try this\nerror: first\nwarning: noise\nfatal: the cause\nhint: more\n";
    assert_eq!(message(stderr), "fatal: the cause");
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
    Git.add_checkout(
        &fixture.clone,
        &checkout,
        "topic",
        "refs/remotes/origin/main",
    )
    .unwrap();
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
    assert!(!Git.owns_checkout(&fixture.clone, &checkout).unwrap());
    assert!(Git.owns_checkout(&fixture.clone, &moved).unwrap());
}

#[test]
fn a_failed_command_names_its_operation_and_the_last_line_of_its_error() {
    let fixture = fixture();
    let error = Git
        .delete_branch(&fixture.clone, "no-such-branch")
        .unwrap_err();
    let Error::CommandFailed {
        operation, message, ..
    } = error
    else {
        panic!("{error:?}");
    };
    assert_eq!(operation, "branch -D");
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
    git(&root, &["clone", "--quiet", "origin.git", "inbox"]);
    let clone = root.join("inbox");
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
