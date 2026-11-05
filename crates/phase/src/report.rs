//! The report that `up`, `down` and `restart` return.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::name::{AppName, PortName, ProcessName};
use crate::plan::Permissions;
use crate::record::DockerCause;

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
    /// What else phase found about the app's processes, such as a port
    /// whose listener it could not find. JSON leaves out an empty list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Note>,
}

impl AppReport {
    /// Returns the report that `outcome` happened to `app`, with no notes.
    #[must_use]
    pub const fn new(app: AppName, outcome: AppOutcome) -> Self {
        Self {
            app,
            outcome,
            notes: Vec::new(),
        }
    }

    /// Returns this report with `notes`.
    #[must_use]
    pub fn with_notes(mut self, notes: Vec<Note>) -> Self {
        self.notes = notes;
        self
    }
}

/// Something phase found about one process of an app, beside the app's
/// outcome. In JSON its kind is `note`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "note", rename_all = "kebab-case", deny_unknown_fields)]
#[non_exhaustive]
pub enum Note {
    /// phase could not find what listens on the process's port, so it did
    /// not check the port before the start, or after readiness or a stop.
    /// It did not let that block the start.
    #[non_exhaustive]
    ListenerUnknown {
        /// The process.
        process: ProcessName,
        /// The app port.
        port: PortName,
        /// Why phase could not tell, which holds no environment value.
        reason: String,
    },
    /// The process's group is gone after a stop, and another process still
    /// holds its port. phase cannot prove that the holder is its own, so it
    /// did not signal it.
    #[non_exhaustive]
    DetachedListener {
        /// The process.
        process: ProcessName,
        /// The app port.
        port: PortName,
        /// The pids of the processes that hold the port.
        holders: Vec<u32>,
    },
    /// phase may not read the process that the run record names, so it
    /// cannot prove the process is its own. phase left it alone, and the
    /// run record keeps it.
    #[non_exhaustive]
    NotOurs {
        /// The process.
        process: ProcessName,
    },
    /// The process led its group and has gone, and a group with its id
    /// still has a live member, but nothing proves that group is the one
    /// phase started: another program may have taken the free pid and led
    /// a new group with it. phase did not signal the group, and the run
    /// record keeps the process.
    #[non_exhaustive]
    GroupUnverified {
        /// The process.
        process: ProcessName,
        /// The group that phase left alone.
        group: u32,
    },
    /// Something of the process's group was still there after `KILL` and
    /// the kill wait. The run record keeps it, and `sweep` can try again.
    #[non_exhaustive]
    Unstoppable {
        /// The process.
        process: ProcessName,
        /// The group that is left.
        group: u32,
    },
}

impl Note {
    /// Returns the note that phase could not find the listener of `port`
    /// of `process`, because of `reason`.
    #[must_use]
    pub const fn listener_unknown(process: ProcessName, port: PortName, reason: String) -> Self {
        Self::ListenerUnknown {
            process,
            port,
            reason,
        }
    }

    /// Returns the note that `holders` still hold `port` of `process`
    /// after its group is gone.
    #[must_use]
    pub const fn detached_listener(
        process: ProcessName,
        port: PortName,
        holders: Vec<u32>,
    ) -> Self {
        Self::DetachedListener {
            process,
            port,
            holders,
        }
    }

    /// Returns the note that phase cannot prove `process` is its own.
    #[must_use]
    pub const fn not_ours(process: ProcessName) -> Self {
        Self::NotOurs { process }
    }

    /// Returns the note that phase left `group`, whose leader `process` has
    /// gone, because nothing proves the group is its own.
    #[must_use]
    pub const fn group_unverified(process: ProcessName, group: u32) -> Self {
        Self::GroupUnverified { process, group }
    }

    /// Returns the note that `group` of `process` survived `KILL`.
    #[must_use]
    pub const fn unstoppable(process: ProcessName, group: u32) -> Self {
        Self::Unstoppable { process, group }
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
    /// The caller cancelled the operation before phase reached the app, so
    /// phase left it as it was.
    Untouched,
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
    /// phase stopped what it could of the app, and left processes that it
    /// could not stop or could not prove are its own. The app's notes name
    /// each one, and the run record keeps them.
    Left,
    /// phase could not act on the app because of a failure that is not a
    /// process's start problem, such as a source that a later release
    /// settles. phase started nothing more of the app.
    #[non_exhaustive]
    Errored {
        /// The failure's stable code, such as `phase::app::not_runnable`.
        code: String,
        /// The failure's message, which holds no environment value.
        message: String,
        /// What kind of Docker failure caused the error, when a Docker
        /// failure caused it, so a program need not read the message. JSON
        /// leaves it out when it is `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<DockerCause>,
    },
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

    /// Returns the outcome of an app that phase could not act on because of
    /// `error`, with the cause of a Docker failure.
    #[must_use]
    pub fn errored(error: &crate::PhaseError) -> Self {
        let cause = match error {
            crate::PhaseError::Docker { source } => Some(source.cause()),
            _ => None,
        };
        Self::Errored {
            code: error.code().to_owned(),
            message: error.to_string(),
            cause,
        }
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
