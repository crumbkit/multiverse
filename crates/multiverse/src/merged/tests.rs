//! The tests of the merge verdict and of the board mount update.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::{BoardMount, PullRequests, Verdict, update_board_mount};
use crate::git::Git;
use crate::name::WorkspaceName;
use crate::record::Workspace;
use crate::store::Store;
use crate::testing::{Fixture, fixture, git, write};
use crate::workspaces::{Mode, NewOutcome, Workspaces};

struct Setup {
    fixture: Fixture,
    root: PathBuf,
    _state: TempDir,
    workspaces: Workspaces<Git>,
}

fn setup() -> Setup {
    let fixture = fixture();
    let state = tempfile::tempdir().unwrap();
    let base = state.path().canonicalize().unwrap();
    let workspaces =
        Workspaces::new(Store::new(base.join("state")), Git).with_mirrors(base.join("mirrors"));
    Setup {
        fixture,
        root: base.join("mxv"),
        _state: state,
        workspaces,
    }
}

fn created(setup: &Setup) -> Workspace {
    let name: WorkspaceName = "reading-list".parse().unwrap();
    match setup
        .workspaces
        .create(&setup.root, &name, &setup.fixture.clone, Mode::Change)
        .unwrap()
    {
        NewOutcome::Created(workspace) => workspace,
        other => panic!("{other:?}"),
    }
}

fn commit(checkout: &Path, file: &str, text: &str) {
    write(checkout.join(file), text);
    git(checkout, &["add", file]);
    git(checkout, &["commit", "--quiet", "-m", "Change"]);
}

/// A source of pull requests that answers the same for every branch.
struct Answers(bool);

impl PullRequests for Answers {
    fn merged(&self, _: &Path, _: &str, _: &str, _: &str) -> bool {
        self.0
    }
}

fn verdict(setup: &Setup, workspace: &Workspace, pulls: bool) -> Verdict {
    setup
        .workspaces
        .merge_verdict(workspace, &Answers(pulls))
        .unwrap()
}

fn kept(verdict: &Verdict) -> &str {
    match verdict {
        Verdict::Kept(reason) => reason,
        other => panic!("{other:?}"),
    }
}

/// Pushes the branch to origin under its own name, and to the main branch
/// of origin, as a fast-forward merge of a published branch does.
fn merge_by_fast_forward(checkout: &Path) {
    git(
        checkout,
        &[
            "push",
            "--quiet",
            "origin",
            "HEAD:refs/heads/reading-list",
            "HEAD:main",
        ],
    );
}

#[test]
fn a_workspace_without_commits_of_its_own_is_kept() {
    let setup = setup();
    let workspace = created(&setup);
    let verdict = verdict(&setup, &workspace, true);
    assert!(kept(&verdict).contains("no checkout holds a commit"));
}

#[test]
fn a_branch_that_origin_main_holds_is_merged() {
    let setup = setup();
    let workspace = created(&setup);
    let checkout = &workspace.checkouts[0].path;
    commit(checkout, "notes.md", "draft");
    merge_by_fast_forward(checkout);
    assert_eq!(verdict(&setup, &workspace, false), Verdict::Merged);
}

#[test]
fn a_branch_that_only_moved_along_origin_main_is_kept() {
    let setup = setup();
    let workspace = created(&setup);
    let checkout = &workspace.checkouts[0].path;
    // Someone else advances main, and the branch follows it with no
    // commit of its own, as `git pull` does.
    git(checkout, &["checkout", "--quiet", "-b", "other"]);
    commit(checkout, "other.md", "other");
    git(checkout, &["push", "--quiet", "origin", "HEAD:main"]);
    git(checkout, &["checkout", "--quiet", "reading-list"]);
    git(checkout, &["merge", "--quiet", "--ff-only", "other"]);
    let refused = verdict(&setup, &workspace, false);
    assert!(kept(&refused).contains("not merged"), "{refused:?}");
}

#[test]
fn a_branch_that_origin_main_lacks_is_kept_unless_a_pull_request_merged() {
    let setup = setup();
    let workspace = created(&setup);
    let checkout = &workspace.checkouts[0].path;
    commit(checkout, "notes.md", "draft");
    let refused = verdict(&setup, &workspace, false);
    assert!(kept(&refused).contains("reading-list"));
    assert_eq!(verdict(&setup, &workspace, true), Verdict::Merged);
}

/// A source of pull requests that answers yes only for a pull request of
/// the branch `reading-list` merged into the branch `main`, as `gh pr list
/// --base main` names it.
struct MergedIntoMain;

impl PullRequests for MergedIntoMain {
    fn merged(&self, _: &Path, branch: &str, base: &str, _: &str) -> bool {
        branch == "reading-list" && base == "main"
    }
}

#[test]
fn the_pull_request_source_is_asked_about_the_base_branch_by_its_name() {
    let setup = setup();
    let workspace = created(&setup);
    let checkout = &workspace.checkouts[0].path;
    commit(checkout, "notes.md", "draft");
    let verdict = setup
        .workspaces
        .merge_verdict(&workspace, &MergedIntoMain)
        .unwrap();
    assert_eq!(verdict, Verdict::Merged);
}

#[test]
fn uncommitted_changes_keep_a_merged_workspace() {
    let setup = setup();
    let workspace = created(&setup);
    let checkout = &workspace.checkouts[0].path;
    commit(checkout, "notes.md", "draft");
    merge_by_fast_forward(checkout);
    write(checkout.join("scratch.md"), "more");
    let refused = verdict(&setup, &workspace, true);
    assert!(kept(&refused).contains("uncommitted changes"));
}

#[test]
fn a_stash_keeps_a_merged_workspace() {
    let setup = setup();
    let workspace = created(&setup);
    let checkout = &workspace.checkouts[0].path;
    commit(checkout, "notes.md", "draft");
    merge_by_fast_forward(checkout);
    write(checkout.join("notes.md"), "edited");
    git(checkout, &["stash", "--quiet"]);
    let refused = verdict(&setup, &workspace, true);
    assert!(kept(&refused).contains("stashes"));
}

#[test]
fn a_detached_head_keeps_a_workspace() {
    let setup = setup();
    let workspace = created(&setup);
    let checkout = &workspace.checkouts[0].path;
    commit(checkout, "notes.md", "draft");
    git(checkout, &["checkout", "--quiet", "--detach"]);
    let refused = verdict(&setup, &workspace, true);
    assert!(kept(&refused).contains("no branch"));
}

#[test]
fn an_unreachable_origin_keeps_a_workspace() {
    let setup = setup();
    let workspace = created(&setup);
    let checkout = &workspace.checkouts[0].path;
    commit(checkout, "notes.md", "draft");
    git(
        checkout,
        &["remote", "set-url", "origin", "/nonexistent/origin.git"],
    );
    let refused = verdict(&setup, &workspace, true);
    assert!(kept(&refused).contains("could not fetch"));
}

#[test]
fn a_board_mount_update_that_cannot_start_says_so() {
    let mount = BoardMount::new(PathBuf::from("/nonexistent-board"), "/repos/app".to_owned());
    let message = update_board_mount(&mount).unwrap_err();
    assert!(message.contains("could not be started"), "{message}");
}
