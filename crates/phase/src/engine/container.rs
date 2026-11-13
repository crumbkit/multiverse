//! The container runner, which runs the containers of a workspace with
//! the `docker` program found on `PATH` and its `compose` plugin.
//! This module owns the `Command`s of `docker` (PROC-1).
//!
//! Each container app of a plan is one Docker Compose project of its own,
//! named `phase-<hash>-<app>`, where `<hash>` is 16 hex digits of a hash of
//! the workspace's stable id. The container of the process `<name>` is
//! `<project>.<name>`, and the named volume `<volume>` is
//! `phase-<hash>.<volume>`, which every app of the workspace shares.
//! Neither the project nor a manifest name holds a `.`, so the part after
//! the `.` is always the container or volume. Every container and named
//! volume carries the labels `mxv.workspace-id=<id>`, the workspace's
//! stable id, `mxv.run-record=<path>`, the absolute path of its run record,
//! and `mxv.container=<name>` or `mxv.volume=<name>`, and a container also
//! carries `mxv.app=<name>`. Compose adds labels of its own.
//!
//! `DockerCli` gives phase's `sweep` the objects of a workspace, and
//! matches an object by `mxv.workspace-id` and `mxv.run-record`. It never
//! matches or removes an object without both labels.
//!
//! Every container runs on docker's default bridge network, with
//! `network_mode: bridge`, so Compose creates no network for the project.
//! A docker daemon has a limited number of address pools for networks, and
//! one network per workspace would use them up. Containers reach each other
//! and the host's processes through their published ports on 127.0.0.1;
//! there is no name resolution between the services.
//!
//! mxv records the id of each container it started and the name of each
//! volume. Before it stops, removes, adopts or reuses a container or
//! volume, it reads the object's labels, and it fails with
//! [`ContainerError::Foreign`] when they do not name this workspace and
//! object. mxv never touches such an object. The labels' check is the
//! only way to get an [`OwnedContainer`] or an owned volume, and the
//! functions that stop, remove, delete or follow an object take one.
//!
//! Each docker command runs with a deadline, after which mxv kills it and
//! fails with [`ContainerError::Failed`].
//!
//! phase's `up`, `down` and `restart` run container apps through
//! `ComposeRunner`, which `runner` holds. Each container app of a plan is
//! one compose project of its own, which `app` names and describes, as the
//! design's Containers section says.
//!
//! The module's parts are these. `names` holds the scope and the checked
//! names and ids. `command` runs the `docker` program, and is the only
//! code in phase that does, apart from the `docker exec` of `exec`, which
//! builds that command for [`Phase::exec`](crate::Phase::exec) and which
//! the engine's exec module runs with the caller's streams. `objects` inspects containers and volumes,
//! mints their handles, and stops, removes and deletes them. `compose`
//! runs `docker compose up` for an app's project. `volumes`
//! finds and deletes the data volumes of a workspace's apps.
//! `secrets` holds the wrapper and the writer that deliver a container's
//! secrets as files in a tmpfs (decision P3). `cli` is
//! the `DockerCli` of phase's `sweep`.

mod app;
mod cli;
mod command;
mod compose;
mod error;
pub(crate) mod exec;
mod names;
mod objects;
mod runner;
mod secrets;
mod volume_owner;
mod volumes;

#[cfg(test)]
mod tests;

pub(crate) use crate::engine::container::cli::DockerCli;
pub use crate::engine::container::error::ContainerError;
pub(crate) use crate::engine::container::names::Identity;
pub use crate::engine::container::names::{BadDockerName, ServiceName, VolumeKey};
pub(crate) use crate::engine::container::objects::answers_within;
pub use crate::engine::container::objects::{ContainerState, Inspected, OwnedContainer, reinspect};
pub(crate) use crate::engine::container::runner::{ComposeRunner, recorded_logs};
pub use crate::engine::container::volumes::{
    WorkspaceVolumes, app_volume_labels, app_volume_name, app_volumes, container_memory,
    delete_app_volumes, labelled_volumes, volume_exists,
};
