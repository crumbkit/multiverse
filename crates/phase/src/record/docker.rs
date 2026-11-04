//! The Docker calls that `sweep` makes, behind a trait. The container
//! module supplies the implementation that runs `docker`, and tests supply
//! a fake.

use std::fmt;
use std::time::Duration;

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
/// recorded one, and otherwise by the name that phase gave it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DockerObject<'r> {
    /// The object's kind.
    pub(crate) kind: DockerKind,
    /// The name that phase gave the object.
    pub(crate) name: &'r DockerName,
    /// The object's recorded id. A volume has none, because its name
    /// identifies it.
    pub(crate) id: Option<&'r str>,
}

/// The Docker calls of `sweep`. Each call fails with [`DockerError`] when
/// Docker does not answer, and never reports an object as gone because
/// Docker did not answer.
pub(crate) trait Docker {
    /// Returns the id of `object` when it exists, matched by its recorded
    /// id when it has one and by its name otherwise, or `None` when it does
    /// not exist. A volume's id is its name.
    ///
    /// # Errors
    ///
    /// Returns [`DockerError`] when Docker does not answer.
    fn find(&self, object: &DockerObject<'_>) -> Result<Option<String>, DockerError>;

    /// Removes the object of `kind` whose id is `id`. A running container
    /// is stopped first, and is killed when it has not stopped after
    /// `wait`.
    ///
    /// # Errors
    ///
    /// Returns [`DockerError`] when Docker does not answer or refuses.
    fn remove(&self, kind: DockerKind, id: &str, wait: Duration) -> Result<(), DockerError>;
}

/// A failure of a Docker call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DockerError {
    /// What failed.
    message: String,
}

impl DockerError {
    /// Returns the failure that `message` describes.
    #[must_use]
    pub const fn new(message: String) -> Self {
        Self { message }
    }
}

impl fmt::Display for DockerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DockerError {}

/// The Docker that `sweep` uses until the container module supplies the
/// real one. Every call fails, so a record that names a Docker object makes
/// `sweep` fail, and `sweep` never reports such an object as gone.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Unreachable;

impl Unreachable {
    /// Returns the failure of every call.
    fn failure() -> DockerError {
        DockerError::new(
            "phase cannot call Docker yet, because its container module is not built".to_owned(),
        )
    }
}

impl Docker for Unreachable {
    fn find(&self, _object: &DockerObject<'_>) -> Result<Option<String>, DockerError> {
        Err(Self::failure())
    }

    fn remove(&self, _kind: DockerKind, _id: &str, _wait: Duration) -> Result<(), DockerError> {
        Err(Self::failure())
    }
}
