//! The secret and generated variables of a host process's starmap file:
//! each variable whose source is `secret` or `generated`.
//!
//! The plan names each such variable and holds no value. A secret whose
//! bind names a reference, such as a 1Password reference, is read only for
//! a unit that a person granted that reference to, and a person grants it
//! once for the repository, the unit and the reference, in phase's secret
//! grant file. A secret without a bind is read from the login Keychain,
//! where a person saves it with `security add-generic-password` under the
//! service and account that its [`SecretFailure::keychain`] names, and a
//! generated variable is created in the Keychain on first use. Neither
//! needs a grant, because mxv reads no reference for them. The Keychain
//! keeps both under the service `starmap.mxv-<workspace id>` and the
//! account `<unit>/<NAME>`, so two workspaces of one repository hold
//! separate values, and `mxv remove` deletes them with [`forget_keychain`].
//!
//! [`UnitSecrets::resolve`] reads a unit's values through starmap-resolve
//! under one deadline of [`SECRET_WAIT`], before phase starts anything.
//! The values stay in memory, in [`SecretValues`], until phase starts the
//! unit through [`WithSecrets`], the environment source that adds them to
//! the manifest's own. A value that cannot be read leaves only its unit
//! out: a variable that is optional stays unset, with a warning, and any
//! other fails the unit with a [`SecretFailure`]. starmap 0.4.3 refuses a
//! default on a secret or generated variable, so no variable that mxv
//! reads here takes a default, and the branch of [`Fallback`] that would
//! give one is not reached.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use phase::{
    AppName, AppliedEnvironment, EnvironmentError, EnvironmentSource, ProcessName, SecretRead,
    WorkspaceId,
};
use sha2::{Digest as _, Sha256};
use starmap::{Bindings, Bound, Mode, Name, Secret, SecretRef, Source, Spec, Unset};
use starmap_resolve::{
    Context, Keychain, Lookup, Places, ResolveProblem, Resolver, SecretError, resolve,
};

use super::{Inputs, ManifestEnvironment, Unrunnable, UnrunnableKind, placed, provided};
use crate::error::Error;
use crate::manifest::{Manifest, Process};
use crate::phase_plan::CallerEnvironment;
use crate::runs::ProcessNote;

#[cfg(test)]
mod tests;

/// How long the secrets of one unit may take to resolve, from the first
/// store's read to the last one's answer. A 1Password prompt that waits
/// for the person counts against it, and a unit whose secrets pass it
/// fails alone.
pub const SECRET_WAIT: Duration = Duration::from_secs(20);

/// The store of a secret whose bind names a 1Password reference.
pub const ONE_PASSWORD: &str = "1password";

/// The store of a generated value and of a secret without a bind.
pub const KEYCHAIN: &str = "keychain";

/// The program that reads 1Password references.
const OP: &str = "op";

/// The variables of mxv's own environment that tell `op` which account to
/// use or that it need not prompt, which starmap's agent reads. No process
/// receives them.
const ONE_PASSWORD_VARIABLES: &[&str] = &["OP_ACCOUNT", "OP_SERVICE_ACCOUNT_TOKEN"];

/// The prefix of the variables that hold a session that `op signin`
/// printed.
const ONE_PASSWORD_SESSION: &str = "OP_SESSION_";

/// Reports whether `name` is a variable of mxv's environment that `op` and
/// starmap's agent read, which no process receives.
pub(crate) fn reads_one_password(name: &str) -> bool {
    ONE_PASSWORD_VARIABLES.contains(&name) || name.starts_with(ONE_PASSWORD_SESSION)
}

/// Where the value of a secret variable comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SecretSource {
    /// The bind names `reference` in `store`, such as
    /// `op://acme-staging/auth/credential` in `1password`. mxv reads it
    /// only for a unit that a person granted it to.
    Reference {
        /// The store that holds the value, such as `1password`.
        store: String,
        /// The reference the store reads.
        reference: String,
    },
    /// A random value that the Keychain keeps, created on first use.
    Generated,
    /// A secret without a bind, which the login Keychain keeps once a
    /// person saved it under the service and account that
    /// [`SecretFailure::keychain`] names.
    Saved,
}

