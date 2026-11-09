use std::path::PathBuf;

use multiverse::phase::host::TableEntry;

use super::ancestors;

fn entry(pid: u32, parent: u32) -> TableEntry {
    TableEntry::new(pid, parent, pid, Some(PathBuf::from("/tmp")), None)
}

#[test]
fn the_walk_collects_each_ancestor_and_stops_before_the_system_parent() {
    let table = [entry(30, 20), entry(20, 10), entry(10, 1), entry(40, 20)];
    assert_eq!(ancestors(&table, 30), [20, 10].into());
}

#[test]
fn a_sibling_that_shares_the_parent_is_no_ancestor() {
    let table = [entry(30, 20), entry(20, 1), entry(40, 20)];
    assert!(!ancestors(&table, 30).contains(&40));
}

#[test]
fn a_loop_in_the_table_ends_the_walk() {
    let table = [entry(30, 20), entry(20, 30)];
    assert_eq!(ancestors(&table, 30), [20, 30].into());
}

#[test]
fn a_process_that_is_its_own_parent_ends_the_walk() {
    let table = [entry(30, 30)];
    assert!(ancestors(&table, 30).is_empty());
}
