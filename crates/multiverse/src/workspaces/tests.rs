use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::backend::{Backend, Changes, Head, Listed, Repository};
use crate::disposable::PathGlob;
use crate::error::Error;
use crate::git::Git;
use crate::name::WorkspaceName;
use crate::record::Workspace;
use crate::store::Store;
use crate::testing::{Fixture, advance_origin, fixture, git, write};
use crate::workspaces::{
    Allow, BranchFate, Fate, Mode, NewOutcome, Removal, RemovalOutcome, RemovalPlan, Workspaces,
};

/// Allows neither discarding changes nor deleting unmerged commits.
const NOTHING: Allow = Allow {
    discard_changes: false,
    delete_unmerged: false,
};

/// Allows discarding changes, as `--force` does.
const FORCE: Allow = Allow {
    discard_changes: true,
    delete_unmerged: false,
};

/// Allows deleting unmerged commits, as `--delete-unmerged` does.
const UNMERGED: Allow = Allow {
    discard_changes: false,
    delete_unmerged: true,
};

/// A fixture repository with workspaces whose root and store live in a
/// temporary directory of their own.
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
    let workspaces = Workspaces::new(Store::new(base.join("state")), Git);
    Setup {
        fixture,
        root: base.join("mxv"),
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

fn ready(setup: &Setup, text: &str, allow: Allow) -> Removal {
    match setup.workspaces.plan_removal(&name(text), allow).unwrap() {
        RemovalPlan::Ready(removal) => removal,
        other => panic!("{other:?}"),
    }
}

/// Returns the removal that [`Workspaces::remove`] carried out, and fails
/// the test when it removed nothing because the workspace changed.
fn done(outcome: RemovalOutcome) -> Removal {
    match outcome {
        RemovalOutcome::Done(removal) => removal,
        RemovalOutcome::Changed(plan) => panic!("{plan:?}"),
    }
}

#[test]
fn a_new_workspace_checks_out_the_latest_origin_main_below_the_workspace_directory() {
    let setup = setup();
    let latest = advance_origin(&setup.fixture);
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0];
    assert_eq!(workspace.path, setup.root.as_path().join("reading-list"));
    assert_eq!(checkout.path, workspace.path.join("app"));
    assert_eq!(checkout.branch, "reading-list");
    assert_eq!(checkout.base, "refs/remotes/origin/main");
    assert_eq!(checkout.commit.as_deref(), Some(latest.as_str()));
    assert_eq!(git(&checkout.path, &["rev-parse", "HEAD"]).trim(), latest);
    assert_eq!(
        git(&checkout.path, &["branch", "--show-current"]).trim(),
        "reading-list"
    );
}

#[test]
fn a_new_workspace_writes_its_agents_file_outside_the_checkout_and_its_record() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let agents = fs::read_to_string(workspace.path.join("AGENTS.md")).unwrap();
    assert!(agents.contains("mxv remove reading-list"), "{agents}");
    assert!(agents.contains("on the branch `reading-list`"), "{agents}");
    assert!(git(&workspace.checkouts[0].path, &["status", "--porcelain"]).is_empty());
    let found = setup.workspaces.find(&name("reading-list")).unwrap();
    assert_eq!(found, Some(workspace));
}

#[test]
fn a_dry_run_of_new_creates_nothing_and_fetches_nothing() {
    let setup = setup();
    advance_origin(&setup.fixture);
    let before = git(&setup.fixture.clone, &["rev-parse", "origin/main"]);
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
    assert_eq!(workspace.checkouts[0].commit, None);
    assert!(!workspace.path.exists());
    assert!(setup.workspaces.list().unwrap().workspaces.is_empty());
    assert_eq!(
        git(&setup.fixture.clone, &["rev-parse", "origin/main"]),
        before
    );
}

#[test]
fn a_second_workspace_with_the_same_name_is_refused() {
    let setup = setup();
    created(&setup, "reading-list");
    let outcome = setup
        .workspaces
        .create(
            &setup.root,
            &name("reading-list"),
            &setup.fixture.clone,
            Mode::Change,
        )
        .unwrap();
    assert!(matches!(outcome, NewOutcome::NameTaken(_)), "{outcome:?}");
}

#[test]
fn a_directory_in_the_way_of_a_new_workspace_is_refused() {
    let setup = setup();
    fs::create_dir_all(setup.root.as_path().join("reading-list")).unwrap();
    let outcome = setup
        .workspaces
        .create(
            &setup.root,
            &name("reading-list"),
            &setup.fixture.clone,
            Mode::Change,
        )
        .unwrap();
    assert!(matches!(outcome, NewOutcome::PathTaken(_)), "{outcome:?}");
}

#[test]
fn a_branch_in_the_way_of_a_new_workspace_is_refused() {
    let setup = setup();
    git(&setup.fixture.clone, &["branch", "reading-list"]);
    let outcome = setup
        .workspaces
        .create(
            &setup.root,
            &name("reading-list"),
            &setup.fixture.clone,
            Mode::Change,
        )
        .unwrap();
    assert!(
        matches!(outcome, NewOutcome::BranchTaken { .. }),
        "{outcome:?}"
    );
}

