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
/// The code of `mxv proxy check` when a row is missing or failed.
pub const PROXY_CHECK_FAILED: Code = Code::new("mxv", "proxy", "check_failed");

/// Every code that a [`ProxyFailure`] returns.
pub(crate) const CODES: &[Code<'static>] = &[
    PROXY_PORT_HELD,
    PROXY_PORT_DENIED,
    PROXY_CADDY_MISSING,
    PROXY_CADDY_TOO_OLD,
    PROXY_START_FAILED,
    PROXY_LOAD_FAILED,
    PROXY_SOCKET_PATH_TOO_LONG,
    PROXY_TRUST_NEEDS_TERMINAL,
    PROXY_TRUST_FAILED,
    PROXY_CHECK_FAILED,
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

/// The document that `mxv proxy check` prints.
pub(crate) const CHECK: Document<Listing<CheckLine>> = Document::new(
    "mxv",
    "proxy-check",
    1,
    "One row for each thing the proxy needs: the Caddy program, its two addresses, the certificate authority, a running Caddy and each extra host name, and whether it is set up.",
);

/// The document that `mxv proxy install` prints.
pub(crate) const INSTALL: Document<Prose<InstallResult>> = Document::new(
    "mxv",
    "proxy-install",
    1,
    "Whether Caddy was started and whether this machine trusts its certificate authority once the install is done.",
);

/// Why a proxy command could not do what it was asked.
#[derive(Debug)]
pub(crate) enum ProxyFailure {
    /// The library could not start Caddy or load the routes.
    Library(ProxyError),
    /// `mxv proxy trust` or `mxv proxy install` ran without a terminal. It
    /// holds the words of the command that the person runs.
    TrustNeedsTerminal(&'static [&'static str]),
    /// `mxv proxy check` found rows that are missing or failed.
    CheckFailed {
        /// The number of rows that are missing or failed.
        failing: usize,
        /// The number of rows.
        total: usize,
    },
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
fn code_of(error: &ProxyError) -> Code<'static> {
    match error {
        ProxyError::PortHeld { .. } => PROXY_PORT_HELD,
        ProxyError::PortDenied { .. } => PROXY_PORT_DENIED,
        ProxyError::CaddyMissing { .. } => PROXY_CADDY_MISSING,
        ProxyError::CaddyTooOld { .. } => PROXY_CADDY_TOO_OLD,
        ProxyError::LoadFailed { .. } => PROXY_LOAD_FAILED,
        ProxyError::SocketPathTooLong { .. } => PROXY_SOCKET_PATH_TOO_LONG,
        _ => PROXY_START_FAILED,
    }
}

