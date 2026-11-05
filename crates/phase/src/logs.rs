//! Reading a process's log. A host process's keeper gives the process a
//! log file at `<state dir>/logs/<workspace id>/<app>.<process>.log`,
//! which receives both its output streams. A container's log comes from
//! `docker logs`, which keeps the container's two streams apart.
//!
//! [`logs`] opens the log and the run record read-only and takes no lock.

use std::fs::File;
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::clock::pause;
use crate::engine::host::LogFollower;
use crate::error::{Operation, PhaseError};
use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::record::process::{self, Identity, Processes};
use crate::record::run::{EntryState, HostEntry};
use crate::record::store::RunStore;

/// How long [`logs`] waits before it reads a followed log again. A log
/// file gives no event when it grows, so a follower polls it.
pub(crate) const FOLLOW_INTERVAL: Duration = Duration::from_millis(200);

/// The most bytes of a log that [`tail`] reads, from the log's end.
pub const MOST_TAIL_BYTES: u64 = 64 * 1024;

/// Returns the log file of `process` of `app` in the workspace whose
/// stable id is `workspace`, under the state directory `state_dir`.
pub(crate) fn log_path(
    state_dir: &Path,
    workspace: &WorkspaceId,
    app: &AppName,
    process: &ProcessName,
) -> PathBuf {
    state_dir
        .join("logs")
        .join(workspace.as_str())
        .join(format!("{app}.{process}.log"))
}

/// Returns the last `lines` lines of the file at `path`, read from its
/// last [`MOST_TAIL_BYTES`] bytes, or no lines when it cannot be read. An
/// unfinished last line counts as a line. A request for more lines than
/// those bytes hold returns fewer.
#[must_use]
pub fn tail(path: &Path, lines: usize) -> Vec<String> {
    let length = std::fs::metadata(path).map_or(0, |metadata| metadata.len());
    let (mut complete, partial) = tail_before(path, lines, length);
    if !partial.is_empty() {
        complete.push(String::from_utf8_lossy(&partial).into_owned());
    }
    complete
}

/// Returns the last lines of the file at `path` before the offset `end`,
/// read from at most [`MOST_TAIL_BYTES`] bytes before it, as complete lines
/// and the unfinished line that `end` cuts, which may be empty. The two
/// hold `lines` lines at most, counting a non-empty unfinished line as one.
/// A file that cannot be read has no lines.
fn tail_before(path: &Path, lines: usize, end: u64) -> (Vec<String>, Vec<u8>) {
    let Ok(mut file) = File::open(path) else {
        return (Vec::new(), Vec::new());
    };
    let start = end.saturating_sub(MOST_TAIL_BYTES);
    // The read takes the byte before `start` too, so that it can tell
    // whether `start` begins a line.
    let before = start.saturating_sub(1);
    if file.seek(SeekFrom::Start(before)).is_err() {
        return (Vec::new(), Vec::new());
    }
    let mut bytes = Vec::new();
    if file.take(end - before).read_to_end(&mut bytes).is_err() {
        return (Vec::new(), Vec::new());
    }
    let read = if start > 0 {
        // A read that starts inside a line drops that partial line, and
        // one that starts just after a newline drops only the newline.
        bytes
            .iter()
            .position(|&byte| byte == b'\n')
            .and_then(|newline| bytes.get(newline + 1..))
            .unwrap_or_default()
    } else {
        &bytes[..]
    };
    let cut = read
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |newline| newline + 1);
    let (complete, partial) = read.split_at(cut);
    let room = lines.saturating_sub(usize::from(!partial.is_empty()));
    let text = String::from_utf8_lossy(complete);
    let all: Vec<&str> = text.lines().collect();
    let skip = all.len().saturating_sub(room);
    let partial = if lines == 0 {
        Vec::new()
    } else {
        partial.to_vec()
    };
    (
        all.into_iter().skip(skip).map(str::to_owned).collect(),
        partial,
    )
}

