//! The failures of the process verbs `up`, `down`, `restart`, `status` and
//! `logs`, and of `remove` while processes run, each with a stable code and
//! a next step (CLI-3).

use std::fmt;
use std::path::{Path, PathBuf};

use crumbkit::{Call, Code, Failure, Field, FieldValue, Recovery};
use multiverse::{Allow, MANIFEST_FILE, ManifestError, ManifestProblem, Waited};

use crate::failures::remove_volumes_line;

/// The code of a process verb that names no workspace, run outside every
/// workspace.
pub const WORKSPACE_NOT_CURRENT: Code = Code::new("mxv", "workspace", "not_current");
/// The code of a removal that running processes stop.
pub const WORKSPACE_RUNNING: Code = Code::new("mxv", "workspace", "running");
/// The code of `mxv add` to a workspace of one checkout whose containers'
/// volumes hold data under the names that adding a checkout prefixes.
pub const VOLUME_RENAMED: Code = Code::new("mxv", "volume", "renamed");
/// The code of a unit name that phase cannot run, such as one with an
/// uppercase letter.
pub const MANIFEST_UNIT_NAME: Code = Code::new("mxv", "manifest", "unit_name");
/// The code of a manifest that phase cannot run, such as one whose process
/// sets `PATH`.
pub const MANIFEST_UNRUNNABLE: Code = Code::new("mxv", "manifest", "unrunnable");
/// The code of a removal that the workspace's docker volumes stop.
pub const WORKSPACE_VOLUMES: Code = Code::new("mxv", "workspace", "volumes");
/// The code of a checkout without an `mxv.toml`.
pub const MANIFEST_NOT_FOUND: Code = Code::new("mxv", "manifest", "not_found");
/// The code of a manifest key that the schema does not declare.
pub const MANIFEST_UNKNOWN_KEY: Code = Code::new("mxv", "manifest", "unknown_key");
/// The code of a manifest whose `version` a later version of mxv reads.
pub const MANIFEST_NEWER: Code = Code::new("mxv", "manifest", "newer");
/// The code of a manifest section that a later version of mxv reads.
pub const MANIFEST_RESERVED: Code = Code::new("mxv", "manifest", "reserved");
/// The code of a manifest that is not valid TOML of the schema.
pub const MANIFEST_INVALID: Code = Code::new("mxv", "manifest", "invalid");
/// The code of a manifest that exists and cannot be read.
pub const MANIFEST_UNREADABLE: Code = Code::new("mxv", "manifest", "unreadable");
/// The code of two manifests of a workspace with several checkouts that
/// declare the same merged name.
pub const MANIFEST_NAME_CLASH: Code = Code::new("mxv", "manifest", "name_clash");
/// The code of a process name that the workspace does not know.
pub const PROCESS_NOT_FOUND: Code = Code::new("mxv", "process", "not_found");
/// The code of a process that exited before it was ready.
pub const PROCESS_EXITED: Code = Code::new("mxv", "process", "exited");
/// The code of a process that missed its readiness timeout.
pub const PROCESS_NOT_READY: Code = Code::new("mxv", "process", "not_ready");
/// The code of a process whose readiness port another program answered.
pub const PROCESS_FOREIGN_LISTENER: Code = Code::new("mxv", "process", "foreign_listener");
/// The code of a listen port that a descendant of a process holds from
/// outside the process's group.
pub const PROCESS_DETACHED_LISTENER: Code = Code::new("mxv", "process", "detached_listener");
/// The code of a process group that outlived SIGKILL.
pub const PROCESS_UNSTOPPABLE: Code = Code::new("mxv", "process", "unstoppable");
/// The code of a `build` that ended with a status other than 0.
pub const BUILD_FAILED: Code = Code::new("mxv", "build", "failed");
/// The code of a `build` that could not start.
pub const BUILD_UNSTARTABLE: Code = Code::new("mxv", "build", "unstartable");
/// The code of a `build` that outlived its deadline.
pub const BUILD_TIMED_OUT: Code = Code::new("mxv", "build", "timed_out");
/// The code of a stored port that something outside the workspace holds.
pub const PORT_TAKEN: Code = Code::new("mxv", "port", "taken");
/// The code of a port name that the manifest does not declare.
pub const PORT_NOT_FOUND: Code = Code::new("mxv", "port", "not_found");

