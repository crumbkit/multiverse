use std::collections::BTreeSet;
use std::net::{Ipv4Addr, TcpListener};

use crate::error::Error;
use crate::ports::{DEFAULT_PORT_RANGE, accepts, allocate, is_taken};

#[test]
fn an_allocated_port_is_free_in_the_range_and_outside_the_reserved_ports() {
    let first = allocate(&DEFAULT_PORT_RANGE, &BTreeSet::new()).unwrap();
    let reserved = BTreeSet::from([first]);
    let second = allocate(&DEFAULT_PORT_RANGE, &reserved).unwrap();
    assert_ne!(first, second);
    assert!(DEFAULT_PORT_RANGE.contains(&first) && DEFAULT_PORT_RANGE.contains(&second));
    assert!(!is_taken(second));
}

#[test]
fn a_range_whose_every_port_is_reserved_or_taken_has_no_free_port() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let held = listener.local_addr().unwrap().port();
    let next = held.wrapping_add(1);
    let result = allocate(&(held..=next), &BTreeSet::from([next]));
    assert!(
        matches!(result, Err(Error::NoFreePort { .. })),
        "{result:?}"
    );
}

#[test]
fn a_range_of_one_free_port_allocates_that_port() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    assert_eq!(allocate(&(port..=port), &BTreeSet::new()).unwrap(), port);
}

#[test]
fn a_port_with_a_listener_is_taken_and_accepts_connections() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(is_taken(port));
    assert!(accepts(port));
    drop(listener);
    assert!(!accepts(port));
}

#[test]
fn a_port_that_a_server_holds_on_every_address_is_taken() {
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(is_taken(port));
}
