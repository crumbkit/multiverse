//! The manifest `mxv.toml` at a repository's root, which declares the
//! application's named ports and the processes and containers that
//! `mxv up` starts.
//!
//! The manifest is parsed strictly: a key that the schema does not declare
//! fails with [`ManifestProblem::UnknownKey`], which names the key and its
//! line. The top-level section `actions` is reserved for a later version
//! and fails with [`ManifestProblem::Reserved`]. A `[secrets.<name>]` table
//! declares a credential with the `check` command that uses it and the
//! `sign_in` command line that obtains it.
//!
//! Processes and containers share one set of names, and either may depend
//! on the other.
//!
//! ```toml
//! ports = ["api", "web", "dynamodb"]
//! routes = ["web"]
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
//! argv = ["./target/debug/api"]
//! build = ["cargo", "build", "--bin", "api"]
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
//! A process's `starmap` names the starmap file that declares the
//! variables it reads, for tools such as jumpgate that read the manifest:
//!
//! ```toml
//! [processes.cli]
//! argv = ["./target/debug/cli"]
//! cwd = "cli"
//! starmap = "cli/starmap.toml"
//! ```
//!
//! The path is relative to the top of the checkout, whatever the
//! process's `cwd`. A leading `./` and repeated or trailing `/` are
//! dropped, and an empty path, a path that starts with `/` and a path with
//! a `..` name fail with [`ManifestProblem::Invalid`]. A read against a
//! checkout, such as [`Manifest::read`], also checks that the file is a
//! regular file inside the checkout, and fails with
//! [`ManifestProblem::StarmapFile`] otherwise. The plan of `mxv up` reads
//! the file and gives the process its variables whose bind is
//! `multiverse/port` or `multiverse/url`. A process without the key names
//! none, and mxv and such tools then read `<cwd>/starmap.toml` when it
//! exists.
//!
//! A process's `build` is a command that `mxv up` and `mxv restart` run
//! before the process starts, under a place from the machine's job limiter,
//! as [`crate::Runs::build`] describes. No shell reads it.
//!
//! `routes` names ports of `ports` that the proxy serves at the host name
//! `<port>.<workspace>.mxv.localhost`.
//!
//! An `env` value may name a host port as `{port.<name>}`, and writes a
//! literal brace as `{{` or `}}`. Any other brace fails the manifest.
//!
//! `[tasks.<name>]` tables declare the commands that `mxv run` runs, such
//! as a test suite, which `mxv up` never starts:
//!
//! ```toml
//! [tasks.test]
//! dir = "."
//! argv = ["cargo", "nextest", "run"]
//! needs = ["queues"]
//! gate = false
//! ```
//!
//! `dir` defaults to the top of the checkout, and no shell reads `argv`. A
//! task takes one place from the machine's job limiter unless `gate` is
//! `false`.
//!
//! `[service.<name>]` tables declare local AWS services, which a process
//! lists in `needs`, as [`crate::service`] describes. A process, container
//! or port may not have a name that starts with `mxv-`, which mxv keeps
//! for the apps and ports of the services' backends.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{self, Read as _};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use phase::BadLogPattern;
pub use phase::LogPattern;
use serde::Deserialize;
use serde::de::IgnoredAny;
use toml::Spanned;

use crate::braces::{self, Piece};
use crate::disposable::{BadGlob, PathGlob};
use crate::host::ManifestNames;
use crate::name::{WorkspaceName, is_unit_name};
use crate::needs::{Declared, Holder};
use crate::redacted::Redacted;
use crate::service::{Backend, PROVISION_APP, RESERVED_PREFIX, Service};
use crate::store;

mod services;
#[cfg(test)]
mod tests;

/// The name of the manifest file at a repository's root under mxv's
/// profile, [`Host::mxv`](crate::Host::mxv). Another host's file is
/// [`Host::manifest_file`](crate::Host::manifest_file).
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Manifest {
    /// The named ports, in the order declared. mxv gives each workspace its
    /// own host port for each name.
    pub ports: Vec<String>,
    /// The ports, by name, that the proxy serves at a stable host name
    /// `<port>.<workspace>.mxv.localhost`. Each is one of [`Manifest::ports`]
    /// or of the ports that a manifest joins.
    pub routes: Vec<String>,
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
    /// The local AWS services, by name, which processes list in `needs`.
    /// A workspace with several checkouts shares one set of service names,
    /// and merges equal declarations of one name.
    pub services: BTreeMap<String, Service>,
    /// The tasks that `mxv run` runs, by name. A merged manifest holds
    /// none, because [`Workspace::task_homes`] keeps each file's tasks apart.
    ///
    /// [`Workspace::task_homes`]: crate::Workspace::task_homes
    pub tasks: BTreeMap<TaskName, Task>,
    /// The credentials the workspace's work needs, by name, each with the
    /// command that proves it works and the command that signs in when it
    /// does not. `mxv preflight` runs each check.
    pub secrets: BTreeMap<String, Secret>,
    /// One sentence for each top-level table that mxv ignores although
    /// its name is close to one of mxv's own, such as `[proceses.web]`,
    /// or is phase's `[process.*]`, and for each `[needs.*]` or
    /// `[provides.*]` table that [`crate::needs::parse`] refuses, with the
    /// file and line in front.
    pub warnings: Vec<String>,
    /// The `[needs.*]` and `[provides.*]` tables, by the file that
    /// declares them. A manifest read from one file holds them under
    /// [`Holder::Workspace`], and a workspace's merged manifest holds each
    /// checkout's under [`Holder::Checkout`] with the checkout's folder. A
    /// file that declares none adds no entry. The tables name units by
    /// their names in their own file.
    pub needs: BTreeMap<Holder, Declared>,
}

/// The name of a task of a manifest, which follows the rule of a unit's
/// name: 1 to 64 letters, digits, '-' and '_'.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TaskName(String);

impl TaskName {
    /// Returns the name `text`, or `None` when it breaks the rule.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        is_unit_name(text).then(|| Self(text.to_owned()))
    }

    /// Returns the name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TaskName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A credential that a workspace's work needs, declared under
/// `[secrets.<name>]`. The manifest holds no value: it holds the command
/// that uses the credential and the command that obtains it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Secret {
    /// A command that uses the credential for real, such as `aws sts
    /// get-caller-identity` or `op read` of one item. It succeeds when the
    /// credential works. mxv discards its output, because a read command
    /// prints the value.
    pub check: Command,
    /// The full command line the person runs to sign in, such as
    /// `aws sso login --profile work`.
    pub sign_in: String,
}

/// A command that `mxv run` runs, declared under `[tasks.<name>]`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Task {
    /// The directory it runs in, relative to the checkout.
    pub dir: PathBuf,
    /// Its program and arguments, which no shell reads.
    pub argv: Command,
    /// The services whose variables it receives.
    pub needs: Vec<String>,
    /// Whether it takes a place from the machine's job limiter.
    pub gate: bool,
}