/// Every code that a [`RunFailure`] returns.
pub(crate) const CODES: &[&Code] = &[
    &WORKSPACE_NOT_CURRENT,
    &WORKSPACE_RUNNING,
    &WORKSPACE_VOLUMES,
    &VOLUME_RENAMED,
    &MANIFEST_UNIT_NAME,
    &MANIFEST_UNRUNNABLE,
    &MANIFEST_NOT_FOUND,
    &MANIFEST_UNKNOWN_KEY,
    &MANIFEST_NEWER,
    &MANIFEST_RESERVED,
    &MANIFEST_INVALID,
    &MANIFEST_UNREADABLE,
    &MANIFEST_NAME_CLASH,
    &PROCESS_NOT_FOUND,
    &PROCESS_EXITED,
    &PROCESS_NOT_READY,
    &PROCESS_FOREIGN_LISTENER,
    &PROCESS_DETACHED_LISTENER,
    &PROCESS_UNSTOPPABLE,
    &BUILD_FAILED,
    &BUILD_UNSTARTABLE,
    &BUILD_TIMED_OUT,
    &PORT_TAKEN,
    &PORT_NOT_FOUND,
];

/// A failure of a process verb.
#[derive(Debug)]
pub(crate) enum RunFailure {
    /// The command line names no workspace, and the current directory is
    /// in none.
    NotCurrent,
    /// The workspace's checkout has no `mxv.toml`.
    ManifestMissing {
        /// The checkout.
        checkout: PathBuf,
        /// The path of the workspace manifest, which does not exist either.
        workspace: PathBuf,
    },
    /// The manifest cannot be read or understood.
    Manifest(ManifestError),
    /// The workspace knows no process with the name.
    UnknownProcess {
        /// The workspace's name.
        workspace: String,
        /// The process's name.
        process: String,
    },
    /// A process's `build` ended with a status other than 0.
    BuildFailed {
        /// The workspace's name.
        workspace: String,
        /// The process.
        process: String,
        /// The build's exit code, or `None` when a signal ended it.
        code: Option<i32>,
        /// The signal that ended the build, or `None`.
        signal: Option<i32>,
    },
    /// A process's `build` could not start.
    BuildUnstartable {
        /// The workspace's name.
        workspace: String,
        /// The process.
        process: String,
        /// Why, in phase's words.
        reason: String,
    },
    /// A process's `build` outlived the deadline of the command's builds.
    BuildTimedOut {
        /// The workspace's name.
        workspace: String,
        /// The process.
        process: String,
        /// How many seconds the builds ran.
        seconds: u64,
    },
    /// Something outside the workspace holds a port that a process
    /// receives.
    PortTaken {
        /// The workspace's name.
        workspace: String,
        /// The process.
        process: String,
        /// The port's name.
        port: String,
        /// The host port.
        number: u16,
        /// The workspace's processes whose groups still have members and
        /// may hold the port, or empty when none does.
        holders: Vec<String>,
    },
    /// The manifest declares no port with the name.
    UnknownPort {
        /// The workspace's name.
        workspace: String,
        /// The port's name.
        port: String,
    },
    /// A process exited before it was ready.
    Exited {
        /// The workspace's name.
        workspace: String,
        /// The process.
        process: String,
        /// Its exit code, or `None` when a signal ended it.
        code: Option<i32>,
        /// The signal that ended it, or `None`.
        signal: Option<i32>,
        /// Its log, or `None` for a container, whose output docker keeps.
        log: Option<Box<Path>>,
    },
    /// A process missed its readiness timeout.
    NotReady {
        /// The workspace's name.
        workspace: String,
        /// The process.
        process: String,
        /// What `up` waited for.
        waited: Box<Waited>,
        /// The timeout in seconds.
        seconds: u64,
        /// Its log, or `None` for a container, whose output docker keeps.
        log: Option<Box<Path>>,
    },
    /// Another program answered on a process's readiness port.
    ForeignListener {
        /// The workspace's name.
        workspace: String,
        /// The process.
        process: String,
        /// The port's name.
        port: String,
        /// The host port.
        number: u16,
        /// The process that holds the port, or `None` when mxv may not
        /// read its descriptors.
        pid: Option<u32>,
        /// The process's log, or `None` for a container, whose output
        /// docker keeps.
        log: Option<Box<Path>>,
    },
    /// A descendant of a process that left the process's group held its
    /// listen port, so `up` stopped both.
    DetachedListener {
        /// The workspace's name.
        workspace: String,
        /// The process.
        process: String,
        /// The port's name.
        port: String,
        /// The host port.
        number: u16,
        /// The descendant.
        pid: u32,
        /// The process's log, or `None` for a container, whose output
        /// docker keeps.
        log: Option<Box<Path>>,
    },
    /// A process group outlived SIGKILL, or `down` left it alone because
    /// it could not prove the group its own.
    Unstoppable {
        /// The process.
        process: String,
        /// Its process group's id.
        pid: u32,
        /// Whether `down` left the group without signalling it, because
        /// the process that led it has ended and nothing proves that the
        /// members left in it are the ones `up` started.
        unverified: bool,
        /// The units that an `up` which found the group did not start, or
        /// none for `down`.
        not_started: Vec<String>,
    },
    /// The workspace has running processes, so `remove` refuses.
    Running {
        /// The workspace's name.
        workspace: String,
        /// The running processes.
        processes: Vec<String>,
    },
    /// `mxv add` would prefix the names of volumes that hold the data of
    /// the workspace's one checkout, so its containers would start on
    /// empty volumes and the old ones would sit unused.
    VolumeRenamed {
        /// The workspace's name.
        workspace: String,
        /// The volumes, each with its old and new docker name.
        volumes: Vec<multiverse::RenamedVolume>,
    },
    /// The workspace's plan cannot be run by phase.
    Unrunnable {
        /// The workspace's name.
        workspace: String,
        /// The unit at fault, or `None` when the fault is the manifest's.
        unit: Option<String>,
        /// What phase refused.
        message: String,
    },
    /// The workspace has docker volumes, which may hold data, so `remove`
    /// keeps the workspace unless `--delete-volumes` allows deleting them.
    Volumes {
        /// The workspace's name.
        workspace: String,
        /// The volumes.
        volumes: Vec<String>,
        /// What the refused removal's flags allowed, which the recovery
        /// keeps.
        allow: Allow,
    },
}

