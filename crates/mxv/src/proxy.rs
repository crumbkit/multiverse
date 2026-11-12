//! `mxv proxy`, the sync that the lifecycle verbs run, and the failures of
//! the proxy.
//!
//! The proxy is Caddy, which serves each route that a workspace's manifest
//! declares at a stable name below `mxv.localhost`. The library keeps the
//! routes in line with the workspace records; this module prints what the
//! library did and turns its errors into failures and warnings.

use std::collections::BTreeMap;
use std::fmt;
use std::process::ExitCode;

use crumbkit::{
    Call, Children, Code, Document, Failure, Field, Listing, Prose, Recovery, Row, Run, Spelling,
    Yes,
};
use multiverse::{
    Error, ProcessStatus, Proxy, ProxyError, ProxyStatus, Route, Store, SyncMode, Workspace,
    WorkspaceName,
};
use schemars::JsonSchema;
use serde::Serialize;

use crate::cli::ProxyCommand;
use crate::environment::Environment;
use crate::failures::MxvFailure;
use crate::rows::ProcessRow;

#[cfg(test)]
mod tests;

/// The code of a proxy that cannot listen, because another process holds
/// the address.
pub const PROXY_PORT_HELD: Code = Code::new("mxv", "proxy", "port_held");
/// The code of a proxy that the system does not let the person listen
/// with on the address.
pub const PROXY_PORT_DENIED: Code = Code::new("mxv", "proxy", "port_denied");
/// The code of a machine without a working `caddy` program.
pub const PROXY_CADDY_MISSING: Code = Code::new("mxv", "proxy", "caddy_missing");
/// The code of a `caddy` program older than mxv drives.
pub const PROXY_CADDY_TOO_OLD: Code = Code::new("mxv", "proxy", "caddy_too_old");
/// The code of a Caddy that did not start.
pub const PROXY_START_FAILED: Code = Code::new("mxv", "proxy", "start_failed");
/// The code of a Caddy that refused the configuration.
pub const PROXY_LOAD_FAILED: Code = Code::new("mxv", "proxy", "load_failed");
/// The code of a state directory whose admin socket path is too long.
pub const PROXY_SOCKET_PATH_TOO_LONG: Code = Code::new("mxv", "proxy", "socket_path_too_long");
/// The code of `mxv proxy trust` run without a terminal.
pub const PROXY_TRUST_NEEDS_TERMINAL: Code = Code::new("mxv", "proxy", "trust_needs_terminal");
/// The code of a `caddy trust` that ended with a failure.
pub const PROXY_TRUST_FAILED: Code = Code::new("mxv", "proxy", "trust_failed");

/// Every code that a [`ProxyFailure`] returns.
pub(crate) const CODES: &[&Code] = &[
    &PROXY_PORT_HELD,
    &PROXY_PORT_DENIED,
    &PROXY_CADDY_MISSING,
    &PROXY_CADDY_TOO_OLD,
    &PROXY_START_FAILED,
    &PROXY_LOAD_FAILED,
    &PROXY_SOCKET_PATH_TOO_LONG,
    &PROXY_TRUST_NEEDS_TERMINAL,
    &PROXY_TRUST_FAILED,
];

/// The document that `mxv proxy status` prints.
pub(crate) const STATUS: Document<Listing<RouteRow, ProxySummary>> = Document::new(
    "mxv",
    "proxy-status",
    1,
    "Whether the proxy runs, its addresses and certificate authority, and each route that a workspace declares with the unit behind it.",
)
.when_empty("no workspace declares a route");

/// The document that `mxv proxy start` prints.
pub(crate) const START: Document<Listing<RouteRow, ProxySummary>> = Document::new(
    "mxv",
    "proxy-start",
    1,
    "The state of the proxy once it serves the routes of every workspace, and each route.",
)
.when_empty("no workspace declares a route");

/// The document that `mxv proxy stop` prints.
pub(crate) const STOP: Document<Listing<RouteRow, ProxySummary>> = Document::new(
    "mxv",
    "proxy-stop",
    1,
    "The state of the proxy once it has stopped, and each route that a workspace declares.",
)
.when_empty("no workspace declares a route");

