//! The proxy that serves each workspace's HTTP ports at stable names.
//!
//! A manifest's `routes` name ports. The proxy is a Caddy process that
//! phase supervises, and it forwards `<port>.<workspace>.mxv.localhost` to
//! the workspace's host port. [`Proxy::sync`] rebuilds the whole
//! configuration from the workspace records on every call and remembers
//! nothing, so a record that changes while no sync runs is picked up by the
//! next one.
//!
//! The proxy keeps its files in `<state>/proxy`: the admin socket
//! `admin.sock`, the lock `lock`, the file `bootstrap.json` that Caddy
//! starts from, and phase's own state in `phase`. Caddy's data, including
//! its certificate authority, is in `<data>/proxy/caddy`.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::{Duration, Instant, SystemTime};

use phase::{
    AppName, AppOutcome, AppPlan, AppliedEnvironment, Cancel, EnvironmentError, EnvironmentSource,
    Handle, Phase, Plan, Probe, ProbeCheck, ProcessName, ProcessPlan, Resolve, Restart,
    RestartLimits, RunnerKind, Selection, Source, UpDeadlines, WorkspaceId,
    WorkspaceName as PhaseWorkspace,
};

use crate::clock::{POLL_INTERVAL, pause};
use crate::error::Error;
use crate::name::WorkspaceName;
use crate::phase_plan::CallerEnvironment;
use crate::ports;
use crate::store::{self, LOCK_WAIT, Store};

mod admin;
mod caddy_config;
mod process;
mod routes;
#[cfg(test)]
mod tests;

use routes::declared;
pub use routes::{DOMAIN, Declared, Derived, Route, Shadowed, derive};

/// The name of the proxy's one workspace in phase, and its id.
const WORKSPACE: &str = "mxv-proxy";

/// How long a start waits for Caddy to answer on its admin socket.
const START_WAIT: Duration = Duration::from_secs(10);

/// How long a stop waits for Caddy to end.
const STOP_WAIT: Duration = Duration::from_secs(15);

/// The longest path of a Unix socket on macOS, with the final NUL, is 104
/// bytes.
const MOST_SOCKET_BYTES: usize = 103;

/// Where the proxy keeps its files and what it listens on.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Settings {
    /// mxv's state directory, whose `proxy` directory holds the proxy's
    /// files.
    state: PathBuf,
    /// mxv's data directory, whose `proxy/caddy` directory holds Caddy's
    /// data.
    data: PathBuf,
    /// The address that serves HTTP.
    http: SocketAddr,
    /// The address that serves HTTPS.
    https: SocketAddr,
    /// The Caddy program, or `None` to search `search` for it.
    caddy: Option<PathBuf>,
    /// The directories that hold programs, in the order of `PATH`.
    search: Vec<PathBuf>,
}

