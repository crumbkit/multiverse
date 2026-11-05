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
        .add(
            &name("triage"),
            &Repo::new(&setup.web.clone),
            Mode::Change,
            None,
        )
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
        .add(
            &name("triage"),
            &Repo::new(&setup.web.clone),
            Mode::DryRun,
            None,
        )
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
        .add(
            &name("triage"),
            &Repo::new(&other.clone),
            Mode::Change,
            None,
        )
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
        .add(
            &name("triage"),
            &Repo::new(&setup.web.clone),
            Mode::Change,
            None,
        )
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
            .add(
                &name("triage"),
                &Repo::new(&setup.web.clone),
                Mode::Change,
                None
            )
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
        .add(
            &name("triage"),
            &Repo::new(&setup.web.clone),
            Mode::Change,
            None,
        )
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
fn add_refuses_a_workspace_of_one_checkout_with_a_workspace_manifest_unless_given_the_next_one() {
    let setup = setup();
    let workspace = created(&setup, "triage", &[Repo::new(&setup.api.clone)]);
    let text = "disposable = [\"web/coverage/\"]\n\n[processes.server]\nargv = [\"true\"]\n";
    write(workspace.manifest_file(), text);
    let outcome = setup
        .workspaces
        .add(
            &name("triage"),
            &Repo::new(&setup.web.clone),
            Mode::Change,
            None,
        )
        .unwrap();
    match outcome {
        AddOutcome::ManifestNeeded {
            folder, manifest, ..
        } => {
            assert_eq!(folder, "api");
            assert_eq!(manifest, workspace.manifest_file());
        }
        other => panic!("{other:?}"),
    }
    assert!(!setup.root.join("triage/web").exists());
    assert_eq!(fs::read_to_string(workspace.manifest_file()).unwrap(), text);
}

#[test]
fn add_installs_the_given_workspace_manifest_with_paths_relative_to_the_workspace() {
    let setup = setup();
    let mut workspace = created(&setup, "triage", &[Repo::new(&setup.api.clone)]);
    write(
        workspace.manifest_file(),
        "ports = [\"http\"]\n\n[processes.server]\nargv = [\"true\"]\n",
    );
    workspace.ports.insert("http".to_owned(), 21000);
    setup.workspaces.store.update(&mut workspace).unwrap();
    let next = "ports = [\"http\"]\ndisposable = [\"web/coverage/\"]\n\n[processes.server]\nargv = [\"true\"]\ncwd = \"api\"\n";
    let outcome = setup
        .workspaces
        .add(
            &name("triage"),
            &Repo::new(&setup.web.clone),
            Mode::Change,
            Some(next),
        )
        .unwrap();
    let AddOutcome::Added(workspace) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(fs::read_to_string(workspace.manifest_file()).unwrap(), next);
    assert_eq!(workspace.ports.get("http"), Some(&21000));
    let manifest = workspace.manifest().unwrap().unwrap();
    assert_eq!(
        manifest.processes["server"].cwd,
        setup.root.join("triage/api")
    );
    assert_eq!(
        manifest
            .disposable
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["web/coverage/"]
    );
}

#[test]
fn add_refuses_a_given_workspace_manifest_that_the_merged_manifest_rejects_and_changes_nothing() {
    let setup = setup();
    let before = created(&setup, "triage", &[Repo::new(&setup.api.clone)]);
    let next = "[processes.server]\nargv = [\"true\"]\ndepends_on = [\"web-missing\"]\n";
    let outcome = setup
        .workspaces
        .add(
            &name("triage"),
            &Repo::new(&setup.web.clone),
            Mode::Change,
            Some(next),
        )
        .unwrap();
    assert!(
        matches!(outcome, AddOutcome::ManifestRefused(_)),
        "{outcome:?}"
    );
    assert!(!setup.root.join("triage/web").exists());
    assert!(!before.manifest_file().exists());
    let read = setup.workspaces.find(&name("triage")).unwrap().unwrap();
    assert_eq!(read.checkouts, before.checkouts);
}

#[test]
fn add_refuses_a_default_folder_that_is_not_a_folder_name() {
    let setup = setup();
    created(&setup, "triage", &[Repo::new(&setup.api.clone)]);
    let infra = fixture_named("_infra");
    let outcome = setup
        .workspaces
        .add(
            &name("triage"),
            &Repo::new(&infra.clone),
            Mode::Change,
            None,
        )
        .unwrap();
    assert!(
        matches!(outcome, AddOutcome::BadFolder { ref folder, .. } if folder == "_infra"),
        "{outcome:?}"
    );
    let outcome = setup
        .workspaces
        .add(
            &name("triage"),
            &Repo::named(&infra.clone, "infra").unwrap(),
            Mode::Change,
            None,
        )
        .unwrap();
    assert!(matches!(outcome, AddOutcome::Added(_)), "{outcome:?}");
}

