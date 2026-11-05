//! The store of run records, which is the one module of phase that writes
//! durable files (DUR-1) and takes file locks (DUR-2).
//!
//! The record of a workspace is `<state_dir>/runs/<workspace-id>.json`. The
//! store writes it to a temporary file beside it with mode 0600, syncs the
//! file, renames it over the record and syncs the directory, so a reader
//! sees the old record or the new one and never part of one. A reader
//! opens the record read-only and takes no lock.
//!
//! The lifecycle lock of a workspace is an exclusive `flock` on
//! `<state_dir>/runs/<workspace-id>.lock`, which `up`, `down`, `restart`
//! and `sweep` hold while they change what runs. The operating system
//! releases it when the file closes, which happens when the
//! [`LifecycleLock`] drops or the process that holds it ends, so a crashed
//! phase leaves no stale lock. The lock file is never deleted.
//!
//! This follows multiverse's state store.

use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::clock::{POLL_INTERVAL, now, pause};
use crate::error::PhaseError;
use crate::name::WorkspaceId;
use crate::name::{AppName, ProcessName};
use crate::record::run::{
    Ended, EntryState, HostProcess, Launching, ObjectKey, RUN_VERSION, Ready, RunRecord, RunSchema,
    Started, Unplanned,
};

#[cfg(test)]
mod tests;

/// The most bytes of a run record that the store reads. A larger record is
/// unreadable.
const MOST_RECORD_BYTES: u64 = 1024 * 1024;

/// The schema prefix of every version of the run record.
const SCHEMA_PREFIX: &str = "phase/run/v";

/// The run records and lifecycle locks under one state directory.
#[derive(Clone, Debug)]
pub(crate) struct RunStore {
    /// The directory `<state_dir>/runs`.
    dir: PathBuf,
}

/// A held lifecycle lock, which the operating system releases when it
/// drops.
#[derive(Debug)]
#[must_use = "the lock is released when it drops"]
pub(crate) struct LifecycleLock {
    /// The locked file, which holds the lock while it is open.
    _file: File,
}

/// The part of a record that the store reads first, so a record of a newer
/// version is refused before its other fields are read.
#[derive(Deserialize)]
struct Versioned {
    /// The record's schema, such as `phase/run/v1`.
    schema: String,
}

