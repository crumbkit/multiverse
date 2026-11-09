//! The host runner, which starts each host process under its keeper and the
//! sandbox, in its own process group, and stops it with its stop signal and
//! grace. The keeper module builds the `Command`s of keepers and of the app
//! processes they start (PROC-1), and the engine reaches them only through
//! `HostRunner`.
//!
//! `System` is the runner of this machine. It starts a keeper with the
//! keeper module's `spawn`, reads and signals processes through the
//! record module's process table, finds which process holds a listening
//! port, and follows a process's log. A test supplies its own runner.
//!
//! The module also holds what mxv's own process runner reads from the
//! host: the start of a process, the signals to a process and its group,
//! which process holds a listening port, and a follower of a process's log.

mod follow;
mod listen;

#[cfg(test)]
mod tests;

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

pub use crate::engine::host::follow::{LogFollower, MOST_FOLLOW_BYTES};
pub use crate::engine::host::listen::{Holder, listen_holder};
use crate::engine::keeper::{Kept, Launch, spawn};
use crate::engine::probe::LogLines;
use crate::error::PhaseError;
use crate::plan::restart::StopSignal;
use crate::record::process::{self, Identity, System};
pub use crate::record::process::{
    BadProcessId, Member, ProcessError, ProcessGroup, ProcessId, RawStart, TableEntry,
    group_exists, live_members, process_table, raw_start, reap, signal_group, started_at,
};
use crate::record::run::HostProcess;

/// What the engine asks of the machine to run host processes: start a
/// keeper, stop a keeper and its app, tell whether a recorded process is
/// still the one phase started, follow a log file, and find what listens
/// on a port.
pub(crate) trait HostRunner {
    /// Starts the keeper that `launch` describes and waits for its
    /// handshake, as the keeper module's `spawn` does.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::KeeperUnstartable`] or
    /// [`PhaseError::ProcessUnstartable`], as `spawn` does.
    fn launch(&self, launch: &Launch<'_>) -> Result<Kept, PhaseError>;

    /// Stops the keeper and the app that `stop` names, as [`HostStop`]
    /// says. It gives up the wait for the stop signal at `deadline`, sends
    /// `KILL`, and waits the kill wait, so it returns by `deadline` plus the
    /// kill wait. It checks each process's
    /// identity first, and never signals a process that it cannot prove is
    /// the one phase started.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::ProcessTable`] when the process table cannot
    /// be read or a signal cannot be sent.
    fn stop(&self, stop: &HostStop<'_>, deadline: Instant) -> Result<HostStopped, PhaseError>;

    /// Returns what the recorded `process` is now. It reaps nothing.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::ProcessTable`] when the process table cannot
    /// be read.
    fn identity(&self, process: &HostProcess) -> Result<Identity, PhaseError>;

    /// Returns a follower of the log file `log` that starts at the file's
    /// current end, so the engine calls it before it launches the process
    /// that writes the log. A log that does not exist yet starts at its
    /// first byte.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::ProcessTable`] when the follower cannot be
    /// set up.
    fn follow_log(&self, log: &Path) -> Result<Box<dyn LogLines + Send>, PhaseError>;

    /// Returns which process holds the socket that listens on the TCP port
    /// `port`, as seen from the process group `group`, as
    /// [`listen_holder`] says. It returns `None` when this system gives no
    /// way to tell. [`Holder::Other`] with no pid means that no process
    /// whose descriptors phase may read holds it, which includes nothing
    /// listening at all.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::ProcessTable`] when phase cannot tell, which
    /// the engine reports as unknown and does not let block a start.
    fn listener(&self, group: u32, port: u16) -> Result<Option<Holder>, PhaseError>;

    /// Reports whether something holds `port` on a loopback address,
    /// without telling what. The engine asks this when
    /// [`HostRunner::listener`] names no holder.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::ProcessTable`] when phase cannot tell.
    fn port_in_use(&self, port: u16) -> Result<bool, PhaseError>;
}

/// The keeper and the app that [`HostRunner::stop`] stops, and how.
///
/// `stop` sends `TERM` to the keeper, which stops its app with `signal`
/// and the app's own grace. When the keeper is gone, or goes while the app
/// is still there, `stop` sends `signal` to the app's group itself. It
/// waits `grace` and `kill_wait` in all, counted again from the app's
/// signal when `stop` sends one, then sends `KILL` to what is left of both,
/// and waits `kill_wait` again.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HostStop<'a> {
    /// The keeper, or `None` when none is recorded.
    pub(crate) keeper: Option<&'a HostProcess>,
    /// The app, or `None` when none is recorded.
    pub(crate) app: Option<&'a HostProcess>,
    /// The signal that asks the app to stop.
    pub(crate) signal: StopSignal,
    /// How long the app has to stop after its signal.
    pub(crate) grace: Duration,
    /// How long `stop` waits after `KILL`.
    pub(crate) kill_wait: Duration,
}

/// How [`HostRunner::stop`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostStopped {
    /// `stop` signalled the keeper or the app, and checked that both are
    /// gone.
    Stopped,
    /// Nothing of either was left, so `stop` signalled nothing.
    AlreadyGone,
    /// phase may not read a recorded process, so it cannot prove that the
    /// process is its own, and `stop` left it alone. A process that phase
    /// proved its own was stopped.
    NotOurs,
    /// A recorded process led `group` and has gone, and the group still
    /// has a live member, but nothing proves the group is the one phase
    /// started, so `stop` signalled nothing of it.
    GroupUnverified {
        /// The group that `stop` left alone.
        group: u32,
    },
    /// Something of `group` was still there after `KILL` and the kill
    /// wait.
    Unstoppable {
        /// The group that is left: the app's when the app is left, and the
        /// keeper's otherwise.
        group: u32,
    },
}