impl SecretSource {
    /// Returns the store that holds the value.
    #[must_use]
    pub fn store(&self) -> &str {
        match self {
            Self::Reference { store, .. } => store,
            Self::Generated | Self::Saved => KEYCHAIN,
        }
    }

    /// Returns the reference the store reads, or `None` for a value that
    /// the Keychain keeps under the workspace.
    #[must_use]
    pub fn reference(&self) -> Option<&str> {
        match self {
            Self::Reference { reference, .. } => Some(reference),
            Self::Generated | Self::Saved => None,
        }
    }
}

/// Why a unit did not start because of one of its secret variables.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SecretProblem {
    /// No person granted the unit the variable's reference, because no
    /// terminal could ask, `--no-input` was given, or the answer was not
    /// yes.
    NotGranted,
    /// The store could not give the value, or did not answer within
    /// [`SECRET_WAIT`].
    Unavailable {
        /// Why, in a clause that holds no value.
        reason: String,
    },
}

/// A unit that `up` left out because of one of its secret variables, which
/// is required and declares no default.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SecretFailure {
    /// The unit.
    pub unit: String,
    /// The variable.
    pub variable: String,
    /// Where its value comes from.
    pub source: SecretSource,
    /// The starmap file that declares it.
    pub file: PathBuf,
    /// What is wrong.
    pub problem: SecretProblem,
    /// The `sign_in` of the manifest's `[secrets.*]` table that matches the
    /// variable, by its name or by a check command that names the
    /// reference, or `None` when no table matches.
    pub sign_in: Option<String>,
    /// The Keychain item that holds the value of a secret without a bind,
    /// where a person saves it, or `None` for any other variable.
    pub keychain: Option<KeychainItem>,
}

/// The login Keychain item of a secret without a bind: a generic password
/// whose service and account these are.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct KeychainItem {
    /// The service, `starmap.mxv-<workspace id>`.
    pub service: String,
    /// The account, `<app>/<NAME>`, where the app is the unit's name when
    /// it is already lower case letters, digits and hyphens, and otherwise
    /// starmap's form of the name with a short hash of it after it.
    pub account: String,
}

impl KeychainItem {
    /// Returns the command that saves the item's value. `security` asks
    /// for the value when `-w` comes last, so it never reaches a command
    /// line.
    #[must_use]
    pub fn save_command(&self) -> String {
        format!(
            "security add-generic-password -U -s {} -a {} -w",
            self.service, self.account
        )
    }
}

/// One secret that a person is asked to let mxv read: the unit that reads
/// it, the variable, the store and the reference, and the file that names
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AskedSecret {
    /// The unit.
    pub unit: String,
    /// The variable.
    pub variable: String,
    /// The store, such as `1password`.
    pub store: String,
    /// The reference.
    pub reference: String,
    /// The starmap file that names it.
    pub file: PathBuf,
}

/// What a declared variable falls back to when its value cannot be read.
#[derive(Debug)]
enum Fallback {
    /// The variable must have a value, so its unit fails.
    Required,
    /// The variable may stay unset.
    Optional,
    /// The variable takes this default, as its spec writes it. starmap
    /// 0.4.3 refuses a default on a secret or generated variable, so no
    /// variable that mxv reads takes this branch.
    Default(Secret),
}

impl Fallback {
    /// Returns what the variable `variable` falls back to in local mode.
    fn of(variable: &starmap::Variable) -> Self {
        match variable.unset().in_mode(Mode::Local) {
            Unset::Required => Self::Required,
            Unset::Optional => Self::Optional,
            Unset::Default(value) => Self::Default(Secret::new(value.to_string())),
        }
    }
}