/// The document that `mxv proxy trust` prints.
pub(crate) const TRUST: Document<Prose<TrustResult>> = Document::new(
    "mxv",
    "proxy-trust",
    1,
    "The name of the certificate authority that this machine now trusts.",
);

/// Why a proxy command could not do what it was asked.
#[derive(Debug)]
pub(crate) enum ProxyFailure {
    /// The library could not start Caddy or load the routes.
    Library(ProxyError),
    /// `mxv proxy trust` ran without a terminal.
    TrustNeedsTerminal,
    /// `caddy trust` ended with a failure.
    TrustFailed {
        /// How `caddy trust` ended, in a phrase.
        ended: String,
    },
}

/// Turns the error of the library into the failure that prints, or into
/// the failure of the record store when a file failed.
pub(crate) fn fail(error: ProxyError) -> crumbkit::Error {
    match error {
        ProxyError::Store(inner) => MxvFailure::from(inner).into(),
        other => ProxyFailure::Library(other).into(),
    }
}

/// Returns the code of `error`, which the warning of a lifecycle verb and
/// the failure of a proxy command both print.
fn code_of(error: &ProxyError) -> &'static Code {
    match error {
        ProxyError::PortHeld { .. } => &PROXY_PORT_HELD,
        ProxyError::PortDenied { .. } => &PROXY_PORT_DENIED,
        ProxyError::CaddyMissing { .. } => &PROXY_CADDY_MISSING,
        ProxyError::CaddyTooOld { .. } => &PROXY_CADDY_TOO_OLD,
        ProxyError::LoadFailed { .. } => &PROXY_LOAD_FAILED,
        ProxyError::SocketPathTooLong { .. } => &PROXY_SOCKET_PATH_TOO_LONG,
        _ => &PROXY_START_FAILED,
    }
}

impl Failure for ProxyFailure {
    fn code(&self) -> &'static Code {
        match self {
            Self::Library(error) => code_of(error),
            Self::TrustNeedsTerminal => &PROXY_TRUST_NEEDS_TERMINAL,
            Self::TrustFailed { .. } => &PROXY_TRUST_FAILED,
        }
    }

    fn recovery(&self) -> Recovery {
        match self {
            Self::Library(ProxyError::PortHeld { .. } | ProxyError::PortDenied { .. }) => {
                Recovery::hint(
                    "stop the process that holds the address, or set `proxy_http` or `proxy_https` in ~/.config/mxv/config.toml to another loopback address, and run `mxv proxy start`",
                )
            }
            Self::Library(ProxyError::CaddyMissing { .. } | ProxyError::CaddyTooOld { .. }) => {
                Recovery::hint(
                    "install Caddy 2.8 or later, for example `brew install caddy`, or set `caddy` in ~/.config/mxv/config.toml to its path, and run `mxv proxy start`",
                )
            }
            Self::Library(ProxyError::SocketPathTooLong { .. }) => Recovery::hint(
                "set XDG_STATE_HOME to a shorter directory, and run `mxv proxy start`",
            ),
            Self::Library(ProxyError::LoadFailed { .. }) => Recovery::hint(
                "fix the route that the message names in the workspace's mxv.toml, and run `mxv proxy start`",
            ),
            Self::Library(_) => Recovery::hint(
                "fix the cause that the message names, and run `mxv proxy start` again; `mxv proxy status` shows what runs",
            ),
            Self::TrustNeedsTerminal => Recovery::Terminal(Call::new(&["proxy", "trust"])),
            Self::TrustFailed { .. } => Recovery::Terminal(Call::new(&["proxy", "trust"])),
        }
    }

    fn fields(&self) -> Vec<Field<'_>> {
        match self {
            Self::Library(ProxyError::PortHeld { address, key })
            | Self::Library(ProxyError::PortDenied { address, key }) => vec![
                Field::new("address", address.to_string()),
                Field::new("key", *key),
            ],
            Self::Library(ProxyError::CaddyTooOld { path, found }) => vec![
                Field::new("path", path.display().to_string()),
                Field::new("version", found),
            ],
            Self::Library(_) | Self::TrustNeedsTerminal => Vec::new(),
            Self::TrustFailed { ended } => vec![Field::new("ended", ended)],
        }
    }
}

