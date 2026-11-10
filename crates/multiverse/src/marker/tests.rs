//! Tests of the workspace marker that deadband reads.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;

use super::{MARKER_FILE, Marker, delete, ensure};
use crate::testing::write;

#[test]
fn ensure_writes_an_empty_marker_with_mode_0644_in_a_directory_without_one() {
    let dir = tempfile::tempdir().unwrap();

    let marker = ensure(dir.path()).unwrap();

    let path = dir.path().join(MARKER_FILE);
    assert_eq!(marker, Marker::Written);
    let metadata = fs::symlink_metadata(&path).unwrap();
    assert!(metadata.is_file());
    assert_eq!(metadata.len(), 0);
    assert_eq!(metadata.permissions().mode() & 0o777, 0o644);
}

#[test]
fn ensure_leaves_a_marker_that_is_already_there_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MARKER_FILE);
    write(&path, "a person's note\n");

    let marker = ensure(dir.path()).unwrap();

    assert_eq!(marker, Marker::Present);
    assert_eq!(fs::read_to_string(&path).unwrap(), "a person's note\n");
}

#[test]
fn ensure_leaves_a_directory_or_a_link_of_the_markers_name_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MARKER_FILE);
    fs::create_dir(&path).unwrap();
    assert_eq!(ensure(dir.path()).unwrap(), Marker::Blocked(path.clone()));
    assert!(path.is_dir());

    fs::remove_dir(&path).unwrap();
    std::os::unix::fs::symlink("elsewhere", &path).unwrap();
    assert_eq!(ensure(dir.path()).unwrap(), Marker::Blocked(path.clone()));
    assert_eq!(
        fs::read_link(&path).unwrap(),
        std::path::Path::new("elsewhere")
    );
}

#[test]
fn delete_removes_an_empty_marker_and_keeps_one_that_holds_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(MARKER_FILE);
    ensure(dir.path()).unwrap();
    delete(dir.path()).unwrap();
    assert!(!path.exists());
    delete(dir.path()).unwrap();

    write(&path, "a person's note\n");
    delete(dir.path()).unwrap();
    assert!(path.is_file());
}

#[test]
fn ensure_fails_when_the_directory_cannot_be_written() {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();

    let result = ensure(dir.path());

    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(result.is_err(), "{result:?}");
    assert!(!dir.path().join(MARKER_FILE).exists());
}