/// One secret or generated variable of a unit's starmap file.
#[derive(Debug)]
struct Declared {
    /// The variable.
    name: Name,
    /// Where its value comes from.
    source: SecretSource,
    /// What it falls back to.
    fallback: Fallback,
}

/// The secret and generated variables of one host process's starmap file.
#[derive(Debug)]
pub(crate) struct UnitSecrets {
    /// The unit.
    unit: String,
    /// The starmap file.
    file: PathBuf,
    /// The file's spec.
    spec: Spec,
    /// The directory that the unit's source is.
    source: PathBuf,
    /// Each variable, in the spec's order.
    declared: Vec<Declared>,
}

/// Returns the secret and generated variables of the starmap file of
/// `process`, named `name`, or `None` when the plan reads no starmap file,
/// the process has none, or it declares no such variable. A variable that
/// the manifest's `env` or `ports` sets is left out, because the manifest's
/// value replaces it.
///
/// # Errors
///
/// Returns the errors of reading the starmap file, and [`Unrunnable`] for a
/// variable that phase gives every process itself, such as `PATH`.
pub(crate) fn of_unit(
    inputs: &Inputs<'_>,
    name: &str,
    process: &Process,
) -> Result<Option<UnitSecrets>, Unrunnable> {
    let Some((spec, file)) = provided::spec_of(inputs, name, process)? else {
        return Ok(None);
    };
    let declared: Vec<Declared> = spec
        .variables()
        .filter(|(variable, _)| {
            !process.env.contains_key(variable.as_str())
                && !process.ports.contains_key(variable.as_str())
        })
        .filter_map(|(variable, declared)| {
            let source = match declared.source() {
                Source::Secret { bind: Some(bind) } => match bind.reference() {
                    SecretRef::OnePassword(reference) => SecretSource::Reference {
                        store: ONE_PASSWORD.to_owned(),
                        reference: reference.to_string(),
                    },
                    // starmap marks the reference non-exhaustive, so a
                    // store that a later starmap adds is read by no
                    // resolver here until mxv learns it.
                    _ => return None,
                },
                Source::Secret { bind: None } => SecretSource::Saved,
                Source::Generated { .. } => SecretSource::Generated,
                // starmap marks the source non-exhaustive; a source that a
                // later starmap adds is no secret of mxv's until it learns
                // it.
                Source::Setting { .. } | Source::Inherited | Source::Provided(_) | _ => {
                    return None;
                }
            };
            Some(Declared {
                name: variable.clone(),
                source,
                fallback: Fallback::of(declared),
            })
        })
        .collect();
    if declared.is_empty() {
        return Ok(None);
    }
    if let Some(variable) = declared
        .iter()
        .find(|declared| super::PHASE_SETS.contains(&declared.name.as_str()))
    {
        return Err(Unrunnable {
            kind: UnrunnableKind::Plan,
            unit: Some(name.to_owned()),
            message: format!(
                "the starmap file of the process `{name}` declares {} as a secret, which phase gives every process from mxv's own environment",
                variable.name.as_str()
            ),
        });
    }
    let (source, _) = placed(inputs, name, process)?;
    Ok(Some(UnitSecrets {
        unit: name.to_owned(),
        file,
        spec,
        source,
        declared,
    }))
}

/// Returns the names of the secret and generated variables of `process`,
/// named `name`, which the plan names and gives no value.
///
/// # Errors
///
/// Returns the errors of [`of_unit`].
pub(crate) fn names(
    inputs: &Inputs<'_>,
    name: &str,
    process: &Process,
) -> Result<Vec<String>, Unrunnable> {
    Ok(of_unit(inputs, name, process)?
        .map(|secrets| secrets.names().map(str::to_owned).collect())
        .unwrap_or_default())
}

impl UnitSecrets {
    /// Returns the unit.
    pub(crate) fn unit(&self) -> &str {
        &self.unit
    }

