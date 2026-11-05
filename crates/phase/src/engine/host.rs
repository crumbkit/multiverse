//! The host runner, which starts each host process under its keeper and the
//! sandbox, in its own process group, and stops it with its stop signal and
//! grace. multiverse's process runner lands here. Until the host runner
//! lands, the keeper module builds the `Command`s of keepers and of the app
//! processes they start (PROC-1).
//!
//! It already holds what mxv's own process runner reads from the host:
//! the start of a process, the signals to a process and its group, which
//! process holds a listening port, and a follower of a process's log.

mod follow;
mod listen;

#[cfg(test)]
mod tests;

pub use crate::engine::host::follow::{LogFollower, MOST_FOLLOW_BYTES};
pub use crate::engine::host::listen::{Holder, listen_holder};
pub use crate::record::process::{
    ProcessError, RawStart, group_exists, raw_start, reap, signal_group, signal_pid,
};