#[test]
fn new_refuses_a_default_folder_that_is_not_a_folder_name_only_for_several_repositories() {
    let setup = setup();
    let infra = fixture_named("_infra");
    let repos = [Repo::new(&setup.api.clone), Repo::new(&infra.clone)];
    let outcome = setup
        .workspaces
        .create_repos(&setup.root, &name("triage"), &repos, Mode::Change, None)
        .unwrap();
    assert!(
        matches!(outcome, NewOutcome::BadFolder { ref folder, .. } if folder == "_infra"),
        "{outcome:?}"
    );
    created(&setup, "solo", &[Repo::new(&infra.clone)]);
}

#[test]
fn new_checks_a_workspace_manifest_against_the_clones_and_deletes_the_workspace_when_it_fails() {
    let setup = setup();
    let text = "[processes.proxy]\nargv = [\"true\"]\ndepends_on = [\"web-missing\"]\n";
    let outcome = setup
        .workspaces
        .create_repos(
            &setup.root,
            &name("triage"),
            &both(&setup),
            Mode::Change,
            Some(text),
        )
        .unwrap();
    assert!(
        matches!(outcome, NewOutcome::ManifestRefused(_)),
        "{outcome:?}"
    );
    assert!(!setup.root.join("triage").exists());
    assert!(!setup.state.join("triage.json").exists());
}

/// Writes the record of `triage` as an `add` of `web` that stopped after
/// it wrote the record with the unfinished checkout, and returns the
/// checkout's directory.
fn stopped_add(setup: &Setup) -> PathBuf {
    let mut workspace = created(setup, "triage", &[Repo::new(&setup.api.clone)]);
    let mut planned = workspace.checkouts[0].clone();
    planned.name = "web".to_owned();
    planned.source.clone_from(&setup.web.clone);
    planned.path = setup.root.join("triage/web");
    planned.commit = None;
    planned.mirror = None;
    workspace.checkouts.push(planned);
    setup.workspaces.store.update(&mut workspace).unwrap();
    setup.root.join("triage/web")
}

/// Clones `web`'s origin into the unfinished checkout's directory at
/// `path`, as an earlier run would have.
fn cloned_at(setup: &Setup, path: &Path) {
    git(
        &setup.root,
        &[
            "clone",
            "--quiet",
            &setup.web.origin.display().to_string(),
            "triage/web",
        ],
    );
    assert!(path.join(".git").is_dir());
}

/// A workspace manifest that refers to a unit that no file declares, so
/// the merged manifest fails.
const BROKEN: &str = "[processes.proxy]\nargv = [\"true\"]\ndepends_on = [\"web-missing\"]\n";

#[test]
fn a_workspace_with_an_unfinished_checkout_names_it() {
    let setup = setup();
    stopped_add(&setup);
    let workspace = setup.workspaces.find(&name("triage")).unwrap().unwrap();
    assert_eq!(
        workspace
            .unfinished_checkout()
            .map(|checkout| checkout.name.as_str()),
        Some("web")
    );
    let finished = created(&setup, "other", &[Repo::new(&setup.web.clone)]);
    assert_eq!(finished.unfinished_checkout(), None);
}

#[test]
fn a_resumed_add_that_fails_restores_the_earlier_record_and_deletes_the_partial_clone() {
    let setup = setup();
    let path = stopped_add(&setup);
    fs::create_dir_all(path.join(".git/objects")).unwrap();
    let outcome = setup
        .workspaces
        .add(
            &name("triage"),
            &Repo::new(&setup.web.clone),
            Mode::Change,
            Some(BROKEN),
        )
        .unwrap();
    assert!(
        matches!(outcome, AddOutcome::ManifestRefused(_)),
        "{outcome:?}"
    );
    let read = setup.workspaces.find(&name("triage")).unwrap().unwrap();
    assert_eq!(read.checkouts.len(), 1);
    assert_eq!(read.unfinished_checkout(), None);
    assert!(!path.exists());
}

#[test]
fn a_resumed_add_that_fails_keeps_a_finished_clone_that_holds_work_and_says_so() {
    let setup = setup();
    let path = stopped_add(&setup);
    cloned_at(&setup, &path);
    git(&path, &["switch", "--quiet", "-c", "triage"]);
    write(path.join("notes.md"), "draft\n");
    let outcome = setup
        .workspaces
        .add(
            &name("triage"),
            &Repo::new(&setup.web.clone),
            Mode::Change,
            Some(BROKEN),
        )
        .unwrap();
    assert!(
        matches!(outcome, AddOutcome::ResumeBlocked { ref reason, .. } if reason.contains("merged manifest fails") && reason.contains("notes.md")),
        "{outcome:?}"
    );
    assert_eq!(
        fs::read_to_string(path.join("notes.md")).unwrap(),
        "draft\n"
    );
    let read = setup.workspaces.find(&name("triage")).unwrap().unwrap();
    assert!(read.unfinished_checkout().is_some());
}

