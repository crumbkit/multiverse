//! The failures of the container engine's own steps: running `docker`,
//! checking an object's labels, and reading the run directory.
//!
//! The host program converts a [`ContainerError`] into its own error type
//! through [`super::Host::Error`], so each failure keeps the code and the
//! message that the host gives it.

use std::fmt;
use std::io;
use std::path::PathBuf;

use crate::record::{DockerCause, DockerError, DockerKind};

/// A failure of a step of the container engine.
#[derive(Debug)]
#[non_exhaustive]
pub enum ContainerError {
    /// No `docker` program is on `PATH`.
    #[non_exhaustive]
    Missing {
        /// Why the program could not start.
        source: io::Error,
    },
    /// The `docker` program runs, and its `compose` plugin is not
    /// installed.
    #[non_exhaustive]
    ComposeMissing {
        /// The line of docker's standard error that says what failed.
        message: String,
    },
    /// docker has no free address pool for a network, so it cannot connect
    /// a container.
    #[non_exhaustive]
    NoAddressPool {
        /// The line of docker's standard error that says what failed.
        message: String,
        /// The line of docker's standard error in one of Docker's own
        /// error forms, which [`crate::DockerContext::docker_line`]
        /// describes, or `None`.
        docker_line: Option<String>,
    },
    /// The `docker` program runs, and its daemon does not answer.
    #[non_exhaustive]
    Unavailable {
        /// The line of docker's standard error that says what failed.
        message: String,
        /// The line of docker's standard error in one of Docker's own
        /// error forms, which [`crate::DockerContext::docker_line`]
        /// describes, or `None`.
        docker_line: Option<String>,
    },
    /// A docker command failed or missed its deadline, or a step of the
    /// engine around it failed.
    #[non_exhaustive]
    Failed {
        /// The docker command or step, such as `compose up`.
        operation: &'static str,
        /// The compose service of the container that the command worked
        /// on, which is its process's name, such as `server`, when it
        /// worked on one. It is never a container's id or Docker name.
        service: Option<String>,
        /// The line of docker's standard error that says what failed.
        message: String,
        /// What kind of failure it is: [`DockerCause::Refused`],
        /// [`DockerCause::PullFailed`], [`DockerCause::TimedOut`] or
        /// [`DockerCause::Unexpected`].
        cause: DockerCause,
        /// The line of docker's standard error in one of Docker's own
        /// error forms, which [`crate::DockerContext::docker_line`]
        /// describes, or `None`.
        docker_line: Option<String>,
    },
    /// The caller cancelled the operation while a docker command ran, so
    /// the engine stopped the command.
    #[non_exhaustive]
    Cancelled {
        /// The docker command, such as `pull`.
        operation: &'static str,
    },
    /// A docker container or volume has a name of the scope, or is one that
    /// the run directory records, and its labels do not name the scope and
    /// the object, so the engine leaves it alone.
    #[non_exhaustive]
    Foreign {
        /// `container` or `volume`.
        kind: &'static str,
        /// The object's docker name.
        name: String,
    },
    /// A directory of the run directory cannot be read.
    #[non_exhaustive]
    Unreadable {
        /// The directory.
        path: PathBuf,
        /// The failure.
        source: io::Error,
    },
}

impl ContainerError {
    /// Returns the failure of the docker command `operation` on the
    /// container of the compose service `service`, which docker answered
    /// with the error `message`.
    pub(crate) fn failed(
        operation: &'static str,
        service: Option<&str>,
        message: impl Into<String>,
    ) -> Self {
        Self::failure(operation, service, message, DockerCause::Refused)
    }

    /// Returns the failure of the docker command `operation`, which did
    /// not finish within its deadline, as `message` says.
    pub(crate) fn timed_out(operation: &'static str, message: impl Into<String>) -> Self {
        Self::failure(operation, None, message, DockerCause::TimedOut)
    }

    /// Returns the failure of the step `operation` on the container of
    /// `service` around a docker command, which `message` describes, such
    /// as a check of a scope or a read of what docker printed.
    pub(crate) fn unexpected(
        operation: &'static str,
        service: Option<&str>,
        message: impl Into<String>,
    ) -> Self {
        Self::failure(operation, service, message, DockerCause::Unexpected)
    }

    /// Returns the failure of `operation` on the container of `service`
    /// with `cause`.
    fn failure(
        operation: &'static str,
        service: Option<&str>,
        message: impl Into<String>,
        cause: DockerCause,
    ) -> Self {
        Self::Failed {
            operation,
            service: service.map(str::to_owned),
            message: message.into(),
            cause,
            docker_line: None,
        }
    }

    /// Returns this failure with `line` as Docker's own error line, which
    /// the caller took from a docker command's standard error under the
    /// rules of [`crate::DockerContext::docker_line`]. A failure that holds
    /// no line of docker's standard error stays as it is.
    #[must_use]
    pub(crate) fn with_docker_line(mut self, line: Option<String>) -> Self {
        if let Self::Unavailable { docker_line, .. }
        | Self::NoAddressPool { docker_line, .. }
        | Self::Failed { docker_line, .. } = &mut self
        {
            *docker_line = line;
        }
        self
    }