/// The image whose `cp` copies a volume's data into another, pinned by tag.
const COPY_IMAGE: &str = "busybox:1.37.0";

/// Writes the message of [`RunFailure::Unstoppable`] for the process group
/// `pid` of `process`. `not_started` names the units that an `up` which
/// found the group did not start, and is empty for `down`.
fn fmt_unstoppable(
    formatter: &mut fmt::Formatter<'_>,
    process: &str,
    pid: u32,
    unverified: bool,
    not_started: &[String],
) -> fmt::Result {
    let verb = if not_started.is_empty() {
        "`mxv down`"
    } else {
        "phase"
    };
    if unverified {
        write!(
            formatter,
            "the process that led the process group {pid} of {process} has ended and the group still has members, but mxv cannot prove they are the ones it started, so {verb} did not signal them and kept the record"
        )?;
    } else {
        write!(
            formatter,
            "the process group {pid} of {process} still runs after SIGKILL"
        )?;
    }
    if not_started.is_empty() {
        Ok(())
    } else {
        write!(
            formatter,
            "; `mxv up` stopped what it had started and did not start {}",
            not_started.join(", ")
        )
    }
}

/// Returns `path` as text for a field or a hint.
fn shown(path: &Path) -> String {
    path.display().to_string()
}

/// Returns the recovery of the process `process` of `workspace`, which
/// exited or was not ready. mxv starts its own apps, such as
/// mxv-provision, with the workspace, and `up` takes no name of one, so
/// their hint runs `up` for the whole workspace.
fn start_failed(workspace: &str, process: &str) -> Recovery {
    if process.starts_with(multiverse::RESERVED_PREFIX) {
        Recovery::hint(format!(
            "run `mxv logs -w {workspace} {process}` to read what failed, fix it, and run `mxv up -w {workspace}` again"
        ))
    } else {
        Recovery::hint(format!(
            "fix the cause that the log names, and run `mxv up -w {workspace} {process}` again; `mxv logs -w {workspace} {process}` prints the log"
        ))
    }
}

