//! The Docker of phase's `sweep`, which runs the `docker` program.
//!
//! It matches an object by phase's workspace label, `mxv.workspace-id`.
//! An object that mxv created before it wrote that label carries only
//! `mxv.store` and `mxv.workspace`, and matches when a scope of those
//! labels is given with [`DockerCli::with_legacy`]. An object that carries
//! no mxv label never matches, even by a recorded id, so `sweep` never
//! removes it. Labels are read as JSON and compared exactly.
//!
//! An object is gone only when Docker's daemon answered that it does not
//! have it. Every other failure, such as an unknown context, a daemon that
//! does not answer or a missing `docker` program, fails the call with
//! [`DockerError::Unavailable`].

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::time::Duration;

use crate::engine::container::app::{SECRET_FILES, SECRETS_LABEL};
use crate::engine::container::command::{
    DOCKER, DOCKER_DEADLINE, Finished, check_with, message, quick_with, run_program,
};
use crate::engine::container::compose::ComposeEnv;
use crate::engine::container::names::{STORE_LABEL, Scope, WORKSPACE_ID_LABEL, WORKSPACE_LABEL};
use crate::engine::container::objects::{AnonymousVolumes, rm_args, stop_seconds};
use crate::engine::container::runner::docker_time_ms;
use crate::engine::container::{ContainerError, ContainerState};
use crate::record::docker::{Docker, DockerError, DockerKind, DockerObject, SeenContainer};

#[cfg(test)]
mod tests;

/// The Docker that runs the `docker` program for phase's `sweep`.
#[derive(Clone, Debug)]
pub(crate) struct DockerCli {
    /// The `docker` program.
    program: OsString,
    /// The scope whose older labels an object without phase's workspace
    /// label must carry, or `None` to match such objects never.
    legacy: Option<Scope>,
}

impl DockerCli {
    /// Returns the Docker that runs the `docker` program found on `PATH`.
    pub(crate) fn new() -> Self {
        Self::with_program(DOCKER.into())
    }

    /// Returns the Docker that runs `program` as the `docker` program.
    pub(crate) const fn with_program(program: OsString) -> Self {
        Self {
            program,
            legacy: None,
        }
    }

    /// Returns the Docker that also matches an object without phase's
    /// workspace label when its `mxv.store` and `mxv.workspace` labels name
    /// `scope`.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "mxv passes the scope of its older labels when its commands move to phase"
        )
    )]
    pub(crate) fn with_legacy(mut self, scope: Scope) -> Self {
        self.legacy = Some(scope);
        self
    }

    /// Returns the `docker` program.
    fn program(&self) -> &OsStr {
        &self.program
    }

    /// Reports whether an object with `labels` belongs to the workspace
    /// `workspace`. Its `mxv.workspace-id` label decides when it has one.
    /// Otherwise its `mxv.store` and `mxv.workspace` labels must name the
    /// legacy scope, when one is given. Each value must equal the expected
    /// one exactly, with nothing trimmed.
    fn ours(&self, workspace: &str, labels: &BTreeMap<String, String>) -> bool {
        if let Some(workspace_id) = labels.get(WORKSPACE_ID_LABEL) {
            return workspace_id == workspace;
        }
        let is = |key: &str, value: &str| labels.get(key).is_some_and(|found| found == value);
        self.legacy
            .as_ref()
            .and_then(Scope::legacy)
            .is_some_and(|(store, workspace)| {
                is(STORE_LABEL, store.as_str()) && is(WORKSPACE_LABEL, workspace.as_str())
            })
    }

    /// Inspects `object` with the Go template `format`, whose first line is
    /// the object's id and whose second is its labels as [`fn@format`]
    /// prints them, and returns the id and the lines after the labels, or
    /// `None` when the daemon answered that the object does not exist or
    /// it does not carry this workspace's labels.
    fn inspect(
        &self,
        object: &DockerObject<'_>,
        format: &str,
    ) -> Result<Option<(String, String)>, DockerError> {
        let (noun, _, _) = inspect_of(object.kind);
        let operation = inspect_operation(object.kind);
        let target = object.id.unwrap_or_else(|| object.name.as_str());
        let finished = quick_with(
            self.program(),
            [noun, "inspect", "--format", format, "--", target],
            operation,
        )
        .map_err(|error| unavailable(&error))?;
        if finished.missing() {
            return Ok(None);
        }
        if !finished.status.success() {
            let id = object.id.filter(|_| object.kind == DockerKind::Container);
            return Err(
                DockerError::unavailable(message(&finished.stderr)).during_on_id(operation, id)
            );
        }
        let mut lines = finished.stdout.lines();
        let id = lines.next().unwrap_or_default();
        let labels = labels(lines.next().unwrap_or_default()).unwrap_or_default();
        if id.is_empty() || !self.ours(object.workspace.as_str(), &labels) {
            return Ok(None);
        }
        let rest: Vec<&str> = lines.collect();
        Ok(Some((id.to_owned(), rest.join("\n"))))
    }

    /// Returns the failure of the call `operation` on the container whose
    /// id is `id` that failed with `finished`: Docker did not answer when
    /// its daemon does not answer now, and otherwise it refused.
    fn failure(
        &self,
        finished: &Finished,
        operation: &'static str,
        id: Option<&str>,
    ) -> DockerError {
        match check_with(self.program()) {
            Ok(()) => DockerError::refused(message(&finished.stderr)).during_on_id(operation, id),
            Err(error) => unavailable(&error),
        }
    }
}

