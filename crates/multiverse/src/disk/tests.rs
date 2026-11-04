use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tempfile::TempDir;

use crate::git::Git;
use crate::name::WorkspaceName;
use crate::record::Workspace;
use crate::store::Store;
use crate::testing::{Fixture, fixture, git, write};
use crate::workspaces::{Mode, NewOutcome, Workspaces};

use super::DiskUsage;

/// A fixture repository that ignores `.env`, `target/` and
/// `node_modules/`, and one workspace of it.
struct Setup {
    fixture: Fixture,
    _state: TempDir,
    workspaces: Workspaces<Git>,
    workspace: Workspace,
}

impl Setup {
    fn checkout(&self) -> PathBuf {
        self.workspace.checkouts[0].path.clone()
    }

    fn usage(&self) -> DiskUsage {
        self.workspaces.disk_usage(&self.workspace).unwrap()
    }
}

fn setup() -> Setup {
    let fixture = fixture();
    write(
        fixture.clone.join(".git/info/exclude"),
        ".env\ntarget/\nnode_modules/\n",
    );
    let state = tempfile::tempdir().unwrap();
    let base = state.path().canonicalize().unwrap();
    let workspaces = Workspaces::new(Store::new(base.join("state")), Git);
    let name: WorkspaceName = "reading-list".parse().unwrap();
    let NewOutcome::Created(workspace) = workspaces
        .create(&base.join("mxv"), &name, &fixture.clone, Mode::Change)
        .unwrap()
    else {
        panic!("the workspace was not created");
    };
    Setup {
        fixture,
        _state: state,
        workspaces,
        workspace,
    }
}

/// Writes a file of `bytes` bytes of a pattern that no file system
/// compresses away.
fn write_bytes(path: &Path, bytes: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text: Vec<u8> = (0..bytes)
        .map(|at| u8::try_from(at * 7 % 251).unwrap())
        .collect();
    write(path, text);
}

/// Sets the modification time of every file below `dir`, and of `dir`
/// itself, to `time`, without following symbolic links.
fn backdate(dir: &Path, time: SystemTime) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let metadata = fs::symlink_metadata(&path).unwrap();
        if metadata.is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            backdate(&path, time);
        } else {
            fs::File::open(&path).unwrap().set_modified(time).unwrap();
        }
    }
}

/// Backdates the workspace's checkout and the repository's git directory,
/// which holds the checkout's `HEAD`, index and reflog, by `days` days.
fn backdate_all(setup: &Setup, days: u64) -> SystemTime {
    let time = SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60);
    backdate(&setup.workspace.path, time);
    backdate(&setup.fixture.clone.join(".git"), time);
    time
}

#[test]
fn the_disposable_size_counts_the_bytes_of_each_disposable_directory() {
    let setup = setup();
    let checkout = setup.checkout();
    write_bytes(&checkout.join("target/debug/app"), 64 * 1024);
    write_bytes(
        &checkout.join("web/node_modules/left-pad/index.js"),
        32 * 1024,
    );
    write_bytes(&checkout.join("notes.md"), 16 * 1024);
    let usage = setup.usage();
    assert!(usage.disposable_bytes >= 96 * 1024, "{usage:?}");
    assert!(usage.disposable_bytes < 112 * 1024, "{usage:?}");
    assert!(
        usage.bytes >= usage.disposable_bytes + 16 * 1024,
        "{usage:?}"
    );
    let paths: Vec<&str> = usage
        .disposable
        .iter()
        .map(|found| found.path.as_str())
        .collect();
    assert_eq!(paths, ["target/", "web/node_modules/"]);
    assert!(usage.disposable[0].bytes >= 64 * 1024, "{usage:?}");
    assert_eq!(usage.disposable[0].checkout, "app");
}

#[test]
fn a_file_with_two_hard_links_counts_once() {
    let setup = setup();
    let checkout = setup.checkout();
    write_bytes(&checkout.join("target/one"), 256 * 1024);
    let once = setup.usage().disposable_bytes;
    fs::hard_link(checkout.join("target/one"), checkout.join("target/two")).unwrap();
    let linked = setup.usage().disposable_bytes;
    assert!(linked < once + 16 * 1024, "{once} then {linked}");
}

