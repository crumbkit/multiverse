use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tempfile::TempDir;

use crate::git::Git;
use crate::name::WorkspaceName;
use crate::record::Workspace;
use crate::store::Store;
use crate::testing::{Fixture, fixture, git, write};
use crate::workspaces::{Mode, NewOutcome, Workspaces};

use crate::disposable::DisposableSource;
use crate::error::Error;

use super::{Cleaned, DiskUsage, DisposablePath};

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

    /// Cleans `planned` from the workspace, which is idle.
    fn clean(&self, planned: &[DisposablePath]) -> Cleaned {
        self.workspaces
            .clean(&self.workspace, planned, |_| Ok::<_, Error>(None))
            .unwrap()
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
    // Each of the four directories takes a block of its own on most file
    // systems.
    assert!(usage.disposable_bytes <= 128 * 1024, "{usage:?}");
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

// The file system stamps times from a coarse clock, which can lag
// `SystemTime::now()` on Linux, so these tests compare one measurement with
// another, with a git command or more between them.

/// Returns when the workspace of `setup` last changed.
fn modified(setup: &Setup) -> SystemTime {
    setup.usage().modified.unwrap()
}

#[test]
fn the_last_modification_is_the_newest_change_outside_the_disposable_directories() {
    let setup = setup();
    let checkout = setup.checkout();
    write(checkout.join("notes.md"), "notes");
    write_bytes(&checkout.join("target/debug/app"), 1024);
    let first = modified(&setup);
    write(checkout.join("target/debug/app"), "rebuilt");
    assert_eq!(
        modified(&setup),
        first,
        "a build inside target/ counted as a change"
    );
    write(checkout.join("notes.md"), "edited");
    assert!(modified(&setup) > first);
}

#[test]
fn a_deleted_file_counts_as_a_change_of_the_checkout() {
    let setup = setup();
    let checkout = setup.checkout();
    fs::create_dir_all(checkout.join("docs")).unwrap();
    write(checkout.join("docs/old.md"), "old");
    let first = modified(&setup);
    fs::remove_file(checkout.join("docs/old.md")).unwrap();
    assert!(modified(&setup) > first);
}

#[test]
fn a_restored_modification_time_still_counts_as_a_change() {
    let setup = setup();
    let checkout = setup.checkout();
    write(checkout.join("notes.md"), "notes");
    let first = modified(&setup);
    let file = fs::File::options()
        .append(true)
        .open(checkout.join("notes.md"))
        .unwrap();
    file.set_modified(SystemTime::UNIX_EPOCH).unwrap();
    assert!(modified(&setup) > first);
}

#[test]
fn a_commit_counts_as_a_modification_of_the_checkout() {
    let setup = setup();
    let first = modified(&setup);
    git(
        &setup.checkout(),
        &["commit", "--quiet", "--allow-empty", "-m", "Work"],
    );
    assert!(modified(&setup) > first);
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
    let cleaned = setup.clean(&planned.disposable).deleted;
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
    let cleaned = setup.clean(&planned.disposable);
    assert!(cleaned.deleted.is_empty(), "{cleaned:?}");
    assert_eq!(cleaned.kept.len(), 1, "{cleaned:?}");
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
    let cleaned = setup.clean(&planned.disposable);
    assert!(cleaned.deleted.is_empty(), "{cleaned:?}");
    assert_eq!(cleaned.kept.len(), 1, "{cleaned:?}");
    assert!(outside.join("precious").exists());
}

#[test]
fn a_disposable_directory_that_holds_a_git_entry_is_kept_with_a_reason() {
    let setup = setup();
    let checkout = setup.checkout();
    write_bytes(&checkout.join("target/debug/app"), 1024);
    fs::create_dir_all(checkout.join("node_modules/fork/.git")).unwrap();
    write(
        checkout.join("node_modules/fork/.git/HEAD"),
        "ref: refs/heads/main\n",
    );
    let usage = setup.usage();
    let paths: Vec<&str> = usage
        .disposable
        .iter()
        .map(|found| found.path.as_str())
        .collect();
    assert_eq!(paths, ["target/"]);
    assert_eq!(usage.kept.len(), 1, "{usage:?}");
    assert_eq!(usage.kept[0].path, "node_modules/");
    assert!(
        usage.kept[0].reason.contains("node_modules/fork/.git"),
        "{usage:?}"
    );
}

#[test]
fn cleaning_keeps_a_planned_directory_that_gained_a_git_entry_by_then() {
    let setup = setup();
    let checkout = setup.checkout();
    write_bytes(&checkout.join("target/debug/app"), 1024);
    let planned = setup.usage();
    fs::create_dir_all(checkout.join("target/vendored")).unwrap();
    write(checkout.join("target/vendored/.git"), "gitdir: elsewhere\n");
    let cleaned = setup.clean(&planned.disposable);
    assert!(cleaned.deleted.is_empty(), "{cleaned:?}");
    assert!(cleaned.kept[0].reason.contains(".git"), "{cleaned:?}");
    assert!(checkout.join("target/vendored/.git").exists());
}

#[test]
fn cleaning_deletes_nothing_when_the_check_finds_the_workspace_busy() {
    let setup = setup();
    let checkout = setup.checkout();
    write_bytes(&checkout.join("target/debug/app"), 1024);
    let planned = setup.usage();
    let cleaned = setup
        .workspaces
        .clean(&setup.workspace, &planned.disposable, |_| {
            Ok::<_, Error>(Some("its process web runs".to_owned()))
        })
        .unwrap();
    assert_eq!(cleaned.busy.as_deref(), Some("its process web runs"));
    assert!(cleaned.deleted.is_empty());
    assert!(checkout.join("target/debug/app").exists());
}

#[test]
fn a_failed_deletion_names_the_entry_that_failed_and_the_run_goes_on() {
    use std::os::unix::fs::PermissionsExt as _;
    let setup = setup();
    let checkout = setup.checkout();
    write_bytes(&checkout.join("target/locked/app"), 1024);
    write_bytes(&checkout.join("target/loose/app"), 1024);
    write_bytes(&checkout.join("web/node_modules/left-pad/index.js"), 1024);
    let locked = checkout.join("target/locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    let planned = setup.usage();
    let cleaned = setup.clean(&planned.disposable);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(cleaned.failed.len(), 1, "{cleaned:?}");
    let failed = &cleaned.failed[0];
    assert_eq!(failed.path, "target/");
    assert_eq!(failed.failed, locked.join("app"));
    assert!(failed.bytes > 0, "{cleaned:?}");
    assert!(!checkout.join("target/loose").exists());
    let deleted: Vec<&str> = cleaned
        .deleted
        .iter()
        .map(|found| found.path.as_str())
        .collect();
    assert_eq!(deleted, ["web/node_modules/"]);
}

#[test]
fn a_file_with_a_hard_link_outside_the_deleted_directory_frees_nothing() {
    let setup = setup();
    let checkout = setup.checkout();
    let store = setup.fixture.dir.path().join("store/package.js");
    write_bytes(&store, 64 * 1024);
    fs::create_dir_all(checkout.join("node_modules/package")).unwrap();
    fs::hard_link(&store, checkout.join("node_modules/package/index.js")).unwrap();
    let planned = setup.usage();
    assert!(planned.disposable[0].bytes >= 64 * 1024, "{planned:?}");
    let cleaned = setup.clean(&planned.disposable);
    assert!(cleaned.deleted[0].bytes < 64 * 1024, "{cleaned:?}");
    assert!(store.exists());
}

#[test]
fn an_invalid_manifest_falls_back_to_the_built_in_set_with_a_warning() {
    let setup = setup();
    let checkout = setup.checkout();
    write(checkout.join("mxv.toml"), "disposable = [\"*\"]\n");
    write(checkout.join(".env"), "TOKEN=x");
    let usage = setup.usage();
    assert!(usage.disposable.is_empty(), "{usage:?}");
    assert_eq!(usage.warnings.len(), 1, "{usage:?}");
    assert!(usage.warnings[0].contains("mxv.toml"), "{usage:?}");
}

#[test]
fn measuring_a_checkout_does_not_count_as_a_change_of_it() {
    let setup = setup();
    let checkout = setup.checkout();
    write(checkout.join("notes.md"), "notes");
    git(&checkout, &["add", "notes.md"]);
    git(&checkout, &["commit", "--quiet", "-m", "Add notes"]);
    let first = modified(&setup);
    assert_eq!(modified(&setup), first);
}

#[test]
fn a_directory_swapped_for_a_symbolic_link_during_the_deletion_is_never_followed() {
    let setup = setup();
    let checkout = setup.checkout();
    let tree = checkout.join("target/b");
    for index in 0..20 {
        write_bytes(&tree.join(format!("file-{index}")), 16);
    }
    let decoy = setup.fixture.dir.path().join("decoy");
    for index in 0..20 {
        // The same names, so a deletion by path through the link would
        // find and delete them.
        write_bytes(&decoy.join(format!("file-{index}")), 16);
    }
    let mut swapped = false;
    let result = super::tree::remove(&checkout, "target/", &mut |_| {
        if !swapped {
            swapped = true;
            #[expect(
                clippy::disallowed_methods,
                reason = "the test swaps a directory for a link, as another process would"
            )]
            fs::rename(&tree, checkout.join("target/b.old")).unwrap();
            std::os::unix::fs::symlink(&decoy, &tree).unwrap();
        }
    });
    assert!(swapped);
    assert!(result.is_err(), "{result:?}");
    let left = fs::read_dir(&decoy).unwrap().count();
    assert_eq!(left, 20, "the deletion followed the link into the decoy");
}

