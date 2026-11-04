//! The manifest `mxv.toml` at a repository's root, which declares the
//! application's named ports and the processes that `mxv up` starts.
//!
//! The manifest is parsed strictly: a key that the schema does not declare
//! fails with [`ManifestProblem::UnknownKey`], which names the key and its
//! line. The top-level sections `containers`, `secrets` and `actions` are
//! reserved for later versions and fail with [`ManifestProblem::Reserved`].
//!
//! ```toml
//! ports = ["api", "web"]
//!
//! [processes.api]
//! argv = ["cargo", "run", "--bin", "api"]
//! env = { RUST_LOG = "info" }
//! ports = { PORT = "api" }
//! listen = "api"
//! ready = { timeout_seconds = 120 }
//!
//! [processes.web]
//! argv = ["npm", "run", "dev"]
//! cwd = "web"
//! ports = { PORT = "web", API_PORT = "api" }
//! listen = "web"
//! depends_on = ["api"]
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io::{self, Read as _};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde::de::IgnoredAny;
use toml::Spanned;

#[cfg(test)]
mod tests;

/// The name of the manifest file at a repository's root.
pub const MANIFEST_FILE: &str = "mxv.toml";

/// The most bytes of a manifest that mxv reads. A larger file fails as
/// invalid.
pub const MOST_MANIFEST_BYTES: u64 = 64 * 1024;

/// The readiness timeout of a process whose `ready` table sets no
/// `timeout_seconds`.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(60);

/// The longest readiness timeout that a manifest may set, one hour.
pub const MOST_READY_SECONDS: u64 = 3600;

/// The manifest version that this version of mxv reads. A manifest may
/// state it as `version = 1`, and a higher version fails with
/// [`ManifestProblem::Newer`].
pub const MANIFEST_VERSION: u64 = 1;

/// The most bytes of a process or port name.
const MOST_NAME_BYTES: usize = 64;

/// An application's manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Manifest {
    /// The named ports, in the order declared. mxv gives each workspace its
    /// own host port for each name.
    pub ports: Vec<String>,
    /// The processes, by name.
    pub processes: BTreeMap<String, Process>,
}

/// One process of a manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Process {
    /// The program and its arguments, as separate values. No shell reads
    /// them.
    pub argv: Vec<String>,
    /// The directory the process starts in, relative to the repository's
    /// root. It defaults to `.`.
    pub cwd: PathBuf,
    /// The variables that the process receives on top of mxv's own
    /// environment.
    pub env: BTreeMap<String, String>,
    /// The ports that the process receives: each key is the name of an
    /// environment variable, and each value names a port of
    /// [`Manifest::ports`].
    pub ports: BTreeMap<String, String>,
    /// The port that the process listens on, which is one of the ports it
    /// receives, or `None`. A port has at most one listener, and `mxv up`
    /// checks that the port is free before it starts the listener.
    pub listen: Option<String>,
    /// The check that tells `mxv up` the process is ready, or `None` when
    /// the process counts as started once it has run for a short grace
    /// period. A listener without a `ready` table is ready when its listen
    /// port accepts a connection.
    pub ready: Option<Ready>,
    /// The processes that start, and are ready, before this one.
    pub depends_on: Vec<String>,
}

/// A readiness check: a TCP connection to a named port on the loopback
/// address succeeds within the timeout.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Ready {
    /// The port of [`Manifest::ports`] to connect to.
    pub port: String,
    /// How long `mxv up` waits for the connection.
    pub timeout: Duration,
}

/// A manifest that cannot be read or understood.
#[derive(Debug)]
#[non_exhaustive]
pub struct ManifestError {
    /// The manifest's path.
    pub path: PathBuf,
    /// The line, counted from 1, that the problem is on, when the problem
    /// has one.
    pub line: Option<usize>,
    /// What is wrong.
    pub problem: ManifestProblem,
}

