//! The seam through which an app's environment reaches its process.
//!
//! phase defines these traits, and starmap implements them. A value takes
//! only these paths: from the [`EnvironmentSource`]'s resolvers into an
//! [`AppliedEnvironment`], and from there into the `Command` of a keeper or
//! a `docker compose up` child, or, for a container's secret file, through
//! [`AppliedEnvironment::write_value`] into the standard input of `docker
//! exec`. Neither trait has a method that returns a value, and neither
//! requires `Debug`, `Display` or `Serialize`, so phase cannot put a value
//! in a plan, a record, a status document, a log or an error.
//!
//! Once a value is applied to a `Command`, the `Command` holds it, and
//! `Command`'s own `Debug` output and `get_envs` show it. The engine's
//! host and container modules, which own every `Command`, never format one
//! or read its environment back.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::io;
use std::process::Command;
use std::time::{Instant, SystemTime};

use crate::name::{AppName, ProcessName};

#[cfg(test)]
mod tests;

/// Resolves the environment of each process of an app, including its
/// secrets.
///
/// The source supplies every value a process receives, literal or secret,
/// because the plan holds names only. Two processes of one app can receive
/// different values for one name, such as `PORT`. A host process inherits
/// only `PATH` and `HOME` from the program that calls phase, so the source
/// also supplies any other inherited variable an app relies on, such as
/// `LANG`.
pub trait EnvironmentSource {
    /// Resolves every variable that `process` of `app` receives, before
    /// `deadline`. An optional variable without a value is left out of
    /// the environment's [`AppliedEnvironment::names`].
    ///
    /// # Errors
    ///
    /// Returns an [`EnvironmentError`] that names the variable and the
    /// resolver when a resolver cannot supply a value, for example because a
    /// 1Password vault is locked or the deadline passed.
    fn resolve(
        &self,
        app: &AppName,
        process: &ProcessName,
        deadline: Instant,
    ) -> Result<Box<dyn AppliedEnvironment>, EnvironmentError>;
}

/// The resolved environment of one process, which can list its variables' names
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
    ///
    /// A variable whose name this set lacks is absent. The source leaves
    /// out a variable that the plan marks optional, in
    /// [`ProcessPlan::optional_env`](crate::ProcessPlan::optional_env),
    /// when it has no value for it, such as `LANG` when the caller's shell
    /// does not set it. phase then starts a host process with the variable
    /// unset, leaves the variable out of a container's service, and writes
    /// no secret file for it. For a required variable without a value the
    /// source returns an [`EnvironmentError`] from
    /// [`EnvironmentSource::resolve`] instead, and phase refuses an
    /// environment whose names lack a required variable with the same
    /// error, so the process never starts with it unset or empty.
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

    /// Writes the bytes of the value of the variable `name` to `out`, and
    /// nothing else. phase calls it only to deliver a container's secret
    /// as a file: it buffers the bytes in memory that it overwrites with
    /// zeros afterwards, also when the buffer grows, writes them to the
    /// standard input of `docker exec`, and never logs, formats or stores
    /// them. The zeroing is best effort: the implementation, the operating
    /// system's pipe buffers and the `docker` program may keep copies of
    /// their own. An implementation must not log the value either.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::NotFound`] when the environment has no
    /// variable `name`, which phase never asks for an absent optional
    /// secret, the error of `out`, and, by default,
    /// [`io::ErrorKind::Unsupported`] for an environment that cannot write
    /// a value, which phase reports as a container secret it cannot
    /// deliver as a file.
    fn write_value(&self, name: &str, out: &mut dyn io::Write) -> io::Result<()> {
        let _ = (name, out);
        Err(io::ErrorKind::Unsupported.into())
    }

    /// Sets each variable on `command` under its own name. A host process's
    /// keeper receives its environment this way.
    fn apply(&self, command: &mut Command) {
        self.apply_as(command, &|name| Some(name.to_owned()));
    }
}

/// The resolver that [`require`] names in its error: the environment
/// source as a whole, which returned an environment without the variable.
const SOURCE: &str = "the environment source";

/// The reason of a [`require`] error, which holds no value.
#[derive(Debug)]
struct LeftOut;

impl fmt::Display for LeftOut {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "the resolved environment does not name the variable, which the plan requires",
        )
    }
}

impl Error for LeftOut {}

/// Checks that `environment` names each variable of `required`, the
/// variables that a process receives and does not mark optional. phase
/// calls it wherever it applies a resolved environment: before a host
/// process's keeper starts, when a keeper resolves a fresh environment for
/// a restart, and before a container starts. A source that leaves out a
/// required variable therefore fails the start, and the process never runs
/// with the variable unset or empty.
///
/// # Errors
///
/// Returns an [`EnvironmentError`] that names the first required variable
/// that `environment` lacks, and holds no value.
pub(crate) fn require<'n>(
    required: impl IntoIterator<Item = &'n String>,
    environment: &dyn AppliedEnvironment,
) -> Result<(), EnvironmentError> {
    let names = environment.names();
    match required.into_iter().find(|name| !names.contains(*name)) {
        Some(name) => Err(EnvironmentError::new(
            name.clone(),
            SOURCE.to_owned(),
            Box::new(LeftOut),
        )),
        None => Ok(()),
    }
}

/// Resolves the environment of `process` of `app` through `source` before
/// `deadline`, and checks it with [`require`].
///
/// # Errors
///
/// Returns the source's [`EnvironmentError`], or that of [`require`].
pub(crate) fn resolve_required(
    source: &dyn EnvironmentSource,
    app: &AppName,
    process: &crate::ProcessPlan,
    deadline: Instant,
) -> Result<Box<dyn AppliedEnvironment>, EnvironmentError> {
    let environment = source.resolve(app, &process.name, deadline)?;
    require(process.required_env(), environment.as_ref())?;
    Ok(environment)
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
