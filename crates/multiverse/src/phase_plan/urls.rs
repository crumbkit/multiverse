//! The variables that tell a host process or task the proxy's names for
//! its own workspace: `MXV_DOMAIN`, `MXV_HOST_<PORT>` and `MXV_URL_<PORT>`
//! for each served route, and `MXV_HOST` and `MXV_URL` for the route that a
//! process listens on.

use std::net::SocketAddr;

use crate::manifest::host_label;
use crate::name::WorkspaceName;
use crate::proxy::Route;

/// The prefix of the variable that holds a route's host name.
const HOST_PREFIX: &str = "MXV_HOST_";

/// The prefix of the variable that holds a route's URL.
const URL_PREFIX: &str = "MXV_URL_";

/// The routes that the proxy serves for one workspace, and the HTTPS
/// address that their URLs carry. A workspace that serves no route has none,
/// and receives no variable.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProxyUrls {
    /// The workspace's domain, such as `contacts.mxv.localhost`.
    domain: String,
    /// The routes, each with its host and URL.
    routes: Vec<(String, String, String)>,
}

impl ProxyUrls {
    /// Returns the names for `workspace` from `served`, the routes that the
    /// proxy serves for every workspace, whose URLs carry the port of
    /// `https` unless it is 443.
    pub(crate) fn of(
        workspace: &WorkspaceName,
        served: &[Route],
        https: SocketAddr,
        domain: &str,
    ) -> Self {
        let routes: Vec<(String, String, String)> = served
            .iter()
            .filter(|route| &route.workspace == workspace)
            .map(|route| (route.name.clone(), route.host.clone(), route.url(https)))
            .collect();
        if routes.is_empty() {
            return Self::default();
        }
        Self {
            domain: format!("{}.{domain}", host_label(workspace.as_str())),
            routes,
        }
    }

    /// Returns the variables that every host process and task of the
    /// workspace receives, or none when it serves no route.
    pub(crate) fn shared(&self) -> Vec<(String, String)> {
        if self.routes.is_empty() {
            return Vec::new();
        }
        let mut variables = vec![("MXV_DOMAIN".to_owned(), self.domain.clone())];
        for (name, host, url) in &self.routes {
            let suffix = super::port_suffix(name);
            variables.push((format!("{HOST_PREFIX}{suffix}"), host.clone()));
            variables.push((format!("{URL_PREFIX}{suffix}"), url.clone()));
        }
        variables
    }

    /// Returns the URL of the served route named `port`, or `None` when no
    /// served route has that name.
    pub(crate) fn url(&self, port: &str) -> Option<&str> {
        self.routes
            .iter()
            .find(|(name, _, _)| name == port)
            .map(|(_, _, url)| url.as_str())
    }

    /// Returns `MXV_HOST` and `MXV_URL` for a process that listens on the
    /// port `port`, or none when no served route has that name.
    pub(crate) fn listened(&self, port: &str) -> Vec<(String, String)> {
        self.routes
            .iter()
            .find(|(name, _, _)| name == port)
            .map(|(_, host, url)| {
                vec![
                    ("MXV_HOST".to_owned(), host.clone()),
                    ("MXV_URL".to_owned(), url.clone()),
                ]
            })
            .unwrap_or_default()
    }
}