/// Passes the lines of the log that `request` names to `sink`, from the
/// log's start, or only its last `request.lines` lines. With
/// `request.follow`, it then passes each new line until the sink breaks,
/// or until the log is quiet and neither the process nor its keeper runs,
/// so nothing can write to it any more. A container's log comes from
/// `docker logs` for the container that the record names, and its follow
/// ends when the container stops.
///
/// # Errors
///
/// Returns [`PhaseError::UnknownTarget`] when neither the record nor a log
/// file names the process, [`PhaseError::NotBuilt`] for `request.since`,
/// because a log file holds no times, [`PhaseError::LogUnreadable`] when
/// the log cannot be read, [`PhaseError::Docker`] when `docker logs`
/// fails, and the errors of reading the record and the process table.
pub(crate) fn logs(
    store: &RunStore,
    state_dir: &Path,
    workspace: &WorkspaceId,
    request: &LogRequest,
    sink: &mut dyn LineSink,
    processes: &dyn Processes,
) -> Result<(), PhaseError> {
    if request.since.is_some() {
        return Err(PhaseError::NotBuilt {
            operation: Operation::Logs,
        });
    }
    let record = store.read(workspace)?;
    let (app, process) = (&request.app, &request.process);
    let mut container = None;
    let mut host = false;
    if let Some(record) = &record {
        if let Some(entry) = record
            .containers
            .iter()
            .find(|entry| &entry.app == app && &entry.process == process)
        {
            // A container that `down` stopped is gone, so its entry has no
            // log, even while the entry keeps the container's id.
            container = Some(
                entry
                    .id
                    .clone()
                    .filter(|_| entry.state != EntryState::Stopped),
            );
        }
        let (keeper, entry) = record.host_pair(app, process);
        host = keeper.is_some() || entry.is_some();
    }
    if let Some(id) = container {
        // A container that has not started yet, or that phase stopped, has
        // no log.
        return match id {
            Some(id) => container_logs(state_dir, workspace, &id, request, sink),
            None => Ok(()),
        };
    }
    let path = log_path(state_dir, workspace, app, process);
    let unreadable = |source: io::Error| PhaseError::LogUnreadable {
        path: path.clone(),
        source,
    };
    let opened = match request.lines {
        // The follower opens at the log's end, and the last lines are read
        // up to that point, so each line reaches the sink once.
        Some(_) => LogFollower::at_end(&path)
            .and_then(|follower| follower.ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))),
        None => LogFollower::from_start(&path),
    };
    let mut follower = match opened {
        Ok(follower) => follower,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if !host {
                return Err(PhaseError::UnknownTarget {
                    target: format!("{app}/{process}"),
                });
            }
            LogFollower::at_end_or_empty(&path).map_err(unreadable)?
        }
        Err(error) => return Err(unreadable(error)),
    };
    if let Some(lines) = request.lines {
        let end = follower.position().map_err(unreadable)?;
        let (last, partial) = tail_before(&path, lines, end);
        for line in &last {
            if sink.line(&LogLine::merged(line)).is_break() {
                return Ok(());
            }
        }
        // The unfinished line that the follower's position cuts waits in
        // the follower for the rest of its bytes.
        follower.hold(partial);
        if !request.follow {
            if let Some(rest) = follower.take_partial() {
                let _ = sink.line(&LogLine::merged(&rest));
            }
            return Ok(());
        }
    }
    loop {
        let lines = follower.lines().map_err(unreadable)?;
        let quiet = lines.is_empty();
        for line in &lines {
            if sink.line(&LogLine::merged(line)).is_break() {
                return Ok(());
            }
        }
        if quiet {
            // A read that returned lines is followed by another, so the
            // last lines before the process ended still reach the sink.
            if !request.follow || !writing(store, workspace, app, process, processes)? {
                return Ok(());
            }
            pause(FOLLOW_INTERVAL);
        }
    }
}