impl DockerCli {
    /// Removes the container `id` as `down` removes one. It turns off the
    /// container's restart policy and reads its status again. A running or
    /// paused container gets its stop signal and waits up to `wait`; any
    /// other container, such as one that Docker is restarting, is removed
    /// at once, because `docker stop` would first wait out Docker's restart
    /// delay. Docker refuses to update a container that has stopped, so a
    /// refused update fails the removal only when the container still runs.
    /// docker removes the container's anonymous volumes with it, and its
    /// named volumes stay.
    fn remove_container(&self, id: &str, wait: Duration) -> Result<(), DockerError> {
        let updated = quick_with(
            self.program(),
            ["update", "--restart", "no", "--", id],
            "update",
        )
        .map_err(|error| unavailable(&error))?;
        let inspected = quick_with(
            self.program(),
            ["container", "inspect", "--format", STATE_FORMAT, "--", id],
            "container inspect",
        )
        .map_err(|error| unavailable(&error))?;
        if inspected.missing() {
            return Ok(());
        }
        if !inspected.status.success() {
            return Err(self.failure(&inspected, "container inspect", Some(id)));
        }
        let state = ContainerState::parse(&inspected.stdout);
        if state.runs() {
            if !updated.status.success() && !updated.missing() {
                return Err(self.failure(&updated, "update", Some(id)));
            }
            let seconds = stop_seconds(wait).to_string();
            let stopped = run_program(
                self.program(),
                ["stop", "--time", &seconds, "--", id],
                ComposeEnv::default(),
                "stop",
                wait + DOCKER_DEADLINE,
            )
            .map_err(|error| unavailable(&error))?;
            if !stopped.status.success() && !stopped.missing() {
                return Err(self.failure(&stopped, "stop", Some(id)));
            }
        }
        let removed = quick_with(self.program(), rm_args(id, AnonymousVolumes::Remove), "rm")
            .map_err(|error| unavailable(&error))?;
        if removed.status.success() || removed.missing() {
            Ok(())
        } else {
            Err(self.failure(&removed, "rm", Some(id)))
        }
    }
}

/// The Go template of `docker container inspect` that prints a container's
/// status and exit code, which [`ContainerState::parse`] reads.
const STATE_FORMAT: &str = "{{.State.Status}} {{.State.ExitCode}}";

/// Returns the Go template of `docker container inspect` that prints,
/// after [`STATE_FORMAT`], how many times Docker restarted a container and
/// the value of its secrets label, and on the next line when Docker last
/// started it, which [`status_state`] reads.
fn restarts_format() -> String {
    format!(
        "{{{{.RestartCount}}}} {{{{index .Config.Labels {SECRETS_LABEL:?}}}}}\n{{{{.State.StartedAt}}}}"
    )
}

