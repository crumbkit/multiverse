use std::fs;

use crate::backend::Backend as _;
use crate::error::Error;
use crate::git::Git;
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
    assert_eq!(Git.base(&fixture.clone).unwrap(), "origin/main");
}

#[test]
fn a_checkout_starts_at_the_fetched_base_on_a_branch_that_tracks_nothing() {
    let fixture = fixture();
    let latest = advance_origin(&fixture);
    Git.fetch(&fixture.clone).unwrap();
    let checkout = fixture.dir.path().join("work");
    let commit = Git
        .add_checkout(&fixture.clone, &checkout, "reading-list", "origin/main")
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
    assert!(Git.changes(&fixture.clone).unwrap().is_empty());
    write(fixture.clone.join("notes.md"), "notes");
    assert_eq!(Git.changes(&fixture.clone).unwrap(), ["?? notes.md"]);
}

#[test]
fn a_branch_without_commits_of_its_own_is_merged_and_one_with_a_commit_is_not() {
    let fixture = fixture();
    let checkout = fixture.dir.path().join("work");
    Git.add_checkout(&fixture.clone, &checkout, "topic", "origin/main")
        .unwrap();
    assert!(
        Git.is_merged(&fixture.clone, "topic", "origin/main")
            .unwrap()
    );
    git(
        &checkout,
        &["commit", "--quiet", "--allow-empty", "-m", "Work"],
    );
    assert!(
        !Git.is_merged(&fixture.clone, "topic", "origin/main")
            .unwrap()
    );
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
