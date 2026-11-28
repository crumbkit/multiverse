//! The technical failures of workspace operations: a program that cannot
//! run, a file that cannot be read or written, or a record that cannot be
//! understood. The outcomes that the domain expects, such as a taken name
//! or a checkout with changes, are variants of each operation's outcome
//! instead.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

#[cfg(test)]
mod tests;

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
    /// An origin that the person named in place of a repository has no
    /// default branch to start a checkout from, as an origin without
    /// commits has.
    OriginNoBranch {
        /// The origin's URL, without a user name or password.
        origin: String,
        /// The mirror's directory.
        mirror: PathBuf,
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
    /// [`crate::Store::update`] or [`crate::Runs::stable_id`] found no
    /// record of the workspace, such as when another command archived or
    /// removed the workspace after this one read its record or while it
    /// waited for the workspace's lock, so it wrote nothing.
    RecordMissing {
        /// The workspace's name.
        name: crate::name::WorkspaceName,
    },
    /// [`crate::Store::update`] or [`crate::Store::advance`] found the
    /// workspace's record in another state than the one the command read,
    /// such as when `mxv new` finished it meanwhile, so it wrote nothing.
    StateChanged {
        /// The workspace's name.
        name: crate::name::WorkspaceName,
    },
    /// [`crate::Store::advance`] was asked for a move between two states
    /// that no workspace makes, so it wrote nothing. No command asks for
    /// one, so this is a defect of mxv.
    TransitionRefused {
        /// The workspace's name.
        name: crate::name::WorkspaceName,
        /// The state that the record holds.
        from: crate::record::WorkspaceState,
        /// The state that the caller asked for.
        to: crate::record::WorkspaceState,
    },
    /// [`crate::Store::create`], [`crate::Store::update`],
    /// [`crate::Store::advance`] or [`crate::Store::repoint`] was given a
    /// workspace whose checkout is not made the way its kind says: a clone
    /// without a mirror, or a worktree with one. It wrote nothing, because
    /// the store reads such a record as corrupt. No command builds one, so
    /// this is a defect of mxv.
    MismatchedCheckout {
        /// The workspace's name.
        name: crate::name::WorkspaceName,
        /// The folder of the first checkout that does not match.
        folder: String,
    },
    /// A method that acts on the workspace `acted_on` was given the lock of
    /// the workspace `locked`. It did nothing. No command passes such a
    /// lock, so this is a defect of mxv.
    WrongLock {
        /// The name of the workspace that the lock holds.
        locked: crate::name::WorkspaceName,
        /// The name of the workspace that the method acts on.
        acted_on: crate::name::WorkspaceName,
    },
    /// A method of [`crate::Workspaces`] was given an [`crate::Archives`]
    /// of another host profile than its own, so the two would write and
    /// read archive references, folders and commits under different names.
    /// The method did nothing. mxv gives both [`crate::Host::mxv`], so
    /// this is a defect of the program that paired them.
    HostMismatch {
        /// The name of the host of the workspaces.
        workspaces: crate::host::HostName,
        /// The name of the host of the archives.
        archives: crate::host::HostName,
    },
    /// The workspace or the archive belongs to another host profile than
    /// the one that the [`crate::Workspaces`] or [`crate::Archives`] run
    /// under, as its [`crate::HostData::profile`] says, so only that host
    /// changes it. Nothing was written.
    OtherHost {
        /// The workspace's name.
        workspace: crate::name::WorkspaceName,
        /// The name of the host profile that owns it.
        owner: crate::host::HostName,
    },
    /// The host data given for a workspace holds more than
    /// [`crate::MOST_HOST_VALUES`] values or more than
    /// [`crate::MOST_HOST_DATA_BYTES`] bytes of JSON. Nothing was written.
    HostDataTooLarge {
        /// The workspace's name.
        name: crate::name::WorkspaceName,
        /// The number of values.
        values: usize,
        /// The bytes of the data as JSON.
        bytes: usize,
    },
    /// The [`crate::ArchiveHook::save`] of the archive's host failed. The
    /// archive stopped before it moved any checkout, so the workspace is
    /// whole and the next archive calls the hook again.
    HostHookFailed {
        /// The workspace's name.
        name: crate::name::WorkspaceName,
        /// The hook's message.
        message: String,
    },
    /// The host's hook wrote something that an archive cannot keep: a
    /// symbolic link, a socket, a device or a name that is not UTF-8. The
    /// archive stopped before it moved any checkout.
    HostFileUnkeepable {
        /// The workspace's name.
        name: crate::name::WorkspaceName,
        /// The entry, relative to the directory that the hook wrote into.
        path: String,
    },
    /// The host's hook wrote more than [`crate::MOST_HOST_FILES`] files.
    /// The archive stopped before it moved any checkout.
    HostFilesTooMany {
        /// The workspace's name.
        name: crate::name::WorkspaceName,
        /// The number of files, counted up to one past the limit.
        count: usize,
    },
    /// The host's hook wrote a file larger than
    /// [`crate::ARCHIVE_MAX_FILE`]. The archive stopped before it moved any
    /// checkout.
    HostFileTooLarge {
        /// The workspace's name.
        name: crate::name::WorkspaceName,
        /// The file, relative to the directory that the hook wrote into.
        path: String,
        /// Its size in bytes.
        bytes: u64,
    },
    /// The host's hook wrote more than [`crate::MOST_HOST_FILE_BYTES`] bytes
    /// in all. The archive stopped before it moved any checkout.
    HostFilesTooLarge {
        /// The workspace's name.
        name: crate::name::WorkspaceName,
        /// The bytes counted, up to the file that passed the limit.
        bytes: u64,
    },
    /// The workspace's record holds no stable id, and the archive record of
    /// its name is an unfinished archive of an older multiverse without
    /// one, or cannot be read, so mxv gives the workspace no id and cannot
    /// name its containers. It started nothing.
    ArchiveUnfinished {
        /// The workspace's name.
        name: crate::name::WorkspaceName,
        /// Whether the archive record can be read. `mxv archive` resumes
        /// a readable one; an unreadable one needs `mxv remove` or a
        /// person who moves the archive's directory.
        readable: bool,
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
    /// Another mxv command holds the lock of a repository's grant file,
    /// such as one that grants the permissions of the same repository's
    /// units, for all of the lock wait, so the grant wrote nothing.
    GrantsBusy {
        /// The grant file's lock file.
        path: PathBuf,
    },
    /// The Keychain refused to delete the generated and saved secrets of a
    /// workspace that `mxv remove` deletes, such as when it is locked or
    /// the person refused its prompt, so the removal kept the workspace's
    /// record. Some of the secrets may be gone, and running the removal
    /// again deletes the rest.
    KeychainRefused {
        /// The Keychain service that holds the secrets, such as
        /// `starmap.mxv-<id>`.
        service: String,
        /// The Keychain's reason, which holds no value.
        reason: String,
    },
    /// Every dated name that a repair tries for an older archive, from
    /// `NAME-<date>` to `NAME-<date>-<most>`, is taken by a workspace, an
    /// archive or a directory, so the repair changed nothing.
    DatedNamesTaken {
        /// The archive's name.
        name: crate::name::WorkspaceName,
        /// The last dated name that the repair tried.
        last: String,
    },
    /// A restore that stopped made the workspace's directory, and the
    /// directory holds entries that the restore's mark does not account
    /// for, such as files that the person put there afterwards. mxv deleted
    /// nothing, and the archive keeps its mark.
    StoppedRestoreHoldsOtherFiles {
        /// The archive's name.
        name: crate::name::WorkspaceName,
        /// The workspace's directory.
        path: PathBuf,
        /// Each entry that the mark does not account for, relative to
        /// `path`, in sorted order. A directory that the mark does not
        /// account for is named once, without its contents.
        paths: Vec<String>,
    },
    /// Another mxv command holds the lock of a mirror, such as a
    /// `mxv new` that still fetches it, for longer than the wait.
    MirrorBusy {
        /// The mirror's directory.
        mirror: PathBuf,
    },
    /// The fetch of a mirror from its origin failed, such as when the
    /// origin does not answer or refuses the person's credentials.
    MirrorFetchFailed {
        /// The origin's URL.
        origin: String,
        /// The mirror's directory.
        mirror: PathBuf,
        /// The line of git's standard error that says what failed.
        message: String,
    },
    /// ssh refused to connect to the host of an SSH origin, at once and
    /// without asking a question. mxv makes ssh refuse where it would have
    /// asked for a passphrase or about a host key, so a fetch never waits on
    /// a person who may not be there.
    SshRefused {
        /// The origin's URL, with its user name and without a password.
        remote: String,
        /// Where ssh logs in: `user@host` when the URL names a user, and
        /// `host` otherwise, with the host name in lower case and without
        /// a port. [`Error::ssh_host`] returns the host alone.
        login: String,
        /// The port that the URL names, when it is not 22.
        port: Option<u16>,
        /// Why ssh refused.
        refusal: SshRefusal,
        /// The line of ssh's standard error that says why.
        message: String,
    },
    /// A template of workspace manifests cannot be used, such as one that
    /// is not valid TOML, has no valid `[template]` table, or is not a
    /// valid manifest once that table is removed.
    TemplateInvalid {
        /// The template's file.
        path: PathBuf,
        /// What is wrong, in a sentence.
        message: String,
    },
    /// More than one template lists the origin of the repository, so mxv
    /// cannot tell which one to copy.
    TemplateAmbiguous {
        /// The origin's URL, without credentials.
        origin: String,
        /// The templates that list it, in name order.
        templates: Vec<PathBuf>,
    },
    /// A file that `--manifest` names could have been written by a
    /// repository or an agent, because it lies inside a git work tree or
    /// under the root of the workspaces, so mxv does not trust it as a
    /// workspace manifest.
    ManifestUntrusted {
        /// The file, as the person named it.
        path: PathBuf,
        /// Where it lies, in a clause, such as `it lies inside the git work
        /// tree /home/someone/app`.
        reason: String,
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
        /// docker's own line when docker printed one, and otherwise the whole
        /// sentence of phase or mxv that says what failed.
        message: String,
        /// The line that docker printed for its own error, which
        /// `message` then also holds, or `None`. Without it, `message` is
        /// the whole sentence of phase or mxv, which the error prints alone.
        docker_line: Option<String>,
    },
    /// docker has no free address pool for a network, so it cannot
    /// connect a container.
    DockerNoAddressPool {
        /// docker's own line when docker printed one, and otherwise the whole
        /// sentence of phase or mxv that says what failed.
        message: String,
        /// The line that docker printed for its own error, which
        /// `message` then also holds, or `None`. Without it, `message` is
        /// the whole sentence of phase or mxv, which the error prints alone.
        docker_line: Option<String>,
    },
    /// The `docker` program runs, and its daemon does not answer.
    DockerUnavailable {
        /// docker's own line when docker printed one, and otherwise the whole
        /// sentence of phase or mxv that says what failed.
        message: String,
        /// The line that docker printed for its own error, which
        /// `message` then also holds, or `None`. Without it, `message` is
        /// the whole sentence of phase or mxv, which the error prints alone.
        docker_line: Option<String>,
    },
    /// A docker command failed or missed its deadline.
    DockerFailed {
        /// The docker command, such as `run`.
        operation: String,
        /// The compose service of the container that the command worked
        /// on, which is the name of its process in the manifest, such as
        /// `server`, when it worked on one. It is never a container's id
        /// or Docker name.
        container: Option<String>,
        /// docker's own line when docker printed one, and otherwise the whole
        /// sentence of phase or mxv that says what failed.
        message: String,
        /// The line that docker printed for its own error, which
        /// `message` then also holds, or `None`. Without it, `message` is
        /// the whole sentence of phase or mxv, which the error prints alone.
        docker_line: Option<String>,
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
    /// phase could not start, stop or report the workspace's app, for a
    /// reason that no other variant names, such as a keeper that gave no
    /// handshake.
    Phase {
        /// phase's stable code, such as `phase::keeper::unstartable`.
        code: &'static str,
        /// phase's message, which holds no environment value.
        message: String,
    },
}

/// Returns the host of `login`, an [`Error::SshRefused`] login of the form
/// `user@host` or `host`.
fn ssh_host(login: &str) -> &str {
    login.rsplit_once('@').map_or(login, |(_, host)| host)
}

impl Error {
    /// Returns the host that ssh refused to connect to, for an
    /// [`Error::SshRefused`], and `None` for every other failure.
    #[must_use]
    pub fn ssh_host(&self) -> Option<&str> {
        match self {
            Self::SshRefused { login, .. } => Some(ssh_host(login)),
            Self::RepositoryNotFound { .. }
            | Self::NoOrigin { .. }
            | Self::EmptyOrigin { .. }
            | Self::NoDefaultBranch { .. }
            | Self::OriginNoBranch { .. }
            | Self::ProgramMissing { .. }
            | Self::CommandFailed { .. }
            | Self::TimedOut { .. }
            | Self::StateUnreadable { .. }
            | Self::Unwritable { .. }
            | Self::CorruptRecord { .. }
            | Self::Inaccessible { .. }
            | Self::MismatchedRecord { .. }
            | Self::RecordMissing { .. }
            | Self::StateChanged { .. }
            | Self::TransitionRefused { .. }
            | Self::MismatchedCheckout { .. }
            | Self::WrongLock { .. }
            | Self::HostMismatch { .. }
            | Self::OtherHost { .. }
            | Self::HostDataTooLarge { .. }
            | Self::HostHookFailed { .. }
            | Self::HostFileUnkeepable { .. }
            | Self::HostFilesTooMany { .. }
            | Self::HostFileTooLarge { .. }
            | Self::HostFilesTooLarge { .. }
            | Self::ArchiveUnfinished { .. }
            | Self::NewerRecord { .. }
            | Self::ProcessUnstartable { .. }
            | Self::ProcessTable { .. }
            | Self::UnsupportedOs { .. }
            | Self::Busy { .. }
            | Self::GrantsBusy { .. }
            | Self::KeychainRefused { .. }
            | Self::DatedNamesTaken { .. }
            | Self::StoppedRestoreHoldsOtherFiles { .. }
            | Self::MirrorBusy { .. }
            | Self::MirrorFetchFailed { .. }
            | Self::TemplateInvalid { .. }
            | Self::TemplateAmbiguous { .. }
            | Self::ManifestUntrusted { .. }
            | Self::NoFreePort { .. }
            | Self::DockerMissing { .. }
            | Self::DockerComposeMissing { .. }
            | Self::DockerNoAddressPool { .. }
            | Self::DockerUnavailable { .. }
            | Self::DockerFailed { .. }
            | Self::DockerForeign { .. }
            | Self::Phase { .. } => None,
        }
    }
}

/// Why ssh refused to connect to the host of an SSH origin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshRefusal {
    /// The host accepted none of the keys that ssh offered, such as when
    /// the only key has a passphrase and no ssh-agent holds it.
    AuthenticationFailed,
    /// The known hosts file has no key for the host, and ssh would have
    /// asked whether to trust the key that the host showed.
    HostKeyUnverified,
    /// The host showed a key that differs from the one in the known hosts
    /// file.
    HostKeyChanged,
}

/// Returns `message` after `: `, or nothing when it is empty, for the end
/// of a failure's text.
fn detail(message: &str) -> String {
    if message.is_empty() {
        String::new()
    } else {
        format!(": {message}")
    }
}

impl Error {
    /// Writes the text of a failure to lock or fetch a mirror.
    fn fmt_fetch(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OriginNoBranch { origin, .. } => write!(
                formatter,
                "the origin {origin} has no default branch, so a workspace has nothing to branch from"
            ),
            Self::MirrorBusy { mirror } => write!(
                formatter,
                "another mxv command holds the lock of the mirror {} and has not released it",
                mirror.display()
            ),
            Self::MirrorFetchFailed {
                origin,
                mirror,
                message,
            } => write!(
                formatter,
                "mxv could not fetch {origin} into the mirror {}{}",
                mirror.display(),
                detail(message)
            ),
            Self::SshRefused {
                remote,
                login,
                refusal,
                message,
                ..
            } => {
                let host = ssh_host(login);
                let why = match refusal {
                    SshRefusal::AuthenticationFailed => {
                        "accepted none of the keys that ssh offered"
                    }
                    SshRefusal::HostKeyUnverified => {
                        "has a key that the known hosts file does not hold"
                    }
                    SshRefusal::HostKeyChanged => {
                        "has a key that differs from the one in the known hosts file"
                    }
                };
                write!(
                    formatter,
                    "ssh refused to fetch {remote}, because {host} {why}{}",
                    detail(message)
                )
            }
            Self::RepositoryNotFound { .. }
            | Self::NoOrigin { .. }
            | Self::EmptyOrigin { .. }
            | Self::NoDefaultBranch { .. }
            | Self::ProgramMissing { .. }
            | Self::CommandFailed { .. }
            | Self::TimedOut { .. }
            | Self::StateUnreadable { .. }
            | Self::Unwritable { .. }
            | Self::CorruptRecord { .. }
            | Self::Inaccessible { .. }
            | Self::MismatchedRecord { .. }
            | Self::RecordMissing { .. }
            | Self::StateChanged { .. }
            | Self::TransitionRefused { .. }
            | Self::MismatchedCheckout { .. }
            | Self::WrongLock { .. }
            | Self::HostMismatch { .. }
            | Self::OtherHost { .. }
            | Self::HostDataTooLarge { .. }
            | Self::HostHookFailed { .. }
            | Self::HostFileUnkeepable { .. }
            | Self::HostFilesTooMany { .. }
            | Self::HostFileTooLarge { .. }
            | Self::HostFilesTooLarge { .. }
            | Self::ArchiveUnfinished { .. }
            | Self::NewerRecord { .. }
            | Self::ProcessUnstartable { .. }
            | Self::ProcessTable { .. }
            | Self::UnsupportedOs { .. }
            | Self::Busy { .. }
            | Self::GrantsBusy { .. }
            | Self::KeychainRefused { .. }
            | Self::DatedNamesTaken { .. }
            | Self::StoppedRestoreHoldsOtherFiles { .. }
            | Self::TemplateInvalid { .. }
            | Self::TemplateAmbiguous { .. }
            | Self::ManifestUntrusted { .. }
            | Self::NoFreePort { .. }
            | Self::DockerMissing { .. }
            | Self::DockerComposeMissing { .. }
            | Self::DockerNoAddressPool { .. }
            | Self::DockerUnavailable { .. }
            | Self::DockerFailed { .. }
            | Self::DockerForeign { .. }
            | Self::Phase { .. } => Ok(()),
        }
    }
}