/// Returns the state, the restarts and the start time of a container that
/// `docker container inspect` printed as `printed` with [`STATE_FORMAT`]
/// and [`restarts_format`]. A running container whose process receives
/// secret files and that Docker has restarted waits for files that its
/// restart removed, so it counts as restarting, as `up` counts it as not
/// running. A count or a time that cannot be read is `None`.
fn status_state(printed: &str) -> SeenContainer {
    let mut lines = printed.lines();
    let state = ContainerState::parse(lines.next().unwrap_or_default());
    let mut fields = lines.next().unwrap_or_default().split_whitespace();
    let restarts = fields.next().and_then(|count| count.parse::<u32>().ok());
    let files = fields.next() == Some(SECRET_FILES);
    let started_at_ms = lines.next().and_then(|stamp| docker_time_ms(stamp.trim()));
    let state =
        if state == ContainerState::Running && restarts.is_some_and(|count| count > 0) && files {
            ContainerState::Restarting
        } else {
            state
        };
    SeenContainer::new(state, restarts, started_at_ms)
}

/// Returns the failure of a Docker that `error` shows does not answer,
/// with the cause of `error`.
fn unavailable(error: &ContainerError) -> DockerError {
    error.describe(DockerError::unavailable(error.to_string()).with_cause(error.cause()))
}

/// Returns the operation of the inspect command of `kind`, as phase names
/// it in an error, such as `container inspect`.
const fn inspect_operation(kind: DockerKind) -> &'static str {
    match kind {
        DockerKind::Container => "container inspect",
        DockerKind::Network => "network inspect",
        DockerKind::Volume => "volume inspect",
    }
}

/// Returns the words of the inspect command of `kind`, and the Go template
/// path of its id and labels.
const fn inspect_of(kind: DockerKind) -> (&'static str, &'static str, &'static str) {
    match kind {
        DockerKind::Container => ("container", "{{.Id}}", ".Config.Labels"),
        DockerKind::Network => ("network", "{{.Id}}", ".Labels"),
        DockerKind::Volume => ("volume", "{{.Name}}", ".Labels"),
    }
}

/// Returns the Go template that prints an object's id on one line and its
/// labels on the next, as JSON. JSON escapes every tab and newline in a
/// label's value, so no value can reach another field or line.
fn format(kind: DockerKind) -> String {
    let (_, id, labels) = inspect_of(kind);
    format!("{id}\n{{{{json {labels}}}}}")
}

/// Returns the labels that `docker inspect` printed as `printed`, the JSON
/// of [`fn@format`]'s second line, or `None` when they are not a map of
/// strings. Docker prints `null` for an object without labels.
fn labels(printed: &str) -> Option<BTreeMap<String, String>> {
    serde_json::from_str::<Option<BTreeMap<String, String>>>(printed)
        .ok()
        .flatten()
}

impl Docker for DockerCli {
    fn find(&self, object: &DockerObject<'_>) -> Result<Option<String>, DockerError> {
        Ok(self
            .inspect(object, &format(object.kind))?
            .map(|(id, _)| id))
    }

    fn container_seen(
        &self,
        object: &DockerObject<'_>,
    ) -> Result<Option<SeenContainer>, DockerError> {
        let format = format!(
            "{}\n{STATE_FORMAT}\n{}",
            format(DockerKind::Container),
            restarts_format()
        );
        let object = DockerObject {
            kind: DockerKind::Container,
            ..*object
        };
        Ok(self
            .inspect(&object, &format)?
            .map(|(_, rest)| status_state(&rest)))
    }

    fn remove(&self, kind: DockerKind, id: &str, wait: Duration) -> Result<(), DockerError> {
        let removed = match kind {
            DockerKind::Container => return self.remove_container(id, wait),
            DockerKind::Network => {
                quick_with(self.program(), ["network", "rm", "--", id], "network rm")
            }
            DockerKind::Volume => {
                quick_with(self.program(), ["volume", "rm", "--", id], "volume rm")
            }
        }
        .map_err(|error| unavailable(&error))?;
        if removed.status.success() || removed.missing() {
            Ok(())
        } else {
            let operation = match kind {
                DockerKind::Network => "network rm",
                _ => "volume rm",
            };
            Err(self.failure(&removed, operation, None))
        }
    }
}