    /// Returns the names of the variables.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.declared.iter().map(|declared| declared.name.as_str())
    }

    /// Returns each reference that the unit reads, which a person grants.
    pub(crate) fn reads(&self) -> Vec<SecretRead> {
        self.declared
            .iter()
            .filter_map(|declared| match &declared.source {
                SecretSource::Reference { store, reference } => {
                    Some(SecretRead::new(declared.name.as_str(), store, reference))
                }
                SecretSource::Generated | SecretSource::Saved => None,
            })
            .collect()
    }

    /// Returns the secret of `reads`, which no person granted, as a person
    /// is asked about it.
    pub(crate) fn asked(&self, reads: &[SecretRead]) -> Vec<AskedSecret> {
        reads
            .iter()
            .map(|read| AskedSecret {
                unit: self.unit.clone(),
                variable: read.variable().to_owned(),
                store: read.store().to_owned(),
                reference: read.reference().to_owned(),
                file: self.file.clone(),
            })
            .collect()
    }

    /// Returns the failure of the first required variable of `missing`,
    /// the reads that no person granted, or `None` when each of them falls
    /// back to a default or may stay unset. `manifest` gives the sign-in.
    pub(crate) fn not_granted(
        &self,
        missing: &[SecretRead],
        manifest: &Manifest,
    ) -> Option<SecretFailure> {
        self.declared
            .iter()
            .filter(|declared| matches!(declared.fallback, Fallback::Required))
            .find(|declared| {
                missing
                    .iter()
                    .any(|read| read.variable() == declared.name.as_str())
            })
            .map(|declared| self.failure(declared, SecretProblem::NotGranted, manifest))
    }

    /// Returns the failure of every required variable that `reason`
    /// keeps from being read, such as a secret grant file that cannot be
    /// read, or `None` when the unit reads no reference.
    pub(crate) fn unreadable(&self, reason: &str, manifest: &Manifest) -> Option<SecretFailure> {
        self.declared
            .iter()
            .filter(|declared| declared.source.reference().is_some())
            .find(|declared| matches!(declared.fallback, Fallback::Required))
            .map(|declared| {
                self.failure(
                    declared,
                    SecretProblem::Unavailable {
                        reason: reason.to_owned(),
                    },
                    manifest,
                )
            })
    }

    /// Returns the failure of `declared` with `problem`.
    fn failure(
        &self,
        declared: &Declared,
        problem: SecretProblem,
        manifest: &Manifest,
    ) -> SecretFailure {
        SecretFailure {
            unit: self.unit.clone(),
            variable: declared.name.as_str().to_owned(),
            source: declared.source.clone(),
            file: self.file.clone(),
            problem,
            sign_in: sign_in(manifest, declared),
            keychain: None,
        }
    }

    /// Reads the value of each variable through `resolvers` by `deadline`,
    /// for the workspace whose stable id is `id`. A reference that
    /// `granted` lacks is never read. A variable whose value cannot be read
    /// takes its default, or stays unset when it is optional, with a
    /// warning that names it; any other fails the unit. `manifest` gives
    /// the sign-in of a failure.
    pub(crate) fn resolve(
        &self,
        id: &WorkspaceId,
        resolvers: &[&dyn Resolver],
        granted: &[SecretRead],
        deadline: Instant,
        manifest: &Manifest,
    ) -> Resolved {
        let allowed: BTreeSet<&str> = granted.iter().map(SecretRead::reference).collect();
        let guards: Vec<Guarded<'_>> = resolvers
            .iter()
            .map(|inner| Guarded {
                inner: *inner,
                allowed: &allowed,
            })
            .collect();
        let caller = Bindings::new();
        let workspace = keychain_workspace(id);
        let app = keychain_app(&self.unit);
        let places = Places::new(self.source.clone()).workspace(
            workspace.clone(),
            app.clone(),
            self.source.clone(),
        );
        let mut context = Context::new(places, &caller, deadline);
        for guard in &guards {
            context = context.resolver(guard);
        }
        let mut resolution = resolve(&self.spec, Mode::Local, &context);
        let mut values = Vec::new();
        let mut notes = Vec::new();
        for declared in &self.declared {
            let problem = if let Some(Bound::Secret(secret)) =
                resolution.bindings.get(&declared.name)
            {
                values.push((declared.name.as_str().to_owned(), secret.duplicate()));
                continue;
            } else if declared
                .source
                .reference()
                .is_some_and(|reference| !allowed.contains(reference))
            {
                SecretProblem::NotGranted
            } else {
                let index = resolution
                    .failures
                    .iter()
                    .position(|failure| failure.name == declared.name);
                match index {
                    Some(index) => SecretProblem::Unavailable {
                        reason: describe(&resolution.failures.swap_remove(index).problem, declared),
                    },
                    // An optional variable that no store holds stays
                    // unset, as starmap leaves it.
                    None => continue,
                }
            };
            match &declared.fallback {
                Fallback::Required => {
                    let mut failure = self.failure(declared, problem, manifest);
                    if declared.source == SecretSource::Saved {
                        failure.keychain = Some(KeychainItem {
                            service: format!("starmap.{}", workspace.as_str()),
                            account: format!("{}/{}", app.as_str(), declared.name.as_str()),
                        });
                    }
                    return Resolved::Failed(failure);
                }
                Fallback::Default(default) => {
                    values.push((declared.name.as_str().to_owned(), default.duplicate()));
                    notes.push(self.fell_back(declared, &problem, "started it with the default"));
                }
                Fallback::Optional => {
                    notes.push(self.fell_back(declared, &problem, "left it unset"));
                }
            }
        }
        Resolved::Ready { values, notes }
    }

    /// Returns the warning that `declared` fell back, as `did` says,
    /// because of `problem`.
    fn fell_back(&self, declared: &Declared, problem: &SecretProblem, did: &str) -> ProcessNote {
        let why = match problem {
            SecretProblem::NotGranted => "no person granted it".to_owned(),
            SecretProblem::Unavailable { reason } => reason.clone(),
        };
        let from = declared.source.reference().map_or_else(
            || format!("the {}", declared.source.store()),
            |reference| reference.to_owned(),
        );
        ProcessNote::SecretDefaulted {
            variable: declared.name.as_str().to_owned(),
            message: format!(
                "could not read the secret {} from {from}, because {why}, so mxv {did}",
                declared.name.as_str()
            ),
        }
    }
}

