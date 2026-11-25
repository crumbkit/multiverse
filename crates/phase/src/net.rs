//! The network owner (BASE-5), which is the one module of phase that opens
//! a TCP connection. A host port check, a container's published-port check
//! and the `http` and `tcp` probes connect through it.

use std::io;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// Connects to `address` within `timeout`, as
/// [`TcpStream::connect_timeout`] does.
///
/// # Errors
///
/// Returns the error of [`TcpStream::connect_timeout`], such as
/// [`io::ErrorKind::ConnectionRefused`] when nothing listens at `address`,
/// [`io::ErrorKind::TimedOut`] when `timeout` passes first, and
/// [`io::ErrorKind::InvalidInput`] when `timeout` is zero.
#[expect(
    clippy::disallowed_methods,
    reason = "crates/phase/src/net.rs is the network owner, and it opens TCP connections (BASE-5)"
)]
pub(crate) fn connect_timeout(address: &SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    TcpStream::connect_timeout(address, timeout)
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::net::{Ipv4Addr, SocketAddr, TcpListener};
    use std::time::Duration;

    use super::connect_timeout;

    const WAIT: Duration = Duration::from_secs(5);

    #[test]
    fn a_connect_reaches_a_listener() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let stream = connect_timeout(&address, WAIT).unwrap();
        assert_eq!(stream.peer_addr().unwrap(), address);
    }

    #[test]
    fn a_connect_where_nothing_listens_is_refused() {
        let address = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap();
        let error = connect_timeout(&address, WAIT).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[test]
    fn a_zero_timeout_is_refused_as_std_refuses_it() {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, 9));
        let error = connect_timeout(&address, Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
