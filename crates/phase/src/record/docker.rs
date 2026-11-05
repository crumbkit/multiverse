//! The Docker calls that `sweep` makes, behind a trait. The container
//! module's `DockerCli` runs `docker`, and tests supply a fake.

use std::fmt;
use std::time::Duration;

use crate::engine::container::ContainerState;
use crate::name::WorkspaceId;
use crate::record::DockerName;

/// A kind of Docker object that the run record names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DockerKind {
    /// A container.
    Container,
    /// A network.
    Network,
    /// A volume.
    Volume,
}

/// A Docker object that the run record names: by its id once phase has
/// recorded one, and otherwise by the name that phase gave it and the
/// workspace label that phase put on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DockerObject<'r> {
    /// The workspace whose stable id phase put in the object's workspace
    /// label.
    pub(crate) workspace: &'r WorkspaceId,
    /// The object's kind.
    pub(crate) kind: DockerKind,
    /// The name that phase gave the object.
    pub(crate) name: &'r DockerName,
    /// The object's recorded id. A volume has none, because its name
    /// identifies it.
    pub(crate) id: Option<&'r str>,
}

/// The Docker calls of `sweep`. Each call fails with
/// [`DockerError::Unavailable`] when Docker does not answer, and never
/// reports an object as gone because Docker did not answer.
///
/// Only Docker's daemon can say that an object does not exist. A call
/// treats an object as gone only when the daemon itself answered that it
/// does not have it, such as `Error response from daemon: No such
/// container`. Every other failure is an error: an unknown Docker
/// context, a daemon that does not answer and a missing `docker` program
/// all fail with [`DockerError::Unavailable`].
pub(crate) trait Docker {
    /// Returns the id of `object` when it exists, or `None` when the
    /// daemon answered that it does not exist or that it exists without
    /// this workspace's labels. An object with a recorded id matches by
    /// that id. An object
    /// without one matches by its name, and only when it also carries
    /// phase's workspace label with `object.workspace`, so an object of
    /// another workspace or program that has the same name is never
    /// matched. A volume's id is its name.
    ///
    /// # Errors
    ///
    /// Returns [`DockerError::Unavailable`] when Docker does not answer,
    /// including when its context is unknown or its program is missing.
    fn find(&self, object: &DockerObject<'_>) -> Result<Option<String>, DockerError>;

    /// Returns the state of the container `object`, matched as
    /// [`Docker::find`] matches it, or `None` when the daemon answered that
    /// it does not exist or that it exists without this workspace's labels.
    /// `status` calls it, so a container's state comes from Docker and never
    /// from the record alone.
    ///
    /// # Errors
    ///
    /// Returns [`DockerError::Unavailable`] when Docker does not answer.
    fn container_state(
        &self,
        object: &DockerObject<'_>,
    ) -> Result<Option<ContainerState>, DockerError>;

    /// Removes the object of `kind` whose id is `id`. A running container
    /// is stopped first, and is killed when it has not stopped after
    /// `wait`. An object that the daemon answers it does not have counts
    /// as removed.
    ///
    /// # Errors
    ///
    /// Returns [`DockerError::Unavailable`] when Docker does not answer,
    /// including when its context is unknown or its program is missing,
    /// and [`DockerError::Refused`] when it answers and refuses.
    fn remove(&self, kind: DockerKind, id: &str, wait: Duration) -> Result<(), DockerError>;
}

/// A failure of a Docker call.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DockerError {
    /// Docker did not answer, so phase cannot tell what exists.
    #[non_exhaustive]
    Unavailable {
        /// What failed.
        message: String,
        /// What kind of failure it is.
        cause: DockerCause,
    },
    /// Docker answered and refused the call, such as a removal of a
    /// container that another program holds.
    #[non_exhaustive]
    Refused {
        /// Docker's answer.
        message: String,
        /// What kind of failure it is.
        cause: DockerCause,
    },
}