/// What [`UnitSecrets::resolve`] read for one unit.
#[derive(Debug)]
pub(crate) enum Resolved {
    /// The unit starts with these values, and with a warning for each
    /// variable that fell back.
    Ready {
        /// The value of each variable that has one.
        values: Vec<(String, Secret)>,
        /// One warning for each variable that fell back.
        notes: Vec<ProcessNote>,
    },
    /// A required variable has no value, so the unit does not start.
    Failed(SecretFailure),
}

/// Returns why `problem` left `declared` without a value, in a clause that
/// holds no value.
fn describe(problem: &ResolveProblem, declared: &Declared) -> String {
    let keychain = !matches!(declared.source, SecretSource::Reference { .. });
    match problem {
        ResolveProblem::Secret(SecretError::Unsupported) | ResolveProblem::NoResolver
            if keychain && !cfg!(target_os = "macos") =>
        {
            "the Keychain, which keeps generated and saved secrets, exists only on macOS".to_owned()
        }
        ResolveProblem::Secret(error) => error.to_string(),
        ResolveProblem::Deadline => format!(
            "the stores did not answer within {} seconds",
            SECRET_WAIT.as_secs()
        ),
        ResolveProblem::Unbound => "the login Keychain holds no value saved for it".to_owned(),
        ResolveProblem::NoResolver => "no store reads it".to_owned(),
        // A problem of providers or places, or one that a later
        // starmap-resolve adds, has no words of mxv's own.
        ResolveProblem::NoProvider(_)
        | ResolveProblem::Provider(_)
        | ResolveProblem::NoPlace(_)
        | _ => "starmap could not resolve it".to_owned(),
    }
}

