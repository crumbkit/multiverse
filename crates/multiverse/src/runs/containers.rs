//! The containers of a workspace's app: starting one, waiting for its
//! readiness, and reporting its state and volumes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Instant;

use crate::clock::{FOLLOW_INTERVAL, pause};
use crate::docker::{self, ContainerState};
use crate::error::Error;
use crate::manifest::{Container, Manifest};
use crate::name::WorkspaceName;
use crate::ports;
use crate::process::{self, Launch};
use crate::record::Workspace;
use crate::runs::{
    Kind, ProcessState, ProcessStatus, Runs, START_GRACE, TAIL_LINES, UpOutcome, code_and_signal,
    remove_if_present, tail,
};
use crate::store;

impl Runs {
    /// Returns the names of the containers that `manifest` declares, and,
    /// when there is no manifest or it declares containers, the containers
    /// of the workspace that docker lists. A manifest without containers
    /// never makes mxv run docker.
    pub(super) fn containers(
        &self,
        workspace: &Workspace,
        manifest: Option<&Manifest>,
    ) -> BTreeSet<String> {
        let mut names: BTreeSet<String> = manifest
            .map(|manifest| manifest.containers.keys().cloned().collect())
            .unwrap_or_default();
        if manifest.is_none_or(|manifest| !manifest.containers.is_empty()) {
            names.extend(docker::listed(&workspace.name));
        }
        names
    }

    /// Returns the containers of the workspace `name` that exist, running
    /// or stopped, when there is no manifest or it declares containers.
    pub(super) fn existing_containers(
        &self,
        name: &WorkspaceName,
        manifest: Option<&Manifest>,
    ) -> BTreeSet<String> {
        if manifest.is_some_and(|manifest| manifest.containers.is_empty()) {
            return BTreeSet::new();
        }
        docker::listed(name).into_iter().collect()
    }

    /// Reports whether the container `container` of the workspace `name`
    /// runs. It reports `false` when docker cannot say.
    pub(super) fn container_runs(name: &WorkspaceName, container: &str) -> bool {
        matches!(
            docker::state(&docker::container_name(name, container)),
            Ok(Some(ContainerState::Running))
        )
    }

    /// Returns the containers of the workspace `name` that run, in name
    /// order.
    pub(super) fn running_containers(name: &WorkspaceName) -> Vec<String> {
        docker::listed(name)
            .into_iter()
            .filter(|container| Self::container_runs(name, container))
            .collect()
    }

    /// Returns the status of the container `container`, which docker
    /// reports.
    pub(super) fn container_status(
        &self,
        workspace: &Workspace,
        manifest: Option<&Manifest>,
        container: &str,
    ) -> Result<ProcessStatus, Error> {
        let state = match docker::state(&docker::container_name(&workspace.name, container))? {
            None => ProcessState::Stopped,
            Some(ContainerState::Running) => ProcessState::Running { pid: None },
            Some(ContainerState::Exited(code)) => {
                let (code, signal) = code_and_signal(Some(code));
                ProcessState::Exited { code, signal }
            }
        };
        Ok(self.container_status_of(workspace, manifest, container, state))
    }

    /// Returns the status of the container `container` in `state`.
    pub(super) fn container_status_of(
        &self,
        workspace: &Workspace,
        manifest: Option<&Manifest>,
        container: &str,
        state: ProcessState,
    ) -> ProcessStatus {
        let ports: BTreeMap<String, u16> = manifest
            .and_then(|manifest| manifest.containers.get(container))
            .map(|declared| {
                declared
                    .ports
                    .iter()
                    .filter_map(|published| {
                        workspace
                            .ports
                            .get(&published.port)
                            .map(|number| (published.port.clone(), *number))
                    })
                    .collect()
            })
            .unwrap_or_default();
        ProcessStatus {
            name: container.to_owned(),
            kind: Kind::Container,
            state,
            ports,
            log: self.log(&workspace.name, container),
            group_alive: false,
        }
    }