#[test]
fn a_git_entry_in_any_case_marks_a_nested_repository() {
    let setup = setup();
    let checkout = setup.checkout();
    fs::create_dir_all(checkout.join("node_modules/fork/.GIT")).unwrap();
    let usage = setup.usage();
    assert!(usage.disposable.is_empty(), "{usage:?}");
    assert!(usage.kept[0].reason.contains(".GIT"), "{usage:?}");
}

#[test]
fn a_directory_that_looks_like_a_bare_repository_is_kept() {
    let setup = setup();
    let checkout = setup.checkout();
    let bare = checkout.join("target/mirror.git");
    git(
        &checkout,
        &["init", "--quiet", "--bare", "target/mirror.git"],
    );
    assert!(bare.join("HEAD").exists());
    let usage = setup.usage();
    assert!(usage.disposable.is_empty(), "{usage:?}");
    assert!(
        usage.kept[0].reason.contains("target/mirror.git/"),
        "{usage:?}"
    );
}

#[test]
fn the_disk_usage_marks_a_path_that_only_the_manifest_makes_disposable() {
    let setup = setup();
    let checkout = setup.checkout();
    write(
        setup.fixture.clone.join(".git/info/exclude"),
        ".env\ntarget/\nnode_modules/\ndata/\n",
    );
    write(checkout.join("mxv.toml"), "disposable = [\"data/\"]\n");
    write_bytes(&checkout.join("data/pg.db"), 1024);
    write_bytes(&checkout.join("target/debug/app"), 1024);
    let usage = setup.usage();
    let sources: Vec<(&str, DisposableSource)> = usage
        .disposable
        .iter()
        .map(|found| (found.path.as_str(), found.source))
        .collect();
    assert_eq!(
        sources,
        [
            ("data/", DisposableSource::Manifest),
            ("target/", DisposableSource::BuiltIn)
        ]
    );
}