#[test]
fn a_failed_fetch_leaves_no_record_and_no_directory() {
    let setup = setup();
    fs::remove_dir_all(&setup.fixture.origin).unwrap();
    let result = setup.workspaces.create(
        &setup.root,
        &name("reading-list"),
        &setup.fixture.clone,
        Mode::Change,
    );
    assert!(result.is_err());
    assert!(setup.workspaces.list().unwrap().workspaces.is_empty());
    assert!(!setup.root.as_path().join("reading-list").exists());
}

#[test]
fn removing_a_clean_workspace_removes_its_checkout_branch_directory_and_record() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let removal = done(
        setup
            .workspaces
            .remove(ready(&setup, "reading-list", NOTHING))
            .unwrap(),
    );
    assert_eq!(removal.checkouts[0].directory, Fate::Removed);
    assert_eq!(removal.checkouts[0].branch, BranchFate::Deleted);
    assert_eq!(removal.directory, Fate::Removed);
    assert!(setup.fixture.clone.exists());
    assert!(!workspace.path.exists());
    assert!(git(&setup.fixture.clone, &["branch", "--list", "reading-list"]).is_empty());
    assert_eq!(setup.workspaces.find(&name("reading-list")).unwrap(), None);
}

#[test]
fn a_workspace_with_an_untracked_file_is_not_removed_unless_forced() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    write(workspace.checkouts[0].path.join("notes.md"), "notes");
    let plan = setup
        .workspaces
        .plan_removal(&name("reading-list"), NOTHING)
        .unwrap();
    let RemovalPlan::Dirty { changes, .. } = plan else {
        panic!("{plan:?}");
    };
    assert_eq!(changes, ["app: ?? notes.md"]);
    let removal = done(
        setup
            .workspaces
            .remove(ready(&setup, "reading-list", FORCE))
            .unwrap(),
    );
    assert_eq!(removal.checkouts[0].directory, Fate::Removed);
    assert!(!workspace.path.exists());
}

/// Commits `count` empty commits in `checkout`.
fn commit(checkout: &std::path::Path, count: usize) {
    for _ in 0..count {
        git(
            checkout,
            &["commit", "--quiet", "--allow-empty", "-m", "Work"],
        );
    }
}

fn branch_exists(setup: &Setup) -> bool {
    !git(&setup.fixture.clone, &["branch", "--list", "reading-list"]).is_empty()
}

#[test]
fn force_discards_changes_but_keeps_an_unmerged_branch_and_counts_its_commits() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    commit(&workspace.checkouts[0].path, 2);
    write(workspace.checkouts[0].path.join("notes.md"), "notes");
    let removal = done(
        setup
            .workspaces
            .remove(ready(&setup, "reading-list", FORCE))
            .unwrap(),
    );
    assert_eq!(
        removal.checkouts[0].branch,
        BranchFate::KeptUnmerged { commits: Some(2) }
    );
    assert!(!workspace.path.exists());
    assert!(branch_exists(&setup));
}

#[test]
fn delete_unmerged_drops_an_unmerged_branch_and_counts_its_commits() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    commit(&workspace.checkouts[0].path, 3);
    let plan = ready(&setup, "reading-list", UNMERGED);
    assert_eq!(
        plan.checkouts[0].branch,
        BranchFate::DeletedUnmerged { commits: Some(3) }
    );
    done(setup.workspaces.remove(plan).unwrap());
    assert!(!branch_exists(&setup));
}

#[test]
fn delete_unmerged_does_not_discard_uncommitted_changes() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    write(workspace.checkouts[0].path.join("notes.md"), "notes");
    let plan = setup
        .workspaces
        .plan_removal(&name("reading-list"), UNMERGED)
        .unwrap();
    assert!(matches!(plan, RemovalPlan::Dirty { .. }), "{plan:?}");
}

/// Ignores `.env` and `target/` in the fixture's clone and its checkouts.
fn ignore(setup: &Setup) {
    write(
        setup.fixture.clone.join(".git/info/exclude"),
        ".env\ntarget/\nnode_modules/\n",
    );
}

#[test]
fn an_ignored_file_outside_the_disposable_set_stops_the_removal_unless_forced() {
    let setup = setup();
    ignore(&setup);
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0].path;
    write(checkout.join(".env"), "TOKEN=x");
    fs::create_dir_all(checkout.join("target/debug")).unwrap();
    write(checkout.join("target/debug/mxv"), "binary");
    let plan = setup
        .workspaces
        .plan_removal(&name("reading-list"), NOTHING)
        .unwrap();
    let RemovalPlan::IgnoredFiles { paths, .. } = plan else {
        panic!("{plan:?}");
    };
    assert_eq!(paths, ["app: .env"]);
    assert!(checkout.join(".env").exists());

    let removal = ready(&setup, "reading-list", FORCE);
    assert_eq!(removal.discarded, ["app: .env"]);
    assert_eq!(removal.disposable, ["app: target/"]);
    done(setup.workspaces.remove(removal).unwrap());
    assert!(!workspace.path.exists());
}