/// What is wrong with a manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ManifestProblem {
    /// The manifest holds a key that the schema does not declare.
    UnknownKey {
        /// The key.
        key: String,
    },
    /// The manifest states a version that a later version of mxv reads.
    Newer {
        /// The version that the manifest states.
        version: u64,
    },
    /// The manifest holds a section that a later version of mxv reads.
    Reserved {
        /// The section, such as `containers`.
        section: String,
    },
    /// The manifest is not valid TOML of the schema, or a value breaks a
    /// rule, such as a reference to a port it does not declare.
    Invalid {
        /// What is wrong, in a sentence.
        message: String,
    },
    /// The manifest exists, and cannot be read.
    Unreadable {
        /// Why it cannot be read.
        message: String,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        let place = match self.line {
            Some(line) => format!("{path}:{line}"),
            None => path.to_string(),
        };
        match &self.problem {
            ManifestProblem::UnknownKey { key } => {
                write!(
                    formatter,
                    "{place}: the manifest has the unknown key `{key}`"
                )
            }
            ManifestProblem::Newer { version } => write!(
                formatter,
                "{place}: the manifest is version {version}, and this mxv reads version {MANIFEST_VERSION}"
            ),
            ManifestProblem::Reserved { section } => write!(
                formatter,
                "{place}: the section `{section}` is reserved for a later version of mxv"
            ),
            ManifestProblem::Invalid { message } => write!(formatter, "{place}: {message}"),
            ManifestProblem::Unreadable { message } => {
                write!(formatter, "cannot read {place}: {message}")
            }
        }
    }
}

impl std::error::Error for ManifestError {}

/// The manifest as TOML holds it, with the spans that errors cite.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    /// The manifest's version.
    version: Option<Spanned<u64>>,
    /// The named ports.
    #[serde(default)]
    ports: Vec<Spanned<String>>,
    /// The processes.
    #[serde(default)]
    processes: BTreeMap<Spanned<String>, RawProcess>,
    /// Reserved for containers.
    containers: Option<Spanned<IgnoredAny>>,
    /// Reserved for secrets.
    secrets: Option<Spanned<IgnoredAny>>,
    /// Reserved for actions.
    actions: Option<Spanned<IgnoredAny>>,
}

/// One process as TOML holds it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProcess {
    /// The program and its arguments.
    argv: Spanned<Vec<String>>,
    /// The starting directory.
    cwd: Option<Spanned<String>>,
    /// The extra variables.
    #[serde(default)]
    env: BTreeMap<Spanned<String>, String>,
    /// The port variables.
    #[serde(default)]
    ports: BTreeMap<Spanned<String>, Spanned<String>>,
    /// The port that the process listens on.
    listen: Option<Spanned<String>>,
    /// The readiness check.
    ready: Option<RawReady>,
    /// The dependencies.
    #[serde(default)]
    depends_on: Vec<Spanned<String>>,
}

/// A readiness check as TOML holds it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReady {
    /// The port to connect to, which defaults to the listen port.
    port: Option<Spanned<String>>,
    /// The timeout in seconds.
    timeout_seconds: Option<Spanned<u64>>,
}

/// Builds the errors of one manifest's text, with the line of each span.
struct Errors<'a> {
    /// The manifest's path.
    path: &'a Path,
    /// The manifest's text.
    text: &'a str,
}

impl Errors<'_> {
    /// Returns the line, counted from 1, at the start of `span`.
    fn line(&self, span: &Range<usize>) -> usize {
        let end = span.start.min(self.text.len());
        self.text
            .get(..end)
            .map_or(0, |before| before.matches('\n').count())
            + 1
    }

    /// Returns the error of `problem` on the line of `span`.
    fn at(&self, span: Option<&Range<usize>>, problem: ManifestProblem) -> ManifestError {
        ManifestError {
            path: self.path.to_path_buf(),
            line: span.map(|span| self.line(span)),
            problem,
        }
    }

    /// Returns an invalid-manifest error with `message` on the line of
    /// `span`.
    fn invalid(&self, span: &Range<usize>, message: String) -> ManifestError {
        self.at(Some(span), ManifestProblem::Invalid { message })
    }

    /// Returns the error of a TOML or schema error.
    fn of_toml(&self, error: &toml::de::Error) -> ManifestError {
        let message = error.message();
        let problem = match unknown_field(message) {
            Some(key) => ManifestProblem::UnknownKey { key },
            None => ManifestProblem::Invalid {
                message: message.trim().to_owned(),
            },
        };
        self.at(error.span().as_ref(), problem)
    }
}

/// Returns the key of serde's message about an unknown field, such as
/// ``unknown field `command`, expected one of ...``.
fn unknown_field(message: &str) -> Option<String> {
    let rest = message.strip_prefix("unknown field `")?;
    let end = rest.find('`')?;
    rest.get(..end).map(str::to_owned)
}

