//! The tests of workspaces whose checkouts are clones of a mirror.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;

use crate::error::Error;
use crate::git::Git;
use crate::name::WorkspaceName;
use crate::record::{Workspace, WorkspaceKind, WorkspaceState};
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
fn an_unpushed_refusal_names_when_the_mirror_last_fetched_origin() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    commit(&workspace.checkouts[0].path, "One");
    let before = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    let refused = plan(&setup, "reading-list", FORCE);
    let RemovalPlan::Unpushed { fetched, .. } = refused else {
        panic!("{refused:?}");
    };
    assert!(fetched.is_some_and(|time| time > before), "{fetched:?}");
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
    // A removal fetches nothing, so it sees the deletion once the person
    // fetches.
    let unchanged = plan(&setup, "reading-list", NOTHING);
    assert!(matches!(unchanged, RemovalPlan::Ready(_)), "{unchanged:?}");
    git(&checkout, &["fetch", "--quiet", "--prune", "origin"]);
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
    assert_eq!(legacy.version, crate::record::ID_RECORD_VERSION);
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

#[test]
fn a_commit_that_only_the_reflog_holds_counts_as_unpushed() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.clone();
    git(&checkout, &["checkout", "--quiet", "--detach"]);
    commit(&checkout, "Left behind");
    git(&checkout, &["checkout", "--quiet", "reading-list"]);
    let refused = plan(&setup, "reading-list", NOTHING);
    assert!(
        matches!(refused, RemovalPlan::Unpushed { commits: 1, .. }),
        "{refused:?}"
    );
}

/// Commits a change to `file` in `checkout`, so the commit has a patch.
fn change(checkout: &Path, file: &str, text: &str, message: &str) {
    write(checkout.join(file), text);
    git(checkout, &["add", file]);
    git(checkout, &["commit", "--quiet", "-m", message]);
}

#[test]
fn a_detached_commit_cherry_picked_onto_the_pushed_branch_is_not_unpushed() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.clone();
    git(&checkout, &["checkout", "--quiet", "--detach"]);
    change(&checkout, "notes.md", "draft", "Drafted detached");
    let detached = git(&checkout, &["rev-parse", "HEAD"]);
    git(&checkout, &["checkout", "--quiet", "reading-list"]);
    git(&checkout, &["cherry-pick", detached.trim()]);
    git(&checkout, &["push", "--quiet", "origin", "reading-list"]);
    removed(&setup, "reading-list", NOTHING);
    assert!(!checkout.exists());
}

#[test]
fn a_branch_rebased_and_pushed_is_removed_without_deleting_unmerged_commits() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.clone();
    change(&checkout, "notes.md", "draft", "Drafted");
    git(&checkout, &["push", "--quiet", "origin", "reading-list"]);
    advance_origin(&setup.fixture);
    git(&checkout, &["fetch", "--quiet", "origin"]);
    git(&checkout, &["rebase", "--quiet", "origin/main"]);
    git(
        &checkout,
        &["push", "--quiet", "--force", "origin", "reading-list"],
    );
    removed(&setup, "reading-list", NOTHING);
    assert!(!checkout.exists());
}

#[test]
fn a_commit_amended_and_pushed_is_removed_without_deleting_unmerged_commits() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.clone();
    change(&checkout, "notes.md", "draft", "Drafted");
    git(&checkout, &["push", "--quiet", "origin", "reading-list"]);
    write(checkout.join("notes.md"), "final");
    git(
        &checkout,
        &["commit", "--quiet", "--all", "--amend", "--no-edit"],
    );
    git(
        &checkout,
        &["push", "--quiet", "--force", "origin", "reading-list"],
    );
    removed(&setup, "reading-list", NOTHING);
    assert!(!checkout.exists());
}

#[test]
fn a_commit_that_a_reset_on_the_branch_undid_is_removed_without_deleting_unmerged_commits() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.clone();
    change(&checkout, "notes.md", "draft", "Undone");
    git(&checkout, &["reset", "--quiet", "--hard", "HEAD~1"]);
    removed(&setup, "reading-list", NOTHING);
    assert!(!checkout.exists());
}