#[test]
fn ignored_build_directories_are_disposable_and_do_not_stop_the_removal() {
    let setup = setup();
    ignore(&setup);
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0].path;
    fs::create_dir_all(checkout.join("target/debug")).unwrap();
    fs::create_dir_all(checkout.join("web/node_modules/left-pad")).unwrap();
    write(checkout.join("web/node_modules/left-pad/index.js"), "");
    let removal = ready(&setup, "reading-list", NOTHING);
    assert_eq!(
        removal.disposable,
        ["app: target/", "app: web/node_modules/"]
    );
    assert!(removal.discarded.is_empty());
    done(setup.workspaces.remove(removal).unwrap());
    assert!(!workspace.path.exists());
}

#[test]
fn commits_on_a_detached_head_stop_the_removal_unless_unmerged_commits_may_be_deleted() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0].path;
    git(checkout, &["switch", "--quiet", "--detach"]);
    commit(checkout, 1);
    let plan = setup
        .workspaces
        .plan_removal(&name("reading-list"), FORCE)
        .unwrap();
    let RemovalPlan::StrayCommits { commits, .. } = plan else {
        panic!("{plan:?}");
    };
    assert_eq!(commits, 1);
    let removal = ready(&setup, "reading-list", UNMERGED);
    assert_eq!(removal.checkouts[0].stray_commits, 1);
    done(setup.workspaces.remove(removal).unwrap());
    assert!(!workspace.path.exists());
}

#[test]
fn a_symbolic_link_in_place_of_a_checkout_is_not_removed() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.clone();
    let moved = workspace.path.join("moved");
    git(
        &setup.fixture.clone,
        &[
            "worktree",
            "move",
            checkout.to_str().unwrap(),
            moved.to_str().unwrap(),
        ],
    );
    std::os::unix::fs::symlink(&moved, &checkout).unwrap();
    let plan = setup
        .workspaces
        .plan_removal(&name("reading-list"), FORCE)
        .unwrap();
    assert!(matches!(plan, RemovalPlan::Unowned { .. }), "{plan:?}");
}

#[test]
fn a_symbolic_link_in_place_of_a_workspace_directory_is_not_removed() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let elsewhere = setup.root.join("elsewhere");
    #[expect(
        clippy::disallowed_methods,
        reason = "the test moves a workspace directory as a person would"
    )]
    fs::rename(&workspace.path, &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &workspace.path).unwrap();
    let plan = setup
        .workspaces
        .plan_removal(&name("reading-list"), FORCE)
        .unwrap();
    assert!(matches!(plan, RemovalPlan::Unowned { .. }), "{plan:?}");
    assert!(elsewhere.join("AGENTS.md").exists());
}

#[test]
fn a_checkout_that_fails_partway_leaves_no_checkout_branch_directory_or_record() {
    let setup = setup();
    let hooks = setup.fixture.dir.path().join("hooks");
    fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("post-checkout");
    write(
        &hook,
        "#!/bin/sh\necho 'fatal: the hook refused' >&2\nexit 1\n",
    );
    fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    git(
        &setup.fixture.clone,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
    let result = setup.workspaces.create(
        &setup.root,
        &name("reading-list"),
        &setup.fixture.clone,
        Mode::Change,
    );
    assert!(
        matches!(result, Err(Error::CommandFailed { .. })),
        "{result:?}"
    );
    assert!(!branch_exists(&setup));
    let listed = git(&setup.fixture.clone, &["worktree", "list", "--porcelain"]);
    assert!(!listed.contains("reading-list"), "{listed}");
    assert!(!setup.root.join("reading-list").exists());
    assert!(setup.workspaces.list().unwrap().workspaces.is_empty());
}

#[test]
fn a_workspace_whose_checkout_is_gone_is_removed_and_the_repository_forgets_the_checkout() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    fs::remove_dir_all(&workspace.path).unwrap();
    let removal = done(
        setup
            .workspaces
            .remove(ready(&setup, "reading-list", NOTHING))
            .unwrap(),
    );
    assert_eq!(removal.checkouts[0].directory, Fate::Missing);
    assert_eq!(removal.directory, Fate::Missing);
    let listed = git(&setup.fixture.clone, &["worktree", "list", "--porcelain"]);
    assert!(!listed.contains("reading-list"), "{listed}");
    assert_eq!(setup.workspaces.find(&name("reading-list")).unwrap(), None);
}

#[test]
fn a_checkout_directory_that_its_repository_does_not_list_is_not_removed() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.clone();
    git(
        &setup.fixture.clone,
        &["worktree", "remove", checkout.to_str().unwrap()],
    );
    fs::create_dir_all(&checkout).unwrap();
    let plan = setup
        .workspaces
        .plan_removal(&name("reading-list"), FORCE)
        .unwrap();
    assert!(matches!(plan, RemovalPlan::Unowned { .. }), "{plan:?}");
    assert!(checkout.exists());
}

#[test]
fn a_workspace_directory_with_other_files_is_kept_after_its_checkout_is_removed() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    write(workspace.path.join("scratch.txt"), "mine");
    let removal = done(
        setup
            .workspaces
            .remove(ready(&setup, "reading-list", NOTHING))
            .unwrap(),
    );
    assert_eq!(removal.directory, Fate::Kept);
    assert!(workspace.path.join("scratch.txt").exists());
    assert!(!workspace.path.join("AGENTS.md").exists());
}

#[test]
fn removing_a_workspace_that_does_not_exist_finds_nothing() {
    let setup = setup();
    let plan = setup
        .workspaces
        .plan_removal(&name("nothing"), NOTHING)
        .unwrap();
    assert!(matches!(plan, RemovalPlan::NotFound), "{plan:?}");
}