impl Settings {
    /// Returns settings for the state directory `state` and the data
    /// directory `data`, with HTTP on 127.0.0.1:80, HTTPS on 127.0.0.1:443
    /// and no named Caddy program.
    #[must_use]
    pub fn new(state: PathBuf, data: PathBuf) -> Self {
        Self {
            state,
            data,
            http: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 80)),
            https: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 443)),
            caddy: None,
            search: Vec::new(),
        }
    }

    /// Returns the settings with `http` and `https` as the listen
    /// addresses.
    #[must_use]
    pub const fn with_addresses(mut self, http: SocketAddr, https: SocketAddr) -> Self {
        self.http = http;
        self.https = https;
        self
    }

    /// Returns the settings with `caddy` as the Caddy program, or with the
    /// search of `PATH` when it is `None`.
    #[must_use]
    pub fn with_caddy(mut self, caddy: Option<PathBuf>) -> Self {
        self.caddy = caddy;
        self
    }

    /// Returns the settings with `dirs`, the directories of the caller's
    /// `PATH`, as where to search for `caddy`.
    #[must_use]
    pub fn with_search(mut self, dirs: Vec<PathBuf>) -> Self {
        self.search = dirs;
        self
    }

    /// Returns the address that serves HTTP.
    #[must_use]
    pub const fn http(&self) -> SocketAddr {
        self.http
    }

    /// Returns the address that serves HTTPS.
    #[must_use]
    pub const fn https(&self) -> SocketAddr {
        self.https
    }

    /// Returns the directory that holds the proxy's files.
    #[must_use]
    pub fn dir(&self) -> PathBuf {
        self.state.join("proxy")
    }

    /// Returns the path of Caddy's admin socket.
    #[must_use]
    pub fn socket(&self) -> PathBuf {
        self.dir().join("admin.sock")
    }

    /// Returns the directory that holds Caddy's data, including its
    /// certificate authority.
    #[must_use]
    pub fn caddy_data(&self) -> PathBuf {
        self.data.join("proxy").join("caddy")
    }

    /// Returns the path of the local authority's root certificate.
    #[must_use]
    pub fn root_certificate(&self) -> PathBuf {
        self.caddy_data()
            .join("pki")
            .join("authorities")
            .join("local")
            .join("root.crt")
    }

    /// Returns the path of the file that Caddy starts from.
    fn bootstrap(&self) -> PathBuf {
        self.dir().join("bootstrap.json")
    }
}

/// Why the proxy could not serve the routes.
#[derive(Debug)]
#[non_exhaustive]
pub enum ProxyError {
    /// Something else holds a listen address.
    PortHeld {
        /// The address.
        address: SocketAddr,
        /// The config key that sets it.
        key: &'static str,
    },
    /// The system does not let this person listen on the address.
    PortDenied {
        /// The address.
        address: SocketAddr,
        /// The config key that sets it.
        key: &'static str,
    },
    /// No Caddy program runs.
    CaddyMissing {
        /// The path that the config names, or `None` when mxv searched
        /// `PATH`.
        named: Option<PathBuf>,
        /// What the search or the program said.
        reason: String,
    },
    /// The Caddy program is older than mxv drives.
    CaddyTooOld {
        /// The program.
        path: PathBuf,
        /// Its version.
        found: String,
    },
    /// Caddy did not start.
    StartFailed {
        /// Why, in a sentence.
        message: String,
    },
    /// Caddy refused the configuration.
    LoadFailed {
        /// Why, in a sentence.
        message: String,
    },
    /// The admin socket's path is longer than a socket address can hold.
    SocketPathTooLong {
        /// The path.
        path: PathBuf,
        /// Its length in bytes.
        bytes: usize,
    },
    /// A file or lock of the proxy failed.
    Store(Error),
}

impl From<Error> for ProxyError {
    fn from(error: Error) -> Self {
        Self::Store(error)
    }
}

impl fmt::Display for ProxyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PortHeld { address, key } => write!(
                f,
                "{address} is held by another process, so the proxy cannot listen on it; stop that process, or set {key} in the config file to another loopback address"
            ),
            Self::PortDenied { address, key } => write!(
                f,
                "the system does not let this person listen on {address}; set {key} in the config file to a port from 1024 up, or allow the port"
            ),
            Self::CaddyMissing {
                named: Some(path),
                reason,
            } => write!(
                f,
                "the caddy program {} does not run ({reason}); install Caddy, for example `brew install caddy`, or fix `caddy` in the config file",
                path.display()
            ),
            Self::CaddyMissing {
                named: None,
                reason,
            } => write!(
                f,
                "mxv found no working `caddy` program on PATH ({reason}); install Caddy, for example `brew install caddy`, or set `caddy` in the config file to its path"
            ),
            Self::CaddyTooOld { path, found } => write!(
                f,
                "{} is Caddy {found}, and mxv needs Caddy 2.8 or later",
                path.display()
            ),
            Self::StartFailed { message } => write!(f, "Caddy did not start: {message}"),
            Self::LoadFailed { message } => write!(f, "Caddy refused the configuration: {message}"),
            Self::SocketPathTooLong { path, bytes } => write!(
                f,
                "the admin socket path {} is {bytes} bytes, and a socket path holds at most {MOST_SOCKET_BYTES}; use a shorter XDG_STATE_HOME",
                path.display()
            ),
            Self::Store(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ProxyError {}

/// What a sync does when Caddy does not run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    /// Start Caddy when any workspace has a route. `up`, `restart`,
    /// `restore` and `mxv proxy start` use it.
    Start,
    /// Leave Caddy stopped. `down`, `archive`, `remove` and `ports reset`
    /// use it, because they only take routes away or change them.
    IfRunning,
}

