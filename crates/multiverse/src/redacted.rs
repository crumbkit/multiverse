//! A value that mxv passes to a process or a container without showing it,
//! such as the value of an `env` variable, which may be a secret.

use std::fmt;

/// A value that mxv never shows. Its `Debug` prints `<redacted>`, and it
/// implements neither `Display` nor `Serialize`, so it reaches no row,
/// log, error or file. mxv reads it only to build the environment of the
/// process or container that receives it.
#[derive(Clone, PartialEq, Eq)]
pub struct Redacted(String);

impl Redacted {
    /// Returns `value`, which mxv will not show.
    pub(crate) const fn new(value: String) -> Self {
        Self(value)
    }

    /// Returns the value, for the environment of the process or container
    /// that receives it.
    pub(crate) fn reveal(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Redacted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// A test compares a value with the text it expects without showing it.
#[cfg(test)]
impl PartialEq<&str> for Redacted {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}