/// The names of the other manifests that a manifest joins, which its units
/// may refer to, as [`Manifest::parse_joining`] describes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Joined {
    /// The ports of the other manifests.
    pub ports: Vec<String>,
    /// The processes and containers of the other manifests.
    pub units: Vec<String>,
    /// The ports that a process of the other manifests listens on or a
    /// container of them publishes, each with that unit's name.
    pub listened: Vec<(String, String)>,
    /// The folders of the workspace's checkouts, when the manifest is the
    /// workspace manifest of a workspace with several checkouts. Its write
    /// paths are then relative to the workspace's directory, and each
    /// must lie inside one of these folders.
    pub checkouts: Vec<String>,
    /// The services of the other manifests, by name. Services keep their
    /// names when manifests merge.
    pub services: BTreeMap<String, Service>,
}

impl Joined {
    /// Adds the ports, units and services of `manifest`, and the ports
    /// that its units listen on or publish.
    pub fn add(&mut self, manifest: &Manifest) {
        self.services.extend(
            manifest
                .services
                .iter()
                .map(|(name, service)| (name.clone(), service.clone())),
        );
        self.ports.extend(manifest.ports.iter().cloned());
        self.units.extend(
            manifest
                .processes
                .keys()
                .chain(manifest.containers.keys())
                .cloned(),
        );
        self.listened.extend(manifest.listeners());
    }

    /// Adds the checkout in the folder `folder` of the workspace.
    pub fn add_checkout(&mut self, folder: &str) {
        self.checkouts.push(folder.to_owned());
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
    /// The owner that the root directory of each named volume gets, as
    /// phase's numeric `uid` or `uid:gid`, or `keep` to leave each volume
    /// as it is. `None` leaves the choice to phase, which gives the
    /// volumes to the user that the container runs as.
    pub owner: Option<String>,
    /// The check that tells `mxv up` the container is ready, or `None`
    /// when it counts as started once it has run for a short grace period.
    /// A container that publishes one port and has no `ready` table is
    /// ready when that port answers.
    pub ready: Option<Ready>,
    /// The processes and containers that start, and are ready, before this
    /// one.
    pub depends_on: Vec<String>,
    /// The extra host names that the proxy serves as more hosts of the
    /// route whose port the container publishes.
    pub hosts: Vec<String>,
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
    /// What the process may do under phase's sandbox, or `None` when it
    /// runs unsandboxed. An empty [`Permissions`] runs it sandboxed with no
    /// grants beyond phase's own.
    pub permissions: Option<Permissions>,
    /// The services that the process uses, by name. The process starts
    /// after their backends are ready and their resources exist, and
    /// receives their variables.
    pub needs: Vec<String>,
    /// The command that builds the process, which `mxv up` and `mxv
    /// restart` run before they start it, or `None` when it has no build
    /// step. It is not part of phase's plan, so a change to it alone does
    /// not restart a running process.
    pub build: Option<Command>,
    /// The extra host names that the proxy serves as more hosts of the
    /// route whose port the process listens on.
    pub hosts: Vec<String>,
    /// The starmap file that declares the variables the process reads, or
    /// `None` when the process names none, and tools such as jumpgate then
    /// read `<cwd>/starmap.toml`. Its path is relative to the top of the
    /// checkout, whatever the process's `cwd`, and holds no `..`. A merged
    /// manifest holds it as an absolute path below the directory that its
    /// file's paths are relative to. mxv checks that the file exists, and
    /// the plan of `mxv up` reads its provided variables.
    pub starmap: Option<PathBuf>,
}

/// A program and its arguments, as separate values that no shell reads.
/// It always holds a program, because [`Command::new`] is the only way to
/// make one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command(Vec<String>);

impl Command {
    /// Returns the command of `words`, whose first word is the program, or
    /// `None` when `words` is empty or its program is empty.
    #[must_use]
    pub fn new(words: Vec<String>) -> Option<Self> {
        words
            .first()
            .is_some_and(|program| !program.is_empty())
            .then_some(Self(words))
    }

    /// Returns the program and its arguments.
    #[must_use]
    pub fn words(&self) -> &[String] {
        &self.0
    }
}

/// What a sandboxed process may do beyond what phase's sandbox always
/// allows, which is to read its checkout and the system's directories and
/// to write its own state and temporary directories.
///
/// phase's first-stage sandbox lets a process connect over loopback only to
/// the ports of the units that `connect` names. `connect` must name only
/// units of `depends_on`, and the manifest fails otherwise. A unit of
/// `depends_on` that `connect` leaves out is unreachable. A sandboxed process
/// cannot reach any external host until phase runs its allowlist proxy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Permissions {
    /// The paths that the process may write: `$STATE`, the process's own
    /// state directory, or a path relative to the checkout without `..`,
    /// such as `target`. A merged manifest holds each relative path as an
    /// absolute path below the directory that its file's paths are
    /// relative to.
    pub write: BTreeSet<String>,
    /// The ports of [`Manifest::ports`] that the process may listen on.
    /// Each is its `listen` port or a port it receives, and a process with
    /// a `listen` port must name it.
    pub listen: BTreeSet<String>,
    /// The processes and containers that the process may connect to, each
    /// one a unit that it depends on.
    pub connect: BTreeSet<String>,
}

/// The write path of a process's [`Permissions`] that names the process's
/// own state directory.
pub(crate) const STATE: &str = "$STATE";

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
        let parts = braces::split(text, |inside| {
            inside
                .strip_prefix("port.")
                .filter(|port| is_unit_name(port))
                .map(str::to_owned)
        })
        .ok()?
        .into_iter()
        .map(|piece| match piece {
            Piece::Text(literal) => Part::Text(literal),
            Piece::Placeholder(port) => Part::Port(port),
        })
        .collect();
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
    /// Two services declare one table or queue differently, or bind one
    /// variable of a process that needs both to different values. Every
    /// service of a workspace shares the backend of its kind, so a
    /// resource name means one resource.
    ServiceClash {
        /// The resource's or the variable's name.
        name: String,
        /// What clashes, in a sentence.
        message: String,
    },
    /// The file that a process's `starmap` key names is missing, is a
    /// symbolic link, is not a regular file, reaches outside its checkout
    /// through a linked directory, or cannot be checked.
    StarmapFile {
        /// The process's name, as its manifest writes it.
        unit: String,
        /// The file, relative to the checkout, as the manifest names it
        /// once normalised.
        file: PathBuf,
        /// What mxv found at the file's path.
        found: StarmapFound,
    },
}

/// What mxv found at the path of a process's starmap file that is not a
/// regular file inside the checkout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StarmapFound {
    /// No file is at the path.
    Missing,
    /// The path is a symbolic link, or reaches outside the checkout
    /// through a linked directory.
    Link,
    /// The path is a directory or another kind of file that is not a
    /// regular file.
    NotAFile,
    /// The path's file type cannot be read.
    Unreadable,
}

impl StarmapFound {
    /// Returns the value's name in an error's fields, such as `missing`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Link => "link",
            Self::NotAFile => "not_a_file",
            Self::Unreadable => "unreadable",
        }
    }
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
            ManifestProblem::Invalid { message }
            | ManifestProblem::Unrunnable { message }
            | ManifestProblem::ServiceClash { message, .. } => {
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
            ManifestProblem::StarmapFile { unit, file, found } => {
                let what = match found {
                    StarmapFound::Missing => "does not exist",
                    StarmapFound::Link => {
                        "is a symbolic link or reaches outside the checkout through a linked directory, and a starmap file is a regular file inside the checkout"
                    }
                    StarmapFound::NotAFile => "is not a regular file",
                    StarmapFound::Unreadable => {
                        "cannot be checked, because its file type cannot be read"
                    }
                };
                write!(
                    formatter,
                    "{place}: the starmap file {} of `{unit}` {what}",
                    file.display()
                )
            }
        }
    }
}