/// What a sync left served.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Synced {
    /// Every route the running proxy serves, or none when it does not run.
    pub served: Vec<Route>,
    /// The HTTPS address, which the URLs of the routes carry.
    pub https: Option<SocketAddr>,
}

impl Synced {
    /// Returns the URLs of the routes of `workspace`, in host order.
    #[must_use]
    pub fn urls_of(&self, workspace: &WorkspaceName) -> Vec<String> {
        let Some(https) = self.https else {
            return Vec::new();
        };
        self.served
            .iter()
            .filter(|route| &route.workspace == workspace)
            .map(|route| format!("{} at {}", route.name, route.url(https)))
            .collect()
    }
}

/// The local certificate authority that Caddy made.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Authority {
    /// Whether this machine trusts the root, or `None` when mxv cannot
    /// tell on this system.
    pub trusted: Option<bool>,
}

/// The state of the proxy.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Status {
    /// Whether Caddy answers on the admin socket.
    pub running: bool,
    /// Caddy's version, when mxv could run `caddy version`.
    pub version: Option<String>,
    /// Caddy's process id, when phase has it.
    pub pid: Option<u32>,
    /// The address that serves HTTP.
    pub http: SocketAddr,
    /// The address that serves HTTPS.
    pub https: SocketAddr,
    /// The authority, or `None` when Caddy has not made one.
    pub authority: Option<Authority>,
    /// The routes, served and shadowed, and the problems.
    pub routes: Derived,
}

/// The proxy: Caddy, supervised by phase, and the files around it.
#[derive(Debug)]
pub struct Proxy {
    /// Where the files are and what the proxy listens on.
    settings: Settings,
    /// The phase that supervises Caddy.
    phase: Phase,
}

impl Proxy {
    /// Returns the proxy for `settings`.
    #[must_use]
    pub fn new(settings: Settings) -> Self {
        let phase = Phase::new(settings.dir().join("phase"));
        Self { settings, phase }
    }

    /// Returns the proxy with `program`, followed by `leading`, as the
    /// program that runs phase's keeper, as [`crate::Runs::with_keeper`]
    /// names it.
    #[must_use]
    pub fn with_keeper(mut self, program: PathBuf, leading: Vec<std::ffi::OsString>) -> Self {
        self.phase = self.phase.with_keeper(program, leading);
        self
    }

    /// Returns the proxy with `caller` as the variables that Caddy's keeper
    /// receives.
    #[must_use]
    pub fn with_caller(mut self, caller: &CallerEnvironment) -> Self {
        self.phase = self.phase.with_inherited(caller.inherited());
        self
    }

