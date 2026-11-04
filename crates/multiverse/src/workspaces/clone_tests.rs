//! The tests of workspaces whose checkouts are clones of a mirror.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;

use crate::error::Error;
use crate::git::Git;
use crate::name::WorkspaceName;
use crate::record::{Workspace, WorkspaceKind};
use crate::store::{self, Store};
use crate::testing::{Fixture, advance_origin, fixture, git, write};
use crate::workspaces::{
    Allow, BranchFate, Fate, Mode, NewOutcome, Removal, RemovalOutcome, RemovalPlan, Workspaces,
};

const NOTHING: Allow = Allow {
    discard_changes: false,
    delete_unmerged: false,
};

const FORCE: Allow = Allow {
    discard_changes: true,
    delete_unmerged: false,
};

const UNMERGED: Allow = Allow {
    discard_changes: false,
    delete_unmerged: true,
};

/// A fixture repository with workspaces of clones, whose root, store and
/// mirrors live in a temporary directory of their own.
struct Setup {
    fixture: Fixture,
    root: PathBuf,
    mirrors: PathBuf,
    _state: TempDir,
    workspaces: Workspaces<Git>,
}

fn setup() -> Setup {
    let fixture = fixture();
    let state = tempfile::tempdir().unwrap();
    let base = state.path().canonicalize().unwrap();
    let mirrors = base.join("mirrors");
    let workspaces =
        Workspaces::new(Store::new(base.join("state")), Git).with_mirrors(mirrors.clone());
    Setup {
        fixture,
        root: base.join("mxv"),
        mirrors,
        _state: state,
        workspaces,
    }
}

fn name(text: &str) -> WorkspaceName {
    text.parse().unwrap()
}

fn created(setup: &Setup, text: &str) -> Workspace {
    match setup
        .workspaces
        .create(&setup.root, &name(text), &setup.fixture.clone, Mode::Change)
        .unwrap()
    {
        NewOutcome::Created(workspace) => workspace,
        other => panic!("{other:?}"),
    }
}

fn plan(setup: &Setup, text: &str, allow: Allow) -> RemovalPlan {
    setup.workspaces.plan_removal(&name(text), allow).unwrap()
}

fn removed(setup: &Setup, text: &str, allow: Allow) -> Removal {
    let RemovalPlan::Ready(removal) = plan(setup, text, allow) else {
        panic!("the removal is refused");
    };
    match setup.workspaces.remove(removal).unwrap() {
        RemovalOutcome::Done(removal) => removal,
        RemovalOutcome::Changed(plan) => panic!("{plan:?}"),
    }
}

/// Returns every reference and worktree of the person's repository, which
/// a workspace of clones must never change.
fn person_state(setup: &Setup) -> String {
    let refs = git(&setup.fixture.clone, &["for-each-ref"]);
    let worktrees = git(&setup.fixture.clone, &["worktree", "list", "--porcelain"]);
    format!("{refs}\n{worktrees}")
}

fn commit(checkout: &Path, message: &str) {
    git(
        checkout,
        &["commit", "--quiet", "--allow-empty", "-m", message],
    );
}

#[test]
fn a_new_workspace_is_a_clone_with_a_git_directory_of_its_own() {
    let setup = setup();
    let latest = advance_origin(&setup.fixture);
    let before = person_state(&setup);
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0];
    assert_eq!(workspace.kind, WorkspaceKind::Clone);
    assert!(checkout.path.join(".git").is_dir());
    assert_eq!(checkout.commit.as_deref(), Some(latest.as_str()));
    assert_eq!(
        git(&checkout.path, &["branch", "--show-current"]).trim(),
        "reading-list"
    );
    let origin = git(&checkout.path, &["remote", "get-url", "origin"]);
    assert_eq!(Path::new(origin.trim()), setup.fixture.origin);
    let mirror = checkout.mirror.clone().unwrap();
    assert!(mirror.starts_with(&setup.mirrors));
    let alternates =
        fs::read_to_string(checkout.path.join(".git/objects/info/alternates")).unwrap();
    assert_eq!(Path::new(alternates.trim()), mirror.join("objects"));
    assert_eq!(person_state(&setup), before);
    let found = setup.workspaces.find(&name("reading-list")).unwrap();
    assert_eq!(found, Some(workspace));
}

