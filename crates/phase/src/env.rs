//! The seam through which an app's environment reaches its process.
//!
//! phase defines these traits, and starmap implements them. A value takes
//! only one path: from the [`EnvironmentSource`]'s resolvers into an
//! [`AppliedEnvironment`], and from there into the `Command` of a keeper or
//! a `docker compose up` child. Neither trait has a method that returns a
//! value, and neither requires `Debug`, `Display` or `Serialize`, so phase
//! cannot put a value in a plan, a record, a status document, a log or an
//! error.
//!
//! Once a value is applied to a `Command`, the `Command` holds it, and
//! `Command`'s own `Debug` output and `get_envs` show it. The engine's
//! host and container modules, which own every `Command`, never format one
//! or read its environment back.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::process::Command;
use std::time::{Instant, SystemTime};

use crate::name::AppName;

#[cfg(test)]
mod tests;

/// Resolves an app's environment, including its secrets.
pub trait EnvironmentSource {
    /// Resolves every variable that `app` reads, before `deadline`.
    ///
    /// # Errors
    ///
    /// Returns an [`EnvironmentError`] that names the variable and the
    /// resolver when a resolver cannot supply a value, for example because a
    /// 1Password vault is locked or the deadline passed.
    fn resolve(
        &self,
        app: &AppName,
        deadline: Instant,
    ) -> Result<Box<dyn AppliedEnvironment>, EnvironmentError>;
}

/// The resolved environment of one app, which can list its variables' names
/// and apply their values to a `Command`, and can never return a value.
///
/// A `Box<dyn AppliedEnvironment>` cannot be formatted:
///
/// ```compile_fail
/// fn show(environment: &dyn phase::AppliedEnvironment) -> String {
///     format!("{environment:?}")
/// }
/// ```
///
/// ```compile_fail
/// fn show(environment: &dyn phase::AppliedEnvironment) -> String {
///     format!("{environment}")
/// }
/// ```
///
/// and cannot be serialized:
///
/// ```compile_fail
/// fn write(environment: &dyn phase::AppliedEnvironment) -> String {
///     serde_json::to_string(environment).unwrap_or_default()
/// }
/// ```
pub trait AppliedEnvironment: Send {
    /// Returns the names of the variables, which are never secret.
    fn names(&self) -> BTreeSet<String>;

    /// Returns the earliest time at which one of the values expires, such as
    /// the end of a short-lived credential, or `None` when no value expires.
    /// phase resolves the environment again after this time before it
    /// restarts a process, and `status` shows a process whose values have
    /// expired.
    fn expires_at(&self) -> Option<SystemTime>;

    /// Returns a keyed hash of every name and value, such as HMAC-SHA-256
    /// or BLAKE3's keyed mode over the pairs in name order, with `key` as
    /// the key. phase compares digests to find a process whose environment
    /// changed, and keeps only the digest in the run record. Equal names
    /// and values give equal digests under one key, and the digest never
    /// reveals a value.
    fn digest(&self, key: &[u8; 32]) -> [u8; 32];

    /// Sets each variable on `command` under the name that `reference`
    /// returns for the variable's own name. A variable for which `reference`
    /// returns `None` is left out.
    ///
    /// A container app uses this to set each value under a private
    /// reference, such as `MXV_ENV_postgres_1`, on its `docker compose up`
    /// child.
    fn apply_as(&self, command: &mut Command, reference: &dyn Fn(&str) -> Option<String>);

    /// Sets each variable on `command` under its own name. A host process's
    /// keeper receives its environment this way.
    fn apply(&self, command: &mut Command) {
        self.apply_as(command, &|name| Some(name.to_owned()));
    }
}

/// A resolver could not supply the value of a variable. The error names the
/// variable and the resolver, and holds no value.
#[derive(Debug)]
pub struct EnvironmentError {
    /// The variable whose value is missing.
    variable: String,
    /// The resolver that could not supply it, such as `1password`.
    resolver: String,
    /// Why the resolver failed. It holds no value.
    source: Box<dyn Error + Send + Sync>,
}

impl EnvironmentError {
    /// Returns the error of `resolver` failing to supply `variable` because
    /// of `source`, which must hold no value of any variable.
    #[must_use]
    pub fn new(variable: String, resolver: String, source: Box<dyn Error + Send + Sync>) -> Self {
        Self {
            variable,
            resolver,
            source,
        }
    }

    /// Returns the name of the variable whose value is missing.
    #[must_use]
    pub fn variable(&self) -> &str {
        &self.variable
    }

    /// Returns the resolver that could not supply the value.
    #[must_use]
    pub fn resolver(&self) -> &str {
        &self.resolver
    }
}

impl fmt::Display for EnvironmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} could not supply the variable {}",
            self.resolver, self.variable
        )
    }
}

impl Error for EnvironmentError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}
