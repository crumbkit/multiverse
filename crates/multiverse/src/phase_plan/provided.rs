//! The provided variables of a host process's starmap file: each variable
//! whose source is `provided` and whose bind's kind is `multiverse/port` or
//! `multiverse/url`.
//!
//! A bind names a unit of the workspace's merged manifest with `process` or
//! `app`, such as `{ provided = "multiverse/url", app = "catalogue-api" }`,
//! and may name one of that unit's ports with `port`. Without `port` it
//! names the port the unit listens on. [`PortProvider`] gives the host port
//! that the workspace holds for that port. [`UrlProvider`] gives the URL of
//! the proxy's route of that port, or `http://127.0.0.1:<port>` when the
//! proxy serves no route of it.
//!
//! The process's starmap file is the one its `starmap` key names, or
//! `<cwd>/starmap.toml` when the key is absent and that file exists. A
//! process with neither has no provided variables.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use starmap::{Bindings, Bound, Mode, Name, ProvidedRef, ProviderKind, Source, Spec, Unset};
use starmap_resolve::{
    Context, Places, Provider, ProviderError, ResolveProblem, check_binds, resolve,
};

use super::{Inputs, ProxyUrls, Unrunnable, UnrunnableKind, placed};
use crate::manifest::{Manifest, Process, ReadyCheck, Unit};

#[cfg(test)]
mod tests;

/// The kind of a bind whose value is a host port.
pub(crate) const PORT_KIND: &str = "multiverse/port";

/// The kind of a bind whose value is a URL.
pub(crate) const URL_KIND: &str = "multiverse/url";

/// The name of the starmap file that a process without a `starmap` key
/// reads in its `cwd`.
const DEFAULT_FILE: &str = "starmap.toml";

/// How long the resolution of one process's provided variables may take.
/// No provider waits on anything, so it ends well within it.
const RESOLVE_WAIT: Duration = Duration::from_secs(5);

/// The qualifiers that a multiverse bind takes.
const QUALIFIERS: &[&str] = &["process", "app", "port"];

/// The value of `port` in the fields of a bind that names no port, which
/// asks for the port the unit listens on.
const LISTEN: &str = "listen";

/// What the two providers read: the workspace's merged manifest, its host
/// ports and the proxy's routes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Neighbours<'a> {
    /// The workspace's merged manifest.
    manifest: &'a Manifest,
    /// The workspace's host ports, by port name.
    ports: &'a BTreeMap<String, u16>,
    /// The proxy's routes of the workspace.
    urls: &'a ProxyUrls,
}

impl<'a> Neighbours<'a> {
    /// Returns what the providers read from `manifest`, `ports` and `urls`.
    pub(crate) fn new(
        manifest: &'a Manifest,
        ports: &'a BTreeMap<String, u16>,
        urls: &'a ProxyUrls,
    ) -> Self {
        Self {
            manifest,
            ports,
            urls,
        }
    }

    /// Returns the host port that `reference` names.
    fn number(&self, reference: &ProvidedRef) -> Result<u16, ProviderError> {
        let target = target(self.manifest, reference).map_err(|unknown| unknown.error())?;
        self.ports.get(&target.port).copied().ok_or_else(|| {
            ProviderError::new(format!(
                "the workspace holds no host port yet for the port `{}` of `{}`; `mxv up` gives it one",
                target.port, target.unit
            ))
        })
    }
}

/// The provider of `multiverse/port`, whose value is a host port.
#[derive(Debug)]
pub(crate) struct PortProvider<'a> {
    /// What it reads.
    neighbours: Neighbours<'a>,
    /// Its kind.
    kind: ProviderKind,
}

/// The provider of `multiverse/url`, whose value is the URL of a port.
#[derive(Debug)]
pub(crate) struct UrlProvider<'a> {
    /// What it reads.
    neighbours: Neighbours<'a>,
    /// Its kind.
    kind: ProviderKind,
}

/// Returns the two providers, which read `neighbours`.
///
/// # Errors
///
/// Returns [`Unrunnable`] when a provider's kind does not parse, which
/// this module's constants always do.
pub(crate) fn providers(
    neighbours: Neighbours<'_>,
) -> Result<(PortProvider<'_>, UrlProvider<'_>), Unrunnable> {
    Ok((
        PortProvider {
            neighbours,
            kind: kind(PORT_KIND)?,
        },
        UrlProvider {
            neighbours,
            kind: kind(URL_KIND)?,
        },
    ))
}

