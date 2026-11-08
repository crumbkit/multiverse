//! The failures of phase's operations.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crate::env::EnvironmentError;
use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::record::DockerError;

/// A failure of one of [`Phase`](crate::Phase)'s operations.
#[derive(Debug)]
#[non_exhaustive]
pub enum PhaseError {
    /// The operation is declared but phase does not carry it out yet.
    #[non_exhaustive]
    NotBuilt {
        /// The operation.
        operation: Operation,
    },
    /// A resolver could not supply a value of the app's environment, so
    /// phase started nothing.
    #[non_exhaustive]
    Environment {
        /// The app.
        app: AppName,
        /// The resolver's failure.
        source: EnvironmentError,
    },
    /// Another `up`, `down`, `restart` or `sweep` of the workspace held its
    /// lifecycle lock for all of the lock wait, so this operation did
    /// nothing.
    #[non_exhaustive]
    Busy {
        /// The workspace's stable id.
        workspace: WorkspaceId,
        /// How long the operation waited for the lock.
        waited: Duration,
    },
    /// The workspace's run record cannot be read, so phase does nothing to
    /// that workspace. Other workspaces are unaffected.
    #[non_exhaustive]
    RecordUnreadable {
        /// The workspace's stable id.
        workspace: WorkspaceId,
        /// The record's file.
        path: PathBuf,
        /// Why it cannot be read.
        reason: String,
    },
    /// A newer phase wrote the workspace's run record, so this phase does
    /// nothing to that workspace until it is upgraded.
    #[non_exhaustive]
    NewerRecord {
        /// The workspace's stable id.
        workspace: WorkspaceId,
        /// The record's file.
        path: PathBuf,
        /// The record's schema, such as `phase/run/v2`.
        schema: String,
    },
    /// phase cannot write a file of its state directory.
    #[non_exhaustive]
    StateUnwritable {
        /// The file or directory.
        path: PathBuf,
        /// The failure.
        source: io::Error,
    },
    /// phase cannot read a process's log file.
    #[non_exhaustive]
    LogUnreadable {
        /// The log file.
        path: PathBuf,
        /// The failure.
        source: io::Error,
    },
    /// phase cannot read the process table or signal a process.
    #[non_exhaustive]
    ProcessTable {
        /// What failed.
        message: String,
    },
    /// A Docker call failed: Docker did not answer, so phase cannot tell
    /// what is running, or it answered and refused.
    #[non_exhaustive]
    Docker {
        /// Docker's failure.
        source: DockerError,
    },
    /// The keeper of a host process could not start, refused its
    /// specification, or gave no handshake before the deadline, so the
    /// process did not start.
    #[non_exhaustive]
    KeeperUnstartable {
        /// The app.
        app: AppName,
        /// The process.
        process: ProcessName,
        /// Why, which holds no environment value.
        reason: String,
    },
    /// The keeper of a host process started, and could not start the
    /// process, for example because its program does not exist.
    #[non_exhaustive]
    ProcessUnstartable {
        /// The app.
        app: AppName,
        /// The process.
        process: ProcessName,
        /// Why, which holds no environment value.
        reason: String,
    },
    /// A container process that receives secrets as files would have them
    /// owned by root, because neither its plan's `secret_owner`, its app's
    /// run-as user nor its image names another user. An image that starts
    /// as root often switches to another user before it reads its secrets,
    /// and could not read them then.
    #[non_exhaustive]
    SecretOwnerUnknown {
        /// The app.
        app: AppName,
        /// The process.
        process: ProcessName,
    },
    /// phase could not choose a numeric owner for the workspace volumes
    /// that a container app mounts, so it started none of the app's
    /// containers. The user that the app's containers run as is a name,
    /// and phase resolves a name to numbers by running `id` in the app's
    /// image. The failure holds nothing that `id` printed.
    #[non_exhaustive]
    VolumeOwner {
        /// The app.
        app: AppName,
        /// The plan's run-as user that the app's containers run as, or
        /// `None` when the user came from the image's `USER`. The image's
        /// author controls that text, so phase does not repeat it.
        user: Option<String>,
        /// What went wrong.
        problem: VolumeOwnerProblem,
    },
    /// phase cannot run the app yet, because what it needs is a later
    /// release's work.
    #[non_exhaustive]
    NotRunnable {
        /// The app.
        app: AppName,
        /// What the app needs.
        reason: NotRunnable,
    },
    /// The selection names an app or a process that the plan lacks.
    #[non_exhaustive]
    UnknownTarget {
        /// The target as `APP` or `APP/PROCESS`.
        target: String,
    },
    /// `exec` into a container process found no running container of the
    /// process that the app's labels name, so it ran nothing. `exec` never
    /// starts a container: `up` does.
    #[non_exhaustive]
    NotRunning {
        /// The app.
        app: AppName,
        /// The process.
        process: ProcessName,
    },
    /// `exec` could not start the command in the context of a process, for
    /// example because its program does not exist or its sandbox cannot
    /// be built.
    #[non_exhaustive]
    ExecUnstartable {
        /// The app.
        app: AppName,
        /// The process whose context the command runs in.
        process: ProcessName,
        /// Why, which holds no environment value.
        reason: String,
    },
    /// `exec` refused a sandboxed host command under
    /// [`ExecIo::Inherit`](crate::ExecIo::Inherit) whose caller said its
    /// standard input is a terminal, or whose caller's standard input,
    /// output or error is one. A sandboxed command runs in a session of its
    /// own with no controlling terminal, so that it cannot push input into
    /// the caller's shell or read what the user types, and phase does not
    /// relay a terminal to it yet.
    #[non_exhaustive]
    ExecNeedsTerminal {
        /// The app.
        app: AppName,
        /// The process whose context the command would run in.
        process: ProcessName,
    },
    /// phase tried to record a change to an object that the run record does
    /// not name, which is a fault in phase.
    #[non_exhaustive]
    Unplanned {
        /// The workspace's stable id.
        workspace: WorkspaceId,
        /// The object.
        object: String,
    },
}

