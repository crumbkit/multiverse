//! Holds a workspace's lock while an mxv command waits for it, and deletes
//! the workspace's record during the wait, as an `mxv archive` that held
//! the lock first would.

use std::fs::{self, OpenOptions};
use std::path::Path;
use std::time::{Duration, Instant};

/// How long the test holds the lock after it starts the command. The
/// command reads the workspace's record within milliseconds, and then
/// waits for the lock for up to five seconds.
const HOLD: Duration = Duration::from_millis(500);

/// Takes the lock of the workspace `name` under mxv's state directory
/// `state`, which is `$XDG_STATE_HOME/mxv`, runs `command` in a thread,
/// deletes the workspace's record after [`HOLD`], releases the lock, and
/// returns what `command` returned. A command that read the record before
/// the wait finds it gone under the lock. A command that had not yet read
/// it finds no workspace at all, which mxv reports with the same code, so
/// the result does not depend on the timing.
pub(crate) fn delete_record_while_locked<T: Send>(
    state: &Path,
    name: &str,
    command: impl FnOnce() -> T + Send,
) -> T {
    let locks = state.join("locks");
    fs::create_dir_all(&locks).unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test fixture opens the lock file that mxv takes for the workspace"
    )]
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(locks.join(format!("{name}.lock")))
        .unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test fixture holds the workspace's lock as another mxv command would"
    )]
    file.lock().unwrap();
    std::thread::scope(|scope| {
        let waiting = scope.spawn(command);
        let deadline = Instant::now() + HOLD;
        while Instant::now() < deadline && !waiting.is_finished() {
            std::thread::yield_now();
        }
        fs::remove_file(state.join("workspaces").join(format!("{name}.json"))).unwrap();
        drop(file);
        waiting.join().unwrap()
    })
}