/// Returns the provider kind `text`.
fn kind(text: &str) -> Result<ProviderKind, Unrunnable> {
    text.parse().map_err(|error| Unrunnable {
        kind: UnrunnableKind::Plan,
        unit: None,
        message: format!("the provider kind {text} does not parse: {error}"),
    })
}

impl Provider for PortProvider<'_> {
    fn kind(&self) -> &ProviderKind {
        &self.kind
    }

    fn provide(
        &self,
        _name: &Name,
        reference: &ProvidedRef,
        _deadline: Instant,
    ) -> Result<OsString, ProviderError> {
        self.neighbours
            .number(reference)
            .map(|number| OsString::from(number.to_string()))
    }

    fn check(&self, _name: &Name, reference: &ProvidedRef) -> Result<(), ProviderError> {
        target(self.neighbours.manifest, reference)
            .map(drop)
            .map_err(|unknown| unknown.error())
    }
}

impl Provider for UrlProvider<'_> {
    fn kind(&self) -> &ProviderKind {
        &self.kind
    }

    fn provide(
        &self,
        _name: &Name,
        reference: &ProvidedRef,
        _deadline: Instant,
    ) -> Result<OsString, ProviderError> {
        let number = self.neighbours.number(reference)?;
        let target =
            target(self.neighbours.manifest, reference).map_err(|unknown| unknown.error())?;
        Ok(OsString::from(
            self.neighbours
                .urls
                .url(&target.port)
                .map_or_else(|| format!("http://127.0.0.1:{number}"), str::to_owned),
        ))
    }

    fn check(&self, _name: &Name, reference: &ProvidedRef) -> Result<(), ProviderError> {
        target(self.neighbours.manifest, reference)
            .map(drop)
            .map_err(|unknown| unknown.error())
    }
}

/// The unit and port that a bind names.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    /// The unit.
    unit: String,
    /// The port, by its name in the merged manifest.
    port: String,
}

/// A bind that names no port of the workspace: the unit and the port it
/// asks for, and what is wrong.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Unknown {
    /// The unit that the bind names, or empty when it names none.
    pub(crate) app: String,
    /// The port that the bind names, or [`LISTEN`] when it names none.
    pub(crate) port: String,
    /// What is wrong, in a clause.
    pub(crate) reason: String,
}

impl Unknown {
    /// Returns the provider error that states the problem.
    fn error(&self) -> ProviderError {
        ProviderError::new(self.reason.clone())
    }
}

/// Returns the unit and port of `manifest` that `reference` names.
///
/// # Errors
///
/// Returns [`Unknown`] when the bind has a qualifier other than `process`,
/// `app` and `port`, names both `process` and `app` or neither, names a
/// unit the manifest does not have, names a port the unit does not have,
/// or names no port and the unit listens on none.
fn target(manifest: &Manifest, reference: &ProvidedRef) -> Result<Target, Unknown> {
    let process = reference.qualifier("process");
    let app = reference.qualifier("app");
    let named = process.or(app).unwrap_or_default().to_owned();
    let port = reference.qualifier("port");
    let asked = port.unwrap_or(LISTEN).to_owned();
    let unknown = |reason: String| Unknown {
        app: named.clone(),
        port: asked.clone(),
        reason,
    };
    if let Some((key, _)) = reference
        .qualifiers()
        .find(|(key, _)| !QUALIFIERS.contains(key))
    {
        return Err(unknown(format!(
            "the bind has the qualifier `{key}`, and a multiverse bind takes only `process` or `app`, and `port`"
        )));
    }
    if let (Some(process), Some(app)) = (process, app) {
        return Err(unknown(format!(
            "the bind names both `process = \"{process}\"` and `app = \"{app}\"`, and names its unit with only one of them"
        )));
    }
    if named.is_empty() {
        return Err(unknown(
            "the bind names no unit, and names one with `process` or `app`".to_owned(),
        ));
    }
    let Some(unit) = manifest.unit(&named) else {
        return Err(unknown(format!(
            "the workspace's manifest has no unit `{named}`, so it has no port `{asked}` of `{named}`"
        )));
    };
    let ports = unit_ports(unit);
    let port = match port {
        Some(port) if ports.contains(port) => port.to_owned(),
        Some(port) => {
            return Err(unknown(format!(
                "`{named}` has no port `{port}`; its ports are {}",
                listed(&ports)
            )));
        }
        None => match unit {
            Unit::Process(Process {
                listen: Some(listen),
                ..
            }) => listen.clone(),
            Unit::Process(_) | Unit::Container(_) => {
                return Err(unknown(format!(
                    "`{named}` listens on no port, so the bind names one of its ports with `port`; its ports are {}",
                    listed(&ports)
                )));
            }
        },
    };
    Ok(Target {
        unit: named.clone(),
        port,
    })
}

