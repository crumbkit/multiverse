//! The Docker of phase's `sweep`, which runs the `docker` program.
//!
//! It matches an object by phase's workspace label, `mxv.workspace-id`.
//! An object that mxv created before it wrote that label carries only
//! `mxv.store` and `mxv.workspace`, and matches when a scope of those
//! labels is given with [`DockerCli::with_legacy`], until the object is
//! recreated. An object that carries no mxv label never matches, even by a
//! recorded id, so `sweep` never removes it.

use std::ffi::{OsStr, OsString};
use std::time::Duration;

use crate::engine::container::ContainerError;
use crate::engine::container::command::{
    DOCKER, DOCKER_DEADLINE, Finished, check_with, message, quick_with, run_program,
};
use crate::engine::container::compose::ComposeEnv;
use crate::engine::container::names::{STORE_LABEL, Scope, WORKSPACE_ID_LABEL, WORKSPACE_LABEL};
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

    /// Reports whether an object whose workspace id, store and workspace
    /// labels are `workspace_id`, `store` and `name` belongs to the
    /// workspace `workspace`.
    fn ours(&self, workspace: &str, workspace_id: &str, store: &str, name: &str) -> bool {
        if !workspace_id.is_empty() {
            return workspace_id == workspace;
        }
        self.legacy.as_ref().is_some_and(|scope| {
            store == scope.store().as_str() && name == scope.workspace().as_str()
        })
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

/// Returns the Go template that prints an object's id and its workspace
/// id, store and workspace labels, separated by tabs.
fn format(kind: DockerKind) -> String {
    let (_, id, labels) = inspect_of(kind);
    format!(
        "{id}\t{{{{index {labels} \"{WORKSPACE_ID_LABEL}\"}}}}\t{{{{index {labels} \"{STORE_LABEL}\"}}}}\t{{{{index {labels} \"{WORKSPACE_LABEL}\"}}}}"
    )
}

/// Returns a label's value as the template printed it, with Go's
/// placeholder for a missing value read as empty.
fn label(field: Option<&str>) -> &str {
    match field.map(str::trim) {
        None | Some("<no value>") => "",
        Some(value) => value,
    }
}

impl Docker for DockerCli {
    fn find(&self, object: &DockerObject<'_>) -> Result<Option<String>, DockerError> {
        let (noun, _, _) = inspect_of(object.kind);
        let target = object.id.unwrap_or_else(|| object.name.as_str());
        let format = format(object.kind);
        let finished = quick_with(
            self.program(),
            [noun, "inspect", "--format", &format, "--", target],
            "inspect",
        )
        .map_err(|error| unavailable(&error))?;
        if finished.missing() {
            return Ok(None);
        }
        if !finished.status.success() {
            return Err(DockerError::unavailable(message(&finished.stderr)));
        }
        let mut fields = finished
            .stdout
            .lines()
            .next()
            .unwrap_or_default()
            .split('\t');
        let id = fields.next().unwrap_or_default().trim().to_owned();
        let workspace_id = label(fields.next());
        let store = label(fields.next());
        let name = label(fields.next());
        if id.is_empty() || !self.ours(object.workspace.as_str(), workspace_id, store, name) {
            return Ok(None);
        }
        Ok(Some(id))
    }

    fn remove(&self, kind: DockerKind, id: &str, wait: Duration) -> Result<(), DockerError> {
        let removed = match kind {
            DockerKind::Container => {
                let seconds = wait.as_secs().to_string();
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
                quick_with(self.program(), ["rm", "--force", "--", id], "rm")
            }
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