#[test]
fn a_symbolic_link_counts_as_itself_and_its_target_does_not_count() {
    let setup = setup();
    let checkout = setup.checkout();
    let outside = setup.fixture.dir.path().join("outside");
    write_bytes(&outside.join("big"), 512 * 1024);
    let before = setup.usage().bytes;
    std::os::unix::fs::symlink(&outside, checkout.join("target")).unwrap();
    let after = setup.usage();
    assert!(after.bytes < before + 16 * 1024, "{before} then {after:?}");
    // git reports the link as `target`, without a slash, so it is not a
    // disposable directory.
    assert!(after.disposable.is_empty(), "{after:?}");
}

#[test]
fn a_workspace_whose_directory_is_gone_uses_no_bytes() {
    let setup = setup();
    fs::remove_dir_all(&setup.workspace.path).unwrap();
    let usage = setup.usage();
    assert_eq!(usage.bytes, 0);
    assert_eq!(usage.disposable_bytes, 0);
    assert!(usage.disposable.is_empty());
}

#[test]
fn the_last_modification_is_the_newest_file_outside_the_disposable_directories() {
    let setup = setup();
    let checkout = setup.checkout();
    write(checkout.join("notes.md"), "notes");
    write_bytes(&checkout.join("target/debug/app"), 1024);
    let old = backdate_all(&setup, 10);
    write(checkout.join("target/debug/app"), "rebuilt");
    let modified = setup.usage().modified.unwrap();
    assert!(
        modified < old + Duration::from_secs(5),
        "a build inside target/ counted as a change: {modified:?}"
    );
    write(checkout.join("notes.md"), "edited");
    let modified = setup.usage().modified.unwrap();
    assert!(modified > old + Duration::from_secs(60), "{modified:?}");
}

#[test]
fn a_commit_counts_as_a_modification_of_the_checkout() {
    let setup = setup();
    let old = backdate_all(&setup, 10);
    assert!(setup.usage().modified.unwrap() < old + Duration::from_secs(5));
    git(
        &setup.checkout(),
        &["commit", "--quiet", "--allow-empty", "-m", "Work"],
    );
    assert!(setup.usage().modified.unwrap() > old + Duration::from_secs(60));
}

#[test]
fn cleaning_deletes_the_disposable_directories_and_nothing_else() {
    let setup = setup();
    let checkout = setup.checkout();
    write_bytes(&checkout.join("target/debug/app"), 64 * 1024);
    write_bytes(&checkout.join("web/node_modules/left-pad/index.js"), 1024);
    write(checkout.join(".env"), "TOKEN=x");
    write(checkout.join("notes.md"), "untracked notes");
    let planned = setup.usage();
    let cleaned = setup
        .workspaces
        .clean(&setup.workspace, &planned.disposable)
        .unwrap();
    let paths: Vec<&str> = cleaned.iter().map(|found| found.path.as_str()).collect();
    assert_eq!(paths, ["target/", "web/node_modules/"]);
    assert!(cleaned[0].bytes >= 64 * 1024);
    assert!(!checkout.join("target").exists());
    assert!(!checkout.join("web/node_modules").exists());
    assert!(checkout.join("web").exists());
    assert!(checkout.join(".env").exists());
    assert!(checkout.join("notes.md").exists());
    assert!(checkout.join(".git").exists());
    assert_eq!(setup.usage().disposable_bytes, 0);
}

#[test]
fn cleaning_skips_a_planned_directory_that_holds_a_tracked_file_by_then() {
    let setup = setup();
    let checkout = setup.checkout();
    write_bytes(&checkout.join("target/kept"), 1024);
    let planned = setup.usage();
    git(&checkout, &["add", "--force", "target/kept"]);
    let cleaned = setup
        .workspaces
        .clean(&setup.workspace, &planned.disposable)
        .unwrap();
    assert!(cleaned.is_empty(), "{cleaned:?}");
    assert!(checkout.join("target/kept").exists());
}

#[test]
fn cleaning_skips_a_planned_directory_that_became_a_symbolic_link() {
    let setup = setup();
    let checkout = setup.checkout();
    write_bytes(&checkout.join("target/out"), 1024);
    let planned = setup.usage();
    let outside = setup.fixture.dir.path().join("outside");
    write_bytes(&outside.join("precious"), 1024);
    fs::remove_dir_all(checkout.join("target")).unwrap();
    std::os::unix::fs::symlink(&outside, checkout.join("target")).unwrap();
    let cleaned = setup
        .workspaces
        .clean(&setup.workspace, &planned.disposable)
        .unwrap();
    assert!(cleaned.is_empty(), "{cleaned:?}");
    assert!(outside.join("precious").exists());
}