/// Writes mxv's sentence `lead` followed by docker's own line, when
/// docker printed one, and otherwise phase's `message` alone, so no words
/// repeat. A failure with neither writes `lead` alone.
fn lead_in(
    formatter: &mut fmt::Formatter<'_>,
    lead: &dyn fmt::Display,
    docker_line: Option<&str>,
    message: &str,
) -> fmt::Result {
    match docker_line {
        Some(line) => write!(formatter, "{lead}: {line}"),
        None if message.is_empty() => write!(formatter, "{lead}"),
        None => formatter.write_str(message),
    }
}

impl Error {
    /// Writes the text of a docker failure. A failure whose message is
    /// phase's whole sentence prints that sentence alone.
    fn fmt_docker(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DockerMissing { .. } => {
                formatter.write_str("the manifest declares containers, and docker is not installed")
            }
            Self::DockerComposeMissing {
                message,
                docker_line,
            } => lead_in(
                formatter,
                &"docker runs, and its compose plugin is not installed",
                docker_line.as_deref(),
                message,
            ),
            Self::DockerNoAddressPool {
                message,
                docker_line,
            } => lead_in(
                formatter,
                &"docker has no free address pool for a container's network",
                docker_line.as_deref(),
                message,
            ),
            Self::DockerUnavailable {
                message,
                docker_line,
            } => lead_in(
                formatter,
                &"the docker daemon does not answer",
                docker_line.as_deref(),
                message,
            ),
            Self::DockerFailed {
                operation,
                container,
                message,
                docker_line,
            } => {
                let lead = match container {
                    Some(container) => {
                        format!("docker {operation} failed for the container {container}")
                    }
                    None => format!("docker {operation} failed"),
                };
                lead_in(formatter, &lead, docker_line.as_deref(), message)
            }
            Self::DockerForeign { kind, name } => write!(
                formatter,
                "the docker {kind} {name} has a name of mxv's, and its labels name another mxv state directory or none, so mxv leaves it alone"
            ),
            Self::RepositoryNotFound { .. }
            | Self::NoOrigin { .. }
            | Self::EmptyOrigin { .. }
            | Self::NoDefaultBranch { .. }
            | Self::OriginNoBranch { .. }
            | Self::ProgramMissing { .. }
            | Self::CommandFailed { .. }
            | Self::TimedOut { .. }
            | Self::StateUnreadable { .. }
            | Self::Unwritable { .. }
            | Self::CorruptRecord { .. }
            | Self::Inaccessible { .. }
            | Self::MismatchedRecord { .. }
            | Self::RecordMissing { .. }
            | Self::StateChanged { .. }
            | Self::TransitionRefused { .. }
            | Self::MismatchedCheckout { .. }
            | Self::WrongLock { .. }
            | Self::HostMismatch { .. }
            | Self::OtherHost { .. }
            | Self::HostDataTooLarge { .. }
            | Self::HostHookFailed { .. }
            | Self::HostFileUnkeepable { .. }
            | Self::HostFilesTooMany { .. }
            | Self::HostFileTooLarge { .. }
            | Self::HostFilesTooLarge { .. }
            | Self::ArchiveUnfinished { .. }
            | Self::NewerRecord { .. }
            | Self::ProcessUnstartable { .. }
            | Self::ProcessTable { .. }
            | Self::UnsupportedOs { .. }
            | Self::Busy { .. }
            | Self::GrantsBusy { .. }
            | Self::KeychainRefused { .. }
            | Self::DatedNamesTaken { .. }
            | Self::StoppedRestoreHoldsOtherFiles { .. }
            | Self::MirrorBusy { .. }
            | Self::MirrorFetchFailed { .. }
            | Self::SshRefused { .. }
            | Self::TemplateInvalid { .. }
            | Self::TemplateAmbiguous { .. }
            | Self::ManifestUntrusted { .. }
            | Self::NoFreePort { .. }
            | Self::Phase { .. } => Ok(()),
        }
    }
}

