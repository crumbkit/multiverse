//! The Docker calls that `sweep` makes, behind a trait. The container
//! module's `DockerCli` runs `docker`, and tests supply a fake.

use std::borrow::Cow;
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::engine::container::ContainerState;
use crate::name::WorkspaceId;
use crate::record::DockerName;

/// A kind of Docker object that the run record names, and that a
/// [`DockerError`] about a foreign object names. In JSON a kind is its name
/// in snake case, such as `"volume"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DockerKind {
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

    /// Returns the state of the container `object` and how many times
    /// Docker has restarted it, matched as [`Docker::find`] matches it, or
    /// `None` when the daemon answered that it does not exist or that it
    /// exists without this workspace's labels. `status` calls it, so a
    /// container's state comes from Docker and never from the record
    /// alone.
    ///
    /// # Errors
    ///
    /// Returns [`DockerError::Unavailable`] when Docker does not answer.
    fn container_seen(
        &self,
        object: &DockerObject<'_>,
    ) -> Result<Option<SeenContainer>, DockerError>;

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

/// What Docker reports of a container: its state, how many times Docker
/// has restarted it under its restart policy, and when Docker last started
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SeenContainer {
    /// The container's state.
    pub(crate) state: ContainerState,
    /// How many times Docker has restarted the container under its restart
    /// policy, or `None` when phase cannot read the count. A `docker
    /// restart` and a start after a stop do not add to it.
    pub(crate) restarts: Option<u32>,
    /// When Docker last started the container, exactly as it printed
    /// `State.StartedAt`, or `None` when it printed nothing. Every start
    /// changes it, including one that phase did not ask for.
    pub(crate) started_at: Option<String>,
}

impl SeenContainer {
    /// Returns a container in `state` that Docker has restarted `restarts`
    /// times and last started at `started_at`.
    pub(crate) const fn new(
        state: ContainerState,
        restarts: Option<u32>,
        started_at: Option<String>,
    ) -> Self {
        Self {
            state,
            restarts,
            started_at,
        }
    }
}

/// A failure of a Docker call.
///
/// Beside its message and cause, it names what phase was doing, so a
/// program can match on those facts instead of reading the message: the
/// docker command or step ([`DockerError::operation`]), the container it
/// worked on, by its compose service ([`DockerError::service`]) or its id
/// ([`DockerError::container_id`]), and for a foreign object, its
/// kind and name ([`DockerError::foreign`]). None of them holds a value of
/// a variable. [`DockerError::docker_line`] gives Docker's own error line
/// apart from phase's sentence, when Docker printed one.
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
        /// What phase was doing when Docker failed, boxed so that the
        /// error stays small.
        context: Box<DockerContext>,
    },
    /// Docker answered and refused the call, such as a removal of a
    /// container that another program holds.
    #[non_exhaustive]
    Refused {
        /// Docker's answer.
        message: String,
        /// What kind of failure it is.
        cause: DockerCause,
        /// What phase was doing when Docker failed, boxed so that the
        /// error stays small.
        context: Box<DockerContext>,
    },
}

/// What phase was doing when a Docker call failed, and the line that Docker
/// printed for its own error. It holds names and Docker's own line only,
/// and no value of a variable.
///
/// In JSON each field is left out when it is `None`, and a field that this
/// version does not know is ignored, so a report from a newer phase still
/// parses.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DockerContext {
    /// The docker command or step as phase names it, such as `pull`,
    /// `compose up`, `stop`, `rm`, `container inspect`, `logs` or `secret
    /// write`, or `None` when phase did not name one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<Cow<'static, str>>,
    /// The compose service of the container that the command worked on,
    /// which is the name of its process in the plan, such as `server`, or
    /// `None`. `up`, `down` and the secret writes name a container this
    /// way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// The 64-hex-digit id of the container that the command worked on, or
    /// `None`. `sweep`, which knows a container only by the id the run
    /// record keeps, names a container this way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
    /// The object whose labels do not name phase's scope, for a failure
    /// whose cause is [`DockerCause::Foreign`], or `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreign: Option<ForeignObject>,
    /// The line that Docker printed for its own error, such as `Error
    /// response from daemon: ... port is already allocated`, kept to
    /// [`MOST_DOCKER_MESSAGE_BYTES`] bytes, or `None`.
    ///
    /// It comes only from a docker command's standard error, and only
    /// from a line in one of Docker's own error forms: `Error response
    /// from daemon:`, `error from daemon in stream:`, `Error: No such
    /// container:`, `Cannot connect to the Docker daemon` or `failed to
    /// connect to the docker API at`. `docker compose up --detach` gives
    /// its last line of standard error in one of these forms. `docker
    /// logs` gives such a line only when it is the last of standard error
    /// and docker then exits with code 1. Every other command, including
    /// the `docker exec` of a secret write, gives its last line only when
    /// every line of its standard error is in one of these forms. Any
    /// other failure leaves it `None`, so a line that a container or an
    /// app wrote, which may hold a credential, does not become it. The
    /// section on containers in phase's design says more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_line: Option<String>,
}

