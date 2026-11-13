//! `up`, `down`, `restart`, `status` and `logs` of a workspace's app, which
//! phase runs from the plan that `phase_plan` builds.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use phase::container::{
    VolumeKey, WorkspaceVolumes, app_volume_labels, app_volume_name, app_volumes,
    delete_app_volumes, volume_exists,
};
use phase::{
    AppName, AppOutcome, Cancel, GrantScope, Growth, Handle, LineSink, LogLine, LogNote,
    LogRequest, Note, PhaseError, ProcessName, Report, RunnerKind, Selection, StartProblem,
    StatusRow, Target, UnknownReason, UpDeadlines, WorkspaceId,
};

use super::{
    DownOutcome, DownResult, ExitReason, Footprint, Kind, Needed, ProcessNote, ProcessState,
    ProcessStatus, RenamedVolume, Runs, StillNeeded, Unstoppable, UpOutcome, Waited, received,
};
use crate::archive::MintHeld;
use crate::error::Error;
use crate::manifest::{Manifest, ReadyCheck, Unit};
use crate::name::WorkspaceName;
use crate::phase_plan::{self, Built, Inputs, MAIN, Unrunnable};
use crate::record::Workspace;
use crate::service::PROVISION_APP;
use crate::store::{self, Lock, Store};

/// How long `up` gives phase to build each selected checkout, and how long
/// [`Runs::build`] lets a command's `build` steps run. mxv's apps run from
/// their checkouts with no phase build, so phase itself does not wait for
/// the first of these.
pub(super) const BUILD_WAIT: Duration = Duration::from_secs(3600);

/// How long `up` gives phase beyond the selected units' readiness
/// timeouts, which covers pulling an image and starting each keeper.
const START_MARGIN: Duration = Duration::from_secs(600);

/// How long `down` gives phase for each selected unit beyond its stop
/// grace and phase's kill wait.
const STOP_MARGIN: Duration = Duration::from_secs(30);

/// The file of a workspace's run directory that says phase has run one of
/// its containers, so its data volumes may exist. mxv runs no docker
/// command for the volumes of a workspace without it.
const CONTAINERS_RAN: &str = ".containers";

mod build;
mod exec;
mod task;
#[cfg(test)]
mod tests;

pub use build::{Admission, Asked, BuildEnd, BuildStop, BuildsPassed, Place};
pub use exec::{Exec, ExecEnd, ExecTarget};
pub use task::{MiseListing, MiseTaskName, PreparedTask, TaskEnd, TaskServices, TaskStart};

/// Returns the checkout that holds `workspace`'s manifest. A workspace
/// holds one checkout for now.
fn checkout(workspace: &Workspace) -> &Path {
    workspace
        .checkouts
        .first()
        .map_or(workspace.path.as_path(), |checkout| checkout.path.as_path())
}

/// Returns the app of the unit `name`, or `None` for a name that is no
/// app's, which no plan holds.
fn app(name: &str) -> Option<AppName> {
    phase_plan::app_name(name).ok()
}

/// Returns the process name of each unit's one process.
fn main_process() -> Option<ProcessName> {
    ProcessName::try_from(MAIN.to_owned()).ok()
}

/// Returns the selection of the units `names`, or of every unit when
/// `names` is empty.
fn selection(names: &[String]) -> Selection {
    if names.is_empty() {
        return Selection::All;
    }
    Selection::only(
        names
            .iter()
            .filter_map(|name| app(name))
            .map(|app| Target { app, process: None })
            .collect(),
    )
}

/// Returns the process id of a status row's handle, or `None` for a
/// container or a row without a handle.
const fn pid(row: &StatusRow) -> Option<u32> {
    match &row.handle {
        Some(Handle::Pid { pid, .. }) => Some(*pid),
        _ => None,
    }
}

/// Returns the state that mxv reports for the phase row `row`.
fn state_of(row: &StatusRow) -> ProcessState {
    use phase::ProcessState as Phase;
    let pid = pid(row);
    match row.state {
        Phase::Ready | Phase::Running | Phase::Paused => ProcessState::Running { pid },
        Phase::Starting => ProcessState::Starting { pid },
        Phase::Stopping => ProcessState::Stopping { pid },
        Phase::Restarting => ProcessState::Restarting,
        Phase::Completed => ProcessState::Exited {
            code: Some(0),
            signal: None,
            reason: None,
        },
        Phase::Exited {
            reason: Some(_), ..
        } => ProcessState::Exited {
            code: None,
            signal: None,
            reason: Some(ExitReason::Unrecorded),
        },
        Phase::Exited { code, signal, .. } => {
            let (code, signal) = ended(row.runner == RunnerKind::Host, code, signal);
            ProcessState::Exited {
                code,
                signal,
                reason: None,
            }
        }
        Phase::Unknown { reason, .. } => ProcessState::Unknown {
            reason: describe_unknown(reason),
        },
        _ => ProcessState::Stopped,
    }
}

/// Returns whether phase has seen the process of the phase row `row`
/// ready since it last started, or `None` when it does not run.
fn ready_of(row: &StatusRow) -> Option<bool> {
    use phase::ProcessState as Phase;
    match row.state {
        Phase::Ready => Some(true),
        Phase::Running | Phase::Paused | Phase::Starting | Phase::Restarting => Some(false),
        _ => None,
    }
}

/// Returns the apps of `report` that a failed `up` rolls back.
fn started_from_nothing(report: &Report) -> Vec<Target> {
    report
        .apps
        .iter()
        .filter(|app| matches!(app.outcome, AppOutcome::Started))
        .map(|app| Target {
            app: app.app.clone(),
            process: None,
        })
        .collect()
}

/// Returns the notes of `report` that mxv shows, by unit: the secret files
/// that `up` rewrote or kept for each app, and the host processes that
/// `up` restarted because their part of the plan changed.
fn notes_of(report: &Report) -> BTreeMap<String, Vec<ProcessNote>> {
    let mut notes: BTreeMap<String, Vec<ProcessNote>> = BTreeMap::new();
    for app in &report.apps {
        for note in &app.notes {
            let shown = match note {
                Note::SecretsRewritten { message, .. } => ProcessNote::SecretsRewritten {
                    message: message.clone(),
                },
                Note::SecretsKept { message, .. } => ProcessNote::SecretsKept {
                    message: message.clone(),
                },
                Note::PlanChanged { message, .. } => ProcessNote::PlanChanged {
                    message: message.clone(),
                },
                Note::VolumeOwnerUnchecked { message, .. } => ProcessNote::VolumeOwnerUnchecked {
                    message: message.clone(),
                },
                _ => continue,
            };
            notes
                .entry(app.app.as_str().to_owned())
                .or_default()
                .push(shown);
        }
    }
    notes
}

