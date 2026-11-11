use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::*;
use crate::testing::write;

/// A workspace directory with a checkout folder `app`, and an archive
/// directory beside it that does not exist yet.
struct Scene {
    dir: TempDir,
    workspace: PathBuf,
    archive: PathBuf,
}

fn scene() -> Scene {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let workspace = root.join("reading-list");
    fs::create_dir_all(workspace.join("app/config")).unwrap();
    Scene {
        archive: root.join("data/archives/0123/"),
        workspace,
        dir,
    }
}

impl Scene {
    /// Writes `text` at `path` below the workspace with the bits `mode`.
    fn file(&self, path: &str, text: &str, mode: u32) {
        let on_disk = self.workspace.join(path);
        write(&on_disk, text);
        fs::set_permissions(&on_disk, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Returns each path with where it lies on the disk.
    fn sources(&self, paths: &[&str]) -> Vec<(String, PathBuf)> {
        paths
            .iter()
            .map(|path| ((*path).to_owned(), self.workspace.join(path)))
            .collect()
    }

    /// Returns a fresh workspace directory beside the first, with the
    /// checkout folder `app` and nothing else, as restore rebuilds it.
    fn rebuilt(&self) -> PathBuf {
        let rebuilt = self.dir.path().canonicalize().unwrap().join("rebuilt");
        fs::create_dir_all(rebuilt.join("app")).unwrap();
        rebuilt
    }
}

fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

fn put_back(found: Result<Result<IgnoredRestored, IgnoredRefusal>, Error>) -> IgnoredRestored {
    match found {
        Ok(Ok(put_back)) => put_back,
        Ok(Err(refusal)) => panic!("restore refused: {refusal:?}"),
        Err(error) => panic!("restore failed: {error}"),
    }
}

#[test]
fn a_kept_dot_env_is_written_back_with_its_bytes_and_its_mode() {
    let scene = scene();
    scene.file("app/.env", "TOKEN=abc\n", 0o640);
    scene.file("app/config/master.key", "0123456789", 0o600);
    let kept = keep(
        &scene.archive,
        &scene.sources(&["app/.env", "app/config/master.key"]),
    )
    .unwrap();
    let rebuilt = scene.rebuilt();

    let restored = put_back(restore(&scene.archive, &kept, &rebuilt, false));

    assert_eq!(restored.written, ["app/.env", "app/config/master.key"]);
    assert!(restored.lost.is_empty());
    assert_eq!(
        fs::read_to_string(rebuilt.join("app/.env")).unwrap(),
        "TOKEN=abc\n"
    );
    assert_eq!(mode_of(&rebuilt.join("app/.env")), 0o640);
    assert_eq!(
        fs::read_to_string(rebuilt.join("app/config/master.key")).unwrap(),
        "0123456789"
    );
    assert_eq!(mode_of(&rebuilt.join("app/config/master.key")), 0o600);
}

#[test]
fn the_record_of_each_kept_file_names_its_kind_size_mode_and_digest() {
    let scene = scene();
    scene.file("app/.env", "TOKEN=abc\n", 0o640);

    let kept = keep(&scene.archive, &scene.sources(&["app/.env"])).unwrap();

    assert_eq!(
        kept,
        [IgnoredEntry {
            path: "app/.env".to_owned(),
            kind: IgnoredKind::File,
            size: 10,
            mode: 0o640,
            digest: crate::store::file_digest(&scene.workspace.join("app/.env")).unwrap(),
        }]
    );
}

#[test]
fn the_ignored_tarball_and_its_directory_are_readable_only_by_the_user() {
    let scene = scene();
    scene.file("app/.env", "TOKEN=abc\n", 0o644);

    keep(&scene.archive, &scene.sources(&["app/.env"])).unwrap();

    assert_eq!(mode_of(&scene.archive), 0o700);
    assert_eq!(mode_of(&scene.archive.join(IGNORED_TAR)), 0o600);
}

#[test]
fn an_ignored_symbolic_link_is_kept_as_a_link() {
    let scene = scene();
    scene.file("app/.env.production", "TOKEN=prod\n", 0o600);
    std::os::unix::fs::symlink(".env.production", scene.workspace.join("app/.env")).unwrap();
    let kept = keep(&scene.archive, &scene.sources(&["app/.env"])).unwrap();
    let rebuilt = scene.rebuilt();

    put_back(restore(&scene.archive, &kept, &rebuilt, false));

    assert_eq!(
        kept.first().map(|entry| entry.kind),
        Some(IgnoredKind::Link)
    );
    assert_eq!(
        fs::read_link(rebuilt.join("app/.env")).unwrap(),
        Path::new(".env.production")
    );
}

#[test]
fn a_rerun_after_a_partial_tar_writes_it_again_and_finishes() {
    let scene = scene();
    scene.file("app/.env", "TOKEN=abc\n", 0o600);
    crate::store::private_dir(&scene.archive).unwrap();
    let stale = scene.archive.join(format!(".{IGNORED_TAR}.99999.tmp"));
    write(&stale, "half a tar");

    let kept = keep(&scene.archive, &scene.sources(&["app/.env"])).unwrap();

    assert!(!stale.exists(), "the stale temporary file is still there");
    let rebuilt = scene.rebuilt();
    let restored = put_back(restore(&scene.archive, &kept, &rebuilt, false));
    assert_eq!(restored.written, ["app/.env"]);
}

/// Keeps `app/.env` and `app/config/master.key`, retires the checkout `app`
/// with a record of master.key that no longer matches the tar, and reruns
/// the keep with `web/.env` as its source, carrying app's entries.
fn rerun_after_a_retired_checkout(scene: &Scene) -> (Vec<IgnoredEntry>, Vec<IgnoredEntry>) {
    scene.file("app/.env", "TOKEN=abc\n", 0o640);
    scene.file("app/config/master.key", "0123456789", 0o600);
    let first = keep(
        &scene.archive,
        &scene.sources(&["app/.env", "app/config/master.key"]),
    )
    .unwrap();
    // The checkout app is retired, so its files exist only in the tar. The
    // record of master.key no longer matches the tar, as a damaged entry.
    let mut carried = first.clone();
    carried[1].digest = "0".repeat(64);
    fs::remove_dir_all(scene.workspace.join("app")).unwrap();
    fs::create_dir_all(scene.workspace.join("web")).unwrap();
    scene.file("web/.env", "TOKEN=web\n", 0o600);
    let kept = keep_carrying(&scene.archive, &scene.sources(&["web/.env"]), &carried).unwrap();
    (carried, kept)
}

#[test]
fn a_rerun_carries_the_whole_entries_of_a_retired_checkout_into_the_new_tar() {
    let scene = scene();
    let (carried, kept) = rerun_after_a_retired_checkout(&scene);

    assert_eq!(
        kept.iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        ["web/.env", "app/.env", "app/config/master.key"],
        "the sources come first, then the whole carried entries, then the damaged ones"
    );
    assert_eq!(kept[1..], carried[..]);
    let rebuilt = scene.rebuilt();
    let restored = put_back(restore(&scene.archive, &kept, &rebuilt, true));
    assert_eq!(restored.written, ["web/.env", "app/.env"]);
    assert_eq!(
        fs::read_to_string(rebuilt.join("app/.env")).unwrap(),
        "TOKEN=abc\n"
    );
    assert_eq!(mode_of(&rebuilt.join("app/.env")), 0o640);
    assert_eq!(
        fs::read_to_string(rebuilt.join("web/.env")).unwrap(),
        "TOKEN=web\n"
    );
    assert_eq!(restored.lost, ["app/config/master.key"]);
}

#[test]
fn a_restore_after_a_rerun_refuses_with_a_carried_entry_the_old_tar_held_damaged() {
    let scene = scene();
    let (_, kept) = rerun_after_a_retired_checkout(&scene);
    let rebuilt = scene.rebuilt();

    let refused = restore(&scene.archive, &kept, &rebuilt, false).unwrap();

    assert_eq!(
        refused.unwrap_err(),
        IgnoredRefusal::Damaged(vec!["app/config/master.key".to_owned()])
    );
    assert!(!rebuilt.join("app/.env").exists(), "restore wrote a file");
}

#[test]
fn an_ignored_file_edited_after_the_tar_is_named_as_changed() {
    let scene = scene();
    scene.file("app/.env", "TOKEN=abc\n", 0o600);
    scene.file("app/config/master.key", "0123456789", 0o600);
    let sources = scene.sources(&["app/.env", "app/config/master.key"]);
    let kept = keep(&scene.archive, &sources).unwrap();
    assert!(changed(&kept, &sources).unwrap().is_empty());

    scene.file("app/.env", "TOKEN=xyz\n", 0o600);
    fs::set_permissions(
        scene.workspace.join("app/config/master.key"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();

    assert_eq!(
        changed(&kept, &sources).unwrap(),
        ["app/.env", "app/config/master.key"]
    );
}

#[test]
fn a_damaged_tarball_refuses_restore_and_names_the_entries() {
    let scene = scene();
    scene.file("app/.env", "TOKEN=abc\n", 0o600);
    scene.file("app/config/master.key", "0123456789", 0o600);
    let mut kept = keep(
        &scene.archive,
        &scene.sources(&["app/.env", "app/config/master.key"]),
    )
    .unwrap();
    // The record's digest of master.key no longer matches the tar's bytes.
    kept.get_mut(1).unwrap().digest = "0".repeat(64);
    let rebuilt = scene.rebuilt();

    let refused = restore(&scene.archive, &kept, &rebuilt, false).unwrap();

    assert_eq!(
        refused.unwrap_err(),
        IgnoredRefusal::Damaged(vec!["app/config/master.key".to_owned()])
    );
    assert!(!rebuilt.join("app/.env").exists(), "restore wrote a file");
}

#[test]
fn a_missing_tarball_refuses_restore_and_names_every_entry() {
    let scene = scene();
    scene.file("app/.env", "TOKEN=abc\n", 0o600);
    let kept = keep(&scene.archive, &scene.sources(&["app/.env"])).unwrap();
    fs::remove_file(scene.archive.join(IGNORED_TAR)).unwrap();

    let refused = restore(&scene.archive, &kept, &scene.rebuilt(), false).unwrap();

    assert_eq!(
        refused.unwrap_err(),
        IgnoredRefusal::Damaged(vec!["app/.env".to_owned()])
    );
}

#[test]
fn a_forced_restore_writes_the_whole_entries_and_lists_the_damaged_ones_as_lost() {
    let scene = scene();
    scene.file("app/.env", "TOKEN=abc\n", 0o600);
    scene.file("app/config/master.key", "0123456789", 0o600);
    let mut kept = keep(
        &scene.archive,
        &scene.sources(&["app/.env", "app/config/master.key"]),
    )
    .unwrap();
    kept.get_mut(1).unwrap().digest = "0".repeat(64);
    let rebuilt = scene.rebuilt();

    let restored = put_back(restore(&scene.archive, &kept, &rebuilt, true));

    assert_eq!(restored.written, ["app/.env"]);
    assert_eq!(restored.lost, ["app/config/master.key"]);
    assert!(!rebuilt.join("app/config/master.key").exists());
}

#[test]
fn restore_refuses_to_overwrite_an_ignored_path_the_checkout_already_holds() {
    let scene = scene();
    scene.file("app/.env", "TOKEN=abc\n", 0o600);
    scene.file("app/config/master.key", "0123456789", 0o600);
    let kept = keep(
        &scene.archive,
        &scene.sources(&["app/.env", "app/config/master.key"]),
    )
    .unwrap();
    let rebuilt = scene.rebuilt();
    write(rebuilt.join("app/.env"), "TOKEN=committed\n");

    let refused = restore(&scene.archive, &kept, &rebuilt, true).unwrap();

    assert_eq!(
        refused.unwrap_err(),
        IgnoredRefusal::Taken(vec!["app/.env".to_owned()])
    );
    assert_eq!(
        fs::read_to_string(rebuilt.join("app/.env")).unwrap(),
        "TOKEN=committed\n"
    );
    assert!(!rebuilt.join("app/config/master.key").exists());
}

#[test]
fn restore_refuses_a_path_whose_parent_in_the_checkout_is_a_symbolic_link() {
    let scene = scene();
    scene.file("app/config/master.key", "0123456789", 0o600);
    let kept = keep(&scene.archive, &scene.sources(&["app/config/master.key"])).unwrap();
    let rebuilt = scene.rebuilt();
    let elsewhere = scene.dir.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, rebuilt.join("app/config")).unwrap();

    let refused = restore(&scene.archive, &kept, &rebuilt, false).unwrap();

    assert_eq!(
        refused.unwrap_err(),
        IgnoredRefusal::Taken(vec!["app/config/master.key".to_owned()])
    );
    assert!(!elsewhere.join("master.key").exists());
}

#[test]
fn a_recorded_path_that_leaves_the_workspace_is_refused_as_damaged() {
    let scene = scene();
    scene.file("app/.env", "TOKEN=abc\n", 0o600);
    let mut kept = keep(&scene.archive, &scene.sources(&["app/.env"])).unwrap();
    kept.get_mut(0).unwrap().path = "../outside".to_owned();

    let refused = restore(&scene.archive, &kept, &scene.rebuilt(), false).unwrap();

    assert_eq!(
        refused.unwrap_err(),
        IgnoredRefusal::Damaged(vec!["../outside".to_owned()])
    );
}

#[test]
fn a_path_longer_than_a_tar_header_holds_is_kept() {
    let scene = scene();
    let deep = format!("app/{}/.env", "nested-directory-name".repeat(6));
    fs::create_dir_all(scene.workspace.join(&deep).parent().unwrap()).unwrap();
    scene.file(&deep, "TOKEN=abc\n", 0o600);
    let kept = keep(&scene.archive, &scene.sources(&[deep.as_str()])).unwrap();
    let rebuilt = scene.rebuilt();

    let restored = put_back(restore(&scene.archive, &kept, &rebuilt, false));

    assert_eq!(restored.written, std::slice::from_ref(&deep));
    assert_eq!(
        fs::read_to_string(rebuilt.join(&deep)).unwrap(),
        "TOKEN=abc\n"
    );
}