#[test]
fn a_tag_that_origin_holds_on_a_commit_no_branch_holds_is_not_unpushed() {
    let setup = setup();
    let person = &setup.fixture.clone;
    git(person, &["checkout", "--quiet", "--detach"]);
    commit(person, "Released");
    git(person, &["tag", "--annotate", "--message", "Release", "v1"]);
    git(person, &["push", "--quiet", "origin", "v1"]);
    git(person, &["checkout", "--quiet", "main"]);
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.clone();
    assert!(!git(&checkout, &["tag", "--list", "v1"]).trim().is_empty());
    // The mirror holds origin's tags as of its last fetch, and a removal
    // counts against them without contacting origin.
    removed(&setup, "reading-list", NOTHING);
    assert!(!checkout.exists());
}

#[test]
fn a_local_tag_that_origin_does_not_hold_counts_as_unpushed() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.clone();
    git(&checkout, &["checkout", "--quiet", "--detach"]);
    commit(&checkout, "Tagged");
    git(&checkout, &["tag", "local"]);
    git(&checkout, &["checkout", "--quiet", "reading-list"]);
    git(&checkout, &["reflog", "expire", "--expire=now", "--all"]);
    let refused = plan(&setup, "reading-list", NOTHING);
    assert!(
        matches!(refused, RemovalPlan::Unpushed { commits: 1, .. }),
        "{refused:?}"
    );
}

#[test]
fn a_note_that_origin_does_not_hold_counts_as_unpushed() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.clone();
    git(
        &checkout,
        &["notes", "add", "--message", "Reviewed", "HEAD"],
    );
    let refused = plan(&setup, "reading-list", NOTHING);
    assert!(
        matches!(refused, RemovalPlan::Unpushed { commits: 1, .. }),
        "{refused:?}"
    );
    git(&checkout, &["push", "--quiet", "origin", "refs/notes/*"]);
    // The mirror holds no notes, so pushing one does not clear the count.
    let refused = plan(&setup, "reading-list", NOTHING);
    assert!(
        matches!(refused, RemovalPlan::Unpushed { commits: 1, .. }),
        "{refused:?}"
    );
    git(&checkout, &["update-ref", "-d", "refs/notes/commits"]);
    removed(&setup, "reading-list", NOTHING);
}

#[test]
fn a_commit_in_a_submodule_that_its_origin_does_not_hold_counts_as_unpushed() {
    let setup = setup();
    let root = setup.fixture.dir.path().canonicalize().unwrap();
    git(&root, &["init", "--quiet", "--bare", "library.git"]);
    let library = root.join("library");
    git(&root, &["clone", "--quiet", "library.git", "library"]);
    commit(&library, "Library");
    git(&library, &["push", "--quiet", "origin", "HEAD"]);
    let person = &setup.fixture.clone;
    let url = root.join("library.git");
    git(
        person,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "--quiet",
            "add",
            url.to_str().unwrap(),
            "library",
        ],
    );
    git(person, &["commit", "--quiet", "-m", "Add the library"]);
    git(person, &["push", "--quiet", "origin", "main"]);
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.clone();
    git(
        &checkout,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "--quiet",
            "update",
            "--init",
        ],
    );
    removed(&setup, "reading-list", NOTHING);
    let workspace = created(&setup, "second");
    let checkout = workspace.checkouts[0].path.clone();
    git(
        &checkout,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "--quiet",
            "update",
            "--init",
        ],
    );
    commit(&checkout.join("library"), "Only here");
    let refused = plan(&setup, "second", NOTHING);
    assert!(
        matches!(refused, RemovalPlan::Unpushed { commits: 1, .. }),
        "{refused:?}"
    );
}

#[test]
fn a_stash_added_after_the_plan_stops_a_forced_removal() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.clone();
    write(checkout.join("notes.md"), "first");
    git(
        &checkout,
        &["stash", "push", "--quiet", "--include-untracked"],
    );
    let RemovalPlan::Ready(confirmed) = plan(&setup, "reading-list", FORCE) else {
        panic!("the forced removal is refused");
    };
    assert_eq!(confirmed.checkouts[0].stashes.len(), 1);
    git(&checkout, &["stash", "drop", "--quiet"]);
    write(checkout.join("notes.md"), "second");
    git(
        &checkout,
        &["stash", "push", "--quiet", "--include-untracked"],
    );
    let outcome = setup.workspaces.remove(confirmed).unwrap();
    assert!(matches!(outcome, RemovalOutcome::Changed(_)), "{outcome:?}");
    assert!(checkout.exists());
}