impl std::error::Error for ManifestError {}

/// The top-level keys that mxv reads. [`Manifest::parse`] leaves any other
/// top-level table to the tools that read it.
const OWN_KEYS: &[&str] = &[
    "version",
    "ports",
    "routes",
    "processes",
    "containers",
    "disposable",
    "clone_on_new",
    "service",
    "tasks",
    "secrets",
    "actions",
    "needs",
    "provides",
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
    /// The ports that the proxy serves.
    #[serde(default)]
    routes: Vec<Spanned<String>>,
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
    /// The local AWS services.
    #[serde(default)]
    service: BTreeMap<Spanned<String>, services::RawService>,
    /// The tasks.
    #[serde(default)]
    tasks: BTreeMap<Spanned<String>, RawTask>,
    /// The credentials the work needs.
    #[serde(default)]
    secrets: BTreeMap<Spanned<String>, RawSecret>,
    /// Reserved for actions.
    actions: Option<Spanned<IgnoredAny>>,
}

/// One secret as TOML holds it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSecret {
    /// The command that uses the credential.
    check: Spanned<Vec<String>>,
    /// The command line that signs in.
    sign_in: Spanned<String>,
}

/// One task as TOML holds it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTask {
    /// The directory it runs in.
    dir: Option<Spanned<String>>,
    /// The program and its arguments.
    argv: Spanned<Vec<String>>,
    /// The services it uses.
    #[serde(default)]
    needs: Vec<Spanned<String>>,
    /// Whether it takes a place from the machine's job limiter.
    gate: Option<bool>,
}

/// One process as TOML holds it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProcess {
    /// The program and its arguments.
    argv: Spanned<Vec<String>>,
    /// The command that builds the process.
    build: Option<Spanned<Vec<String>>>,
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
    /// The services the process uses.
    #[serde(default)]
    needs: Vec<Spanned<String>>,
    /// The sandbox's permissions.
    permissions: Option<Spanned<RawPermissions>>,
    /// The extra host names of the route.
    #[serde(default)]
    hosts: Vec<Spanned<String>>,
    /// The starmap file that declares the variables the process reads.
    starmap: Option<Spanned<String>>,
}

/// The permissions of a process as TOML holds them. A key that is not one
/// of these, such as phase's `secrets`, fails as unknown.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPermissions {
    /// The paths that the process may write.
    #[serde(default)]
    write: Vec<Spanned<String>>,
    /// The ports that the process may listen on.
    #[serde(default)]
    listen: Vec<Spanned<String>>,
    /// The units that the process may connect to.
    #[serde(default)]
    connect: Vec<Spanned<String>>,
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
    /// The owner of the volumes' root directories.
    owner: Option<Spanned<String>>,
    /// The readiness check.
    ready: Option<RawReady>,
    /// The dependencies.
    #[serde(default)]
    depends_on: Vec<Spanned<String>>,
    /// Permissions, which a container cannot take, so it fails with a
    /// sentence that says so.
    permissions: Option<Spanned<IgnoredAny>>,
    /// The extra host names of the route.
    #[serde(default)]
    hosts: Vec<Spanned<String>>,
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
    /// The names of the host that reads the manifest.
    names: &'a ManifestNames,
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
/// mxv calls `processes`. phase's top-level `permissions` is not among
/// them: mxv refuses it, because mxv sandboxes each process by its own
/// `[processes.<name>.permissions]`. `[needs.*]` is mxv's own, which
/// [`crate::needs`] reads.
const PHASE_TABLES: &[&str] = &["app", "release", "build", "process"];

/// The spellings that `[needs.<name>]` and `[provides.<name>]` replaced,
/// which [`crate::needs::parse`] refuses with a warning of its own, so the
/// check of top-level tables gives them none.
const RENAMED_TABLES: &[&str] = &["wants", "offers"];

/// Checks that no port, process or container of `raw` starts with the
/// prefix that the host keeps for its own names, or with
/// [`RESERVED_PREFIX`].
fn check_reserved_names(errors: &Errors<'_>, raw: &RawManifest) -> Result<(), ManifestError> {
    if let Some((name, prefix)) = raw
        .ports
        .iter()
        .chain(raw.processes.keys())
        .chain(raw.containers.keys())
        .find_map(|name| {
            reserved_prefix(name.get_ref(), &errors.names.unit_prefix).map(|prefix| (name, prefix))
        })
    {
        return Err(errors.invalid(&name.span(), reserved_message(name.get_ref(), prefix)));
    }
    Ok(())
}

/// Returns the reserved prefix that `name` starts with: `unit_prefix`, the
/// host's own, or [`RESERVED_PREFIX`], which names the apps and ledger
/// ports that [`crate::Runs`] adds under every host, such as
/// `mxv-services` and `mxv-aws`.
pub(crate) fn reserved_prefix<'a>(name: &str, unit_prefix: &'a str) -> Option<&'a str> {
    [unit_prefix, RESERVED_PREFIX]
        .into_iter()
        .find(|prefix| name.starts_with(prefix))
}

/// Checks that `raw`'s `version` is 1 or greater and no greater than
/// [`MANIFEST_VERSION`], and that it holds none of the reserved table
/// `actions`.
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
    if let Some(value) = &raw.actions {
        return Err(errors.at(
            Some(&value.span()),
            ManifestProblem::Reserved {
                section: "actions".to_owned(),
            },
        ));
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
            toml::de::DeValue::String(_)
            | toml::de::DeValue::Integer(_)
            | toml::de::DeValue::Float(_)
            | toml::de::DeValue::Boolean(_)
            | toml::de::DeValue::Datetime(_) => false,
        };
        if !tables {
            return Err(errors.at(
                Some(&key.span()),
                ManifestProblem::UnknownKey {
                    key: name.to_owned(),
                },
            ));
        }
        if name == "permissions" {
            return Err(errors.invalid(
                &key.span(),
                "the top-level table `[permissions]` sandboxes nothing, because mxv sandboxes each process on its own; move its keys to `[processes.<name>.permissions]` of each process that should run sandboxed".to_owned(),
            ));
        }
        let place = format!("{}:{}", errors.path.display(), errors.line(&key.span()));
        if name == "process" {
            warnings.push(format!(
                "{place}: mxv ignores the table `[process.*]`, which is phase's, and runs only the processes of `[processes.*]`"
            ));
        } else if !PHASE_TABLES.contains(&name)
            && !RENAMED_TABLES.contains(&name)
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

/// Returns the sentence of a process, container or port named `name`,
/// which starts with `prefix`, the prefix that the host keeps for its own
/// names.
pub(crate) fn reserved_message(name: &str, prefix: &str) -> String {
    format!(
        "`{name}` starts with `{prefix}`, which mxv keeps for the apps and ports of the services' backends"
    )
}