impl fmt::Display for ProxyFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Library(error) => write!(formatter, "mxv could not run the proxy: {error}"),
            Self::TrustNeedsTerminal => formatter.write_str(
                "mxv refuses to trust the certificate authority without a terminal, because the system asks the person for a password",
            ),
            Self::TrustFailed { ended } => {
                write!(formatter, "caddy trust ended with {ended}, so this machine does not trust the authority")
            }
        }
    }
}

impl std::error::Error for ProxyFailure {}

/// What a lifecycle verb says about the proxy once its own work is done:
/// the line that names the URLs, and the warnings. The verb prints it
/// after its document, so the line follows the unit lines.
#[derive(Debug, Default)]
pub(crate) struct Served {
    /// The lines for standard error.
    lines: Vec<String>,
}

impl Served {
    /// Prints the lines on standard error.
    pub(crate) fn print(&self) {
        crate::say(&self.lines);
    }

    /// Returns the lines, for a test.
    #[cfg(test)]
    pub(crate) fn lines(&self) -> &[String] {
        &self.lines
    }
}

/// Brings the proxy in line with the records after a lifecycle verb ran
/// for `workspace`, and leaves in `served` the line that names the URLs of
/// the workspace's routes, or a warning that carries the failure's code.
/// A failure never fails the verb, because the units already run or
/// stopped.
pub(crate) fn sync(
    environment: &Environment,
    store: &Store,
    workspace: &Workspace,
    mode: SyncMode,
    served: &mut Served,
) {
    let declares = workspace
        .manifest()
        .ok()
        .flatten()
        .is_some_and(|manifest| !manifest.routes.is_empty());
    let proxy = match environment.proxy() {
        Ok(proxy) => proxy,
        Err(failure) => {
            served.lines.push(format!(
                "warning: mxv could not read the proxy's settings: {failure}."
            ));
            return;
        }
    };
    match proxy.sync(store, mode, declares) {
        Ok(synced) => {
            if mode == SyncMode::Start {
                let urls = synced.urls_of(&workspace.name);
                if !urls.is_empty() {
                    served.lines.push(format!("mxv serves {}.", join(&urls)));
                }
            }
        }
        Err(error) => served.lines.extend(warning(&workspace.name, mode, error)),
    }
}

/// Returns the lines of the warning for a sync of the routes of `name` that
/// `error` stopped.
fn warning(name: &WorkspaceName, mode: SyncMode, error: ProxyError) -> Vec<String> {
    let code = match &error {
        ProxyError::Store(_) => None,
        other => Some(Spelling::COLONS.spell(code_of(other))),
    };
    let what = match mode {
        SyncMode::Start => "serve",
        SyncMode::IfRunning => "update",
    };
    let reason = match error {
        ProxyError::Store(inner) => {
            let failure = MxvFailure::from(inner);
            let spelled = Spelling::COLONS.spell(Failure::code(&failure));
            return vec![
                format!("warning: mxv could not {what} the routes of {name}, because {failure}."),
                format!("{}error code: {spelled}", still_run(mode)),
            ];
        }
        other => other.to_string(),
    };
    vec![
        format!("warning: mxv could not {what} the routes of {name}, because {reason}."),
        format!(
            "Run `mxv proxy start` once that is fixed. {}error code: {}",
            still_run(mode),
            code.unwrap_or_default()
        ),
    ]
}

/// Returns the sentence that says the units still run after a verb that
/// starts them, and nothing after a verb that stops or removes them.
fn still_run(mode: SyncMode) -> &'static str {
    match mode {
        SyncMode::Start => "The units still run. ",
        SyncMode::IfRunning => "",
    }
}

