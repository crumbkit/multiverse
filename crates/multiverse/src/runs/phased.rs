//! `up`, `down`, `restart`, `status` and `logs` of a workspace's app, which
//! phase runs from the plan that `phase_plan` builds.
//!
//! A workspace whose processes or containers mxv started before phase ran
//! its apps keeps its run files in `<runs>/<workspace>/`. While such a
//! file exists, `status`, `down` and `logs` read it as mxv did before, and
//! `up` refuses to start anything while one of those processes or
//! containers may still run, with [`UpOutcome::LegacyRunning`]. Once
//! nothing of them runs, `up` removes their containers and their files,
//! apart from the lock and the record of their volumes, and phase runs the
//! app from then on.

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
    AppName, AppOutcome, Cancel, Handle, LineSink, LogLine, LogNote, LogRequest, Note, PhaseError,
    ProcessName, Report, RunnerKind, Sandboxing, Selection, StartProblem, StatusRow, Target,
    UnknownReason, UpDeadlines, WorkspaceId,
};

use super::{
    DownOutcome, ExitReason, Follower, Footprint, Kind, ProcessNote, ProcessState, ProcessStatus,
    RenamedVolume, Runs, Unstoppable, UpOutcome, Waited, code_and_signal, received, tail,
};
use crate::error::Error;
use crate::manifest::{Manifest, ReadyCheck, Unit};
use crate::name::WorkspaceName;
use crate::phase_plan::{self, Built, Inputs, MAIN};
use crate::record::Workspace;
use crate::run_files::RunHost;
use crate::service::PROVISION_APP;
use crate::store::{self, Lock, Store};

/// How long `up` gives phase to build each selected checkout. mxv's apps
/// run from their checkouts without a build, so phase does not wait for
/// it.
const BUILD_WAIT: Duration = Duration::from_secs(3600);

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

/// The files of a workspace's run directory that stay once nothing that
/// mxv started before phase ran its apps runs: the lock that `up` and
/// `down` take, the record of the older containers' volumes, which
/// `mxv remove --delete-volumes` reads, and [`CONTAINERS_RAN`].
const KEPT_FILES: &[&str] = &[".lock", ".volumes", CONTAINERS_RAN];

mod exec;
#[cfg(test)]
mod tests;

pub use exec::{Exec, ExecEnd, ExecTarget};

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

/// Returns the notes of `report` that mxv shows, by unit: the secret files
/// that `up` rewrote or kept for each app.
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

/// The reason that phase gives for a host process whose app declares no
/// permissions. The end-to-end tests of `status` hold the same sentence,
/// so they fail if phase changes it.
const UNDECLARED: &str = "its app declares no permissions";

/// Returns the warning for the host process `unit` of the workspace
/// `workspace`, which `up` found already running, when phase recorded it
/// as started with or without permissions and its manifest now asks for
/// the other: `wants` is true when the manifest gives it a `permissions`
/// table. It returns `None` when the two agree, or when `row` records no
/// sandbox state. A process recorded as unsandboxed for another reason,
/// such as a platform without a sandbox, declared its permissions.
fn stale_sandbox(workspace: &str, unit: &str, wants: bool, row: &StatusRow) -> Option<ProcessNote> {
    let declared = !matches!(
        row.sandbox.as_ref()?,
        Sandboxing::Unsandboxed { reason, .. } if reason == UNDECLARED
    );
    let restart = format!("mxv restart -w {workspace} {unit}");
    match (wants, declared) {
        (true, false) => Some(ProcessNote::SandboxStale {
            message: format!(
                "{unit} still runs unsandboxed, because it was running before its manifest gave it permissions; run `{restart}` to run it under the sandbox"
            ),
        }),
        (false, true) => Some(ProcessNote::SandboxStale {
            message: format!(
                "{unit} still runs as its earlier permissions allowed, because it was running before its manifest dropped them; run `{restart}` to run it unsandboxed"
            ),
        }),
        _ => None,
    }
}

/// Returns the exit code and the signal that mxv reports for a process
/// that phase saw end with `code` or `signal`, which is a host process
/// when `host`. A host process that runs a shell, such as `sh -c`, may end
/// with a code from 129 to 192, which mxv reads as 128 plus the number of
/// the signal that ended the shell's program, as it did before phase ran
/// its processes. A container's code stays the one docker reports.
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
        _ => "phase cannot tell what the process is doing",
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
    /// The workspace that the volumes belong to.
    workspace: WorkspaceName,
    /// The volumes' docker names, in name order.
    names: Vec<String>,
    /// The volumes of the containers that mxv started before phase ran
    /// the workspace's apps.
    legacy: WorkspaceVolumes,
    /// The volumes of the workspace's container apps.
    apps: WorkspaceVolumes,
}

