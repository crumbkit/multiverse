//! The keeper's writer of the run record, a thread of its own that writes
//! each change of the app's state through the journal, so that waiting
//! for the workspace's lifecycle lock never blocks supervision.
//!
//! The writer writes a change after the keeper has acted, so the record
//! can lag the app. A change applies only while the record still names the
//! keeper's own app: a `down` or another `up` that recorded a process of
//! its own meanwhile keeps its entry, and the writer notes the skipped
//! change in the diagnostic log.
//!
//! Once the keeper is told to hurry, as it is after a stop request, the
//! writer asks for the lifecycle lock once more without waiting. When
//! another operation holds the lock, the writer drops the changes it
//! holds and notes each one in the diagnostic log, because that operation
//! records the stop itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use crate::engine::keeper::KeeperArgs;
use crate::engine::keeper::logfile::Diagnostics;
use crate::error::PhaseError;
use crate::name::WorkspaceId;
use crate::record::run::{Ended, EntryState, HostProcess, ObjectKey, Ready, Started};
use crate::record::store::{Journal, RunStore};

/// How long the writer waits for the lifecycle lock before it drops a
/// change. An `up` holds the lock while it builds and starts apps, and the
/// engine records the first start itself, so a dropped change is a later
/// one.
pub(crate) const RECORD_LOCK_WAIT: Duration = Duration::from_secs(30 * 60);

/// A change of the app's state.
#[derive(Clone, Debug)]
pub(super) enum Note {
    /// The app started as this process. It applies when the entry names
    /// no process or this one.
    Started(HostProcess),
    /// The keeper restarted the app as the first process, after the
    /// second, after this many restarts in all, and the app is ready as
    /// the last field says, or `None` when nothing has observed it ready.
    /// It applies when the entry names either process.
    Restarted(HostProcess, HostProcess, u32, Option<Ready>),
    /// A stop request came, and the keeper is about to stop the group of
    /// this process. It applies when the entry names this process.
    Stopping(HostProcess),
    /// This process ended by itself as the end says, and the keeper is
    /// about to stop what is left of its group. It applies when the entry
    /// names this process and another operation has not recorded it
    /// stopped.
    Ending(HostProcess, Ended),
    /// This process ended as the end says, and its group is gone. It
    /// applies when the entry names this process and another operation has
    /// not recorded it stopped.
    Stopped(HostProcess, Ended),
}

impl Note {
    /// Reports whether the note applies to an entry that names `recorded`
    /// and is in `state`. An entry that another operation already recorded
    /// as stopped keeps the end that operation recorded.
    pub(super) fn applies_to(&self, recorded: Option<&HostProcess>, state: EntryState) -> bool {
        if matches!(self, Self::Ending(..) | Self::Stopped(..)) && state == EntryState::Stopped {
            return false;
        }
        match (self, recorded) {
            (Self::Started(_), None) => true,
            (
                Self::Started(host)
                | Self::Stopping(host)
                | Self::Ending(host, _)
                | Self::Stopped(host, _),
                Some(recorded),
            ) => host == recorded,
            (Self::Restarted(host, previous, ..), Some(recorded)) => {
                host == recorded || previous == recorded
            }
            (
                Self::Restarted(..) | Self::Stopping(_) | Self::Ending(..) | Self::Stopped(..),
                None,
            ) => false,
        }
    }

    /// Describes the change for the diagnostic log.
    fn describe(&self) -> String {
        match self {
            Self::Started(host) => format!("the start of process {}", host.pid),
            Self::Restarted(host, ..) => format!("the restart as process {}", host.pid),
            Self::Stopping(host) => format!("the stopping of process {}", host.pid),
            Self::Ending(host, _) => format!("the end of process {}", host.pid),
            Self::Stopped(host, _) => format!("the stop of process {}", host.pid),
        }
    }
}

/// The handle of the writer thread.
#[derive(Debug)]
pub(crate) struct Recorder {
    /// The changes to write, in order.
    notes: Sender<Note>,
    /// Whether the writer should stop waiting for the lock.
    hurry: Hurry,
}

/// The flag that tells the writer to stop waiting for the lifecycle lock
/// after its next try.
#[derive(Clone, Debug, Default)]
pub(crate) struct Hurry(Arc<AtomicBool>);

