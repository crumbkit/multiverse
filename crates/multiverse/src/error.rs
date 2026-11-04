//! The technical failures of workspace operations: a program that cannot
//! run, a file that cannot be read or written, or a record that cannot be
//! understood. The outcomes that the domain expects, such as a taken name
//! or a checkout with changes, are variants of each operation's outcome
//! instead.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// A technical failure of a workspace operation.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The path given as a repository does not exist, or is not inside a
    /// git repository with a working tree.
    RepositoryNotFound {
        /// The path as the caller gave it.
        path: PathBuf,
    },
    /// The repository has no remote named `origin`.
    NoOrigin {
        /// The repository's top-level directory.
        repository: PathBuf,
    },
    /// The repository's copy of `origin` holds no branch, which is the case
    /// for a new `origin` that has no commits yet.
    EmptyOrigin {
        /// The repository's top-level directory.
        repository: PathBuf,
    },
    /// The repository's copy of `origin` holds branches, but no
    /// `origin/HEAD`, `origin/main` or `origin/master` to branch from.
    NoDefaultBranch {
        /// The repository's top-level directory.
        repository: PathBuf,
    },
    /// The version-control program could not start, such as when `git` is
    /// not on `PATH`.
    ProgramMissing {
        /// The program's name.
        program: &'static str,
        /// Why it could not start.
        source: io::Error,
    },
    /// A version-control command exited with a failure.
    CommandFailed {
        /// What the command was doing, such as `fetch origin`.
        operation: &'static str,
        /// The directory the command ran in.
        dir: PathBuf,
        /// The command's exit code, or `None` when a signal ended it.
        code: Option<i32>,
        /// The last line of the command's standard error.
        message: String,
    },
    /// A version-control command did not end before its deadline, so the
    /// operation stopped it.
    TimedOut {
        /// What the command was doing.
        operation: &'static str,
        /// The directory the command ran in.
        dir: PathBuf,
        /// The deadline that the command missed.
        deadline: Duration,
    },
    /// A file or directory of the state store cannot be read.
    StateUnreadable {
        /// The path of the file or directory.
        path: PathBuf,
        /// Why it cannot be read.
        source: io::Error,
    },
    /// A file or directory of the state store, or of a workspace, cannot be
    /// written or removed.
    Unwritable {
        /// The path of the file or directory.
        path: PathBuf,
        /// Why it cannot be written.
        source: io::Error,
    },
    /// A workspace record is not valid JSON of the record's form.
    CorruptRecord {
        /// The path of the record.
        path: PathBuf,
        /// What is wrong with it.
        source: serde_json::Error,
    },
    /// mxv cannot tell what is at a path, such as when a parent directory
    /// cannot be searched.
    Inaccessible {
        /// The path.
        path: PathBuf,
        /// The error of the lookup.
        source: io::Error,
    },
    /// A workspace record holds a name other than the one its file is
    /// named for, such as on a file system that ignores case.
    MismatchedRecord {
        /// The path of the record.
        path: PathBuf,
        /// The name that the record holds.
        name: String,
    },
    /// A workspace record was written by a newer version of multiverse.
    NewerRecord {
        /// The path of the record.
        path: PathBuf,
        /// The record's version.
        version: u32,
    },
    /// An app's process could not start, such as when its `cwd` does not
    /// exist.
    ProcessUnstartable {
        /// The process's name in the manifest.
        process: String,
        /// Why it could not start.
        source: io::Error,
    },
    /// The process table could not be read, which `ps` reads to tell a
    /// running process from a reused process id.
    ProcessTable {
        /// What went wrong.
        message: String,
    },
    /// The operating system answered a process-table call with a struct
    /// of another size than the one mxv reads, so mxv cannot read it.
    UnsupportedOs {
        /// The call and the sizes.
        message: String,
    },
    /// Another mxv command holds the lock of a workspace, or of port
    /// allocation, for longer than [`crate::LOCK_WAIT`] by default.
    Busy {
        /// The lock file.
        path: PathBuf,
    },
    /// Every port of the port range is held by a workspace or taken.
    NoFreePort {
        /// Which range was searched.
        source: io::Error,
    },
    /// The manifest declares containers, and no `docker` program is on
    /// `PATH`.
    DockerMissing {
        /// Why the program could not start.
        source: io::Error,
    },
    /// The `docker` program runs, and its `compose` plugin, which starts
    /// the containers, is not installed.
    DockerComposeMissing {
        /// The line of docker's standard error that says what failed.
        message: String,
    },
    /// docker has no free address pool for a network, so it cannot
    /// connect a container.
    DockerNoAddressPool {
        /// The line of docker's standard error that says what failed.
        message: String,
    },
    /// The `docker` program runs, and its daemon does not answer.
    DockerUnavailable {
        /// The line of docker's standard error that says what failed.
        message: String,
    },
    /// A docker command failed or missed its deadline.
    DockerFailed {
        /// The docker command, such as `run`.
        operation: &'static str,
        /// The manifest name of the container that the command worked on,
        /// when it worked on one.
        container: Option<String>,
        /// The line of docker's standard error that says what failed.
        message: String,
    },
    /// A docker container or volume has a name of mxv's, or is one that
    /// mxv recorded, and its labels do not name this store, workspace and
    /// object, so mxv leaves it alone.
    DockerForeign {
        /// `container` or `volume`.
        kind: &'static str,
        /// The object's docker name.
        name: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RepositoryNotFound { path } => write!(
                formatter,
                "{} is not a git repository with a working tree",
                path.display()
            ),
            Self::NoOrigin { repository } => write!(
                formatter,
                "the repository {} has no remote named origin",
                repository.display()
            ),
            Self::EmptyOrigin { repository } => write!(
                formatter,
                "the origin of {} has no commits, so a workspace has nothing to branch from",
                repository.display()
            ),
            Self::NoDefaultBranch { repository } => write!(
                formatter,
                "the repository {} has no origin/HEAD, origin/main or origin/master to branch from",
                repository.display()
            ),
            Self::ProgramMissing { program, .. } => write!(formatter, "cannot start {program}"),
            Self::CommandFailed {
                operation,
                dir,
                code,
                message,
            } => {
                write!(formatter, "git {operation} failed in {}", dir.display())?;
                if let Some(code) = code {
                    write!(formatter, " with exit code {code}")?;
                }
                if !message.is_empty() {
                    write!(formatter, ": {message}")?;
                }
                Ok(())
            }
            Self::TimedOut {
                operation,
                dir,
                deadline,
            } => write!(
                formatter,
                "git {operation} in {} did not finish within {} seconds, so mxv stopped it",
                dir.display(),
                deadline.as_secs()
            ),
            Self::StateUnreadable { path, .. } => {
                write!(formatter, "cannot read {}", path.display())
            }
            Self::Unwritable { path, .. } => write!(formatter, "cannot write {}", path.display()),
            Self::CorruptRecord { path, .. } => {
                write!(
                    formatter,
                    "the workspace record {} is corrupt",
                    path.display()
                )
            }
            Self::Inaccessible { path, source } => write!(
                formatter,
                "mxv cannot tell what is at {}: {source}",
                path.display()
            ),
            Self::MismatchedRecord { path, name } => write!(
                formatter,
                "the workspace record {} holds the name {name}, which its file name does not match",
                path.display()
            ),
            Self::NewerRecord { path, version } => write!(
                formatter,
                "the workspace record {} has version {version}, which this version of multiverse cannot read",
                path.display()
            ),
            Self::ProcessUnstartable { process, source } => {
                write!(formatter, "cannot start the process {process}: {source}")
            }
            Self::ProcessTable { message } => {
                write!(formatter, "cannot read the process table: {message}")
            }
            Self::UnsupportedOs { message } => write!(
                formatter,
                "mxv cannot read this operating system's process table: {message}"
            ),
            Self::Busy { path } => write!(
                formatter,
                "another mxv command holds the lock {} and has not released it",
                path.display()
            ),
            Self::NoFreePort { source } => {
                write!(formatter, "cannot find a free port: {source}")
            }
            Self::DockerMissing { .. } => {
                formatter.write_str("the manifest declares containers, and docker is not installed")
            }
            Self::DockerComposeMissing { message } => {
                formatter.write_str("docker runs, and its compose plugin is not installed")?;
                if !message.is_empty() {
                    write!(formatter, ": {message}")?;
                }
                Ok(())
            }
            Self::DockerNoAddressPool { message } => {
                formatter.write_str("docker has no free address pool for a container's network")?;
                if !message.is_empty() {
                    write!(formatter, ": {message}")?;
                }
                Ok(())
            }
            Self::DockerUnavailable { message } => {
                formatter.write_str("the docker daemon does not answer")?;
                if !message.is_empty() {
                    write!(formatter, ": {message}")?;
                }
                Ok(())
            }
            Self::DockerFailed {
                operation,
                container,
                message,
            } => {
                write!(formatter, "docker {operation} failed")?;
                if let Some(container) = container {
                    write!(formatter, " for the container {container}")?;
                }
                if !message.is_empty() {
                    write!(formatter, ": {message}")?;
                }
                Ok(())
            }
            Self::DockerForeign { kind, name } => write!(
                formatter,
                "the docker {kind} {name} has a name of mxv's, and its labels name another mxv state directory or none, so mxv leaves it alone"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ProgramMissing { source, .. }
            | Self::StateUnreadable { source, .. }
            | Self::Unwritable { source, .. }
            | Self::ProcessUnstartable { source, .. }
            | Self::NoFreePort { source }
            | Self::Inaccessible { source, .. }
            | Self::DockerMissing { source } => Some(source),
            Self::CorruptRecord { source, .. } => Some(source),
            Self::RepositoryNotFound { .. }
            | Self::NoOrigin { .. }
            | Self::EmptyOrigin { .. }
            | Self::NoDefaultBranch { .. }
            | Self::CommandFailed { .. }
            | Self::TimedOut { .. }
            | Self::MismatchedRecord { .. }
            | Self::NewerRecord { .. }
            | Self::ProcessTable { .. }
            | Self::UnsupportedOs { .. }
            | Self::Busy { .. }
            | Self::DockerUnavailable { .. }
            | Self::DockerComposeMissing { .. }
            | Self::DockerNoAddressPool { .. }
            | Self::DockerFailed { .. }
            | Self::DockerForeign { .. } => None,
        }
    }
}
