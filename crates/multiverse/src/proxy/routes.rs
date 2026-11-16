//! The routes of the workspaces, derived from their records and manifests
//! on every sync. Nothing remembers a route between two syncs.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use crate::error::Error;
use crate::manifest::{MOST_LABEL_BYTES, host_label};
use crate::name::WorkspaceName;
use crate::store::Store;

/// The domain below which every route lives. Browsers and resolvers send
/// every name below `localhost` to the loopback address.
pub const DOMAIN: &str = "mxv.localhost";

/// One route: a host name that the proxy answers, and the host port it
/// forwards to.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub struct Route {
    /// The name of the route in the manifest's `routes`, such as `web`.
    pub name: String,
    /// The workspace that declares the route.
    pub workspace: WorkspaceName,
    /// The host name, such as `web.reading-list.mxv.localhost`.
    pub host: String,
    /// The workspace's host port that the route forwards to.
    pub port: u16,
    /// The process or container that listens on the port, or `None` when
    /// no unit of the manifest does.
    pub unit: Option<String>,
    /// The extra host names that the proxy serves for the route, from the
    /// `hosts` of the unit that listens on the port.
    pub extra: Vec<String>,
}

impl Route {
    /// Returns the URL of the route over HTTPS, which carries the port of
    /// `https` unless it is 443.
    #[must_use]
    pub fn url(&self, https: SocketAddr) -> String {
        match https.port() {
            443 => format!("https://{}", self.host),
            port => format!("https://{}:{port}", self.host),
        }
    }

    /// Returns the URLs of the route's extra host names over HTTPS, which
    /// carry the port of `https` unless it is 443.
    #[must_use]
    pub fn extra_urls(&self, https: SocketAddr) -> Vec<String> {
        self.extra
            .iter()
            .map(|host| match https.port() {
                443 => format!("https://{host}"),
                port => format!("https://{host}:{port}"),
            })
            .collect()
    }
}

/// What one workspace declares: the routes of its manifest and the ports of
/// its ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Declared {
    /// The workspace.
    pub workspace: WorkspaceName,
    /// The names in the manifest's `routes`.
    pub routes: Vec<String>,
    /// The host port of each named port in the record's ledger.
    pub ports: BTreeMap<String, u16>,
    /// The unit that listens on each named port.
    pub listeners: BTreeMap<String, String>,
    /// The extra host names of the unit that listens on each routed port.
    pub hosts: BTreeMap<String, Vec<String>>,
}

impl Declared {
    /// Returns what the workspace `workspace` declares.
    #[must_use]
    pub const fn new(
        workspace: WorkspaceName,
        routes: Vec<String>,
        ports: BTreeMap<String, u16>,
        listeners: BTreeMap<String, String>,
        hosts: BTreeMap<String, Vec<String>>,
    ) -> Self {
        Self {
            workspace,
            routes,
            ports,
            listeners,
            hosts,
        }
    }
}

/// A route that the proxy does not serve, because another workspace's name
/// makes the same host label and sorts first.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Shadowed {
    /// The route that is not served.
    pub route: Route,
    /// The workspace that keeps the host label.
    pub by: WorkspaceName,
}

/// An extra host name that the proxy does not serve, because the workspace
/// whose name sorts first lists the same name.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ShadowedHost {
    /// The host name.
    pub host: String,
    /// The workspace that lists it without being served.
    pub workspace: WorkspaceName,
    /// The name of the route that would carry it.
    pub route: String,
    /// The workspace that serves the name.
    pub by: WorkspaceName,
}

/// The routes of every workspace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Derived {
    /// The routes the proxy serves, in host order.
    pub served: Vec<Route>,
    /// The routes the proxy does not serve.
    pub shadowed: Vec<Shadowed>,
    /// The extra host names of served routes that the proxy does not
    /// serve.
    pub shadowed_hosts: Vec<ShadowedHost>,
    /// A sentence for each record or manifest that mxv could not read and
    /// each workspace name that makes no host label.
    pub problems: Vec<String>,
}

