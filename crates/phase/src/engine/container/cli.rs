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

use crate::engine::container::command::{
    DOCKER, DOCKER_DEADLINE, Finished, check_with, message, quick_with, run_program,
};
use crate::engine::container::compose::ComposeEnv;
use crate::engine::container::names::{STORE_LABEL, Scope, WORKSPACE_ID_LABEL, WORKSPACE_LABEL};
use crate::engine::container::objects::stop_seconds;
use crate::engine::container::{ContainerError, ContainerState};
use crate::record::docker::{Docker, DockerError, DockerKind, DockerObject};

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
        let target = object.id.unwrap_or_else(|| object.name.as_str());
        let finished = quick_with(
            self.program(),
            [noun, "inspect", "--format", format, "--", target],
            "inspect",
        )
        .map_err(|error| unavailable(&error))?;
        if finished.missing() {
            return Ok(None);
        }
        if !finished.status.success() {
            return Err(DockerError::unavailable(message(&finished.stderr)));
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

    /// Returns the failure of a call that failed with `finished`: Docker
    /// did not answer when its daemon does not answer now, and otherwise
    /// it refused.
    fn failure(&self, finished: &Finished) -> DockerError {
        match check_with(self.program()) {
            Ok(()) => DockerError::refused(message(&finished.stderr)),
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
            "inspect",
        )
        .map_err(|error| unavailable(&error))?;
        if inspected.missing() {
            return Ok(());
        }
        if !inspected.status.success() {
            return Err(self.failure(&inspected));
        }
        let state = ContainerState::parse(&inspected.stdout);
        if state.runs() {
            if !updated.status.success() && !updated.missing() {
                return Err(self.failure(&updated));
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
                return Err(self.failure(&stopped));
            }
        }
        let removed = quick_with(self.program(), ["rm", "--force", "--", id], "rm")
            .map_err(|error| unavailable(&error))?;
        if removed.status.success() || removed.missing() {
            Ok(())
        } else {
            Err(self.failure(&removed))
        }
    }
}

/// The Go template of `docker container inspect` that prints a container's
/// status and exit code, which [`ContainerState::parse`] reads.
const STATE_FORMAT: &str = "{{.State.Status}} {{.State.ExitCode}}";

/// Returns the failure of a Docker that `error` shows does not answer.
fn unavailable(error: &ContainerError) -> DockerError {
    DockerError::unavailable(error.to_string())
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

    fn container_state(
        &self,
        object: &DockerObject<'_>,
    ) -> Result<Option<ContainerState>, DockerError> {
        let format = format!("{}\n{STATE_FORMAT}", format(DockerKind::Container));
        let object = DockerObject {
            kind: DockerKind::Container,
            ..*object
        };
        Ok(self
            .inspect(&object, &format)?
            .map(|(_, rest)| ContainerState::parse(&rest)))
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
            Err(self.failure(&removed))
        }
    }
}
