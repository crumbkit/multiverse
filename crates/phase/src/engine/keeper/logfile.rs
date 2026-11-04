//! The files a keeper appends to: its app's log and its own diagnostic
//! log. phase never rewrites either.
//!
//! The diagnostic log is `<log dir>/<app>.<process>.keeper.log`, beside the
//! app's log. The keeper writes a line there when it starts, when the app
//! starts or ends, before each restart, when it cannot resolve the
//! environment or write the run record, and when it exits. Its standard
//! error goes there too. A line names variables and resolvers, and never
//! holds a value.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use crate::clock::wall_now;

use crate::name::{AppName, ProcessName};

/// Returns the path of the diagnostic log of the keeper of `process` of
/// `app`, whose app logs to `log`.
pub(crate) fn diagnostic_log(log: &Path, app: &AppName, process: &ProcessName) -> PathBuf {
    let dir = log.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!("{app}.{process}.keeper.log"))
}

/// Opens `path` for appending, creating it with mode 0600 and its
/// directory with mode 0700 when they do not exist. It refuses a file that
/// is a symbolic link.
///
/// # Errors
///
/// Returns the error of creating the directory or opening the file.
pub(crate) fn open_append(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/engine/keeper/logfile.rs opens the logs that a keeper and its app append to, which phase never rewrites"
    )]
    let opened = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits()).unwrap_or(0))
        .open(path);
    opened
}

/// The keeper's diagnostic log, which the supervisor and the record writer
/// share. A keeper whose diagnostic log cannot be opened writes no
/// diagnostics.
#[derive(Clone, Debug)]
pub(crate) struct Diagnostics {
    /// The open log, or `None`.
    file: Option<Arc<File>>,
}

impl Diagnostics {
    /// Opens the diagnostic log at `path`.
    pub(crate) fn open(path: &Path) -> Self {
        Self {
            file: open_append(path).ok().map(Arc::new),
        }
    }

    /// Appends `text` as one line that starts with the time in
    /// milliseconds since the Unix epoch. The caller passes text that holds
    /// no environment value. A failed write is ignored, because the keeper
    /// has nowhere else to report it and must go on supervising.
    pub(crate) fn note(&self, text: &str) {
        let Some(file) = &self.file else {
            return;
        };
        let millis = wall_now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis());
        let line = format!("{millis} {text}\n");
        let _ = file.as_ref().write_all(line.as_bytes());
    }
}
