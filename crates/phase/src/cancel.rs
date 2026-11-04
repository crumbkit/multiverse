//! A flag that asks a running operation to stop what it started.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(test)]
mod tests;

/// A flag that a caller sets, for example from a Ctrl-C handler, to ask
/// `up`, `restart` or `exec` to stop. `up` then stops what that call
/// started. Clones share one flag.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    /// Returns a flag that is not set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the flag. Setting it again has no further effect.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Reports whether the flag is set.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}
