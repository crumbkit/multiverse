//! The tests of workspaces that hold several repositories.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::git::Git;
use crate::name::WorkspaceName;
use crate::record::{MULTI_CHECKOUT_RECORD_VERSION, Workspace};
use crate::store::Store;
use crate::testing::{Fixture, fixture_named, git, write};
use crate::workspaces::{
    AddOutcome, Allow, Mode, NewOutcome, RemovalOutcome, RemovalPlan, Repo, Workspaces,
};

/// Two fixture repositories, `api` and `web`, and workspaces of clones
/// whose root, store and mirrors live in a temporary directory.
struct Setup {
    api: Fixture,
    web: Fixture,
    root: PathBuf,
    state: PathBuf,
    _dir: TempDir,
    workspaces: Workspaces<Git>,
}

fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let workspaces =
        Workspaces::new(Store::new(base.join("state")), Git).with_mirrors(base.join("mirrors"));
    Setup {
        api: fixture_named("api"),
        web: fixture_named("web"),
        root: base.join("mxv"),
        state: base.join("state"),
        _dir: dir,
        workspaces,
    }
}

fn name(text: &str) -> WorkspaceName {
    text.parse().unwrap()
}

fn created(setup: &Setup, text: &str, repos: &[Repo]) -> Workspace {
    match setup
        .workspaces
        .create_repos(&setup.root, &name(text), repos, Mode::Change, None)
        .unwrap()
    {
        NewOutcome::Created(workspace) => workspace,
        other => panic!("{other:?}"),
    }
}

fn both(setup: &Setup) -> Vec<Repo> {
    vec![Repo::new(&setup.api.clone), Repo::new(&setup.web.clone)]
}

fn branch(checkout: &Path) -> String {
    git(checkout, &["branch", "--show-current"])
        .trim()
        .to_owned()
}

#[test]
fn a_workspace_of_two_repositories_holds_a_clone_of_each_in_a_folder_named_after_it_on_the_workspace_branch()
 {
    let setup = setup();
    let workspace = created(&setup, "triage", &both(&setup));
    let folder = setup.root.join("triage");
    assert_eq!(workspace.checkouts.len(), 2);
    for (checkout, repo) in workspace.checkouts.iter().zip(["api", "web"]) {
        assert_eq!(checkout.name, repo);
        assert_eq!(checkout.path, folder.join(repo));
        assert!(checkout.path.join(".git").is_dir());
        assert_eq!(branch(&checkout.path), "triage");
    }
    let record = fs::read_to_string(setup.state.join("triage.json")).unwrap();
    assert!(record.contains("\"version\": 4"), "{record}");
}

#[test]
fn the_single_repository_form_keeps_its_checkout_and_record_version() {
    let setup = setup();
    let workspace = created(&setup, "triage", &[Repo::new(&setup.api.clone)]);
    assert_eq!(workspace.checkouts[0].path, setup.root.join("triage/api"));
    let record = fs::read_to_string(setup.state.join("triage.json")).unwrap();
    assert!(record.contains("\"version\": 3"), "{record}");
}

#[test]
fn two_repositories_with_the_same_name_are_refused_unless_one_gets_another_folder() {
    let setup = setup();
    let other = fixture_named("api");
    let repos = [Repo::new(&setup.api.clone), Repo::new(&other.clone)];
    let outcome = setup
        .workspaces
        .create_repos(&setup.root, &name("triage"), &repos, Mode::Change, None)
        .unwrap();
    match outcome {
        NewOutcome::FolderTaken { folder, repository } => {
            assert_eq!(folder, "api");
            assert_eq!(repository, other.clone);
        }
        other => panic!("{other:?}"),
    }
    assert!(!setup.root.join("triage").exists());
    let renamed = [
        Repo::new(&setup.api.clone),
        Repo::named(&other.clone, "api-two").unwrap(),
    ];
    let workspace = created(&setup, "triage", &renamed);
    assert_eq!(workspace.checkouts[1].name, "api-two");
    assert!(setup.root.join("triage/api-two/.git").is_dir());
}

#[test]
fn a_folder_is_refused_when_it_names_a_file_that_mxv_writes() {
    assert!(Repo::named("/src/app", "mxv.toml").is_err());
    assert!(Repo::named("/src/app", "Upper").is_err());
    assert!(Repo::named("/src/app", "app.v2").is_ok());
}

#[test]
fn a_failed_second_clone_deletes_the_first_and_the_record() {
    let setup = setup();
    fs::remove_dir_all(&setup.web.origin).unwrap();
    let error = setup
        .workspaces
        .create_repos(
            &setup.root,
            &name("triage"),
            &both(&setup),
            Mode::Change,
            None,
        )
        .unwrap_err();
    assert!(
        matches!(error, crate::error::Error::MirrorFetchFailed { .. }),
        "{error:?}"
    );
    assert!(!setup.root.join("triage").exists());
    assert!(!setup.state.join("triage.json").exists());
}

