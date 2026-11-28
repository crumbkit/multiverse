//! The provided variables of a host process's starmap file: each variable
//! whose source is `provided` and whose bind's kind is `multiverse/port` or
//! `multiverse/url`.
//!
//! A bind names a unit with `process` or `app`: a unit of the process's own
//! checkout by its bare name, such as `app = "web"`, and a unit of another
//! checkout of the workspace as `<checkout>/<unit>`, such as
//! `{ provided = "multiverse/url", app = "catalogue/api" }`, where the
//! checkout is its folder in the workspace. A bare name that the own
//! checkout does not declare names a unit of the workspace's own manifest.
//! `port` may name one of that unit's ports by the name its own manifest
//! gives it. Without `port` it names the port the unit listens on. The
//! names stay the same whatever the workspace's other checkouts, because
//! [`Place`] turns them into the merged manifest's prefixed names. [`PortProvider`] gives the host port
//! that the workspace holds for that port. [`UrlProvider`] gives the URL of
//! the proxy's route of that port, or `http://127.0.0.1:<port>` when the
//! proxy serves no route of it.
//!
//! The process's starmap file is the one its `starmap` key names, or
//! `<cwd>/starmap.toml` when the key is absent and that file exists. A
//! process with neither has no provided variables.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use starmap::{Bindings, Bound, Mode, Name, ProvidedRef, ProviderKind, Source, Spec, Unset};
use starmap_resolve::{
    Context, Places, Provider, ProviderError, ResolveProblem, check_binds, resolve,
};

use super::{
    Inputs, ProvidedBind, ProxyUrls, RefusedBind, RefusedBinds, Unrunnable, UnrunnableKind, placed,
};
use crate::manifest::{Manifest, Process, ReadyCheck, StarmapFound, Unit};
use crate::runs::ProcessNote;

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
    /// The checkouts of the workspace and the one that holds the process.
    place: &'a Place,
}

impl<'a> Neighbours<'a> {
    /// Returns what the providers of the process at `place` read from
    /// `inputs`.
    pub(crate) fn new(inputs: &'a Inputs<'a>, place: &'a Place) -> Self {
        Self {
            manifest: inputs.manifest,
            ports: inputs.ports,
            urls: &inputs.urls,
            place,
        }
    }

    /// Returns the unit and port that `reference` names.
    fn target(&self, reference: &ProvidedRef) -> Result<Target, Unknown> {
        target(self.manifest, self.place, reference)
    }

    /// Returns the host port that `reference` names.
    fn number(&self, reference: &ProvidedRef) -> Result<u16, ProviderError> {
        let target = self.target(reference).map_err(|unknown| unknown.error())?;
        self.ports.get(&target.port).copied().ok_or_else(|| {
            ProviderError::new(format!(
                "the workspace holds no host port yet for {}; `mxv up` gives it one",
                port_phrase(&target.app, &target.shown)
            ))
        })
    }
}

/// The checkouts of a workspace, by name, and the one that holds a
/// process. A bind names a unit of the process's own checkout by its bare
/// name, and a unit of another checkout as `<checkout>/<unit>`. In a
/// workspace of several checkouts the merged manifest prefixes each
/// checkout's names, and the place turns a bind's names into merged ones.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Place {
    /// The names of the workspace's checkouts, their folders. It is empty
    /// when the plan does not know them.
    checkouts: Vec<String>,
    /// The checkout that holds the process, or `None` when the process
    /// runs from the workspace's own directory or the checkouts are not
    /// known.
    own: Option<String>,
}

impl Place {
    /// Returns the place of the process `name` of `inputs`. The checkouts
    /// are every root but the last, which is the workspace's own
    /// directory, as [`super::roots`] lists them.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`placed`].
    fn of(inputs: &Inputs<'_>, name: &str, process: &Process) -> Result<Self, Unrunnable> {
        let (source, _) = placed(inputs, name, process)?;
        let roots = inputs.roots.split_last().map_or(&[][..], |(_, rest)| rest);
        let folder = |path: &Path| {
            path.file_name()
                .map(|folder| folder.to_string_lossy().into_owned())
        };
        let checkouts: Vec<String> = roots.iter().filter_map(|root| folder(root)).collect();
        let own = roots
            .iter()
            .find(|root| super::lexical(root) == super::lexical(&source))
            .and_then(|root| folder(root));
        Ok(Self { checkouts, own })
    }