#[test]
fn a_new_clone_has_no_hooks_and_none_of_the_persons_repository_config() {
    let setup = setup();
    git(
        &setup.fixture.clone,
        &["config", "core.hooksPath", "planted"],
    );
    let hooks = setup.fixture.clone.join(".git/hooks");
    write(hooks.join("pre-commit"), "#!/bin/sh\nexit 1\n");
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0].path;
    let hooks = checkout.join(".git/hooks");
    assert!(
        !hooks.exists() || fs::read_dir(&hooks).unwrap().next().is_none(),
        "the clone has hooks"
    );
    let config = git(checkout, &["config", "--local", "--list"]);
    assert!(!config.contains("hookspath"), "{config}");
    commit(checkout, "Works without the planted hook");
}

#[test]
fn a_mirror_never_collects_or_prunes_objects() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let mirror = workspace.checkouts[0].mirror.clone().unwrap();
    assert_eq!(git(&mirror, &["config", "gc.auto"]).trim(), "0");
    assert_eq!(git(&mirror, &["config", "gc.pruneExpire"]).trim(), "never");
    assert_eq!(
        git(&mirror, &["rev-parse", "--is-bare-repository"]).trim(),
        "true"
    );
}

#[test]
fn two_workspaces_of_one_origin_share_its_mirror() {
    let setup = setup();
    let first = created(&setup, "reading-list");
    advance_origin(&setup.fixture);
    let second = created(&setup, "inbox");
    assert_eq!(first.checkouts[0].mirror, second.checkouts[0].mirror);
    assert_ne!(first.checkouts[0].commit, second.checkouts[0].commit);
}

#[test]
fn a_dry_run_of_a_clone_creates_nothing_and_fetches_nothing() {
    let setup = setup();
    let outcome = setup
        .workspaces
        .create(
            &setup.root,
            &name("reading-list"),
            &setup.fixture.clone,
            Mode::DryRun,
        )
        .unwrap();
    let NewOutcome::Planned(workspace) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(workspace.kind, WorkspaceKind::Clone);
    assert!(!workspace.path.exists());
    assert!(!setup.mirrors.exists());
}

#[test]
fn a_failed_mirror_fetch_fails_with_its_origin_and_leaves_no_record_or_directory() {
    let setup = setup();
    fs::remove_dir_all(&setup.fixture.origin).unwrap();
    let error = setup
        .workspaces
        .create(
            &setup.root,
            &name("reading-list"),
            &setup.fixture.clone,
            Mode::Change,
        )
        .unwrap_err();
    assert!(
        matches!(error, Error::MirrorFetchFailed { .. }),
        "{error:?}"
    );
    assert!(setup.workspaces.list().unwrap().workspaces.is_empty());
    assert!(!setup.root.join("reading-list").exists());
}

#[test]
fn a_mirror_that_another_command_holds_fails_as_busy_and_leaves_nothing() {
    let mut setup = setup();
    setup.workspaces.mirror_wait = Duration::from_millis(50);
    let first = created(&setup, "reading-list");
    let mirror = first.checkouts[0].mirror.clone().unwrap();
    let mut lock = mirror.into_os_string();
    lock.push(".lock");
    let _held = store::lock(Path::new(&lock), Duration::ZERO).unwrap();
    let error = setup
        .workspaces
        .create(
            &setup.root,
            &name("inbox"),
            &setup.fixture.clone,
            Mode::Change,
        )
        .unwrap_err();
    assert!(matches!(error, Error::MirrorBusy { .. }), "{error:?}");
    assert!(!setup.root.join("inbox").exists());
}

#[test]
fn removing_a_pushed_clone_deletes_it_keeps_the_mirror_and_leaves_the_persons_repository_alone() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.clone();
    commit(&checkout, "Pushed work");
    git(&checkout, &["push", "--quiet", "origin", "reading-list"]);
    let before = person_state(&setup);
    let removal = removed(&setup, "reading-list", NOTHING);
    assert_eq!(removal.checkouts[0].directory, Fate::Removed);
    assert_eq!(removal.checkouts[0].branch, BranchFate::Deleted);
    assert!(!workspace.path.exists());
    assert!(workspace.checkouts[0].mirror.clone().unwrap().is_dir());
    assert_eq!(person_state(&setup), before);
    assert_eq!(setup.workspaces.find(&name("reading-list")).unwrap(), None);
}

