//! The manifest `mxv.toml` at a repository's root, which declares the
//! application's named ports and the processes and containers that
//! `mxv up` starts.
//!
//! The manifest is parsed strictly: a key that the schema does not declare
//! fails with [`ManifestProblem::UnknownKey`], which names the key and its
//! line. The top-level sections `secrets` and `actions` are reserved for
//! later versions and fail with [`ManifestProblem::Reserved`].
//!
//! Processes and containers share one set of names, and either may depend
//! on the other.
//!
//! ```toml
//! ports = ["api", "web", "dynamodb"]
//! disposable = ["web/test-results/"]
//! clone_on_new = ["node_modules/"]
//!
//! [containers.dynamodb]
//! image = "amazon/dynamodb-local:2.5.4"
//! command = ["-jar", "DynamoDBLocal.jar", "-sharedDb", "-dbPath", "/data"]
//! ports = { 8000 = "dynamodb" }
//! volumes = { data = "/data" }
//!
//! [processes.api]
//! argv = ["cargo", "run", "--bin", "api"]
//! env = { RUST_LOG = "info" }
//! ports = { PORT = "api", DYNAMODB_PORT = "dynamodb" }
//! listen = "api"
//! ready = { timeout_seconds = 120 }
//! depends_on = ["dynamodb"]
//!
//! [processes.web]
//! argv = ["npm", "run", "dev"]
//! cwd = "web"
//! env = { API_URL = "http://127.0.0.1:{port.api}" }
//! ports = { PORT = "web" }
//! listen = "web"
//! depends_on = ["api"]
//!
//! [processes.worker]
//! argv = ["cargo", "run", "--bin", "worker"]
//! ready = { log = "^worker ready", timeout_seconds = 300 }
//! ```
//!
//! An `env` value may name a host port as `{port.<name>}`, and writes a
//! literal brace as `{{` or `}}`. Any other brace fails the manifest.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use regex::Regex;
use serde::Deserialize;
use serde::de::IgnoredAny;
use toml::Spanned;

use crate::disposable::{BadGlob, PathGlob};
use crate::name::{WorkspaceName, is_unit_name};
use crate::redacted::Redacted;

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

/// The most bytes of an image reference.
const MOST_IMAGE_BYTES: usize = 255;

/// An application's manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Manifest {
    /// The named ports, in the order declared. mxv gives each workspace its
    /// own host port for each name.
    pub ports: Vec<String>,
    /// The processes, by name.
    pub processes: BTreeMap<String, Process>,
    /// The containers, by name. No container has the name of a process.
    pub containers: BTreeMap<String, Container>,
    /// The globs of the repository's own disposable paths, such as
    /// `web/test-results/`, which extend the built-in [`DISPOSABLE`] set.
    /// A path that a glob matches is disposable only while git reports it
    /// as ignored.
    ///
    /// [`DISPOSABLE`]: crate::DISPOSABLE
    pub disposable: Vec<PathGlob>,
    /// The globs of the ignored directories, such as `node_modules/`, that
    /// `mxv new` clones from the repository into a new checkout, on top of
    /// the ones that the config file names.
    pub clone_on_new: Vec<PathGlob>,
    /// One sentence for each top-level table that mxv ignores although
    /// its name is close to one of mxv's own, such as `[proceses.web]`,
    /// or is phase's `[process.*]`, with the file and line in front.
    pub warnings: Vec<String>,
}

/// The names of the other manifests that a manifest joins, which its units
/// may refer to, as [`Manifest::parse_joining`] describes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Joined {
    /// The ports of the other manifests.
    pub ports: Vec<String>,
    /// The processes and containers of the other manifests.
    pub units: Vec<String>,
}

impl Joined {
    /// Adds the ports and units of `manifest`.
    pub fn add(&mut self, manifest: &Manifest) {
        self.ports.extend(manifest.ports.iter().cloned());
        self.units.extend(
            manifest
                .processes
                .keys()
                .chain(manifest.containers.keys())
                .cloned(),
        );
    }
}

/// One process or container of a manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit<'a> {
    /// A process.
    Process(&'a Process),
    /// A container.
    Container(&'a Container),
}

impl<'a> Unit<'a> {
    /// Returns the processes and containers that start, and are ready,
    /// before this one.
    #[must_use]
    pub fn depends_on(self) -> &'a [String] {
        match self {
            Unit::Process(process) => &process.depends_on,
            Unit::Container(container) => &container.depends_on,
        }
    }

    /// Returns the readiness check, or `None`.
    #[must_use]
    pub const fn ready(self) -> Option<&'a Ready> {
        match self {
            Unit::Process(process) => process.ready.as_ref(),
            Unit::Container(container) => container.ready.as_ref(),
        }
    }
}

/// One container of a manifest, which `mxv up` runs with the docker CLI.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Container {
    /// The image, pinned by a tag other than `latest` or by a digest.
    pub image: String,
    /// The arguments that replace the image's command, or `None` to keep
    /// it.
    pub command: Option<Vec<String>>,
    /// The variables that the container receives. Their values are
    /// [`Redacted`], so no row, log or error shows them.
    pub env: BTreeMap<String, Redacted>,
    /// The container ports that mxv publishes on the loopback address, in
    /// the order of their container port.
    pub ports: Vec<Published>,
    /// The named volumes that the container mounts: each key names a
    /// volume, which belongs to the workspace, and each value is the
    /// absolute path it mounts at inside the container.
    pub volumes: BTreeMap<String, String>,
    /// The check that tells `mxv up` the container is ready, or `None`
    /// when it counts as started once it has run for a short grace period.
    /// A container that publishes one port and has no `ready` table is
    /// ready when that port answers.
    pub ready: Option<Ready>,
    /// The processes and containers that start, and are ready, before this
    /// one.
    pub depends_on: Vec<String>,
}