/// Passes the log of the container `id`, which the run record of
/// `workspace` names for the request's process, to `sink`, as [`logs`]
/// describes.
fn container_logs(
    state_dir: &Path,
    workspace: &WorkspaceId,
    id: &crate::record::ContainerId,
    request: &LogRequest,
    sink: &mut dyn LineSink,
) -> Result<(), PhaseError> {
    let mut pass = |is_stderr: bool, text: &str| {
        let stream = if is_stderr {
            LogStream::Stderr
        } else {
            LogStream::Stdout
        };
        sink.line(&LogLine::new(stream, text))
    };
    crate::engine::container::recorded_logs(state_dir, workspace, id, request, &mut pass)
}

/// Reports whether the keeper or the process of `process` of `app` that
/// the record names now still runs as the process phase started.
fn writing(
    store: &RunStore,
    workspace: &WorkspaceId,
    app: &AppName,
    process: &ProcessName,
    processes: &dyn Processes,
) -> Result<bool, PhaseError> {
    let Some(record) = store.read(workspace)? else {
        return Ok(false);
    };
    let (keeper, entry) = record.host_pair(app, process);
    let look = |named: Option<&HostEntry>| -> Result<Identity, PhaseError> {
        match named
            .filter(|named| named.live())
            .and_then(|named| named.host.as_ref())
        {
            Some(host) => process::identify(processes, host),
            None => Ok(Identity::Gone),
        }
    };
    let keeper = look(keeper)?;
    // Members that the process left in its group hold the log open and
    // may still write to it, so following goes on while they run. Reading
    // a log signals nothing, so no proof that the group is phase's own is
    // needed here.
    Ok(keeper == Identity::Ours || matches!(look(entry)?, Identity::Ours | Identity::LeaderGone))
}

/// Which log `logs` reads, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LogRequest {
    /// The app.
    pub app: AppName,
    /// The app's process.
    pub process: ProcessName,
    /// Whether `logs` keeps passing new lines after the existing ones, until
    /// the sink breaks.
    pub follow: bool,
    /// How far back the log starts, or `None` for the whole log.
    pub since: Option<Duration>,
    /// How many of the log's last lines `logs` passes before it follows,
    /// or `None` for every line from the log's start. A host process's last
    /// lines come from at most the log's last [`MOST_TAIL_BYTES`] bytes, so
    /// a larger count passes fewer lines.
    pub lines: Option<usize>,
}

impl LogRequest {
    /// Returns the request for the whole log of `process` of `app`, without
    /// following it.
    #[must_use]
    pub const fn new(app: AppName, process: ProcessName) -> Self {
        Self {
            app,
            process,
            follow: false,
            since: None,
            lines: None,
        }
    }
}

/// Receives the lines of a log, one at a time.
pub trait LineSink {
    /// Receives one line. Returning [`ControlFlow::Break`] ends the read,
    /// which is how a caller stops following a log.
    fn line(&mut self, line: &LogLine<'_>) -> ControlFlow<()>;
}

/// One line of a process's log, without its line ending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LogLine<'a> {
    /// The stream the process wrote the line to, or `None` when the log
    /// does not say, as a host process's log file, which receives both
    /// streams, does not.
    pub stream: Option<LogStream>,
    /// The line's text.
    pub text: &'a str,
}

impl<'a> LogLine<'a> {
    /// Returns the line `text` that the process wrote to `stream`.
    #[must_use]
    pub const fn new(stream: LogStream, text: &'a str) -> Self {
        Self {
            stream: Some(stream),
            text,
        }
    }

    /// Returns the line `text` of a log that does not say which stream the
    /// process wrote it to.
    #[must_use]
    pub const fn merged(text: &'a str) -> Self {
        Self { stream: None, text }
    }
}

/// The stream a process wrote a log line to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

#[cfg(test)]
mod tests;