#[test]
fn a_clone_with_unpushed_commits_is_kept_unless_unmerged_commits_may_be_deleted() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.clone();
    commit(&checkout, "One");
    commit(&checkout, "Two");
    let refused = plan(&setup, "reading-list", FORCE);
    assert!(
        matches!(refused, RemovalPlan::Unpushed { commits: 2, .. }),
        "{refused:?}"
    );
    let removal = removed(&setup, "reading-list", UNMERGED);
    assert_eq!(
        removal.checkouts[0].branch,
        BranchFate::DeletedUnmerged { commits: Some(2) }
    );
    assert!(!checkout.exists());
}

#[test]
fn a_commit_on_a_branch_that_origin_deleted_counts_as_unpushed() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.clone();
    commit(&checkout, "Squashed upstream");
    git(&checkout, &["push", "--quiet", "origin", "reading-list"]);
    git(&setup.fixture.origin, &["branch", "-D", "reading-list"]);
    let refused = plan(&setup, "reading-list", NOTHING);
    assert!(
        matches!(refused, RemovalPlan::Unpushed { commits: 1, .. }),
        "{refused:?}"
    );
}

#[test]
fn a_clone_with_a_stash_is_kept_unless_changes_may_be_discarded() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.clone();
    write(checkout.join("notes.md"), "draft");
    git(
        &checkout,
        &["stash", "push", "--quiet", "--include-untracked"],
    );
    let refused = plan(&setup, "reading-list", NOTHING);
    assert!(
        matches!(refused, RemovalPlan::Stashed { stashes: 1, .. }),
        "{refused:?}"
    );
    removed(&setup, "reading-list", FORCE);
    assert!(!checkout.exists());
}

#[test]
fn a_clone_with_an_untracked_file_is_kept_unless_forced() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    write(workspace.checkouts[0].path.join("notes.md"), "draft");
    let refused = plan(&setup, "reading-list", NOTHING);
    assert!(matches!(refused, RemovalPlan::Dirty { .. }), "{refused:?}");
    removed(&setup, "reading-list", FORCE);
    assert!(!workspace.path.exists());
}

#[test]
fn a_clone_whose_git_directory_is_a_link_or_a_file_is_not_removed() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let dot_git = workspace.checkouts[0].path.join(".git");
    fs::remove_dir_all(&dot_git).unwrap();
    write(&dot_git, "gitdir: /elsewhere\n");
    let refused = plan(&setup, "reading-list", FORCE);
    assert!(
        matches!(refused, RemovalPlan::Unowned { .. }),
        "{refused:?}"
    );
    assert!(dot_git.exists());
}

#[test]
fn a_clone_whose_directory_is_gone_is_removed_with_its_record() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    fs::remove_dir_all(&workspace.checkouts[0].path).unwrap();
    let removal = removed(&setup, "reading-list", NOTHING);
    assert_eq!(removal.checkouts[0].directory, Fate::Missing);
    assert_eq!(setup.workspaces.find(&name("reading-list")).unwrap(), None);
}

#[test]
fn a_workspace_of_worktrees_keeps_working_after_workspaces_gain_mirrors() {
    let setup = setup();
    let worktrees = Workspaces::new(setup.workspaces.store.clone(), Git);
    let legacy = match worktrees
        .create(
            &setup.root,
            &name("inbox-ingest"),
            &setup.fixture.clone,
            Mode::Change,
        )
        .unwrap()
    {
        NewOutcome::Created(workspace) => workspace,
        other => panic!("{other:?}"),
    };
    assert_eq!(legacy.kind, WorkspaceKind::Worktree);
    assert_eq!(legacy.version, 1);
    let found = setup
        .workspaces
        .find(&name("inbox-ingest"))
        .unwrap()
        .unwrap();
    assert_eq!(found.kind, WorkspaceKind::Worktree);
    let removal = removed(&setup, "inbox-ingest", NOTHING);
    assert_eq!(removal.checkouts[0].branch, BranchFate::Deleted);
    assert!(git(&setup.fixture.clone, &["branch", "--list", "inbox-ingest"]).is_empty());
    assert!(!legacy.path.exists());
}

#[test]
fn a_new_clone_ignores_what_the_persons_repository_excludes_outside_gitignore() {
    let setup = setup();
    write(setup.fixture.clone.join(".git/info/exclude"), ".env\n");
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0].path;
    write(checkout.join(".env"), "TOKEN=x");
    let status = git(checkout, &["status", "--porcelain", "--ignored"]);
    assert_eq!(status, "!! .env\n");
}