impl Error {
    /// Writes the text of a repair, a restore or a removal of an archive
    /// that found something in its way and changed nothing.
    fn fmt_repair(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatedNamesTaken { name, last } => write!(
                formatter,
                "every dated name for the older archive of {name}, up to {last}, is already taken"
            ),
            Self::StoppedRestoreHoldsOtherFiles { name, path, paths } => write!(
                formatter,
                "a stopped restore of {name} made the directory {}, which now holds entries that the restore did not make, so mxv deleted nothing and kept the record of what the restore made: {}",
                path.display(),
                paths.join(", ")
            ),
            Self::RepositoryNotFound { .. }
            | Self::NoOrigin { .. }
            | Self::EmptyOrigin { .. }
            | Self::NoDefaultBranch { .. }
            | Self::OriginNoBranch { .. }
            | Self::ProgramMissing { .. }
            | Self::CommandFailed { .. }
            | Self::TimedOut { .. }
            | Self::StateUnreadable { .. }
            | Self::Unwritable { .. }
            | Self::CorruptRecord { .. }
            | Self::Inaccessible { .. }
            | Self::MismatchedRecord { .. }
            | Self::RecordMissing { .. }
            | Self::StateChanged { .. }
            | Self::TransitionRefused { .. }
            | Self::MismatchedCheckout { .. }
            | Self::WrongLock { .. }
            | Self::HostMismatch { .. }
            | Self::OtherHost { .. }
            | Self::HostDataTooLarge { .. }
            | Self::HostHookFailed { .. }
            | Self::HostFileUnkeepable { .. }
            | Self::HostFilesTooMany { .. }
            | Self::HostFileTooLarge { .. }
            | Self::HostFilesTooLarge { .. }
            | Self::ArchiveUnfinished { .. }
            | Self::NewerRecord { .. }
            | Self::ProcessUnstartable { .. }
            | Self::ProcessTable { .. }
            | Self::UnsupportedOs { .. }
            | Self::Busy { .. }
            | Self::GrantsBusy { .. }
            | Self::KeychainRefused { .. }
            | Self::MirrorBusy { .. }
            | Self::MirrorFetchFailed { .. }
            | Self::SshRefused { .. }
            | Self::TemplateInvalid { .. }
            | Self::TemplateAmbiguous { .. }
            | Self::ManifestUntrusted { .. }
            | Self::NoFreePort { .. }
            | Self::DockerMissing { .. }
            | Self::DockerComposeMissing { .. }
            | Self::DockerNoAddressPool { .. }
            | Self::DockerUnavailable { .. }
            | Self::DockerFailed { .. }
            | Self::DockerForeign { .. }
            | Self::Phase { .. } => Ok(()),
        }
    }

    /// Writes the text of a template or a `--manifest` file that mxv
    /// cannot use.
    fn fmt_template(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TemplateInvalid { path, message } => write!(
                formatter,
                "the template {} cannot be used: {message}",
                path.display()
            ),
            Self::ManifestUntrusted { path, reason } => write!(
                formatter,
                "mxv does not copy {} as a workspace manifest, because {reason}, where a repository or an agent could have written it",
                path.display()
            ),
            Self::TemplateAmbiguous { origin, templates } => write!(
                formatter,
                "more than one template lists the origin {origin}: {}",
                templates
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::RepositoryNotFound { .. }
            | Self::NoOrigin { .. }
            | Self::EmptyOrigin { .. }
            | Self::NoDefaultBranch { .. }
            | Self::OriginNoBranch { .. }
            | Self::ProgramMissing { .. }
            | Self::CommandFailed { .. }
            | Self::TimedOut { .. }
            | Self::StateUnreadable { .. }
            | Self::Unwritable { .. }
            | Self::CorruptRecord { .. }
            | Self::Inaccessible { .. }
            | Self::MismatchedRecord { .. }
            | Self::RecordMissing { .. }
            | Self::StateChanged { .. }
            | Self::TransitionRefused { .. }
            | Self::MismatchedCheckout { .. }
            | Self::WrongLock { .. }
            | Self::HostMismatch { .. }
            | Self::OtherHost { .. }
            | Self::HostDataTooLarge { .. }
            | Self::HostHookFailed { .. }
            | Self::HostFileUnkeepable { .. }
            | Self::HostFilesTooMany { .. }
            | Self::HostFileTooLarge { .. }
            | Self::HostFilesTooLarge { .. }
            | Self::ArchiveUnfinished { .. }
            | Self::NewerRecord { .. }
            | Self::ProcessUnstartable { .. }
            | Self::ProcessTable { .. }
            | Self::UnsupportedOs { .. }
            | Self::Busy { .. }
            | Self::GrantsBusy { .. }
            | Self::KeychainRefused { .. }
            | Self::DatedNamesTaken { .. }
            | Self::StoppedRestoreHoldsOtherFiles { .. }
            | Self::MirrorBusy { .. }
            | Self::MirrorFetchFailed { .. }
            | Self::SshRefused { .. }
            | Self::NoFreePort { .. }
            | Self::DockerMissing { .. }
            | Self::DockerComposeMissing { .. }
            | Self::DockerNoAddressPool { .. }
            | Self::DockerUnavailable { .. }
            | Self::DockerFailed { .. }
            | Self::DockerForeign { .. }
            | Self::Phase { .. } => Ok(()),
        }
    }
}