impl Failure for ProxyFailure {
    fn code(&self) -> Code<'_> {
        match self {
            Self::Library(error) => code_of(error),
            Self::TrustNeedsTerminal(_) => PROXY_TRUST_NEEDS_TERMINAL,
            Self::CheckFailed { .. } => PROXY_CHECK_FAILED,
            Self::TrustFailed { .. } => PROXY_TRUST_FAILED,
        }
    }

    fn recovery(&self) -> Recovery {
        match self {
            Self::Library(
                ProxyError::PortHeld { config, .. } | ProxyError::PortDenied { config, .. },
            ) => Recovery::hint(format!(
                "stop the process that holds the address, or set `proxy_http` or `proxy_https` in {} to another loopback address, and run `mxv proxy start`",
                config.display()
            )),
            Self::Library(
                ProxyError::CaddyMissing { config, .. } | ProxyError::CaddyTooOld { config, .. },
            ) => Recovery::hint(format!(
                "install Caddy 2.8 or later, for example `brew install caddy`, or set `caddy` in {} to its path, and run `mxv proxy start`",
                config.display()
            )),
            Self::Library(ProxyError::SocketPathTooLong { .. }) => Recovery::hint(
                "set XDG_STATE_HOME to a shorter directory, and run `mxv proxy start`",
            ),
            Self::Library(ProxyError::LoadFailed { .. }) => Recovery::hint(
                "fix the route that the message names in the workspace's mxv.toml, and run `mxv proxy start`",
            ),
            Self::Library(_) => Recovery::hint(
                "fix the cause that the message names, and run `mxv proxy start` again; `mxv proxy status` shows what runs",
            ),
            Self::TrustNeedsTerminal(words) => Recovery::Terminal(Call::new(words)),
            Self::CheckFailed { .. } => Recovery::hint(
                "fix each row marked missing or failed, and run `mxv proxy check` again; `mxv proxy install` sets up Caddy and the certificate authority",
            ),
            Self::TrustFailed { .. } => Recovery::Terminal(Call::new(&["proxy", "trust"])),
        }
    }

    fn fields(&self) -> Vec<Field<'_>> {
        match self {
            Self::Library(ProxyError::PortHeld { address, key, .. })
            | Self::Library(ProxyError::PortDenied { address, key, .. }) => vec![
                Field::new("address", address.to_string()),
                Field::new("key", *key),
            ],
            Self::Library(ProxyError::CaddyTooOld { path, found, .. }) => vec![
                Field::new("path", path.display().to_string()),
                Field::new("version", found),
            ],
            Self::Library(_) | Self::TrustNeedsTerminal(_) => Vec::new(),
            Self::CheckFailed { failing, total } => vec![
                Field::new("failing", failing.to_string()),
                Field::new("total", total.to_string()),
            ],
            Self::TrustFailed { ended } => vec![Field::new("ended", ended)],
        }
    }
}

impl fmt::Display for ProxyFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Library(error) => write!(formatter, "mxv could not run the proxy: {error}"),
            Self::TrustNeedsTerminal(_) => formatter.write_str(
                "mxv refuses to trust the certificate authority without a terminal, because the system asks the person for a password",
            ),
            Self::CheckFailed { failing, total } => {
                write!(formatter, "{failing} of {total} proxy checks failed")
            }
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
                served.lines.extend(extra_lines(
                    &synced.extra_urls_of(&workspace.name),
                    &synced.extra_hosts_of(&workspace.name),
                    &*environment.resolver(),
                ));
                served.lines.extend(
                    synced
                        .problems_of(&workspace.name)
                        .iter()
                        .map(|problem| format!("note: {problem}.")),
                );
                let mut states = States::new(environment, store);
                served.lines.extend(displaced_lines(
                    &workspace.name,
                    &synced.displaced_by(&workspace.name),
                    &mut |other| states.of(other).values().any(|state| *state == "running"),
                ));
            }
        }
        Err(error) => served.lines.extend(warning(&workspace.name, mode, error)),
    }
}

/// Returns the lines about a workspace's extra host names: where the proxy
/// serves them, and for each name that does not resolve to this machine the
/// line to add to `/etc/hosts`. `urls` holds each route and URL as
/// `web at https://name`, and `hosts` the names. mxv never writes the file.
fn extra_lines(
    urls: &[String],
    hosts: &[String],
    resolver: &dyn multiverse::Resolver,
) -> Vec<String> {
    if urls.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!("mxv also serves {}.", join(urls))];
    lines.extend(
        multiverse::missing_hosts(hosts, resolver)
            .into_iter()
            .map(|(name, found)| match found.advice(&name) {
                Some(advice) => format!("note: {}; {advice}", found.detail(&name)),
                None => format!("note: {}", found.detail(&name)),
            }),
    );
    lines
}