/// Joins `items` as a sentence does: `a`, `a and b`, `a, b and c`.
fn join(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// One route, as `mxv proxy status` prints it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct RouteRow {
    /// The route's name in the workspace's `routes`.
    route: String,
    /// The workspace that declares the route.
    workspace: String,
    /// The URL that serves the route, or `(not served)` when another
    /// workspace's name makes the same host label.
    url: String,
    /// The process or container that listens on the route's port, or null
    /// when no unit does.
    unit: Option<String>,
    /// The state of the unit: `running`, `stopped`, and so on, or why the
    /// route has no unit.
    state: String,
    /// One line under the route when it is not served.
    notes: Vec<String>,
}

impl Row for RouteRow {
    const COLUMNS: &'static [&'static str] = &["route", "workspace", "url", "unit", "state"];
    const CHILDREN: Option<Children> = Some(Children::field("notes").help(
        "one indented line under a route that the proxy does not serve, which says which workspace keeps the host name.",
    ));
    // A person copies the workspace into the next command.
    const UNCUT_COLUMNS: &'static [&'static str] = &["workspace"];

    fn child_lines(&self) -> Vec<String> {
        self.notes.clone()
    }
}

/// What `mxv proxy status` says above its rows.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ProxySummary {
    /// Whether Caddy answers on the admin socket.
    running: bool,
    /// Caddy's version, or null when mxv could not tell.
    version: Option<String>,
    /// Caddy's process id, or null.
    pid: Option<u32>,
    /// The address that serves HTTP.
    http: String,
    /// The address that serves HTTPS.
    https: String,
    /// `none` when Caddy has not made a certificate authority, `trusted`
    /// or `untrusted` when this machine does or does not trust it, and
    /// `unknown` when mxv cannot tell on this system.
    authority: AuthorityState,
    /// The number of routes that the proxy serves once it runs.
    routes: usize,
    /// One sentence for each record or manifest that mxv could not read.
    problems: Vec<String>,
}

/// Whether this machine trusts the proxy's certificate authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AuthorityState {
    /// Caddy has not made one.
    None,
    /// This machine trusts it.
    Trusted,
    /// This machine does not trust it.
    Untrusted,
    /// mxv cannot tell on this system.
    Unknown,
}

impl fmt::Display for ProxySummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.running {
            let mut about = Vec::new();
            if let Some(version) = &self.version {
                about.push(format!("caddy {version}"));
            }
            if let Some(pid) = self.pid {
                about.push(format!("pid {pid}"));
            }
            let about = if about.is_empty() {
                String::new()
            } else {
                format!(" ({})", about.join(", "))
            };
            write!(
                formatter,
                "proxy: running{about}, http {}, https {}",
                self.http, self.https
            )?;
        } else if self.routes == 0 {
            formatter.write_str(
                "proxy: not running; no workspace declares a route, and `mxv up` starts it when one does",
            )?;
        } else {
            write!(
                formatter,
                "proxy: not running, although {} {} exist{}; `mxv proxy start` starts it",
                self.routes,
                if self.routes == 1 { "route" } else { "routes" },
                if self.routes == 1 { "s" } else { "" },
            )?;
        }
        match self.authority {
            AuthorityState::None => {}
            AuthorityState::Trusted => {
                formatter.write_str("\ncertificate authority: created, trusted by this machine")?;
            }
            AuthorityState::Untrusted => formatter.write_str(
                "\ncertificate authority: created, not trusted by this machine; a person runs `mxv proxy trust`",
            )?,
            AuthorityState::Unknown => formatter.write_str(
                "\ncertificate authority: created; mxv cannot tell whether this machine trusts it",
            )?,
        }
        for problem in &self.problems {
            write!(formatter, "\nwarning: {problem}")?;
        }
        Ok(())
    }
}

/// What `mxv proxy trust` did.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct TrustResult {
    /// The common name of the authority that this machine now trusts.
    authority: String,
}

impl fmt::Display for TrustResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("mxv asked Caddy to trust its local certificate authority.")
    }
}