#[test]
fn a_commit_made_after_the_plan_stops_a_removal_of_unmerged_commits() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.clone();
    commit(&checkout, "Seen");
    let RemovalPlan::Ready(confirmed) = plan(&setup, "reading-list", UNMERGED) else {
        panic!("the removal is refused");
    };
    git(&checkout, &["reset", "--quiet", "--hard", "HEAD~1"]);
    git(&checkout, &["reflog", "expire", "--expire=now", "--all"]);
    commit(&checkout, "Unseen");
    let outcome = setup.workspaces.remove(confirmed).unwrap();
    assert!(matches!(outcome, RemovalOutcome::Changed(_)), "{outcome:?}");
}

#[test]
fn a_removal_plans_without_contacting_an_origin_it_cannot_reach() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.clone();
    git(
        &checkout,
        &["remote", "set-url", "origin", "/nonexistent/origin.git"],
    );
    let plan = plan(&setup, "reading-list", NOTHING);
    assert!(matches!(plan, RemovalPlan::Ready(_)), "{plan:?}");
}

#[test]
fn a_fetch_failure_never_shows_the_credentials_of_the_origin_url() {
    let setup = setup();
    git(
        &setup.fixture.clone,
        &[
            "remote",
            "set-url",
            "origin",
            "http://someone:s3cr3t-token@127.0.0.1:9/app.git",
        ],
    );
    let error = setup
        .workspaces
        .create(
            &setup.root,
            &name("reading-list"),
            &setup.fixture.clone,
            Mode::Change,
        )
        .unwrap_err();
    let Error::MirrorFetchFailed { ref origin, .. } = error else {
        panic!("{error:?}");
    };
    assert_eq!(origin, "http://127.0.0.1:9/app.git");
    assert!(!error.to_string().contains("s3cr3t"), "{error}");
    assert!(!format!("{error:?}").contains("s3cr3t"), "{error:?}");
}

#[test]
fn a_held_mirror_lock_is_announced_once_before_waiting() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NOTICES: AtomicUsize = AtomicUsize::new(0);
    fn notice(_mirror: &Path, _wait: Duration) {
        NOTICES.fetch_add(1, Ordering::SeqCst);
    }
    let mut setup = setup();
    setup.workspaces.mirror_wait = Duration::from_millis(50);
    let first = created(&setup, "reading-list");
    assert_eq!(NOTICES.load(Ordering::SeqCst), 0);
    let mirror = first.checkouts[0].mirror.clone().unwrap();
    let mut lock = mirror.into_os_string();
    lock.push(".lock");
    let _held = store::lock(Path::new(&lock), Duration::ZERO).unwrap();
    let workspaces = setup.workspaces.with_mirror_notice(notice);
    let error = workspaces
        .create(
            &setup.root,
            &name("inbox"),
            &setup.fixture.clone,
            Mode::Change,
        )
        .unwrap_err();
    assert!(matches!(error, Error::MirrorBusy { .. }), "{error:?}");
    assert_eq!(NOTICES.load(Ordering::SeqCst), 1);
}