/// Returns the `sign_in` of the `[secrets.*]` table of `manifest` that
/// matches `declared`: one named after the variable, in any case, or one
/// whose check command names the variable's reference.
fn sign_in(manifest: &Manifest, declared: &Declared) -> Option<String> {
    let variable = declared.name.as_str();
    manifest
        .secrets
        .iter()
        .find(|(name, secret)| {
            name.eq_ignore_ascii_case(variable)
                || declared.source.reference().is_some_and(|reference| {
                    secret
                        .check
                        .words()
                        .iter()
                        .any(|word| word.contains(reference))
                })
        })
        .map(|(_, secret)| secret.sign_in.clone())
}

/// A resolver that hands each lookup to `inner`, except a reference that
/// `allowed` lacks, which it refuses without reading, so an ungranted
/// reference never reaches a store.
struct Guarded<'a> {
    /// The resolver that reads.
    inner: &'a dyn Resolver,
    /// The references that a person granted.
    allowed: &'a BTreeSet<&'a str>,
}

impl Guarded<'_> {
    /// Reports whether `lookup` names a reference that no person granted.
    fn refuses(&self, lookup: &Lookup<'_>) -> bool {
        match lookup {
            Lookup::Reference { reference, .. } => {
                !self.allowed.contains(reference_text(reference).as_str())
            }
            // mxv reads no reference for these, so they need no grant.
            Lookup::Generated { .. } | Lookup::Saved { .. } | _ => false,
        }
    }
}

/// Returns the text of `reference`, as the grant file writes it.
fn reference_text(reference: &SecretRef) -> String {
    match reference {
        SecretRef::OnePassword(reference) => reference.to_string(),
        // starmap marks the reference non-exhaustive, and mxv grants no
        // reference of a store it does not know.
        _ => String::new(),
    }
}

impl Resolver for Guarded<'_> {
    fn prepare(&self, lookups: &[Lookup<'_>], deadline: Instant) {
        let allowed: Vec<Lookup<'_>> = lookups
            .iter()
            .filter(|lookup| !self.refuses(lookup))
            .copied()
            .collect();
        self.inner.prepare(&allowed, deadline);
    }

    fn finish(&self) {
        self.inner.finish();
    }

    fn resolve(&self, lookup: Lookup<'_>, deadline: Instant) -> Result<Secret, SecretError> {
        if self.refuses(&lookup) {
            return Err(SecretError::Denied);
        }
        self.inner.resolve(lookup, deadline)
    }
}

/// Returns the starmap workspace name that the Keychain keeps the values of
/// the workspace whose stable id is `id` under: `mxv-<id>`, so its service
/// is `starmap.mxv-<id>`. A starmap name starts with a letter, and a stable
/// id may start with a digit, so the name has the prefix.
pub(crate) fn keychain_workspace(id: &WorkspaceId) -> starmap_resolve::WorkspaceName {
    let app = starmap_resolve::AppName::of_dir(Path::new(&format!("mxv-{}", id.as_str())));
    starmap_resolve::WorkspaceName::from(&app)
}

/// Returns the app of the unit `unit` in its Keychain accounts. A unit
/// name that is already an app name is kept. Any other, such as `web_api`,
/// which starmap would write as `web-api`, gets a suffix of the first 8 hex
/// digits of the SHA-256 of the name, so units `web_api` and `web-api` of
/// one workspace keep separate values.
pub(crate) fn keychain_app(unit: &str) -> starmap_resolve::AppName {
    let app = starmap_resolve::AppName::of_dir(Path::new(unit));
    if app.as_str() == unit {
        return app;
    }
    let digest = Sha256::digest(unit.as_bytes());
    let suffix: String = digest
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    starmap_resolve::AppName::of_dir(Path::new(&format!("{}-{suffix}", app.as_str())))
}