/// What kind of failure a [`DockerError`] is, which a program can match on
/// instead of reading the message. A cause holds no value of a variable,
/// and no text of Docker's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DockerCause {
    /// No `docker` program is on `PATH`.
    DockerMissing,
    /// The `docker` program runs, and its `compose` plugin is not
    /// installed.
    ComposeMissing,
    /// The `docker` program runs, and its daemon does not answer, or its
    /// context is unknown.
    DaemonUnavailable,
    /// Docker has no free address pool for a network, so it cannot
    /// connect a container.
    NoAddressPool,
    /// Docker answered a pull of the app's image with an error, such as an
    /// image that the registry does not have.
    PullFailed,
    /// Docker answered a command with an error.
    Refused,
    /// A docker command did not finish within its deadline, so phase
    /// stopped it.
    TimedOut,
    /// The caller cancelled the operation while a docker command ran, so
    /// phase stopped the command.
    Cancelled,
    /// A docker object has a name of phase's, and its labels do not name
    /// the scope, so phase leaves it alone.
    Foreign,
    /// A step of phase around a docker command failed, such as a check of
    /// a scope or a read of the run directory, or docker printed what
    /// phase cannot read.
    Unexpected,
}

/// The most bytes of Docker's message that a [`DockerError`] keeps. A
/// longer message keeps its first bytes up to a character boundary and
/// ends with `...`, so a report or an error stays short.
pub const MOST_DOCKER_MESSAGE_BYTES: usize = 2048;

/// The marker at the end of a message that was cut short.
const CUT: &str = "...";

/// Returns `message`, kept to [`MOST_DOCKER_MESSAGE_BYTES`] bytes.
///
/// Docker's messages may echo the arguments of the command that failed,
/// such as a container's name or id, a volume's name or a path. phase
/// never puts a value of a variable in an argument, a compose file or a
/// label: a value reaches only the environment of a `docker compose up`
/// child, under a private reference that the compose file names. So a
/// message that echoes what phase sent holds no value.
fn bounded(mut message: String) -> String {
    if message.len() <= MOST_DOCKER_MESSAGE_BYTES {
        return message;
    }
    let mut end = MOST_DOCKER_MESSAGE_BYTES - CUT.len();
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message.push_str(CUT);
    message
}

impl DockerError {
    /// Returns the failure of a Docker that did not answer, which `message`
    /// describes, kept to [`MOST_DOCKER_MESSAGE_BYTES`] bytes. Its cause is
    /// [`DockerCause::DaemonUnavailable`].
    #[must_use]
    pub fn unavailable(message: String) -> Self {
        Self::Unavailable {
            message: bounded(message),
            cause: DockerCause::DaemonUnavailable,
        }
    }

    /// Returns the failure of a Docker that answered and refused, with its
    /// answer `message`, kept to [`MOST_DOCKER_MESSAGE_BYTES`] bytes. Its
    /// cause is [`DockerCause::Refused`].
    #[must_use]
    pub fn refused(message: String) -> Self {
        Self::Refused {
            message: bounded(message),
            cause: DockerCause::Refused,
        }
    }

    /// Returns this failure with the cause `cause`. Its message stays.
    #[must_use]
    pub(crate) fn with_cause(mut self, cause: DockerCause) -> Self {
        match &mut self {
            Self::Unavailable { cause: kept, .. } | Self::Refused { cause: kept, .. } => {
                *kept = cause;
            }
        }
        self
    }

    /// Returns what kind of failure this is.
    #[must_use]
    pub const fn cause(&self) -> DockerCause {
        match self {
            Self::Unavailable { cause, .. } | Self::Refused { cause, .. } => *cause,
        }
    }
}

impl fmt::Display for DockerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { message, .. } => {
                write!(formatter, "Docker did not answer: {message}")
            }
            Self::Refused { message, .. } => write!(formatter, "Docker refused: {message}"),
        }
    }
}

impl std::error::Error for DockerError {}