fn created_with_manifest(setup: &Setup, text: &str, manifest: &str) -> Workspace {
    match setup
        .workspaces
        .create_with_manifest(
            &setup.root,
            &name(text),
            &setup.fixture.clone,
            Mode::Change,
            Some(manifest),
        )
        .unwrap()
    {
        NewOutcome::Created(workspace) => workspace,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_workspace_created_with_a_manifest_holds_it_above_the_checkout() {
    let setup = setup();
    let workspace = created_with_manifest(&setup, "reading-list", "ports = [\"web\"]\n");
    assert_eq!(workspace.manifest_file(), workspace.path.join("mxv.toml"));
    assert_eq!(
        fs::read_to_string(workspace.manifest_file()).unwrap(),
        "ports = [\"web\"]\n"
    );
    assert_eq!(workspace.manifest().unwrap().unwrap().ports, ["web"]);
}

#[test]
fn a_failed_creation_deletes_the_manifest_it_wrote_with_the_rest() {
    let setup = setup();
    fs::remove_dir_all(&setup.fixture.origin).unwrap();
    let error = setup
        .workspaces
        .create_with_manifest(
            &setup.root,
            &name("reading-list"),
            &setup.fixture.clone,
            Mode::Change,
            Some("ports = [\"web\"]\n"),
        )
        .unwrap_err();
    assert!(
        matches!(error, Error::MirrorFetchFailed { .. }),
        "{error:?}"
    );
    assert!(!setup.root.join("reading-list").exists());
}

#[test]
fn removing_a_workspace_deletes_its_manifest_and_its_directory() {
    let setup = setup();
    let workspace = created_with_manifest(&setup, "reading-list", "ports = [\"web\"]\n");
    let RemovalPlan::Ready(planned) = plan(&setup, "reading-list", NOTHING) else {
        panic!("the removal is refused");
    };
    assert_eq!(planned.workspace_manifest, Some(workspace.manifest_file()));
    let removal = removed(&setup, "reading-list", NOTHING);
    assert_eq!(removal.directory, Fate::Removed);
    assert_eq!(removal.workspace_manifest, Some(workspace.manifest_file()));
    assert!(!workspace.path.exists());
}

#[test]
fn removing_a_workspace_without_a_manifest_names_none() {
    let setup = setup();
    created(&setup, "reading-list");
    let removal = removed(&setup, "reading-list", NOTHING);
    assert_eq!(removal.workspace_manifest, None);
}

#[test]
fn a_workspace_manifest_that_links_to_the_checkouts_makes_its_paths_wait_for_a_confirmation() {
    let setup = setup();
    // The repository commits its own mxv.toml, which the link points at.
    write(setup.fixture.clone.join(".git/info/exclude"), "data/\n");
    write(
        setup.fixture.clone.join("mxv.toml"),
        "disposable = [\"data/\"]\n",
    );
    git(&setup.fixture.clone, &["add", "mxv.toml"]);
    commit(&setup.fixture.clone, "Declare data/ disposable");
    git(
        &setup.fixture.clone,
        &["push", "--quiet", "origin", "HEAD:main"],
    );
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0].path;
    std::os::unix::fs::symlink(checkout.join("mxv.toml"), workspace.manifest_file()).unwrap();
    fs::create_dir_all(checkout.join("data")).unwrap();
    write(checkout.join("data/file"), "");
    let planned = plan(&setup, "reading-list", NOTHING);
    let RemovalPlan::Ready(removal) = planned else {
        panic!("the removal is refused: {planned:?}");
    };
    assert_eq!(removal.manifest, ["app: data/"], "{removal:?}");
    assert!(
        removal
            .warnings
            .iter()
            .any(|warning| warning.contains("symbolic link")),
        "{removal:?}"
    );
}

#[test]
fn a_path_that_the_workspace_manifest_makes_disposable_is_deleted_without_asking() {
    let setup = setup();
    write(
        setup.fixture.clone.join(".git/info/exclude"),
        "data/\ncache/\nmxv.toml\n",
    );
    let workspace = created_with_manifest(&setup, "reading-list", "disposable = [\"data/\"]\n");
    let checkout = &workspace.checkouts[0].path;
    write(checkout.join("mxv.toml"), "disposable = [\"cache/\"]\n");
    for dir in ["data", "cache"] {
        fs::create_dir_all(checkout.join(dir)).unwrap();
        write(checkout.join(dir).join("file"), "");
    }
    let RemovalPlan::IgnoredFiles { paths, .. } = plan(&setup, "reading-list", NOTHING) else {
        panic!("the checkout's own glob is not used beside the workspace manifest");
    };
    assert!(
        paths.iter().any(|path| path.contains("cache/")),
        "{paths:?}"
    );
    fs::remove_dir_all(checkout.join("cache")).unwrap();
    fs::remove_file(checkout.join("mxv.toml")).unwrap();
    let RemovalPlan::Ready(removal) = plan(&setup, "reading-list", NOTHING) else {
        panic!("the removal is refused");
    };
    assert!(removal.manifest.is_empty(), "{removal:?}");
    assert_eq!(removal.disposable, ["app: data/"]);
}

#[test]
fn a_manifest_source_inside_a_repository_or_under_the_root_is_refused() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let elsewhere = tempfile::tempdir().unwrap();
    let outside = elsewhere.path().join("app.toml");
    write(&outside, "ports = []\n");
    setup
        .workspaces
        .check_manifest_source(&outside, &setup.root)
        .unwrap();
    let link = elsewhere.path().join("link.toml");
    write(setup.fixture.clone.join("mxv.toml"), "ports = []\n");
    std::os::unix::fs::symlink(setup.fixture.clone.join("mxv.toml"), &link).unwrap();
    write(workspace.checkouts[0].path.join("mxv.toml"), "ports = []\n");
    write(workspace.manifest_file(), "ports = []\n");
    for (path, place) in [
        (
            setup.fixture.clone.join("mxv.toml"),
            "inside the git work tree",
        ),
        (link, "inside the git work tree"),
        (
            workspace.checkouts[0].path.join("mxv.toml"),
            "under the root of the workspaces",
        ),
        (
            workspace.manifest_file(),
            "under the root of the workspaces",
        ),
    ] {
        let error = setup
            .workspaces
            .check_manifest_source(&path, &setup.root)
            .unwrap_err();
        assert!(
            matches!(&error, Error::ManifestUntrusted { reason, .. } if reason.contains(place)),
            "{}: {error:?}",
            path.display()
        );
    }
}