impl PhaseError {
    /// Returns the stable code of the failure, such as
    /// `phase::record::unreadable`, which a program can match on.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotBuilt { .. } => "phase::not_built",
            Self::Environment { .. } => "phase::environment",
            Self::Busy { .. } => "phase::lock::busy",
            Self::RecordUnreadable { .. } => "phase::record::unreadable",
            Self::NewerRecord { .. } => "phase::record::newer",
            Self::StateUnwritable { .. } => "phase::state::unwritable",
            Self::LogUnreadable { .. } => "phase::log::unreadable",
            Self::ProcessTable { .. } => "phase::process_table",
            Self::Docker {
                source: DockerError::Refused { .. },
            } => "phase::docker::refused",
            Self::Docker { .. } => "phase::docker::unavailable",
            Self::KeeperUnstartable { .. } => "phase::keeper::unstartable",
            Self::ProcessUnstartable { .. } => "phase::process::unstartable",
            Self::SecretOwnerUnknown { .. } => "phase::container::secret_owner_unknown",
            Self::VolumeOwner {
                problem: VolumeOwnerProblem::ResolvesToRoot,
                ..
            } => "phase::container::volume_owner_root",
            Self::VolumeOwner { .. } => "phase::container::volume_owner_unresolved",
            Self::NotRunnable { .. } => "phase::app::not_runnable",
            Self::UnknownTarget { .. } => "phase::select::unknown",
            Self::NotRunning { .. } => "phase::exec::not_running",
            Self::ExecUnstartable { .. } => "phase::exec::unstartable",
            Self::ExecNeedsTerminal { .. } => "phase::exec::sandboxed_terminal",
            Self::Unplanned { .. } => "phase::record::unplanned",
        }
    }
}