#[test]
fn a_new_workspace_of_a_repository_whose_origin_has_no_commits_fails_and_leaves_nothing() {
    let setup = setup();
    let root = setup.fixture.dir.path().canonicalize().unwrap();
    git(
        &root,
        &[
            "init",
            "--quiet",
            "--bare",
            "--initial-branch=main",
            "empty.git",
        ],
    );
    git(&root, &["clone", "--quiet", "empty.git", "fresh"]);
    let result = setup.workspaces.create(
        &setup.root,
        &name("reading-list"),
        &root.join("fresh"),
        Mode::Change,
    );
    assert!(
        matches!(result, Err(Error::EmptyOrigin { .. })),
        "{result:?}"
    );
    assert!(setup.workspaces.list().unwrap().workspaces.is_empty());
    assert!(!setup.root.as_path().join("reading-list").exists());
}

#[test]
fn an_ignored_file_inside_a_tracked_build_directory_is_not_disposable() {
    let setup = setup();
    write(
        setup.fixture.clone.join(".git/info/exclude"),
        "*.secret\nbuild\n",
    );
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0].path;
    fs::create_dir_all(checkout.join("tools/dist")).unwrap();
    write(checkout.join("tools/dist/README"), "tracked");
    git(checkout, &["add", "tools/dist/README"]);
    commit(checkout, 1);
    write(checkout.join("tools/dist/prod.secret"), "secret");
    write(checkout.join("build"), "an ignored file named build");
    let plan = setup
        .workspaces
        .plan_removal(&name("reading-list"), NOTHING)
        .unwrap();
    let RemovalPlan::IgnoredFiles { paths, .. } = plan else {
        panic!("{plan:?}");
    };
    assert_eq!(paths, ["app: build", "app: tools/dist/prod.secret"]);
}

#[test]
fn a_branch_whose_base_is_gone_counts_as_unmerged_with_an_unknown_count() {
    let setup = setup();
    created(&setup, "reading-list");
    git(
        &setup.fixture.clone,
        &["update-ref", "-d", "refs/remotes/origin/main"],
    );
    let removal = ready(&setup, "reading-list", NOTHING);
    assert_eq!(
        removal.checkouts[0].branch,
        BranchFate::KeptUnmerged { commits: None }
    );
    let removal = ready(&setup, "reading-list", UNMERGED);
    assert_eq!(
        removal.checkouts[0].branch,
        BranchFate::DeletedUnmerged { commits: None }
    );
    done(setup.workspaces.remove(removal).unwrap());
    assert!(!branch_exists(&setup));
}

#[test]
fn a_workspace_path_that_cannot_be_inspected_stops_the_removal() {
    use std::os::unix::fs::PermissionsExt as _;
    let setup = setup();
    created(&setup, "reading-list");
    fs::set_permissions(&setup.root, fs::Permissions::from_mode(0o000)).unwrap();
    let plan = setup.workspaces.plan_removal(&name("reading-list"), FORCE);
    fs::set_permissions(&setup.root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(plan, Err(Error::Inaccessible { .. })), "{plan:?}");
}

#[test]
fn commits_on_a_detached_head_of_a_checkout_whose_directory_is_gone_stop_the_removal() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0].path;
    git(checkout, &["switch", "--quiet", "--detach"]);
    commit(checkout, 2);
    fs::remove_dir_all(checkout).unwrap();
    let plan = setup
        .workspaces
        .plan_removal(&name("reading-list"), FORCE)
        .unwrap();
    let RemovalPlan::StrayCommits { commits, .. } = plan else {
        panic!("{plan:?}");
    };
    assert_eq!(commits, 2);
    let removal = ready(&setup, "reading-list", UNMERGED);
    assert_eq!(removal.checkouts[0].directory, Fate::Missing);
    assert_eq!(removal.checkouts[0].stray_commits, 2);
    done(setup.workspaces.remove(removal).unwrap());
}

#[test]
fn a_checkout_locked_while_initializing_is_unlocked_and_removed() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.to_str().unwrap().to_owned();
    git(
        &setup.fixture.clone,
        &["worktree", "lock", "--reason", "initializing", &checkout],
    );
    done(
        setup
            .workspaces
            .remove(ready(&setup, "reading-list", NOTHING))
            .unwrap(),
    );
    assert!(!workspace.path.exists());
    let listed = git(&setup.fixture.clone, &["worktree", "list", "--porcelain"]);
    assert!(!listed.contains("reading-list"), "{listed}");
}

#[test]
fn a_checkout_locked_for_another_reason_stops_the_removal() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.to_str().unwrap().to_owned();
    git(
        &setup.fixture.clone,
        &["worktree", "lock", "--reason", "on a USB disk", &checkout],
    );
    let plan = setup
        .workspaces
        .plan_removal(&name("reading-list"), FORCE)
        .unwrap();
    let RemovalPlan::Locked { reason, .. } = plan else {
        panic!("{plan:?}");
    };
    assert_eq!(reason, "on a USB disk");
}

