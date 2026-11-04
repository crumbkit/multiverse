//! The Docker calls that `sweep` makes, behind a trait. The container
//! module's `DockerCli` runs `docker`, and tests supply a fake.

use std::fmt;
use std::time::Duration;

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
pub(crate) trait Docker {
    /// Returns the id of `object` when it exists, or `None` when it does
    /// not. An object with a recorded id matches by that id. An object
    /// without one matches by its name, and only when it also carries
    /// phase's workspace label with `object.workspace`, so an object of
    /// another workspace or program that has the same name is never
    /// matched. A volume's id is its name.
    ///
    /// # Errors
    ///
    /// Returns [`DockerError::Unavailable`] when Docker does not answer.
    fn find(&self, object: &DockerObject<'_>) -> Result<Option<String>, DockerError>;

    /// Removes the object of `kind` whose id is `id`. A running container
    /// is stopped first, and is killed when it has not stopped after
    /// `wait`.
    ///
    /// # Errors
    ///
    /// Returns [`DockerError::Unavailable`] when Docker does not answer,
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
    },
    /// Docker answered and refused the call, such as a removal of a
    /// container that another program holds.
    #[non_exhaustive]
    Refused {
        /// Docker's answer.
        message: String,
    },
}

impl DockerError {
    /// Returns the failure of a Docker that did not answer, which `message`
    /// describes.
    #[must_use]
    pub const fn unavailable(message: String) -> Self {
        Self::Unavailable { message }
    }

    /// Returns the failure of a Docker that answered and refused, with its
    /// answer `message`.
    #[must_use]
    pub const fn refused(message: String) -> Self {
        Self::Refused { message }
    }
}

impl fmt::Display for DockerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { message } => write!(formatter, "Docker did not answer: {message}"),
            Self::Refused { message } => write!(formatter, "Docker refused: {message}"),
        }
    }
}

impl std::error::Error for DockerError {}
