//! The JSON that mxv gives Caddy: one HTTP server with a route and an error
//! page for each host, a TLS policy that signs with Caddy's own authority,
//! and the admin socket.

use serde_json::{Map, Value, json};

use super::Settings;
use super::routes::Route;

/// Returns the configuration that serves `routes`. It is the same value
/// for the same routes and settings, so a sync compares it with the running
/// configuration.
pub(super) fn render(settings: &Settings, routes: &[Route]) -> Value {
    let hosts: Vec<&str> = routes.iter().map(|route| route.host.as_str()).collect();
    let mut apps = Map::new();
    apps.insert(
        "pki".to_owned(),
        json!({ "certificate_authorities": { "local": { "install_trust": false } } }),
    );
    if !routes.is_empty() {
        apps.insert(
            "http".to_owned(),
            json!({
                "http_port": settings.http.port(),
                "https_port": settings.https.port(),
                "servers": {
                    "mxv": {
                        "listen": [settings.http.to_string(), settings.https.to_string()],
                        "routes": routes.iter().map(forward).collect::<Vec<_>>(),
                        "errors": {
                            "routes": routes.iter().map(error_page).collect::<Vec<_>>(),
                        },
                        "automatic_https": { "disable_redirects": true },
                    },
                },
            }),
        );
        apps.insert(
            "tls".to_owned(),
            json!({
                "automation": {
                    "policies": [{ "subjects": hosts, "issuers": [{ "module": "internal" }] }],
                },
            }),
        );
    }
    json!({
        "admin": {
            "listen": format!("unix/{}|0600", settings.socket().display()),
            "config": { "persist": false },
        },
        "storage": {
            "module": "file_system",
            "root": settings.caddy_data().display().to_string(),
        },
        "logging": logging(),
        "apps": apps,
    })
}

/// Returns the logging section: Caddy's own messages at `INFO` on standard
/// error, where phase keeps them in the proxy's log file.
fn logging() -> Value {
    json!({ "logs": { "default": { "level": "INFO", "writer": { "output": "stderr" } } } })
}

/// Returns the route that forwards the host of `route` to its port.
fn forward(route: &Route) -> Value {
    json!({
        "match": [{ "host": [route.host] }],
        "handle": [{
            "handler": "reverse_proxy",
            "upstreams": [{ "dial": format!("localhost:{}", route.port) }],
        }],
        "terminal": true,
    })
}

/// Returns the error route that answers a host whose upstream does not
/// answer, which is the usual state of a stopped workspace.
fn error_page(route: &Route) -> Value {
    json!({
        "match": [{ "host": [route.host] }],
        "handle": [{
            "handler": "static_response",
            "status_code": "502",
            "headers": { "Content-Type": ["text/plain; charset=utf-8"] },
            "body": format!(
                "{} is a route of the stopped workspace {}.\nRun `mxv up -w {}` to start it.\n",
                route.host, route.workspace, route.workspace
            ),
        }],
        "terminal": true,
    })
}