/// Reports whether `name` is a valid process or port name: 1 to 64 ASCII
/// letters, digits, `-` and `_`, starting with a letter or a digit.
fn is_name(name: &str) -> bool {
    name.len() <= MOST_NAME_BYTES
        && name
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character))
}

/// Reports whether `name` is a valid environment variable name: an ASCII
/// letter or `_`, then letters, digits and `_`.
fn is_variable(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Reports whether `path` is relative and never leaves the directory it
/// starts in.
fn stays_inside(path: &Path) -> bool {
    let mut depth: usize = 0;
    for component in path.components() {
        match component {
            Component::Normal(_) => depth = depth.saturating_add(1),
            Component::CurDir => {}
            Component::ParentDir => match depth.checked_sub(1) {
                Some(less) => depth = less,
                None => return false,
            },
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

impl Manifest {
    /// Returns the manifest at `<checkout>/mxv.toml`, or `None` when the
    /// checkout has none.
    ///
    /// # Errors
    ///
    /// Returns a [`ManifestError`] when the file cannot be read, is larger
    /// than [`MOST_MANIFEST_BYTES`], or breaks the schema.
    pub fn read(checkout: &Path) -> Result<Option<Self>, ManifestError> {
        let path = checkout.join(MANIFEST_FILE);
        let unreadable = |error: io::Error| ManifestError {
            path: path.clone(),
            line: None,
            problem: ManifestProblem::Unreadable {
                message: error.to_string(),
            },
        };
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(unreadable(error)),
        };
        let mut text = String::new();
        file.take(MOST_MANIFEST_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(unreadable)?;
        if u64::try_from(text.len()).unwrap_or(u64::MAX) > MOST_MANIFEST_BYTES {
            return Err(ManifestError {
                path,
                line: None,
                problem: ManifestProblem::Invalid {
                    message: format!("the manifest is larger than {MOST_MANIFEST_BYTES} bytes"),
                },
            });
        }
        let manifest = Self::parse(&text, &path)?;
        manifest.check_cwds(checkout, &path)?;
        Ok(Some(manifest))
    }

    /// Checks that each process's `cwd` that exists in `checkout` stays
    /// inside it once symbolic links are resolved. A `cwd` that does not
    /// exist is left for `mxv up` to report when the process cannot start.
    fn check_cwds(&self, checkout: &Path, path: &Path) -> Result<(), ManifestError> {
        let Ok(root) = checkout.canonicalize() else {
            return Ok(());
        };
        for (name, process) in &self.processes {
            if let Ok(resolved) = checkout.join(&process.cwd).canonicalize()
                && !resolved.starts_with(&root)
            {
                return Err(ManifestError {
                    path: path.to_path_buf(),
                    line: None,
                    problem: ManifestProblem::Invalid {
                        message: format!(
                            "the cwd `{}` of `{name}` leaves the repository through a symbolic link to {}",
                            process.cwd.display(),
                            resolved.display()
                        ),
                    },
                });
            }
        }
        Ok(())
    }

    /// Parses the manifest `text`, which was read from `path`.
    ///
    /// # Errors
    ///
    /// Returns a [`ManifestError`] that names the line of the first
    /// problem it finds.
    pub fn parse(text: &str, path: &Path) -> Result<Self, ManifestError> {
        let errors = Errors { path, text };
        let raw: RawManifest = toml::from_str(text).map_err(|error| errors.of_toml(&error))?;
        if let Some(version) = &raw.version {
            match *version.get_ref() {
                0 => {
                    return Err(errors.invalid(
                        &version.span(),
                        "the manifest version must be 1 or greater".to_owned(),
                    ));
                }
                found if found > MANIFEST_VERSION => {
                    return Err(errors.at(
                        Some(&version.span()),
                        ManifestProblem::Newer { version: found },
                    ));
                }
                _ => {}
            }
        }
        for (section, value) in [
            ("containers", &raw.containers),
            ("secrets", &raw.secrets),
            ("actions", &raw.actions),
        ] {
            if let Some(value) = value {
                return Err(errors.at(
                    Some(&value.span()),
                    ManifestProblem::Reserved {
                        section: section.to_owned(),
                    },
                ));
            }
        }
        let mut ports = Vec::new();
        for port in &raw.ports {
            if !is_name(port.get_ref()) {
                return Err(errors.invalid(
                    &port.span(),
                    format!(
                        "`{}` is not a port name, which holds 1 to 64 letters, digits, '-' and '_'",
                        port.get_ref()
                    ),
                ));
            }
            if ports.contains(port.get_ref()) {
                return Err(errors.invalid(
                    &port.span(),
                    format!("the port `{}` is declared twice", port.get_ref()),
                ));
            }
            ports.push(port.get_ref().clone());
        }
        let names: BTreeSet<&str> = raw
            .processes
            .keys()
            .map(|name| name.get_ref().as_str())
            .collect();
        let mut processes = BTreeMap::new();
        let mut listeners: BTreeMap<String, String> = BTreeMap::new();
        for (name, raw_process) in &raw.processes {
            let process = check_process(&errors, name, raw_process, &ports, &names)?;
            if let (Some(port), Some(listen)) = (&process.listen, &raw_process.listen) {
                if let Some(other) = listeners.get(port) {
                    return Err(errors.invalid(
                        &listen.span(),
                        format!(
                            "the processes `{other}` and `{}` both listen on the port `{port}`, which has at most one listener",
                            name.get_ref()
                        ),
                    ));
                }
                listeners.insert(port.clone(), name.get_ref().clone());
            }
            processes.insert(name.get_ref().clone(), process);
        }
        let manifest = Self { ports, processes };
        if let Some(name) = manifest.cycle() {
            let span = raw
                .processes
                .keys()
                .find(|key| key.get_ref() == &name)
                .map(Spanned::span);
            return Err(errors.at(
                span.as_ref(),
                ManifestProblem::Invalid {
                    message: format!(
                        "the process `{name}` depends on itself through a dependency cycle"
                    ),
                },
            ));
        }
        Ok(manifest)
    }

    /// Returns the processes that `mxv up` starts for `names`, in the
    /// order it starts them: each process after its dependencies, and
    /// processes that do not depend on each other in name order. Empty
    /// `names` select every process.
    ///
    /// # Errors
    ///
    /// Returns the first name that the manifest declares no process for.
    pub fn start_order(&self, names: &[String]) -> Result<Vec<String>, String> {
        let selected: Vec<&String> = if names.is_empty() {
            self.processes.keys().collect()
        } else {
            names.iter().collect()
        };
        let mut order = Vec::new();
        for name in selected {
            if !self.processes.contains_key(name) {
                return Err(name.clone());
            }
            self.visit(name, &mut order, &mut Vec::new());
        }
        Ok(order)
    }

    /// Adds `name` to `order` after its dependencies, unless it is already
    /// there. `path` holds the processes being visited, so a cycle, which
    /// `parse` refuses, cannot recurse for ever.
    fn visit(&self, name: &str, order: &mut Vec<String>, path: &mut Vec<String>) {
        if order.iter().any(|done| done == name) || path.iter().any(|seen| seen == name) {
            return;
        }
        path.push(name.to_owned());
        if let Some(process) = self.processes.get(name) {
            for dependency in &process.depends_on {
                self.visit(dependency, order, path);
            }
        }
        path.pop();
        order.push(name.to_owned());
    }

    /// Returns a process that depends on itself, directly or through other
    /// processes, or `None`.
    fn cycle(&self) -> Option<String> {
        self.processes
            .keys()
            .find(|name| self.reaches(name, name, &mut BTreeSet::new()))
            .cloned()
    }

    /// Reports whether `target` is a dependency of `from`, directly or
    /// through other processes.
    fn reaches<'a>(&'a self, from: &'a str, target: &str, seen: &mut BTreeSet<&'a str>) -> bool {
        let Some(process) = self.processes.get(from) else {
            return false;
        };
        process.depends_on.iter().any(|dependency| {
            dependency == target
                || (seen.insert(dependency.as_str()) && self.reaches(dependency, target, seen))
        })
    }
}

/// Checks one raw process against the declared `ports` and process
/// `names`, and returns it.
fn check_process(
    errors: &Errors<'_>,
    name: &Spanned<String>,
    raw: &RawProcess,
    ports: &[String],
    names: &BTreeSet<&str>,
) -> Result<Process, ManifestError> {
    let process = name.get_ref();
    if !is_name(process) {
        return Err(errors.invalid(
            &name.span(),
            format!("`{process}` is not a process name, which holds 1 to 64 letters, digits, '-' and '_'"),
        ));
    }
    if raw.argv.get_ref().is_empty() {
        return Err(errors.invalid(
            &raw.argv.span(),
            format!("the process `{process}` has an empty argv, which needs at least the program"),
        ));
    }
    let cwd = match &raw.cwd {
        Some(cwd) => {
            let path = PathBuf::from(cwd.get_ref());
            if !stays_inside(&path) {
                return Err(errors.invalid(
                    &cwd.span(),
                    format!(
                        "the cwd `{}` of `{process}` is not a relative path inside the repository",
                        cwd.get_ref()
                    ),
                ));
            }
            path
        }
        None => PathBuf::from("."),
    };
    let declared = |port: &Spanned<String>| {
        if ports.contains(port.get_ref()) {
            Ok(port.get_ref().clone())
        } else {
            Err(errors.invalid(
                &port.span(),
                format!(
                    "the process `{process}` names the port `{}`, which `ports` does not declare",
                    port.get_ref()
                ),
            ))
        }
    };
    let mut env = BTreeMap::new();
    for (variable, value) in &raw.env {
        check_variable(errors, process, variable)?;
        env.insert(variable.get_ref().clone(), value.clone());
    }
    let mut port_variables = BTreeMap::new();
    for (variable, port) in &raw.ports {
        check_variable(errors, process, variable)?;
        if env.contains_key(variable.get_ref()) {
            return Err(errors.invalid(
                &variable.span(),
                format!(
                    "the process `{process}` sets {} both in `env` and in `ports`",
                    variable.get_ref()
                ),
            ));
        }
        port_variables.insert(variable.get_ref().clone(), declared(port)?);
    }
    let listen = match &raw.listen {
        Some(listen) => {
            let port = declared(listen)?;
            if !port_variables.values().any(|received| received == &port) {
                return Err(errors.invalid(
                    &listen.span(),
                    format!(
                        "the process `{process}` listens on `{port}`, which it does not receive; add a variable for it to `ports`"
                    ),
                ));
            }
            Some(port)
        }
        None => None,
    };
    let ready = match &raw.ready {
        Some(ready) => {
            let timeout = match &ready.timeout_seconds {
                Some(seconds) if (1..=MOST_READY_SECONDS).contains(seconds.get_ref()) => {
                    Duration::from_secs(*seconds.get_ref())
                }
                Some(seconds) => {
                    return Err(errors.invalid(
                        &seconds.span(),
                        format!("timeout_seconds of `{process}` is not between 1 and {MOST_READY_SECONDS}"),
                    ));
                }
                None => DEFAULT_READY_TIMEOUT,
            };
            let port = match &ready.port {
                Some(port) => {
                    let port_name = declared(port)?;
                    if listen.as_ref() != Some(&port_name)
                        && !port_variables.values().any(|received| received == &port_name)
                    {
                        return Err(errors.invalid(
                            &port.span(),
                            format!(
                                "the readiness port `{port_name}` of `{process}` is neither its listen port nor a port it receives"
                            ),
                        ));
                    }
                    port_name
                }
                None => listen.clone().ok_or_else(|| {
                    errors.invalid(
                        &raw.argv.span(),
                        format!(
                            "the `ready` table of `{process}` names no port, and the process has no `listen` port to default to"
                        ),
                    )
                })?,
            };
            Some(Ready { port, timeout })
        }
        None => listen.clone().map(|port| Ready {
            port,
            timeout: DEFAULT_READY_TIMEOUT,
        }),
    };
    let mut depends_on = Vec::new();
    for dependency in &raw.depends_on {
        if !names.contains(dependency.get_ref().as_str()) {
            return Err(errors.invalid(
                &dependency.span(),
                format!(
                    "the process `{process}` depends on `{}`, which the manifest does not declare",
                    dependency.get_ref()
                ),
            ));
        }
        depends_on.push(dependency.get_ref().clone());
    }
    Ok(Process {
        argv: raw.argv.get_ref().clone(),
        cwd,
        env,
        ports: port_variables,
        listen,
        ready,
        depends_on,
    })
}

/// Checks that `variable` is a valid environment variable name.
fn check_variable(
    errors: &Errors<'_>,
    process: &str,
    variable: &Spanned<String>,
) -> Result<(), ManifestError> {
    if is_variable(variable.get_ref()) {
        Ok(())
    } else {
        Err(errors.invalid(
            &variable.span(),
            format!(
                "`{}` of `{process}` is not an environment variable name",
                variable.get_ref()
            ),
        ))
    }
}