impl Volumes {
    /// Returns the volumes' docker names, in name order.
    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// Reports whether the workspace has no volume to delete.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

impl Runs {
    /// Returns the failure of `error`, which phase reported for the
    /// workspace whose stable id is `id`.
    fn phase_error(&self, id: &WorkspaceId, error: PhaseError) -> Error {
        Error::of_phase(error, self.phase.run_record(id))
    }

    /// Reports whether the workspace `name` has run files of processes or
    /// containers that mxv started before phase ran its apps.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when the run files cannot be
    /// read, and the errors of reading docker's containers.
    pub fn has_legacy(&self, name: &WorkspaceName) -> Result<bool, Error> {
        Ok(!self.recorded(name)?.is_empty() || !self.existing_containers(name)?.is_empty())
    }

    /// Returns the processes and containers of `workspace` that mxv
    /// started before phase ran its apps and that may still run, in the
    /// order [`Runs::running`] gives. `up` and `restart` start nothing
    /// while one is listed.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Runs::status`].
    pub fn legacy_live(&self, workspace: &Workspace) -> Result<Vec<String>, Error> {
        if !self.has_legacy(&workspace.name)? {
            return Ok(Vec::new());
        }
        self.legacy_running(workspace)
    }

    /// Removes the containers and the run files of the workspace `name`
    /// that mxv started before phase ran its apps, once none of them runs.
    /// The lock and the record of the older containers' volumes stay.
    fn drop_legacy(&self, name: &WorkspaceName) -> Result<(), Error> {
        let lifecycle = self.lifecycle(name);
        if !lifecycle.recorded()?.is_empty()
            && let Some(prepared) = lifecycle.existing()?
        {
            lifecycle.stop_all(&prepared)?;
        }
        let dir = self.workspace_dir(name);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(Error::StateUnreadable { path: dir, source }),
        };
        for entry in entries {
            let entry = entry.map_err(|source| Error::StateUnreadable {
                path: dir.clone(),
                source,
            })?;
            let kept = entry
                .file_name()
                .to_str()
                .is_some_and(|file| KEPT_FILES.contains(&file));
            if kept {
                continue;
            }
            let path = entry.path();
            let removed = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            match removed {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(source) => return Err(Error::Unwritable { path, source }),
            }
        }
        Ok(())
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
            group_alive: None,
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
    /// run record or an older run file names. Without a manifest it
    /// reports only those that a record names. A workspace with run files
    /// of processes that mxv started before phase ran its apps reports
    /// those files, as mxv did before.
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
        if self.has_legacy(&workspace.name)? {
            return self.legacy_status(workspace, manifest);
        }
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
    /// phase runs and that may run. A process or container that mxv
    /// started before phase ran its apps is left out, so a reclaim counts
    /// no memory for it.
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
        let mut running = self.legacy_live(workspace)?;
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
    /// `up` starts nothing while a process or container that mxv started
    /// before phase ran its apps may still run, and nothing when a
    /// container's volume holds data under the older name and phase's
    /// volume of that name does not exist.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Store::list`] and [`Store::update`],
    /// [`Error::NoFreePort`] when no port can be allocated, the errors of
    /// [`Runs::status`], the docker errors, [`Error::ProcessUnstartable`]
    /// for a process that cannot start, and [`Error::Phase`] for another
    /// failure of phase.
    pub fn up(
        &self,
        _lock: &Lock,
        store: &Store,
        workspace: &mut Workspace,
        manifest: &Manifest,
        names: &[String],
    ) -> Result<UpOutcome, Error> {
        let order = match manifest.start_order(names) {
            Ok(order) => order,
            Err(unknown) => return Ok(UpOutcome::UnknownProcess(unknown)),
        };
        if self.has_legacy(&workspace.name)? {
            let live = self.legacy_running(workspace)?;
            if !live.is_empty() {
                return Ok(UpOutcome::LegacyRunning { processes: live });
            }
            self.drop_legacy(&workspace.name)?;
        }
        let host = RunHost {
            records: Some(store),
        };
        let saved = self.lifecycle_with(&host, &workspace.name).saved_id()?;
        workspace.id = Some(saved.id().clone());
        let id = saved.id().clone();
        if let Some(outcome) = self.unmoved_volume(&workspace.name, &id, manifest, &order)? {
            return Ok(outcome);
        }
        let held = self
            .phase_rows(workspace, None)?
            .values()
            .any(|row| row.state.may_run());
        self.assign_ports(store, workspace, manifest, &BTreeSet::new(), held)?;
        let built = match phase_plan::build(&self.inputs(workspace, &id, manifest)) {
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
                self.roll_back_phase(&id, &report);
                return Ok(outcome);
            }
            Err(error) => {
                self.roll_back_phase(&id, &report);
                return Err(error);
            }
        }
        let rows = self.phase_rows(workspace, Some(manifest))?;
        let mut notes = notes_of(&report);
        for app in &report.apps {
            let unit = app.app.as_str();
            if app.outcome != AppOutcome::AlreadyRunning {
                continue;
            }
            let (Some(process), Some(row)) = (manifest.processes.get(unit), rows.get(unit)) else {
                continue;
            };
            if let Some(note) = stale_sandbox(
                workspace.name.as_str(),
                unit,
                process.permissions.is_some(),
                row,
            ) {
                notes.entry(unit.to_owned()).or_default().push(note);
            }
        }
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