#[test]
fn a_parent_component_swapped_for_a_symbolic_link_before_the_deletion_is_never_followed() {
    let setup = setup();
    let checkout = setup.checkout();
    write_bytes(&checkout.join("web/node_modules/left-pad/index.js"), 16);
    let decoy = setup.fixture.dir.path().join("decoyweb");
    write_bytes(&decoy.join("node_modules/left-pad/index.js"), 16);
    #[expect(
        clippy::disallowed_methods,
        reason = "the test swaps a directory for a link, as another process would"
    )]
    fs::rename(checkout.join("web"), checkout.join("web.old")).unwrap();
    std::os::unix::fs::symlink(&decoy, checkout.join("web")).unwrap();
    let result = super::tree::remove(&checkout, "web/node_modules/", &mut |_| {});
    assert!(result.is_err(), "{result:?}");
    assert!(decoy.join("node_modules/left-pad/index.js").exists());
}

#[test]
fn a_parent_component_swapped_during_the_deletion_leaves_the_link_target_alone() {
    let setup = setup();
    let checkout = setup.checkout();
    for index in 0..20 {
        write_bytes(&checkout.join(format!("web/node_modules/file-{index}")), 16);
    }
    let decoy = setup.fixture.dir.path().join("decoyweb");
    for index in 0..20 {
        write_bytes(&decoy.join(format!("node_modules/file-{index}")), 16);
    }
    let mut swapped = false;
    let _ = super::tree::remove(&checkout, "web/node_modules/", &mut |_| {
        if !swapped {
            swapped = true;
            #[expect(
                clippy::disallowed_methods,
                reason = "the test swaps a directory for a link, as another process would"
            )]
            fs::rename(checkout.join("web"), checkout.join("web.old")).unwrap();
            std::os::unix::fs::symlink(&decoy, checkout.join("web")).unwrap();
        }
    });
    assert!(swapped);
    let left = fs::read_dir(decoy.join("node_modules")).unwrap().count();
    assert_eq!(left, 20, "the deletion followed the link into the decoy");
}

/// Returns the real volume of each directory, except that a directory
/// named `mnt` looks like the mount point of another file system.
fn mounted_at_mnt(
    dir: &std::os::fd::OwnedFd,
    name: &std::ffi::OsStr,
    stat: &rustix::fs::Stat,
) -> super::volume::Volume {
    let real = super::volume::of(dir, name, stat);
    if name == "mnt" {
        super::volume::Volume {
            mount: real.mount.map(|mount| mount.wrapping_add(1)),
            device: if real.mount.is_some() {
                real.device
            } else {
                rustix::fs::Dev::MAX
            },
        }
    } else {
        real
    }
}

