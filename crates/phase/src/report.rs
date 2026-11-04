//! The report that `up`, `down` and `restart` return.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::name::{AppName, PortName, ProcessName};
use crate::plan::Permissions;

#[cfg(test)]
mod tests;

/// What `up`, `down` or `restart` did to each app it acted on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Report {
    /// One entry per app, in the order phase acted on them.
    pub apps: Vec<AppReport>,
}

impl Report {
    /// Returns the report of `apps`.
    #[must_use]
    pub const fn new(apps: Vec<AppReport>) -> Self {
        Self { apps }
    }
}

/// What an operation did to one app. In JSON the outcome's fields sit
/// beside `app`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AppReport {
    /// The app.
    pub app: AppName,
    /// What happened to it.
    #[serde(flatten)]
    pub outcome: AppOutcome,
}

impl AppReport {
    /// Returns the report that `outcome` happened to `app`.
    #[must_use]
    pub const fn new(app: AppName, outcome: AppOutcome) -> Self {
        Self { app, outcome }
    }
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
    /// phase restarted the app, because its inputs changed or `restart`
    /// asked for it.
    Restarted,
    /// phase did not start the app because an app it depends on did not
    /// start, or because another app failed to start.
    NotStarted,
    /// phase did not start the app because its permissions grew beyond what
    /// was granted. `mxv phase grants approve APP` grants them, and the next
    /// `up` starts it.
    #[non_exhaustive]
    AwaitingGrant {
        /// The permissions the app asks for beyond its grant.
        asked: Permissions,
    },
    /// A process of the app failed to start, so phase stopped what it had
    /// started of the app.
    #[non_exhaustive]
    Failed {
        /// The process that failed.
        process: ProcessName,
        /// Why it failed.
        problem: StartProblem,
    },
    /// phase stopped the app and checked that its processes are gone.
    Stopped,
    /// The app was not running.
    NotRunning,
    /// `down` left the shared service running because these running apps
    /// need it.
    #[non_exhaustive]
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

impl AppOutcome {
    /// Returns the outcome of an app whose permissions grew by `asked`.
    #[must_use]
    pub const fn awaiting_grant(asked: Permissions) -> Self {
        Self::AwaitingGrant { asked }
    }

    /// Returns the outcome of an app whose `process` failed to start
    /// because of `problem`.
    #[must_use]
    pub const fn failed(process: ProcessName, problem: StartProblem) -> Self {
        Self::Failed { process, problem }
    }

    /// Returns the outcome of a shared service that `down` left running
    /// because the apps `needed_by` need it.
    #[must_use]
    pub const fn kept_running(needed_by: Vec<AppName>) -> Self {
        Self::KeptRunning { needed_by }
    }
}

/// Why a process failed to start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
#[non_exhaustive]
pub enum StartProblem {
    /// Another process already listened on the process's port, so phase did
    /// not start it.
    #[non_exhaustive]
    PortTaken {
        /// The app port.
        port: PortName,
        /// The pids of the processes that hold the port, which may be empty
        /// when the operating system does not say.
        holders: Vec<u32>,
    },
    /// The process exited before it was ready.
    #[non_exhaustive]
    Exited {
        /// The exit status, or `None` when a signal ended the process.
        code: Option<i32>,
        /// The signal that ended the process, or `None`.
        signal: Option<i32>,
        /// The last lines of the process's log.
        log_tail: Vec<String>,
    },
    /// The process did not become ready within its ready timeout.
    #[non_exhaustive]
    NotReady {
        /// How long phase waited.
        #[serde(rename = "waited_ms", with = "crate::millis")]
        waited: Duration,
    },
    /// The process's port answered, but a process outside the app's process
    /// group holds it, so the probe was not reaching the app.
    #[non_exhaustive]
    ForeignListener {
        /// The app port.
        port: PortName,
        /// The pids of the processes that hold the port.
        holders: Vec<u32>,
    },
}

impl StartProblem {
    /// Returns the problem of `port` being held by the processes `holders`
    /// before the process started.
    #[must_use]
    pub const fn port_taken(port: PortName, holders: Vec<u32>) -> Self {
        Self::PortTaken { port, holders }
    }

    /// Returns the problem of the process exiting with `code` or `signal`
    /// before it was ready, with the last lines of its log.
    #[must_use]
    pub const fn exited(code: Option<i32>, signal: Option<i32>, log_tail: Vec<String>) -> Self {
        Self::Exited {
            code,
            signal,
            log_tail,
        }
    }

    /// Returns the problem of the process not becoming ready after
    /// `waited`.
    #[must_use]
    pub const fn not_ready(waited: Duration) -> Self {
        Self::NotReady { waited }
    }

    /// Returns the problem of `port` being held by the processes `holders`,
    /// which are outside the app's process group.
    #[must_use]
    pub const fn foreign_listener(port: PortName, holders: Vec<u32>) -> Self {
        Self::ForeignListener { port, holders }
    }
}
