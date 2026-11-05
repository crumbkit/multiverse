//! Reading a process's log. A host process's keeper gives the process a
//! log file at `<state dir>/logs/<workspace id>/<app>.<process>.log`,
//! which receives both its output streams. A container's log comes from
//! `docker logs`, which phase does not read yet.
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
/// last [`MOST_TAIL_BYTES`] bytes, or no lines when it cannot be read.
#[must_use]
pub fn tail(path: &Path, lines: usize) -> Vec<String> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let length = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let start = length.saturating_sub(MOST_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.take(MOST_TAIL_BYTES).read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut all: Vec<&str> = text.lines().collect();
    if start > 0 && !all.is_empty() {
        // The read started inside a line, so its first line is partial.
        all.remove(0);
    }
    let skip = all.len().saturating_sub(lines);
    all.into_iter().skip(skip).map(str::to_owned).collect()
}

/// Passes the lines of the log that `request` names to `sink`, from the
/// log's start. With `request.follow`, it then passes each new line until
/// the sink breaks, or until the log is quiet and neither the process nor
/// its keeper runs, so nothing can write to it any more.
///
/// # Errors
///
/// Returns [`PhaseError::UnknownTarget`] when neither the record nor a log
/// file names the process, [`PhaseError::NotBuilt`] for a container's log
/// and for `request.since`, because a log file holds no times,
/// [`PhaseError::LogUnreadable`] when the log cannot be read, and the
/// errors of reading the record and the process table.
pub(crate) fn logs(
    store: &RunStore,
    state_dir: &Path,
    workspace: &WorkspaceId,
    request: &LogRequest,
    sink: &mut dyn LineSink,
    processes: &dyn Processes,
) -> Result<(), PhaseError> {
    let not_built = PhaseError::NotBuilt {
        operation: Operation::Logs,
    };
    if request.since.is_some() {
        return Err(not_built);
    }
    let record = store.read(workspace)?;
    let (app, process) = (&request.app, &request.process);
    let in_record = record.as_ref().map_or(Default::default(), |record| {
        let container = record
            .containers
            .iter()
            .any(|entry| &entry.app == app && &entry.process == process);
        let (keeper, entry) = record.host_pair(app, process);
        (container, keeper.is_some() || entry.is_some())
    });
    if in_record.0 {
        return Err(not_built);
    }
    let path = log_path(state_dir, workspace, app, process);
    let unreadable = |source: io::Error| PhaseError::LogUnreadable {
        path: path.clone(),
        source,
    };
    let mut follower = match LogFollower::from_start(&path) {
        Ok(follower) => follower,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if !in_record.1 {
                return Err(PhaseError::UnknownTarget {
                    target: format!("{app}/{process}"),
                });
            }
            LogFollower::at_end_or_empty(&path).map_err(unreadable)?
        }
        Err(error) => return Err(unreadable(error)),
    };
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
    for host in [keeper, entry]
        .into_iter()
        .flatten()
        .filter(|named| named.live())
        .filter_map(|named| named.host.as_ref())
    {
        if process::identify(processes, host)? == Identity::Ours {
            return Ok(true);
        }
    }
    Ok(false)
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