/// Returns the listing of the proxy's state and routes.
fn listing(
    environment: &Environment,
    proxy: &Proxy,
    store: &Store,
) -> Result<Listing<RouteRow, ProxySummary>, crumbkit::Error> {
    let status = proxy.status(store).map_err(fail)?;
    let mut states = States::new(environment, store);
    let https = status.https;
    let mut rows: Vec<RouteRow> = status
        .routes
        .served
        .iter()
        .map(|route| states.row(route, route.url(https), Vec::new()))
        .collect();
    rows.extend(status.routes.shadowed.iter().map(|shadowed| {
        let label = shadowed
            .route
            .host
            .strip_prefix(&format!("{}.", shadowed.route.name))
            .and_then(|rest| rest.strip_suffix(&format!(".{}", multiverse::ROUTE_DOMAIN)))
            .unwrap_or_default()
            .to_owned();
        let note = format!(
            "{} and {} both map to the host label {label}; {} serves it",
            shadowed.route.workspace, shadowed.by, shadowed.by
        );
        states.row(&shadowed.route, "(not served)".to_owned(), vec![note])
    }));
    let summary = summary(&status);
    Ok(Listing::from_all(rows, None).summary(summary))
}

/// Returns the summary of `status`.
fn summary(status: &ProxyStatus) -> ProxySummary {
    ProxySummary {
        running: status.running,
        version: status.version.clone(),
        pid: status.pid,
        http: status.http.to_string(),
        https: status.https.to_string(),
        authority: status
            .authority
            .as_ref()
            .map_or(AuthorityState::None, |authority| match authority.trusted {
                Some(true) => AuthorityState::Trusted,
                Some(false) => AuthorityState::Untrusted,
                None => AuthorityState::Unknown,
            }),
        routes: status.routes.served.len(),
        problems: status.routes.problems.clone(),
    }
}

/// The states of the units of the workspaces that routes name, read once
/// for each workspace.
struct States<'a> {
    /// The environment, whose run files hold the states.
    environment: &'a Environment,
    /// The store of the workspace records.
    store: &'a Store,
    /// The states by workspace, each unit with its state.
    read: BTreeMap<WorkspaceName, BTreeMap<String, &'static str>>,
}

impl<'a> States<'a> {
    /// Returns an empty cache.
    const fn new(environment: &'a Environment, store: &'a Store) -> Self {
        Self {
            environment,
            store,
            read: BTreeMap::new(),
        }
    }

    /// Returns the row of `route`, whose URL is `url`.
    fn row(&mut self, route: &Route, url: String, notes: Vec<String>) -> RouteRow {
        let state = match &route.unit {
            None => format!("no unit listens on {}", route.name),
            Some(unit) => self
                .of(&route.workspace)
                .get(unit)
                .map_or("unknown", |state| state)
                .to_owned(),
        };
        RouteRow {
            route: route.name.clone(),
            workspace: route.workspace.to_string(),
            url,
            unit: route.unit.clone(),
            state,
            notes,
        }
    }

    /// Returns the state of each unit of `workspace`.
    fn of(&mut self, workspace: &WorkspaceName) -> &BTreeMap<String, &'static str> {
        if !self.read.contains_key(workspace) {
            let states = self.states(workspace).unwrap_or_default();
            self.read.insert(workspace.clone(), states);
        }
        self.read.entry(workspace.clone()).or_default()
    }

    /// Reads the state of each unit of `workspace`, or `None` when its
    /// record or run files cannot be read.
    fn states(&self, workspace: &WorkspaceName) -> Option<BTreeMap<String, &'static str>> {
        let record = self.store.read(workspace).ok().flatten()?;
        let manifest = record.manifest().ok().flatten();
        let statuses: Vec<ProcessStatus> = self
            .environment
            .runs()
            .ok()?
            .status(&record, manifest.as_ref())
            .ok()?;
        Some(
            statuses
                .iter()
                .map(|status| (status.name.clone(), ProcessRow::from(status).state_text()))
                .collect(),
        )
    }
}