/// A backend that runs git, and lets a test act at four points, as
/// another process might: before the fetch, after the checkout is added,
/// before a branch is deleted, and before a checkout is removed.
struct Scripted {
    /// Runs in the repository before the fetch.
    before_fetch: fn(&Path),
    /// Runs in the repository after the checkout is added, and returns the
    /// error that the checkout then fails with, if any.
    after_add: fn(&Path, &Path) -> Option<Error>,
    /// Runs in the repository before a branch is deleted.
    before_delete: fn(&Path),
    /// Runs with the checkout before `git worktree remove`, and returns the
    /// error that the removal then fails with, if any, in place of running
    /// it, as a `git worktree remove` that missed its deadline does.
    before_remove: fn(&Path) -> Option<Error>,
}

impl std::fmt::Debug for Scripted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Scripted")
    }
}

impl Backend for Scripted {
    fn repository(&self, path: &Path) -> Result<Repository, Error> {
        Git.repository(path)
    }
    fn fetch(&self, repository: &Path) -> Result<(), Error> {
        (self.before_fetch)(repository);
        Git.fetch(repository)
    }
    fn base(&self, repository: &Path) -> Result<String, Error> {
        Git.base(repository)
    }
    fn branch_tip(&self, repository: &Path, branch: &str) -> Result<Option<String>, Error> {
        Git.branch_tip(repository, branch)
    }
    fn create_branch(
        &self,
        repository: &Path,
        branch: &str,
        base: &str,
    ) -> Result<Option<String>, Error> {
        Git.create_branch(repository, branch, base)
    }
    fn add_checkout(
        &self,
        repository: &Path,
        checkout: &Path,
        branch: &str,
    ) -> Result<String, Error> {
        let commit = Git.add_checkout(repository, checkout, branch)?;
        match (self.after_add)(repository, checkout) {
            Some(error) => Err(error),
            None => Ok(commit),
        }
    }
    fn listed_checkout(&self, repository: &Path, checkout: &Path) -> Result<Option<Listed>, Error> {
        Git.listed_checkout(repository, checkout)
    }
    fn unlock_checkout(&self, repository: &Path, checkout: &Path) -> Result<(), Error> {
        Git.unlock_checkout(repository, checkout)
    }
    fn changes(&self, checkout: &Path) -> Result<Changes, Error> {
        Git.changes(checkout)
    }
    fn head(&self, checkout: &Path) -> Result<Head, Error> {
        Git.head(checkout)
    }
    fn tracks(&self, checkout: &Path, path: &str) -> Result<bool, Error> {
        Git.tracks(checkout, path)
    }
    fn remove_checkout(
        &self,
        repository: &Path,
        checkout: &Path,
        force: bool,
    ) -> Result<(), Error> {
        if let Some(error) = (self.before_remove)(checkout) {
            return Err(error);
        }
        Git.remove_checkout(repository, checkout, force)
    }
    fn prune_checkouts(&self, repository: &Path) -> Result<(), Error> {
        Git.prune_checkouts(repository)
    }
    fn unmerged_commits(
        &self,
        repository: &Path,
        revision: &str,
        base: &str,
    ) -> Result<Option<u32>, Error> {
        Git.unmerged_commits(repository, revision, base)
    }
    fn stray_commits(&self, dir: &Path, revision: &str) -> Result<u32, Error> {
        Git.stray_commits(dir, revision)
    }
    fn origin_url(&self, repository: &Path) -> Result<String, Error> {
        Git.origin_url(repository)
    }
    fn prepare_mirror(&self, mirror: &Path) -> Result<(), Error> {
        Git.prepare_mirror(mirror)
    }
    fn fetch_mirror(&self, mirror: &Path, url: &str, default: &str) -> Result<(), Error> {
        Git.fetch_mirror(mirror, url, default)
    }
    fn clone_mirror(&self, mirror: &Path, checkout: &Path, url: &str) -> Result<(), Error> {
        Git.clone_mirror(mirror, checkout, url)
    }
    fn exclude_file(&self, repository: &Path) -> Result<PathBuf, Error> {
        Git.exclude_file(repository)
    }
    fn switch_branch(&self, checkout: &Path, branch: &str) -> Result<String, Error> {
        Git.switch_branch(checkout, branch)
    }
    fn refresh_origin(&self, checkout: &Path) -> Result<(), Error> {
        Git.refresh_origin(checkout)
    }
    fn unpushed_commits(&self, checkout: &Path) -> Result<Vec<String>, Error> {
        Git.unpushed_commits(checkout)
    }
    fn stashes(&self, checkout: &Path) -> Result<Vec<String>, Error> {
        Git.stashes(checkout)
    }
    fn delete_branch_at(&self, repository: &Path, branch: &str, commit: &str) -> Result<(), Error> {
        (self.before_delete)(repository);
        Git.delete_branch_at(repository, branch, commit)
    }
}

/// Returns workspaces whose backend is `backend`, with the root and store
/// of `setup`.
fn scripted(setup: &Setup, backend: Scripted) -> Workspaces<Scripted> {
    Workspaces::new(Store::new(setup.root.with_file_name("state")), backend)
}