/// Returns the stable id that the record of the workspace `name` holds on
/// disk. When the record holds none, `store` mints one and saves the record
/// with it first, unless an archive record of the name holds the mint, and
/// then it returns [`Error::ArchiveUnfinished`] before anything starts.
pub(super) fn saved_workspace_id(
    store: &Store,
    name: &WorkspaceName,
) -> Result<WorkspaceId, Error> {
    let no_id = |reason: &str| Error::DockerFailed {
        operation: "workspace id".to_owned(),
        container: None,
        message: format!("mxv cannot name the workspace's containers, because {reason}"),
        docker_line: None,
    };
    let mut record = store
        .read(name)?
        .ok_or_else(|| no_id("the workspace has no record"))?;
    if record.id.is_none() {
        if let Some(held) = store.mint_held(name) {
            return Err(Error::ArchiveUnfinished {
                name: name.clone(),
                readable: held == MintHeld::OlderRetiring,
            });
        }
        store.update(&mut record)?;
    }
    record
        .id
        .ok_or_else(|| no_id("the workspace's record holds no stable id"))
}

/// Returns the exit code and the signal that a shell's `code` means: a code
/// from 129 to 192 is 128 plus the number of the signal that ended the
/// program.
fn code_and_signal(code: Option<i32>) -> (Option<i32>, Option<i32>) {
    match code {
        Some(code) if (129..=192).contains(&code) => (None, Some(code - 128)),
        code => (code, None),
    }
}

/// Returns the exit code and the signal that mxv reports for a process
/// that phase saw end with `code` or `signal`, which is a host process
/// when `host`. A host process that runs a shell, such as `sh -c`, may end
/// with a code from 129 to 192, which mxv reads as 128 plus the number of
/// the signal that ended the shell's program. A container's code stays the one docker reports.
fn ended(host: bool, code: Option<i32>, signal: Option<i32>) -> (Option<i32>, Option<i32>) {
    match signal {
        Some(signal) => (None, Some(signal)),
        None if host => code_and_signal(code),
        None => (code, None),
    }
}

/// Returns the process group that phase left of the unit `name`, whose
/// app phase reported as `app` with the outcome `Left`, and whose status
/// row was `row`. The app's notes name the group, and say whether phase
/// could not stop it or could not prove it its own. Without a note, the
/// group is the row's, and counts as unproven.
fn left(name: &str, app: &phase::AppReport, row: Option<&StatusRow>) -> Unstoppable {
    let (pid, unverified) = app
        .notes
        .iter()
        .find_map(|note| match note {
            Note::Unstoppable { group, .. } => Some((*group, false)),
            Note::GroupUnverified { group, .. } => Some((*group, true)),
            _ => None,
        })
        .unwrap_or_else(|| (row.and_then(pid).unwrap_or_default(), true));
    Unstoppable {
        process: name.to_owned(),
        pid,
        unverified,
    }
}

/// Returns why phase cannot tell what a process is doing, as a sentence
/// that a row prints.
const fn describe_unknown(reason: UnknownReason) -> &'static str {
    match reason {
        UnknownReason::Unreadable => {
            "phase may not read the process that its run record names, so it cannot tell whether the process is its own"
        }
        UnknownReason::Unsignallable => "the run record names a process that phase never signals",
        UnknownReason::DockerUnavailable => {
            "docker did not answer, so phase cannot tell whether the container runs"
        }
        UnknownReason::UnrecognisedState => {
            "docker reported a container status that phase does not know"
        }
        UnknownReason::LeaderGone => {
            "the process has ended and left members in its process group, and nothing proves that the group is still the one phase started"
        }
        _ => "phase cannot tell what the unit is doing",
    }
}

/// Where [`Runs::logs`] passes a log: each line's text to `pass`, which
/// ends the read when it breaks, and each note about the log to `noted`.
pub struct LogOutput<'a> {
    /// The closure that receives each line.
    pub pass: &'a mut dyn FnMut(String) -> ControlFlow<()>,
    /// The closure that receives each note, such as that the log's
    /// container is gone.
    pub noted: &'a mut dyn FnMut(LogNote),
}

impl std::fmt::Debug for LogOutput<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("LogOutput").finish_non_exhaustive()
    }
}

impl LineSink for LogOutput<'_> {
    fn line(&mut self, line: &LogLine<'_>) -> ControlFlow<()> {
        (self.pass)(line.text.to_owned())
    }

    fn note(&mut self, note: LogNote) {
        (self.noted)(note);
    }
}

/// The docker volumes of a workspace that a removal deletes, which only
/// [`Runs::volumes`] returns, after it checked that each one is the
/// workspace's. [`Runs::delete_volumes`] deletes them from that workspace
/// alone.
#[derive(Debug, PartialEq, Eq)]
pub struct Volumes {
    /// The volumes of the workspace's container apps.
    apps: WorkspaceVolumes,
}

impl Volumes {
    /// Returns the volumes' docker names, in name order.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.apps.names()
    }

    /// Reports whether the workspace has no volume to delete.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.apps.names().is_empty()
    }
}

/// What [`Runs::forget_id`] did with a workspace id.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Forgotten {
    /// phase kept its run record, for the reason that each sentence gives,
    /// and the volumes and the logs stay.
    Held(Vec<String>),
    /// The run record and the logs are gone, with the docker volumes that
    /// these names name.
    Done {
        /// The names of the deleted volumes.
        volumes: Vec<String>,
    },
}

/// Permissions that the host units of a repository ask for and a person
/// has not granted, which [`Runs::granting`] returns. Only
/// [`Runs::grant`] adds them to the repository's grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionAsk {
    /// The repositories whose grant the units share.
    scope: GrantScope,
    /// What each unit asks for beyond the grant.
    growth: Growth,
    /// The repository's name as a person knows it, such as its folder.
    repository: String,
}

impl PermissionAsk {
    /// Returns the repository's name, such as its folder name, or the
    /// folder names of a workspace's several checkouts joined by `+`.
    #[must_use]
    pub fn repository(&self) -> &str {
        &self.repository
    }

    /// Returns the source directories of the repositories, sorted.
    #[must_use]
    pub fn paths(&self) -> &[PathBuf] {
        self.scope.paths()
    }

    /// Returns what each unit asks for, whose [`Growth::lines`] a question
    /// and a refusal print.
    #[must_use]
    pub const fn growth(&self) -> &Growth {
        &self.growth
    }
}

/// Whether the grant of a workspace's repository covers the permissions of
/// the units that a command selects, as [`Runs::granting`] finds it.
#[derive(Debug)]
pub enum Granting {
    /// The grant covers every selected host unit, or the selection has
    /// none.
    Covered,
    /// Some selected host unit is new or asks for more than its grant.
    Asks(PermissionAsk),
    /// The manifest declares no unit with the name.
    UnknownProcess(String),
    /// phase cannot run the manifest.
    Unrunnable(Unrunnable),
}