    /// Returns the line of docker's standard error in one of Docker's own
    /// error forms that this failure holds, or `None`.
    #[must_use]
    pub fn docker_line(&self) -> Option<&str> {
        match self {
            Self::Unavailable { docker_line, .. }
            | Self::NoAddressPool { docker_line, .. }
            | Self::Failed { docker_line, .. } => docker_line.as_deref(),
            Self::Missing { .. }
            | Self::ComposeMissing { .. }
            | Self::Cancelled { .. }
            | Self::Foreign { .. }
            | Self::Unreadable { .. } => None,
        }
    }

    /// Returns this failure of a pull of an image: a refusal becomes
    /// [`DockerCause::PullFailed`], and any other failure stays as it is.
    #[must_use]
    pub(crate) fn in_pull(mut self) -> Self {
        if let Self::Failed { cause, .. } = &mut self
            && *cause == DockerCause::Refused
        {
            *cause = DockerCause::PullFailed;
        }
        self
    }

    /// Returns what kind of failure this is. A failure to read the run
    /// directory is [`DockerCause::Unexpected`].
    #[must_use]
    pub const fn cause(&self) -> DockerCause {
        match self {
            Self::Missing { .. } => DockerCause::DockerMissing,
            Self::ComposeMissing { .. } => DockerCause::ComposeMissing,
            Self::NoAddressPool { .. } => DockerCause::NoAddressPool,
            Self::Unavailable { .. } => DockerCause::DaemonUnavailable,
            Self::Failed { cause, .. } => *cause,
            Self::Cancelled { .. } => DockerCause::Cancelled,
            Self::Foreign { .. } => DockerCause::Foreign,
            Self::Unreadable { .. } => DockerCause::Unexpected,
        }
    }

    /// Returns `error` with what this failure says phase was doing: the
    /// docker command or step and its container, or the foreign object,
    /// and with Docker's own error line when this failure holds one.
    pub(crate) fn describe(&self, error: DockerError) -> DockerError {
        let error = error.with_docker_line(self.docker_line().map(str::to_owned));
        match self {
            Self::Failed {
                operation, service, ..
            } => error.during(operation, service.as_deref()),
            Self::Cancelled { operation } => error.during(operation, None),
            Self::Foreign { kind, name } => match foreign_kind(kind) {
                Some(kind) => error.about_foreign(kind, name),
                None => error,
            },
            Self::Missing { .. }
            | Self::ComposeMissing { .. }
            | Self::NoAddressPool { .. }
            | Self::Unavailable { .. }
            | Self::Unreadable { .. } => error,
        }
    }

    /// Returns the failure of an object of `kind` named `name` whose labels
    /// do not name the scope.
    pub(crate) fn foreign(kind: &'static str, name: &str) -> Self {
        Self::Foreign {
            kind,
            name: name.to_owned(),
        }
    }
}

/// Returns the kind of object that [`ContainerError::Foreign`] names as
/// `kind`, or `None` for a kind that phase does not label.
fn foreign_kind(kind: &str) -> Option<DockerKind> {
    match kind {
        "container" => Some(DockerKind::Container),
        "network" => Some(DockerKind::Network),
        "volume" => Some(DockerKind::Volume),
        _ => None,
    }
}

impl fmt::Display for ContainerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { .. } => formatter.write_str("docker is not installed"),
            Self::ComposeMissing { message } => {
                write!(
                    formatter,
                    "docker's compose plugin is not installed: {message}"
                )
            }
            Self::NoAddressPool { message, .. } => {
                write!(formatter, "docker has no free address pool: {message}")
            }
            Self::Unavailable { message, .. } => {
                write!(formatter, "the docker daemon does not answer: {message}")
            }
            Self::Failed {
                operation,
                service,
                message,
                ..
            } => {
                write!(formatter, "docker {operation} failed")?;
                if let Some(service) = service {
                    write!(formatter, " for the container {service}")?;
                }
                write!(formatter, ": {message}")
            }
            Self::Cancelled { operation } => {
                write!(formatter, "docker {operation} was cancelled")
            }
            Self::Foreign { kind, name } => write!(
                formatter,
                "the labels of the docker {kind} {name} do not name its scope"
            ),
            Self::Unreadable { path, .. } => {
                write!(formatter, "cannot read {}", path.display())
            }
        }
    }
}

impl std::error::Error for ContainerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Missing { source } | Self::Unreadable { source, .. } => Some(source),
            Self::ComposeMissing { .. }
            | Self::NoAddressPool { .. }
            | Self::Unavailable { .. }
            | Self::Failed { .. }
            | Self::Cancelled { .. }
            | Self::Foreign { .. } => None,
        }
    }
}