#[test]
fn a_clones_disposable_directories_are_planned_and_deleted_with_it() {
    let setup = setup();
    write(setup.fixture.clone.join(".git/info/exclude"), "target/\n");
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0].path;
    fs::create_dir_all(checkout.join("target/debug")).unwrap();
    write(checkout.join("target/debug/mxv"), "binary");
    let RemovalPlan::Ready(planned) = plan(&setup, "reading-list", NOTHING) else {
        panic!("the removal is refused");
    };
    assert_eq!(planned.checkouts[0].disposable, ["target/"]);
    let removal = removed(&setup, "reading-list", NOTHING);
    assert_eq!(removal.directory, Fate::Removed);
    assert!(!workspace.path.exists());
}

#[test]
fn a_clone_whose_deletion_was_interrupted_is_finished_by_a_second_removal() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0].path;
    // An interrupted removal moved the clone aside and deleted part of it,
    // its `.git` among them, before it stopped.
    let aside = workspace.path.join(".app.mxv-removing");
    #[expect(
        clippy::disallowed_methods,
        reason = "the test moves the clone aside as an interrupted removal does"
    )]
    let moved = fs::rename(checkout, &aside);
    moved.unwrap();
    fs::remove_dir_all(aside.join(".git")).unwrap();
    write(aside.join("left.txt"), "left behind");
    let removal = removed(&setup, "reading-list", NOTHING);
    assert_eq!(removal.checkouts[0].directory, Fate::Missing);
    assert_eq!(removal.directory, Fate::Removed);
    assert!(!workspace.path.exists());
    assert!(
        setup
            .workspaces
            .find(&name("reading-list"))
            .unwrap()
            .is_none()
    );
}

/// Stops the run at the step named `marker`, as a process killed there
/// would: it panics, so no undo runs.
fn stop_at_marker(step: &str) -> Result<(), Error> {
    assert_ne!(step, "marker", "the test stops the run at the marker");
    Ok(())
}

#[test]
fn a_clone_creation_stopped_before_its_last_write_leaves_the_record_creating() {
    let mut setup = setup();
    setup.workspaces.checkpoint = stop_at_marker;
    let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        setup.workspaces.create(
            &setup.root,
            &name("reading-list"),
            &setup.fixture.clone,
            Mode::Change,
        )
    }));
    assert!(stopped.is_err(), "{stopped:?}");
    let read = setup
        .workspaces
        .find(&name("reading-list"))
        .unwrap()
        .unwrap();
    assert_eq!(read.state(), WorkspaceState::Creating);
}

#[test]
fn a_clone_creation_that_finishes_leaves_the_record_live() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    assert_eq!(workspace.state(), WorkspaceState::Live);
    let read = setup
        .workspaces
        .find(&name("reading-list"))
        .unwrap()
        .unwrap();
    assert_eq!(read.state(), WorkspaceState::Live);
}