    /// Returns the settings.
    #[must_use]
    pub const fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Brings the proxy in line with the workspace records. `caller_routes`
    /// says whether the workspace that asked declares a route. A caller
    /// without routes finds nothing to do while Caddy has never run.
    ///
    /// # Errors
    ///
    /// Returns the [`ProxyError`] that stopped Caddy from starting or from
    /// loading the configuration.
    pub fn sync(
        &self,
        store: &Store,
        mode: SyncMode,
        caller_routes: bool,
    ) -> Result<Synced, ProxyError> {
        let socket = self.settings.socket();
        if !caller_routes && !socket.exists() {
            return Ok(Synced::default());
        }
        store::private_dir(&self.settings.dir())?;
        let _lock = store::lock(&self.settings.dir().join("lock"), LOCK_WAIT)?;
        let (found, _) = declared(store)?;
        let derived = derive(found);
        let mut current = self.current();
        if derived.served.is_empty() {
            if current.is_some() {
                self.stop_locked()?;
            }
            return Ok(Synced::default());
        }
        if current.is_none() {
            if mode == SyncMode::IfRunning {
                return Ok(Synced::default());
            }
            self.start_locked(&derived.served)?;
            current = self.current();
        }
        let config = caddy_config::render(&self.settings, &derived.served);
        if current.as_ref() != Some(&config) {
            self.load(&config)?;
        }
        Ok(Synced {
            served: derived.served,
            https: Some(self.settings.https),
        })
    }

    /// Stops Caddy when it runs.
    ///
    /// # Errors
    ///
    /// Returns the error of the lock or of phase.
    pub fn stop(&self) -> Result<bool, ProxyError> {
        store::private_dir(&self.settings.dir())?;
        let _lock = store::lock(&self.settings.dir().join("lock"), LOCK_WAIT)?;
        let was = self.answering() || self.pid().is_some();
        self.stop_locked()?;
        Ok(was)
    }

    /// Returns the state of the proxy and the routes of `store`.
    ///
    /// # Errors
    ///
    /// Returns the error of reading the store.
    pub fn status(&self, store: &Store) -> Result<Status, ProxyError> {
        let (found, problems) = declared(store)?;
        let mut routes = derive(found);
        routes.problems.extend(problems);
        let running = self.answering();
        let version = self
            .program()
            .ok()
            .and_then(|(path, _)| match process::version(&path) {
                process::Version::Found(version) => Some(version),
                process::Version::Unusable(_) => None,
            });
        let certificate = self.settings.root_certificate();
        let authority = certificate.exists().then(|| Authority {
            trusted: process::trusted(&certificate),
        });
        Ok(Status {
            running,
            version,
            pid: self.pid(),
            http: self.settings.http,
            https: self.settings.https,
            authority,
            routes,
        })
    }

    /// Returns the name of the local authority's root certificate, which
    /// Caddy reports once it runs, or `None` when Caddy does not run.
    #[must_use]
    pub fn authority_name(&self) -> Option<String> {
        let response = admin::get(&self.settings.socket(), "/pki/ca/local").ok()?;
        let value: serde_json::Value = serde_json::from_slice(&response.body).ok()?;
        value.get("root_common_name")?.as_str().map(str::to_owned)
    }

    /// Runs `caddy trust` against the running proxy, with the terminal of
    /// the caller.
    ///
    /// # Errors
    ///
    /// Returns a [`ProxyError`] when Caddy is missing or does not run, and
    /// the I/O error of starting `caddy trust` as [`ProxyError::StartFailed`].
    pub fn trust(&self) -> Result<ExitStatus, ProxyError> {
        let (caddy, _) = self.program()?;
        if !self.answering() {
            return Err(ProxyError::StartFailed {
                message: "the proxy does not run, so it has no authority to trust; `mxv proxy start` starts it".to_owned(),
            });
        }
        process::trust(&caddy, &self.settings.socket()).map_err(|error| ProxyError::StartFailed {
            message: error.to_string(),
        })
    }

    /// Reports whether Caddy answers on the admin socket.
    fn answering(&self) -> bool {
        admin::get(&self.settings.socket(), "/config/").is_ok_and(|response| response.status == 200)
    }

    /// Returns the configuration that Caddy runs, or `None` when it does
    /// not answer. This is the only request that a sync makes to find out.
    fn current(&self) -> Option<serde_json::Value> {
        let response = admin::get(&self.settings.socket(), "/config/").ok()?;
        (response.status == 200)
            .then(|| serde_json::from_slice(&response.body).unwrap_or(serde_json::Value::Null))
    }