    /// Returns the refusal of a selected container whose volume holds data
    /// under the name that mxv gave it before phase ran its apps, when
    /// phase's volume of that name does not exist, or `None`.
    fn unmoved_volume(
        &self,
        name: &WorkspaceName,
        id: &WorkspaceId,
        manifest: &Manifest,
        order: &[String],
    ) -> Result<Option<UpOutcome>, Error> {
        let mounted: Vec<(&String, &String)> = order
            .iter()
            .filter_map(|unit| manifest.containers.get(unit).map(|found| (unit, found)))
            .flat_map(|(unit, container)| {
                container.volumes.keys().map(move |volume| (unit, volume))
            })
            .collect();
        if mounted.is_empty() {
            return Ok(None);
        }
        let lifecycle = self.lifecycle(name);
        let Some(prepared) = lifecycle.existing()? else {
            return Ok(None);
        };
        for (container, volume) in mounted {
            let Ok(key) = VolumeKey::new(volume) else {
                continue;
            };
            let Some(legacy) = lifecycle.legacy_volume(&prepared, &key)? else {
                continue;
            };
            let Some(phase) = app_volume_name(&self.phase, id, &key) else {
                continue;
            };
            if !volume_exists(&phase)? {
                return Ok(Some(UpOutcome::VolumeNotMoved {
                    container: container.clone(),
                    volume: volume.clone(),
                    legacy,
                    phase,
                    labels: app_volume_labels(&self.phase, id, &key),
                }));
            }
        }
        Ok(None)
    }

