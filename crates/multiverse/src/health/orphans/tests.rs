use std::fs;
use std::path::Path;

use super::{CHANGED, OrphanKind, Swept, UNRECORDED};
use crate::archive::{Archive, ArchiveState, Archives};
use crate::git::Git;
use crate::health::tests::{archive, workspace};
use crate::runs::Runs;
use crate::store::Store;
use crate::testing::write;
use crate::workspaces::Workspaces;

/// The workspace records, archives and run files of one temporary
/// directory.
struct Setup {
    root: tempfile::TempDir,
    workspaces: Workspaces<Git>,
    archives: Archives,
    runs: Runs,
}

impl Setup {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        Self {
            workspaces: Workspaces::new(Store::new(state.join("workspaces")), Git),
            archives: Archives::new(state.join("archives"), root.path().join("data")),
            runs: Runs::new(&state),
            root,
        }
    }

    fn path(&self, relative: &str) -> std::path::PathBuf {
        self.root.path().join(relative)
    }

    /// Writes the record of `archive` where [`Archives::list`] reads it.
    fn write_archive(&self, archive: &Archive) {
        let dir = self.path("state/archives").join(archive.name.as_str());
        fs::create_dir_all(&dir).unwrap();
        write(
            dir.join("archive.json"),
            serde_json::to_vec_pretty(archive).unwrap(),
        );
    }
}

/// Makes the directory `path` with one file in it.
fn plant(path: &Path) {
    fs::create_dir_all(path).unwrap();
    write(path.join("file"), b"x");
}

#[test]
fn a_data_directory_that_the_fresh_listing_no_longer_holds_is_kept() {
    let setup = Setup::new();
    let data = setup.path("data/0123456789abcdef0123456789abcdef");
    plant(&data);
    let found = setup
        .workspaces
        .orphans(&setup.archives, &setup.runs, false)
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].kind, OrphanKind::IgnoredFiles);
    // Another command deletes the directory between the listing and the
    // sweep, so the fresh listing no longer holds it.
    fs::remove_dir_all(&data).unwrap();

    let swept = setup
        .workspaces
        .sweep_orphan(&setup.archives, &setup.runs, &found[0], false)
        .unwrap();
    assert!(
        matches!(&swept, Swept::Kept(reasons) if reasons == &[CHANGED]),
        "{swept:?}"
    );
}

#[test]
fn a_data_directory_that_a_record_names_again_is_kept() {
    let setup = Setup::new();
    let record = archive("docs", 'a', ArchiveState::Archived);
    let data = setup.path("data").join(&record.id);
    plant(&data);
    let found = setup
        .workspaces
        .orphans(&setup.archives, &setup.runs, false)
        .unwrap();
    assert_eq!(found.len(), 1);
    // An archive writes the record that names the directory between the
    // listing and the sweep.
    setup.write_archive(&record);

    let swept = setup
        .workspaces
        .sweep_orphan(&setup.archives, &setup.runs, &found[0], false)
        .unwrap();
    assert!(
        matches!(&swept, Swept::Kept(reasons) if reasons == &[CHANGED]),
        "{swept:?}"
    );
    assert!(data.join("file").exists());
}

#[test]
fn a_directory_of_the_archives_without_its_own_record_is_listed_and_never_deleted() {
    let setup = Setup::new();
    let other = archive("other", 'b', ArchiveState::Archived);
    let stray = setup.path("state/archives/.docs.0123");
    plant(&stray);
    // A hidden directory that holds another archive's record.
    let copied = setup.path("state/archives/.copy");
    plant(&copied);
    write(
        copied.join("archive.json"),
        serde_json::to_vec_pretty(&other).unwrap(),
    );
    // A running archive makes its directory before it writes its record.
    let started = setup.path("state/archives/notes");
    plant(&started);

    let found = setup
        .workspaces
        .orphans(&setup.archives, &setup.runs, false)
        .unwrap();
    let paths: Vec<_> = found.iter().map(|orphan| orphan.path.clone()).collect();
    assert_eq!(paths, [copied.clone(), stray.clone(), started.clone()]);
    for orphan in &found {
        assert_eq!(orphan.kind, OrphanKind::Unrecorded);
        assert!(orphan.bytes > 0);
        let swept = setup
            .workspaces
            .sweep_orphan(&setup.archives, &setup.runs, orphan, true)
            .unwrap();
        assert!(
            matches!(&swept, Swept::Kept(reasons) if reasons == &[UNRECORDED]),
            "{swept:?}"
        );
    }
    assert!(stray.join("file").exists() && copied.join("file").exists());
    assert!(started.join("file").exists());
}