/// A container port that mxv publishes as a named port.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Published {
    /// The TCP port inside the container.
    pub container: u16,
    /// The port of [`Manifest::ports`] that it is published on.
    pub port: String,
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
    /// environment, whose values may name host ports.
    pub env: BTreeMap<String, EnvValue>,
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

/// A readiness check, which must pass within the timeout.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Ready {
    /// What `mxv up` waits for.
    pub check: ReadyCheck,
    /// How long `mxv up` waits.
    pub timeout: Duration,
}

/// What a readiness check waits for.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadyCheck {
    /// A TCP connection to the named port of [`Manifest::ports`] on the
    /// loopback address succeeds.
    Port(String),
    /// The process writes a log line that matches the pattern, after
    /// `mxv up` started it. A process that listens on no port, such as a
    /// worker that reads a queue, uses it.
    Log(LogPattern),
}

/// A regular expression that a readiness check matches against each new
/// log line.
#[derive(Clone, Debug)]
pub struct LogPattern(Regex);

impl LogPattern {
    /// Returns the pattern as the manifest writes it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Reports whether `line` matches the pattern.
    #[must_use]
    pub fn is_match(&self, line: &str) -> bool {
        self.0.is_match(line)
    }
}

impl PartialEq for LogPattern {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for LogPattern {}

/// The value of an `env` variable, which may name host ports as
/// `{port.<name>}` and writes a literal brace as `{{` or `}}`. Its
/// `Debug` prints `<redacted>`, and it implements neither `Display` nor
/// `Serialize`. [`EnvValue::render`] is the only way to read it.
#[derive(Clone, PartialEq, Eq)]
pub struct EnvValue {
    /// The value as the manifest writes it.
    text: String,
    /// The value's literal text and port placeholders, in order.
    parts: Vec<Part>,
}

/// One part of an [`EnvValue`].
#[derive(Clone, Debug, PartialEq, Eq)]
enum Part {
    /// Literal text, with each doubled brace read as one brace.
    Text(String),
    /// The host port of the named port.
    Port(String),
}

impl EnvValue {
    /// Returns the value with each placeholder replaced by its host port in
    /// `ports`, by port name, for the environment of the process. A
    /// placeholder whose port `ports` lacks stays as the manifest writes
    /// it.
    #[must_use]
    pub fn render(&self, ports: &BTreeMap<String, u16>) -> Redacted {
        let mut rendered = String::new();
        for part in &self.parts {
            match part {
                Part::Text(text) => rendered.push_str(text),
                Part::Port(port) => match ports.get(port) {
                    Some(number) => rendered.push_str(&number.to_string()),
                    None => {
                        rendered.push_str("{port.");
                        rendered.push_str(port);
                        rendered.push('}');
                    }
                },
            }
        }
        Redacted::new(rendered)
    }

    /// Returns the names of the ports that the value's placeholders name,
    /// in order.
    pub fn ports(&self) -> impl Iterator<Item = &str> {
        self.parts.iter().filter_map(|part| match part {
            Part::Port(port) => Some(port.as_str()),
            Part::Text(_) => None,
        })
    }

    /// Returns the value with each placeholder's port renamed by `rename`,
    /// and its text written again to match, with each literal brace
    /// doubled.
    fn renamed(&self, rename: &dyn Fn(&str) -> String) -> Self {
        let mut text = String::new();
        let parts = self
            .parts
            .iter()
            .map(|part| match part {
                Part::Text(literal) => {
                    text.push_str(&literal.replace('{', "{{").replace('}', "}}"));
                    Part::Text(literal.clone())
                }
                Part::Port(port) => {
                    let port = rename(port);
                    text.push_str("{port.");
                    text.push_str(&port);
                    text.push('}');
                    Part::Port(port)
                }
            })
            .collect();
        Self { text, parts }
    }

    /// Reads `text`, or returns `None` when a brace is neither doubled nor
    /// part of a `{port.<name>}` placeholder whose name is a port name.
    fn parse(text: &str) -> Option<Self> {
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut rest = text;
        while let Some(at) = rest.find(['{', '}']) {
            literal.push_str(rest.get(..at)?);
            let tail = rest.get(at..)?;
            if let Some(after) = tail.strip_prefix("{{") {
                literal.push('{');
                rest = after;
            } else if let Some(after) = tail.strip_prefix("}}") {
                literal.push('}');
                rest = after;
            } else {
                let inside = tail.strip_prefix("{port.")?;
                let end = inside.find('}')?;
                let port = inside.get(..end)?;
                if !is_unit_name(port) {
                    return None;
                }
                if !literal.is_empty() {
                    parts.push(Part::Text(std::mem::take(&mut literal)));
                }
                parts.push(Part::Port(port.to_owned()));
                rest = inside.get(end + 1..)?;
            }
        }
        literal.push_str(rest);
        if !literal.is_empty() {
            parts.push(Part::Text(literal));
        }
        Some(Self {
            text: text.to_owned(),
            parts,
        })
    }
}

impl fmt::Debug for EnvValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// A test compares a value with the text the manifest writes without
/// showing it.
impl PartialEq<&str> for EnvValue {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
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
    /// A process or container has a name that phase cannot run an app
    /// under: phase names a container's compose project after the unit,
    /// and a project name holds only lowercase letters, digits, `-` and
    /// `_`.
    UnitName {
        /// The unit's name.
        name: String,
    },
    /// The manifest parses, and phase cannot run it, such as a process
    /// that sets `PATH`, which phase gives every process itself.
    Unrunnable {
        /// What is wrong, in a sentence.
        message: String,
    },
    /// The manifest declares a process, container or port whose name, once
    /// a workspace with several checkouts merges its manifests, equals a
    /// name that another of the workspace's manifests declares.
    NameClash {
        /// The merged name, such as `api-server`.
        name: String,
        /// The other manifest that declares the name.
        other: PathBuf,
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
            ManifestProblem::Invalid { message } | ManifestProblem::Unrunnable { message } => {
                write!(formatter, "{place}: {message}")
            }
            ManifestProblem::UnitName { name } => write!(
                formatter,
                "{place}: `{name}` is not a name mxv can run, which holds 1 to 64 lowercase letters, digits, '-' and '_' and starts with a letter or a digit"
            ),
            ManifestProblem::Unreadable { message } => {
                write!(formatter, "cannot read {place}: {message}")
            }
            ManifestProblem::NameClash { name, other } => write!(
                formatter,
                "{place}: the workspace's merged manifest has two units or two ports named `{name}`, one from this file and one from {}",
                other.display()
            ),
        }
    }
}