impl Error {
    /// Writes the text of a failure that concerns a host's data or files.
    fn fmt_host(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what_to_do = "so the archive stopped before it moved any checkout; the host keeps fewer files, such as one archive file that it makes itself, and archives again";
        match self {
            Self::OtherHost { workspace, owner } => write!(
                formatter,
                "the workspace {workspace} belongs to the host {owner}, so only {owner} changes it and nothing was written"
            ),
            Self::HostDataTooLarge {
                name,
                values,
                bytes,
            } => write!(
                formatter,
                "the host data of the workspace {name} holds {values} values in {bytes} bytes, and a record keeps at most {} values in {} bytes, so nothing was written; keep fewer values and put bulk data in the host's files",
                crate::record::MOST_HOST_VALUES,
                crate::record::MOST_HOST_DATA_BYTES,
            ),
            Self::HostHookFailed { name, message } => write!(
                formatter,
                "the host's archive hook failed to save the files of the workspace {name}, so the archive stopped before it moved any checkout: {message}"
            ),
            Self::HostFileUnkeepable { name, path } => write!(
                formatter,
                "the host's archive hook wrote {path} for the workspace {name}, which is not a regular file or directory, {what_to_do}"
            ),
            Self::HostFilesTooMany { name, count } => write!(
                formatter,
                "the host's archive hook wrote at least {count} files for the workspace {name}, more than the {} an archive keeps, {what_to_do}",
                crate::archive::MOST_HOST_FILES
            ),
            Self::HostFileTooLarge { name, path, bytes } => write!(
                formatter,
                "the host's archive hook wrote {path} of {bytes} bytes for the workspace {name}, more than the {} bytes an archive keeps of one file, {what_to_do}",
                crate::archive::ARCHIVE_MAX_FILE
            ),
            Self::HostFilesTooLarge { name, bytes } => write!(
                formatter,
                "the host's archive hook wrote at least {bytes} bytes for the workspace {name}, more than the {} bytes an archive keeps of a host's files, {what_to_do}",
                crate::archive::MOST_HOST_FILE_BYTES
            ),
            Self::RepositoryNotFound { .. }
            | Self::NoOrigin { .. }
            | Self::EmptyOrigin { .. }
            | Self::NoDefaultBranch { .. }
            | Self::OriginNoBranch { .. }
            | Self::ProgramMissing { .. }
            | Self::CommandFailed { .. }
            | Self::TimedOut { .. }
            | Self::StateUnreadable { .. }
            | Self::Unwritable { .. }
            | Self::CorruptRecord { .. }
            | Self::Inaccessible { .. }
            | Self::MismatchedRecord { .. }
            | Self::RecordMissing { .. }
            | Self::StateChanged { .. }
            | Self::TransitionRefused { .. }
            | Self::MismatchedCheckout { .. }
            | Self::WrongLock { .. }
            | Self::HostMismatch { .. }
            | Self::ArchiveUnfinished { .. }
            | Self::NewerRecord { .. }
            | Self::ProcessUnstartable { .. }
            | Self::ProcessTable { .. }
            | Self::UnsupportedOs { .. }
            | Self::Busy { .. }
            | Self::GrantsBusy { .. }
            | Self::KeychainRefused { .. }
            | Self::DatedNamesTaken { .. }
            | Self::StoppedRestoreHoldsOtherFiles { .. }
            | Self::MirrorBusy { .. }
            | Self::MirrorFetchFailed { .. }
            | Self::SshRefused { .. }
            | Self::TemplateInvalid { .. }
            | Self::TemplateAmbiguous { .. }
            | Self::ManifestUntrusted { .. }
            | Self::NoFreePort { .. }
            | Self::DockerMissing { .. }
            | Self::DockerComposeMissing { .. }
            | Self::DockerNoAddressPool { .. }
            | Self::DockerUnavailable { .. }
            | Self::DockerFailed { .. }
            | Self::DockerForeign { .. }
            | Self::Phase { .. } => Ok(()),
        }
    }
}