/// Returns the lines that tell the person which running workspaces keep the
/// old URL in their variables because `workspace` took a name from them.
/// `running` says whether a workspace has a unit that runs. Each line names
/// the command that gives the workspace its variables again. mxv keeps no
/// memory of who held a name before, so a workspace that started while the
/// name was already shadowed gets the line too.
fn displaced_lines(
    workspace: &WorkspaceName,
    displaced: &[WorkspaceName],
    running: &mut dyn FnMut(&WorkspaceName) -> bool,
) -> Vec<String> {
    displaced
        .iter()
        .filter(|other| running(other))
        .map(|other| {
            format!(
                "note: {workspace} now serves a name that {other} also declares, and {other} runs with the URL it was given when it started; run `mxv restart -w {other}` to give it the current variables."
            )
        })
        .collect()
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
    /// The lines under the route: why it is not served, its extra host
    /// names and whether each one resolves to this machine.
    notes: Vec<String>,
}

impl RouteRow {
    /// Returns the notes that follow the row.
    fn child_lines(&self) -> Vec<String> {
        self.notes.clone()
    }
}

impl Row for RouteRow {
    const COLUMNS: &'static [&'static str] = &["route", "workspace", "url", "unit", "state"];
    const CHILDREN: Option<Children<Self>> = Some(Children::field("notes").lines(Self::child_lines).help(
        "one indented line under a route for each extra host name it serves, which says when the name does not resolve to this machine, and for a route or name that the proxy does not serve, which says which workspace keeps it.",
    ));
    // A person copies the workspace into the next command.
    const UNCUT_COLUMNS: &'static [&'static str] = &["workspace"];
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
    let mut names: Vec<String> = status
        .routes
        .served
        .iter()
        .flat_map(|route| route.extra.iter().cloned())
        .collect();
    names.sort();
    names.dedup();
    let unresolved: std::collections::BTreeMap<String, multiverse::Resolution> =
        multiverse::missing_hosts(&names, &*environment.resolver())
            .into_iter()
            .collect();
    let mut rows: Vec<RouteRow> = status
        .routes
        .served
        .iter()
        .map(|route| {
            let notes = route
                .extra
                .iter()
                .zip(route.extra_urls(https))
                .map(|(name, url)| match unresolved.get(name) {
                    Some(multiverse::Resolution::TimedOut) => {
                        format!("{url}  (the lookup of {name} timed out)")
                    }
                    Some(_) => format!(
                        "{url}  (not in /etc/hosts; add {})",
                        multiverse::hosts_line(name)
                    ),
                    None => url,
                })
                .collect();
            states.row(route, route.url(https), notes)
        })
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
    for hidden in &status.routes.shadowed_hosts {
        let note = format!(
            "{} is not served for {}; {} lists it too and serves it",
            hidden.host, hidden.workspace, hidden.by
        );
        if let Some(row) = rows
            .iter_mut()
            .find(|row| row.workspace == hidden.workspace.to_string() && row.route == hidden.route)
        {
            row.notes.push(note);
        }
    }
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
        return Err(ProxyFailure::TrustNeedsTerminal(&["proxy", "trust"]).into());
    }
    let proxy = environment.proxy()?;
    let authority = ask_and_trust(output, &proxy)?;
    Ok(Prose::new(TrustResult { authority }))
}

/// Asks the person to confirm, runs `caddy trust` against the running
/// proxy and returns the authority's name.
fn ask_and_trust(output: &mut Run, proxy: &Proxy) -> Result<String, crumbkit::Error> {
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
    Ok(authority)
}

/// What `mxv proxy install` did.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct InstallResult {
    /// The path of the Caddy program.
    caddy: String,
    /// Caddy's version.
    version: String,
    /// Whether this run started Caddy, which is false when it already ran.
    started: bool,
    /// Whether this machine already trusted the certificate authority, so
    /// that mxv asked nothing.
    already_trusted: bool,
    /// The common name of the authority that this machine trusts.
    authority: Option<String>,
}

impl fmt::Display for InstallResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.already_trusted {
            formatter
                .write_str("This machine already trusts Caddy's local certificate authority.")?;
        } else {
            formatter.write_str("mxv asked Caddy to trust its local certificate authority.")?;
        }
        formatter.write_str(
            "\nmxv's proxy is ready. `mxv up` serves each route of a workspace at https://<route>.<workspace>.mxv.localhost.",
        )
    }
}