impl std::error::Error for ManifestError {}

/// The top-level keys that mxv reads. [`Manifest::parse`] leaves any other
/// top-level table to the tools that read it.
const OWN_KEYS: &[&str] = &[
    "version",
    "ports",
    "processes",
    "containers",
    "disposable",
    "clone_on_new",
    "secrets",
    "actions",
];

/// The manifest as TOML holds it, with the spans that errors cite. It
/// declares no `deny_unknown_fields`, because another tool's top-level
/// table may share the file; [`check_top_level`] refuses every other
/// unknown top-level key.
#[derive(Deserialize)]
struct RawManifest {
    /// The manifest's version.
    version: Option<Spanned<u64>>,
    /// The named ports.
    #[serde(default)]
    ports: Vec<Spanned<String>>,
    /// The processes.
    #[serde(default)]
    processes: BTreeMap<Spanned<String>, RawProcess>,
    /// The containers.
    #[serde(default)]
    containers: BTreeMap<Spanned<String>, RawContainer>,
    /// The globs of the repository's disposable paths.
    #[serde(default)]
    disposable: Vec<Spanned<String>>,
    /// The globs of the ignored directories that a new workspace clones.
    #[serde(default)]
    clone_on_new: Vec<Spanned<String>>,
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
    env: BTreeMap<Spanned<String>, Spanned<String>>,
    /// The port variables.
    #[serde(default)]
    ports: BTreeMap<Spanned<String>, Spanned<String>>,
    /// The port that the process listens on.
    listen: Option<Spanned<String>>,
    /// The readiness check.
    ready: Option<Spanned<RawReady>>,
    /// The dependencies.
    #[serde(default)]
    depends_on: Vec<Spanned<String>>,
}

/// One container as TOML holds it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawContainer {
    /// The image.
    image: Spanned<String>,
    /// The arguments that replace the image's command.
    command: Option<Vec<String>>,
    /// The variables.
    #[serde(default)]
    env: BTreeMap<Spanned<String>, String>,
    /// The published ports, from container port to port name.
    #[serde(default)]
    ports: BTreeMap<Spanned<String>, Spanned<String>>,
    /// The volumes, from volume name to mount path.
    #[serde(default)]
    volumes: BTreeMap<Spanned<String>, Spanned<String>>,
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
    /// The pattern of the log line to wait for.
    log: Option<Spanned<String>>,
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

/// The top-level tables of phase, which reads the same `mxv.toml`. mxv
/// never warns about them, except `process`, which phase uses for what
/// mxv calls `processes`.
const PHASE_TABLES: &[&str] = &[
    "app",
    "release",
    "build",
    "process",
    "permissions",
    "needs",
    "service",
];

/// Checks that `raw`'s `version` is 1 or greater and no greater than
/// [`MANIFEST_VERSION`], and that it holds none of the reserved tables
/// `secrets` and `actions`.
fn check_version_and_reserved(errors: &Errors<'_>, raw: &RawManifest) -> Result<(), ManifestError> {
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
    for (section, value) in [("secrets", &raw.secrets), ("actions", &raw.actions)] {
        if let Some(value) = value {
            return Err(errors.at(
                Some(&value.span()),
                ManifestProblem::Reserved {
                    section: section.to_owned(),
                },
            ));
        }
    }
    Ok(())
}

/// Checks that `text` is valid TOML and that each top-level key that mxv
/// does not read holds a table or an array of tables, such as the `[app]`,
/// `[service.web]` or `[[release]]` of another tool that shares the file,
/// which mxv leaves alone. Any other unknown top-level key fails with
/// [`ManifestProblem::UnknownKey`].
///
/// It returns a warning for each such table whose name is close to one of
/// mxv's own, which [`near_miss`] decides, and for phase's `[process.*]`,
/// so a misspelt table never silently declares nothing.
fn check_top_level(errors: &Errors<'_>, text: &str) -> Result<Vec<String>, ManifestError> {
    let table = toml::de::DeTable::parse(text).map_err(|error| errors.of_toml(&error))?;
    let mut warnings = Vec::new();
    for (key, value) in table.get_ref() {
        let name = key.get_ref().as_ref();
        if OWN_KEYS.contains(&name) {
            continue;
        }
        let tables = match value.get_ref() {
            toml::de::DeValue::Table(_) => true,
            toml::de::DeValue::Array(items) => {
                !items.is_empty()
                    && items
                        .iter()
                        .all(|item| matches!(item.get_ref(), toml::de::DeValue::Table(_)))
            }
            _ => false,
        };
        if !tables {
            return Err(errors.at(
                Some(&key.span()),
                ManifestProblem::UnknownKey {
                    key: name.to_owned(),
                },
            ));
        }
        let place = format!("{}:{}", errors.path.display(), errors.line(&key.span()));
        if name == "process" {
            warnings.push(format!(
                "{place}: mxv ignores the table `[process.*]`, which is phase's, and runs only the processes of `[processes.*]`"
            ));
        } else if !PHASE_TABLES.contains(&name)
            && let Some(meant) = near_miss(name)
        {
            warnings.push(format!(
                "{place}: mxv ignores the table `[{name}]`, whose name is close to mxv's `{meant}`, and reads nothing from it"
            ));
        }
    }
    Ok(warnings)
}