/// Writes the message of [`Error::ArchiveUnfinished`], apart from
/// [`Error`]'s `fmt`, which would otherwise pass clippy's line limit.
fn archive_unfinished(
    formatter: &mut fmt::Formatter<'_>,
    name: &crate::name::WorkspaceName,
    readable: bool,
) -> fmt::Result {
    if readable {
        write!(
            formatter,
            "the workspace {name} has no stable id beside an archive of it that an older mxv did not finish, so mxv cannot name its containers and started nothing"
        )
    } else {
        write!(
            formatter,
            "the workspace {name} has no stable id, and mxv cannot read the archive record of its name, so mxv cannot name its containers and started nothing"
        )
    }
}

/// Writes the message of [`Error::HostMismatch`], apart from
/// [`Error`]'s `fmt`, which would otherwise pass clippy's line limit.
fn host_mismatch(
    formatter: &mut fmt::Formatter<'_>,
    workspaces: &crate::host::HostName,
    archives: &crate::host::HostName,
) -> fmt::Result {
    write!(
        formatter,
        "the workspaces belong to the host {workspaces} and the archives to the host {archives}, so nothing was done"
    )
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
            Self::StateChanged { name } => write!(
                formatter,
                "the record of the workspace {name} changed state after this command read it, so mxv wrote nothing"
            ),
            Self::TransitionRefused { name, from, to } => write!(
                formatter,
                "mxv cannot move the workspace {name} from {from} to {to}, so it wrote nothing"
            ),
            Self::MismatchedCheckout { name, folder } => write!(
                formatter,
                "the checkout {folder} of the workspace {name} does not match the workspace's kind, so mxv wrote nothing"
            ),
            Self::WrongLock { locked, acted_on } => write!(
                formatter,
                "the lock held is the lock of the workspace {locked}, not of {acted_on}, so mxv did nothing to {acted_on}"
            ),
            Self::HostMismatch {
                workspaces,
                archives,
            } => host_mismatch(formatter, workspaces, archives),
            Self::OtherHost { .. }
            | Self::HostDataTooLarge { .. }
            | Self::HostHookFailed { .. }
            | Self::HostFileUnkeepable { .. }
            | Self::HostFilesTooMany { .. }
            | Self::HostFileTooLarge { .. }
            | Self::HostFilesTooLarge { .. } => self.fmt_host(formatter),
            Self::ArchiveUnfinished { name, readable } => {
                archive_unfinished(formatter, name, *readable)
            }
            Self::RecordMissing { name } => write!(
                formatter,
                "the workspace {name} has no record, so mxv wrote none"
            ),
            Self::NewerRecord { path, version } => write!(
                formatter,
                "the workspace record {} has version {version}, which this version of multiverse cannot read",
                path.display()
            ),
            Self::ProcessUnstartable { process, source } => {
                write!(formatter, "cannot start the unit {process}: {source}")
            }
            Self::ProcessTable { message } => {
                write!(formatter, "cannot read the process table: {message}")
            }
            Self::UnsupportedOs { message } => write!(
                formatter,
                "mxv cannot read this operating system's process table: {message}"
            ),
            Self::Busy { path } => fmt_busy(formatter, "lock", path),
            Self::GrantsBusy { path } => fmt_busy(formatter, "grant lock", path),
            Self::KeychainRefused { service, reason } => fmt_keychain(formatter, service, reason),
            Self::DatedNamesTaken { .. } | Self::StoppedRestoreHoldsOtherFiles { .. } => {
                self.fmt_repair(formatter)
            }
            Self::MirrorBusy { .. }
            | Self::MirrorFetchFailed { .. }
            | Self::SshRefused { .. }
            | Self::OriginNoBranch { .. } => self.fmt_fetch(formatter),
            Self::TemplateInvalid { .. }
            | Self::TemplateAmbiguous { .. }
            | Self::ManifestUntrusted { .. } => self.fmt_template(formatter),
            Self::NoFreePort { source } => {
                write!(formatter, "cannot find a free port: {source}")
            }
            Self::DockerMissing { .. }
            | Self::DockerComposeMissing { .. }
            | Self::DockerNoAddressPool { .. }
            | Self::DockerUnavailable { .. }
            | Self::DockerFailed { .. }
            | Self::DockerForeign { .. } => self.fmt_docker(formatter),
            Self::Phase { code, message } => {
                write!(formatter, "phase failed with {code}: {message}")
            }
        }
    }
}