    /// Returns the merged name of `name`, which the manifest of `checkout`
    /// declares.
    fn merged(&self, checkout: Option<&str>, name: &str) -> String {
        match checkout {
            Some(checkout) if self.checkouts.len() > 1 => {
                crate::lookup::prefixed(&crate::lookup::checkout_prefix(checkout), name)
            }
            Some(_) | None => name.to_owned(),
        }
    }

    /// Reports whether `name` starts with the prefix of one of several
    /// checkouts, so it is a merged name of a checkout's unit, which a bind
    /// never names.
    fn is_prefixed(&self, name: &str) -> bool {
        self.checkouts.len() > 1
            && self.checkouts.iter().any(|checkout| {
                name.starts_with(&format!("{}-", crate::lookup::checkout_prefix(checkout)))
            })
    }

    /// Returns the name that the manifest of `checkout` gives the merged
    /// name `merged`.
    fn own_name<'n>(&self, checkout: Option<&str>, merged: &'n str) -> &'n str {
        match checkout {
            Some(checkout) if self.checkouts.len() > 1 => merged
                .strip_prefix(&format!("{}-", crate::lookup::checkout_prefix(checkout)))
                .unwrap_or(merged),
            Some(_) | None => merged,
        }
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
        self.neighbours
            .target(reference)
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
        let target = self
            .neighbours
            .target(reference)
            .map_err(|unknown| unknown.error())?;
        Ok(OsString::from(
            self.neighbours
                .urls
                .url(&target.port)
                .map_or_else(|| format!("http://127.0.0.1:{number}"), str::to_owned),
        ))
    }

    fn check(&self, _name: &Name, reference: &ProvidedRef) -> Result<(), ProviderError> {
        self.neighbours
            .target(reference)
            .map(drop)
            .map_err(|unknown| unknown.error())
    }
}

/// The unit and port that a bind names.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    /// The unit, by its name in the merged manifest.
    unit: String,
    /// The port, by its name in the merged manifest.
    port: String,
    /// The unit as the bind writes it.
    app: String,
    /// The port by its name in the unit's own manifest.
    shown: String,
}

