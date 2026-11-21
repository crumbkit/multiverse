//! A flag that asks a running operation to stop what it started.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(test)]
mod tests;

/// A flag that a caller sets, for example from a thread that reads the
/// signals of a Ctrl-C, to ask `up`, `restart` or `exec` to stop. `up` then
/// stops what that call started. Clones share one flag.
///
/// The flag also holds the cancel that each child process of phase's
/// one-shot runs watches, so [`Cancel::cancel`] stops such a run at once.
/// [`Cancel::cancel`] takes a lock, so a signal handler must not call it;
/// call it from an ordinary thread.
#[derive(Clone, Debug, Default)]
pub struct Cancel {
    /// Whether the flag is set.
    flag: Arc<AtomicBool>,
    /// The cancel that phase's one-shot child processes watch.
    runs: crumbkit_process::Cancel,
}

impl Cancel {
    /// Returns a flag that is not set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the flag and stops every one-shot child process that watches
    /// it. Setting it again has no further effect.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.runs.cancel();
    }

    /// Reports whether the flag is set.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Returns the cancel that a one-shot child process watches, which
    /// [`Cancel::cancel`] cancels.
    pub(crate) const fn runs(&self) -> &crumbkit_process::Cancel {
        &self.runs
    }
}