/// Why phase could not choose a numeric owner for an app's volumes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum VolumeOwnerProblem {
    /// The user's name resolves to uid 0 in the app's image. A name that
    /// means root may hide that the app runs as root, so phase gives a
    /// volume to root only when the plan's volume owner says `0:0`.
    ResolvesToRoot,
    /// Docker refused to create or start the helper that runs `id` in the
    /// app's image, as when the image has no `id` program or no such user.
    /// phase keeps none of Docker's text, because Docker's line can quote
    /// the image's `USER`.
    IdNotRun,
    /// `id` in the app's image exited with this status, so the name could
    /// not be resolved.
    #[non_exhaustive]
    IdFailed {
        /// The exit status of `id`.
        status: i64,
    },
    /// `id` in the app's image exited with status 0 and printed something
    /// other than one number. phase does not repeat what it printed.
    IdUnreadable,
}

/// What an app needs that phase cannot do yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum NotRunnable {
    /// The app's program comes from a release, which phase does not
    /// download and verify yet.
    Release,
    /// The app's program is built from its checkout, which phase does not
    /// do yet.
    CheckoutBuild,
    /// A process of the container app names a working directory, which
    /// phase does not set for a container yet.
    ContainerWorkingDirectory,
    /// The container app does not run from an image.
    ContainerWithoutImage,
    /// A process of the container app is a oneshot, which phase runs only
    /// on the host yet.
    Oneshot,
}

impl fmt::Display for NotRunnable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Release => "phase does not download and verify a release yet",
            Self::CheckoutBuild => "phase does not build a checkout's program yet",
            Self::ContainerWorkingDirectory => {
                "phase does not set a container's working directory yet"
            }
            Self::ContainerWithoutImage => "the container app does not run from an image",
            Self::Oneshot => "phase does not run a oneshot process in a container yet",
        })
    }
}

/// One of [`Phase`](crate::Phase)'s operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Operation {
    /// [`Phase::up`](crate::Phase::up).
    Up,
    /// [`Phase::down`](crate::Phase::down).
    Down,
    /// [`Phase::status`](crate::Phase::status).
    Status,
    /// [`Phase::logs`](crate::Phase::logs).
    Logs,
    /// [`Phase::sweep`](crate::Phase::sweep).
    Sweep,
    /// [`Phase::exec`](crate::Phase::exec).
    Exec,
    /// [`Phase::restart`](crate::Phase::restart).
    Restart,
}

impl fmt::Display for Operation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Status => "status",
            Self::Logs => "logs",
            Self::Sweep => "sweep",
            Self::Exec => "exec",
            Self::Restart => "restart",
        })
    }
}