    /// Loads `config` and keeps it as the file Caddy starts from.
    fn load(&self, config: &serde_json::Value) -> Result<(), ProxyError> {
        let bytes = serde_json::to_vec_pretty(config).map_err(|error| ProxyError::LoadFailed {
            message: error.to_string(),
        })?;
        let response = admin::post(&self.settings.socket(), "/load", &bytes).map_err(|error| {
            ProxyError::LoadFailed {
                message: error.to_string(),
            }
        })?;
        if response.status != 200 {
            return Err(ProxyError::LoadFailed {
                message: String::from_utf8_lossy(&response.body).trim().to_owned(),
            });
        }
        store::replace_file(&self.settings.bootstrap(), &bytes)?;
        Ok(())
    }

    /// Returns the Caddy program and its version, or why there is none.
    fn program(&self) -> Result<(PathBuf, String), ProxyError> {
        let path = match &self.settings.caddy {
            Some(path) => path.clone(),
            None => {
                process::find(&self.settings.search).ok_or_else(|| ProxyError::CaddyMissing {
                    named: None,
                    reason: "no executable file is named caddy".to_owned(),
                })?
            }
        };
        match process::version(&path) {
            process::Version::Unusable(reason) => Err(ProxyError::CaddyMissing {
                named: self.settings.caddy.clone(),
                reason,
            }),
            process::Version::Found(found) => {
                let old = process::major_minor(&found).is_some_and(|have| have < process::OLDEST);
                if old {
                    Err(ProxyError::CaddyTooOld { path, found })
                } else {
                    Ok((path, found))
                }
            }
        }
    }

