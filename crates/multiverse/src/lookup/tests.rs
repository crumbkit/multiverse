use std::fs;
use std::path::Path;

use crate::lookup::ManifestSource;
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
