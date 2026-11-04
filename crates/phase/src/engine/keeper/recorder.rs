//! The keeper's writer of the run record, a thread of its own that writes
//! each change of the app's state through the journal, so that waiting
//! for the workspace's lifecycle lock never blocks supervision.
//!
//! The writer writes a change after the keeper has acted, so the record
//! can lag the app. A change applies only while the record still names the
//! keeper's own app: a `down` or another `up` that recorded a process of
//! its own meanwhile keeps its entry, and the writer notes the skipped
//! change in the diagnostic log.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use crate::engine::keeper::KeeperArgs;
use crate::engine::keeper::logfile::Diagnostics;
use crate::name::WorkspaceId;
use crate::record::run::{HostProcess, ObjectKey, Started};
use crate::record::store::{Journal, RunStore};

/// How long the writer waits for the lifecycle lock before it drops a
/// change. An `up` holds the lock while it builds and starts apps, and the
/// engine records the first start itself, so a dropped change is a later
/// one.
pub(crate) const RECORD_LOCK_WAIT: Duration = Duration::from_secs(30 * 60);

/// A change of the app's state.
#[derive(Clone, Debug)]
enum Note {
    /// The app started as this process. It applies when the entry names
    /// no process or this one.
    Started(HostProcess),
    /// The keeper restarted the app as the first process, after the
    /// second, after this many restarts in all. It applies when the entry
    /// names either process.
    Restarted(HostProcess, HostProcess, u32),
    /// The keeper is about to stop the group of this process. It applies
    /// when the entry names this process.
    Stopping(HostProcess),
    /// The group of this process is gone. It applies when the entry names
    /// this process.
    Stopped(HostProcess),
}

impl Note {
    /// Reports whether the note applies to an entry that names `recorded`.
    fn applies_to(&self, recorded: Option<&HostProcess>) -> bool {
        match (self, recorded) {
            (Self::Started(_), None) => true,
            (Self::Started(host) | Self::Stopping(host) | Self::Stopped(host), Some(recorded)) => {
                host == recorded
            }
            (Self::Restarted(host, previous, _), Some(recorded)) => {
                host == recorded || previous == recorded
            }
            (Self::Restarted(..) | Self::Stopping(_) | Self::Stopped(_), None) => false,
        }
    }

    /// Describes the change for the diagnostic log.
    fn describe(&self) -> String {
        match self {
            Self::Started(host) => format!("the start of process {}", host.pid),
            Self::Restarted(host, _, _) => format!("the restart as process {}", host.pid),
            Self::Stopping(host) => format!("the stopping of process {}", host.pid),
            Self::Stopped(host) => format!("the stop of process {}", host.pid),
        }
    }
}

/// The handle of the writer thread.
#[derive(Debug)]
pub(crate) struct Recorder {
    /// The changes to write, in order.
    notes: Sender<Note>,
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
        thread::spawn(move || {
            while let Ok(note) = queue.recv() {
                write(&store, &workspace, &key, note, &queue, &diagnostics);
            }
            finished();
        });
        Self { notes }
    }

    /// Records that the app started as `host`.
    pub(crate) fn started(&self, host: HostProcess) {
        let _ = self.notes.send(Note::Started(host));
    }

    /// Records that the keeper restarted the app as `host` after
    /// `previous`, after `restarts` restarts in all.
    pub(crate) fn restarted(&self, host: HostProcess, previous: HostProcess, restarts: u32) {
        let _ = self.notes.send(Note::Restarted(host, previous, restarts));
    }

    /// Records that the keeper is about to stop the group of `host`.
    pub(crate) fn stopping(&self, host: HostProcess) {
        let _ = self.notes.send(Note::Stopping(host));
    }

    /// Records that the group of `host` is gone.
    pub(crate) fn stopped(&self, host: HostProcess) {
        let _ = self.notes.send(Note::Stopped(host));
    }

    /// Tells the writer that no change follows, so it ends once it has
    /// written the changes it holds.
    pub(crate) fn close(self) {
        drop(self.notes);
    }
}

/// Writes `first`, and every change queued behind it, under one hold of
/// the lifecycle lock. A change that cannot be written is noted in
/// `diagnostics` and dropped, because the keeper must go on supervising.
fn write(
    store: &RunStore,
    workspace: &WorkspaceId,
    key: &ObjectKey,
    first: Note,
    queue: &Receiver<Note>,
    diagnostics: &Diagnostics,
) {
    let mut journal = match Journal::open(store, workspace, RECORD_LOCK_WAIT) {
        Ok(journal) => journal,
        Err(error) => {
            diagnostics.note(&format!(
                "the run record could not be written ({}): {error}",
                error.code()
            ));
            return;
        }
    };
    let mut note = Some(first);
    while let Some(current) = note {
        note = queue.try_recv().ok();
        let recorded = recorded_host(&journal, key);
        if !current.applies_to(recorded.as_ref()) {
            diagnostics.note(&format!(
                "the run record names another process, so the keeper skips {}",
                current.describe()
            ));
            continue;
        }
        let written = match current {
            Note::Started(host) => journal.started(key, Started::Host(host)),
            Note::Restarted(host, _, restarts) => journal.restarted(key, host, restarts),
            Note::Stopping(_) => journal.stopping(key),
            Note::Stopped(_) => journal.stopped(key),
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
