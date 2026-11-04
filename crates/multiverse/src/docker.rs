//! The docker backend of a manifest's containers, which runs the `docker`
//! program found on `PATH` and its `compose` plugin.
//!
//! One docker daemon can serve several mxv state directories, such as two
//! people's or two test runs', and each of them may have a workspace with
//! the same name. Each state directory therefore has a store id, a random
//! 32-digit hex number that [`crate::Runs`] keeps in it, and a [`Scope`]
//! names one workspace of one store.
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
//! `mxv.container=<name>` or `mxv.volume=<name>`. Compose adds labels of
//! its own.
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
//! [`Error::DockerForeign`] when they do not name this store, workspace and
//! object. mxv never touches such an object. The labels' check is the only
//! way to get an [`OwnedContainer`] or an [`OwnedVolume`], and the
//! functions that stop, remove, delete or follow an object take one. An
//! anonymous volume carries no such labels, so an [`AnonymousVolume`]
//! comes from the inspection of the owned container that mounts it.
//!
//! Each docker command runs with a deadline, after which mxv kills it and
//! fails with [`Error::DockerFailed`].
//!
//! The module's parts are these. `names` holds the [`Scope`] and the
//! checked names and ids. `command` runs the `docker` program, and is the
//! only code in mxv that does. `objects` inspects containers and volumes,
//! mints their handles, and stops, removes and deletes them. `compose`
//! writes the compose file, holds the values of the containers' variables
//! in a [`ComposeEnv`], and runs `docker compose up`. `lifecycle` keeps the
//! run records of a workspace's containers, and [`Lifecycle`] is what
//! [`crate::Runs`] calls. A [`Prepared`] from [`Lifecycle::prepare`] or
//! [`Lifecycle::existing`] is the only way to name the workspace's scope,
//! and the caller converts the manifest's containers into
//! [`ContainerSpecs`], so the module reads no manifest.

mod command;
mod compose;
mod lifecycle;
mod names;
mod objects;

#[cfg(test)]
mod tests;

pub(crate) use crate::docker::compose::{
    ComposeValue, ContainerSpec, ContainerSpecs, PublishedPort,
};
pub(crate) use crate::docker::lifecycle::{Lifecycle, Prepared, Start, WorkspaceVolumes};
pub(crate) use crate::docker::names::{ServiceName, VolumeKey};
pub(crate) use crate::docker::objects::{ContainerState, OwnedContainer, answers, reinspect};
#[cfg(doc)]
use crate::docker::{
    compose::ComposeEnv,
    names::Scope,
    objects::{AnonymousVolume, OwnedVolume},
};
#[cfg(doc)]
use crate::error::Error;
