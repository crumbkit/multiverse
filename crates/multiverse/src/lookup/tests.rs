use std::fs;
use std::path::Path;

use crate::lookup::ManifestSource;
use crate::manifest::ManifestProblem;
use crate::record::{Checkout, Workspace};
use crate::testing::write;

fn workspace(dir: &Path) -> Workspace {
    let path = dir.join("reading-list");
    let checkout = Checkout::new(
        "app".to_owned(),
        &dir.join("source"),
        path.join("app"),
        "reading-list".to_owned(),
        "refs/remotes/origin/main".to_owned(),
    );
    fs::create_dir_all(path.join("app")).unwrap();
    Workspace::new("reading-list".parse().unwrap(), path, vec![checkout])
}

#[test]
fn a_workspace_with_neither_file_is_bare() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    let inputs = workspace.manifest_inputs();
    assert_eq!(inputs.source(), ManifestSource::None);
    assert_eq!(inputs.chosen(), None);
    assert_eq!(workspace.manifest().unwrap(), None);
}

#[test]
fn the_checkouts_manifest_is_read_when_the_workspace_has_none() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    write(workspace.path.join("app/mxv.toml"), "ports = [\"api\"]\n");
    let inputs = workspace.manifest_inputs();
    assert_eq!(inputs.source(), ManifestSource::Repository);
    assert_eq!(inputs.repository, Some(workspace.path.join("app/mxv.toml")));
    assert_eq!(workspace.manifest().unwrap().unwrap().ports, ["api"]);
}

#[test]
fn the_workspace_manifest_wins_and_is_never_merged_with_the_checkouts() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    write(workspace.path.join("app/mxv.toml"), "ports = [\"api\"]\n");
    write(workspace.path.join("mxv.toml"), "ports = [\"web\"]\n");
    let inputs = workspace.manifest_inputs();
    assert_eq!(inputs.source(), ManifestSource::Workspace);
    assert_eq!(inputs.workspace, Some(workspace.path.join("mxv.toml")));
    assert_eq!(inputs.repository, Some(workspace.path.join("app/mxv.toml")));
    assert_eq!(
        inputs.chosen(),
        Some(workspace.path.join("mxv.toml").as_path())
    );
    assert_eq!(workspace.manifest().unwrap().unwrap().ports, ["web"]);
}

#[test]
fn an_invalid_workspace_manifest_fails_and_never_falls_back_to_the_checkouts() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    write(workspace.path.join("app/mxv.toml"), "ports = [\"api\"]\n");
    write(workspace.path.join("mxv.toml"), "ports = \n");
    let error = workspace.manifest().unwrap_err();
    assert_eq!(error.path, workspace.path.join("mxv.toml"));
}

#[test]
fn a_cwd_in_the_workspace_manifest_is_relative_to_the_checkout() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    fs::create_dir(workspace.path.join("app/web")).unwrap();
    write(
        workspace.path.join("mxv.toml"),
        "[processes.web]\nargv = [\"true\"]\ncwd = \"web\"\n",
    );
    let manifest = workspace.manifest().unwrap().unwrap();
    assert_eq!(manifest.processes["web"].cwd, Path::new("web"));
}

#[test]
fn a_dangling_link_at_the_workspace_manifest_fails_as_unreadable_and_names_the_link() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    write(workspace.path.join("app/mxv.toml"), "ports = [\"api\"]\n");
    std::os::unix::fs::symlink(dir.path().join("gone.toml"), workspace.manifest_file()).unwrap();
    let error = workspace.manifest().unwrap_err();
    assert_eq!(error.path, workspace.manifest_file());
    let ManifestProblem::Unreadable { message } = &error.problem else {
        panic!("{error:?}");
    };
    assert!(message.contains("gone.toml"), "{message}");
}

#[test]
fn a_regular_workspace_manifest_is_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    write(workspace.manifest_file(), "disposable = [\"data/\"]\n");
    assert!(workspace.manifest_trusted());
}

#[test]
fn a_workspace_manifest_that_links_to_the_checkouts_is_read_and_not_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    write(
        workspace.path.join("app/mxv.toml"),
        "disposable = [\"data/\"]\n",
    );
    std::os::unix::fs::symlink("app/mxv.toml", workspace.manifest_file()).unwrap();
    assert_eq!(
        workspace.manifest_inputs().source(),
        ManifestSource::Workspace
    );
    assert_eq!(workspace.manifest().unwrap().unwrap().disposable.len(), 1);
    assert!(!workspace.manifest_trusted());
}

#[test]
fn a_workspace_manifest_with_a_second_hard_link_is_not_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    write(
        workspace.path.join("app/mxv.toml"),
        "disposable = [\"data/\"]\n",
    );
    fs::hard_link(
        workspace.path.join("app/mxv.toml"),
        workspace.manifest_file(),
    )
    .unwrap();
    assert!(!workspace.manifest_trusted());
}

#[test]
fn a_record_whose_checkout_holds_the_workspace_manifests_path_has_no_workspace_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reading-list");
    fs::create_dir_all(&path).unwrap();
    // An old record whose checkout is the workspace's directory itself.
    let checkout = Checkout::new(
        "app".to_owned(),
        &dir.path().join("source"),
        path.clone(),
        "reading-list".to_owned(),
        "refs/remotes/origin/main".to_owned(),
    );
    let workspace = Workspace::new("reading-list".parse().unwrap(), path, vec![checkout]);
    write(workspace.manifest_file(), "disposable = [\"data/\"]\n");
    assert!(workspace.checkout_holds_manifest_file());
    let inputs = workspace.manifest_inputs();
    assert_eq!(inputs.workspace, None);
    assert_eq!(inputs.source(), ManifestSource::Repository);
    assert!(!workspace.manifest_trusted());
}
