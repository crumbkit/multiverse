use std::fs;
use std::path::PathBuf;

use tempfile::TempDir;

use crate::disposable::PathGlob;
use crate::git::Git;
use crate::name::WorkspaceName;
use crate::record::Workspace;
use crate::store::Store;
use crate::testing::{Fixture, fixture, write};
use crate::workspaces::{Mode, NewOutcome, Workspaces};

use super::{Cloned, UNSUPPORTED};

/// A fixture repository that ignores `node_modules/` and `target/` and
/// holds both, the workspaces of it, and the root to create them in.
struct Setup {
    fixture: Fixture,
    _state: TempDir,
    root: PathBuf,
    workspaces: Workspaces<Git>,
}

fn setup() -> Setup {
    let fixture = fixture();
    write(
        fixture.clone.join(".git/info/exclude"),
        "node_modules/\ntarget/\n",
    );
    fs::create_dir_all(fixture.clone.join("node_modules/left-pad")).unwrap();
    write(
        fixture.clone.join("node_modules/left-pad/index.js"),
        "module.exports = pad;",
    );
    fs::create_dir_all(fixture.clone.join("target/debug")).unwrap();
    write(fixture.clone.join("target/debug/app"), "binary");
    let state = tempfile::tempdir().unwrap();
    let base = state.path().canonicalize().unwrap();
    Setup {
        workspaces: Workspaces::new(Store::new(base.join("state")), Git),
        root: base.join("mxv"),
        _state: state,
        fixture,
    }
}

fn created(setup: &Setup, mode: Mode) -> Workspace {
    let name: WorkspaceName = "reading-list".parse().unwrap();
    match setup
        .workspaces
        .create(&setup.root, &name, &setup.fixture.clone, mode)
        .unwrap()
    {
        NewOutcome::Created(workspace) | NewOutcome::Planned(workspace) => workspace,
        other => panic!("{other:?}"),
    }
}

fn globs(texts: &[&str]) -> Vec<PathGlob> {
    texts
        .iter()
        .map(|text| PathGlob::parse(text).unwrap())
        .collect()
}

/// Reports whether `cloned` says that the file system cannot clone, in
/// which case the test has nothing more to check.
fn unsupported(cloned: &Cloned) -> bool {
    cloned
        .warning
        .as_deref()
        .is_some_and(|warning| warning.contains(UNSUPPORTED))
}

#[test]
fn a_new_checkout_gets_a_clone_of_the_ignored_node_modules_and_never_target() {
    let setup = setup();
    let workspace = created(&setup, Mode::Change);
    let checkout = &workspace.checkouts[0].path;
    let cloned = setup
        .workspaces
        .clone_ignored(&workspace, &globs(&["*/"]), Mode::Change)
        .unwrap();
    assert!(!checkout.join("target").exists(), "target/ was cloned");
    if unsupported(&cloned) {
        assert!(!checkout.join("node_modules").exists());
        return;
    }
    assert_eq!(cloned.paths, ["app: node_modules/"], "{cloned:?}");
    assert_eq!(cloned.warning, None);
    assert_eq!(
        fs::read_to_string(checkout.join("node_modules/left-pad/index.js")).unwrap(),
        "module.exports = pad;"
    );
}

#[test]
fn a_dry_run_names_the_directories_it_would_clone_and_clones_nothing() {
    let setup = setup();
    let workspace = created(&setup, Mode::DryRun);
    let cloned = setup
        .workspaces
        .clone_ignored(&workspace, &globs(&["node_modules/"]), Mode::DryRun)
        .unwrap();
    assert_eq!(cloned.paths, ["app: node_modules/"]);
    assert_eq!(cloned.warning, None);
    assert!(!workspace.path.exists());
}

#[test]
fn a_directory_that_no_glob_names_is_not_cloned() {
    let setup = setup();
    let workspace = created(&setup, Mode::Change);
    let cloned = setup
        .workspaces
        .clone_ignored(&workspace, &globs(&["web/node_modules/"]), Mode::Change)
        .unwrap();
    assert!(cloned.paths.is_empty(), "{cloned:?}");
    assert!(!workspace.checkouts[0].path.join("node_modules").exists());
}

#[test]
fn a_directory_whose_parent_is_missing_from_the_checkout_is_not_cloned() {
    let setup = setup();
    fs::create_dir_all(setup.fixture.clone.join("scratch/node_modules/x")).unwrap();
    write(setup.fixture.clone.join("scratch/node_modules/x/i.js"), "");
    let workspace = created(&setup, Mode::Change);
    let cloned = setup
        .workspaces
        .clone_ignored(&workspace, &globs(&["node_modules/"]), Mode::Change)
        .unwrap();
    assert!(
        !cloned.paths.iter().any(|path| path.contains("scratch")),
        "{cloned:?}"
    );
    assert!(!workspace.checkouts[0].path.join("scratch").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn a_clone_that_fails_with_enotsup_warns_that_the_file_system_cannot_clone() {
    let error = std::io::Error::from_raw_os_error(libc::ENOTSUP);
    let text = super::warning(std::path::Path::new("/w/app"), "node_modules/", &error);
    assert!(text.contains(super::UNSUPPORTED), "{text}");
}