/// The steps of `mxv proxy install` that touch the machine, so that a test
/// can stand in for each one and end the run between them.
pub(crate) struct InstallSteps<'a> {
    /// Starts Caddy when it does not run, with the routes there are or with
    /// none, and says whether it started it.
    pub(crate) start: &'a mut dyn FnMut() -> Result<bool, crumbkit::Error>,
    /// Says whether this machine trusts the local certificate authority.
    pub(crate) trusted: &'a mut dyn FnMut() -> Result<bool, crumbkit::Error>,
    /// Asks the person, runs `caddy trust` and returns the authority's name.
    pub(crate) trust: &'a mut dyn FnMut() -> Result<String, crumbkit::Error>,
    /// Whether a person can answer the question.
    pub(crate) can_answer: bool,
}

/// What the steps of an install did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Installed {
    /// Whether this run started Caddy.
    started: bool,
    /// Whether the machine already trusted the authority.
    already_trusted: bool,
    /// The authority that this run asked the machine to trust.
    authority: Option<String>,
}

/// Runs the steps of an install in order: start Caddy, then read the trust,
/// then ask when the machine does not trust the authority. Each step leaves
/// a state that the next run finds and continues from. A Caddy that started
/// stays running when the trust is refused or fails, and the next run asks
/// again because it reads the trust again.
pub(crate) fn install_with(steps: InstallSteps<'_>) -> Result<Installed, crumbkit::Error> {
    let started = (steps.start)()?;
    let already_trusted = (steps.trusted)()?;
    let authority = if already_trusted {
        None
    } else {
        if !steps.can_answer {
            return Err(ProxyFailure::TrustNeedsTerminal(&["proxy", "install"]).into());
        }
        Some((steps.trust)()?)
    };
    Ok(Installed {
        started,
        already_trusted,
        authority,
    })
}

/// Returns the line that says what an install did to Caddy, which listens
/// on `http` and `https` and serves `routes` routes.
fn ran_line(
    started: bool,
    routes: usize,
    http: std::net::SocketAddr,
    https: std::net::SocketAddr,
) -> String {
    if started {
        format!("mxv started Caddy on {http} and {https}.")
    } else if routes > 0 {
        format!("Caddy already runs on {http} and {https}; mxv loaded the current routes.")
    } else {
        format!("Caddy already runs on {http} and {https}.")
    }
}

/// Sets the proxy up: finds Caddy, starts it, and asks the person to trust
/// its certificate authority unless this machine already does. Caddy starts
/// with the routes of every workspace, or with none, so the person can trust
/// the authority before any workspace declares a route. It never stops a
/// Caddy that runs. Running it again changes nothing that is already set up.
/// Without a terminal it starts Caddy and then refuses to ask.
pub(crate) fn install(
    output: &mut Run,
    environment: &Environment,
) -> Result<Prose<InstallResult>, crumbkit::Error> {
    let proxy = environment.proxy()?;
    let store = Store::new(environment.records()?);
    let (caddy, version) = proxy.caddy().map_err(fail)?;
    let settings = proxy.settings();
    let (http, https) = (settings.http(), settings.https());
    crate::say(&[format!("mxv found caddy {version} at {}.", caddy.display())]);
    let mut start = || {
        let ensured = proxy.ensure_running(&store).map_err(fail)?;
        crate::say(&[ran_line(ensured.started, ensured.routes, http, https)]);
        Ok(ensured.started)
    };
    let mut trusted = || {
        Ok(proxy
            .status(&store)
            .map_err(fail)?
            .authority
            .is_some_and(|authority| authority.trusted == Some(true)))
    };
    let mut trust = || ask_and_trust(output, &proxy);
    let done = install_with(InstallSteps {
        start: &mut start,
        trusted: &mut trusted,
        trust: &mut trust,
        can_answer: crate::person_can_answer(),
    })?;
    Ok(Prose::new(InstallResult {
        caddy: caddy.display().to_string(),
        version,
        started: done.started,
        already_trusted: done.already_trusted,
        authority: done.authority,
    }))
}