impl RunStore {
    /// Returns the store of the run records under `state_dir`.
    pub(crate) fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join("runs"),
        }
    }

    /// Returns the path of the record of `workspace`.
    pub(crate) fn path(&self, workspace: &WorkspaceId) -> PathBuf {
        self.dir.join(format!("{workspace}.json"))
    }

    /// Returns the path of the lock file of `workspace`.
    fn lock_path(&self, workspace: &WorkspaceId) -> PathBuf {
        self.dir.join(format!("{workspace}.lock"))
    }

    /// Returns the record of `workspace`, or `None` when it has none. It
    /// opens the record read-only and takes no lock.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::NewerRecord`] when a newer phase wrote the
    /// record, and [`PhaseError::RecordUnreadable`] when it cannot be read
    /// or is no run record of `workspace`.
    pub(crate) fn read(&self, workspace: &WorkspaceId) -> Result<Option<RunRecord>, PhaseError> {
        let path = self.path(workspace);
        let unreadable = |reason: String| PhaseError::RecordUnreadable {
            workspace: workspace.clone(),
            path: path.clone(),
            reason,
        };
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(unreadable(error.to_string())),
        };
        let mut bytes = Vec::new();
        file.take(MOST_RECORD_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| unreadable(error.to_string()))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MOST_RECORD_BYTES {
            return Err(unreadable(format!(
                "it holds more than {MOST_RECORD_BYTES} bytes"
            )));
        }
        let versioned: Versioned =
            serde_json::from_slice(&bytes).map_err(|error| unreadable(error.to_string()))?;
        let version = versioned
            .schema
            .strip_prefix(SCHEMA_PREFIX)
            .and_then(|version| version.parse::<u32>().ok())
            .ok_or_else(|| {
                unreadable(format!(
                    "its schema {:?} is no run record",
                    versioned.schema
                ))
            })?;
        if version > RUN_VERSION {
            return Err(PhaseError::NewerRecord {
                workspace: workspace.clone(),
                path,
                schema: versioned.schema,
            });
        }
        let mut record: RunRecord =
            serde_json::from_slice(&bytes).map_err(|error| unreadable(error.to_string()))?;
        // An older record reads as the current version with the fields it
        // lacks at their defaults, and phase writes it back as the current
        // version.
        record.schema = RunSchema::CURRENT;
        if &record.workspace != workspace {
            return Err(unreadable(format!(
                "it names the workspace {}",
                record.workspace
            )));
        }
        Ok(Some(record))
    }

    /// Writes `record` over the record of its workspace in one step. The
    /// caller holds the workspace's lifecycle lock.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::StateUnwritable`] when a step fails. The
    /// temporary file is removed after a failed step.
    #[cfg(test)]
    pub(crate) fn write(&self, record: &RunRecord, lock: &LifecycleLock) -> Result<(), PhaseError> {
        self.replace(record, lock)?;
        self.sync_dir()
    }

    /// Writes `record` to a temporary file and renames it over the record
    /// of its workspace, without syncing the directory. The caller holds
    /// the workspace's lifecycle lock.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::StateUnwritable`] when a step fails. The
    /// temporary file is removed after a failed step.
    fn replace(&self, record: &RunRecord, _lock: &LifecycleLock) -> Result<(), PhaseError> {
        let target = self.path(&record.workspace);
        let unwritable = |path: &Path, source| PhaseError::StateUnwritable {
            path: path.to_path_buf(),
            source,
        };
        let mut bytes = serde_json::to_vec_pretty(record)
            .map_err(|error| unwritable(&target, io::Error::other(error)))?;
        bytes.push(b'\n');
        self.create_dir()?;
        let temp = self.dir.join(format!(
            ".{}.json.{}.tmp",
            record.workspace,
            std::process::id()
        ));
        // A temporary file that a crashed write of this process id left is
        // removed, so `create_new` below never meets it.
        match fs::remove_file(&temp) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(unwritable(&temp, error)),
        }
        #[expect(
            clippy::disallowed_methods,
            reason = "crates/phase/src/record/store.rs is the module of phase that writes durable files (DUR-1)"
        )]
        let opened = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(no_follow())
            .mode(0o600)
            .open(&temp);
        let written =
            opened.and_then(|mut file| file.write_all(&bytes).and_then(|()| file.sync_all()));
        if let Err(error) = written {
            let _ = fs::remove_file(&temp);
            return Err(unwritable(&temp, error));
        }
        #[expect(
            clippy::disallowed_methods,
            reason = "crates/phase/src/record/store.rs is the module of phase that writes durable files (DUR-1)"
        )]
        let renamed = fs::rename(&temp, &target);
        if let Err(error) = renamed {
            let _ = fs::remove_file(&temp);
            return Err(unwritable(&target, error));
        }
        Ok(())
    }

    /// Deletes the record of `workspace`. Deleting a record that does not
    /// exist succeeds. The caller holds the workspace's lifecycle lock.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::StateUnwritable`] when the record cannot be
    /// deleted.
    pub(crate) fn remove(
        &self,
        workspace: &WorkspaceId,
        _lock: &LifecycleLock,
    ) -> Result<(), PhaseError> {
        let path = self.path(workspace);
        match fs::remove_file(&path) {
            Ok(()) => self.sync_dir(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(PhaseError::StateUnwritable { path, source }),
        }
    }

    /// Takes the lifecycle lock of `workspace`. When another holder has
    /// it, it tries again every [`POLL_INTERVAL`] until `wait` passes.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::Busy`] when another holder keeps the lock for
    /// all of `wait`, and [`PhaseError::StateUnwritable`] when the lock
    /// file cannot be opened or locked.
    pub(crate) fn lock(
        &self,
        workspace: &WorkspaceId,
        wait: Duration,
    ) -> Result<LifecycleLock, PhaseError> {
        self.lock_unless(workspace, wait, &|| false)
    }

    /// Takes the lifecycle lock of `workspace` as [`RunStore::lock`] does,
    /// and stops waiting after the first failed try once `give_up` holds.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`RunStore::lock`], and [`PhaseError::Busy`]
    /// when `give_up` holds while another holder keeps the lock.
    pub(crate) fn lock_unless(
        &self,
        workspace: &WorkspaceId,
        wait: Duration,
        give_up: &dyn Fn() -> bool,
    ) -> Result<LifecycleLock, PhaseError> {
        let path = self.lock_path(workspace);
        self.create_dir()?;
        #[expect(
            clippy::disallowed_methods,
            reason = "crates/phase/src/record/store.rs is the module of phase that writes durable files (DUR-1)"
        )]
        let opened = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path);
        let file = opened.map_err(|source| PhaseError::StateUnwritable {
            path: path.clone(),
            source,
        })?;
        let deadline = now() + wait;
        // Another process releases a `flock` with no event that this
        // process can wait on, so the lock polls (BASE-11).
        loop {
            #[expect(
                clippy::disallowed_methods,
                reason = "crates/phase/src/record/store.rs owns phase's file locks and their deadline (DUR-2)"
            )]
            let attempt = file.try_lock();
            match attempt {
                Ok(()) => return Ok(LifecycleLock { _file: file }),
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(source)) => {
                    return Err(PhaseError::StateUnwritable { path, source });
                }
            }
            if now() >= deadline || give_up() {
                return Err(PhaseError::Busy {
                    workspace: workspace.clone(),
                    waited: wait,
                });
            }
            pause(POLL_INTERVAL);
        }
    }

    /// Creates the records directory with mode 0700 when it does not
    /// exist, and narrows the mode of one that grants its group or others
    /// any access to 0700. It refuses a records directory that is a
    /// symbolic link or no directory.
    fn create_dir(&self) -> Result<(), PhaseError> {
        let unwritable = |source| PhaseError::StateUnwritable {
            path: self.dir.clone(),
            source,
        };
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(unwritable)?;
        let metadata = fs::symlink_metadata(&self.dir).map_err(unwritable)?;
        if !metadata.is_dir() {
            return Err(unwritable(io::Error::other(
                "the records directory is a symbolic link or no directory",
            )));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))
                .map_err(unwritable)?;
        }
        Ok(())
    }

    /// Syncs the records directory, so a rename or a removal in it
    /// survives a power loss. Some file systems cannot sync a directory and
    /// fail with EINVAL or ENOTSUP, and there the sync is skipped, because
    /// the rename itself has succeeded.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::StateUnwritable`] when the directory cannot be
    /// opened or synced for another reason.
    fn sync_dir(&self) -> Result<(), PhaseError> {
        let synced = File::open(&self.dir).and_then(|handle| handle.sync_all());
        match synced {
            Ok(()) => Ok(()),
            Err(error) if cannot_sync_directory(&error) => Ok(()),
            Err(source) => Err(PhaseError::StateUnwritable {
                path: self.dir.clone(),
                source,
            }),
        }
    }
}

