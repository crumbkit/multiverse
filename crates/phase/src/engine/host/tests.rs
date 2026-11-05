use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;

use super::follow::{LogFollower, MOST_FOLLOW_BYTES};
use super::listen::{listening_inodes, socket_descriptors, socket_listens_on};
use super::{Holder, listen_holder};
use crate::engine::probe::LogLines as _;

/// Appends `text` to the log at `path`, creating it when it is missing.
fn append(path: &Path, text: &str) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes the log that a follower reads"
    )]
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

/// Replaces the log at `path` with `text`, as a cap that empties it does.
fn replace(path: &Path, text: &str) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test empties the log that a follower reads, as a cap does"
    )]
    OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

#[test]
fn a_follower_at_the_end_returns_each_complete_line_written_after_it_opened() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.log");
    append(&path, "before\n");
    let mut follower = LogFollower::at_end(&path).unwrap().unwrap();
    assert_eq!(follower.path(), path);
    append(&path, "first\nsec");
    assert_eq!(follower.lines().unwrap(), ["first"]);
    append(&path, "ond\n");
    assert_eq!(follower.lines().unwrap(), ["second"]);
    assert!(follower.lines().unwrap().is_empty());
    assert!(
        LogFollower::at_end(&dir.path().join("missing.log"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_follower_from_the_start_reads_what_the_log_already_holds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.log");
    append(&path, "one\ntwo\n");
    let mut follower = LogFollower::from_start(&path).unwrap();
    assert_eq!(follower.lines().unwrap(), ["one", "two"]);
    assert!(LogFollower::from_start(&dir.path().join("missing.log")).is_err());
}

#[test]
fn a_log_that_does_not_exist_yet_counts_as_empty_and_is_read_from_its_start_once_it_appears() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("later.log");
    let mut follower = LogFollower::at_end_or_empty(&path).unwrap();
    assert!(follower.new_lines().unwrap().is_empty());
    append(&path, "ready\n");
    assert_eq!(follower.new_lines().unwrap(), ["ready"]);
}

/// Renames the log at `path` to `older`, as a restart's rotation does.
fn rotate(path: &Path, older: &Path) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test renames the log that a follower reads, as a rotation does"
    )]
    fs::rename(path, older).unwrap();
}

#[test]
fn a_follower_starts_again_at_the_start_of_a_log_that_became_shorter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.log");
    append(&path, "a long first line\n");
    let mut follower = LogFollower::at_end_or_empty(&path).unwrap();
    append(&path, "half");
    assert!(follower.lines().unwrap().is_empty());
    // A cap empties the log, and the held partial line belongs to the old
    // contents.
    replace(&path, "new\n");
    assert_eq!(follower.lines().unwrap(), ["new"]);
}

#[test]
fn a_line_longer_than_one_read_comes_back_in_parts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.log");
    append(&path, "");
    let mut follower = LogFollower::at_end_or_empty(&path).unwrap();
    let most = usize::try_from(MOST_FOLLOW_BYTES).unwrap();
    append(&path, &"x".repeat(most + 10));
    let parts = follower.lines().unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].len(), most);
    assert!(follower.lines().unwrap().is_empty());
    append(&path, "\n");
    assert_eq!(follower.lines().unwrap(), ["x".repeat(10)]);
    fs::remove_file(&path).unwrap();
}

#[test]
fn an_unfinished_line_and_a_read_never_hold_more_than_the_bound() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.log");
    append(&path, "");
    let mut follower = LogFollower::at_end_or_empty(&path).unwrap();
    let most = usize::try_from(MOST_FOLLOW_BYTES).unwrap();
    append(&path, &"x".repeat(most - 5));
    assert!(follower.lines().unwrap().is_empty());
    append(&path, &format!("{}\n", "y".repeat(20)));
    let parts = follower.lines().unwrap();
    assert_eq!(
        parts,
        [format!("{}{}", "x".repeat(most - 5), "y".repeat(5))]
    );
    assert_eq!(follower.lines().unwrap(), ["y".repeat(15)]);
}

#[test]
fn a_follower_moves_to_the_new_log_after_the_old_one_is_renamed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.log");
    append(&path, "one\nhal");
    let mut follower = LogFollower::from_start(&path).unwrap();
    assert_eq!(follower.lines().unwrap(), ["one"]);
    append(&path, "f\nend");
    rotate(&path, &dir.path().join("a.log.1"));
    // The old file stays open while no new log exists.
    assert_eq!(follower.lines().unwrap(), ["half"]);
    append(&path, "new\n");
    assert_eq!(follower.lines().unwrap(), ["end"]);
    assert_eq!(follower.lines().unwrap(), ["new"]);
}

#[test]
fn the_listening_sockets_of_a_port_come_from_the_lines_in_state_0a() {
    let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:5DC1 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41234 1 0000000000000000 100 0 0 10 0\n   1: 0100007F:5DC1 0100007F:9C40 01 00000000:00000000 00:00000000 00000000  1000        0 41999 1 0000000000000000 20 4 30 10 -1\n   2: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 17000 1 0000000000000000 100 0 0 10 0\n";
    assert_eq!(listening_inodes(table, 24001), ["41234"]);
    assert_eq!(listening_inodes(table, 8080), ["17000"]);
    assert!(listening_inodes(table, 9).is_empty());
}

#[test]
fn the_socket_descriptors_of_a_descriptor_list_are_the_entries_of_type_socket() {
    let mut list = Vec::new();
    for (fd, kind) in [(0_i32, 1_i32), (3, 2), (4, 3), (7, 2)] {
        list.extend_from_slice(&fd.to_ne_bytes());
        list.extend_from_slice(&kind.to_ne_bytes());
    }
    assert_eq!(socket_descriptors(&list), [3, 7]);
}

#[test]
fn a_socket_info_listens_on_a_port_only_as_a_tcp_socket_in_the_listen_state() {
    let info = |kind: i32, port: u16, state: i32| {
        let mut info = vec![0_u8; 792];
        info[256..260].copy_from_slice(&kind.to_ne_bytes());
        info[268..270].copy_from_slice(&port.to_be_bytes());
        info[344..348].copy_from_slice(&state.to_ne_bytes());
        info
    };
    assert!(socket_listens_on(&info(2, 24_001, 1), 24_001));
    assert!(!socket_listens_on(&info(2, 24_001, 1), 24_002));
    assert!(!socket_listens_on(&info(2, 24_001, 4), 24_001));
    assert!(!socket_listens_on(&info(1, 24_001, 1), 24_001));
    assert!(!socket_listens_on(&info(2, 24_001, 1)[..300], 24_001));
}

#[test]
fn this_process_holds_its_own_listening_socket_as_a_member_of_its_group() {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let own = u32::try_from(rustix::process::getpgrp().as_raw_nonzero().get()).unwrap();
    let held = listen_holder(own, port).unwrap();
    // A group that no process leads sees this process as another holder.
    let other = listen_holder(0x7fff_fff0, port).unwrap();
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        assert_eq!(held, Some(Holder::Group));
        assert_eq!(other, Some(Holder::Other(Some(std::process::id()))));
    } else {
        assert_eq!((held, other), (None, None));
    }
    drop(listener);
}
