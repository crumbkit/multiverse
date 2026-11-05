//! A follower of a process's log file, which reads the complete lines that
//! the process writes as the file grows.

use std::fs::File;
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use crate::engine::probe::LogLines;

/// The most bytes of a log that one [`LogFollower::lines`] reads.
pub const MOST_FOLLOW_BYTES: u64 = 1024 * 1024;

/// A log file that is read as it grows. It holds a partial last line until
/// its line break arrives, and starts again at the file's start when the
/// file becomes shorter than what it has read, as when a cap empties it.
#[derive(Debug)]
pub struct LogFollower {
    /// The log's path.
    path: PathBuf,
    /// The open log, positioned after the last byte read, or `None` while
    /// the log does not exist.
    file: Option<File>,
    /// The bytes after the last line break, which wait for the rest of
    /// their line.
    partial: Vec<u8>,
}

impl LogFollower {
    /// Opens the log at `path` at its start.
    ///
    /// # Errors
    ///
    /// Returns the error of opening the log, such as
    /// [`io::ErrorKind::NotFound`] when it does not exist.
    pub fn from_start(path: &Path) -> io::Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            file: Some(File::open(path)?),
            partial: Vec::new(),
        })
    }

    /// Opens the log at `path` at its end, or returns `None` when it does
    /// not exist.
    ///
    /// # Errors
    ///
    /// Returns the error of opening the log or of seeking to its end.
    pub fn at_end(path: &Path) -> io::Result<Option<Self>> {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        file.seek(SeekFrom::End(0))?;
        Ok(Some(Self {
            path: path.to_path_buf(),
            file: Some(file),
            partial: Vec::new(),
        }))
    }

    /// Opens the log at `path` at its end. A log that does not exist yet
    /// counts as empty, and is read from its start once it appears.
    ///
    /// # Errors
    ///
    /// Returns the error of opening an existing log or of seeking to its
    /// end.
    pub fn at_end_or_empty(path: &Path) -> io::Result<Self> {
        Ok(Self::at_end(path)?.unwrap_or_else(|| Self {
            path: path.to_path_buf(),
            file: None,
            partial: Vec::new(),
        }))
    }

    /// Returns the log's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the complete lines written since the last read, without
    /// their line endings, up to [`MOST_FOLLOW_BYTES`] bytes of them; the
    /// rest waits for the next read. A line longer than
    /// [`MOST_FOLLOW_BYTES`] comes back in parts of at most that many
    /// bytes. When the log becomes shorter than what was read, as when a
    /// cap empties it, the next read starts at its start. When the follower
    /// has read all of its open file and the path now names another file,
    /// as after the log is renamed and a new one is created, the follower
    /// returns the old file's unfinished last line, if any, and the next
    /// read starts at the new file's start.
    ///
    /// # Errors
    ///
    /// Returns the error that kept the log from being read.
    pub fn lines(&mut self) -> io::Result<Vec<String>> {
        let file = match &mut self.file {
            Some(file) => file,
            None => match File::open(&self.path) {
                Ok(file) => self.file.insert(file),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(error) => return Err(error),
            },
        };
        let position = file.stream_position()?;
        let length = file.metadata()?.len();
        if length < position {
            // A cap emptied the log, so the next line starts at its start.
            file.seek(SeekFrom::Start(0))?;
            self.partial.clear();
        }
        // The buffer holds the unfinished line and this read, and never
        // more than `MOST_FOLLOW_BYTES` bytes in all.
        let held = u64::try_from(self.partial.len()).unwrap_or(u64::MAX);
        let room = MOST_FOLLOW_BYTES.saturating_sub(held);
        let mut bytes = Vec::new();
        file.take(room).read_to_end(&mut bytes)?;
        let drained = u64::try_from(bytes.len()).unwrap_or(u64::MAX) < room;
        let replaced = drained && replaced(file, &self.path)?;
        self.partial.extend_from_slice(&bytes);
        let mut lines = Vec::new();
        if let Some(end) = self.partial.iter().rposition(|byte| *byte == b'\n') {
            let rest = self.partial.split_off(end + 1);
            let complete = std::mem::replace(&mut self.partial, rest);
            lines.extend(
                String::from_utf8_lossy(&complete)
                    .lines()
                    .map(str::to_owned),
            );
        }
        let full = u64::try_from(self.partial.len()).unwrap_or(u64::MAX) >= MOST_FOLLOW_BYTES;
        if full || (replaced && !self.partial.is_empty()) {
            let part = std::mem::take(&mut self.partial);
            lines.push(String::from_utf8_lossy(&part).into_owned());
        }
        if replaced {
            self.file = None;
        }
        Ok(lines)
    }
}

/// Reports whether `path` names a file other than the open `file`. A path
/// that names no file is not a replacement, because the follower keeps
/// reading its open file until a new one appears.
fn replaced(file: &File, path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt as _;
    let open = file.metadata()?;
    match std::fs::metadata(path) {
        Ok(named) => Ok((named.dev(), named.ino()) != (open.dev(), open.ino())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

impl LogLines for LogFollower {
    fn new_lines(&mut self) -> io::Result<Vec<String>> {
        self.lines()
    }
}