#[test]
fn the_agents_file_lists_every_checkout_with_its_path_and_origin() {
    let setup = setup();
    let workspace = created(&setup, "triage", &both(&setup));
    let agents = fs::read_to_string(workspace.agents_file()).unwrap();
    for (checkout, fixture) in workspace.checkouts.iter().zip([&setup.api, &setup.web]) {
        assert!(
            agents.contains(&format!("Its path is `{}`", checkout.path.display())),
            "{agents}"
        );
        assert!(
            agents.contains(&format!("its origin is `{}`", fixture.origin.display())),
            "{agents}"
        );
    }
}

#[test]
fn add_clones_the_repository_on_the_workspace_branch_and_rewrites_the_agents_file_and_record() {
    let setup = setup();
    created(&setup, "triage", &[Repo::new(&setup.api.clone)]);
    let outcome = setup
        .workspaces
        .add(&name("triage"), &Repo::new(&setup.web.clone), Mode::Change)
        .unwrap();
    let AddOutcome::Added(workspace) = outcome else {
        panic!("{outcome:?}");
    };
    let web = &workspace.checkouts[1];
    assert_eq!(web.path, setup.root.join("triage/web"));
    assert_eq!(branch(&web.path), "triage");
    assert!(web.commit.is_some());
    let read = setup.workspaces.find(&name("triage")).unwrap().unwrap();
    assert_eq!(read.checkouts.len(), 2);
    assert_eq!(read.version, MULTI_CHECKOUT_RECORD_VERSION);
    let agents = fs::read_to_string(workspace.agents_file()).unwrap();
    assert!(agents.contains("`web/`"), "{agents}");
}

#[test]
fn a_dry_run_of_add_changes_nothing() {
    let setup = setup();
    let before = created(&setup, "triage", &[Repo::new(&setup.api.clone)]);
    let outcome = setup
        .workspaces
        .add(&name("triage"), &Repo::new(&setup.web.clone), Mode::DryRun)
        .unwrap();
    assert!(matches!(outcome, AddOutcome::Planned(ref planned) if planned.checkouts.len() == 2));
    assert!(!setup.root.join("triage/web").exists());
    assert_eq!(
        setup.workspaces.find(&name("triage")).unwrap().unwrap(),
        before
    );
}

#[test]
fn add_refuses_a_folder_that_a_checkout_holds() {
    let setup = setup();
    created(&setup, "triage", &[Repo::new(&setup.api.clone)]);
    let other = fixture_named("api");
    let outcome = setup
        .workspaces
        .add(&name("triage"), &Repo::new(&other.clone), Mode::Change)
        .unwrap();
    assert!(
        matches!(outcome, AddOutcome::FolderTaken { ref folder, .. } if folder == "api"),
        "{outcome:?}"
    );
}

#[test]
fn add_refuses_a_repository_with_a_branch_named_after_the_workspace() {
    let setup = setup();
    created(&setup, "triage", &[Repo::new(&setup.api.clone)]);
    git(&setup.web.clone, &["branch", "triage"]);
    let outcome = setup
        .workspaces
        .add(&name("triage"), &Repo::new(&setup.web.clone), Mode::Change)
        .unwrap();
    assert!(
        matches!(outcome, AddOutcome::BranchTaken { ref branch, .. } if branch == "triage"),
        "{outcome:?}"
    );
}

#[test]
fn a_failed_add_deletes_the_new_clone_and_restores_the_record() {
    let setup = setup();
    let before = created(&setup, "triage", &[Repo::new(&setup.api.clone)]);
    let agents = fs::read_to_string(before.agents_file()).unwrap();
    fs::remove_dir_all(&setup.web.origin).unwrap();
    assert!(
        setup
            .workspaces
            .add(&name("triage"), &Repo::new(&setup.web.clone), Mode::Change)
            .is_err()
    );
    assert!(!setup.root.join("triage/web").exists());
    let read = setup.workspaces.find(&name("triage")).unwrap().unwrap();
    assert_eq!(read.checkouts, before.checkouts);
    assert_eq!(read.version, 3);
    assert_eq!(fs::read_to_string(before.agents_file()).unwrap(), agents);
}

#[test]
fn add_moves_the_ports_of_the_first_checkouts_manifest_to_their_prefixed_names_and_releases_the_rest()
 {
    let setup = setup();
    let mut workspace = created(&setup, "triage", &[Repo::new(&setup.api.clone)]);
    write(
        workspace.checkouts[0].path.join("mxv.toml"),
        "ports = [\"http\"]\n",
    );
    workspace.ports.insert("http".to_owned(), 21000);
    workspace.ports.insert("gone".to_owned(), 21001);
    setup.workspaces.store.update(&mut workspace).unwrap();
    let outcome = setup
        .workspaces
        .add(&name("triage"), &Repo::new(&setup.web.clone), Mode::Change)
        .unwrap();
    let AddOutcome::Added(workspace) = outcome else {
        panic!("{outcome:?}");
    };
    let expected: std::collections::BTreeMap<String, u16> =
        [("api-http".to_owned(), 21000)].into_iter().collect();
    assert_eq!(workspace.ports, expected);
    let read = setup.workspaces.find(&name("triage")).unwrap().unwrap();
    assert_eq!(read.ports, expected);
}