/// Returns the `[needs.*]` and `[provides.*]` tables of the manifest
/// `text`, read from `path`, under [`Holder::Workspace`], and adds a
/// warning to `warnings` for each table that [`crate::needs::parse`]
/// refuses, with the file, line and code in front. A provide may name a
/// unit of `units`, the units of the file and of the manifests it joins.
fn needs_of(
    text: &str,
    path: &Path,
    units: &BTreeSet<&str>,
    warnings: &mut Vec<String>,
) -> BTreeMap<Holder, Declared> {
    let parsed = crate::needs::parse(text, &|unit| units.contains(unit));
    for refusal in &parsed.refusals {
        let place = refusal.line.map_or_else(
            || path.display().to_string(),
            |line| format!("{}:{line}", path.display()),
        );
        warnings.push(format!(
            "{place}: {}: {}",
            refusal.code.as_str(),
            refusal.message
        ));
    }
    if parsed.declared.is_empty() {
        BTreeMap::new()
    } else {
        BTreeMap::from([(Holder::Workspace, parsed.declared)])
    }
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
        if let Some(permissions) = &mut process.permissions {
            permissions
                .connect
                .retain(|name| !joined.units.contains(name));
        }
        process
            .needs
            .retain(|service| manifest.services.contains_key(service));
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
        .ledger_ports()
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
        workspace_dir: None,
        grace: crate::runs::STOP_GRACE,
        caller: &crate::phase_plan::CallerEnvironment::default(),
        own_command: Some(crate::phase_plan::checking_own_command()),
        urls: crate::phase_plan::ProxyUrls::default(),
        read_starmaps: false,
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
        Self::read_as(checkout, &ManifestNames::MXV)
    }

    /// Returns the manifest at `<checkout>/<file>`, where `names` holds
    /// the host's file name, read under the host's `names` as
    /// [`Manifest::read`] reads mxv's.
    pub(crate) fn read_as(
        checkout: &Path,
        names: &ManifestNames,
    ) -> Result<Option<Self>, ManifestError> {
        Self::read_file_as(
            &checkout.join(&*names.file),
            checkout,
            &Joined::default(),
            names,
        )
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
        Self::read_file_as(path, checkout, joined, &ManifestNames::MXV)
    }

    /// Returns the manifest at `path` as [`Manifest::read_file_joining`]
    /// does, under the host's `names`.
    pub(crate) fn read_file_as(
        path: &Path,
        checkout: &Path,
        joined: &Joined,
        names: &ManifestNames,
    ) -> Result<Option<Self>, ManifestError> {
        let Some(text) = Self::read_unparsed(path)? else {
            return Ok(None);
        };
        Self::parse_at_as(&text, path, checkout, joined, names).map(Some)
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
        Self::parse_at_as(text, path, checkout, joined, &ManifestNames::MXV)
    }

    /// Parses the manifest `text` as [`Manifest::parse_at`] does, under
    /// the host's `names`.
    pub(crate) fn parse_at_as(
        text: &str,
        path: &Path,
        checkout: &Path,
        joined: &Joined,
        names: &ManifestNames,
    ) -> Result<Self, ManifestError> {
        let manifest = Self::parse_in(text, path, joined, Some(checkout), names)?;
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
        let file = match store::open(&path) {
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

    /// Checks that each process's `cwd` and each task's `dir` that exists
    /// in `checkout` stays inside it once symbolic links are resolved. One
    /// that does not exist is left for the command that starts it to
    /// report.
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
        for (name, task) in &self.tasks {
            if let Ok(resolved) = checkout.join(&task.dir).canonicalize()
                && !resolved.starts_with(&root)
            {
                return Err(ManifestError {
                    path: path.to_path_buf(),
                    line: None,
                    problem: ManifestProblem::Invalid {
                        message: format!(
                            "the dir `{}` of the task `{name}` leaves the repository through a symbolic link to {}",
                            task.dir.display(),
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
        Self::parse_in(text, path, joined, None, &ManifestNames::MXV)
    }

    /// Parses the manifest `text` as [`Manifest::parse_joining`] does, and
    /// when `checkout` names the directory that its paths are relative to,
    /// checks that each process's starmap file is a regular file there.
    fn parse_in(
        text: &str,
        path: &Path,
        joined: &Joined,
        checkout: Option<&Path>,
        names: &ManifestNames,
    ) -> Result<Self, ManifestError> {
        let errors = Errors { path, text, names };
        let mut warnings = check_top_level(&errors, text)?;
        let raw: RawManifest = toml::from_str(text).map_err(|error| errors.of_toml(&error))?;
        check_version_and_reserved(&errors, &raw)?;
        let ports = check_ports(&errors, &raw.ports)?;
        let routes = check_routes(&errors, &raw.routes, &ports, joined)?;
        check_reserved_names(&errors, &raw)?;
        let services = services::check_services(&errors, &raw.service)?;
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
        let mut hosted: BTreeSet<String> = BTreeSet::new();
        for (name, raw_process) in &raw.processes {
            let mut process = check_process(
                &errors,
                name,
                raw_process,
                &known,
                &names,
                &Root {
                    checkouts: &joined.checkouts,
                    checkout,
                },
            )?;
            let own: Vec<&Spanned<String>> = raw_process
                .env
                .keys()
                .chain(raw_process.ports.keys())
                .collect();
            process.needs = services::check_needs(
                &errors,
                name.get_ref(),
                &raw_process.needs,
                &own,
                &services,
                &joined.services,
            )?;
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
            process.hosts = check_hosts(
                &errors,
                &Hosted {
                    kind: "process",
                    unit: name.get_ref(),
                    ports: process.listen.iter().map(String::as_str).collect(),
                    routes: &routes,
                },
                &raw_process.hosts,
                &mut hosted,
            )?;
            processes.insert(name.get_ref().clone(), process);
        }
        let containers = check_containers(
            &errors,
            &raw,
            &Shared {
                known: &known,
                names: &names,
                routes: &routes,
            },
            &mut listeners,
            &mut hosted,
        )?;
        check_listen_grants(&errors, &raw, &listeners, joined)?;
        let mut tasks = BTreeMap::new();
        for (name, raw_task) in &raw.tasks {
            let (task_name, task) = check_task(&errors, name, raw_task, &services, joined)?;
            tasks.insert(task_name, task);
        }
        let secrets = check_secrets(&errors, &raw.secrets)?;
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
            routes,
            processes,
            containers,
            disposable: globs(&raw.disposable, PathGlob::parse_directory, "disposable")?,
            clone_on_new: globs(&raw.clone_on_new, PathGlob::parse_clone, "clone_on_new")?,
            services,
            tasks,
            secrets,
            needs: needs_of(text, path, &names, &mut warnings),
            warnings,
        };
        check_cycle(&errors, &raw, &manifest)?;
        check_runnable(&errors, &raw, &manifest, joined)?;
        Ok(manifest)
    }

    /// Returns the manifest with the needs and provides of its own file,
    /// which a manifest read from one file holds under
    /// [`Holder::Workspace`], held by `holder` in their place.
    #[must_use]
    pub(crate) fn held_by(mut self, holder: Holder) -> Self {
        if let Some(declared) = self.needs.remove(&Holder::Workspace) {
            self.needs.insert(holder, declared);
        }
        self
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
                    permissions: process.permissions.as_ref().map(|permissions| Permissions {
                        write: permissions.write.clone(),
                        listen: permissions.listen.iter().map(|port| rename(port)).collect(),
                        connect: permissions
                            .connect
                            .iter()
                            .map(|unit| rename(unit))
                            .collect(),
                    }),
                    needs: process.needs.clone(),
                    build: process.build.clone(),
                    hosts: process.hosts.clone(),
                    starmap: process.starmap.clone(),
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
            routes: names(&self.routes),
            processes,
            containers,
            disposable: self.disposable.clone(),
            clone_on_new: self.clone_on_new.clone(),
            services: self.services.clone(),
            tasks: self.tasks.clone(),
            secrets: self.secrets.clone(),
            warnings: self.warnings.clone(),
            needs: self.needs.clone(),
        }
    }

    /// Returns the name of the process or container that listens on or
    /// publishes the port `port`, or `None` when no unit does.
    #[must_use]
    pub fn listener_of(&self, port: &str) -> Option<String> {
        self.listeners()
            .into_iter()
            .find(|(listened, _)| listened == port)
            .map(|(_, unit)| unit)
    }

    /// Returns the extra host names of the process or container `unit`, or
    /// none when it lists none or does not exist.
    #[must_use]
    pub fn hosts_of(&self, unit: &str) -> &[String] {
        match self.unit(unit) {
            Some(Unit::Process(process)) => &process.hosts,
            Some(Unit::Container(container)) => &container.hosts,
            None => &[],
        }
    }

    /// Returns each port that a process listens on or a container
    /// publishes, with the unit's name.
    fn listeners(&self) -> Vec<(String, String)> {
        self.processes
            .iter()
            .filter_map(|(name, process)| {
                process
                    .listen
                    .as_ref()
                    .map(|port| (port.clone(), name.clone()))
            })
            .chain(self.containers.iter().flat_map(|(name, container)| {
                container
                    .ports
                    .iter()
                    .map(|published| (published.port.clone(), name.clone()))
            }))
            .collect()
    }

    /// Returns the backends that the manifest's services run on, in order.
    #[must_use]
    pub fn backends(&self) -> BTreeSet<Backend> {
        crate::service::backends(self.services.values())
    }

    /// Returns the ports that the workspace's port ledger holds for the
    /// manifest: its own ports, then the port of each kind of its
    /// services, which [`crate::ServiceKind::port`] names, once each.
    #[must_use]
    pub fn ledger_ports(&self) -> Vec<String> {
        self.ports
            .iter()
            .cloned()
            .chain(
                crate::service::ports(self.services.values())
                    .into_iter()
                    .map(str::to_owned),
            )
            .collect()
    }

    /// Returns the names of the apps that mxv adds for the manifest's
    /// services, in the order they start: each backend, then
    /// [`PROVISION_APP`]. A manifest without services adds none.
    #[must_use]
    pub fn service_apps(&self) -> Vec<String> {
        let mut apps: Vec<String> = self
            .backends()
            .into_iter()
            .map(|backend| backend.app().to_owned())
            .collect();
        if !apps.is_empty() {
            apps.push(PROVISION_APP.to_owned());
        }
        apps
    }

    /// Returns the services that `process` needs, by name, in the order it
    /// lists them, leaving out a name that the manifest does not declare.
    pub fn needed<'a>(
        &'a self,
        process: &'a Process,
    ) -> impl Iterator<Item = (&'a String, &'a Service)> {
        process
            .needs
            .iter()
            .filter_map(|name| self.services.get_key_value(name))
    }

    /// Returns the variables of each service, by service name, that a
    /// process that needs only that service receives, with the backends'
    /// host ports of `ports`. A backend whose port `ports` lacks renders
    /// as `<port>`.
    #[must_use]
    pub fn service_listing(
        &self,
        ports: &BTreeMap<String, u16>,
    ) -> BTreeMap<String, BTreeMap<String, String>> {
        self.services
            .iter()
            .map(|(name, service)| (name.clone(), crate::service::variables([service], ports)))
            .collect()
    }

    /// Returns the variables that `process` receives from the services it
    /// needs, with the backends' host ports of `ports`: the
    /// [`COMMON_VARIABLES`](crate::COMMON_VARIABLES), the endpoint variable
    /// of each kind it needs, and each needed service's own `env`. A
    /// backend whose port `ports` lacks renders as `<port>`. A process that
    /// needs no service receives none.
    #[must_use]
    pub fn service_variables(
        &self,
        process: &Process,
        ports: &BTreeMap<String, u16>,
    ) -> BTreeMap<String, String> {
        if process.needs.is_empty() {
            return BTreeMap::new();
        }
        crate::service::variables(self.needed(process).map(|(_, service)| service), ports)
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

/// Checks the declared ports and returns their names in order.
fn check_ports(errors: &Errors<'_>, raw: &[Spanned<String>]) -> Result<Vec<String>, ManifestError> {
    let mut ports = Vec::new();
    for port in raw {
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
    Ok(ports)
}

/// Checks the `routes` against the declared `ports` and the ports of
/// `joined`, and returns their names in order.
fn check_routes(
    errors: &Errors<'_>,
    raw: &[Spanned<String>],
    ports: &[String],
    joined: &Joined,
) -> Result<Vec<String>, ManifestError> {
    let mut routes: Vec<String> = Vec::new();
    for route in raw {
        let name = route.get_ref();
        if !ports.contains(name) && !joined.ports.contains(name) {
            return Err(errors.invalid(
                &route.span(),
                format!("the route `{name}` is not a name in `ports`"),
            ));
        }
        if routes.contains(name) {
            return Err(errors.invalid(
                &route.span(),
                format!("the route `{name}` is declared twice"),
            ));
        }
        routes.push(name.clone());
    }
    if let Err(message) = check_labels(&routes) {
        let span = raw.first().map_or(0..0, Spanned::span);
        return Err(errors.invalid(&span, message));
    }
    Ok(routes)
}

/// The unit whose `hosts` [`check_hosts`] checks.
struct Hosted<'a> {
    /// `process` or `container`, for the messages.
    kind: &'a str,
    /// The unit's name.
    unit: &'a str,
    /// The ports that the unit listens on or publishes.
    ports: Vec<&'a str>,
    /// The manifest's `routes`.
    routes: &'a [String],
}

/// The most bytes of a host name.
const MOST_HOST_BYTES: usize = 253;

/// What the checks of a manifest's containers share.
struct Shared<'a> {
    /// The ports that the manifest and the manifests it joins declare.
    known: &'a [String],
    /// The names of the manifest's units and of the units it joins.
    names: &'a BTreeSet<&'a str>,
    /// The routes of the manifest.
    routes: &'a [String],
}

/// Returns the checked containers of `raw`, recording each port a container
/// publishes in `listeners` and each host name it lists in `hosted`.
fn check_containers(
    errors: &Errors<'_>,
    raw: &RawManifest,
    shared: &Shared<'_>,
    listeners: &mut BTreeMap<String, String>,
    hosted: &mut BTreeSet<String>,
) -> Result<BTreeMap<String, Container>, ManifestError> {
    let mut containers = BTreeMap::new();
    for (name, raw_container) in &raw.containers {
        let mut container =
            check_container(errors, name, raw_container, shared.known, shared.names)?;
        container.hosts = check_hosts(
            errors,
            &Hosted {
                kind: "container",
                unit: name.get_ref(),
                ports: container
                    .ports
                    .iter()
                    .map(|published| published.port.as_str())
                    .collect(),
                routes: shared.routes,
            },
            &raw_container.hosts,
            hosted,
        )?;
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
    Ok(containers)
}

/// Checks the extra host names `raw` of `unit`: the unit must listen on or
/// publish exactly one port that `routes` names, each name must be a
/// lower-case DNS name that is neither an IP address nor `localhost` nor
/// below `mxv.localhost`, and no name may repeat one that `seen` holds. It
/// returns the names and adds them to `seen`.
fn check_hosts(
    errors: &Errors<'_>,
    unit: &Hosted<'_>,
    raw: &[Spanned<String>],
    seen: &mut BTreeSet<String>,
) -> Result<Vec<String>, ManifestError> {
    let Some(first) = raw.first() else {
        return Ok(Vec::new());
    };
    let Hosted {
        kind,
        unit: name,
        ports,
        routes,
    } = unit;
    let routed: Vec<&&str> = ports
        .iter()
        .filter(|port| routes.iter().any(|route| route == **port))
        .collect();
    if routed.is_empty() {
        return Err(errors.invalid(
            &first.span(),
            format!(
                "the {kind} `{name}` lists `hosts` but listens on no port that `routes` names, so no route can serve them"
            ),
        ));
    }
    if routed.len() > 1 {
        return Err(errors.invalid(
            &first.span(),
            format!(
                "the {kind} `{name}` lists `hosts` but publishes more than one port that `routes` names, so mxv cannot tell which route serves them"
            ),
        ));
    }
    let mut hosts = Vec::new();
    for host in raw {
        let text = host.get_ref();
        if let Some(reason) = host_problem(text, &errors.names.domain) {
            return Err(errors.invalid(
                &host.span(),
                format!("the host `{text}` of the {kind} `{name}` {reason}"),
            ));
        }
        if !seen.insert(text.clone()) {
            return Err(errors.invalid(
                &host.span(),
                format!("the host `{text}` is listed twice in this manifest"),
            ));
        }
        hosts.push(text.clone());
    }
    Ok(hosts)
}

/// Returns why `host` cannot be an extra host name, or `None` when it can.
fn host_problem(host: &str, own_domain: &str) -> Option<String> {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Some("is an IP address, and `hosts` takes DNS names".to_owned());
    }
    if host == "localhost" || host == own_domain || host.ends_with(&format!(".{own_domain}")) {
        return Some(format!(
            "is `localhost` or below `{own_domain}`, which mxv serves itself"
        ));
    }
    let label = |label: &str| {
        !label.is_empty()
            && label.len() <= MOST_LABEL_BYTES
            && label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    };
    if host.len() > MOST_HOST_BYTES || !host.split('.').all(label) {
        return Some(format!(
            "is not a lower-case DNS name, which holds labels of 1 to {MOST_LABEL_BYTES} letters, digits and '-' separated by '.' and at most {MOST_HOST_BYTES} bytes in all"
        ));
    }
    None
}

/// The most bytes of one DNS label, which a route's port name and its
/// workspace name each become in the route's host name.
pub(crate) const MOST_LABEL_BYTES: usize = 63;

/// Returns `name` as one label of a host name: lower case, with every `_`
/// and `.` written as `-`.
pub(crate) fn host_label(name: &str) -> String {
    name.chars()
        .map(|character| match character {
            '_' | '.' => '-',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}

/// Checks that the host labels of the route names `routes` are no longer
/// than [`MOST_LABEL_BYTES`] and that no two are equal, and returns the
/// message of the first route that breaks a rule.
pub(crate) fn check_labels(routes: &[String]) -> Result<(), String> {
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();
    for route in routes {
        let label = host_label(route);
        if label.len() > MOST_LABEL_BYTES {
            return Err(format!(
                "the route `{route}` makes a host label of {} bytes, and a label holds at most {MOST_LABEL_BYTES}",
                label.len()
            ));
        }
        if let Some(other) = seen.insert(label.clone(), route) {
            return Err(format!(
                "the routes `{other}` and `{route}` both make the host label `{label}`"
            ));
        }
    }
    Ok(())
}

/// Checks the task `name` and returns its name and the task. Its `needs`
/// name services of `services` or of `joined`.
fn check_task(
    errors: &Errors<'_>,
    name: &Spanned<String>,
    raw: &RawTask,
    services: &BTreeMap<String, Service>,
    joined: &Joined,
) -> Result<(TaskName, Task), ManifestError> {
    let task = name.get_ref();
    let task_name = TaskName::new(task).ok_or_else(|| {
        errors.invalid(
            &name.span(),
            format!(
                "`{task}` is not a task name, which holds 1 to 64 letters, digits, '-' and '_'"
            ),
        )
    })?;
    let argv = Command::new(raw.argv.get_ref().clone()).ok_or_else(|| {
        errors.invalid(
            &raw.argv.span(),
            format!("the task `{task}` has an empty argv, which needs at least the program"),
        )
    })?;
    let dir = match &raw.dir {
        Some(dir) => {
            let path = PathBuf::from(dir.get_ref());
            if !stays_inside(&path) {
                return Err(errors.invalid(
                    &dir.span(),
                    format!(
                        "the dir `{}` of the task `{task}` is not a relative path inside the repository",
                        dir.get_ref()
                    ),
                ));
            }
            path
        }
        None => PathBuf::from("."),
    };
    let needs = services::check_needs(errors, task, &raw.needs, &[], services, &joined.services)?;
    Ok((
        task_name,
        Task {
            dir,
            argv,
            needs,
            gate: raw.gate.unwrap_or(true),
        },
    ))
}

/// Checks each raw secret, and returns them by name.
fn check_secrets(
    errors: &Errors<'_>,
    raw: &BTreeMap<Spanned<String>, RawSecret>,
) -> Result<BTreeMap<String, Secret>, ManifestError> {
    let mut secrets = BTreeMap::new();
    for (name, raw_secret) in raw {
        secrets.insert(
            name.get_ref().clone(),
            check_secret(errors, name, raw_secret)?,
        );
    }
    Ok(secrets)
}

/// Checks one raw secret, and returns it.
fn check_secret(
    errors: &Errors<'_>,
    name: &Spanned<String>,
    raw: &RawSecret,
) -> Result<Secret, ManifestError> {
    let secret = name.get_ref();
    if !is_unit_name(secret) {
        return Err(errors.invalid(
            &name.span(),
            format!(
                "`{secret}` is not a secret name, which holds 1 to 64 letters, digits, '-' and '_'"
            ),
        ));
    }
    let check = Command::new(raw.check.get_ref().clone()).ok_or_else(|| {
        errors.invalid(
            &raw.check.span(),
            format!("the secret `{secret}` has an empty check, which needs at least the program"),
        )
    })?;
    if raw.sign_in.get_ref().trim().is_empty() {
        return Err(errors.invalid(
            &raw.sign_in.span(),
            format!("the secret `{secret}` has an empty sign_in, which needs the command line to sign in"),
        ));
    }
    // mxv prints the line on a terminal, so a control character such as an
    // escape could rewrite what a person reads.
    if raw.sign_in.get_ref().chars().any(char::is_control) {
        return Err(errors.invalid(
            &raw.sign_in.span(),
            format!("the sign_in of the secret `{secret}` holds a control character"),
        ));
    }
    Ok(Secret {
        check,
        sign_in: raw.sign_in.get_ref().clone(),
    })
}

/// The directory that a manifest's paths are relative to, for the checks
/// of its processes' paths.
struct Root<'a> {
    /// The folders of the workspace's checkouts, for a workspace manifest
    /// of several checkouts, or empty.
    checkouts: &'a [String],
    /// The directory on disk, when the manifest was read against one, or
    /// `None` when it was parsed alone.
    checkout: Option<&'a Path>,
}

/// Checks one raw process against the declared `ports` and process
/// `names`, and its paths against `root`, and returns it.
fn check_process(
    errors: &Errors<'_>,
    name: &Spanned<String>,
    raw: &RawProcess,
    ports: &[String],
    names: &BTreeSet<&str>,
    root: &Root<'_>,
) -> Result<Process, ManifestError> {
    let checkouts = root.checkouts;
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
    let build = raw
        .build
        .as_ref()
        .map(|build| {
            Command::new(build.get_ref().clone()).ok_or_else(|| {
                errors.invalid(
                    &build.span(),
                    format!(
                        "the process `{process}` has an empty build, which needs at least the program"
                    ),
                )
            })
        })
        .transpose()?;
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
    let starmap = raw
        .starmap
        .as_ref()
        .map(|starmap| check_starmap(errors, process, starmap, root))
        .transpose()?;
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
    let permissions = match &raw.permissions {
        Some(permissions) => Some(check_permissions(
            errors,
            process,
            permissions,
            &Granted {
                ports,
                listen: listen.as_ref(),
                receives: &|port| {
                    port_variables.values().any(|received| received == port)
                        || templated.contains(port)
                },
                names,
                depends_on: &depends_on,
                checkouts,
            },
        )?),
        None => None,
    };
    Ok(Process {
        argv: raw.argv.get_ref().clone(),
        cwd,
        env,
        ports: port_variables,
        listen,
        ready,
        depends_on,
        permissions,
        needs: Vec::new(),
        build,
        hosts: Vec::new(),
        starmap,
    })
}

/// Checks the `starmap` value `raw` of the process `process` against the
/// path rules, and when `root` names a directory on disk, checks that the
/// file is a regular file inside it. Returns the normalised path.
fn check_starmap(
    errors: &Errors<'_>,
    process: &str,
    raw: &Spanned<String>,
    root: &Root<'_>,
) -> Result<PathBuf, ManifestError> {
    let names = match starmap_problem(raw.get_ref(), root.checkouts) {
        Ok(names) => names,
        Err(reason) => {
            let rule = if root.checkouts.is_empty() {
                "a path relative to the checkout, such as `cli/starmap.toml`".to_owned()
            } else {
                format!(
                    "a path relative to the workspace's directory inside one of its checkouts, such as `{}/starmap.toml`",
                    root.checkouts.first().map_or("api", String::as_str)
                )
            };
            return Err(errors.invalid(
                &raw.span(),
                format!(
                    "the starmap file {:?} of `{process}` {reason}; a starmap file is {rule}",
                    raw.get_ref()
                ),
            ));
        }
    };
    let file: PathBuf = names.iter().collect();
    if let Some(checkout) = root.checkout {
        // A workspace manifest's file must stay inside the checkout that
        // its first name is.
        let inside = match names.first() {
            Some(folder) if !root.checkouts.is_empty() => checkout.join(folder),
            _ => checkout.to_path_buf(),
        };
        if let Err(found) = starmap_file_found(&checkout.join(&file), &inside) {
            return Err(errors.at(
                Some(&raw.span()),
                ManifestProblem::StarmapFile {
                    unit: process.to_owned(),
                    file,
                    found,
                },
            ));
        }
    }
    Ok(file)
}

/// Returns the names of the `starmap` value `value`, without the empty
/// and `.` names that a leading `./` and repeated or trailing `/` make, or
/// why the value breaks the path rules: it is empty once normalised,
/// starts with `/` or holds a `..` name. A workspace manifest of several
/// checkouts, whose folders are `checkouts`, must name a file inside one
/// of them. jumpgate applies the same rules.
fn starmap_problem<'a>(value: &'a str, checkouts: &[String]) -> Result<Vec<&'a str>, String> {
    if value.starts_with('/') {
        return Err("is absolute".to_owned());
    }
    let names: Vec<&str> = value
        .split('/')
        .filter(|name| !name.is_empty() && *name != ".")
        .collect();
    if names.contains(&"..") {
        return Err("holds `..`".to_owned());
    }
    let Some(first) = names.first() else {
        return Err("is empty".to_owned());
    };
    if !checkouts.is_empty() && !checkouts.iter().any(|folder| folder == first) {
        return Err(format!(
            "lies in none of the workspace's checkouts, {}",
            checkouts
                .iter()
                .map(|folder| format!("`{folder}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(names)
}

/// Checks that `file` is a regular file, is not a symbolic link, and stays
/// inside the directory `inside` once the links of its parent directories
/// are resolved, or returns what is there instead.
pub(crate) fn starmap_file_found(file: &Path, inside: &Path) -> Result<(), StarmapFound> {
    let metadata = match fs::symlink_metadata(file) {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Err(StarmapFound::Missing);
        }
        Err(_) => return Err(StarmapFound::Unreadable),
    };
    if metadata.file_type().is_symlink() {
        return Err(StarmapFound::Link);
    }
    if !metadata.file_type().is_file() {
        return Err(StarmapFound::NotAFile);
    }
    let (Ok(resolved), Ok(root)) = (file.canonicalize(), inside.canonicalize()) else {
        return Err(StarmapFound::Unreadable);
    };
    if resolved.starts_with(&root) {
        Ok(())
    } else {
        Err(StarmapFound::Link)
    }
}

/// What a process's permissions are checked against.
struct Granted<'a> {
    /// The ports that the manifest declares or joins.
    ports: &'a [String],
    /// The process's listen port, or `None`.
    listen: Option<&'a String>,
    /// Reports whether the process receives a port.
    receives: &'a dyn Fn(&String) -> bool,
    /// The processes and containers of the manifest and of the manifests
    /// it joins.
    names: &'a BTreeSet<&'a str>,
    /// The units that the process depends on.
    depends_on: &'a [String],
    /// The folders of the workspace's checkouts, for a workspace manifest
    /// of several checkouts, or empty.
    checkouts: &'a [String],
}

/// Checks the permissions `raw` of the process `process` against what
/// `granted` says of it, and returns them: each write path is `$STATE` or
/// a relative path inside the checkout, each listen port is a port that
/// the process listens on or receives, its own listen port is among them,
/// and each connection names a unit that the process depends on.
fn check_permissions(
    errors: &Errors<'_>,
    process: &str,
    raw: &Spanned<RawPermissions>,
    granted: &Granted<'_>,
) -> Result<Permissions, ManifestError> {
    let mut permissions = Permissions::default();
    for path in &raw.get_ref().write {
        if let Some(reason) = write_problem(path.get_ref(), granted.checkouts) {
            let rule = if granted.checkouts.is_empty() {
                "a path relative to the checkout, such as `target`".to_owned()
            } else {
                format!(
                    "a path relative to the workspace's directory inside one of its checkouts, such as `{}/target`",
                    granted.checkouts.first().map_or("api", String::as_str)
                )
            };
            return Err(errors.invalid(
                &path.span(),
                format!(
                    "the write path {:?} of `{process}` {reason}; a write path is `$STATE` or {rule}",
                    path.get_ref()
                ),
            ));
        }
        permissions.write.insert(path.get_ref().clone());
    }
    for port in &raw.get_ref().listen {
        let name = port.get_ref();
        if !granted.ports.contains(name) {
            return Err(errors.invalid(
                &port.span(),
                format!(
                    "the permissions of `{process}` let it listen on `{name}`, which `ports` does not declare"
                ),
            ));
        }
        if granted.listen != Some(name) && !(granted.receives)(name) {
            return Err(errors.invalid(
                &port.span(),
                format!(
                    "the permissions of `{process}` let it listen on `{name}`, which it neither listens on nor receives; add a variable for it to `ports`"
                ),
            ));
        }
        permissions.listen.insert(name.clone());
    }
    if let Some(listen) = granted.listen
        && !permissions.listen.contains(listen)
    {
        return Err(errors.invalid(
            &raw.span(),
            format!(
                "the process `{process}` listens on `{listen}`, which its permissions do not let it listen on; add `listen = [\"{listen}\"]` to its permissions"
            ),
        ));
    }
    for unit in &raw.get_ref().connect {
        let name = unit.get_ref();
        if !granted.names.contains(name.as_str()) {
            return Err(errors.invalid(
                &unit.span(),
                format!(
                    "the permissions of `{process}` let it connect to `{name}`, which the manifest does not declare as a unit; a sandboxed process cannot reach an external host until phase's allowlist proxy is built"
                ),
            ));
        }
        if !granted.depends_on.contains(name) {
            return Err(errors.invalid(
                &unit.span(),
                format!(
                    "the permissions of `{process}` let it connect to `{name}`, which it does not depend on; add `{name}` to its `depends_on`, because a sandboxed process may connect only to units it depends on"
                ),
            ));
        }
        permissions.connect.insert(name.clone());
    }
    Ok(permissions)
}

/// Checks that no process's permissions let it listen on a port that
/// another unit listens on or publishes, as `listeners` and `joined` name
/// them by port, so a sandboxed process cannot take a dependency's port.
fn check_listen_grants(
    errors: &Errors<'_>,
    raw: &RawManifest,
    listeners: &BTreeMap<String, String>,
    joined: &Joined,
) -> Result<(), ManifestError> {
    for (name, process) in &raw.processes {
        let Some(permissions) = &process.permissions else {
            continue;
        };
        for port in &permissions.get_ref().listen {
            let other = listeners.get(port.get_ref()).or_else(|| {
                joined
                    .listened
                    .iter()
                    .find(|(listened, _)| listened == port.get_ref())
                    .map(|(_, unit)| unit)
            });
            if let Some(other) = other
                && other != name.get_ref()
            {
                return Err(errors.invalid(
                    &port.span(),
                    format!(
                        "the permissions of `{}` let it listen on `{}`, which `{other}` listens on or publishes; a process may listen only on its own ports",
                        name.get_ref(),
                        port.get_ref()
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Returns why `path` cannot be a write path of a process's permissions,
/// or `None` when it can: it is `$STATE`, or a relative path that names a
/// directory inside the checkout without `..`, outside its `.git`, and
/// holds no quote, backslash or control character, which phase's sandbox
/// profile cannot hold. When `checkouts` names the folders of a
/// workspace's checkouts, the path is relative to the workspace's
/// directory, and it must lie inside one of them without naming the whole
/// checkout.
fn write_problem(path: &str, checkouts: &[String]) -> Option<String> {
    if path == STATE {
        return None;
    }
    if path.is_empty() {
        return Some("is empty".to_owned());
    }
    if path.starts_with('$') {
        return Some("starts with `$`, which only `$STATE` may".to_owned());
    }
    if path.contains('\\') {
        return Some("holds a backslash".to_owned());
    }
    if path
        .chars()
        .any(|character| character == '"' || character.is_control())
    {
        return Some("holds a quote or a control character".to_owned());
    }
    let path = Path::new(path);
    if path.is_absolute() {
        return Some("is absolute".to_owned());
    }
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => names.push(name.to_string_lossy()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Some("holds `..`".to_owned());
            }
        }
    }
    let (in_checkout, checkout) = match names.split_first() {
        Some((first, rest)) if !checkouts.is_empty() => {
            if !checkouts.iter().any(|folder| folder == first.as_ref()) {
                return Some(format!(
                    "lies in none of the workspace's checkouts, {}",
                    checkouts
                        .iter()
                        .map(|folder| format!("`{folder}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            (rest, Some(first))
        }
        _ => (names.as_slice(), None),
    };
    let Some(top) = in_checkout.first() else {
        return Some(match checkout {
            Some(folder) => format!(
                "names the whole checkout `{folder}`, whose `.git` a sandboxed process may only read"
            ),
            None => "names the whole checkout, whose `.git` a sandboxed process may only read"
                .to_owned(),
        });
    };
    if top.eq_ignore_ascii_case(".git") {
        return Some(
            "lies in the checkout's `.git`, which a sandboxed process may only read".to_owned(),
        );
    }
    None
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
                    let pattern = LogPattern::new(log.get_ref()).map_err(|bad| {
                        let message = match &bad {
                            BadLogPattern::Empty => {
                                format!("the `log` pattern of `{process}` is empty")
                            }
                            BadLogPattern::Invalid(problem) => format!(
                                "the `log` pattern of `{process}` is not a regular expression: {problem}"
                            ),
                        };
                        errors.invalid(&log.span(), message)
                    })?;
                    ReadyCheck::Log(pattern)
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

/// Checks that `variable` is a valid environment variable name.
fn check_variable(
    errors: &Errors<'_>,
    process: &str,
    variable: &Spanned<String>,
) -> Result<(), ManifestError> {
    if phase::is_variable_name(variable.get_ref()) {
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

/// Returns the `owner` key of the container `container`, which phase must
/// accept and which needs a volume to apply to.
fn container_owner(
    errors: &Errors<'_>,
    container: &str,
    owner: Option<&Spanned<String>>,
    volumes: &BTreeMap<String, String>,
) -> Result<Option<String>, ManifestError> {
    let Some(owner) = owner else {
        return Ok(None);
    };
    if volumes.is_empty() {
        return Err(errors.invalid(
            &owner.span(),
            format!(
                "the container `{container}` sets `owner` and mounts no volume, and `owner` sets only the owner of a container's volumes"
            ),
        ));
    }
    if !phase::is_volume_owner(owner.get_ref()) {
        return Err(errors.invalid(
            &owner.span(),
            format!(
                "the owner `{}` of `{container}` is not `keep` or a numeric uid or uid:gid such as 999:999, whose ids are below 4294967295",
                owner.get_ref()
            ),
        ));
    }
    Ok(Some(owner.get_ref().clone()))
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
    let owner = container_owner(errors, container, raw.owner.as_ref(), &volumes)?;
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
    if let Some(permissions) = &raw.permissions {
        return Err(errors.invalid(
            &permissions.span(),
            format!(
                "the container `{container}` has permissions, which only a host process takes, because phase's sandbox holds host processes and Docker runs a container"
            ),
        ));
    }
    let depends_on = check_dependencies(errors, container, &raw.depends_on, names)?;
    Ok(Container {
        image: image.clone(),
        command: raw.command.clone(),
        env,
        ports: published,
        volumes,
        owner,
        ready,
        depends_on,
        hosts: Vec::new(),
    })
}