#[test]
fn a_branch_that_another_process_creates_during_the_fetch_is_left_alone() {
    let setup = setup();
    git(
        &setup.fixture.clone,
        &["commit", "--quiet", "--allow-empty", "-m", "Theirs"],
    );
    let theirs = git(&setup.fixture.clone, &["rev-parse", "HEAD"]);
    let workspaces = scripted(
        &setup,
        Scripted {
            before_fetch: |repository| {
                git(repository, &["branch", "reading-list", "HEAD"]);
            },
            after_add: |_, _| None,
            before_delete: |_| {},
            before_remove: |_| None,
        },
    );
    let outcome = workspaces
        .create(
            &setup.root,
            &name("reading-list"),
            &setup.fixture.clone,
            Mode::Change,
        )
        .unwrap();
    assert!(
        matches!(outcome, NewOutcome::BranchTaken { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        git(&setup.fixture.clone, &["rev-parse", "reading-list"]),
        theirs
    );
    assert!(!setup.root.join("reading-list").exists());
    assert!(workspaces.list().unwrap().workspaces.is_empty());
}

#[test]
fn a_checkout_left_locked_while_initializing_by_a_failed_add_is_unlocked_and_removed() {
    let setup = setup();
    let workspaces = scripted(
        &setup,
        Scripted {
            before_fetch: |_| {},
            after_add: |repository, checkout| {
                git(
                    repository,
                    &[
                        "worktree",
                        "lock",
                        "--reason",
                        "initializing",
                        checkout.to_str().unwrap(),
                    ],
                );
                Some(Error::TimedOut {
                    operation: "worktree add",
                    dir: repository.to_path_buf(),
                    deadline: std::time::Duration::from_secs(600),
                })
            },
            before_delete: |_| {},
            before_remove: |_| None,
        },
    );
    let result = workspaces.create(
        &setup.root,
        &name("reading-list"),
        &setup.fixture.clone,
        Mode::Change,
    );
    assert!(matches!(result, Err(Error::TimedOut { .. })), "{result:?}");
    let listed = git(&setup.fixture.clone, &["worktree", "list", "--porcelain"]);
    assert!(!listed.contains("reading-list"), "{listed}");
    assert!(!branch_exists(&setup));
    assert!(!setup.root.join("reading-list").exists());
    assert!(workspaces.list().unwrap().workspaces.is_empty());
}

#[test]
fn a_branch_that_gains_a_commit_after_the_plan_is_kept_at_that_commit() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let plan = ready(&setup, "reading-list", UNMERGED);
    assert_eq!(plan.checkouts[0].branch, BranchFate::Deleted);
    commit(&workspace.checkouts[0].path, 1);
    let moved = git(&setup.fixture.clone, &["rev-parse", "reading-list"]);
    let removal = done(setup.workspaces.remove(plan).unwrap());
    assert_eq!(removal.checkouts[0].branch, BranchFate::KeptMoved);
    assert!(!workspace.path.exists());
    assert_eq!(
        git(&setup.fixture.clone, &["rev-parse", "reading-list"]),
        moved
    );
}

#[test]
fn a_branch_that_moves_while_the_removal_runs_is_kept_at_its_new_commit() {
    let setup = setup();
    created(&setup, "reading-list");
    let workspaces = scripted(
        &setup,
        Scripted {
            before_fetch: |_| {},
            after_add: |_, _| None,
            before_delete: |repository| {
                git(
                    repository,
                    &["commit", "--quiet", "--allow-empty", "-m", "Theirs"],
                );
                git(repository, &["branch", "--force", "reading-list", "HEAD"]);
            },
            before_remove: |_| None,
        },
    );
    let RemovalPlan::Ready(plan) = workspaces
        .plan_removal(&name("reading-list"), UNMERGED)
        .unwrap()
    else {
        panic!("the plan is not ready");
    };
    let RemovalOutcome::Done(removal) = workspaces.remove(plan).unwrap() else {
        panic!("the removal did not run");
    };
    assert_eq!(removal.checkouts[0].branch, BranchFate::KeptMoved);
    assert_eq!(
        git(&setup.fixture.clone, &["rev-parse", "reading-list"]),
        git(&setup.fixture.clone, &["rev-parse", "HEAD"])
    );
    assert_eq!(workspaces.find(&name("reading-list")).unwrap(), None);
}

#[test]
fn commits_on_a_detached_head_made_after_the_plan_stop_the_removal() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let plan = ready(&setup, "reading-list", NOTHING);
    let checkout = &workspace.checkouts[0].path;
    git(checkout, &["switch", "--quiet", "--detach"]);
    commit(checkout, 1);
    let outcome = setup.workspaces.remove(plan).unwrap();
    assert!(
        matches!(
            outcome,
            RemovalOutcome::Changed(RemovalPlan::StrayCommits { commits: 1, .. })
        ),
        "{outcome:?}"
    );
    assert!(checkout.exists());
    assert!(branch_exists(&setup));
    assert!(
        setup
            .workspaces
            .find(&name("reading-list"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn commits_that_no_branch_holds_made_after_the_plan_stop_a_removal_that_allows_deleting_them() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let plan = ready(&setup, "reading-list", UNMERGED);
    let checkout = &workspace.checkouts[0].path;
    git(checkout, &["switch", "--quiet", "--detach"]);
    commit(checkout, 1);
    let outcome = setup.workspaces.remove(plan).unwrap();
    let RemovalOutcome::Changed(RemovalPlan::Ready(fresh)) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(fresh.checkouts[0].stray_commits, 1);
    assert!(checkout.exists());
    assert!(
        setup
            .workspaces
            .find(&name("reading-list"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_file_written_after_a_forced_plan_stops_the_removal() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    write(workspace.checkouts[0].path.join("notes.md"), "notes");
    let plan = ready(&setup, "reading-list", FORCE);
    write(workspace.checkouts[0].path.join("later.md"), "later");
    let outcome = setup.workspaces.remove(plan).unwrap();
    let RemovalOutcome::Changed(RemovalPlan::Ready(fresh)) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(fresh.discarded, ["app: ?? later.md", "app: ?? notes.md"]);
    assert!(workspace.checkouts[0].path.join("later.md").exists());
}

#[test]
fn a_file_written_after_the_plan_stops_the_removal() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let plan = ready(&setup, "reading-list", NOTHING);
    write(workspace.checkouts[0].path.join("notes.md"), "notes");
    let outcome = setup.workspaces.remove(plan).unwrap();
    assert!(
        matches!(outcome, RemovalOutcome::Changed(RemovalPlan::Dirty { .. })),
        "{outcome:?}"
    );
    assert!(workspace.checkouts[0].path.join("notes.md").exists());
}

#[test]
fn an_ignored_directory_that_the_manifest_declares_disposable_does_not_stop_the_removal() {
    let setup = setup();
    write(
        setup.fixture.clone.join(".git/info/exclude"),
        "test-results/\nmxv.toml\n",
    );
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0].path;
    write(
        checkout.join("mxv.toml"),
        "disposable = [\"web/test-results/\"]\n",
    );
    fs::create_dir_all(checkout.join("web/test-results")).unwrap();
    write(checkout.join("web/test-results/report.html"), "<p>ok</p>");
    let plan = setup
        .workspaces
        .plan_removal(&name("reading-list"), NOTHING)
        .unwrap();
    let RemovalPlan::IgnoredFiles { paths, .. } = plan else {
        panic!("{plan:?}");
    };
    // The manifest itself is an ignored file here, and still stops the
    // removal, which shows that only the glob's paths became disposable.
    assert_eq!(paths, ["app: mxv.toml"]);
}

#[test]
fn an_ignored_directory_that_the_person_declares_disposable_does_not_stop_the_removal() {
    let mut setup = setup();
    write(setup.fixture.clone.join(".git/info/exclude"), ".venv/\n");
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0].path;
    fs::create_dir_all(checkout.join("tools/.venv/bin")).unwrap();
    write(checkout.join("tools/.venv/bin/python"), "");
    setup.workspaces = setup
        .workspaces
        .with_disposable(vec![PathGlob::parse(".venv/").unwrap()]);
    let removal = ready(&setup, "reading-list", NOTHING);
    assert_eq!(removal.disposable, ["app: tools/.venv/"]);
    done(setup.workspaces.remove(removal).unwrap());
    assert!(!workspace.path.exists());
}

#[test]
fn a_declared_glob_never_makes_a_directory_with_a_tracked_file_disposable() {
    let mut setup = setup();
    write(setup.fixture.clone.join(".git/info/exclude"), "*.log\n");
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0].path;
    fs::create_dir_all(checkout.join("docs")).unwrap();
    write(checkout.join("docs/README"), "tracked");
    git(checkout, &["add", "docs/README"]);
    commit(checkout, 1);
    write(checkout.join("docs/build.log"), "log");
    setup.workspaces = setup
        .workspaces
        .with_disposable(vec![PathGlob::parse("docs/").unwrap()]);
    let plan = setup
        .workspaces
        .plan_removal(&name("reading-list"), NOTHING)
        .unwrap();
    let RemovalPlan::IgnoredFiles { paths, .. } = plan else {
        panic!("{plan:?}");
    };
    assert_eq!(paths, ["app: docs/build.log"]);
}

#[test]
fn a_disposable_directory_that_holds_a_git_entry_stops_the_removal_unless_forced() {
    let setup = setup();
    ignore(&setup);
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0].path;
    fs::create_dir_all(checkout.join("node_modules/fork/.git")).unwrap();
    let plan = setup
        .workspaces
        .plan_removal(&name("reading-list"), NOTHING)
        .unwrap();
    let RemovalPlan::IgnoredFiles { paths, .. } = plan else {
        panic!("{plan:?}");
    };
    assert_eq!(paths, ["app: node_modules/"]);
}

#[test]
fn an_invalid_manifest_warns_in_the_removal_plan() {
    let setup = setup();
    ignore(&setup);
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0].path;
    write(checkout.join("mxv.toml"), "disposable = [\"*\"]\n");
    git(checkout, &["add", "mxv.toml"]);
    git(checkout, &["commit", "--quiet", "-m", "Add a manifest"]);
    let removal = ready(&setup, "reading-list", NOTHING);
    assert_eq!(removal.warnings.len(), 1, "{removal:?}");
}

