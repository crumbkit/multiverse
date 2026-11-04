//! The report that `up` and `down` return.

use serde::{Deserialize, Serialize};

use crate::name::AppName;

/// What `up` or `down` did to each app it acted on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Report {
    /// One entry per app, in the order phase acted on them.
    pub apps: Vec<AppReport>,
}

/// What an operation did to one app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AppReport {
    /// The app.
    pub app: AppName,
    /// What happened to it.
    pub outcome: AppOutcome,
}

/// What an operation did to one app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum AppOutcome {
    /// phase started the app, and every process is ready.
    Started,
    /// The app was already running with the same inputs, so phase left it
    /// alone.
    AlreadyRunning,
    /// phase restarted the app because its inputs changed.
    Restarted,
    /// phase did not start the app because another app failed to start.
    NotStarted,
    /// phase stopped the app and checked that its processes are gone.
    Stopped,
    /// The app was not running.
    NotRunning,
    /// `down` left the shared service running because these running apps
    /// need it.
    KeptRunning {
        /// The apps that need it.
        needed_by: Vec<AppName>,
    },
    /// A deadline passed while phase was starting the app, so phase does not
    /// know whether it is running. phase does not retry it.
    StartUncertain,
    /// A deadline passed while phase was stopping the app, so phase does not
    /// know whether it has stopped. phase does not retry it.
    StopUncertain,
}