/// A Docker object that has a name of phase's and whose labels do not name
/// phase's scope, so phase leaves it alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ForeignObject {
    /// The object's kind.
    pub kind: DockerKind,
    /// The object's Docker name.
    pub name: String,
}

impl ForeignObject {
    /// Returns the object's kind.
    #[must_use]
    pub const fn kind(&self) -> DockerKind {
        self.kind
    }

    /// Returns the object's Docker name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// What kind of failure a [`DockerError`] is, which a program can match on
/// instead of reading the message. A cause holds no value of a variable,
/// and no text of Docker's.
///
/// In JSON a cause is its name in snake case, such as `"pull_failed"` or
/// `"daemon_unavailable"`, and these names are stable. A name that this
/// version does not know reads as [`DockerCause::Unknown`], so a report
/// from a newer phase still parses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// A cause that this version of phase does not know, read from JSON
    /// that a newer phase wrote. phase never gives it to a failure.
    /// Serializing it again writes `"unknown"`, so the cause's original
    /// name is not kept.
    #[serde(other)]
    Unknown,
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
            context: Box::default(),
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
            context: Box::default(),
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

    /// Returns what phase was doing when Docker failed.
    #[must_use]
    pub const fn context(&self) -> &DockerContext {
        match self {
            Self::Unavailable { context, .. } | Self::Refused { context, .. } => context,
        }
    }

    /// Returns the docker command or step that failed, as phase names it,
    /// such as `pull`, `compose up`, `stop`, `rm`, `container inspect`,
    /// `logs` or `secret write`, or `None` when phase did not name one.
    #[must_use]
    pub fn operation(&self) -> Option<&str> {
        self.context().operation.as_deref()
    }

    /// Returns the compose service of the container that the failed
    /// command worked on, which is its process's name, or `None`.
    #[must_use]
    pub fn service(&self) -> Option<&str> {
        self.context().service.as_deref()
    }

    /// Returns the id of the container that the failed command worked on,
    /// or `None`.
    #[must_use]
    pub fn container_id(&self) -> Option<&str> {
        self.context().container_id.as_deref()
    }

    /// Returns the object whose labels do not name phase's scope, for a
    /// failure whose cause is [`DockerCause::Foreign`], or `None`.
    #[must_use]
    pub const fn foreign(&self) -> Option<&ForeignObject> {
        self.context().foreign.as_ref()
    }

    /// Returns the line that Docker printed for its own error, apart from
    /// phase's sentence, such as `Error response from daemon: ... port is
    /// already allocated`, or `None`. [`DockerContext::docker_line`] says
    /// which lines it can be.
    #[must_use]
    pub fn docker_line(&self) -> Option<&str> {
        self.context().docker_line.as_deref()
    }

    /// Returns this failure with `line` as Docker's own error line, kept
    /// to [`MOST_DOCKER_MESSAGE_BYTES`] bytes. Its message and cause stay.
    /// The caller passes only a line that [`DockerContext::docker_line`]
    /// allows.
    #[must_use]
    pub(crate) fn with_docker_line(mut self, line: Option<String>) -> Self {
        self.context_mut().docker_line = line.map(bounded);
        self
    }

    /// Returns a mutable view of what phase was doing.
    const fn context_mut(&mut self) -> &mut DockerContext {
        match self {
            Self::Unavailable { context, .. } | Self::Refused { context, .. } => context,
        }
    }

    /// Returns this failure as one of the docker command or step
    /// `operation` on the container of the compose service `service`. Its
    /// message and cause stay.
    #[must_use]
    pub(crate) fn during(mut self, operation: &'static str, service: Option<&str>) -> Self {
        let context = self.context_mut();
        context.operation = Some(Cow::Borrowed(operation));
        context.service = service.map(str::to_owned);
        self
    }

    /// Returns this failure as one of the docker command or step
    /// `operation` on the container whose id is `id`. Its message and cause
    /// stay.
    #[must_use]
    pub(crate) fn during_on_id(mut self, operation: &'static str, id: Option<&str>) -> Self {
        let context = self.context_mut();
        context.operation = Some(Cow::Borrowed(operation));
        context.container_id = id.map(str::to_owned);
        self
    }

    /// Returns this failure as one about the foreign object of `kind`
    /// named `name`. Its message and cause stay.
    #[must_use]
    pub(crate) fn about_foreign(mut self, kind: DockerKind, name: &str) -> Self {
        self.context_mut().foreign = Some(ForeignObject {
            kind,
            name: name.to_owned(),
        });
        self
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