#[test]
fn the_removal_plan_marks_a_path_that_only_the_manifest_makes_disposable() {
    let mut setup = setup();
    write(
        setup.fixture.clone.join(".git/info/exclude"),
        "data/\ncoverage/\ntarget/\nmxv.toml\n",
    );
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0].path;
    write(
        checkout.join("mxv.toml"),
        "disposable = [\"data/\", \"coverage/\"]\n",
    );
    for dir in ["data", "coverage", "target"] {
        fs::create_dir_all(checkout.join(dir)).unwrap();
        write(checkout.join(dir).join("file"), "");
    }
    setup.workspaces = setup
        .workspaces
        .with_disposable(vec![PathGlob::parse("coverage/").unwrap()]);
    let removal = ready(&setup, "reading-list", FORCE);
    assert_eq!(removal.manifest, ["app: data/"]);
}

/// Returns the failure of a `git worktree remove` in `checkout` that
/// missed its deadline.
fn timed_out(checkout: &Path) -> Error {
    Error::TimedOut {
        operation: "worktree remove",
        dir: checkout.to_path_buf(),
        deadline: std::time::Duration::from_millis(1),
    }
}

/// Returns a backend that runs git and runs `before_remove` before each
/// `git worktree remove`.
fn removing(before_remove: fn(&Path) -> Option<Error>) -> Scripted {
    Scripted {
        before_fetch: |_| {},
        after_add: |_, _| None,
        before_delete: |_| {},
        before_remove,
    }
}