/// Returns the Keychain service of the workspace whose stable id is `id`.
#[must_use]
pub fn keychain_service(id: &WorkspaceId) -> String {
    format!("starmap.{}", keychain_workspace(id).as_str())
}

/// Deletes the Keychain items of the workspace whose stable id is `id`,
/// which `mxv remove` calls before it deletes the workspace's record and
/// `mxv remove --archived` calls before it deletes the archive's record,
/// and returns how many it deleted. A platform without a Keychain holds none.
///
/// # Errors
///
/// Returns [`Error::KeychainRefused`] when the Keychain refuses, such as
/// when it is locked or the person refuses its prompt. Some items may then
/// be gone, and a second call deletes the rest.
pub fn forget_keychain(id: &WorkspaceId) -> Result<usize, Error> {
    match Keychain::new().forget(&keychain_workspace(id)) {
        Ok(count) => Ok(count),
        Err(SecretError::Unsupported) => Ok(0),
        Err(error) => Err(Error::KeychainRefused {
            service: keychain_service(id),
            reason: error.to_string(),
        }),
    }
}

/// The stores that `up` reads secrets from, built for each `up` from mxv's
/// environment. [`SecretStores::system`] reads 1Password through
/// starmap's agent and `op`, and keeps generated values in the Keychain.
#[derive(Clone)]
pub(crate) struct SecretStores(std::sync::Arc<MakeResolvers>);

/// A function that builds the resolvers for the environment that mxv's
/// `main` read.
type MakeResolvers = dyn Fn(&CallerEnvironment) -> Vec<Box<dyn Resolver>> + Send + Sync;

impl std::fmt::Debug for SecretStores {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretStores")
    }
}

impl Default for SecretStores {
    fn default() -> Self {
        Self::system()
    }
}

impl SecretStores {
    /// Returns the stores of the machine: 1Password, read through
    /// starmap's agent when one runs and through `op` otherwise, and the
    /// Keychain.
    pub(crate) fn system() -> Self {
        Self(std::sync::Arc::new(|caller| {
            vec![one_password(caller), Box::new(Keychain::new())]
        }))
    }

    /// Returns the resolvers for `caller`, the environment that mxv's
    /// `main` read.
    pub(crate) fn resolvers(&self, caller: &CallerEnvironment) -> Vec<Box<dyn Resolver>> {
        (self.0)(caller)
    }
}

/// Returns the resolver of 1Password for `caller`: starmap's agent, which
/// keeps values that `op` read and reads a miss with `op`, when the `op`
/// program lies on the caller's `PATH`, `OP_ACCOUNT` names an account and
/// the agent's socket is the person's own, and otherwise `op` alone. The
/// agent is never started here, so a resolution that finds none reads
/// from `op`.
fn one_password(caller: &CallerEnvironment) -> Box<dyn Resolver> {
    let program = caller
        .path_dirs()
        .into_iter()
        .map(|dir| dir.join(OP))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from(OP));
    agent(&program, caller).unwrap_or_else(|| Box::new(starmap_resolve::OnePassword::new(program)))
}

/// Returns starmap's agent for `program` and `caller`, or `None` when it
/// cannot be used, as [`one_password`] says.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn agent(program: &Path, caller: &CallerEnvironment) -> Option<Box<dyn Resolver>> {
    use starmap_resolve::agent::{Agent, AgentClient, AgentSocket, CacheScope};
    let scope = CacheScope::new(program.to_path_buf(), caller.one_password()).ok()?;
    let socket = AgentSocket::for_user().ok()?;
    Some(Box::new(Agent::new(AgentClient::new(socket), scope)))
}

/// Returns `None`, because starmap's agent runs only on macOS and Linux.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn agent(_program: &Path, _caller: &CallerEnvironment) -> Option<Box<dyn Resolver>> {
    None
}