    /// Starts one container and waits for its readiness, as
    /// [`Runs::up`] describes. It returns `None` when the container is
    /// ready, and the outcome that stops `up` otherwise.
    pub(super) fn start_container(
        &self,
        workspace: &Workspace,
        checkout: &Path,
        name: &str,
        declared: &Container,
    ) -> Result<Option<UpOutcome>, Error> {
        let mut host_ports = Vec::new();
        for published in &declared.ports {
            let Some(number) = workspace.ports.get(&published.port).copied() else {
                continue;
            };
            if ports::is_taken(number) {
                return Ok(Some(UpOutcome::PortTaken {
                    process: name.to_owned(),
                    port: published.port.clone(),
                    number,
                    holders: if Self::container_runs(&workspace.name, name) {
                        vec![name.to_owned()]
                    } else {
                        Vec::new()
                    },
                }));
            }
            host_ports.push(number);
        }
        let container_name = docker::container_name(&workspace.name, name);
        // A container that exited on its own still holds its name, so it
        // is removed before the new one starts.
        docker::remove(&container_name, self.grace)?;
        let log = self.log(&workspace.name, name);
        store::rotate_log(&log, &self.file(&workspace.name, name, "log.1"))?;
        docker::start(&docker::Run {
            workspace: &workspace.name,
            name,
            container: declared,
            host_ports: &host_ports,
        })?;
        self.follow_logs(&workspace.name, checkout, name, &container_name, &log)?;
        let port = declared
            .ready
            .as_ref()
            .and_then(|ready| workspace.ports.get(&ready.port).copied());
        let timeout = declared
            .ready
            .as_ref()
            .map_or(START_GRACE, |ready| ready.timeout);
        let deadline = Instant::now() + timeout;
        loop {
            match docker::state(&container_name)? {
                Some(ContainerState::Running) => {}
                ended => {
                    let code = match ended {
                        Some(ContainerState::Exited(code)) => Some(code),
                        _ => None,
                    };
                    let (code, signal) = code_and_signal(code);
                    // `docker logs` may not have copied the last lines yet.
                    pause(FOLLOW_INTERVAL);
                    return Ok(Some(UpOutcome::Exited {
                        process: name.to_owned(),
                        code,
                        signal,
                        tail: tail(&log, TAIL_LINES),
                        log,
                    }));
                }
            }
            if port.is_some_and(docker::answers) {
                return Ok(None);
            }
            if Instant::now() >= deadline {
                let Some(ready) = &declared.ready else {
                    return Ok(None);
                };
                docker::remove(&container_name, self.grace)?;
                return Ok(Some(UpOutcome::NotReady {
                    process: name.to_owned(),
                    port: ready.port.clone(),
                    number: port.unwrap_or_default(),
                    timeout: ready.timeout,
                    tail: tail(&log, TAIL_LINES),
                    log,
                }));
            }
            pause(FOLLOW_INTERVAL);
        }
    }

    /// Starts a detached `docker logs --follow` that appends the output of
    /// the container to `log` until the container stops.
    fn follow_logs(
        &self,
        name: &WorkspaceName,
        checkout: &Path,
        container: &str,
        container_name: &str,
        log: &Path,
    ) -> Result<(), Error> {
        let exit_file = self.file(name, container, "logs-exit");
        remove_if_present(&exit_file)?;
        let argv = [
            "docker".to_owned(),
            "logs".to_owned(),
            "--follow".to_owned(),
            container_name.to_owned(),
        ];
        // The follower runs in a process group of its own and ends when
        // the container stops, so mxv does not wait for it.
        let _follower = process::launch(&Launch {
            name: container,
            argv: &argv,
            cwd: checkout,
            env: &[],
            log,
            exit_file: &exit_file,
        })?;
        Ok(())
    }

    /// Returns the volumes of the workspace `name` that docker lists, in
    /// name order. It returns none when docker is missing or its daemon
    /// does not answer.
    #[must_use]
    pub fn volumes(&self, name: &WorkspaceName) -> Vec<String> {
        docker::volumes(name)
    }

    /// Deletes the volumes `volumes`, which [`Runs::volumes`] returned.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DockerMissing`], [`Error::DockerUnavailable`] and
    /// [`Error::DockerFailed`].
    pub fn delete_volumes(&self, volumes: &[String]) -> Result<(), Error> {
        docker::remove_volumes(volumes)
    }
}
