//! The keeper's writer of the run record, a thread of its own that writes
//! each change of the app's state through the journal, so that waiting
//! for the workspace's lifecycle lock never blocks supervision.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use crate::engine::keeper::KeeperArgs;
use crate::name::WorkspaceId;
use crate::record::run::{HostProcess, ObjectKey, Started};
use crate::record::store::{Journal, RunStore};

/// How long the writer waits for the lifecycle lock before it drops a
/// change. An `up` holds the lock while it builds and starts apps, and the
/// engine records the first start itself, so a dropped change is a later
/// one.
const RECORD_LOCK_WAIT: Duration = Duration::from_secs(30 * 60);

/// A change of the app's state.
#[derive(Clone, Debug)]
enum Note {
    /// The app started as this process.
    Started(HostProcess),
    /// The keeper is about to stop the app's group.
    Stopping,
    /// The app's group is gone.
    Stopped,
}

/// The handle of the writer thread.
#[derive(Debug)]
pub(crate) struct Recorder {
    /// The changes to write, in order.
    notes: Sender<Note>,
    /// A message when the thread has written every change and ended.
    done: Receiver<()>,
}

impl Recorder {
    /// Starts the writer of the run record that `args` names.
    pub(crate) fn start(args: &KeeperArgs) -> Self {
        let (notes, queue) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let store = RunStore::new(args.state_dir());
        let workspace = args.workspace().clone();
        let key = ObjectKey::Process(args.app().clone(), args.process().clone());
        thread::spawn(move || {
            while let Ok(note) = queue.recv() {
                write(&store, &workspace, &key, note, &queue);
            }
            let _ = finished.send(());
        });
        Self { notes, done }
    }

    /// Records that the app started as `host`.
    pub(crate) fn started(&self, host: HostProcess) {
        let _ = self.notes.send(Note::Started(host));
    }

    /// Records that the keeper is about to stop the app's group.
    pub(crate) fn stopping(&self) {
        let _ = self.notes.send(Note::Stopping);
    }

    /// Records that the app's group is gone.
    pub(crate) fn stopped(&self) {
        let _ = self.notes.send(Note::Stopped);
    }

    /// Waits at most `wait` for the writer to write every change.
    pub(crate) fn finish(self, wait: Duration) {
        drop(self.notes);
        let _ = self.done.recv_timeout(wait);
    }
}

/// Writes `first`, and every change queued behind it, under one hold of
/// the lifecycle lock. A change to a record that does not name the
/// process, or that cannot be written, is dropped, because the keeper has
/// nobody to report it to and must go on supervising.
fn write(
    store: &RunStore,
    workspace: &WorkspaceId,
    key: &ObjectKey,
    first: Note,
    queue: &Receiver<Note>,
) {
    let Ok(mut journal) = Journal::open(store, workspace, RECORD_LOCK_WAIT) else {
        return;
    };
    let mut note = Some(first);
    while let Some(current) = note {
        let _ = match current {
            Note::Started(host) => journal.started(key, Started::Host(host)),
            Note::Stopping => journal.stopping(key),
            Note::Stopped => journal.stopped(key),
        };
        note = queue.try_recv().ok();
    }
}