/// The values of each unit's secret variables, which [`WithSecrets`]
/// gives phase when it starts the unit. They live only in memory, and each
/// is zeroed when it is dropped.
#[derive(Debug, Default)]
pub(crate) struct SecretValues {
    /// The values of each app, by variable name.
    values: BTreeMap<AppName, Vec<(String, Secret)>>,
}

impl SecretValues {
    /// Keeps `values` for `app`.
    pub(crate) fn insert(&mut self, app: AppName, values: Vec<(String, Secret)>) {
        self.values.insert(app, values);
    }
}

/// The environment source of `up`: the manifest's values of each process,
/// with the values of its secret variables added.
pub(crate) struct WithSecrets<'a> {
    /// The manifest's values.
    pub(crate) manifest: &'a ManifestEnvironment,
    /// The secrets.
    pub(crate) secrets: &'a SecretValues,
    /// The apps whose secrets mxv did not read, because phase was to leave
    /// them running, each with one secret variable it declares. phase
    /// resolves an environment for such an app only when its process ended
    /// after mxv asked, and then the app is refused, so that `up` fails and
    /// the next `up` reads its secrets.
    pub(crate) unread: &'a BTreeMap<AppName, String>,
}

impl EnvironmentSource for WithSecrets<'_> {
    fn resolve(
        &self,
        app: &AppName,
        process: &ProcessName,
        deadline: Instant,
    ) -> Result<Box<dyn AppliedEnvironment>, EnvironmentError> {
        if let Some(variable) = self.unread.get(app) {
            return Err(EnvironmentError::new(
                variable.clone(),
                "mxv".to_owned(),
                format!(
                    "mxv read no secret of {app}, because it was running when `up` began; run `mxv up` again to read them"
                )
                .into(),
            ));
        }
        let base = self.manifest.resolve(app, process, deadline)?;
        let secrets = self
            .secrets
            .values
            .get(app)
            .map(|values| {
                values
                    .iter()
                    .map(|(name, secret)| (name.clone(), secret.duplicate()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Box::new(AppliedSecrets { base, secrets }))
    }
}

/// The resolved variables of one process with its secrets.
struct AppliedSecrets {
    /// The manifest's values.
    base: Box<dyn AppliedEnvironment>,
    /// The secrets, by variable name, which replace a manifest value of the
    /// same name.
    secrets: Vec<(String, Secret)>,
}

impl AppliedSecrets {
    /// Returns the secret `name`, if the process has one.
    fn secret(&self, name: &str) -> Option<&Secret> {
        self.secrets
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, secret)| secret)
    }
}

impl AppliedEnvironment for AppliedSecrets {
    fn names(&self) -> BTreeSet<String> {
        let mut names = self.base.names();
        names.extend(self.secrets.iter().map(|(name, _)| name.clone()));
        names
    }

    fn expires_at(&self) -> Option<SystemTime> {
        self.base.expires_at()
    }

    /// Returns SHA-256 over `key`, the manifest values' own digest under
    /// `key`, and each secret's name and value, each followed by a zero
    /// byte.
    fn digest(&self, key: &[u8; 32]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(key);
        hasher.update(self.base.digest(key));
        for (name, secret) in &self.secrets {
            hasher.update(name.as_bytes());
            hasher.update([0]);
            hasher.update(secret.expose().as_bytes());
            hasher.update([0]);
        }
        hasher.finalize().into()
    }

    fn apply_as(&self, command: &mut Command, reference: &dyn Fn(&str) -> Option<String>) {
        self.base.apply_as(command, &|name| {
            if self.secret(name).is_some() {
                None
            } else {
                reference(name)
            }
        });
        for (name, secret) in &self.secrets {
            if let Some(reference) = reference(name) {
                command.env(reference, secret.expose());
            }
        }
    }

    fn write_value(&self, name: &str, out: &mut dyn io::Write) -> io::Result<()> {
        match self.secret(name) {
            Some(secret) => out.write_all(secret.expose().as_bytes()),
            None => self.base.write_value(name, out),
        }
    }
}