/// Returns the real volume of each directory, except that a directory
/// named `target` looks like the mount point of another file system.
fn mounted_at_target(
    dir: &std::os::fd::OwnedFd,
    name: &std::ffi::OsStr,
    stat: &rustix::fs::Stat,
) -> super::volume::Volume {
    let real = super::volume::of(dir, name, stat);
    if name == "target" {
        super::volume::Volume {
            device: rustix::fs::Dev::MAX,
            mount: None,
        }
    } else {
        real
    }
}

#[test]
fn a_directory_on_another_file_system_below_the_path_is_kept_and_never_deleted() {
    let setup = setup();
    let checkout = setup.checkout();
    write_bytes(&checkout.join("target/mnt/image"), 16);
    write_bytes(&checkout.join("target/debug/app"), 16);
    let reason = super::tree::keep_reason_with(&checkout, "target/", mounted_at_mnt).unwrap();
    assert!(reason.contains("target/mnt"), "{reason}");
    assert!(reason.contains("mount point"), "{reason}");
    let result = super::tree::remove_with(&checkout, "target/", mounted_at_mnt, &mut |_| {});
    let (failed, error) = result.unwrap_err();
    assert_eq!(failed, checkout.join("target/mnt"));
    assert!(error.to_string().contains("mount point"), "{error}");
    assert!(checkout.join("target/mnt/image").exists());
    assert!(!checkout.join("target/debug").exists());
}

#[test]
fn a_disposable_path_that_is_itself_a_mount_point_is_kept_and_never_deleted() {
    let setup = setup();
    let checkout = setup.checkout();
    write_bytes(&checkout.join("target/debug/app"), 16);
    let reason = super::tree::keep_reason_with(&checkout, "target/", mounted_at_target).unwrap();
    assert!(reason.contains("mount point"), "{reason}");
    assert!(reason.contains("target/"), "{reason}");
    let result = super::tree::remove_with(&checkout, "target/", mounted_at_target, &mut |_| {});
    let (failed, error) = result.unwrap_err();
    assert_eq!(failed, checkout.join("target"));
    assert!(error.to_string().contains("mount point"), "{error}");
    assert!(checkout.join("target/debug/app").exists());
}

#[test]
fn a_disposable_path_on_the_checkouts_own_file_system_is_not_kept_for_its_mount() {
    let setup = setup();
    let checkout = setup.checkout();
    write_bytes(&checkout.join("target/debug/app"), 16);
    assert_eq!(super::tree::keep_reason(&checkout, "target/"), None);
}

/// Creates `depth` directories named `name` inside each other below `dir`
/// by file descriptor, which no path limit stops, and returns the deepest.
fn nest(dir: &Path, name: &str, depth: usize) -> std::os::fd::OwnedFd {
    use rustix::fs::{CWD, Mode, OFlags, mkdirat, openat};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let mut fd = openat(CWD, dir, flags, Mode::empty()).unwrap();
    for _ in 0..depth {
        mkdirat(&fd, name, Mode::from_raw_mode(0o755)).unwrap();
        fd = openat(&fd, name, flags, Mode::empty()).unwrap();
    }
    fd
}

#[test]
fn a_repository_beyond_the_path_limit_is_still_found_and_kept() {
    let setup = setup();
    let checkout = setup.checkout();
    fs::create_dir_all(checkout.join("node_modules")).unwrap();
    // 60 names of 60 bytes make a path of about 3,600 bytes, beyond the
    // 1,024 bytes of PATH_MAX on macOS.
    let deepest = nest(&checkout.join("node_modules"), &"n".repeat(60), 60);
    rustix::fs::mkdirat(&deepest, ".git", rustix::fs::Mode::from_raw_mode(0o755)).unwrap();
    let reason = super::tree::keep_reason(&checkout, "node_modules/").unwrap();
    assert!(reason.contains(".git"), "{reason}");
}

#[test]
fn a_tree_deeper_than_the_limit_is_kept_with_a_reason() {
    let setup = setup();
    let checkout = setup.checkout();
    fs::create_dir_all(checkout.join("target")).unwrap();
    nest(&checkout.join("target"), "d", super::tree::MOST_DEPTH + 5);
    let usage = setup.usage();
    assert!(usage.disposable.is_empty(), "{usage:?}");
    assert!(usage.kept[0].reason.contains("deeper than"), "{usage:?}");
    let result = super::tree::remove(&checkout, "target/", &mut |_| {});
    let (_, error) = result.unwrap_err();
    assert!(error.to_string().contains("deeper than"), "{error}");
}