/// Returns the key of mxv's own that `name` is likely a misspelling of:
/// one that differs from `name` only in case, or by a trailing `s`, or by
/// at most two inserted, deleted or replaced characters.
fn near_miss(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    OWN_KEYS.iter().copied().find(|own| {
        lower == *own
            || format!("{lower}s") == *own
            || lower == format!("{own}s")
            || edit_distance(&lower, own) <= 2
    })
}

/// Returns the number of characters that must be inserted, deleted or
/// replaced to turn `from` into `to`.
fn edit_distance(from: &str, to: &str) -> usize {
    let to: Vec<char> = to.chars().collect();
    // `previous` holds the distances from the characters of `from` read so
    // far to each prefix of `to`.
    let mut previous: Vec<usize> = (0..=to.len()).collect();
    for (row, left) in from.chars().enumerate() {
        let mut current = vec![row.saturating_add(1)];
        for (pair, right) in previous.windows(2).zip(&to) {
            let [diagonal, above] = pair else {
                continue;
            };
            let beside = current.last().copied().unwrap_or(usize::MAX);
            current.push(
                diagonal
                    .saturating_add(usize::from(left != *right))
                    .min(above.saturating_add(1))
                    .min(beside.saturating_add(1)),
            );
        }
        previous = current;
    }
    previous.last().copied().unwrap_or_default()
}

