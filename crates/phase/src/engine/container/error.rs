//! The failures of the container engine's own steps: running `docker`,
//! checking an object's labels, and reading the run directory.
//!
//! The host program converts a [`ContainerError`] into its own error type
//! through [`super::Host::Error`], so each failure keeps the code and the
//! message that the host gives it.

use std::fmt;
use std::io;
use std::path::PathBuf;

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
    },
    /// The `docker` program runs, and its daemon does not answer.
    #[non_exhaustive]
    Unavailable {
        /// The line of docker's standard error that says what failed.
        message: String,
    },
    /// A docker command failed or missed its deadline, or a step of the
    /// engine around it failed.
    #[non_exhaustive]
    Failed {
        /// The docker command or step, such as `compose up`.
        operation: &'static str,
        /// The name of the container that the command worked on, when it
        /// worked on one.
        container: Option<String>,
        /// The line of docker's standard error that says what failed.
        message: String,
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
    /// Returns the failure of the step `operation` on `container`, which
    /// `message` describes.
    pub(crate) fn failed(
        operation: &'static str,
        container: Option<&str>,
        message: impl Into<String>,
    ) -> Self {
        Self::Failed {
            operation,
            container: container.map(str::to_owned),
            message: message.into(),
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
            Self::NoAddressPool { message } => {
                write!(formatter, "docker has no free address pool: {message}")
            }
            Self::Unavailable { message } => {
                write!(formatter, "the docker daemon does not answer: {message}")
            }
            Self::Failed {
                operation,
                container,
                message,
            } => {
                write!(formatter, "docker {operation} failed")?;
                if let Some(container) = container {
                    write!(formatter, " for the container {container}")?;
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