/// One row of `mxv proxy check`.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct CheckLine {
    /// What the row checks.
    check: String,
    /// `ok`, `missing` for something that `mxv proxy install` or the person
    /// sets up, `failed` for something that is wrong, or `unknown` when mxv
    /// cannot tell.
    state: CheckOutcome,
    /// What mxv found.
    detail: String,
    /// The lines under the row, such as the line to add to `/etc/hosts`.
    notes: Vec<String>,
}

/// How a check came out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CheckOutcome {
    /// The check passed.
    Ok,
    /// Something is not set up yet.
    Missing,
    /// Something is wrong.
    Failed,
    /// mxv cannot tell. This row does not make the check fail.
    Unknown,
}

impl CheckLine {
    /// Returns the notes that follow the row.
    fn child_lines(&self) -> Vec<String> {
        self.notes.clone()
    }
}

impl Row for CheckLine {
    const COLUMNS: &'static [&'static str] = &["check", "state", "detail"];
    const CHILDREN: Option<Children<Self>> = Some(
        Children::field("notes")
            .lines(Self::child_lines)
            .help("one indented line under a row, such as the line to add to /etc/hosts for an extra host name that does not resolve to this machine."),
    );
    // A person copies the line and the address into the next step.
    const UNCUT_COLUMNS: &'static [&'static str] = &["detail"];
}

/// Reads the state of the proxy and of the machine around it. The check
/// never starts Caddy, loads a configuration, takes the proxy's lock or
/// trusts the authority.
pub(crate) fn check(environment: &Environment) -> Result<(Vec<CheckLine>, usize), crumbkit::Error> {
    let proxy = environment.proxy()?;
    let store = Store::new(environment.records()?);
    let result = proxy
        .check(&store, &*environment.resolver())
        .map_err(fail)?;
    let failing = result.failing();
    let lines = result
        .rows
        .into_iter()
        .map(|row| CheckLine {
            check: row.name.to_owned(),
            state: match row.state {
                multiverse::ProxyCheckState::Ok => CheckOutcome::Ok,
                multiverse::ProxyCheckState::Missing => CheckOutcome::Missing,
                multiverse::ProxyCheckState::Unknown => CheckOutcome::Unknown,
                multiverse::ProxyCheckState::Failed => CheckOutcome::Failed,
            },
            detail: row.detail,
            notes: row.notes,
        })
        .collect();
    Ok((lines, failing))
}

/// Prints the rows of the check, and then the failure that counts the rows
/// that are missing or failed, so a person sees both. The exit code is the
/// failure's.
fn finish_check(output: Run, environment: &Environment) -> ExitCode {
    let (lines, failing) = match check(environment) {
        Ok(found) => found,
        Err(error) => return crumbkit::finish(output, CHECK, Err::<Listing<CheckLine>, _>(error)),
    };
    let total = lines.len();
    if failing == 0 {
        return crumbkit::finish(
            output,
            CHECK,
            Ok::<_, crumbkit::Error>(Listing::from_all(lines, None)),
        );
    }
    let mut sink = crumbkit::Sink::standard();
    let mut run = output.clone();
    let _ = crumbkit::finish_to(
        &mut sink,
        output,
        CHECK,
        Ok::<_, crumbkit::Error>(Listing::from_all(lines, None)),
    );
    run.fail(&mut sink, &ProxyFailure::CheckFailed { failing, total })
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
        ProxyCommand::Check => finish_check(output, environment),
        // `install` asks for confirmation through `&mut output`, as `trust`
        // does.
        ProxyCommand::Install => {
            let result = install(&mut output, environment);
            crumbkit::finish(output, INSTALL, result)
        }
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