/// Returns the key of serde's message about an unknown field, such as
/// ``unknown field `command`, expected one of ...``.
fn unknown_field(message: &str) -> Option<String> {
    let rest = message.strip_prefix("unknown field `")?;
    let end = rest.find('`')?;
    rest.get(..end).map(str::to_owned)
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

/// Checks that no unit of `manifest` depends on itself through a cycle.
fn check_cycle(
    errors: &Errors<'_>,
    raw: &RawManifest,
    manifest: &Manifest,
) -> Result<(), ManifestError> {
    let Some(name) = manifest.cycle() else {
        return Ok(());
    };
    let span = raw
        .processes
        .keys()
        .chain(raw.containers.keys())
        .find(|key| key.get_ref() == &name)
        .map(Spanned::span);
    Err(errors.at(
        span.as_ref(),
        ManifestProblem::Invalid {
            message: format!("`{name}` depends on itself through a dependency cycle"),
        },
    ))
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

/// Checks that phase can run `manifest`, whose TOML is `raw`: each unit's
/// name is an app name, and the plan that `mxv up` builds from it keeps
/// phase's rules. It builds that plan with a placeholder host port for
/// each port, so the check needs no workspace.
///
/// A manifest that joins others, as `joined` names, may depend on their
/// units and use their ports. The check leaves out those dependencies,
/// whose apps the other manifests' plans hold, and gives their ports a
/// placeholder too. A merged manifest's plan checks the whole app.
fn check_runnable(
    errors: &Errors<'_>,
    raw: &RawManifest,
    manifest: &Manifest,
    joined: &Joined,
) -> Result<(), ManifestError> {
    let own = |depends_on: &mut Vec<String>| {
        depends_on.retain(|name| !joined.units.contains(name));
    };
    let mut checked = manifest.clone();
    for process in checked.processes.values_mut() {
        own(&mut process.depends_on);
    }
    for container in checked.containers.values_mut() {
        own(&mut container.depends_on);
    }
    let manifest = &checked;
    let span = |name: &str| {
        raw.processes
            .keys()
            .chain(raw.containers.keys())
            .find(|key| key.get_ref() == name)
            .map(Spanned::span)
    };
    for name in manifest.names() {
        if crate::phase_plan::app_name(name).is_err() {
            return Err(errors.at(
                span(name).as_ref(),
                ManifestProblem::UnitName { name: name.clone() },
            ));
        }
    }
    let ports: BTreeMap<String, u16> = manifest
        .ports
        .iter()
        .chain(&joined.ports)
        .zip(1_u16..)
        .map(|(port, number)| (port.clone(), number))
        .collect();
    let id = phase::WorkspaceId::try_from("manifest-check".to_owned()).map_err(|error| {
        errors.at(
            None,
            ManifestProblem::Unrunnable {
                message: error.to_string(),
            },
        )
    })?;
    let workspace = WorkspaceName::try_from("manifest-check".to_owned()).map_err(|error| {
        errors.at(
            None,
            ManifestProblem::Unrunnable {
                message: error.to_string(),
            },
        )
    })?;
    let inputs = crate::phase_plan::Inputs {
        manifest,
        workspace: &workspace,
        id: &id,
        checkout: Path::new("/"),
        roots: Vec::new(),
        ports: &ports,
        grace: crate::runs::STOP_GRACE,
        caller: &crate::phase_plan::CallerEnvironment::default(),
    };
    match crate::phase_plan::build(&inputs) {
        Ok(_) => Ok(()),
        Err(unrunnable) => Err(errors.at(
            unrunnable.unit.as_deref().and_then(span).as_ref(),
            ManifestProblem::Unrunnable {
                message: unrunnable.message,
            },
        )),
    }
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
        Self::read_file(&checkout.join(MANIFEST_FILE), checkout)
    }

    /// Returns the manifest at `path`, whose paths are relative to
    /// `checkout`, or `None` when no file is at `path`. It reads the
    /// workspace manifest above a checkout as [`Manifest::read`] reads the
    /// checkout's own.
    ///
    /// # Errors
    ///
    /// Returns a [`ManifestError`] when the file cannot be read, is larger
    /// than [`MOST_MANIFEST_BYTES`], or breaks the schema.
    pub fn read_file(path: &Path, checkout: &Path) -> Result<Option<Self>, ManifestError> {
        Self::read_file_joining(path, checkout, &Joined::default())
    }

    /// Returns the manifest at `path` as [`Manifest::read_file`] does,
    /// parsed as [`Manifest::parse_joining`] parses it with `joined`.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Manifest::read_file`].
    pub fn read_file_joining(
        path: &Path,
        checkout: &Path,
        joined: &Joined,
    ) -> Result<Option<Self>, ManifestError> {
        let Some(text) = Self::read_unparsed(path)? else {
            return Ok(None);
        };
        Self::parse_at(&text, path, checkout, joined).map(Some)
    }

    /// Parses the manifest `text` as [`Manifest::parse_joining`] does, as
    /// though it were read from `path`, and checks each process's `cwd`
    /// against `checkout` as [`Manifest::read_file`] does.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Manifest::read_file`].
    pub fn parse_at(
        text: &str,
        path: &Path,
        checkout: &Path,
        joined: &Joined,
    ) -> Result<Self, ManifestError> {
        let manifest = Self::parse_joining(text, path, joined)?;
        manifest.check_cwds(checkout, path)?;
        Ok(manifest)
    }

    /// Returns the text of the manifest at `path`, after checking that it
    /// parses, as [`Manifest::read_text`] does, for a file that the person
    /// named, which must exist.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Manifest::read_text`], and a
    /// [`ManifestProblem::Unreadable`] when no file is at `path`.
    pub fn read_named(path: &Path) -> Result<String, ManifestError> {
        let text = Self::read_named_unchecked(path)?;
        Self::parse(&text, path)?;
        Ok(text)
    }

    /// Returns the text of the manifest at `path`, a file that the person
    /// named, without parsing it. It is for the workspace manifest of a
    /// workspace with several checkouts, which refers to the names of
    /// checkouts that exist only once they are cloned, so the caller
    /// checks it with [`Workspace::merged_manifest_with`] after cloning.
    ///
    /// [`Workspace::merged_manifest_with`]: crate::Workspace::merged_manifest_with
    ///
    /// # Errors
    ///
    /// Returns a [`ManifestError`] when the file cannot be read or is
    /// larger than [`MOST_MANIFEST_BYTES`], and a
    /// [`ManifestProblem::Unreadable`] when no file is at `path`.
    pub fn read_named_unchecked(path: &Path) -> Result<String, ManifestError> {
        Self::read_unparsed(path)?.ok_or_else(|| ManifestError {
            path: path.to_path_buf(),
            line: None,
            problem: ManifestProblem::Unreadable {
                message: "no file is at this path".to_owned(),
            },
        })
    }

    /// Returns the text of the manifest at `path`, or `None` when no file
    /// is at `path`. `mxv new` copies a file it reads this way, through
    /// [`Manifest::read_text`], into a new workspace.
    ///
    /// # Errors
    ///
    /// Returns a [`ManifestError`] when the file cannot be read or is
    /// larger than [`MOST_MANIFEST_BYTES`].
    fn read_unparsed(path: &Path) -> Result<Option<String>, ManifestError> {
        let path = path.to_path_buf();
        let unreadable = |error: io::Error| ManifestError {
            path: path.clone(),
            line: None,
            problem: ManifestProblem::Unreadable {
                message: error.to_string(),
            },
        };
        let file = match File::open(&path) {
            Ok(file) => file,
            // A symbolic link that leads nowhere is a file that cannot be
            // read, so the next manifest never takes its place.
            Err(error) if error.kind() == io::ErrorKind::NotFound => match fs::read_link(&path) {
                Ok(target) => {
                    return Err(ManifestError {
                        path,
                        line: None,
                        problem: ManifestProblem::Unreadable {
                            message: format!(
                                "it is a symbolic link to {}, which does not exist",
                                target.display()
                            ),
                        },
                    });
                }
                Err(_) => return Ok(None),
            },
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
        Ok(Some(text))
    }

    /// Returns the text of the manifest at `path`, or `None` when no file
    /// is at `path`, after checking that it parses. `mxv new` copies a
    /// file it reads this way into a new workspace.
    ///
    /// # Errors
    ///
    /// Returns a [`ManifestError`] when the file cannot be read, is larger
    /// than [`MOST_MANIFEST_BYTES`], or fails as [`Manifest::parse`] does.
    pub fn read_text(path: &Path) -> Result<Option<String>, ManifestError> {
        let Some(text) = Self::read_unparsed(path)? else {
            return Ok(None);
        };
        Self::parse(&text, path)?;
        Ok(Some(text))
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
        Self::parse_joining(text, path, &Joined::default())
    }

    /// Parses the manifest `text`, read from `path`, as [`Manifest::parse`]
    /// does, for a manifest that joins other manifests whose names are
    /// `joined`: its units may refer to `joined`'s ports and depend on its
    /// units, as the workspace manifest of a workspace with several
    /// checkouts may refer to the prefixed names of its checkouts. The
    /// returned manifest declares only its own names.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Manifest::parse`].
    pub fn parse_joining(text: &str, path: &Path, joined: &Joined) -> Result<Self, ManifestError> {
        let errors = Errors { path, text };
        let warnings = check_top_level(&errors, text)?;
        let raw: RawManifest = toml::from_str(text).map_err(|error| errors.of_toml(&error))?;
        check_version_and_reserved(&errors, &raw)?;
        let mut ports = Vec::new();
        for port in &raw.ports {
            if !is_unit_name(port.get_ref()) {
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
            .chain(raw.containers.keys())
            .map(|name| name.get_ref().as_str())
            .chain(joined.units.iter().map(String::as_str))
            .collect();
        let known: Vec<String> = ports
            .iter()
            .chain(joined.ports.iter().filter(|port| !ports.contains(port)))
            .cloned()
            .collect();
        if let Some(name) = raw.containers.keys().find(|name| {
            raw.processes
                .keys()
                .any(|process| process.get_ref() == name.get_ref())
        }) {
            return Err(errors.invalid(
                &name.span(),
                format!(
                    "`{}` is both a process and a container, and they share one set of names",
                    name.get_ref()
                ),
            ));
        }
        let mut processes = BTreeMap::new();
        let mut listeners: BTreeMap<String, String> = BTreeMap::new();
        for (name, raw_process) in &raw.processes {
            let process = check_process(&errors, name, raw_process, &known, &names)?;
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
        let mut containers = BTreeMap::new();
        for (name, raw_container) in &raw.containers {
            let container = check_container(&errors, name, raw_container, &known, &names)?;
            for (published, key) in container.ports.iter().zip(raw_container.ports.values()) {
                if let Some(other) = listeners.get(&published.port) {
                    return Err(errors.invalid(
                        &key.span(),
                        format!(
                            "`{other}` and `{}` both listen on the port `{}`, which has at most one listener",
                            name.get_ref(),
                            published.port
                        ),
                    ));
                }
                listeners.insert(published.port.clone(), name.get_ref().clone());
            }
            containers.insert(name.get_ref().clone(), container);
        }
        let globs = |raw: &[Spanned<String>],
                     parse: fn(&str) -> Result<PathGlob, BadGlob>,
                     key: &str| {
            raw.iter()
                    .map(|glob| {
                        parse(glob.get_ref()).map_err(|reason| {
                            // Only the config file's own `disposable` list may
                            // name files or end in a wildcard, so only that
                            // key's message says where such a glob belongs.
                            let elsewhere = match (key, &reason) {
                                ("disposable", BadGlob::NotDirectory | BadGlob::Wildcard) => {
                                    "; only the config file's `disposable` may name files or use a wildcard in the last name"
                                }
                                _ => "",
                            };
                            errors.invalid(
                                &glob.span(),
                                format!(
                                    "the {key} glob {:?} is refused: {reason}{elsewhere}",
                                    glob.get_ref()
                                ),
                            )
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
        };
        let manifest = Self {
            ports,
            processes,
            containers,
            disposable: globs(&raw.disposable, PathGlob::parse_directory, "disposable")?,
            clone_on_new: globs(&raw.clone_on_new, PathGlob::parse_clone, "clone_on_new")?,
            warnings,
        };
        check_cycle(&errors, &raw, &manifest)?;
        check_runnable(&errors, &raw, &manifest, joined)?;
        Ok(manifest)
    }

    /// Returns the process or container `name`, or `None`.
    #[must_use]
    pub fn unit(&self, name: &str) -> Option<Unit<'_>> {
        self.processes
            .get(name)
            .map(Unit::Process)
            .or_else(|| self.containers.get(name).map(Unit::Container))
    }

    /// Returns the manifest with each process, container and port renamed
    /// by `rename`, and every reference to them rewritten to match: a
    /// process's `ports` values, `listen`, `ready.port`, `{port.<name>}`
    /// placeholders and `depends_on`, and a container's published ports,
    /// `ready.port` and `depends_on`. Variable names and volume names keep
    /// their names. A workspace with several checkouts reads each
    /// checkout's manifest renamed this way, so two checkouts can declare
    /// the same name.
    #[must_use]
    pub fn renamed(&self, rename: &dyn Fn(&str) -> String) -> Self {
        let ready = |ready: &Option<Ready>| {
            ready.as_ref().map(|ready| Ready {
                check: match &ready.check {
                    ReadyCheck::Port(port) => ReadyCheck::Port(rename(port)),
                    ReadyCheck::Log(pattern) => ReadyCheck::Log(pattern.clone()),
                },
                timeout: ready.timeout,
            })
        };
        let names = |names: &[String]| names.iter().map(|name| rename(name)).collect();
        let processes = self
            .processes
            .iter()
            .map(|(name, process)| {
                let renamed = Process {
                    argv: process.argv.clone(),
                    cwd: process.cwd.clone(),
                    env: process
                        .env
                        .iter()
                        .map(|(variable, value)| (variable.clone(), value.renamed(rename)))
                        .collect(),
                    ports: process
                        .ports
                        .iter()
                        .map(|(variable, port)| (variable.clone(), rename(port)))
                        .collect(),
                    listen: process.listen.as_deref().map(rename),
                    ready: ready(&process.ready),
                    depends_on: names(&process.depends_on),
                };
                (rename(name), renamed)
            })
            .collect();
        let containers = self
            .containers
            .iter()
            .map(|(name, container)| {
                let mut renamed = container.clone();
                for published in &mut renamed.ports {
                    published.port = rename(&published.port);
                }
                renamed.ready = ready(&container.ready);
                renamed.depends_on = names(&container.depends_on);
                // phase names a volume `<workspace>.<volume>` for every app
                // of the workspace, so a volume keeps its file's prefix
                // to stay apart from another checkout's volume.
                renamed.volumes = container
                    .volumes
                    .iter()
                    .map(|(volume, mount)| (rename(volume), mount.clone()))
                    .collect();
                (rename(name), renamed)
            })
            .collect();
        Self {
            ports: names(&self.ports),
            processes,
            containers,
            disposable: self.disposable.clone(),
            clone_on_new: self.clone_on_new.clone(),
            warnings: self.warnings.clone(),
        }
    }

    /// Returns the names of every process and container, in name order.
    fn names(&self) -> BTreeSet<&String> {
        self.processes
            .keys()
            .chain(self.containers.keys())
            .collect()
    }

    /// Returns the processes and containers that `mxv up` starts for
    /// `names`, in the order it starts them: each one after its
    /// dependencies, and those that do not depend on each other in name
    /// order. Empty `names` select every process and container.
    ///
    /// # Errors
    ///
    /// Returns the first name that the manifest declares no process or
    /// container for.
    pub fn start_order(&self, names: &[String]) -> Result<Vec<String>, String> {
        let selected: Vec<&String> = if names.is_empty() {
            self.names().into_iter().collect()
        } else {
            names.iter().collect()
        };
        let mut order = Vec::new();
        for name in selected {
            if self.unit(name).is_none() {
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
        if let Some(unit) = self.unit(name) {
            for dependency in unit.depends_on() {
                self.visit(dependency, order, path);
            }
        }
        path.pop();
        order.push(name.to_owned());
    }

    /// Returns a process that depends on itself, directly or through other
    /// processes, or `None`.
    fn cycle(&self) -> Option<String> {
        self.names()
            .into_iter()
            .find(|name| self.reaches(name, name, &mut BTreeSet::new()))
            .cloned()
    }

    /// Reports whether `target` is a dependency of `from`, directly or
    /// through other processes.
    fn reaches<'a>(&'a self, from: &'a str, target: &str, seen: &mut BTreeSet<&'a str>) -> bool {
        let Some(unit) = self.unit(from) else {
            return false;
        };
        unit.depends_on().iter().any(|dependency| {
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
    if !is_unit_name(process) {
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
    let env = check_env(errors, process, raw, ports)?;
    let templated: BTreeSet<String> = env
        .values()
        .flat_map(EnvValue::ports)
        .map(str::to_owned)
        .collect();
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
            if !port_variables.values().any(|received| received == &port)
                && !templated.contains(&port)
            {
                return Err(errors.invalid(
                    &listen.span(),
                    format!(
                        "the process `{process}` listens on `{port}`, which it does not receive; add a variable for it to `ports`, or name it in an `env` value as `{{port.{port}}}`"
                    ),
                ));
            }
            Some(port)
        }
        None => None,
    };
    let ready = check_ready(
        errors,
        process,
        raw,
        listen.as_ref(),
        |port| port_variables.values().any(|received| received == port) || templated.contains(port),
        &declared,
    )?;
    let depends_on = check_dependencies(errors, process, &raw.depends_on, names)?;
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

/// Checks the `env` table of the process `process`: each variable's name,
/// and each value's braces and the ports that its placeholders name.
fn check_env(
    errors: &Errors<'_>,
    process: &str,
    raw: &RawProcess,
    ports: &[String],
) -> Result<BTreeMap<String, EnvValue>, ManifestError> {
    let mut env = BTreeMap::new();
    for (variable, value) in &raw.env {
        check_variable(errors, process, variable)?;
        let parsed = EnvValue::parse(value.get_ref()).ok_or_else(|| {
            errors.invalid(
                &value.span(),
                format!(
                    "the value of `{}` in `{process}` has a brace that is not part of a port placeholder such as `{{port.api}}`; write `{{{{` or `}}}}` for a literal brace",
                    variable.get_ref()
                ),
            )
        })?;
        if let Some(port) = parsed
            .ports()
            .find(|port| !ports.iter().any(|declared| declared == port))
        {
            return Err(errors.invalid(
                &value.span(),
                format!(
                    "the value of `{}` in `{process}` names the port `{port}`, which `ports` does not declare",
                    variable.get_ref()
                ),
            ));
        }
        env.insert(variable.get_ref().clone(), parsed);
    }
    Ok(env)
}

/// Checks the `ready` table of the process `process`, whose listen port is
/// `listen` and which receives the ports that `receives` accepts, and
/// returns its readiness check. `declared` checks that a port is declared.
fn check_ready(
    errors: &Errors<'_>,
    process: &str,
    raw: &RawProcess,
    listen: Option<&String>,
    receives: impl Fn(&String) -> bool,
    declared: &impl Fn(&Spanned<String>) -> Result<String, ManifestError>,
) -> Result<Option<Ready>, ManifestError> {
    Ok(match &raw.ready {
        Some(spanned) => {
            let ready = spanned.get_ref();
            let timeout = ready_timeout(errors, process, ready)?;
            let check = match (&ready.port, &ready.log) {
                (Some(_), Some(_)) => {
                    return Err(errors.invalid(
                        &spanned.span(),
                        format!(
                            "the `ready` table of `{process}` sets both `port` and `log`, and a readiness check waits for one of them"
                        ),
                    ));
                }
                (None, Some(log)) => {
                    let pattern = Regex::new(log.get_ref()).map_err(|error| {
                        errors.invalid(
                            &log.span(),
                            format!(
                                "the `log` pattern of `{process}` is not a regular expression: {}",
                                regex_problem(&error)
                            ),
                        )
                    })?;
                    ReadyCheck::Log(LogPattern(pattern))
                }
                (Some(port), None) => {
                    let port_name = declared(port)?;
                    if listen != Some(&port_name) && !receives(&port_name) {
                        return Err(errors.invalid(
                            &port.span(),
                            format!(
                                "the readiness port `{port_name}` of `{process}` is neither its listen port nor a port it receives"
                            ),
                        ));
                    }
                    ReadyCheck::Port(port_name)
                }
                (None, None) => ReadyCheck::Port(listen.cloned().ok_or_else(|| {
                    errors.invalid(
                        &raw.argv.span(),
                        format!(
                            "the `ready` table of `{process}` names no port or log pattern, and the process has no `listen` port to default to"
                        ),
                    )
                })?),
            };
            Some(Ready { check, timeout })
        }
        None => listen.cloned().map(|port| Ready {
            check: ReadyCheck::Port(port),
            timeout: DEFAULT_READY_TIMEOUT,
        }),
    })
}

/// Returns what is wrong with a pattern that `error` rejected, on one
/// line. A syntax error prints the pattern with a caret under the problem
/// and then a line `error: <problem>`, and only that problem is kept.
fn regex_problem(error: &regex::Error) -> String {
    let text = error.to_string();
    text.lines()
        .find_map(|line| line.trim().strip_prefix("error: "))
        .map_or_else(
            || text.split_whitespace().collect::<Vec<_>>().join(" "),
            str::to_owned,
        )
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

/// Returns the timeout of the readiness check `ready` of `unit`.
fn ready_timeout(
    errors: &Errors<'_>,
    unit: &str,
    ready: &RawReady,
) -> Result<Duration, ManifestError> {
    match &ready.timeout_seconds {
        Some(seconds) if (1..=MOST_READY_SECONDS).contains(seconds.get_ref()) => {
            Ok(Duration::from_secs(*seconds.get_ref()))
        }
        Some(seconds) => Err(errors.invalid(
            &seconds.span(),
            format!("timeout_seconds of `{unit}` is not between 1 and {MOST_READY_SECONDS}"),
        )),
        None => Ok(DEFAULT_READY_TIMEOUT),
    }
}

/// Checks that each of the `dependencies` of `unit` names a process or
/// container of `names`, and returns them.
fn check_dependencies(
    errors: &Errors<'_>,
    unit: &str,
    dependencies: &[Spanned<String>],
    names: &BTreeSet<&str>,
) -> Result<Vec<String>, ManifestError> {
    let mut depends_on = Vec::new();
    for dependency in dependencies {
        if !names.contains(dependency.get_ref().as_str()) {
            return Err(errors.invalid(
                &dependency.span(),
                format!(
                    "`{unit}` depends on `{}`, which the manifest does not declare",
                    dependency.get_ref()
                ),
            ));
        }
        depends_on.push(dependency.get_ref().clone());
    }
    Ok(depends_on)
}

/// Reports whether `image` is pinned: it carries a digest, or a tag other
/// than `latest`. A tag follows the last `:` after the last `/`, because a
/// registry's host may carry a port, as in `localhost:5000/busybox`.
fn is_pinned(image: &str) -> bool {
    if image.contains('@') {
        return true;
    }
    let last = image.rsplit('/').next().unwrap_or(image);
    last.rsplit_once(':')
        .is_some_and(|(_, tag)| !tag.is_empty() && tag != "latest")
}

/// Reports whether `image` holds only the characters of an image
/// reference and does not start with `-`, so docker never reads it as an
/// option.
fn is_image(image: &str) -> bool {
    image.len() <= MOST_IMAGE_BYTES
        && image
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric())
        && image
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-/:@".contains(character))
}

/// Checks one raw container against the declared `ports` and the `names`
/// of processes and containers, and returns it.
fn check_container(
    errors: &Errors<'_>,
    name: &Spanned<String>,
    raw: &RawContainer,
    ports: &[String],
    names: &BTreeSet<&str>,
) -> Result<Container, ManifestError> {
    let container = name.get_ref();
    if !is_unit_name(container) {
        return Err(errors.invalid(
            &name.span(),
            format!("`{container}` is not a container name, which holds 1 to 64 letters, digits, '-' and '_'"),
        ));
    }
    let image = raw.image.get_ref();
    if !is_image(image) {
        return Err(errors.invalid(
            &raw.image.span(),
            format!("the image `{image}` of `{container}` is not an image reference"),
        ));
    }
    if !is_pinned(image) {
        return Err(errors.invalid(
            &raw.image.span(),
            format!(
                "the image `{image}` of `{container}` is not pinned; pin it with a tag other than `latest`, or with a digest"
            ),
        ));
    }
    let mut env = BTreeMap::new();
    for (variable, value) in &raw.env {
        check_variable(errors, container, variable)?;
        env.insert(variable.get_ref().clone(), Redacted::new(value.clone()));
    }
    let mut published = Vec::new();
    for (key, port) in &raw.ports {
        let number = key
            .get_ref()
            .parse::<u16>()
            .ok()
            .filter(|number| *number > 0)
            .ok_or_else(|| {
                errors.invalid(
                    &key.span(),
                    format!(
                        "`{}` of `{container}` is not a container port, which is a number from 1 to 65535",
                        key.get_ref()
                    ),
                )
            })?;
        if !ports.contains(port.get_ref()) {
            return Err(errors.invalid(
                &port.span(),
                format!(
                    "the container `{container}` publishes the port `{}`, which `ports` does not declare",
                    port.get_ref()
                ),
            ));
        }
        published.push(Published {
            container: number,
            port: port.get_ref().clone(),
        });
    }
    published.sort_by_key(|published| published.container);
    let mut volumes = BTreeMap::new();
    for (volume, path) in &raw.volumes {
        if !is_unit_name(volume.get_ref()) {
            return Err(errors.invalid(
                &volume.span(),
                format!(
                    "`{}` of `{container}` is not a volume name, which holds 1 to 64 letters, digits, '-' and '_'",
                    volume.get_ref()
                ),
            ));
        }
        let mount = path.get_ref();
        if !mount.starts_with('/') || mount.contains([':', ',']) {
            return Err(errors.invalid(
                &path.span(),
                format!(
                    "the volume `{}` of `{container}` mounts at `{mount}`, which is not an absolute path without ':' or ','",
                    volume.get_ref()
                ),
            ));
        }
        volumes.insert(volume.get_ref().clone(), mount.clone());
    }
    let only = match published.as_slice() {
        [one] => Some(one.port.clone()),
        _ => None,
    };
    let ready = match &raw.ready {
        Some(ready) => {
            let timeout = ready_timeout(errors, container, ready)?;
            if let Some(log) = &ready.log {
                return Err(errors.invalid(
                    &log.span(),
                    format!(
                        "the `ready` table of `{container}` sets `log`, and a container's readiness check waits only for a port"
                    ),
                ));
            }
            let port = match &ready.port {
                Some(port) => {
                    if !published.iter().any(|published| &published.port == port.get_ref()) {
                        return Err(errors.invalid(
                            &port.span(),
                            format!(
                                "the readiness port `{}` of `{container}` is not a port it publishes",
                                port.get_ref()
                            ),
                        ));
                    }
                    port.get_ref().clone()
                }
                None => only.clone().ok_or_else(|| {
                    errors.invalid(
                        &raw.image.span(),
                        format!(
                            "the `ready` table of `{container}` names no port, and the container does not publish exactly one port to default to"
                        ),
                    )
                })?,
            };
            Some(Ready {
                check: ReadyCheck::Port(port),
                timeout,
            })
        }
        None => only.map(|port| Ready {
            check: ReadyCheck::Port(port),
            timeout: DEFAULT_READY_TIMEOUT,
        }),
    };
    let depends_on = check_dependencies(errors, container, &raw.depends_on, names)?;
    Ok(Container {
        image: image.clone(),
        command: raw.command.clone(),
        env,
        ports: published,
        volumes,
        ready,
        depends_on,
    })
}