/// Returns the state and routes of the proxy.
pub(crate) fn status(
    environment: &Environment,
) -> Result<Listing<RouteRow, ProxySummary>, crumbkit::Error> {
    let proxy = environment.proxy()?;
    let store = Store::new(environment.records()?);
    listing(environment, &proxy, &store)
}

/// Starts the proxy, whether or not the workspace that asks declares a
/// route, and returns its state and routes.
pub(crate) fn start(
    environment: &Environment,
) -> Result<Listing<RouteRow, ProxySummary>, crumbkit::Error> {
    let proxy = environment.proxy()?;
    let store = Store::new(environment.records()?);
    proxy.sync(&store, SyncMode::Start, true).map_err(fail)?;
    listing(environment, &proxy, &store)
}

/// Stops the proxy, and returns its state and routes.
pub(crate) fn stop(
    environment: &Environment,
) -> Result<Listing<RouteRow, ProxySummary>, crumbkit::Error> {
    let proxy = environment.proxy()?;
    let store = Store::new(environment.records()?);
    proxy.stop().map_err(fail)?;
    listing(environment, &proxy, &store)
}

/// Asks the person to confirm, then runs `caddy trust`, which adds the
/// proxy's certificate authority to this machine's trust store. An agent's
/// shell has no terminal, so it is refused before the proxy is touched.
pub(crate) fn trust(
    output: &mut Run,
    environment: &Environment,
) -> Result<Prose<TrustResult>, crumbkit::Error> {
    if !crate::person_can_answer() {
        return Err(ProxyFailure::TrustNeedsTerminal.into());
    }
    let proxy = environment.proxy()?;
    let Some(authority) = proxy.authority_name() else {
        return Err(fail(ProxyError::StartFailed {
            message: "the proxy does not run, so it has no authority to trust; `mxv proxy start` starts it"
                .to_owned(),
        }));
    };
    let key = proxy
        .settings()
        .caddy_data()
        .join("pki/authorities/local/root.key");
    output.confirm(
        Yes::default(),
        &format!(
            "mxv will ask Caddy to add its local certificate authority, \"{authority}\", to this machine's trust store.\nThe system will ask for your password. Any program that can read the authority's key at {} could then sign a certificate that this machine trusts for any name.\nContinue?",
            key.display()
        ),
    )?;
    let ended = proxy.trust().map_err(fail)?;
    if !ended.success() {
        return Err(ProxyFailure::TrustFailed {
            ended: ended.to_string(),
        }
        .into());
    }
    Ok(Prose::new(TrustResult { authority }))
}

impl From<Error> for ProxyFailure {
    fn from(error: Error) -> Self {
        Self::Library(ProxyError::Store(error))
    }
}

/// Runs the `mxv proxy` verb `command` and returns the exit code.
pub(crate) fn run(mut output: Run, environment: &Environment, command: ProxyCommand) -> ExitCode {
    match command {
        ProxyCommand::Status => crumbkit::finish(output, STATUS, status(environment)),
        ProxyCommand::Start => crumbkit::finish(output, START, start(environment)),
        ProxyCommand::Stop => crumbkit::finish(output, STOP, stop(environment)),
        // `trust` asks for confirmation through `&mut output`, so it runs
        // before `finish`.
        ProxyCommand::Trust => {
            let result = trust(&mut output, environment);
            crumbkit::finish(output, TRUST, result)
        }
    }
}

/// Runs a lifecycle verb that keeps the proxy's routes in line with the
/// workspace records. `call` runs the verb with the notes to fill, and
/// `finish` prints its result and returns the exit code. The notes about the
/// routes print after the result.
pub(crate) fn serving<T>(
    mut output: Run,
    call: impl FnOnce(&mut Run, &mut Served) -> T,
    finish: impl FnOnce(Run, T) -> ExitCode,
) -> ExitCode {
    let mut served = Served::default();
    let result = call(&mut output, &mut served);
    let code = finish(output, result);
    served.print();
    code
}