impl HostRunner for System {
    fn launch(&self, launch: &Launch<'_>) -> Result<Kept, PhaseError> {
        spawn(launch)
    }

    fn stop(&self, stop: &HostStop<'_>, deadline: Instant) -> Result<HostStopped, PhaseError> {
        process::stop_host(self, stop, deadline)
    }

    fn identity(&self, process: &HostProcess) -> Result<Identity, PhaseError> {
        process::identify(self, process)
    }

    fn follow_log(&self, log: &Path) -> Result<Box<dyn LogLines + Send>, PhaseError> {
        LogFollower::at_end_or_empty(log)
            .map(|lines| Box::new(lines) as Box<dyn LogLines + Send>)
            .map_err(|error| PhaseError::ProcessTable {
                message: format!("the log {} cannot be read: {error}", log.display()),
            })
    }

    fn listener(&self, group: u32, port: u16) -> Result<Option<Holder>, PhaseError> {
        listen_holder(group, port).map_err(PhaseError::from)
    }

    fn port_in_use(&self, port: u16) -> Result<bool, PhaseError> {
        // The standard library sets `SO_REUSEADDR`, so a port that only
        // closed connections hold in `TIME_WAIT` binds. On macOS that also
        // lets the bind succeed beside a listener on the wildcard address,
        // so a connect to each loopback address finds such a listener.
        match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => return Ok(true),
            Err(error) => {
                return Err(PhaseError::ProcessTable {
                    message: format!("phase cannot tell whether port {port} is in use: {error}"),
                });
            }
        }
        let loopbacks = [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ];
        Ok(loopbacks.into_iter().any(|address| {
            TcpStream::connect_timeout(&SocketAddr::new(address, port), PORT_CONNECT_WAIT).is_ok()
        }))
    }
}

/// How long [`HostRunner::port_in_use`] waits for a connect to a loopback
/// address. A connect that does not succeed within it counts as nothing
/// listening there.
const PORT_CONNECT_WAIT: Duration = Duration::from_millis(100);

/// Returns `true` when `identity` means that nothing of the recorded
/// process is left: it is gone, or another process has its pid.
pub(crate) const fn is_gone(identity: Identity) -> bool {
    matches!(identity, Identity::Gone | Identity::Reused)
}