impl Hurry {
    /// Tells the writer to try the lock once more and then give up.
    pub(crate) fn now(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Reports whether the writer has been told to hurry.
    fn asked(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl Recorder {
    /// Starts the writer of the run record that `args` names, which notes
    /// each failed write in `diagnostics` and calls `finished` once it has
    /// written every change after [`Recorder::close`].
    pub(crate) fn start(
        args: &KeeperArgs,
        diagnostics: Diagnostics,
        finished: impl FnOnce() + Send + 'static,
    ) -> Self {
        let (notes, queue) = mpsc::channel();
        let store = RunStore::new(args.state_dir());
        let workspace = args.workspace().clone();
        let key = ObjectKey::Process(args.app().clone(), args.process().clone());
        let hurry = Hurry::default();
        let hurried = hurry.clone();
        thread::spawn(move || {
            let writer = Writer {
                store: &store,
                workspace: &workspace,
                key: &key,
                diagnostics: &diagnostics,
                hurry: &hurried,
            };
            while let Ok(note) = queue.recv() {
                writer.write(note, &queue);
            }
            finished();
        });
        Self { notes, hurry }
    }

    /// Records that the app started as `host`.
    pub(crate) fn started(&self, host: HostProcess) {
        let _ = self.notes.send(Note::Started(host));
    }

    /// Records that the keeper restarted the app as `host` after
    /// `previous`, after `restarts` restarts in all, and that the app is
    /// `ready`, or `None` when nothing has observed it ready.
    pub(crate) fn restarted(
        &self,
        host: HostProcess,
        previous: HostProcess,
        restarts: u32,
        ready: Option<Ready>,
    ) {
        let _ = self
            .notes
            .send(Note::Restarted(host, previous, restarts, ready));
    }

    /// Records that a stop request came and the keeper is about to stop
    /// the group of `host`.
    pub(crate) fn stopping(&self, host: HostProcess) {
        let _ = self.notes.send(Note::Stopping(host));
    }

    /// Records that `host` ended by itself as `ended`, and that the keeper
    /// is about to stop what is left of its group.
    pub(crate) fn ending(&self, host: HostProcess, ended: Ended) {
        let _ = self.notes.send(Note::Ending(host, ended));
    }

    /// Records that `host` ended as `ended` and its group is gone.
    pub(crate) fn stopped(&self, host: HostProcess, ended: Ended) {
        let _ = self.notes.send(Note::Stopped(host, ended));
    }

    /// Tells the writer that no change follows, so it ends once it has
    /// written the changes it holds, and returns the flag that makes it
    /// stop waiting for the lock.
    pub(crate) fn close(self) -> Hurry {
        drop(self.notes);
        self.hurry
    }
}

/// What the writer thread writes with.
struct Writer<'w> {
    /// The store.
    store: &'w RunStore,
    /// The workspace.
    workspace: &'w WorkspaceId,
    /// The process's entry.
    key: &'w ObjectKey,
    /// The diagnostic log.
    diagnostics: &'w Diagnostics,
    /// Whether to stop waiting for the lock.
    hurry: &'w Hurry,
}

impl Writer<'_> {
    /// Writes `first`, and every change queued behind it, as [`write()`]
    /// does. When the keeper has been told to hurry and another operation
    /// holds the lock, it drops them and notes each one.
    fn write(&self, first: Note, queue: &Receiver<Note>) {
        let hurry = self.hurry;
        let opened = Journal::open_unless(self.store, self.workspace, RECORD_LOCK_WAIT, &|| {
            hurry.asked()
        });
        match opened {
            Ok(journal) => write(journal, self.key, first, queue, self.diagnostics),
            Err(PhaseError::Busy { .. }) if hurry.asked() => {
                let mut note = Some(first);
                while let Some(current) = note {
                    self.diagnostics.note(&format!(
                        "another operation holds the lifecycle lock and records the stop, so the keeper drops {}",
                        current.describe()
                    ));
                    note = queue.try_recv().ok();
                }
            }
            Err(error) => self.diagnostics.note(&format!(
                "the run record could not be written ({}): {error}",
                error.code()
            )),
        }
    }
}

/// Writes `first`, and every change queued behind it, under one hold of
/// the lifecycle lock that `journal` holds. A change that cannot be written is noted in
/// `diagnostics` and dropped, because the keeper must go on supervising.
fn write(
    mut journal: Journal<'_>,
    key: &ObjectKey,
    first: Note,
    queue: &Receiver<Note>,
    diagnostics: &Diagnostics,
) {
    let mut note = Some(first);
    while let Some(current) = note {
        note = queue.try_recv().ok();
        let recorded = recorded_host(&journal, key);
        let state = journal
            .record()
            .and_then(|record| record.state(key))
            .unwrap_or(EntryState::Planned);
        if !current.applies_to(recorded.as_ref(), state) {
            diagnostics.note(&format!(
                "the run record names another process or already records its stop, so the keeper skips {}",
                current.describe()
            ));
            continue;
        }
        let written = match current {
            Note::Started(host) => journal.started(key, Started::Host(host)),
            Note::Restarted(host, _, restarts, ready) => {
                journal.restarted(key, host, restarts, ready)
            }
            Note::Stopping(_) => journal.stopping(key),
            Note::Ending(_, ended) => journal.ending(key, ended),
            Note::Stopped(_, ended) => journal.stopped_after(key, ended),
        };
        if let Err(error) = written {
            diagnostics.note(&format!(
                "the run record could not be written ({}): {error}",
                error.code()
            ));
        }
    }
}

/// Returns the process that the record's entry of `key` names, or `None`
/// when it names none or the record has no such entry.
fn recorded_host(journal: &Journal<'_>, key: &ObjectKey) -> Option<HostProcess> {
    let ObjectKey::Process(app, process) = key else {
        return None;
    };
    journal
        .record()?
        .processes
        .iter()
        .find(|entry| &entry.app == app && &entry.process == process)?
        .host
        .clone()
}