/// Writes the text of [`Error::Busy`] or [`Error::GrantsBusy`] for the
/// lock file `path`, which `lock` names, such as `grant lock`.
fn fmt_busy(formatter: &mut fmt::Formatter<'_>, lock: &str, path: &std::path::Path) -> fmt::Result {
    write!(
        formatter,
        "another mxv command holds the {lock} {} and has not released it",
        path.display()
    )
}

/// Writes the text of [`Error::KeychainRefused`] for the Keychain service
/// `service`, which refused for `reason`.
fn fmt_keychain(formatter: &mut fmt::Formatter<'_>, service: &str, reason: &str) -> fmt::Result {
    write!(
        formatter,
        "the Keychain refused to delete the secrets of the service {service}, so mxv kept the workspace's record: {reason}"
    )
}

/// A failure of phase's container engine is the docker failure of the same
/// kind, so mxv's codes and messages stay those of its docker backend.
impl From<phase::container::ContainerError> for Error {
    fn from(error: phase::container::ContainerError) -> Self {
        use phase::container::ContainerError as Container;
        match error {
            Container::Missing { source, .. } => Self::DockerMissing { source },
            Container::ComposeMissing { message, .. } => Self::DockerComposeMissing {
                docker_line: docker_line(&message),
                message,
            },
            Container::NoAddressPool { message, .. } => Self::DockerNoAddressPool {
                docker_line: docker_line(&message),
                message,
            },
            Container::Unavailable { message, .. } => Self::DockerUnavailable {
                docker_line: docker_line(&message),
                message,
            },
            Container::Failed {
                operation,
                service,
                message,
                ..
            } => Self::DockerFailed {
                operation: operation.to_string(),
                container: service,
                docker_line: docker_line(&message),
                message,
            },
            Container::Foreign { kind, name, .. } => Self::DockerForeign { kind, name },
            Container::Unreadable { path, source, .. } => Self::StateUnreadable { path, source },
            Container::Cancelled { .. } => Self::engine_failed(&error),
            _ => Self::engine_failed(&error),
        }
    }
}