/// Reports whether the fixture's clone still holds the branch `branch`.
fn has_branch(setup: &Setup, branch: &str) -> bool {
    !git(&setup.fixture.clone, &["branch", "--list", branch]).is_empty()
}

#[test]
fn the_disposable_directories_of_a_worktree_are_deleted_before_git_removes_it() {
    let setup = setup();
    ignore(&setup);
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0].path;
    fs::create_dir_all(checkout.join("target/debug")).unwrap();
    write(checkout.join("target/debug/mxv"), "binary");
    let workspaces = scripted(
        &setup,
        removing(|checkout| {
            assert!(
                !checkout.join("target").exists(),
                "git received the checkout with its target/"
            );
            None
        }),
    );
    let RemovalPlan::Ready(removal) = workspaces
        .plan_removal(&name("reading-list"), NOTHING)
        .unwrap()
    else {
        panic!("the removal is refused");
    };
    assert_eq!(removal.checkouts[0].disposable, ["target/"]);
    done(workspaces.remove(removal).unwrap());
    assert!(!workspace.path.exists());
}

#[test]
fn a_removal_whose_git_times_out_keeps_the_record_and_the_branch_and_runs_again() {
    let setup = setup();
    ignore(&setup);
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0].path;
    fs::create_dir_all(checkout.join("target/debug")).unwrap();
    write(checkout.join("target/debug/mxv"), "binary");
    let workspaces = scripted(&setup, removing(|checkout| Some(timed_out(checkout))));
    let RemovalPlan::Ready(removal) = workspaces
        .plan_removal(&name("reading-list"), NOTHING)
        .unwrap()
    else {
        panic!("the removal is refused");
    };
    let error = workspaces.remove(removal).unwrap_err();
    assert!(matches!(error, Error::TimedOut { .. }), "{error:?}");
    assert!(
        setup
            .workspaces
            .find(&name("reading-list"))
            .unwrap()
            .is_some()
    );
    assert!(has_branch(&setup, "reading-list"));
    assert!(!checkout.join("target").exists());
    done(
        setup
            .workspaces
            .remove(ready(&setup, "reading-list", NOTHING))
            .unwrap(),
    );
    assert!(!workspace.path.exists());
    assert!(
        setup
            .workspaces
            .find(&name("reading-list"))
            .unwrap()
            .is_none()
    );
    assert!(!has_branch(&setup, "reading-list"));
}

#[test]
fn a_worktree_that_an_interrupted_removal_left_without_its_git_file_is_finished_when_forced() {
    let setup = setup();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.checkouts[0].path.clone();
    // A `git worktree remove` that stopped partway deleted the checkout's
    // `.git` file and left other files behind.
    let workspaces = scripted(
        &setup,
        removing(|checkout| {
            fs::remove_file(checkout.join(".git")).unwrap();
            Some(timed_out(checkout))
        }),
    );
    write(checkout.join("notes.md"), "left behind");
    let RemovalPlan::Ready(removal) = workspaces
        .plan_removal(&name("reading-list"), FORCE)
        .unwrap()
    else {
        panic!("the removal is refused");
    };
    assert!(workspaces.remove(removal).is_err());
    assert!(
        setup
            .workspaces
            .find(&name("reading-list"))
            .unwrap()
            .is_some()
    );
    assert!(has_branch(&setup, "reading-list"));
    let plan = setup
        .workspaces
        .plan_removal(&name("reading-list"), NOTHING)
        .unwrap();
    let RemovalPlan::Dirty { changes, .. } = plan else {
        panic!("{plan:?}");
    };
    assert!(changes[0].contains("no .git"), "{changes:?}");
    assert!(checkout.join("notes.md").exists());
    let removal = ready(&setup, "reading-list", FORCE);
    assert!(removal.checkouts[0].partial);
    done(setup.workspaces.remove(removal).unwrap());
    assert!(!workspace.path.exists());
    let listed = git(&setup.fixture.clone, &["worktree", "list", "--porcelain"]);
    assert!(!listed.contains("reading-list"), "{listed}");
    assert!(!has_branch(&setup, "reading-list"));
}