impl fmt::Display for PhaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotBuilt { operation } => {
                write!(
                    formatter,
                    "phase cannot {operation} yet, because it is not built"
                )
            }
            Self::Environment { app, .. } => {
                write!(formatter, "the environment of {app} could not be resolved")
            }
            Self::Busy { workspace, waited } => write!(
                formatter,
                "another operation held the lifecycle lock of the workspace {workspace} for {} ms",
                waited.as_millis()
            ),
            Self::RecordUnreadable {
                workspace,
                path,
                reason,
            } => write!(
                formatter,
                "the run record of the workspace {workspace} at {} cannot be read: {reason}",
                path.display()
            ),
            Self::NewerRecord {
                workspace, schema, ..
            } => write!(
                formatter,
                "a newer phase wrote the run record of the workspace {workspace} as {schema}; upgrade phase to manage it"
            ),
            Self::StateUnwritable { path, .. } => {
                write!(formatter, "phase cannot write {}", path.display())
            }
            Self::LogUnreadable { path, .. } => {
                write!(formatter, "phase cannot read the log {}", path.display())
            }
            Self::ProcessTable { message } => {
                write!(
                    formatter,
                    "phase cannot read or signal processes: {message}"
                )
            }
            Self::Docker {
                source: DockerError::Refused { message, .. },
            } => write!(formatter, "Docker refused a request of the run: {message}"),
            Self::Docker {
                source: DockerError::Unavailable { message, .. },
            } => write!(
                formatter,
                "Docker did not answer, so phase cannot tell what is running: {message}"
            ),
            Self::KeeperUnstartable {
                app,
                process,
                reason,
            } => write!(
                formatter,
                "the keeper of {app}/{process} could not start: {reason}"
            ),
            Self::ProcessUnstartable {
                app,
                process,
                reason,
            } => write!(
                formatter,
                "the process {app}/{process} could not start: {reason}"
            ),
            Self::SecretOwnerUnknown { app, process } => write!(
                formatter,
                "the secret files of {app}/{process} would belong to root; set its secret_owner to the uid:gid the app reads them as, such as 999:999 for an image that switches to user 999, or set its secret_delivery = \"env\""
            ),
            Self::VolumeOwner { app, user, problem } => {
                let user = match user {
                    Some(user) => {
                        format!("the user {user:?} that the containers of {app} run as")
                    }
                    None => format!(
                        "the user named by the image's USER, which the containers of {app} run as,"
                    ),
                };
                match problem {
                    VolumeOwnerProblem::ResolvesToRoot => write!(
                        formatter,
                        "{user} is uid 0 in its image, so phase does not give the app's volumes to it"
                    )?,
                    VolumeOwnerProblem::IdNotRun => write!(
                        formatter,
                        "phase could not resolve {user} to a numeric uid and gid: Docker could not run id in the app's image, as when the image has no id program or no such user"
                    )?,
                    VolumeOwnerProblem::IdFailed { status } => write!(
                        formatter,
                        "phase could not resolve {user} to a numeric uid and gid: id in the app's image exited with status {status}"
                    )?,
                    VolumeOwnerProblem::IdUnreadable => write!(
                        formatter,
                        "phase could not resolve {user} to a numeric uid and gid: id in the app's image did not print one number"
                    )?,
                }
                formatter.write_str(
                    "; set the app's volume owner to a numeric uid:gid such as 999:999, or to keep to leave its volumes as they are",
                )
            }
            Self::NotRunnable { app, reason } => {
                write!(formatter, "phase cannot run {app}: {reason}")
            }
            Self::UnknownTarget { target } => {
                write!(formatter, "the plan has no {target}")
            }
            Self::NotRunning { app, process } => write!(
                formatter,
                "the container of {app}/{process} is not running, so phase cannot run a command in it; start it with up first"
            ),
            Self::ExecUnstartable {
                app,
                process,
                reason,
            } => write!(
                formatter,
                "the command in the context of {app}/{process} could not start: {reason}"
            ),
            Self::ExecNeedsTerminal { app, process } => write!(
                formatter,
                "{app}/{process} runs under the sandbox, and a sandboxed command cannot have the terminal yet; run it with no terminal on its standard input, output or error, since a relay of the terminal is not built yet"
            ),
            Self::Unplanned { workspace, object } => write!(
                formatter,
                "the run record of the workspace {workspace} does not name {object}"
            ),
        }
    }
}

impl std::error::Error for PhaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Environment { source, .. } => Some(source),
            Self::StateUnwritable { source, .. } | Self::LogUnreadable { source, .. } => {
                Some(source)
            }
            Self::NotBuilt { .. }
            | Self::Busy { .. }
            | Self::RecordUnreadable { .. }
            | Self::NewerRecord { .. }
            | Self::ProcessTable { .. }
            | Self::Docker { .. }
            | Self::KeeperUnstartable { .. }
            | Self::ProcessUnstartable { .. }
            | Self::SecretOwnerUnknown { .. }
            | Self::VolumeOwner { .. }
            | Self::NotRunnable { .. }
            | Self::UnknownTarget { .. }
            | Self::NotRunning { .. }
            | Self::ExecUnstartable { .. }
            | Self::ExecNeedsTerminal { .. }
            | Self::Unplanned { .. } => None,
        }
    }
}