/// Reports whether `error` is how a file system that cannot sync a
/// directory refuses it: EINVAL or ENOTSUP.
fn cannot_sync_directory(error: &io::Error) -> bool {
    let code = error.raw_os_error();
    code == Some(rustix::io::Errno::INVAL.raw_os_error())
        || code == Some(rustix::io::Errno::NOTSUP.raw_os_error())
}

/// Returns the `O_NOFOLLOW` flag, so the store never writes through a
/// symbolic link that stands where its temporary file goes.
fn no_follow() -> i32 {
    i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits()).unwrap_or(0)
}

/// The write-ahead journal of one workspace's run, which the engine keeps
/// while it starts or stops objects. It holds the workspace's lifecycle
/// lock, and it writes the record before each step that the record's new
/// state describes:
///
/// - [`Journal::plan`] before phase creates anything;
/// - [`Journal::started`] as each object starts;
/// - [`Journal::stopping`] before phase stops or removes an object;
/// - [`Journal::stopped`] only after phase has checked that it is gone.
///
/// A crash at any point therefore leaves a record that names everything
/// that may be running.
#[derive(Debug)]
pub(crate) struct Journal<'s> {
    /// The store.
    store: &'s RunStore,
    /// The workspace.
    workspace: WorkspaceId,
    /// The record as it is on disk, or `None` when there is none.
    record: Option<RunRecord>,
    /// The workspace's lifecycle lock.
    lock: LifecycleLock,
}

impl<'s> Journal<'s> {
    /// Takes the lifecycle lock of `workspace`, waiting at most `wait`, and
    /// reads its record.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`RunStore::lock`] and [`RunStore::read`].
    pub(crate) fn open(
        store: &'s RunStore,
        workspace: &WorkspaceId,
        wait: Duration,
    ) -> Result<Self, PhaseError> {
        let lock = store.lock(workspace, wait)?;
        Self::locked(store, workspace, lock)
    }

    /// Takes the lifecycle lock of `workspace` as [`RunStore::lock_unless`]
    /// does, and reads its record.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`RunStore::lock_unless`] and
    /// [`RunStore::read`].
    pub(crate) fn open_unless(
        store: &'s RunStore,
        workspace: &WorkspaceId,
        wait: Duration,
        give_up: &dyn Fn() -> bool,
    ) -> Result<Self, PhaseError> {
        let lock = store.lock_unless(workspace, wait, give_up)?;
        Self::locked(store, workspace, lock)
    }