/// Derives the routes of `declared`. A route whose port the ledger lacks is
/// left out, because its workspace has never started. When two workspace
/// names make one host label, the name that sorts first keeps it, and the
/// same holds for an extra host name that two workspaces list.
#[must_use]
pub fn derive(mut declared: Vec<Declared>) -> Derived {
    declared.sort_by(|a, b| a.workspace.cmp(&b.workspace));
    let mut derived = Derived::default();
    let mut owners: BTreeMap<String, WorkspaceName> = BTreeMap::new();
    let mut extra_owners: BTreeMap<String, WorkspaceName> = BTreeMap::new();
    for item in declared {
        let label = host_label(item.workspace.as_str());
        if label.len() > MOST_LABEL_BYTES {
            if !item.routes.is_empty() {
                derived.problems.push(format!(
                    "the workspace {} makes a host label of {} bytes, and a label holds at most {MOST_LABEL_BYTES}",
                    item.workspace,
                    label.len()
                ));
            }
            continue;
        }
        let owner = owners
            .entry(label.clone())
            .or_insert_with(|| item.workspace.clone())
            .clone();
        for name in &item.routes {
            let Some(port) = item.ports.get(name) else {
                continue;
            };
            let listed = item.hosts.get(name).cloned().unwrap_or_default();
            let mut route = Route {
                name: name.clone(),
                workspace: item.workspace.clone(),
                host: format!("{}.{label}.{DOMAIN}", host_label(name)),
                port: *port,
                unit: item.listeners.get(name).cloned(),
                extra: Vec::new(),
            };
            if owner == item.workspace {
                for host in listed {
                    let keeper = extra_owners
                        .entry(host.clone())
                        .or_insert_with(|| item.workspace.clone());
                    if *keeper == item.workspace {
                        route.extra.push(host);
                    } else {
                        derived.shadowed_hosts.push(ShadowedHost {
                            host,
                            workspace: item.workspace.clone(),
                            route: name.clone(),
                            by: keeper.clone(),
                        });
                    }
                }
                derived.served.push(route);
            } else {
                route.extra = listed;
                derived.shadowed.push(Shadowed {
                    route,
                    by: owner.clone(),
                });
            }
        }
    }
    derived.served.sort_by(|a, b| a.host.cmp(&b.host));
    derived
}

/// Reads what every workspace of `store` declares, and a sentence for each
/// record or manifest that cannot be read.
///
/// # Errors
///
/// Returns the error of listing the store's directory.
pub(super) fn declared(store: &Store) -> Result<(Vec<Declared>, Vec<String>), Error> {
    let records = store.list()?;
    let mut problems: Vec<String> = records
        .problems
        .iter()
        .map(|error| format!("a workspace record is left out: {error}"))
        .collect();
    let mut found = Vec::new();
    for workspace in records.workspaces {
        match workspace.manifest() {
            Ok(Some(manifest)) if !manifest.routes.is_empty() => {
                let listeners: BTreeMap<String, String> = manifest
                    .routes
                    .iter()
                    .filter_map(|route| Some((route.clone(), manifest.listener_of(route)?)))
                    .collect();
                let hosts = listeners
                    .iter()
                    .filter_map(|(route, unit)| {
                        let hosts = manifest.hosts_of(unit);
                        (!hosts.is_empty()).then(|| (route.clone(), hosts.to_vec()))
                    })
                    .collect();
                found.push(Declared::new(
                    workspace.name.clone(),
                    manifest.routes.clone(),
                    workspace.ports.clone(),
                    listeners,
                    hosts,
                ));
            }
            Ok(_) => {}
            Err(error) => problems.push(format!(
                "the manifest of {} is left out: {error}",
                workspace.name
            )),
        }
    }
    Ok((found, problems))
}