    /// Stops the apps that a failed `up` started, as its `report` names
    /// them. Each step is best effort, because the caller reports the
    /// failure that stopped `up`.
    fn roll_back_phase(&self, id: &WorkspaceId, report: &Report) {
        let started: Vec<Target> = report
            .apps
            .iter()
            .filter(|app| matches!(app.outcome, AppOutcome::Started))
            .map(|app| Target {
                app: app.app.clone(),
                process: None,
            })
            .collect();
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
                AppOutcome::AwaitingGrant { .. } => {
                    return Err(Error::Phase {
                        code: "phase::grant::awaiting",
                        message: format!("phase waits for a grant of {unit}'s permissions"),
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
    /// left running does not stop the others, and its result says so. A
    /// workspace with run files of processes that mxv started before phase
    /// ran its apps stops those as mxv did before, and once none of them
    /// is left, deletes their files apart from the lock and the volumes'
    /// record.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Runs::status`], [`Error::Unwritable`] when a
    /// run file cannot be deleted, the docker errors, and [`Error::Phase`]
    /// for another failure of phase.
    pub fn down(
        &self,
        lock: &Lock,
        workspace: &Workspace,
        manifest: Option<&Manifest>,
        names: &[String],
    ) -> Result<DownOutcome, Error> {
        if self.has_legacy(&workspace.name)? {
            let outcome = self.legacy_down(lock, workspace, manifest, names)?;
            if !self.has_legacy(&workspace.name)? {
                self.drop_legacy(&workspace.name)?;
            }
            return Ok(outcome);
        }
        let rows = self.phase_rows(workspace, manifest)?;
        let known = Self::phase_known(manifest, &rows);
        if let Some(unknown) = names.iter().find(|name| !known.contains(name)) {
            return Ok(DownOutcome::UnknownProcess(unknown.clone()));
        }
        let Some(id) = &workspace.id else {
            // A workspace without a stable id has never run under phase, so
            // each unit that `down` names is stopped already, as it was for
            // the runner before phase.
            return Ok(DownOutcome::Down(
                known
                    .iter()
                    .rev()
                    .filter(|name| names.contains(name))
                    .map(|name| Ok(Self::phase_unit_status(workspace, manifest, name, None)))
                    .collect(),
            ));
        };
        let count = if names.is_empty() {
            known.len()
        } else {
            names.len()
        };
        let deadline = Instant::now() + self.stop_wait(count);
        let report = self
            .phase
            .down(id, &selection(names), deadline)
            .map_err(|error| self.phase_error(id, error))?;
        let mut by_app: BTreeMap<String, &phase::AppReport> = report
            .apps
            .iter()
            .map(|app| (app.app.as_str().to_owned(), app))
            .collect();
        let mut order: Vec<String> = known
            .iter()
            .filter(|name| by_app.contains_key(*name))
            .cloned()
            .collect();
        order.reverse();
        order.extend(
            by_app
                .keys()
                .filter(|name| !known.contains(name))
                .cloned()
                .collect::<Vec<_>>(),
        );
        let mut results = Vec::new();
        for name in order {
            let Some(app) = by_app.remove(&name) else {
                continue;
            };
            results.push(Self::stopped(
                workspace,
                manifest,
                &name,
                app,
                rows.get(&name),
            )?);
        }
        Ok(DownOutcome::Down(results))
    }

    /// Returns the result of `down` for the unit `name`, whose app phase
    /// reported as `app`, and whose status row before `down` was `row`.
    fn stopped(
        workspace: &Workspace,
        manifest: Option<&Manifest>,
        name: &str,
        app: &phase::AppReport,
        row: Option<&StatusRow>,
    ) -> Result<Result<ProcessStatus, Unstoppable>, Error> {
        match &app.outcome {
            AppOutcome::Left => Ok(Err(left(name, app, row))),
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
                Ok(Ok(status))
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
        if self.has_legacy(&workspace.name)? {
            return self.legacy_logs(workspace, manifest, process, lines, follow, output.pass);
        }
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

    /// Passes the log of `process` that mxv wrote before phase ran the
    /// workspace's apps to `pass`, as [`Runs::logs`] describes.
    fn legacy_logs(
        &self,
        workspace: &Workspace,
        manifest: Option<&Manifest>,
        process: &str,
        lines: usize,
        follow: bool,
        pass: &mut dyn FnMut(String) -> ControlFlow<()>,
    ) -> Result<bool, Error> {
        let known = self.legacy_status(workspace, manifest)?;
        if !known.iter().any(|status| status.name == process) {
            return Ok(false);
        }
        let log = self.log(&workspace.name, process);
        let mut follower = Follower::at_end(&log)?;
        for line in tail(&log, lines) {
            if pass(line).is_break() {
                return Ok(true);
            }
        }
        if !follow {
            return Ok(true);
        }
        while let Some(reader) = follower.as_mut() {
            let read = reader.lines()?;
            let quiet = read.is_empty();
            for line in read {
                if pass(line).is_break() {
                    return Ok(true);
                }
            }
            if quiet {
                // The follow ends once the log is gone or nothing can write
                // to it any more. A read that returned lines is followed by
                // another, so the last lines before the end are passed.
                if !(self.legacy_writing(&workspace.name, process)? && log.exists()) {
                    break;
                }
                reader.wait();
            }
        }
        Ok(true)
    }

    /// Stops what phase's run record of the workspace `name`, whose stable
    /// id is `id`, names and phase no longer supervises, removes the stopped containers that mxv started
    /// before phase ran its apps, and deletes every run file and log of
    /// the workspace. The workspace's volumes stay; [`Runs::delete_volumes`]
    /// deletes them.
    ///
    /// # Errors
    ///
    /// Returns the docker errors, [`Error::Unwritable`] when the directory
    /// cannot be deleted, and [`Error::Phase`] for a failure of phase.
    pub fn forget(&self, name: &WorkspaceName, id: Option<&WorkspaceId>) -> Result<(), Error> {
        if let Some(id) = id {
            self.phase
                .sweep(id, false)
                .map_err(|error| self.phase_error(id, error))?;
            let logs = self.phase.state_dir().join("logs").join(id.as_str());
            match fs::remove_dir_all(&logs) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(source) => return Err(Error::Unwritable { path: logs, source }),
            }
        }
        self.legacy_forget(name)
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
        let mut containers: BTreeSet<String> = self
            .legacy_docker_containers(&workspace.name)?
            .into_iter()
            .collect();
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
    /// id, and the volumes of the containers that mxv started before phase
    /// ran its apps. It runs no docker command for a workspace without a
    /// stable id or records of older containers or volumes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DockerMissing`], [`Error::DockerUnavailable`] and
    /// [`Error::DockerFailed`].
    pub fn volumes(&self, workspace: &Workspace) -> Result<Volumes, Error> {
        let legacy = self.legacy_volumes(&workspace.name)?;
        let ran = self
            .workspace_dir(&workspace.name)
            .join(CONTAINERS_RAN)
            .exists();
        let apps = match &workspace.id {
            Some(id) if ran => app_volumes(&self.phase, id)?,
            _ => WorkspaceVolumes::default(),
        };
        let mut names = legacy.names();
        names.extend(apps.names());
        names.sort();
        Ok(Volumes {
            workspace: workspace.name.clone(),
            names,
            legacy,
            apps,
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
        if volumes.is_empty() {
            return Ok(());
        }
        delete_app_volumes(volumes.apps)?;
        self.legacy_delete_volumes(&volumes.workspace, volumes.legacy)
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