impl Failure for RunFailure {
    fn code(&self) -> &'static Code {
        match self {
            Self::NotCurrent => &WORKSPACE_NOT_CURRENT,
            Self::Running { .. } => &WORKSPACE_RUNNING,
            Self::Volumes { .. } => &WORKSPACE_VOLUMES,
            Self::VolumeRenamed { .. } => &VOLUME_RENAMED,
            Self::Unrunnable { .. } => &MANIFEST_UNRUNNABLE,
            Self::ManifestMissing { .. } => &MANIFEST_NOT_FOUND,
            Self::Manifest(error) => match error.problem {
                ManifestProblem::UnknownKey { .. } => &MANIFEST_UNKNOWN_KEY,
                ManifestProblem::Newer { .. } => &MANIFEST_NEWER,
                ManifestProblem::Reserved { .. } => &MANIFEST_RESERVED,
                ManifestProblem::Unreadable { .. } => &MANIFEST_UNREADABLE,
                ManifestProblem::UnitName { .. } => &MANIFEST_UNIT_NAME,
                ManifestProblem::Unrunnable { .. } => &MANIFEST_UNRUNNABLE,
                ManifestProblem::NameClash { .. } | ManifestProblem::ServiceClash { .. } => {
                    &MANIFEST_NAME_CLASH
                }
                // The library marks the problem non-exhaustive, so a
                // problem that a later version adds reads as invalid until
                // mxv gives it a code of its own.
                _ => &MANIFEST_INVALID,
            },
            Self::UnknownProcess { .. } => &PROCESS_NOT_FOUND,
            Self::Exited { .. } => &PROCESS_EXITED,
            Self::NotReady { .. } => &PROCESS_NOT_READY,
            Self::BuildFailed { .. } => &BUILD_FAILED,
            Self::BuildUnstartable { .. } => &BUILD_UNSTARTABLE,
            Self::BuildTimedOut { .. } => &BUILD_TIMED_OUT,
            Self::ForeignListener { .. } => &PROCESS_FOREIGN_LISTENER,
            Self::DetachedListener { .. } => &PROCESS_DETACHED_LISTENER,
            Self::Unstoppable { .. } => &PROCESS_UNSTOPPABLE,
            Self::PortTaken { .. } => &PORT_TAKEN,
            Self::UnknownPort { .. } => &PORT_NOT_FOUND,
        }
    }

    fn recovery(&self) -> Recovery {
        match self {
            Self::NotCurrent => Recovery::Run(Call::new(&["list"])),
            Self::Running { workspace, .. } => {
                Recovery::Run(Call::new(&["down"]).value("workspace", workspace))
            }
            Self::VolumeRenamed { volumes, .. } => Recovery::hint(format!(
                "copy each volume's data into the volume that the container mounts after the add, {}, then delete each old volume, and run `mxv add` again; to drop the data instead, only delete the old volumes",
                volumes
                    .iter()
                    .map(|volume| format!(
                        "with `docker volume create{} -- {new}` and `docker run --rm -v {old}:/from -v {new}:/to {COPY_IMAGE} cp -a /from/. /to/` and `docker volume rm {old}`",
                        volume
                            .labels
                            .iter()
                            .map(|label| format!(" --label {}", crumbkit::help::quote(label)))
                            .collect::<String>(),
                        new = volume.new,
                        old = volume.old,
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            )),
            Self::Unrunnable { .. } => Recovery::hint("fix what the message names in the manifest"),
            Self::BuildFailed { .. } => Recovery::hint(
                "read the build's output above and fix the build, then run the command again; every running unit is still running",
            ),
            Self::BuildUnstartable { process, .. } => Recovery::hint(format!(
                "check the `build` of {process} in the manifest, such as that its program exists on the PATH, and run the command again"
            )),
            Self::BuildTimedOut { process, .. } => Recovery::hint(format!(
                "find out why the `build` of {process} takes so long, or run it by hand under `deadband run`, and then run the command again"
            )),
            Self::Volumes {
                workspace, allow, ..
            } => Recovery::hint(format!(
                "copy out any data you need from the volumes, then run `{}`, which deletes them",
                remove_volumes_line(workspace, *allow)
            )),
            Self::ManifestMissing {
                checkout,
                workspace,
            } => Recovery::hint(format!(
                "write a workspace manifest at {} or an {MANIFEST_FILE} at the root of {} that declares the app's units",
                shown(workspace),
                shown(checkout)
            )),
            Self::Manifest(error) => match &error.problem {
                ManifestProblem::UnknownKey { key } => Recovery::hint(format!(
                    "remove or rename `{key}` in {}, because the manifest schema does not declare it",
                    shown(&error.path)
                )),
                ManifestProblem::Newer { .. } => {
                    Recovery::hint("upgrade mxv to a version that reads this manifest version")
                }
                ManifestProblem::Reserved { section } => Recovery::hint(format!(
                    "remove the section `{section}` from {}, because this version of mxv does not run it",
                    shown(&error.path)
                )),
                ManifestProblem::Unreadable { .. } => {
                    Recovery::hint(format!("make {} readable", shown(&error.path)))
                }
                ManifestProblem::UnitName { name } => Recovery::hint(format!(
                    "rename the unit `{name}` in {} to 1 to 64 lowercase letters, digits, `-` and `_`, starting with a letter or a digit",
                    shown(&error.path)
                )),
                ManifestProblem::NameClash { name, other } => Recovery::hint(format!(
                    "rename `{name}` in {} or in {}, or give one of the checkouts another folder, whose name prefixes its names, with `--as`",
                    shown(&error.path),
                    shown(other)
                )),
                ManifestProblem::ServiceClash { name, .. } => Recovery::hint(format!(
                    "declare `{name}` the same way in every service of the workspace, or rename it in one of them, in {}",
                    shown(&error.path)
                )),
                _ if error.line.is_some() => Recovery::hint(format!(
                    "fix the line of {} that the message names",
                    shown(&error.path)
                )),
                _ => Recovery::hint(format!(
                    "fix what the message names in {}",
                    shown(&error.path)
                )),
            },
            Self::UnknownProcess { workspace, .. } => {
                Recovery::Run(Call::new(&["status"]).value("workspace", workspace))
            }
            Self::Exited {
                workspace, process, ..
            }
            | Self::NotReady {
                workspace, process, ..
            } => start_failed(workspace, process),
            Self::ForeignListener {
                workspace,
                process,
                number,
                pid: Some(pid),
                ..
            } => Recovery::hint(format!(
                "stop the process {pid}, which listens on port {number}, or make {process} listen on the port it receives, and run `mxv up -w {workspace} {process}` again"
            )),
            Self::ForeignListener {
                workspace,
                process,
                number,
                pid: None,
                ..
            } => Recovery::hint(format!(
                "stop the program of another user that listens on port {number}, or make {process} listen on the port it receives, and run `mxv up -w {workspace} {process}` again"
            )),
            Self::DetachedListener {
                workspace, process, ..
            } => Recovery::hint(format!(
                "make {process} keep the program that listens in its own process group, such as by running the program in the foreground without detaching it, and run `mxv up -w {workspace} {process}` again"
            )),
            Self::Unstoppable {
                pid, not_started, ..
            } => Recovery::hint(if not_started.is_empty() {
                format!("find the process group {pid} with `ps -g {pid}` and stop it by hand")
            } else {
                format!(
                    "find the process group {pid} with `ps -g {pid}` and stop it by hand, then run `mxv up` again"
                )
            }),
            Self::PortTaken {
                workspace, holders, ..
            } if !holders.is_empty() => Recovery::Run(holders.iter().fold(
                Call::new(&["down"]).value("workspace", workspace),
                |call, holder| call.value("processes", holder),
            )),
            Self::PortTaken {
                workspace, port, ..
            } => Recovery::Run(
                Call::new(&["ports", "reset"])
                    .value("workspace", workspace)
                    .value("ports", port),
            ),
            Self::UnknownPort { workspace, .. } => {
                Recovery::Run(Call::new(&["show"]).value("names", workspace))
            }
        }
    }

    fn fields(&self) -> Vec<Field<'_>> {
        match self {
            Self::NotCurrent => Vec::new(),
            Self::Running {
                workspace,
                processes,
            } => vec![
                Field::new("workspace", workspace),
                Field::new("processes", FieldValue::list(processes.iter())),
            ],
            Self::Volumes {
                workspace, volumes, ..
            } => vec![
                Field::new("workspace", workspace),
                Field::new("volumes", FieldValue::list(volumes.iter())),
            ],
            Self::VolumeRenamed { .. }
            | Self::Unrunnable { .. }
            | Self::BuildFailed { .. }
            | Self::BuildUnstartable { .. }
            | Self::BuildTimedOut { .. } => self.transition_fields(),
            Self::ManifestMissing {
                checkout,
                workspace,
            } => vec![
                Field::new("path", shown(&checkout.join(MANIFEST_FILE))),
                Field::new("workspace_path", shown(workspace)),
            ],
            Self::Manifest(error) => manifest_fields(error),
            Self::UnknownProcess { workspace, process } => vec![
                Field::new("workspace", workspace),
                Field::new("process", process),
            ],
            Self::Exited {
                workspace,
                process,
                code,
                signal,
                log,
            } => vec![
                Field::new("workspace", workspace),
                Field::new("process", process),
                Field::new("code", *code),
                Field::new("signal", *signal),
                Field::new("log", log.as_deref().map(shown)),
            ],
            Self::NotReady {
                workspace,
                process,
                waited,
                seconds,
                log,
            } => {
                let mut fields = vec![
                    Field::new("workspace", workspace),
                    Field::new("process", process),
                ];
                match &**waited {
                    Waited::Port { port, number } => {
                        fields.push(Field::new("port", port));
                        fields.push(Field::new("number", *number));
                    }
                    Waited::Log { pattern } => fields.push(Field::new("pattern", pattern)),
                    _ => {}
                }
                fields.extend([
                    Field::new("timeout_seconds", *seconds),
                    Field::new("log", log.as_deref().map(shown)),
                ]);
                fields
            }
            Self::ForeignListener {
                workspace,
                process,
                port,
                number,
                pid,
                log,
            } => vec![
                Field::new("workspace", workspace),
                Field::new("process", process),
                Field::new("port", port),
                Field::new("number", *number),
                Field::new("pid", *pid),
                Field::new("log", log.as_deref().map(shown)),
            ],
            Self::DetachedListener {
                workspace,
                process,
                port,
                number,
                pid,
                log,
            } => vec![
                Field::new("workspace", workspace),
                Field::new("process", process),
                Field::new("port", port),
                Field::new("number", *number),
                Field::new("pid", *pid),
                Field::new("log", log.as_deref().map(shown)),
            ],
            Self::Unstoppable {
                process,
                pid,
                not_started,
                ..
            } => {
                let mut fields = vec![Field::new("process", process), Field::new("pid", *pid)];
                if !not_started.is_empty() {
                    fields.push(Field::new(
                        "not_started",
                        FieldValue::list(not_started.iter()),
                    ));
                }
                fields
            }
            Self::PortTaken {
                workspace,
                process,
                port,
                number,
                holders,
            } => vec![
                Field::new("workspace", workspace),
                Field::new("process", process),
                Field::new("port", port),
                Field::new("number", *number),
                Field::new("holders", FieldValue::list(holders.iter())),
            ],
            Self::UnknownPort { workspace, port } => {
                vec![Field::new("workspace", workspace), Field::new("port", port)]
            }
        }
    }
}

