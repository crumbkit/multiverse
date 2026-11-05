//! The container runner, which runs the containers of a workspace with
//! the `docker` program found on `PATH` and its `compose` plugin.
//! multiverse's docker backend moved here, and mxv's `up`, `down`,
//! `status`, `logs` and `remove` call it through [`Lifecycle`]. This module
//! owns the `Command`s of `docker` (PROC-1).
//!
//! One docker daemon can serve several mxv state directories, such as two
//! people's or two test runs', and each of them may have a workspace with
//! the same name. Each state directory therefore has a store id, a random
//! 32-digit hex number that [`Lifecycle`] keeps in it, and a scope names
//! one workspace of one store.
//!
//! The containers of one workspace form one Docker Compose project, named
//! `mxv-<id8>-<workspace>`, where `<id8>` is the first 8 digits of the store
//! id. A compose project name allows only lowercase letters, digits, `-`
//! and `_`, so the workspace's `_` is written `__` and its `.` is written
//! `_d`, which keeps two workspace names from sharing a project. Each
//! manifest container is one service of the project. mxv writes the
//! project's compose file into the workspace's run directory, and starts
//! one service at a time with `docker compose up`. The compose file names
//! each environment variable and refers to its value by a variable of the
//! `docker compose` process, so the values exist only in that process's
//! environment and in the container.
//!
//! The container of the service `<name>` is named `<project>.<name>`, and
//! its named volume `<volume>` is `<project>.<volume>`. Neither the project
//! nor a manifest name holds a `.`, so the part after the `.` is always the
//! container or volume. Every container and named volume carries three
//! labels: `mxv.store=<id>`, `mxv.workspace=<name>`, and
//! `mxv.container=<name>` or `mxv.volume=<name>`. Those that
//! [`Lifecycle::prepare`] scopes also carry `mxv.workspace-id=<id>`, the
//! workspace's stable id, `mxv.run-record=<path>`, the absolute path of
//! the workspace's run record, and `mxv.app=<name>`, which is the
//! workspace's name until mxv names apps. Compose adds labels of its own.
//!
//! `DockerCli` gives phase's `sweep` the objects of a workspace: it
//! matches an object by `mxv.workspace-id`, and an object that carries only
//! the older labels by `mxv.store` and `mxv.workspace`. It never matches or
//! removes an object without an mxv label.
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
//! [`ContainerError::Foreign`] when they do not name this store, workspace
//! and object. mxv never touches such an object. The labels' check is the
//! only way to get an [`OwnedContainer`] or an owned volume, and the
//! functions that stop, remove, delete or follow an object take one. An
//! anonymous volume carries no such labels, so an anonymous volume's
//! handle comes from the inspection of the owned container that mounts it.
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
//! code in phase that does. `objects` inspects containers and volumes,
//! mints their handles, and stops, removes and deletes them. `compose`
//! writes the compose file, holds the values of the containers' variables
//! in a [`ComposeValue`] each, and runs `docker compose up`. `lifecycle`
//! keeps the run records of a workspace's containers through the host
//! program's [`Host`], and [`Lifecycle`] is what mxv's runs call.
//! `secrets` holds the wrapper and the writer that deliver a container's
//! secrets as files in a tmpfs (decision P3). `cli` is
//! the `DockerCli` of phase's `sweep`. A [`Prepared`] from
//! [`Lifecycle::prepare`] or
//! [`Lifecycle::existing`] is the only way to name the workspace's scope,
//! and the caller converts the manifest's containers into
//! [`ContainerSpecs`], so the module reads no manifest.

mod app;
mod cli;
mod command;
mod compose;
mod error;
mod host;
mod lifecycle;
mod names;
mod objects;
mod run_dir;
mod runner;
mod secrets;

#[cfg(test)]
mod tests;

pub(crate) use crate::engine::container::cli::DockerCli;
pub use crate::engine::container::compose::{
    ComposeValue, ContainerSpec, ContainerSpecs, PublishedPort,
};
pub use crate::engine::container::error::ContainerError;
pub use crate::engine::container::host::{Follow, Host, Versioned};
pub use crate::engine::container::lifecycle::{
    Lifecycle, Prepared, RUN_VERSION, SavedWorkspaceId, Start, WorkspaceVolumes,
};
pub use crate::engine::container::names::{BadDockerName, ServiceName, VolumeKey};
pub(crate) use crate::engine::container::objects::answers_within;
pub use crate::engine::container::objects::{
    ContainerState, Inspected, OwnedContainer, answers, reinspect,
};
pub use crate::engine::container::run_dir::RunDir;
pub(crate) use crate::engine::container::runner::ComposeRunner;