    /// Starts Caddy from a file that holds the configuration of `routes`,
    /// and waits for its admin socket.
    fn start_locked(&self, routes: &[Route]) -> Result<(), ProxyError> {
        let socket = self.settings.socket();
        let bytes = socket.as_os_str().len();
        if bytes > MOST_SOCKET_BYTES {
            return Err(ProxyError::SocketPathTooLong {
                path: socket,
                bytes,
            });
        }
        for (address, key) in [
            (self.settings.http, "proxy_http"),
            (self.settings.https, "proxy_https"),
        ] {
            ports::can_bind(address).map_err(|error| match error.kind() {
                io::ErrorKind::AddrInUse => ProxyError::PortHeld { address, key },
                io::ErrorKind::PermissionDenied => ProxyError::PortDenied { address, key },
                _ => ProxyError::StartFailed {
                    message: format!("{address}: {error}"),
                },
            })?;
        }
        let (caddy, _) = self.program()?;
        let config = caddy_config::render(&self.settings, routes);
        let text = serde_json::to_vec_pretty(&config).map_err(|error| ProxyError::StartFailed {
            message: error.to_string(),
        })?;
        store::replace_file(&self.settings.bootstrap(), &text)?;
        store::private_dir(&self.settings.caddy_data())?;
        match fs::remove_file(&socket) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Error::Unwritable {
                    path: socket,
                    source,
                }
                .into());
            }
        }
        let start_failed = |message: String| ProxyError::StartFailed { message };
        let id = WorkspaceId::try_from(WORKSPACE.to_owned())
            .map_err(|error| start_failed(error.to_string()))?;
        let workspace = PhaseWorkspace::try_from(WORKSPACE.to_owned())
            .map_err(|error| start_failed(error.to_string()))?;
        let app = AppName::try_from("caddy".to_owned())
            .map_err(|error| start_failed(error.to_string()))?;
        let main = ProcessName::try_from(crate::phase_plan::MAIN.to_owned())
            .map_err(|error| start_failed(error.to_string()))?;
        let mut process_plan = ProcessPlan::new(
            main,
            vec![
                caddy.display().to_string(),
                "run".to_owned(),
                "--config".to_owned(),
                self.settings.bootstrap().display().to_string(),
            ],
        );
        let mut limits = RestartLimits::default();
        limits.resolve = Resolve::Never;
        process_plan.restart = Restart::on_failure(limits);
        process_plan.ready = Some(Probe::new(ProbeCheck::log(
            "serving initial configuration".to_owned(),
        )));
        process_plan.ready_timeout = START_WAIT;
        let mut app_plan = AppPlan::new(
            app,
            RunnerKind::Host,
            Source::checkout(self.settings.dir(), None),
        );
        app_plan.processes = vec![process_plan];
        let plan = Plan::new(workspace, id, vec![app_plan])
            .map_err(|error| start_failed(error.to_string()))?;
        let now = Instant::now();
        let report = self
            .phase
            .up(
                &plan,
                &Selection::All,
                &NoEnvironment,
                UpDeadlines::new(now, now + START_WAIT + START_WAIT),
                &Cancel::new(),
            )
            .map_err(|error| start_failed(error.to_string()))?;
        for app in &report.apps {
            match &app.outcome {
                AppOutcome::Failed { problem, .. } => {
                    return Err(start_failed(problem.to_string()));
                }
                AppOutcome::Errored { message, .. } => return Err(start_failed(message.clone())),
                AppOutcome::StartUncertain => {
                    return Err(start_failed(
                        "phase does not know whether Caddy started".to_owned(),
                    ));
                }
                _ => {}
            }
        }
        let deadline = Instant::now() + START_WAIT;
        while UnixStream::connect(&socket).is_err() {
            if Instant::now() >= deadline {
                return Err(start_failed(format!(
                    "the admin socket {} did not appear within {} seconds",
                    socket.display(),
                    START_WAIT.as_secs()
                )));
            }
            pause(POLL_INTERVAL);
        }
        Ok(())
    }

    /// Stops Caddy through phase and removes its socket.
    fn stop_locked(&self) -> Result<(), ProxyError> {
        let id = WorkspaceId::try_from(WORKSPACE.to_owned()).map_err(|error| {
            ProxyError::StartFailed {
                message: error.to_string(),
            }
        })?;
        self.phase
            .down(&id, &Selection::All, Instant::now() + STOP_WAIT)
            .map_err(|error| ProxyError::StartFailed {
                message: error.to_string(),
            })?;
        let _ = fs::remove_file(self.settings.socket());
        Ok(())
    }

    /// Returns Caddy's process id, when phase has it.
    fn pid(&self) -> Option<u32> {
        let id = WorkspaceId::try_from(WORKSPACE.to_owned()).ok()?;
        let status = self.phase.status(&id, None).ok()?;
        status.rows.iter().find_map(|row| match row.handle {
            Some(Handle::Pid { pid, .. }) => Some(pid),
            _ => None,
        })
    }
}

/// The environment of Caddy's process, which holds no variable: phase gives
/// a host process `PATH` and `HOME`.
struct NoEnvironment;

impl EnvironmentSource for NoEnvironment {
    fn resolve(
        &self,
        _app: &AppName,
        _process: &ProcessName,
        _deadline: Instant,
    ) -> Result<Box<dyn AppliedEnvironment>, EnvironmentError> {
        Ok(Box::new(NoValues))
    }
}

/// An environment without a variable.
struct NoValues;

impl AppliedEnvironment for NoValues {
    fn names(&self) -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn expires_at(&self) -> Option<SystemTime> {
        None
    }

    fn digest(&self, _key: &[u8; 32]) -> [u8; 32] {
        [0; 32]
    }

    fn apply_as(
        &self,
        _command: &mut std::process::Command,
        _reference: &dyn Fn(&str) -> Option<String>,
    ) {
    }

    fn write_value(&self, _name: &str, _out: &mut dyn io::Write) -> io::Result<()> {
        Err(io::ErrorKind::NotFound.into())
    }
}