impl Error {
    /// Returns the failure of phase's container engine that has no docker
    /// failure of its own kind, such as a cancelled command, which
    /// `failure` describes. Its message opens by saying what failed.
    fn engine_failed(failure: &dyn fmt::Display) -> Self {
        Self::DockerFailed {
            operation: "container engine".to_owned(),
            container: None,
            message: format!("phase's container engine failed: {failure}"),
            docker_line: None,
        }
    }
}

/// A failure of phase is the failure of the same kind that mxv's own
/// runner had, so mxv's codes stay the same where phase reports the same
/// fact, and is [`Error::Phase`] otherwise. phase's lock is not mxv's, so
/// its `Busy` names the run record that phase locks.
impl Error {
    /// Returns the failure of `error`, which phase reported for the
    /// workspace whose run record is `run_record`.
    pub(crate) fn of_phase(error: phase::PhaseError, run_record: PathBuf) -> Self {
        use phase::PhaseError as P;
        let code = error.code();
        match error {
            P::Busy { .. } => Self::Busy { path: run_record },
            P::GrantsBusy { path, .. } => Self::GrantsBusy { path },
            P::RecordUnreadable { path, reason, .. } => Self::StateUnreadable {
                path,
                source: io::Error::other(reason),
            },
            P::StateUnwritable { path, source, .. } => Self::Unwritable { path, source },
            P::LogUnreadable { path, source, .. } => Self::StateUnreadable { path, source },
            P::ProcessTable { message, .. } => Self::ProcessTable { message },
            P::Docker { source, .. } => Self::of_docker_error(&source, code),
            P::ProcessUnstartable { app, reason, .. } => Self::ProcessUnstartable {
                process: app.to_string(),
                source: io::Error::other(reason),
            },
            P::NotBuilt { .. }
            | P::Environment { .. }
            | P::NewerRecord { .. }
            | P::GrantsUnreadable { .. }
            | P::NewerGrants { .. }
            | P::KeeperUnstartable { .. }
            | P::SecretOwnerUnknown { .. }
            | P::VolumeOwner { .. }
            | P::NotRunnable { .. }
            | P::UnknownTarget { .. }
            | P::NotRunning { .. }
            | P::ExecUnstartable { .. }
            | P::ExecNeedsTerminal { .. }
            | P::Unplanned { .. } => Self::Phase {
                code: error.code(),
                message: error.to_string(),
            },
            _ => Self::Phase {
                code: error.code(),
                message: error.to_string(),
            },
        }
    }
}