/// Returns the ports that `unit` uses: a process's `ports`, its `listen`,
/// the ports its `env` names and its readiness port, and a container's
/// published ports.
fn unit_ports(unit: Unit<'_>) -> BTreeSet<&str> {
    match unit {
        Unit::Process(process) => {
            let mut ports: BTreeSet<&str> = process.ports.values().map(String::as_str).collect();
            ports.extend(process.env.values().flat_map(|value| value.ports()));
            ports.extend(process.listen.as_deref());
            if let Some(ReadyCheck::Port(port)) = process.ready.as_ref().map(|ready| &ready.check) {
                ports.insert(port);
            }
            ports
        }
        Unit::Container(container) => container
            .ports
            .iter()
            .map(|published| published.port.as_str())
            .collect(),
    }
}

/// Returns `ports` as a list of names in backticks, or `none`.
fn listed(ports: &BTreeSet<&str>) -> String {
    if ports.is_empty() {
        return "none".to_owned();
    }
    ports
        .iter()
        .map(|port| format!("`{port}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Returns the starmap file of `process`, as an absolute path when the
/// plan's checkout is one, and whether its `starmap` key names it.
fn file_of(inputs: &Inputs<'_>, process: &Process) -> (PathBuf, bool) {
    match &process.starmap {
        Some(starmap) => (inputs.checkout.join(starmap), true),
        None => (inputs.checkout.join(&process.cwd).join(DEFAULT_FILE), false),
    }
}

/// Returns the spec of `process`, named `name`, and its file, or `None`
/// when the plan reads no starmap file or the process has none.
///
/// # Errors
///
/// Returns [`Unrunnable`] of [`UnrunnableKind::StarmapInvalid`] when the
/// file cannot be read or does not parse.
fn spec_of(
    inputs: &Inputs<'_>,
    name: &str,
    process: &Process,
) -> Result<Option<(Spec, PathBuf)>, Unrunnable> {
    if !inputs.read_starmaps {
        return Ok(None);
    }
    let (file, named) = file_of(inputs, process);
    let invalid = |reason: String| Unrunnable {
        kind: UnrunnableKind::StarmapInvalid { file: file.clone() },
        unit: Some(name.to_owned()),
        message: format!("the starmap file {} of `{name}` {reason}", file.display()),
    };
    let text = match crate::store::read_to_string(&file) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound && !named => return Ok(None),
        Err(error) => return Err(invalid(format!("cannot be read: {error}"))),
    };
    let spec = Spec::parse(&text).map_err(|error| invalid(format!("does not parse: {error}")))?;
    Ok(Some((spec, file)))
}

/// Returns the provided variables of `spec` whose kind is one of the two
/// providers', with their binds.
fn provided_variables(spec: &Spec) -> impl Iterator<Item = (&Name, &ProvidedRef)> {
    spec.variables().filter_map(|(name, variable)| {
        if let Source::Provided(reference) = variable.source()
            && [PORT_KIND, URL_KIND].contains(&reference.kind().as_str())
        {
            Some((name, reference))
        } else {
            None
        }
    })
}

/// Returns the failure of the variable `variable` of the starmap file
/// `file` of `name`, whose bind `unknown` refuses.
fn refused(name: &str, file: &Path, variable: &Name, unknown: Unknown) -> Unrunnable {
    Unrunnable {
        message: format!(
            "the starmap file {} of `{name}` binds {} to the port `{}` of `{}`, and {}",
            file.display(),
            variable.as_str(),
            unknown.port,
            unknown.app,
            unknown.reason
        ),
        kind: UnrunnableKind::ProvidedUnknown {
            variable: variable.as_str().to_owned(),
            app: unknown.app,
            port: unknown.port,
        },
        unit: Some(name.to_owned()),
    }
}

/// Checks the bind of each provided variable of the starmap file of
/// `process`, named `name`, against the workspace's manifest, without
/// resolving any value. It needs no ports, so it runs before `up` gives the
/// workspace any.
///
/// # Errors
///
/// Returns [`Unrunnable`] of [`UnrunnableKind::StarmapInvalid`] for a file
/// that cannot be read or does not parse, and of
/// [`UnrunnableKind::ProvidedUnknown`] for the first bind that names no
/// port of the workspace.
pub(super) fn check(inputs: &Inputs<'_>, name: &str, process: &Process) -> Result<(), Unrunnable> {
    let Some((spec, file)) = spec_of(inputs, name, process)? else {
        return Ok(());
    };
    check_spec(inputs, name, &spec, &file)
}

/// Checks the binds of `spec`, the starmap file `file` of `name`, through
/// [`check_binds`].
fn check_spec(inputs: &Inputs<'_>, name: &str, spec: &Spec, file: &Path) -> Result<(), Unrunnable> {
    let neighbours = Neighbours::new(inputs.manifest, inputs.ports, &inputs.urls);
    let (port, url) = providers(neighbours)?;
    let Some(failure) = check_binds(spec, &[&port, &url]).into_iter().next() else {
        return Ok(());
    };
    let unknown = provided_variables(spec)
        .find(|(variable, _)| **variable == failure.name)
        .and_then(|(_, reference)| target(inputs.manifest, reference).err())
        .unwrap_or_else(|| Unknown {
            app: String::new(),
            port: LISTEN.to_owned(),
            reason: failure.error.to_string(),
        });
    Err(refused(name, file, &failure.name, unknown))
}

/// Returns the value of each provided variable of the starmap file of
/// `process`, named `name`, by variable name. A variable that the spec
/// marks optional and whose port has no host port yet is left out.
///
/// # Errors
///
/// Returns the errors of [`check`], and [`Unrunnable`] of
/// [`UnrunnableKind::ProvidedUnknown`] for a required variable whose port
/// has no host port.
pub(super) fn values(
    inputs: &Inputs<'_>,
    name: &str,
    process: &Process,
) -> Result<BTreeMap<String, String>, Unrunnable> {
    let Some((spec, file)) = spec_of(inputs, name, process)? else {
        return Ok(BTreeMap::new());
    };
    check_spec(inputs, name, &spec, &file)?;
    let wanted: BTreeMap<&Name, &ProvidedRef> = provided_variables(&spec).collect();
    if wanted.is_empty() {
        return Ok(BTreeMap::new());
    }
    let (source, _) = placed(inputs, name, process)?;
    let neighbours = Neighbours::new(inputs.manifest, inputs.ports, &inputs.urls);
    let (port, url) = providers(neighbours)?;
    // No caller value is passed, because a provider of each kind is
    // registered and a provided variable never takes the caller's value
    // then.
    let caller = Bindings::new();
    let context = Context::new(
        Places::new(source),
        &caller,
        crate::clock::now() + RESOLVE_WAIT,
    )
    .provider(&port)
    .provider(&url);
    let resolution = resolve(&spec, Mode::Local, &context);
    for failure in &resolution.failures {
        let Some(reference) = wanted.get(&failure.name) else {
            continue;
        };
        let required = spec.variables().any(|(variable, declared)| {
            variable == &failure.name
                && matches!(declared.unset().in_mode(Mode::Local), Unset::Required)
        });
        if !required {
            continue;
        }
        let unknown = target(inputs.manifest, reference)
            .err()
            .unwrap_or_else(|| Unknown {
                app: reference
                    .qualifier("process")
                    .or_else(|| reference.qualifier("app"))
                    .unwrap_or_default()
                    .to_owned(),
                port: reference.qualifier("port").unwrap_or(LISTEN).to_owned(),
                reason: if let ResolveProblem::Provider(error) = &failure.problem {
                    error.to_string()
                } else {
                    "starmap could not resolve it".to_owned()
                },
            });
        return Err(refused(name, &file, &failure.name, unknown));
    }
    Ok(resolution
        .bindings
        .iter()
        .filter(|(variable, _)| wanted.contains_key(variable))
        .filter_map(|(variable, bound)| {
            let value = if let Bound::Plain(raw) = bound {
                raw.clone().into_string().ok()?
            } else if let Bound::Secret(secret) = bound {
                secret.expose().to_owned()
            } else {
                return None;
            };
            Some((variable.as_str().to_owned(), value))
        })
        .collect())
}