/// Returns the values at fault in a manifest that cannot be used.
fn manifest_fields(error: &ManifestError) -> Vec<Field<'_>> {
    let mut fields = vec![Field::new("path", shown(&error.path))];
    if let Some(line) = error.line {
        fields.push(Field::new("line", line));
    }
    match &error.problem {
        ManifestProblem::UnknownKey { key } => fields.push(Field::new("key", key)),
        ManifestProblem::Reserved { section } => fields.push(Field::new("section", section)),
        ManifestProblem::Newer { version } => fields.push(Field::new("version", *version)),
        ManifestProblem::UnitName { name } => fields.push(Field::new("unit", name)),
        ManifestProblem::Invalid { message }
        | ManifestProblem::Unreadable { message }
        | ManifestProblem::Unrunnable { message } => {
            fields.push(Field::new("message", message));
        }
        ManifestProblem::NameClash { name, other } => {
            fields.push(Field::new("name", name));
            fields.push(Field::new("other", shown(other)));
        }
        ManifestProblem::ServiceClash { name, message } => {
            fields.push(Field::new("name", name));
            fields.push(Field::new("message", message));
        }
        _ => {}
    }
    fields
}

impl fmt::Display for RunFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotCurrent => formatter.write_str(
                "the command names no workspace, and the current directory is in no workspace",
            ),
            Self::Running {
                workspace,
                processes,
            } => write!(
                formatter,
                "the workspace {workspace} has running units: {}",
                processes.join(", ")
            ),
            Self::Volumes {
                workspace, volumes, ..
            } => write!(
                formatter,
                "the workspace {workspace} has docker volumes, which may hold data: {}",
                volumes.join(", ")
            ),
            Self::VolumeRenamed { .. }
            | Self::Unrunnable { .. }
            | Self::BuildFailed { .. }
            | Self::BuildUnstartable { .. }
            | Self::BuildTimedOut { .. } => self.fmt_transition(formatter),
            Self::ManifestMissing {
                checkout,
                workspace,
            } => write!(
                formatter,
                "the workspace has no manifest: neither {} nor {} exists",
                shown(workspace),
                shown(&checkout.join(MANIFEST_FILE))
            ),
            Self::Manifest(error) => error.fmt(formatter),
            Self::UnknownProcess { workspace, process } => write!(
                formatter,
                "the workspace {workspace} has no unit named {process}"
            ),
            Self::Exited {
                process,
                code,
                signal,
                ..
            } => match (code, signal) {
                (Some(code), _) => write!(
                    formatter,
                    "the unit {process} exited with code {code} before it was ready"
                ),
                (None, Some(signal)) => write!(
                    formatter,
                    "signal {signal} ended the unit {process} before it was ready"
                ),
                (None, None) => write!(
                    formatter,
                    "a signal ended the unit {process} before it was ready"
                ),
            },
            Self::NotReady {
                process,
                waited,
                seconds,
                ..
            } => match &**waited {
                Waited::Log { pattern } => write!(
                    formatter,
                    "the unit {process} wrote no log line that matches `{pattern}` within {seconds} seconds, so mxv stopped it"
                ),
                Waited::Port { port, number } => write!(
                    formatter,
                    "the unit {process} did not accept a connection on {port} (port {number}) within {seconds} seconds, so mxv stopped it"
                ),
                _ => write!(
                    formatter,
                    "the unit {process} was not ready within {seconds} seconds, so mxv stopped it"
                ),
            },
            Self::ForeignListener {
                process,
                port,
                number,
                pid: Some(pid),
                ..
            } => write!(
                formatter,
                "a connection to {port} (port {number}) succeeded, and the process {pid}, which is not part of {process}, listens on it, so mxv stopped {process}"
            ),
            Self::ForeignListener {
                process,
                port,
                number,
                pid: None,
                ..
            } => write!(
                formatter,
                "a connection to {port} (port {number}) succeeded, and no process of {process}'s group listens on it, so another program holds the port and mxv stopped {process}"
            ),
            Self::DetachedListener {
                process,
                port,
                number,
                pid,
                ..
            } => write!(
                formatter,
                "the process {pid}, which {process} started, listens on {port} (port {number}) and left {process}'s process group, so `mxv down` could not stop it; mxv stopped both"
            ),
            Self::Unstoppable {
                process,
                pid,
                unverified,
                not_started,
            } => fmt_unstoppable(formatter, process, *pid, *unverified, not_started),
            Self::PortTaken {
                workspace,
                process,
                port,
                number,
                holders,
            } if holders.is_empty() => write!(
                formatter,
                "port {number}, which the workspace {workspace} holds for {port}, is taken by a program outside the workspace, so {process} did not start"
            ),
            Self::PortTaken {
                workspace,
                process,
                port,
                number,
                holders,
            } => write!(
                formatter,
                "port {number}, which the workspace {workspace} holds for {port}, is taken, and the process groups of its own units {} still run and may hold it, so {process} did not start",
                holders.join(", ")
            ),
            Self::UnknownPort { workspace, port } => write!(
                formatter,
                "the manifest of the workspace {workspace} declares no port named {port}"
            ),
        }
    }
}