#[test]
fn add_refuses_a_workspace_manifest_whose_paths_are_relative_to_the_checkout() {
    let setup = setup();
    let workspace = created(&setup, "triage", &[Repo::new(&setup.api.clone)]);
    write(
        workspace.manifest_file(),
        "disposable = [\"data/cache/\"]\n\n[processes.server]\nargv = [\"true\"]\n",
    );
    let outcome = setup
        .workspaces
        .add(&name("triage"), &Repo::new(&setup.web.clone), Mode::Change)
        .unwrap();
    match outcome {
        AddOutcome::ManifestCheckoutRelative { folder, paths, .. } => {
            assert_eq!(folder, "api");
            assert_eq!(
                paths,
                ["processes.server.cwd = \".\"", "disposable \"data/cache/\""]
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(!setup.root.join("triage/web").exists());
}

#[test]
fn add_accepts_a_workspace_manifest_whose_paths_lie_below_the_checkouts_folder_and_keeps_its_ports()
{
    let setup = setup();
    let mut workspace = created(&setup, "triage", &[Repo::new(&setup.api.clone)]);
    write(
        workspace.manifest_file(),
        "ports = [\"http\"]\ndisposable = [\"api/data/\", \"coverage/\"]\n\n[processes.server]\nargv = [\"true\"]\ncwd = \"api\"\n",
    );
    workspace.ports.insert("http".to_owned(), 21000);
    setup.workspaces.store.update(&mut workspace).unwrap();
    let outcome = setup
        .workspaces
        .add(&name("triage"), &Repo::new(&setup.web.clone), Mode::Change)
        .unwrap();
    let AddOutcome::Added(workspace) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(workspace.ports.get("http"), Some(&21000));
    let manifest = workspace.manifest().unwrap().unwrap();
    assert_eq!(
        manifest.processes["server"].cwd,
        setup.root.join("triage/api")
    );
}

#[test]
fn removing_a_workspace_of_two_repositories_removes_both_clones() {
    let setup = setup();
    created(&setup, "triage", &both(&setup));
    let RemovalPlan::Ready(removal) = setup
        .workspaces
        .plan_removal(&name("triage"), Allow::default())
        .unwrap()
    else {
        panic!("the removal is refused");
    };
    assert_eq!(removal.checkouts.len(), 2);
    let RemovalOutcome::Done(_) = setup.workspaces.remove(removal).unwrap() else {
        panic!("the workspace changed");
    };
    assert!(!setup.root.join("triage").exists());
    assert!(!setup.state.join("triage.json").exists());
}

#[test]
fn a_removal_refuses_unpushed_work_in_the_second_checkout_and_names_it() {
    let setup = setup();
    let workspace = created(&setup, "triage", &both(&setup));
    let web = workspace.checkouts[1].path.clone();
    git(&web, &["commit", "--quiet", "--allow-empty", "-m", "Local"]);
    let plan = setup
        .workspaces
        .plan_removal(&name("triage"), Allow::default())
        .unwrap();
    match plan {
        RemovalPlan::Unpushed {
            checkout, commits, ..
        } => {
            assert_eq!(checkout, web);
            assert_eq!(commits, 1);
        }
        other => panic!("{other:?}"),
    }
    assert!(workspace.checkouts[0].path.exists());
}

#[test]
fn a_removal_refuses_a_stash_in_the_second_checkout_and_names_it() {
    let setup = setup();
    let workspace = created(&setup, "triage", &both(&setup));
    let web = workspace.checkouts[1].path.clone();
    write(web.join("notes.md"), "draft\n");
    git(&web, &["stash", "push", "--include-untracked", "--quiet"]);
    let plan = setup
        .workspaces
        .plan_removal(&name("triage"), Allow::default())
        .unwrap();
    assert!(
        matches!(plan, RemovalPlan::Stashed { ref checkout, .. } if *checkout == web),
        "{plan:?}"
    );
}

#[test]
fn a_removal_refuses_when_the_second_checkouts_origin_is_unreachable_and_names_it() {
    let setup = setup();
    let workspace = created(&setup, "triage", &both(&setup));
    fs::remove_dir_all(&setup.web.origin).unwrap();
    let error = setup
        .workspaces
        .plan_removal(&name("triage"), Allow::default())
        .unwrap_err();
    match error {
        crate::error::Error::OriginFetchFailed { checkout, .. } => {
            assert_eq!(checkout, workspace.checkouts[1].path);
        }
        other => panic!("{other:?}"),
    }
}