/// Returns the grant scope of `workspace`, whose stable id is `id`: the
/// sorted source directories of its checkouts, labelled with their folder
/// names, or its own stable id when it has no checkout. Every workspace of
/// the same repositories shares the scope, and so its grant.
fn grant_scope(workspace: &Workspace, id: &WorkspaceId) -> (GrantScope, String) {
    let mut sources: Vec<&Path> = workspace
        .checkouts
        .iter()
        .map(|checkout| checkout.source.as_path())
        .collect();
    sources.sort();
    sources.dedup();
    if sources.is_empty() {
        let name = workspace.name.as_str().to_owned();
        return (GrantScope::new(&name, [PathBuf::from(id.as_str())]), name);
    }
    let names: Vec<String> = sources
        .iter()
        .map(|source| {
            source.file_name().map_or_else(
                || source.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            )
        })
        .collect();
    (
        GrantScope::new(
            &names.join("-"),
            sources.iter().map(|source| source.to_path_buf()),
        ),
        names.join("+"),
    )
}

impl Runs {
    /// Returns what the host units `units` of `workspace`'s `manifest`, by
    /// name, ask for beyond the grant of the workspace's repository, whose
    /// stable id is `id`, or `None` when the grant covers them. A
    /// repository with no grant compares as one that granted nothing.
    fn ask_of(
        &self,
        workspace: &Workspace,
        id: &WorkspaceId,
        manifest: &Manifest,
        units: &[String],
    ) -> Result<Result<Option<PermissionAsk>, Unrunnable>, Error> {
        let requested = match phase_plan::permissions(&self.inputs(workspace, id, manifest), units)
        {
            Ok(requested) => requested,
            Err(unrunnable) => return Ok(Err(unrunnable)),
        };
        if requested.is_empty() {
            return Ok(Ok(None));
        }
        let (scope, repository) = grant_scope(workspace, id);
        let growth = self
            .phase
            .granted(&scope)
            .map_err(|error| self.phase_error(id, error))?
            .unwrap_or_default()
            .growth(&requested);
        Ok(Ok((!growth.is_empty()).then_some(PermissionAsk {
            scope,
            growth,
            repository,
        })))
    }

    /// Returns whether the grant of `workspace`'s repository covers the
    /// permissions of the host units that `names` select in `manifest`,
    /// with their dependencies, or of every unit when `names` is empty. A
    /// unit asks for more when its permissions hold an entry that the grant
    /// lacks, when it would run outside the sandbox and its grant is
    /// sandboxed, or when the grant has no entry for it. Permissions that
    /// stay the same or shrink ask for nothing. It writes nothing, except
    /// the workspace's stable id into `store` when the record holds none.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Store::update`], and [`Error::Phase`] when
    /// the grant file cannot be read.
    pub fn granting(
        &self,
        store: &Store,
        workspace: &Workspace,
        manifest: &Manifest,
        names: &[String],
    ) -> Result<Granting, Error> {
        let order = match manifest.start_order(names) {
            Ok(order) => order,
            Err(unknown) => return Ok(Granting::UnknownProcess(unknown)),
        };
        let id = match &workspace.id {
            Some(id) => id.clone(),
            None => saved_workspace_id(store, &workspace.name)?,
        };
        Ok(match self.ask_of(workspace, &id, manifest, &order)? {
            Ok(None) => Granting::Covered,
            Ok(Some(ask)) => Granting::Asks(ask),
            Err(unrunnable) => Granting::Unrunnable(unrunnable),
        })
    }

    /// Adds what `ask` names to the grant of its repository, which mxv
    /// calls only after a person at a terminal answered yes. The grant only
    /// grows, and every workspace of the repository shares it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Busy`] when another grant holds the grant file's
    /// lock, [`Error::Unwritable`] when it cannot be written, and
    /// [`Error::Phase`] when it cannot be read.
    pub fn grant(&self, ask: &PermissionAsk) -> Result<(), Error> {
        self.phase
            .grant(&ask.scope, &ask.growth)
            .map(drop)
            .map_err(|error| Error::of_phase(error, PathBuf::new()))
    }

    /// Returns the failure of `error`, which phase reported for the
    /// workspace whose stable id is `id`.
    fn phase_error(&self, id: &WorkspaceId, error: PhaseError) -> Error {
        Error::of_phase(error, self.phase.run_record(id))
    }

    /// Returns the plan of `workspace`'s `manifest`, or `None` when the
    /// workspace has no stable id yet or the plan cannot be built, such as
    /// before its first `up` gave it ports.
    fn plan(&self, workspace: &Workspace, manifest: &Manifest) -> Option<Built> {
        let id = workspace.id.as_ref()?;
        phase_plan::build(&self.inputs(workspace, id, manifest)).ok()
    }