#[test]
fn a_resumed_add_that_fails_deletes_a_finished_clone_without_work_and_restores_the_record() {
    let setup = setup();
    let path = stopped_add(&setup);
    cloned_at(&setup, &path);
    git(&path, &["switch", "--quiet", "-c", "triage"]);
    let outcome = setup
        .workspaces
        .add(
            &name("triage"),
            &Repo::new(&setup.web.clone),
            Mode::Change,
            Some(BROKEN),
        )
        .unwrap();
    assert!(
        matches!(outcome, AddOutcome::ManifestRefused(_)),
        "{outcome:?}"
    );
    assert!(!path.exists());
    let read = setup.workspaces.find(&name("triage")).unwrap().unwrap();
    assert_eq!(read.checkouts.len(), 1);
}

#[test]
fn the_same_add_refuses_to_delete_a_partial_clone_that_holds_an_ignored_file() {
    let setup = setup();
    let path = stopped_add(&setup);
    cloned_at(&setup, &path);
    write(path.join(".git/info/exclude"), ".env\n");
    write(path.join(".env"), "SECRET=local\n");
    let outcome = setup
        .workspaces
        .add(
            &name("triage"),
            &Repo::new(&setup.web.clone),
            Mode::Change,
            None,
        )
        .unwrap();
    assert!(
        matches!(outcome, AddOutcome::ResumeBlocked { ref reason, .. } if reason.contains("ignored") && reason.contains(".env")),
        "{outcome:?}"
    );
    assert_eq!(
        fs::read_to_string(path.join(".env")).unwrap(),
        "SECRET=local\n"
    );
}

#[test]
fn add_refuses_a_workspace_whose_one_checkout_has_a_folder_that_cannot_be_a_prefix() {
    let setup = setup();
    let infra = fixture_named("_infra");
    created(&setup, "triage", &[Repo::new(&infra.clone)]);
    let outcome = setup
        .workspaces
        .add(
            &name("triage"),
            &Repo::new(&setup.web.clone),
            Mode::Change,
            None,
        )
        .unwrap();
    assert!(
        matches!(outcome, AddOutcome::ExistingFolderInvalid { ref folder, .. } if folder == "_infra"),
        "{outcome:?}"
    );
    assert!(!setup.root.join("triage/web").exists());
}

#[test]
fn the_same_add_resumes_a_checkout_that_a_stopped_add_left_without_a_directory() {
    let setup = setup();
    let path = stopped_add(&setup);
    let outcome = setup
        .workspaces
        .add(
            &name("triage"),
            &Repo::new(&setup.web.clone),
            Mode::Change,
            None,
        )
        .unwrap();
    let AddOutcome::Added(workspace) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(workspace.checkouts.len(), 2);
    assert!(workspace.checkouts[1].commit.is_some());
    assert_eq!(branch(&path), "triage");
    let agents = fs::read_to_string(workspace.agents_file()).unwrap();
    assert!(agents.contains("`web/`"), "{agents}");
}

#[test]
fn the_same_add_deletes_a_partial_clone_and_clones_again() {
    let setup = setup();
    let path = stopped_add(&setup);
    fs::create_dir_all(path.join(".git/objects")).unwrap();
    let outcome = setup
        .workspaces
        .add(
            &name("triage"),
            &Repo::new(&setup.web.clone),
            Mode::Change,
            None,
        )
        .unwrap();
    assert!(matches!(outcome, AddOutcome::Added(_)), "{outcome:?}");
    assert_eq!(branch(&path), "triage");
}

#[test]
fn the_same_add_keeps_a_clone_that_reached_the_workspace_branch_and_records_its_commit() {
    let setup = setup();
    let path = stopped_add(&setup);
    git(
        &setup.root,
        &[
            "clone",
            "--quiet",
            &setup.web.origin.display().to_string(),
            "triage/web",
        ],
    );
    git(&path, &["switch", "--quiet", "-c", "triage"]);
    write(path.join("notes.md"), "draft\n");
    let outcome = setup
        .workspaces
        .add(
            &name("triage"),
            &Repo::new(&setup.web.clone),
            Mode::Change,
            None,
        )
        .unwrap();
    let AddOutcome::Added(workspace) = outcome else {
        panic!("{outcome:?}");
    };
    let head = git(&path, &["rev-parse", "HEAD"]);
    assert_eq!(workspace.checkouts[1].commit.as_deref(), Some(head.trim()));
    assert_eq!(
        fs::read_to_string(path.join("notes.md")).unwrap(),
        "draft\n"
    );
}