impl RunFailure {
    /// Returns the fields of a failure that phase's running of the
    /// workspace's apps added.
    fn transition_fields(&self) -> Vec<Field<'_>> {
        match self {
            Self::VolumeRenamed { workspace, volumes } => vec![
                Field::new("workspace", workspace),
                Field::new(
                    "volumes",
                    FieldValue::list(volumes.iter().map(|volume| &volume.volume)),
                ),
                Field::new(
                    "old_volumes",
                    FieldValue::list(volumes.iter().map(|volume| &volume.old)),
                ),
                Field::new(
                    "new_volumes",
                    FieldValue::list(volumes.iter().map(|volume| &volume.new)),
                ),
            ],
            Self::Unrunnable {
                workspace,
                unit,
                message,
            } => vec![
                Field::new("workspace", workspace),
                Field::new("unit", unit.as_deref()),
                Field::new("message", message),
            ],
            Self::BuildFailed {
                workspace,
                process,
                code,
                signal,
            } => {
                let mut fields = vec![
                    Field::new("workspace", workspace),
                    Field::new("process", process),
                ];
                if let Some(code) = code {
                    fields.push(Field::new("code", i64::from(*code)));
                }
                if let Some(signal) = signal {
                    fields.push(Field::new("signal", i64::from(*signal)));
                }
                fields
            }
            Self::BuildUnstartable {
                workspace,
                process,
                reason,
            } => vec![
                Field::new("workspace", workspace),
                Field::new("process", process),
                Field::new("reason", reason),
            ],
            Self::BuildTimedOut {
                workspace,
                process,
                seconds,
            } => vec![
                Field::new("workspace", workspace),
                Field::new("process", process),
                Field::new("seconds", *seconds),
            ],
            _ => Vec::new(),
        }
    }

    /// Writes the message of a failure that phase's running of the
    /// workspace's apps added.
    fn fmt_transition(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VolumeRenamed { workspace, volumes } => write!(
                formatter,
                "adding a checkout to the workspace {workspace} prefixes the names of its containers' volumes, and these hold data under the old names: {}; the containers would start on empty volumes",
                volumes
                    .iter()
                    .map(|volume| format!(
                        "{} in {}, which becomes {}",
                        volume.volume, volume.old, volume.new
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Unrunnable {
                unit: Some(unit),
                message,
                ..
            } => write!(formatter, "phase cannot run the unit {unit}: {message}"),
            Self::Unrunnable {
                unit: None,
                message,
                ..
            } => write!(formatter, "phase cannot run the manifest: {message}"),
            Self::BuildFailed {
                process,
                code,
                signal,
                ..
            } => match (code, signal) {
                (Some(code), _) => write!(
                    formatter,
                    "the build of {process} exited with status {code}, so mxv stopped and started nothing"
                ),
                (None, Some(signal)) => write!(
                    formatter,
                    "signal {signal} ended the build of {process}, so mxv stopped and started nothing"
                ),
                (None, None) => write!(
                    formatter,
                    "the build of {process} ended without a status, so mxv stopped and started nothing"
                ),
            },
            Self::BuildUnstartable {
                process, reason, ..
            } => write!(
                formatter,
                "the build of {process} could not start, so mxv stopped and started nothing: {reason}"
            ),
            Self::BuildTimedOut {
                process, seconds, ..
            } => write!(
                formatter,
                "the build of {process} was still running after {seconds} seconds, so mxv ended it and stopped and started nothing"
            ),
            _ => Ok(()),
        }
    }
}

impl std::error::Error for RunFailure {}