    /// Returns what the plan of `workspace`'s `manifest` is built from.
    fn inputs<'a>(
        &'a self,
        workspace: &'a Workspace,
        id: &'a WorkspaceId,
        manifest: &'a Manifest,
    ) -> Inputs<'a> {
        Inputs {
            manifest,
            workspace: &workspace.name,
            id,
            checkout: checkout(workspace),
            roots: phase_plan::roots(workspace),
            workspace_dir: Some(&workspace.path),
            ports: &workspace.ports,
            grace: self.grace,
            caller: &self.caller,
            provisioner: self.provisioner.clone(),
        }
    }

    /// Returns the host ports that the unit `name` of `manifest` receives
    /// in `workspace`, by variable name for a process and by port name for
    /// a container.
    fn unit_ports(
        workspace: &Workspace,
        manifest: Option<&Manifest>,
        name: &str,
    ) -> BTreeMap<String, u16> {
        match manifest.and_then(|manifest| manifest.unit(name)) {
            Some(Unit::Process(process)) => received(workspace, process),
            Some(Unit::Container(container)) => container
                .ports
                .iter()
                .filter_map(|published| {
                    workspace
                        .ports
                        .get(&published.port)
                        .map(|number| (published.port.clone(), *number))
                })
                .collect(),
            None => BTreeMap::new(),
        }
    }

    /// Returns the rows of phase's status of `workspace`, by app, which
    /// are empty before the workspace's first `up` under phase.
    fn phase_rows(
        &self,
        workspace: &Workspace,
        manifest: Option<&Manifest>,
    ) -> Result<BTreeMap<String, StatusRow>, Error> {
        let Some(id) = &workspace.id else {
            return Ok(BTreeMap::new());
        };
        let built = manifest.and_then(|manifest| self.plan(workspace, manifest));
        let status = self
            .phase
            .status(id, built.as_ref().map(|built| &built.plan))
            .map_err(|error| self.phase_error(id, error))?;
        Ok(status
            .rows
            .into_iter()
            .map(|row| (row.app.as_str().to_owned(), row))
            .collect())
    }

    /// Returns the status of the unit `name` from phase's row `row`, or
    /// as stopped without one.
    fn phase_unit_status(
        workspace: &Workspace,
        manifest: Option<&Manifest>,
        name: &str,
        row: Option<&StatusRow>,
    ) -> ProcessStatus {
        let kind = match (manifest.and_then(|manifest| manifest.unit(name)), row) {
            (Some(Unit::Container(_)), _) => Kind::Container,
            (Some(Unit::Process(_)), _) => Kind::Process,
            (None, Some(row)) if row.runner == RunnerKind::Container => Kind::Container,
            // A backend of the services runs in a container whether or not
            // phase still has its row, as after `down`.
            (None, _)
                if manifest.is_some_and(|manifest| {
                    manifest
                        .backends()
                        .iter()
                        .any(|backend| backend.app() == name)
                }) =>
            {
                Kind::Container
            }
            (None, _) => Kind::Process,
        };
        ProcessStatus {
            name: name.to_owned(),
            kind,
            state: row.map_or(ProcessState::Stopped, state_of),
            ports: Self::unit_ports(workspace, manifest, name),
            log: row.and_then(|row| row.log.clone()),
            ready: row.and_then(ready_of),
            sandbox: row
                .and_then(|row| row.sandbox.as_ref())
                .map(ToString::to_string),
            notes: Vec::new(),
        }
    }

    /// Returns the units that `status` reports: the apps of the manifest's
    /// services, then the manifest's units in start order, then those that
    /// only phase's rows name, in name order.
    fn phase_known(manifest: Option<&Manifest>, rows: &BTreeMap<String, StatusRow>) -> Vec<String> {
        let mut names = manifest.map(Manifest::service_apps).unwrap_or_default();
        names.extend(
            manifest
                .and_then(|manifest| manifest.start_order(&[]).ok())
                .unwrap_or_default(),
        );
        for name in rows.keys() {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
    }

    /// Returns the processes and containers of `workspace` and their
    /// states: the manifest's in start order, then any other that phase's
    /// run record names. Without a manifest it reports only those that a
    /// record names.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`], [`Error::CorruptRecord`] or
    /// [`Error::NewerRecord`] for a record that cannot be read, and
    /// [`Error::ProcessTable`] when the process table cannot be read.
    pub fn status(
        &self,
        workspace: &Workspace,
        manifest: Option<&Manifest>,
    ) -> Result<Vec<ProcessStatus>, Error> {
        let rows = self.phase_rows(workspace, manifest)?;
        Ok(Self::phase_known(manifest, &rows)
            .iter()
            .map(|name| Self::phase_unit_status(workspace, manifest, name, rows.get(name)))
            .collect())
    }

    /// Returns each volume of a container in the `mxv.toml` of `workspace`'s
    /// one checkout for which phase holds a docker volume under the name
    /// without a prefix. Adding a second checkout renames such a volume to
    /// `<prefix>-<volume>`, so the container would start on an empty
    /// volume and the old one would hold its data unused. A workspace with
    /// several finished checkouts, one that has never run, and one that
    /// reads a workspace manifest in place of its checkout's `mxv.toml`
    /// return none.
    ///
    /// # Errors
    ///
    /// Returns the docker errors of looking up a volume.
    pub fn renamed_volumes(&self, workspace: &Workspace) -> Result<Vec<RenamedVolume>, Error> {
        // An unfinished checkout never ran, so the workspace ran as the
        // checkouts that have a commit.
        let mut workspace = workspace.clone();
        workspace
            .checkouts
            .retain(|checkout| checkout.commit.is_some());
        // A workspace manifest's volumes keep their names after `add`, so
        // only a workspace that reads its checkout's own file has volumes
        // to rename.
        if workspace.has_several_checkouts()
            || workspace.manifest_inputs().source() != crate::lookup::ManifestSource::Repository
        {
            return Ok(Vec::new());
        }
        let (Some(checkout), Some(id)) = (workspace.checkouts.first(), workspace.id.as_ref())
        else {
            return Ok(Vec::new());
        };
        let Ok(Some(own)) = Manifest::read(&checkout.path) else {
            return Ok(Vec::new());
        };
        let volumes: BTreeSet<&String> = own
            .containers
            .values()
            .flat_map(|container| container.volumes.keys())
            .collect();
        let prefix = crate::lookup::checkout_prefix(&checkout.name);
        let mut renamed = Vec::new();
        for volume in volumes {
            let moved = crate::lookup::prefixed(&prefix, volume);
            let (Ok(key), Ok(moved_key)) = (VolumeKey::new(volume), VolumeKey::new(&moved)) else {
                continue;
            };
            let (Some(old), Some(new)) = (
                app_volume_name(&self.phase, id, &key),
                app_volume_name(&self.phase, id, &moved_key),
            ) else {
                continue;
            };
            if volume_exists(&old)? {
                renamed.push(RenamedVolume {
                    volume: volume.clone(),
                    old,
                    new,
                    labels: app_volume_labels(&self.phase, id, &moved_key),
                });
            }
        }
        Ok(renamed)
    }

    /// Returns the host processes and containers of `workspace` that
    /// phase runs and that may run.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Runs::status`].
    pub fn footprint(&self, workspace: &Workspace) -> Result<Footprint, Error> {
        let manifest = workspace.manifest().ok().flatten();
        let mut footprint = Footprint::default();
        for row in self.phase_rows(workspace, manifest.as_ref())?.values() {
            if !row.state.may_run() {
                continue;
            }
            match &row.handle {
                Some(Handle::Pid { pid, .. }) => footprint.pids.push(*pid),
                Some(Handle::Container { id, .. }) => {
                    footprint.containers.push(id.as_str().to_owned());
                }
                _ => {}
            }
        }
        Ok(footprint)
    }

    /// Returns the resident memory of the containers of `footprint`, as
    /// docker measures it. It runs no docker command for a footprint
    /// without containers.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DockerMissing`], [`Error::DockerUnavailable`] and
    /// [`Error::DockerFailed`].
    pub fn container_memory(&self, footprint: &Footprint) -> Result<u64, Error> {
        Ok(phase::container::container_memory(&footprint.containers)?
            .values()
            .fold(0_u64, |sum, bytes| sum.saturating_add(*bytes)))
    }

    /// Returns the names of the processes and containers of `workspace`
    /// that may run, in start order.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Runs::status`].
    pub fn running(&self, workspace: &Workspace) -> Result<Vec<String>, Error> {
        let mut running = Vec::new();
        let manifest = workspace.manifest().ok().flatten();
        for (name, row) in self.phase_rows(workspace, manifest.as_ref())? {
            if row.state.may_run() && !running.contains(&name) {
                running.push(name);
            }
        }
        Ok(running)
    }

    /// Starts the processes and containers `names` of `manifest`, or every
    /// one when `names` is empty, with their dependencies first, through
    /// phase. It allocates a host port for each named port that
    /// `workspace` does not hold yet, and saves the workspace's stable id
    /// and ports in `store`.
    ///
    /// phase starts each unit after the units it depends on are ready, and
    /// leaves one that already runs alone. A host process with a readiness
    /// check counts as ready when the check passes within its timeout, and
    /// one without counts as ready once it has run for phase's start grace.
    /// When a unit fails, `up` stops the units that it started before it,
    /// so either every selected unit runs or this run leaves none of its
    /// own running.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Store::list`] and [`Store::update`],
    /// [`Error::NoFreePort`] when no port can be allocated, the errors of
    /// [`Runs::status`], the docker errors, [`Error::ProcessUnstartable`]
    /// for a process that cannot start, and [`Error::Phase`] for another
    /// failure of phase.
    ///
    /// When `up` fails after phase ran, `earlier` receives the notes of the
    /// apps it reached, such as the processes it restarted, which keep
    /// running. An `up` that succeeds puts its notes on each status.
    pub fn up(
        &self,
        passed: BuildsPassed<'_>,
        store: &Store,
        workspace: &mut Workspace,
        manifest: &Manifest,
        names: &[String],
        earlier: &mut Vec<ProcessNote>,
    ) -> Result<UpOutcome, Error> {
        // The token says that the builds passed, and it borrows the lock
        // that `up` holds until it returns.
        drop(passed);
        let order = match manifest.start_order(names) {
            Ok(order) => order,
            Err(unknown) => return Ok(UpOutcome::UnknownProcess(unknown)),
        };
        let (id, built) = self.prepared(store, workspace, manifest)?;
        let built = match built {
            Ok(built) => built,
            Err(unrunnable) => return Ok(UpOutcome::Unrunnable(unrunnable)),
        };
        let containers = built
            .plan
            .apps()
            .iter()
            .any(|app| app.runner == RunnerKind::Container);
        if containers {
            store::replace_file(
                &self.workspace_dir(&workspace.name).join(CONTAINERS_RAN),
                b"",
            )?;
        }
        let ready_wait: Duration = order
            .iter()
            .filter_map(|name| manifest.unit(name).and_then(Unit::ready))
            .map(|ready| ready.timeout)
            .sum();
        let backends = u32::try_from(manifest.backends().len()).unwrap_or(u32::MAX);
        let ready_wait = if backends == 0 {
            ready_wait
        } else {
            ready_wait
                + phase_plan::BACKEND_READY_TIMEOUT * backends
                + phase_plan::PROVISION_TIMEOUT
        };
        let now = Instant::now();
        let deadlines = UpDeadlines::new(now + BUILD_WAIT, now + ready_wait + START_MARGIN);
        let report = self
            .phase
            .up(
                &built.plan,
                &selection(names),
                &built.environment,
                deadlines,
                &Cancel::new(),
            )
            .map_err(|error| self.phase_error(&id, error))?;
        match self.failure(workspace, manifest, &id, &report) {
            Ok(None) => {}
            Ok(Some(outcome)) => {
                earlier.extend(notes_of(&report).into_values().flatten());
                self.roll_back_phase(&id, &report);
                return Ok(outcome);
            }
            Err(error) => {
                earlier.extend(notes_of(&report).into_values().flatten());
                self.roll_back_phase(&id, &report);
                return Err(error);
            }
        }
        let rows = self.phase_rows(workspace, Some(manifest))?;
        let mut notes = notes_of(&report);
        // The apps of the services come first, when this `up` reached them.
        let reached: BTreeSet<&str> = report.apps.iter().map(|app| app.app.as_str()).collect();
        let mut shown: Vec<String> = manifest
            .service_apps()
            .into_iter()
            .filter(|name| reached.contains(name.as_str()))
            .collect();
        shown.extend(order);
        Ok(UpOutcome::Up(
            shown
                .iter()
                .map(|name| {
                    let mut status =
                        Self::phase_unit_status(workspace, Some(manifest), name, rows.get(name));
                    status.notes = notes.remove(name).unwrap_or_default();
                    status
                })
                .collect(),
        ))
    }

    /// Gives `workspace` a stable id and a host port for each named port,
    /// saves them in `store`, and returns the id and the plan of
    /// `manifest`, which fails when phase cannot run the manifest.
    fn prepared(
        &self,
        store: &Store,
        workspace: &mut Workspace,
        manifest: &Manifest,
    ) -> Result<(WorkspaceId, Result<Built, phase_plan::Unrunnable>), Error> {
        let id = saved_workspace_id(store, &workspace.name)?;
        workspace.id = Some(id.clone());
        let held = self
            .phase_rows(workspace, None)?
            .values()
            .any(|row| row.state.may_run());
        self.assign_ports(store, workspace, manifest, &BTreeSet::new(), held)?;
        let built = phase_plan::build(&self.inputs(workspace, &id, manifest));
        Ok((id, built))
    }

    /// Stops the apps that a failed `up` started from nothing, as its
    /// `report` names them. An app that it restarted ran before it, so that
    /// app keeps running. Each step is best effort, because the caller reports the
    /// failure that stopped `up`.
    fn roll_back_phase(&self, id: &WorkspaceId, report: &Report) {
        let started = started_from_nothing(report);
        if started.is_empty() {
            return;
        }
        let deadline = Instant::now() + self.stop_wait(started.len());
        let _ = self.phase.down(id, &Selection::only(started), deadline);
    }

    /// Returns how long phase may take to stop `count` units.
    fn stop_wait(&self, count: usize) -> Duration {
        let each = self.grace + self.phase.kill_wait() + STOP_MARGIN;
        each.saturating_mul(u32::try_from(count.max(1)).unwrap_or(u32::MAX))
    }

    /// Returns the outcome of the first app of `report` that failed to
    /// start, or `None` when every app started or already ran.
    fn failure(
        &self,
        workspace: &Workspace,
        manifest: &Manifest,
        id: &WorkspaceId,
        report: &Report,
    ) -> Result<Option<UpOutcome>, Error> {
        for app in &report.apps {
            let unit = app.app.as_str();
            match &app.outcome {
                AppOutcome::Failed { problem, .. } => {
                    return Ok(Some(
                        self.start_problem(workspace, manifest, id, unit, problem)?,
                    ));
                }
                AppOutcome::Errored {
                    code,
                    message,
                    cause,
                    context,
                    ..
                } => {
                    return Err(if code == "phase::process::unstartable" {
                        Error::ProcessUnstartable {
                            process: unit.to_owned(),
                            source: io::Error::other(message.clone()),
                        }
                    } else {
                        errored(code, message, *cause, context.as_ref()).naming(unit)
                    });
                }
                AppOutcome::StartUncertain => {
                    return Err(Error::Phase {
                        code: "phase::start::uncertain",
                        message: format!(
                            "the deadline of up passed while phase was starting {unit}, so phase does not know whether it runs"
                        ),
                    });
                }
                AppOutcome::Left => {
                    let rows = self.phase_rows(workspace, Some(manifest))?;
                    return Ok(Some(UpOutcome::Left {
                        unstoppable: left(unit, app, rows.get(unit)),
                        not_started: report
                            .apps
                            .iter()
                            .filter(|other| matches!(other.outcome, AppOutcome::NotStarted))
                            .map(|other| other.app.as_str().to_owned())
                            .collect(),
                    }));
                }
                _ => {}
            }
        }
        Ok(None)
    }

    /// Returns the outcome of the unit `unit` that failed to start because
    /// of `problem`.
    fn start_problem(
        &self,
        workspace: &Workspace,
        manifest: &Manifest,
        id: &WorkspaceId,
        unit: &str,
        problem: &StartProblem,
    ) -> Result<UpOutcome, Error> {
        let log = self.phase_log(id, manifest, unit);
        let number = |port: &str| workspace.ports.get(port).copied().unwrap_or_default();
        Ok(match problem {
            StartProblem::PortTaken { port, holders, .. } => {
                let rows = self.phase_rows(workspace, Some(manifest))?;
                UpOutcome::PortTaken {
                    process: unit.to_owned(),
                    port: port.to_string(),
                    number: number(port.as_str()),
                    holders: rows
                        .iter()
                        .filter(|(_, row)| pid(row).is_some_and(|pid| holders.contains(&pid)))
                        .map(|(name, _)| name.clone())
                        .collect(),
                }
            }
            StartProblem::Exited { code, signal, .. } => {
                let (code, signal) = ended(log.is_some(), *code, *signal);
                UpOutcome::Exited {
                    process: unit.to_owned(),
                    code,
                    signal,
                    log,
                }
            }
            StartProblem::ForeignListener { port, holders, .. } => UpOutcome::ForeignListener {
                process: unit.to_owned(),
                port: port.to_string(),
                number: number(port.as_str()),
                pid: holders.first().copied(),
                log,
            },
            StartProblem::NotReady { waited, .. } => {
                let ready = manifest.unit(unit).and_then(Unit::ready);
                let waited_for = match ready.map(|ready| &ready.check) {
                    Some(ReadyCheck::Log(pattern)) => Waited::Log {
                        pattern: pattern.as_str().to_owned(),
                    },
                    Some(ReadyCheck::Port(port)) => Waited::Port {
                        port: port.clone(),
                        number: number(port),
                    },
                    None => Waited::Port {
                        port: String::new(),
                        number: 0,
                    },
                };
                UpOutcome::NotReady {
                    process: unit.to_owned(),
                    waited: waited_for,
                    timeout: ready.map_or(*waited, |ready| ready.timeout),
                    log,
                }
            }
            _ => {
                return Err(Error::Phase {
                    code: "phase::start::failed",
                    message: format!("phase could not start {unit}"),
                });
            }
        })
    }

    /// Returns the log file that phase's keeper gives the host process
    /// `unit`, which is a process of `manifest` or [`PROVISION_APP`] when
    /// the manifest declares services, or `None` for a container, whose
    /// log comes from `docker logs`.
    fn phase_log(&self, id: &WorkspaceId, manifest: &Manifest, unit: &str) -> Option<PathBuf> {
        let provisions = unit == PROVISION_APP && !manifest.services.is_empty();
        if !provisions && !manifest.processes.contains_key(unit) {
            return None;
        }
        Some(
            self.phase
                .state_dir()
                .join("logs")
                .join(id.as_str())
                .join(format!("{unit}.{MAIN}.log")),
        )
    }

    /// Stops the processes and containers `names` of `workspace`, or every
    /// one that phase's run record names when `names` is empty, in the
    /// reverse of their start order. phase sends each process group its
    /// stop signal, then `KILL` when it outlives the grace, and never
    /// signals a group that it cannot prove its own. A process that phase
    /// left running does not stop the others, and its result says so.
    ///
    /// With [`Needed::Keep`] and named units, `down` keeps each named unit
    /// that a running unit outside the stopped set depends on, directly or
    /// through other units, and returns it as [`DownResult::StillNeeded`].
    /// A kept unit still needs what it depends on, so the rule runs until
    /// nothing more is kept. A unit whose state phase cannot tell counts as
    /// running. The dependencies come from phase's run record, so a broken
    /// manifest does not change them. With no names, or with
    /// [`Needed::Stop`], `down` stops every selected unit.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Runs::status`], [`Error::Unwritable`] when a
    /// run file cannot be deleted, the docker errors, and [`Error::Phase`]
    /// for another failure of phase.
    pub fn down(
        &self,
        _lock: &Lock,
        workspace: &Workspace,
        manifest: Option<&Manifest>,
        names: &[String],
        needed: Needed,
    ) -> Result<DownOutcome, Error> {
        let rows = self.phase_rows(workspace, manifest)?;
        let known = Self::phase_known(manifest, &rows);
        if let Some(unknown) = names.iter().find(|name| !known.contains(name)) {
            return Ok(DownOutcome::UnknownProcess(unknown.clone()));
        }
        let Some(id) = &workspace.id else {
            // A workspace without a stable id has never run under phase, so
            // each unit that `down` names is stopped already.
            return Ok(DownOutcome::Down(
                known
                    .iter()
                    .rev()
                    .filter(|name| names.contains(name))
                    .map(|name| {
                        DownResult::Stopped(Self::phase_unit_status(
                            workspace, manifest, name, None,
                        ))
                    })
                    .collect(),
            ));
        };
        let kept = if needed == Needed::Keep && !names.is_empty() {
            self.still_needed(id, &rows, names)?
        } else {
            BTreeMap::new()
        };
        let stopping: Vec<String> = names
            .iter()
            .filter(|name| !kept.contains_key(*name))
            .cloned()
            .collect();
        let mut results: BTreeMap<String, DownResult> = kept
            .into_iter()
            .map(|(name, still)| (name, DownResult::StillNeeded(still)))
            .collect();
        // Every named unit may be kept, and then `down` stops nothing.
        if names.is_empty() || !stopping.is_empty() {
            let count = if names.is_empty() {
                known.len()
            } else {
                stopping.len()
            };
            let deadline = Instant::now() + self.stop_wait(count);
            let report = self
                .phase
                .down(id, &selection(&stopping), deadline)
                .map_err(|error| self.phase_error(id, error))?;
            for app in &report.apps {
                let name = app.app.as_str();
                let result = Self::stopped(workspace, manifest, name, app, rows.get(name))?;
                results.insert(name.to_owned(), result);
            }
        }
        let mut order: Vec<String> = known
            .iter()
            .filter(|name| results.contains_key(*name))
            .cloned()
            .collect();
        order.reverse();
        order.extend(
            results
                .keys()
                .filter(|name| !known.contains(name))
                .cloned()
                .collect::<Vec<_>>(),
        );
        Ok(DownOutcome::Down(
            order
                .iter()
                .filter_map(|name| results.remove(name))
                .collect(),
        ))
    }

    /// Returns each of the units `names` that `down` keeps, because a
    /// running unit outside the stopped set depends on it, by name, from
    /// phase's status rows `rows` of the workspace whose stable id is `id`
    /// and the dependencies of its run record. A kept unit leaves the
    /// stopped set and still needs what it depends on, so the rule runs
    /// until nothing more is kept.
    fn still_needed(
        &self,
        id: &WorkspaceId,
        rows: &BTreeMap<String, StatusRow>,
        names: &[String],
    ) -> Result<BTreeMap<String, StillNeeded>, Error> {
        let running: BTreeSet<AppName> = rows
            .iter()
            .filter(|(_, row)| row.state.may_run())
            .filter_map(|(name, _)| app(name))
            .collect();
        let needed = self
            .phase
            .needed_by(id, &running)
            .map_err(|error| self.phase_error(id, error))?;
        let named: BTreeSet<&str> = names.iter().map(String::as_str).collect();
        // The running units that need `name` and that `down` does not stop
        // while it keeps `kept`.
        let needing = |name: &str, kept: &BTreeSet<String>| -> Vec<String> {
            app(name)
                .and_then(|app| needed.get(&app))
                .into_iter()
                .flatten()
                .map(|dependant| dependant.as_str().to_owned())
                .filter(|dependant| !named.contains(dependant.as_str()) || kept.contains(dependant))
                .collect()
        };
        let mut kept: BTreeSet<String> = BTreeSet::new();
        loop {
            let more: Vec<String> = names
                .iter()
                .filter(|name| !kept.contains(*name) && !needing(name, &kept).is_empty())
                .cloned()
                .collect();
            if more.is_empty() {
                break;
            }
            kept.extend(more);
        }
        Ok(kept
            .iter()
            .map(|name| {
                let by = needing(name, &kept);
                let unknown = by
                    .iter()
                    .filter(|dependant| {
                        rows.get(*dependant).is_some_and(|row| {
                            matches!(row.state, phase::ProcessState::Unknown { .. })
                        })
                    })
                    .cloned()
                    .collect();
                (
                    name.clone(),
                    StillNeeded {
                        unit: name.clone(),
                        by,
                        unknown,
                    },
                )
            })
            .collect())
    }

    /// Returns the result of `down` for the unit `name`, whose app phase
    /// reported as `app`, and whose status row before `down` was `row`.
    fn stopped(
        workspace: &Workspace,
        manifest: Option<&Manifest>,
        name: &str,
        app: &phase::AppReport,
        row: Option<&StatusRow>,
    ) -> Result<DownResult, Error> {
        match &app.outcome {
            AppOutcome::Left => Ok(DownResult::Unstoppable(left(name, app, row))),
            AppOutcome::StopUncertain => Err(Error::Phase {
                code: "phase::stop::uncertain",
                message: format!(
                    "the deadline of down passed while phase was stopping {name}, so phase does not know whether it has stopped"
                ),
            }),
            AppOutcome::Errored {
                code,
                message,
                cause,
                context,
                ..
            } => Err(errored(code, message, *cause, context.as_ref()).naming(name)),
            _ => {
                let mut status = Self::phase_unit_status(workspace, manifest, name, None);
                status.log = row.and_then(|row| row.log.clone());
                Ok(DownResult::Stopped(status))
            }
        }
    }

    /// Passes the last `lines` lines of the log of the process or container
    /// `process` of `workspace` to `output`, and with `follow` each later
    /// line until `output.pass` breaks or nothing can write to the log any
    /// more. It returns `false`, and passes nothing, when the workspace
    /// knows no process or container `process`. A note about the log, such
    /// as that its container is gone, goes to `output.noted`.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Runs::status`], [`Error::StateUnreadable`]
    /// when the log cannot be read, and the docker errors.
    pub fn logs(
        &self,
        workspace: &Workspace,
        manifest: Option<&Manifest>,
        process: &str,
        lines: usize,
        follow: bool,
        output: &mut LogOutput<'_>,
    ) -> Result<bool, Error> {
        let known = self.status(workspace, manifest)?;
        if !known.iter().any(|status| status.name == process) {
            return Ok(false);
        }
        let (Some(id), Some(app), Some(main)) = (&workspace.id, app(process), main_process())
        else {
            return Ok(true);
        };
        let mut request = LogRequest::new(app, main);
        request.follow = follow;
        request.lines = Some(lines);
        match self.phase.logs(id, &request, output) {
            // A unit that has never run has no log yet.
            Ok(()) | Err(PhaseError::UnknownTarget { .. }) => Ok(true),
            Err(error) => Err(self.phase_error(id, error)),
        }
    }

    /// Forgets the stable id `id` of the workspace `name`, as `forget_id`
    /// does without deleting volumes, and then deletes every run file of
    /// the workspace, whatever other files it holds. It returns the
    /// sentences that say why phase kept its run record, which is empty
    /// when phase removed it. The workspace's volumes stay;
    /// [`Runs::delete_volumes`] deletes them.
    ///
    /// # Errors
    ///
    /// Returns the docker errors, [`Error::Unwritable`] when the directory
    /// cannot be deleted, and [`Error::Phase`] for a failure of phase.
    pub fn forget(
        &self,
        name: &WorkspaceName,
        id: Option<&WorkspaceId>,
    ) -> Result<Vec<String>, Error> {
        let held = match id {
            Some(id) => match self.forget_id(id, false)? {
                Forgotten::Held(held) => held,
                Forgotten::Done { .. } => Vec::new(),
            },
            None => Vec::new(),
        };
        let dir = self.workspace_dir(name);
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(held),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(held),
            Err(source) => Err(Error::Unwritable { path: dir, source }),
        }
    }

    /// Stops what phase's run record of the workspace id `id` names and
    /// phase no longer supervises, with phase's sweep, which removes the
    /// record's containers, networks and run volumes and then the record.
    /// With `delete_volumes`, it then deletes the data volumes that docker
    /// labels with the id. It deletes the id's logs last, so a failure
    /// before that leaves them, and a later listing finds the id again.
    /// When phase keeps the record, because it cannot stop an item, such
    /// as a process whose start time it may not read, the volumes and the
    /// logs stay too.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Phase`] for a failure of phase, the docker errors,
    /// and [`Error::Unwritable`] when the logs cannot be deleted.
    pub(crate) fn forget_id(
        &self,
        id: &WorkspaceId,
        delete_volumes: bool,
    ) -> Result<Forgotten, Error> {
        let swept = self
            .phase
            .sweep(id, false)
            .map_err(|error| self.phase_error(id, error))?;
        if self.phase.run_record(id).exists() {
            let mut held: Vec<String> = swept
                .items
                .iter()
                .filter_map(|item| {
                    let what = format!("the process {} of {}", item.process, item.app);
                    match item.outcome {
                        phase::SweepOutcome::Unknown => Some(format!(
                            "phase cannot tell whether {what} is the one it started"
                        )),
                        phase::SweepOutcome::Unstoppable => {
                            Some(format!("{what} was still there after phase killed it"))
                        }
                        _ => None,
                    }
                })
                .collect();
            if held.is_empty() {
                held.push("phase could not stop a process that it names".to_owned());
            }
            return Ok(Forgotten::Held(held));
        }
        let mut volumes = Vec::new();
        if delete_volumes {
            let found = self.archived_volumes(id)?;
            volumes = found.names();
            self.delete_volumes(found)?;
        }
        let logs = self.logs_dir(id);
        match fs::remove_dir_all(&logs) {
            Ok(()) => Ok(Forgotten::Done { volumes }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(Forgotten::Done { volumes })
            }
            Err(source) => Err(Error::Unwritable { path: logs, source }),
        }
    }

    /// Returns each workspace id that docker labels a data volume of this
    /// phase with, with the names of those volumes, in id order, whether or
    /// not phase keeps a run record or logs of it, from one `docker volume
    /// ls`. A volume that another state directory's phase made is left out.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DockerMissing`], [`Error::DockerUnavailable`] and
    /// [`Error::DockerFailed`].
    pub(crate) fn volume_ids(&self) -> Result<Vec<(WorkspaceId, Vec<String>)>, Error> {
        Ok(phase::container::labelled_volumes(&self.phase)?)
    }

    /// Returns the directory of phase's logs of the workspace id `id`.
    fn logs_dir(&self, id: &WorkspaceId) -> PathBuf {
        self.phase.state_dir().join("logs").join(id.as_str())
    }

    /// Returns the paths of phase's run record and logs of the workspace
    /// id `id`, those that exist.
    pub(crate) fn phase_paths(&self, id: &WorkspaceId) -> Vec<PathBuf> {
        [self.phase.run_record(id), self.logs_dir(id)]
            .into_iter()
            .filter(|path| fs::symlink_metadata(path).is_ok())
            .collect()
    }

    /// Returns each directory of run files, `<runs>/<name>`, whose name is
    /// a workspace's name and that holds more than the workspace's lock,
    /// in name order. A command that takes the lock of a name that has no
    /// workspace, such as `mxv archive`, leaves the lock file, which holds
    /// nothing, so a directory with only that file is left out.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when a directory cannot be read.
    pub(crate) fn run_dirs(&self) -> Result<Vec<(WorkspaceName, PathBuf)>, Error> {
        let mut found = Vec::new();
        for entry in store::entries(&self.dir)? {
            let Ok(name) = entry.name.parse::<WorkspaceName>() else {
                continue;
            };
            if !entry.dir {
                continue;
            }
            let more = store::entries(&entry.path)?
                .iter()
                .any(|file| file.name != super::LOCK_FILE);
            if more {
                found.push((name, entry.path));
            }
        }
        Ok(found)
    }

    /// Returns each workspace id that phase keeps a run record or logs of,
    /// in id order.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when a directory cannot be read.
    pub(crate) fn phase_ids(&self) -> Result<Vec<WorkspaceId>, Error> {
        let state = self.phase.state_dir();
        let records = store::entries(&state.join("runs"))?
            .into_iter()
            .filter_map(|entry| entry.name.strip_suffix(".json").map(str::to_owned));
        let logs = store::entries(&state.join("logs"))?
            .into_iter()
            .map(|entry| entry.name);
        let ids: BTreeSet<WorkspaceId> = records
            .chain(logs)
            .filter_map(|text| WorkspaceId::try_from(text).ok())
            .collect();
        Ok(ids.into_iter().collect())
    }

    /// Returns the docker containers of `workspace`, running or stopped,
    /// which a removal removes, in name order. It runs no docker command
    /// for a workspace that phase has not run.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when a run directory cannot be
    /// read, and the errors of [`Runs::status`].
    pub fn docker_containers(&self, workspace: &Workspace) -> Result<Vec<String>, Error> {
        let mut containers = BTreeSet::new();
        let manifest = workspace.manifest().ok().flatten();
        for (name, row) in self.phase_rows(workspace, manifest.as_ref())? {
            if row.runner == RunnerKind::Container && row.handle.is_some() {
                containers.insert(name);
            }
        }
        Ok(containers.into_iter().collect())
    }

    /// Returns the volumes of `workspace` that a removal deletes: the data
    /// volumes of its container apps, which docker labels with its stable
    /// id. It runs no docker command for a workspace without a stable id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DockerMissing`], [`Error::DockerUnavailable`] and
    /// [`Error::DockerFailed`].
    pub fn volumes(&self, workspace: &Workspace) -> Result<Volumes, Error> {
        let ran = self
            .workspace_dir(&workspace.name)
            .join(CONTAINERS_RAN)
            .exists();
        let apps = match &workspace.id {
            Some(id) if ran => app_volumes(&self.phase, id)?,
            _ => WorkspaceVolumes::default(),
        };
        Ok(Volumes { apps })
    }

    /// Reports whether any container of the workspace `name` has run, so
    /// docker may hold volumes labelled with its stable id.
    #[must_use]
    pub fn containers_ran(&self, name: &WorkspaceName) -> bool {
        self.workspace_dir(name).join(CONTAINERS_RAN).exists()
    }

    /// Notes that containers of the workspace `name` have run, for a
    /// workspace that a restore rebuilt, so its volumes count again.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unwritable`] when the note cannot be written.
    pub fn remember_containers(&self, name: &WorkspaceName) -> Result<(), Error> {
        let dir = self.workspace_dir(name);
        fs::create_dir_all(&dir).map_err(|source| Error::Unwritable {
            path: dir.clone(),
            source,
        })?;
        store::replace_file(&dir.join(CONTAINERS_RAN), b"")
    }

    /// Returns the docker volumes of an archived workspace, which docker
    /// labels with its stable id `id`. The archive deleted its run
    /// directory, so this lookup does not need the note that containers ran.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DockerMissing`], [`Error::DockerUnavailable`] and
    /// [`Error::DockerFailed`].
    pub fn archived_volumes(&self, id: &WorkspaceId) -> Result<Volumes, Error> {
        Ok(Volumes {
            apps: app_volumes(&self.phase, id)?,
        })
    }

    /// Deletes `volumes`, which [`Runs::volumes`] returned for one
    /// workspace, after it checked each one again.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DockerForeign`], [`Error::DockerMissing`],
    /// [`Error::DockerUnavailable`] and [`Error::DockerFailed`].
    pub fn delete_volumes(&self, volumes: Volumes) -> Result<(), Error> {
        delete_app_volumes(volumes.apps)?;
        Ok(())
    }
}

