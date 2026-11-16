//! Whether the extra host names that a unit lists reach this machine.
//!
//! mxv serves an extra name, but it cannot make the system look the name up
//! as this machine. It never writes `/etc/hosts`. It looks each name up and
//! tells the person which names need a line there.

use std::net::{IpAddr, ToSocketAddrs as _};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// How long one lookup may take before the name counts as unresolved.
const LOOKUP_WAIT: Duration = Duration::from_secs(2);

/// What a lookup of one name found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Every address of the name is a loopback address.
    Loopback,
    /// The name has an address that is not a loopback address.
    Elsewhere,
    /// The lookup found no address, or did not answer in time.
    Unresolved,
}

/// Looks a name up the way the system does.
pub trait Resolver: Send + Sync + std::fmt::Debug {
    /// Returns what the system finds for `name`.
    fn resolve(&self, name: &str) -> Resolution;
}

/// The system's resolver, with a deadline of two seconds for each lookup.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve(&self, name: &str) -> Resolution {
        let (sender, receiver) = mpsc::channel();
        let target = (name.to_owned(), 0_u16);
        thread::spawn(move || {
            let found: Vec<IpAddr> = target
                .to_socket_addrs()
                .map(|addresses| addresses.map(|address| address.ip()).collect())
                .unwrap_or_default();
            let _ = sender.send(found);
        });
        match receiver.recv_timeout(LOOKUP_WAIT) {
            Ok(found) if found.is_empty() => Resolution::Unresolved,
            Ok(found) if found.iter().all(IpAddr::is_loopback) => Resolution::Loopback,
            Ok(_) => Resolution::Elsewhere,
            Err(_) => Resolution::Unresolved,
        }
    }
}

/// Returns the names of `names` that do not resolve only to loopback
/// addresses, in the order given. A name below `.localhost` resolves to this
/// machine by the standard, so it is not looked up. The lookups run side by
/// side.
pub fn missing(names: &[String], resolver: &dyn Resolver) -> Vec<String> {
    let lookups: Vec<&String> = names
        .iter()
        .filter(|name| !name.ends_with(".localhost"))
        .collect();
    thread::scope(|scope| {
        let handles: Vec<_> = lookups
            .iter()
            .map(|name| scope.spawn(|| resolver.resolve(name)))
            .collect();
        lookups
            .iter()
            .zip(handles)
            .filter_map(|(name, handle)| {
                let found = handle.join().unwrap_or(Resolution::Unresolved);
                (found != Resolution::Loopback).then(|| (*name).clone())
            })
            .collect()
    })
}

/// Returns the line that makes `name` resolve to this machine.
#[must_use]
pub fn line_for(name: &str) -> String {
    format!("127.0.0.1 {name}")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{Resolution, Resolver, SystemResolver, line_for, missing};

    #[derive(Debug)]
    struct Fake(BTreeMap<&'static str, Resolution>);

    impl Resolver for Fake {
        fn resolve(&self, name: &str) -> Resolution {
            self.0.get(name).copied().unwrap_or(Resolution::Unresolved)
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn a_name_that_resolves_only_to_loopback_is_not_missing() {
        let fake = Fake(BTreeMap::from([("a.example.com", Resolution::Loopback)]));
        assert!(missing(&names(&["a.example.com"]), &fake).is_empty());
    }

    #[test]
    fn a_name_that_resolves_elsewhere_or_not_at_all_is_missing_in_the_order_given() {
        let fake = Fake(BTreeMap::from([
            ("b.example.com", Resolution::Elsewhere),
            ("c.example.com", Resolution::Loopback),
        ]));
        let list = names(&["z.example.com", "b.example.com", "c.example.com"]);
        assert_eq!(
            missing(&list, &fake),
            names(&["z.example.com", "b.example.com"])
        );
    }

    #[test]
    fn a_name_below_localhost_is_not_looked_up() {
        let fake = Fake(BTreeMap::new());
        assert!(missing(&names(&["app.localhost"]), &fake).is_empty());
    }

    #[test]
    fn the_line_to_add_maps_the_name_to_the_loopback_address() {
        assert_eq!(line_for("a.example.com"), "127.0.0.1 a.example.com");
    }

    #[test]
    fn the_system_resolver_finds_localhost_on_loopback() {
        assert_eq!(SystemResolver.resolve("localhost"), Resolution::Loopback);
    }
}
