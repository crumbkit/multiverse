//! The host ports of a workspace: allocating a free port for each named
//! port, and checking whether a port is taken or accepts connections. It is
//! the one module that opens sockets.

use std::collections::BTreeSet;
use std::hash::{BuildHasher as _, RandomState};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::ops::RangeInclusive;
use std::time::Duration;

use crate::error::Error;

#[cfg(test)]
mod tests;

/// How long one connection attempt to a port waits.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);

/// The host ports that `mxv up` allocates from when the config file sets
/// no range. The range lies below 32768, where the ephemeral ranges of
/// Linux (32768 to 60999) and macOS (49152 to 65535) begin, so an outgoing
/// connection never holds one of these ports by chance.
pub const DEFAULT_PORT_RANGE: RangeInclusive<u16> = 20000..=29999;

/// Returns a port of `range` that is in neither `reserved` nor taken now.
/// The search starts at a random port of the range and wraps around, so
/// two allocations that run at once seldom pick the same port.
///
/// # Errors
///
/// Returns [`Error::NoFreePort`] when every port of the range is reserved
/// or taken.
pub(crate) fn allocate(
    range: &RangeInclusive<u16>,
    reserved: &BTreeSet<u16>,
) -> Result<u16, Error> {
    let first = u32::from(*range.start());
    let size = u32::from(*range.end()).saturating_sub(first) + 1;
    if range.is_empty() {
        return Err(exhausted(range));
    }
    let offset = u32::try_from(RandomState::new().hash_one(0_u8) % u64::from(size)).unwrap_or(0);
    for step in 0..size {
        let Ok(port) = u16::try_from(first + (offset + step) % size) else {
            continue;
        };
        if !reserved.contains(&port) && !is_taken(port) {
            return Ok(port);
        }
    }
    Err(exhausted(range))
}

/// Returns the error of a range whose every port is reserved or taken.
fn exhausted(range: &RangeInclusive<u16>) -> Error {
    Error::NoFreePort {
        source: std::io::Error::other(format!(
            "every port from {} to {} is held by a workspace or taken",
            range.start(),
            range.end()
        )),
    }
}

/// Reports whether something holds `port`: a bind of 127.0.0.1 fails, or a
/// connection to 127.0.0.1 or `::1` succeeds. The connection catches a
/// server that listens on every address, which some systems let a bind of
/// one address share.
pub(crate) fn is_taken(port: u16) -> bool {
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_err() || accepts(port)
}

/// Reports whether a TCP connection to `port` on 127.0.0.1 or on `::1`
/// succeeds. A server that binds `localhost` may listen on either one.
pub(crate) fn accepts(port: u16) -> bool {
    [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ]
    .iter()
    .any(|address| TcpStream::connect_timeout(address, CONNECT_TIMEOUT).is_ok())
}
