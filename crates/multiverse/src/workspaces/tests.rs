use std::fs;
use std::path::PathBuf;

use tempfile::TempDir;

use crate::git::Git;
use crate::name::WorkspaceName;
use crate::record::Workspace;
use crate::store::Store;
use crate::testing::{Fixture, advance_origin, fixture, git, write};
use crate::workspaces::{
    Allow, BranchFate, Fate, Mode, NewOutcome, Removal, RemovalPlan, Workspaces,
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

#[test]
fn a_new_workspace_checks_out_the_latest_origin_main_below_the_workspace_directory() {
    let setup = setup();
    let latest = advance_origin(&setup.fixture);
    let workspace = created(&setup, "reading-list");
    let checkout = &workspace.checkouts[0];
    assert_eq!(workspace.path, setup.root.as_path().join("reading-list"));
    assert_eq!(checkout.path, workspace.path.join("inbox"));
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
    let removal = setup
        .workspaces
        .remove(ready(&setup, "reading-list", NOTHING))
        .unwrap();
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
    assert_eq!(changes, ["inbox: ?? notes.md"]);
    let removal = setup
        .workspaces
        .remove(ready(&setup, "reading-list", FORCE))
        .unwrap();
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
    let removal = setup
        .workspaces
        .remove(ready(&setup, "reading-list", FORCE))
        .unwrap();
    assert_eq!(
        removal.checkouts[0].branch,
        BranchFate::KeptUnmerged { commits: 2 }
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
        BranchFate::DeletedUnmerged { commits: 3 }
    );
    setup.workspaces.remove(plan).unwrap();
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
    assert_eq!(paths, ["inbox: .env"]);
    assert!(checkout.join(".env").exists());

    let removal = ready(&setup, "reading-list", FORCE);
    assert_eq!(removal.discarded, ["inbox: .env"]);
    assert_eq!(removal.disposable, ["inbox: target/"]);
    setup.workspaces.remove(removal).unwrap();
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
        ["inbox: target/", "inbox: web/node_modules/"]
    );
    assert!(removal.discarded.is_empty());
    setup.workspaces.remove(removal).unwrap();
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
    setup.workspaces.remove(removal).unwrap();
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
        matches!(result, Err(crate::error::Error::CommandFailed { .. })),
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
    let removal = setup
        .workspaces
        .remove(ready(&setup, "reading-list", NOTHING))
        .unwrap();
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
    let removal = setup
        .workspaces
        .remove(ready(&setup, "reading-list", NOTHING))
        .unwrap();
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
        matches!(result, Err(crate::error::Error::EmptyOrigin { .. })),
        "{result:?}"
    );
    assert!(setup.workspaces.list().unwrap().workspaces.is_empty());
    assert!(!setup.root.as_path().join("reading-list").exists());
}