#[test]
fn the_same_add_refuses_to_delete_a_directory_that_holds_files_outside_a_clone() {
    let setup = setup();
    let path = stopped_add(&setup);
    fs::create_dir_all(&path).unwrap();
    write(path.join("notes.md"), "draft\n");
    let outcome = setup
        .workspaces
        .add(
            &name("triage"),
            &Repo::new(&setup.web.clone),
            Mode::Change,
            None,
        )
        .unwrap();
    assert!(
        matches!(outcome, AddOutcome::ResumeBlocked { path: ref blocked, .. } if *blocked == path),
        "{outcome:?}"
    );
    assert_eq!(
        fs::read_to_string(path.join("notes.md")).unwrap(),
        "draft\n"
    );
}

#[test]
fn the_same_add_refuses_to_delete_a_partial_clone_that_holds_a_commit_of_its_own() {
    let setup = setup();
    let path = stopped_add(&setup);
    git(
        &setup.root,
        &[
            "clone",
            "--quiet",
            &setup.web.origin.display().to_string(),
            "triage/web",
        ],
    );
    git(
        &path,
        &["commit", "--quiet", "--allow-empty", "-m", "Local"],
    );
    let outcome = setup
        .workspaces
        .add(
            &name("triage"),
            &Repo::new(&setup.web.clone),
            Mode::Change,
            None,
        )
        .unwrap();
    assert!(
        matches!(outcome, AddOutcome::ResumeBlocked { ref reason, .. } if reason.contains("1 commits")),
        "{outcome:?}"
    );
    assert!(path.join(".git").is_dir());
}

#[test]
fn the_same_add_refuses_to_delete_a_partial_clone_that_holds_an_untracked_file() {
    let setup = setup();
    let path = stopped_add(&setup);
    git(
        &setup.root,
        &[
            "clone",
            "--quiet",
            &setup.web.origin.display().to_string(),
            "triage/web",
        ],
    );
    write(path.join("notes.md"), "draft\n");
    let outcome = setup
        .workspaces
        .add(
            &name("triage"),
            &Repo::new(&setup.web.clone),
            Mode::Change,
            None,
        )
        .unwrap();
    assert!(
        matches!(outcome, AddOutcome::ResumeBlocked { ref reason, .. } if reason.contains("untracked")),
        "{outcome:?}"
    );
    assert_eq!(
        fs::read_to_string(path.join("notes.md")).unwrap(),
        "draft\n"
    );
}

#[test]
fn another_add_refuses_while_a_checkout_is_unfinished_and_names_it() {
    let setup = setup();
    stopped_add(&setup);
    let other = fixture_named("docs");
    let outcome = setup
        .workspaces
        .add(
            &name("triage"),
            &Repo::new(&other.clone),
            Mode::Change,
            None,
        )
        .unwrap();
    match outcome {
        AddOutcome::Incomplete {
            folder, repository, ..
        } => {
            assert_eq!(folder, "web");
            assert_eq!(repository, setup.web.clone);
        }
        other => panic!("{other:?}"),
    }
    assert!(!setup.root.join("triage/docs").exists());
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

#[test]
fn a_removal_deletes_a_partial_clone_of_an_unfinished_add_without_a_fetch() {
    let setup = setup();
    let path = stopped_add(&setup);
    fs::create_dir_all(path.join(".git/objects")).unwrap();
    let RemovalPlan::Ready(removal) = setup
        .workspaces
        .plan_removal(&name("triage"), Allow::default())
        .unwrap()
    else {
        panic!("the removal is refused");
    };
    let RemovalOutcome::Done(_) = setup.workspaces.remove(removal).unwrap() else {
        panic!("the workspace changed");
    };
    assert!(!setup.root.join("triage").exists());
    assert!(!setup.state.join("triage.json").exists());
}

#[test]
fn a_removal_refuses_a_partial_clone_of_an_unfinished_add_that_holds_files() {
    let setup = setup();
    let path = stopped_add(&setup);
    fs::create_dir_all(path.join(".git/objects")).unwrap();
    write(path.join("notes.txt"), "kept\n");
    let plan = setup
        .workspaces
        .plan_removal(&name("triage"), Allow::default())
        .unwrap();
    let RemovalPlan::Dirty { changes, .. } = plan else {
        panic!("the removal is not refused: {plan:?}");
    };
    assert!(
        changes
            .first()
            .is_some_and(|change| change.starts_with("web: ")),
        "{changes:?}"
    );
    assert!(path.join("notes.txt").exists());
}