    /// Returns the journal of `workspace` that holds `lock`, with its
    /// record read.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`RunStore::read`].
    fn locked(
        store: &'s RunStore,
        workspace: &WorkspaceId,
        lock: LifecycleLock,
    ) -> Result<Self, PhaseError> {
        let record = store.read(workspace)?;
        Ok(Self {
            store,
            workspace: workspace.clone(),
            record,
            lock,
        })
    }

    /// Returns the record, or `None` when the workspace has none.
    pub(crate) const fn record(&self) -> Option<&RunRecord> {
        self.record.as_ref()
    }

    /// Records the objects of `planned`, each in the planned state, before
    /// phase creates any of them. An object that the record already names
    /// keeps its state.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::StateUnwritable`] when the record cannot be
    /// written.
    pub(crate) fn plan(&mut self, planned: RunRecord) -> Result<(), PhaseError> {
        let mut record = self
            .record
            .clone()
            .unwrap_or_else(|| RunRecord::new(self.workspace.clone(), planned.plan_hash.clone()));
        record.merge(planned);
        self.commit(record)
    }

    /// Records that the object `key` started, as `started` describes.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::Unplanned`] when the record does not name
    /// `key`, and [`PhaseError::StateUnwritable`] when the record cannot be
    /// written.
    pub(crate) fn started(&mut self, key: &ObjectKey, started: Started) -> Result<(), PhaseError> {
        let mut record = self.current(key)?;
        record
            .mark_started(key, started)
            .map_err(|unplanned| self.unplanned(&unplanned))?;
        self.commit(record)
    }

    /// Records that the keeper restarted the host process `key` as `host`,
    /// after `restarts` restarts in all, which clears its readiness.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Journal::started`].
    pub(crate) fn restarted(
        &mut self,
        key: &ObjectKey,
        host: HostProcess,
        restarts: u32,
    ) -> Result<(), PhaseError> {
        let mut record = self.current(key)?;
        record
            .mark_restarted(key, host, restarts)
            .map_err(|unplanned| self.unplanned(&unplanned))?;
        self.commit(record)
    }

    /// Records that phase observed the host process or container `key`
    /// ready, as `ready` says.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Journal::started`].
    pub(crate) fn ready(&mut self, key: &ObjectKey, ready: Ready) -> Result<(), PhaseError> {
        let mut record = self.current(key)?;
        record
            .mark_ready(key, ready)
            .map_err(|unplanned| self.unplanned(&unplanned))?;
        self.commit(record)
    }

    /// Writes the keeper and the entry of `process` of `app` back to
    /// planned with no process, with what `launching` holds, before phase
    /// launches the process again.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Journal::started`].
    pub(crate) fn launching(
        &mut self,
        app: &AppName,
        process: &ProcessName,
        launching: &Launching,
    ) -> Result<(), PhaseError> {
        let key = ObjectKey::Process(app.clone(), process.clone());
        let mut record = self.current(&key)?;
        record
            .mark_launching(app, process, launching)
            .map_err(|unplanned| self.unplanned(&unplanned))?;
        self.commit(record)
    }

    /// Writes the container entries of `processes` of `app` back to
    /// planned with no id, as [`RunRecord::mark_container_launching`]
    /// says, before phase starts their containers again.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Journal::started`].
    pub(crate) fn containers_launching(
        &mut self,
        app: &AppName,
        processes: &[ProcessName],
    ) -> Result<(), PhaseError> {
        let Some(first) = processes.first() else {
            return Ok(());
        };
        let mut record = self.current(&ObjectKey::Container(app.clone(), first.clone()))?;
        for process in processes {
            record
                .mark_container_launching(app, process)
                .map_err(|unplanned| self.unplanned(&unplanned))?;
        }
        if self.record.as_ref() == Some(&record) {
            return Ok(());
        }
        self.commit(record)
    }

    /// Removes the entries of `apps` that phase planned and never tried to
    /// start, as [`RunRecord::forget_unstarted`] says.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::StateUnwritable`] when the record cannot be
    /// written.
    pub(crate) fn forget_unstarted(&mut self, apps: &[AppName]) -> Result<(), PhaseError> {
        let Some(mut record) = self.record.clone() else {
            return Ok(());
        };
        record.forget_unstarted(apps);
        if self.record.as_ref() == Some(&record) {
            return Ok(());
        }
        self.commit(record)
    }