impl Error {
    /// Returns this error with `container` as the container that a failed
    /// docker command worked on, when the command named none.
    #[must_use]
    pub(crate) fn naming(self, container: &str) -> Self {
        match self {
            Self::DockerFailed {
                operation,
                container: None,
                message,
                docker_line,
            } => Self::DockerFailed {
                operation,
                container: Some(container.to_owned()),
                message,
                docker_line,
            },
            other @ (Self::RepositoryNotFound { .. }
            | Self::NoOrigin { .. }
            | Self::EmptyOrigin { .. }
            | Self::NoDefaultBranch { .. }
            | Self::OriginNoBranch { .. }
            | Self::ProgramMissing { .. }
            | Self::CommandFailed { .. }
            | Self::TimedOut { .. }
            | Self::StateUnreadable { .. }
            | Self::Unwritable { .. }
            | Self::CorruptRecord { .. }
            | Self::Inaccessible { .. }
            | Self::MismatchedRecord { .. }
            | Self::RecordMissing { .. }
            | Self::StateChanged { .. }
            | Self::TransitionRefused { .. }
            | Self::MismatchedCheckout { .. }
            | Self::WrongLock { .. }
            | Self::HostMismatch { .. }
            | Self::OtherHost { .. }
            | Self::HostDataTooLarge { .. }
            | Self::HostHookFailed { .. }
            | Self::HostFileUnkeepable { .. }
            | Self::HostFilesTooMany { .. }
            | Self::HostFileTooLarge { .. }
            | Self::HostFilesTooLarge { .. }
            | Self::ArchiveUnfinished { .. }
            | Self::NewerRecord { .. }
            | Self::ProcessUnstartable { .. }
            | Self::ProcessTable { .. }
            | Self::UnsupportedOs { .. }
            | Self::Busy { .. }
            | Self::GrantsBusy { .. }
            | Self::KeychainRefused { .. }
            | Self::DatedNamesTaken { .. }
            | Self::StoppedRestoreHoldsOtherFiles { .. }
            | Self::MirrorBusy { .. }
            | Self::MirrorFetchFailed { .. }
            | Self::SshRefused { .. }
            | Self::TemplateInvalid { .. }
            | Self::TemplateAmbiguous { .. }
            | Self::ManifestUntrusted { .. }
            | Self::NoFreePort { .. }
            | Self::DockerMissing { .. }
            | Self::DockerComposeMissing { .. }
            | Self::DockerNoAddressPool { .. }
            | Self::DockerUnavailable { .. }
            | Self::DockerFailed { .. }
            | Self::DockerForeign { .. }
            | Self::Phase { .. }) => other,
        }
    }

    /// Returns the docker failure `source`, which phase reported with the
    /// code `code`, as the docker failure that mxv reported before phase
    /// ran its apps.
    pub(crate) fn of_docker_error(source: &phase::DockerError, code: &'static str) -> Self {
        let message = match source {
            phase::DockerError::Unavailable { message, .. }
            | phase::DockerError::Refused { message, .. } => message.as_str(),
            _ => "",
        };
        Self::of_docker(code, source.cause(), source.context(), message)
    }

    /// Returns the docker failure that phase reported with the code
    /// `code`, the cause `cause`, the context `context` and the message
    /// `message`, either for a whole run or for one app's outcome.
    ///
    /// The cause chooses the failure, and the context fills it: the docker
    /// command and the compose service of a failed command, the kind and
    /// the name of a foreign object, and docker's own line, which is the
    /// failure's message when docker printed one. Otherwise the message is
    /// phase's. mxv never reads the message. A cancelled command, a
    /// foreign object that phase did not name, and a cause that mxv does
    /// not know, are [`Error::Phase`] with phase's code.
    pub(crate) fn of_docker(
        code: &'static str,
        cause: phase::DockerCause,
        context: &phase::DockerContext,
        message: &str,
    ) -> Self {
        use phase::DockerCause as C;
        let docker_line = context.docker_line.clone();
        let message = docker_line.clone().unwrap_or_else(|| message.to_owned());
        match cause {
            C::DockerMissing => Self::DockerMissing {
                source: io::Error::new(io::ErrorKind::NotFound, message),
            },
            C::ComposeMissing => Self::DockerComposeMissing {
                message,
                docker_line,
            },
            C::DaemonUnavailable => Self::DockerUnavailable {
                message,
                docker_line,
            },
            C::NoAddressPool => Self::DockerNoAddressPool {
                message,
                docker_line,
            },
            C::Foreign => match &context.foreign {
                Some(object) => Self::DockerForeign {
                    kind: docker_kind(object.kind()),
                    name: object.name().to_owned(),
                },
                None => Self::Phase { code, message },
            },
            C::PullFailed | C::Refused | C::TimedOut | C::Unexpected => Self::DockerFailed {
                operation: context.operation.as_deref().unwrap_or("request").to_owned(),
                container: context.service.clone(),
                message,
                docker_line,
            },
            C::Cancelled | C::Unknown => Self::Phase { code, message },
            _ => Self::Phase { code, message },
        }
    }
}

/// Returns `message`, docker's line in an error of phase's container
/// engine, as docker's own line, or `None` when it is empty.
fn docker_line(message: &str) -> Option<String> {
    (!message.is_empty()).then(|| message.to_owned())
}

/// Returns the name of the docker object kind `kind`, such as `volume`.
fn docker_kind(kind: phase::DockerKind) -> &'static str {
    match kind {
        phase::DockerKind::Container => "container",
        phase::DockerKind::Volume => "volume",
        phase::DockerKind::Network => "network",
        phase::DockerKind::Unknown => "object",
        _ => "object",
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
            | Self::OriginNoBranch { .. }
            | Self::CommandFailed { .. }
            | Self::TimedOut { .. }
            | Self::MismatchedRecord { .. }
            | Self::RecordMissing { .. }
            | Self::StateChanged { .. }
            | Self::TransitionRefused { .. }
            | Self::MismatchedCheckout { .. }
            | Self::WrongLock { .. }
            | Self::HostMismatch { .. }
            | Self::OtherHost { .. }
            | Self::HostDataTooLarge { .. }
            | Self::HostHookFailed { .. }
            | Self::HostFileUnkeepable { .. }
            | Self::HostFilesTooMany { .. }
            | Self::HostFileTooLarge { .. }
            | Self::HostFilesTooLarge { .. }
            | Self::ArchiveUnfinished { .. }
            | Self::NewerRecord { .. }
            | Self::ProcessTable { .. }
            | Self::UnsupportedOs { .. }
            | Self::Busy { .. }
            | Self::GrantsBusy { .. }
            | Self::KeychainRefused { .. }
            | Self::DatedNamesTaken { .. }
            | Self::StoppedRestoreHoldsOtherFiles { .. }
            | Self::MirrorBusy { .. }
            | Self::MirrorFetchFailed { .. }
            | Self::SshRefused { .. }
            | Self::TemplateInvalid { .. }
            | Self::TemplateAmbiguous { .. }
            | Self::ManifestUntrusted { .. }
            | Self::DockerUnavailable { .. }
            | Self::DockerComposeMissing { .. }
            | Self::DockerNoAddressPool { .. }
            | Self::DockerFailed { .. }
            | Self::DockerForeign { .. }
            | Self::Phase { .. } => None,
        }
    }
}
