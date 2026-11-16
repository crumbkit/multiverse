//! Whether the extra host names that a unit lists reach this machine.
//!
//! mxv serves an extra name, but it cannot make the system look the name up
//! as this machine. It never writes `/etc/hosts`. It looks each name up and
//! tells the person which names need a line there.

use std::net::{IpAddr, ToSocketAddrs as _};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// How long one lookup may take before the name counts as timed out.
const LOOKUP_WAIT: Duration = Duration::from_secs(2);

/// What a lookup of one name found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Every address of the name is a loopback address.
    Loopback,
    /// The name has an address that is not a loopback address.
    Elsewhere,
    /// The lookup finished and found no address.
    Unresolved,
    /// The lookup did not finish in time, so mxv cannot tell.
    TimedOut,
}

impl Resolution {
    /// Returns the sentence that says what the lookup of `name` found.
    #[must_use]
    pub fn detail(self, name: &str) -> String {
        match self {
            Self::TimedOut => format!(
                "the lookup of {name} did not finish within {} seconds, so mxv cannot tell whether it resolves to this machine",
                LOOKUP_WAIT.as_secs()
            ),
            _ => format!("{name} does not resolve to this machine"),
        }
    }

    /// Returns what a person does about `name`: the line to add to
    /// `/etc/hosts`, or nothing when the lookup timed out, because a line
    /// might already be there.
    #[must_use]
    pub fn advice(self, name: &str) -> Option<String> {
        (self != Self::TimedOut).then(|| format!("add this line to /etc/hosts: {}", line_for(name)))
    }
}

/// Looks a name up the way the system does.
pub trait Resolver: Send + Sync + std::fmt::Debug {
    /// Returns what the system finds for `name`.
    fn resolve(&self, name: &str) -> Resolution;
}

/// Returns the addresses that the system finds for `name`.
fn system_lookup(name: &str) -> Vec<IpAddr> {
    (name, 0_u16)
        .to_socket_addrs()
        .map(|addresses| addresses.map(|address| address.ip()).collect())
        .unwrap_or_default()
}

/// The system's resolver, with a deadline for each lookup.
#[derive(Clone, Copy, Debug)]
pub struct SystemResolver {
    /// How long a lookup may take.
    wait: Duration,
    /// The lookup, which a test replaces.
    lookup: fn(&str) -> Vec<IpAddr>,
}

impl Default for SystemResolver {
    fn default() -> Self {
        Self {
            wait: LOOKUP_WAIT,
            lookup: system_lookup,
        }
    }
}

impl SystemResolver {
    /// Returns a resolver that waits `wait` for `lookup`.
    #[must_use]
    pub const fn with_lookup(wait: Duration, lookup: fn(&str) -> Vec<IpAddr>) -> Self {
        Self { wait, lookup }
    }
}

impl Resolver for SystemResolver {
    fn resolve(&self, name: &str) -> Resolution {
        let (sender, receiver) = mpsc::channel();
        let (target, lookup) = (name.to_owned(), self.lookup);
        let spawned = thread::Builder::new()
            .name("mxv-lookup".to_owned())
            .spawn(move || {
                let _ = sender.send(lookup(&target));
            });
        if spawned.is_err() {
            return Resolution::TimedOut;
        }
        match receiver.recv_timeout(self.wait) {
            Ok(found) if found.is_empty() => Resolution::Unresolved,
            Ok(found) if found.iter().all(IpAddr::is_loopback) => Resolution::Loopback,
            Ok(_) => Resolution::Elsewhere,
            Err(_) => Resolution::TimedOut,
        }
    }
}

/// Returns the names of `names` that do not resolve only to loopback
/// addresses, each with what the lookup found, in the order given. A name
/// below `.localhost` resolves to this machine by the standard, so it is not
/// looked up. The lookups run side by side.
pub fn missing(names: &[String], resolver: &dyn Resolver) -> Vec<(String, Resolution)> {
    let lookups: Vec<&String> = names
        .iter()
        .filter(|name| !name.ends_with(".localhost"))
        .collect();
    thread::scope(|scope| {
        let handles: Vec<_> = lookups
            .iter()
            .map(|name| {
                thread::Builder::new()
                    .spawn_scoped(scope, || resolver.resolve(name))
                    .ok()
            })
            .collect();
        lookups
            .iter()
            .zip(handles)
            .filter_map(|(name, handle)| {
                let found = handle
                    .and_then(|handle| handle.join().ok())
                    .unwrap_or(Resolution::TimedOut);
                (found != Resolution::Loopback).then(|| ((*name).clone(), found))
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
    use std::net::{IpAddr, Ipv4Addr};
    use std::thread;
    use std::time::{Duration, Instant};

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

    fn blocked(_: &str) -> Vec<IpAddr> {
        loop {
            thread::park();
        }
    }

    fn loopback(_: &str) -> Vec<IpAddr> {
        vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
    }

    fn elsewhere(_: &str) -> Vec<IpAddr> {
        vec![IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))]
    }

    fn nothing(_: &str) -> Vec<IpAddr> {
        Vec::new()
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
            [
                ("z.example.com".to_owned(), Resolution::Unresolved),
                ("b.example.com".to_owned(), Resolution::Elsewhere)
            ]
        );
    }

    #[test]
    fn a_name_below_localhost_is_not_looked_up() {
        let fake = Fake(BTreeMap::new());
        assert!(missing(&names(&["app.localhost"]), &fake).is_empty());
    }

    #[test]
    fn a_timed_out_lookup_gives_no_line_to_add() {
        assert_eq!(Resolution::TimedOut.advice("a.example.com"), None);
        assert!(
            Resolution::TimedOut
                .detail("a.example.com")
                .contains("cannot tell")
        );
        assert_eq!(
            Resolution::Unresolved.advice("a.example.com").as_deref(),
            Some("add this line to /etc/hosts: 127.0.0.1 a.example.com")
        );
    }

    #[test]
    fn the_line_to_add_maps_the_name_to_the_loopback_address() {
        assert_eq!(line_for("a.example.com"), "127.0.0.1 a.example.com");
    }

    #[test]
    fn the_system_resolver_sorts_what_its_lookup_finds() {
        let wait = Duration::from_secs(5);
        assert_eq!(
            SystemResolver::with_lookup(wait, loopback).resolve("x"),
            Resolution::Loopback
        );
        assert_eq!(
            SystemResolver::with_lookup(wait, elsewhere).resolve("x"),
            Resolution::Elsewhere
        );
        assert_eq!(
            SystemResolver::with_lookup(wait, nothing).resolve("x"),
            Resolution::Unresolved
        );
    }

    #[test]
    fn a_lookup_that_blocks_times_out_within_the_wait() {
        let resolver = SystemResolver::with_lookup(Duration::from_millis(200), blocked);
        let started = Instant::now();
        assert_eq!(resolver.resolve("x"), Resolution::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn several_blocking_names_take_about_one_wait_together() {
        let resolver = SystemResolver::with_lookup(Duration::from_millis(300), blocked);
        let list = names(&[
            "a.example.com",
            "b.example.com",
            "c.example.com",
            "d.example.com",
        ]);
        let started = Instant::now();
        let found = missing(&list, &resolver);
        assert_eq!(found.len(), 4);
        assert!(
            found
                .iter()
                .all(|(_, found)| *found == Resolution::TimedOut)
        );
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "{:?}",
            started.elapsed()
        );
    }
}