    /// Records that phase is about to stop or remove the object `key`.
    /// A stop request causes the end of a host process that phase stops,
    /// so its entry records the end as requested, as
    /// [`RunRecord::mark_stop_requested`] says.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Journal::started`].
    pub(crate) fn stopping(&mut self, key: &ObjectKey) -> Result<(), PhaseError> {
        let mut record = self.current(key)?;
        if matches!(key, ObjectKey::Process(..)) {
            record
                .mark_stop_requested(key)
                .map_err(|unplanned| self.unplanned(&unplanned))?;
        }
        record
            .set_state(key, EntryState::Stopping)
            .map_err(|unplanned| self.unplanned(&unplanned))?;
        self.commit(record)
    }

    /// Records that the host process `key` ended as `ended`, and that its
    /// keeper is about to stop what is left of its group.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Journal::started`].
    pub(crate) fn ending(&mut self, key: &ObjectKey, ended: Ended) -> Result<(), PhaseError> {
        self.change(key, EntryState::Stopping, Some(ended))
    }

    /// Records that phase checked that the object `key` is gone.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Journal::started`].
    pub(crate) fn stopped(&mut self, key: &ObjectKey) -> Result<(), PhaseError> {
        self.change(key, EntryState::Stopped, None)
    }

    /// Records that the host process `key` ended as `ended`, and that its
    /// group is gone.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Journal::started`].
    pub(crate) fn stopped_after(
        &mut self,
        key: &ObjectKey,
        ended: Ended,
    ) -> Result<(), PhaseError> {
        self.change(key, EntryState::Stopped, Some(ended))
    }

    /// Forgets the completion of the oneshot `key`, as
    /// [`RunRecord::clear_completion`] says, and reports whether it held
    /// one. A record that held none is left as it is on disk.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Journal::started`].
    pub(crate) fn clear_completion(&mut self, key: &ObjectKey) -> Result<bool, PhaseError> {
        let mut record = self.current(key)?;
        let cleared = record
            .clear_completion(key)
            .map_err(|unplanned| self.unplanned(&unplanned))?;
        if cleared {
            self.commit(record)?;
        }
        Ok(cleared)
    }

    /// Deletes the record as [`Journal::finish`] does, unless a host
    /// process's entry records an end that no stop request caused, which
    /// `status` shows as exited until `down` or a later start, and reports
    /// whether it deleted it.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Journal::finish`].
    pub(crate) fn finish_keeping_exits(&mut self) -> Result<bool, PhaseError> {
        if self.record.as_ref().is_some_and(RunRecord::holds_an_exit) {
            return Ok(false);
        }
        self.finish()
    }

    /// Deletes the record when every object in it is stopped, apart from
    /// volumes that outlive the run, and reports whether it did.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::StateUnwritable`] when the record cannot be
    /// deleted.
    pub(crate) fn finish(&mut self) -> Result<bool, PhaseError> {
        match &self.record {
            Some(record) if record.all_stopped() => {
                self.store.remove(&self.workspace, &self.lock)?;
                self.record = None;
                Ok(true)
            }
            Some(_) => Ok(false),
            None => Ok(true),
        }
    }

    /// Sets the state of `key` to `state`, and the end of the host process
    /// `key` to `ended` when it is given, and writes the record.
    fn change(
        &mut self,
        key: &ObjectKey,
        state: EntryState,
        ended: Option<Ended>,
    ) -> Result<(), PhaseError> {
        let mut record = self.current(key)?;
        record
            .set_state(key, state)
            .map_err(|unplanned| self.unplanned(&unplanned))?;
        if let Some(ended) = ended {
            record
                .set_ended(key, ended)
                .map_err(|unplanned| self.unplanned(&unplanned))?;
        }
        self.commit(record)
    }

    /// Returns a copy of the record to change, or the error of a missing
    /// one.
    fn current(&self, key: &ObjectKey) -> Result<RunRecord, PhaseError> {
        self.record
            .clone()
            .ok_or_else(|| self.unplanned(&Unplanned(key.clone())))
    }

    /// Writes `record` and keeps it as the record on disk.
    ///
    /// When the directory sync fails after the rename, the file already
    /// holds `record`, so the journal keeps `record` in memory too and
    /// returns the error. An operation that then stops what it started
    /// writes its next changes over `record`, never over the record before
    /// it.
    fn commit(&mut self, record: RunRecord) -> Result<(), PhaseError> {
        self.store.replace(&record, &self.lock)?;
        self.record = Some(record);
        self.store.sync_dir()
    }

    /// Returns the error of a change to an object the record does not
    /// name.
    fn unplanned(&self, unplanned: &Unplanned) -> PhaseError {
        PhaseError::Unplanned {
            workspace: self.workspace.clone(),
            object: unplanned.0.to_string(),
        }
    }
}