/// Returns the error of an app that phase reported as errored with
/// `code`, `message`, and for a Docker failure its `cause` and `context`:
/// the docker failure that they describe, and [`Error::Phase`] otherwise.
fn errored(
    code: &str,
    message: &str,
    cause: Option<phase::DockerCause>,
    context: Option<&phase::DockerContext>,
) -> Error {
    match (cause, context) {
        (Some(cause), Some(context)) => Error::of_docker(phase_code(code), cause, context, message),
        _ => Error::Phase {
            code: phase_code(code),
            message: message.to_owned(),
        },
    }
}

/// Returns phase's code `code` as a static string when it is one of
/// phase's codes, and `phase::failed` otherwise.
fn phase_code(code: &str) -> &'static str {
    const CODES: &[&str] = &[
        "phase::not_built",
        "phase::environment",
        "phase::lock::busy",
        "phase::record::unreadable",
        "phase::record::newer",
        "phase::state::unwritable",
        "phase::log::unreadable",
        "phase::process_table",
        "phase::docker::refused",
        "phase::docker::unavailable",
        "phase::keeper::unstartable",
        "phase::process::unstartable",
        "phase::app::not_runnable",
        "phase::select::unknown",
        "phase::exec::not_running",
        "phase::exec::unstartable",
        "phase::exec::sandboxed_terminal",
        "phase::record::unplanned",
    ];
    CODES
        .iter()
        .copied()
        .find(|known| *known == code)
        .unwrap_or("phase::failed")
}