/// A bind that names no port of the workspace: the unit and the port it
/// asks for, and what is wrong.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Unknown {
    /// The unit that the bind names, or empty when it names none.
    pub(crate) app: String,
    /// The port that the bind names, or [`LISTEN`] when it names none.
    pub(crate) port: String,
    /// The checkout that the bind names as `<checkout>/<unit>`, or `None`
    /// when it names a bare unit name or none.
    pub(crate) checkout: Option<String>,
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
fn target(manifest: &Manifest, place: &Place, reference: &ProvidedRef) -> Result<Target, Unknown> {
    let process = reference.qualifier("process");
    let app = reference.qualifier("app");
    let named = process.or(app).unwrap_or_default().to_owned();
    let port = reference.qualifier("port");
    let asked = port.unwrap_or(LISTEN).to_owned();
    let unknown = |reason: String| Unknown {
        app: named.clone(),
        port: asked.clone(),
        checkout: named
            .split_once('/')
            .map(|(checkout, _)| checkout.to_owned()),
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
    // `<checkout>/<unit>` names a unit of another checkout. A bare name is
    // a unit of the process's own checkout, or else of the workspace's own
    // manifest, whose names are not prefixed.
    let candidates: Vec<(Option<&str>, String)> = match named.split_once('/') {
        Some((checkout, bare)) => {
            if !place.checkouts.iter().any(|known| known == checkout) {
                return Err(unknown(format!(
                    "the workspace has no checkout `{checkout}`; its checkouts are {}",
                    listed(&place.checkouts.iter().map(String::as_str).collect())
                )));
            }
            vec![(Some(checkout), place.merged(Some(checkout), bare))]
        }
        None => {
            let mut candidates = vec![(
                place.own.as_deref(),
                place.merged(place.own.as_deref(), &named),
            )];
            if !place.is_prefixed(&named) {
                candidates.push((None, named.clone()));
            }
            candidates
        }
    };
    let Some((checkout, merged, unit)) = candidates
        .into_iter()
        .find_map(|(checkout, merged)| manifest.unit(&merged).map(|unit| (checkout, merged, unit)))
    else {
        return Err(unknown(format!(
            "the workspace's manifest has no unit `{named}`"
        )));
    };
    let ports = unit_ports(unit);
    let own_ports: BTreeSet<&str> = ports
        .iter()
        .map(|port| place.own_name(checkout, port))
        .collect();
    let port = match port {
        Some(port) if ports.contains(place.merged(checkout, port).as_str()) => {
            place.merged(checkout, port)
        }
        Some(port) => {
            return Err(unknown(format!(
                "`{named}` has no port `{port}`; its ports are {}",
                listed(&own_ports)
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
                    listed(&own_ports)
                )));
            }
        },
    };
    Ok(Target {
        shown: place.own_name(checkout, &port).to_owned(),
        unit: merged,
        port,
        app: named.clone(),
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
pub(super) fn spec_of(
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
    if !named {
        // The default file gets the checks that the manifest gives a file
        // its `starmap` key names: a regular file inside the process's
        // checkout, and no symbolic link.
        let (source, _) = placed(inputs, name, process)?;
        match crate::manifest::starmap_file_found(&file, &source) {
            Ok(()) => {}
            Err(StarmapFound::Missing) => return Ok(None),
            Err(found) => return Err(invalid(found_reason(found).to_owned())),
        }
    }
    let text = crate::store::read_to_string(&file)
        .map_err(|error| invalid(format!("cannot be read: {error}")))?;
    let spec = Spec::parse(&text).map_err(|error| invalid(format!("does not parse: {error}")))?;
    Ok(Some((spec, file)))
}

/// Returns why a default starmap file that `found` describes is refused.
fn found_reason(found: StarmapFound) -> &'static str {
    match found {
        StarmapFound::Link => {
            "is a symbolic link or reaches outside the checkout through a linked directory, and a starmap file is a regular file inside the checkout"
        }
        StarmapFound::NotAFile => "is not a regular file",
        StarmapFound::Missing => "does not exist",
        StarmapFound::Unreadable => "cannot be checked, because its file type cannot be read",
    }
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

/// Returns how a refusal names the port `port` of `app`: `the listen port
/// of` a unit when the bind names no port, and `the port` otherwise.
fn port_phrase(app: &str, port: &str) -> String {
    if port == LISTEN {
        format!("the listen port of `{app}`")
    } else {
        format!("the port `{port}` of `{app}`")
    }
}

/// Returns the failure of the required variable `variable` of the starmap
/// file `file` of `name`, whose bind names `found`, a unit and port of the
/// manifest that the workspace holds no host port for yet, as `reason`
/// says.
fn unassigned(name: &str, file: &Path, variable: &Name, found: Target, reason: &str) -> Unrunnable {
    Unrunnable {
        message: format!(
            "the starmap file {} of `{name}` binds {} to {}, and {reason}",
            file.display(),
            variable.as_str(),
            port_phrase(&found.app, &found.shown),
        ),
        kind: UnrunnableKind::ProvidedUnassigned(Box::new(ProvidedBind {
            variable: variable.as_str().to_owned(),
            app: found.app,
            port: found.shown,
        })),
        unit: Some(name.to_owned()),
    }
}

/// Returns the refused bind of the variable `variable` of the starmap file
/// `file` of `name`, which `unknown` describes.
fn refused_bind(name: &str, file: &Path, variable: &Name, unknown: Unknown) -> RefusedBind {
    RefusedBind {
        message: format!(
            "the starmap file {} of `{name}` binds {} to {}, and {}",
            file.display(),
            variable.as_str(),
            port_phrase(&unknown.app, &unknown.port),
            unknown.reason
        ),
        unit: name.to_owned(),
        variable: variable.as_str().to_owned(),
        app: unknown.app,
        port: unknown.port,
        checkout: unknown.checkout,
        reason: unknown.reason,
    }
}

/// Returns the one failure that lists every bind of `binds`, or `None`
/// when `binds` is empty. Its unit is the first bind's process. Its
/// message is the first bind's sentence when it lists one bind, and says
/// how many it lists otherwise.
pub(super) fn refusal(binds: Vec<RefusedBind>) -> Option<Unrunnable> {
    let binds = RefusedBinds::sorted(binds)?;
    let first = binds.first();
    let message = if binds.len() == 1 {
        first.message.clone()
    } else {
        format!(
            "{} provided variables of the workspace's starmap files bind units or ports that the workspace does not have",
            binds.len()
        )
    };
    Some(Unrunnable {
        unit: Some(first.unit.clone()),
        message,
        kind: UnrunnableKind::ProvidedUnknown(Box::new(binds)),
    })
}

/// A provided variable that the plan leaves unset, because the spec marks
/// it optional in local mode and its bind names a unit of a checkout that
/// the workspace does not have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Skipped {
    /// The variable.
    pub(crate) variable: String,
    /// The checkout that its bind names.
    pub(crate) checkout: String,
}

impl Skipped {
    /// Returns the note that tells the person about the skip.
    pub(crate) fn note(&self) -> ProcessNote {
        ProcessNote::ProvidedSkipped {
            message: format!(
                "left {} unset, because the workspace has no checkout `{}`",
                self.variable, self.checkout
            ),
            variable: self.variable.clone(),
            checkout: self.checkout.clone(),
        }
    }
}

/// Returns the provided variables of `spec` that the plan leaves unset at
/// `place`: each that is optional in local mode and whose bind names
/// `<checkout>/<unit>` with only one of `process` and `app`, where the
/// workspace has no checkout `<checkout>`. A required variable is never
/// skipped, and a bind whose checkout is present gets the usual checks.
fn skipped(spec: &Spec, place: &Place) -> Vec<Skipped> {
    provided_variables(spec)
        .filter(|(name, _)| {
            spec.variables().any(|(variable, declared)| {
                variable == *name
                    && !matches!(declared.unset().in_mode(Mode::Local), Unset::Required)
            })
        })
        .filter_map(|(name, reference)| {
            let named = match (reference.qualifier("process"), reference.qualifier("app")) {
                (Some(named), None) | (None, Some(named)) => named,
                (Some(_), Some(_)) | (None, None) => return None,
            };
            let (checkout, _) = named.split_once('/')?;
            (!place.checkouts.iter().any(|known| known == checkout)).then(|| Skipped {
                variable: name.as_str().to_owned(),
                checkout: checkout.to_owned(),
            })
        })
        .collect()
}

/// Checks the bind of each provided variable of the starmap file of
/// `process`, named `name`, against the workspace's manifest, without
/// resolving any value. It needs no ports, so it runs before `up` gives the
/// workspace any. It admits each variable that [`skipped`] leaves unset.
///
/// # Errors
///
/// Returns [`Unrunnable`] of [`UnrunnableKind::StarmapInvalid`] for a file
/// that cannot be read or does not parse.
///
/// Returns each bind that names no port of the workspace, for the caller
/// to report with the binds of the other processes through [`refusal`].
pub(super) fn check(
    inputs: &Inputs<'_>,
    name: &str,
    process: &Process,
) -> Result<Vec<RefusedBind>, Unrunnable> {
    let Some((spec, file)) = spec_of(inputs, name, process)? else {
        return Ok(Vec::new());
    };
    check_spec(inputs, name, process, &spec, &file).map(|checked| checked.refused)
}

/// What [`check_spec`] found: the variables that the plan leaves unset,
/// and the binds that it refuses.
struct Checked {
    /// The variables that the plan leaves unset, which the check admits.
    skipped: Vec<Skipped>,
    /// Each bind that names no port of the workspace, in the order of
    /// its variable.
    refused: Vec<RefusedBind>,
}

/// Checks the binds of `spec`, the starmap file `file` of `process`,
/// named `name`, through [`check_binds`], and returns the variables that
/// the plan leaves unset and every bind that it refuses.
fn check_spec(
    inputs: &Inputs<'_>,
    name: &str,
    process: &Process,
    spec: &Spec,
    file: &Path,
) -> Result<Checked, Unrunnable> {
    let place = Place::of(inputs, name, process)?;
    let skipped = skipped(spec, &place);
    let neighbours = Neighbours::new(inputs, &place);
    let (port, url) = providers(neighbours)?;
    let refused = check_binds(spec, &[&port, &url])
        .into_iter()
        .filter(|failure| {
            !skipped
                .iter()
                .any(|skip| skip.variable == failure.name.as_str())
        })
        .map(|failure| {
            let unknown = provided_variables(spec)
                .find(|(variable, _)| **variable == failure.name)
                .and_then(|(_, reference)| neighbours.target(reference).err())
                .unwrap_or_else(|| Unknown {
                    app: String::new(),
                    port: LISTEN.to_owned(),
                    checkout: None,
                    reason: failure.error.to_string(),
                });
            refused_bind(name, file, &failure.name, unknown)
        })
        .collect();
    Ok(Checked { skipped, refused })
}

/// The provided variables of a process: the value of each by variable
/// name, and those that the plan leaves unset.
#[derive(Debug, Default)]
pub(super) struct Provided {
    /// The value of each variable that resolved, by variable name.
    pub(super) values: BTreeMap<String, String>,
    /// The variables that the plan leaves unset, because each is optional
    /// and its bind names a checkout that the workspace does not have.
    pub(super) skipped: Vec<Skipped>,
}

/// Returns the value of each provided variable of the starmap file of
/// `process`, named `name`, by variable name, and the variables that
/// [`skipped`] leaves unset. A variable that the spec marks optional and
/// whose port has no host port yet is left out too, with no note.
///
/// # Errors
///
/// Returns the errors of [`check`], [`Unrunnable`] of
/// [`UnrunnableKind::ProvidedUnknown`] that lists every bind that
/// [`check`] refuses, and of [`UnrunnableKind::ProvidedUnassigned`] for
/// the first required variable whose port has no host port.
pub(super) fn values(
    inputs: &Inputs<'_>,
    name: &str,
    process: &Process,
) -> Result<Provided, Unrunnable> {
    let Some((spec, file)) = spec_of(inputs, name, process)? else {
        return Ok(Provided::default());
    };
    let Checked { skipped, refused } = check_spec(inputs, name, process, &spec, &file)?;
    if let Some(failure) = refusal(refused) {
        return Err(failure);
    }
    let wanted: BTreeMap<&Name, &ProvidedRef> = provided_variables(&spec).collect();
    if wanted.is_empty() {
        return Ok(Provided::default());
    }
    let (source, _) = placed(inputs, name, process)?;
    let place = Place::of(inputs, name, process)?;
    let neighbours = Neighbours::new(inputs, &place);
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
    let mut wrong = Vec::new();
    let mut unassigned_first = None;
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
        let reason = if let ResolveProblem::Provider(error) = &failure.problem {
            error.to_string()
        } else {
            "starmap could not resolve it".to_owned()
        };
        match neighbours.target(reference) {
            Err(unknown) => wrong.push(refused_bind(name, &file, &failure.name, unknown)),
            // The bind names a unit and a port of the manifest, and the
            // workspace holds no host port for that port yet.
            Ok(found) => {
                unassigned_first
                    .get_or_insert_with(|| unassigned(name, &file, &failure.name, found, &reason));
            }
        }
    }
    if let Some(failure) = refusal(wrong).or(unassigned_first) {
        return Err(failure);
    }
    let values = resolution
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
        .collect();
    Ok(Provided { values, skipped })
}