#[test]
fn run_files_of_a_name_without_a_record_are_listed_and_deleted() {
    let setup = Setup::new();
    let gone = setup.path("state/runs/gone");
    plant(&gone);
    // A name with only the lock file, which any command of the name
    // leaves, a live workspace, and an archive that has not finished keep
    // their run files.
    let locked = setup.path("state/runs/locked");
    fs::create_dir_all(&locked).unwrap();
    write(locked.join(".lock"), b"");
    let mut live = workspace(&setup.path("mxv"), "live", 'c', &["app"]);
    setup.workspaces.store.create(&mut live).unwrap();
    plant(&setup.path("state/runs/live"));
    setup.write_archive(&archive("retiring", 'd', ArchiveState::Retiring));
    plant(&setup.path("state/runs/retiring"));

    let found = setup
        .workspaces
        .orphans(&setup.archives, &setup.runs, false)
        .unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        found[0].kind,
        OrphanKind::RunFiles {
            name: "gone".parse().unwrap()
        }
    );
    assert_eq!(found[0].path, gone);
    let swept = setup
        .workspaces
        .sweep_orphan(&setup.archives, &setup.runs, &found[0], false)
        .unwrap();
    assert!(
        matches!(&swept, Swept::Deleted { volumes } if volumes.is_empty()),
        "{swept:?}"
    );
    assert!(!gone.exists());
    assert!(setup.path("state/runs/live/file").exists());
    assert!(setup.path("state/runs/retiring/file").exists());
}

#[test]
fn logs_of_an_id_that_no_record_holds_are_listed_and_deleted() {
    let setup = Setup::new();
    let orphaned = "e".repeat(32);
    let logs = setup.path("state/phase/logs").join(&orphaned);
    plant(&logs);
    // The id of a live workspace keeps its logs.
    let mut live = workspace(&setup.path("mxv"), "live", 'f', &["app"]);
    setup.workspaces.store.create(&mut live).unwrap();
    let kept = setup.path("state/phase/logs").join("f".repeat(32));
    plant(&kept);

    let found = setup
        .workspaces
        .orphans(&setup.archives, &setup.runs, false)
        .unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    let OrphanKind::RunRecord { id, paths } = &found[0].kind else {
        panic!("{found:?}");
    };
    assert_eq!(id.as_str(), orphaned);
    assert_eq!(paths, std::slice::from_ref(&logs));
    let swept = setup
        .workspaces
        .sweep_orphan(&setup.archives, &setup.runs, &found[0], false)
        .unwrap();
    assert!(matches!(swept, Swept::Deleted { .. }), "{swept:?}");
    assert!(!logs.exists());
    assert!(kept.join("file").exists());
}

#[test]
fn a_fresh_listing_measures_no_size() {
    let setup = Setup::new();
    plant(&setup.path("data/0123456789abcdef0123456789abcdef"));
    let sized = setup
        .workspaces
        .orphans(&setup.archives, &setup.runs, false)
        .unwrap();
    let fresh = setup
        .workspaces
        .scan(&setup.archives, &setup.runs, false, false)
        .unwrap();
    assert!(sized[0].bytes > 0);
    assert_eq!(fresh[0].bytes, 0);
    assert_eq!(fresh[0].path, sized[0].path);
}
