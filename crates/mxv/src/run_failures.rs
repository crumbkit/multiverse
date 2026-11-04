//! The failures of the process verbs `up`, `down`, `restart`, `status` and
//! `logs`, and of `remove` while processes run, each with a stable code and
//! a next step (CLI-3).

use std::fmt;
use std::path::{Path, PathBuf};

use crumbkit::{Call, Code, Failure, Field, FieldValue, Recovery};
use multiverse::{Allow, MANIFEST_FILE, ManifestError, ManifestProblem};

use crate::failures::remove_volumes_line;

/// The code of a process verb that names no workspace, run outside every
/// workspace.
pub const WORKSPACE_NOT_CURRENT: Code = Code::new("mxv", "workspace", "not_current");
/// The code of a removal that running processes stop.
pub const WORKSPACE_RUNNING: Code = Code::new("mxv", "workspace", "running");
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
/// The code of a process name that the workspace does not know.
pub const PROCESS_NOT_FOUND: Code = Code::new("mxv", "process", "not_found");
/// The code of a process that exited before it was ready.
pub const PROCESS_EXITED: Code = Code::new("mxv", "process", "exited");
/// The code of a process that missed its readiness timeout.
pub const PROCESS_NOT_READY: Code = Code::new("mxv", "process", "not_ready");
/// The code of a process group that outlived SIGKILL.
pub const PROCESS_UNSTOPPABLE: Code = Code::new("mxv", "process", "unstoppable");
/// The code of a stored port that something outside the workspace holds.
pub const PORT_TAKEN: Code = Code::new("mxv", "port", "taken");
/// The code of a port name that the manifest does not declare.
pub const PORT_NOT_FOUND: Code = Code::new("mxv", "port", "not_found");

/// Every code that a [`RunFailure`] returns.
pub(crate) const CODES: &[&Code] = &[
    &WORKSPACE_NOT_CURRENT,
    &WORKSPACE_RUNNING,
    &WORKSPACE_VOLUMES,
    &MANIFEST_NOT_FOUND,
    &MANIFEST_UNKNOWN_KEY,
    &MANIFEST_NEWER,
    &MANIFEST_RESERVED,
    &MANIFEST_INVALID,
    &MANIFEST_UNREADABLE,
    &PROCESS_NOT_FOUND,
    &PROCESS_EXITED,
    &PROCESS_NOT_READY,
    &PROCESS_UNSTOPPABLE,
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
        /// Its log.
        log: Box<Path>,
        /// The last lines of its log.
        tail: LogTail,
    },
    /// A process missed its readiness timeout.
    NotReady {
        /// The workspace's name.
        workspace: String,
        /// The process.
        process: String,
        /// The readiness port's name.
        port: String,
        /// The host port.
        number: u16,
        /// The timeout in seconds.
        seconds: u64,
        /// Its log.
        log: Box<Path>,
        /// The last lines of its log.
        tail: LogTail,
    },
    /// A process group outlived SIGKILL.
    Unstoppable {
        /// The process.
        process: String,
        /// Its process group's id.
        pid: u32,
    },
    /// The workspace has running processes, so `remove` refuses.
    Running {
        /// The workspace's name.
        workspace: String,
        /// The running processes.
        processes: Vec<String>,
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

/// Returns `path` as text for a field or a hint.
fn shown(path: &Path) -> String {
    path.display().to_string()
}

/// Returns the hint `text` as a recovery.
fn hint(text: impl Into<String>) -> Recovery {
    Recovery::Hint(text.into())
}

impl Failure for RunFailure {
    fn code(&self) -> &'static Code {
        match self {
            Self::NotCurrent => &WORKSPACE_NOT_CURRENT,
            Self::Running { .. } => &WORKSPACE_RUNNING,
            Self::Volumes { .. } => &WORKSPACE_VOLUMES,
            Self::ManifestMissing { .. } => &MANIFEST_NOT_FOUND,
            Self::Manifest(error) => match error.problem {
                ManifestProblem::UnknownKey { .. } => &MANIFEST_UNKNOWN_KEY,
                ManifestProblem::Newer { .. } => &MANIFEST_NEWER,
                ManifestProblem::Reserved { .. } => &MANIFEST_RESERVED,
                ManifestProblem::Unreadable { .. } => &MANIFEST_UNREADABLE,
                // The library marks the problem non-exhaustive, so a
                // problem that a later version adds reads as invalid until
                // mxv gives it a code of its own.
                _ => &MANIFEST_INVALID,
            },
            Self::UnknownProcess { .. } => &PROCESS_NOT_FOUND,
            Self::Exited { .. } => &PROCESS_EXITED,
            Self::NotReady { .. } => &PROCESS_NOT_READY,
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
            Self::Volumes {
                workspace, allow, ..
            } => hint(format!(
                "copy out any data you need from the volumes, then run `{}`, which deletes them",
                remove_volumes_line(workspace, *allow)
            )),
            Self::ManifestMissing { checkout } => hint(format!(
                "write an {MANIFEST_FILE} that declares the app's processes at the root of {}",
                shown(checkout)
            )),
            Self::Manifest(error) => match &error.problem {
                ManifestProblem::UnknownKey { key } => hint(format!(
                    "remove or rename `{key}` in {}, because the manifest schema does not declare it",
                    shown(&error.path)
                )),
                ManifestProblem::Newer { .. } => {
                    hint("upgrade mxv to a version that reads this manifest version")
                }
                ManifestProblem::Reserved { section } => hint(format!(
                    "remove the section `{section}` from {}, because this version of mxv does not run it",
                    shown(&error.path)
                )),
                ManifestProblem::Unreadable { .. } => {
                    hint(format!("make {} readable", shown(&error.path)))
                }
                _ if error.line.is_some() => hint(format!(
                    "fix the line of {} that the message names",
                    shown(&error.path)
                )),
                _ => hint(format!(
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
            } => hint(format!(
                "fix the cause that the log names, and run `mxv up -w {workspace} {process}` again; `mxv logs -w {workspace} {process}` prints the log"
            )),
            Self::Unstoppable { pid, .. } => hint(format!(
                "find the process group {pid} with `ps -g {pid}` and stop it by hand"
            )),
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
            Self::ManifestMissing { checkout } => {
                vec![Field::new("path", shown(&checkout.join(MANIFEST_FILE)))]
            }
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
                tail,
            } => vec![
                Field::new("workspace", workspace),
                Field::new("process", process),
                Field::new("code", *code),
                Field::new("signal", *signal),
                Field::new("log", shown(log)),
                Field::new("tail", FieldValue::list(tail.lines.iter())),
            ],
            Self::NotReady {
                workspace,
                process,
                port,
                number,
                seconds,
                log,
                tail,
            } => vec![
                Field::new("workspace", workspace),
                Field::new("process", process),
                Field::new("port", port),
                Field::new("number", *number),
                Field::new("timeout_seconds", *seconds),
                Field::new("log", shown(log)),
                Field::new("tail", FieldValue::list(tail.lines.iter())),
            ],
            Self::Unstoppable { process, pid } => {
                vec![Field::new("process", process), Field::new("pid", *pid)]
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
        ManifestProblem::Invalid { message } | ManifestProblem::Unreadable { message } => {
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
                "the workspace {workspace} has running processes: {}",
                processes.join(", ")
            ),
            Self::Volumes {
                workspace, volumes, ..
            } => write!(
                formatter,
                "the workspace {workspace} has docker volumes, which may hold data: {}",
                volumes.join(", ")
            ),
            Self::ManifestMissing { checkout } => write!(
                formatter,
                "the checkout {} has no {MANIFEST_FILE}",
                shown(checkout)
            ),
            Self::Manifest(error) => error.fmt(formatter),
            Self::UnknownProcess { workspace, process } => write!(
                formatter,
                "the workspace {workspace} has no process named {process}"
            ),
            Self::Exited {
                process,
                code,
                signal,
                ..
            } => match (code, signal) {
                (Some(code), _) => write!(
                    formatter,
                    "the process {process} exited with code {code} before it was ready"
                ),
                (None, Some(signal)) => write!(
                    formatter,
                    "signal {signal} ended the process {process} before it was ready"
                ),
                (None, None) => write!(
                    formatter,
                    "a signal ended the process {process} before it was ready"
                ),
            },
            Self::NotReady {
                process,
                port,
                number,
                seconds,
                ..
            } => write!(
                formatter,
                "the process {process} did not accept a connection on {port} (port {number}) within {seconds} seconds, so mxv stopped it"
            ),
            Self::Unstoppable { process, pid } => write!(
                formatter,
                "the process group {pid} of {process} still runs after SIGKILL"
            ),
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
                "port {number}, which the workspace {workspace} holds for {port}, is taken, and the process groups of its own processes {} still run and may hold it, so {process} did not start",
                holders.join(", ")
            ),
            Self::UnknownPort { workspace, port } => write!(
                formatter,
                "the manifest of the workspace {workspace} declares no port named {port}"
            ),
        }
    }
}

impl std::error::Error for RunFailure {
    /// The last lines of the log of a process that exited or missed its
    /// timeout, one cause for each line, so the text form prints each on a
    /// `cause:` line under the message.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Exited { tail, .. } | Self::NotReady { tail, .. } => tail
                .first
                .as_deref()
                .map(|line| line as &(dyn std::error::Error + 'static)),
            _ => None,
        }
    }
}

/// The last lines of a process's log, as a failure reports them.
#[derive(Debug)]
pub(crate) struct LogTail {
    /// The lines, oldest first.
    lines: Box<[String]>,
    /// The same lines as a chain of errors, the first of which the failure
    /// gives as its source.
    first: Option<Box<TailLine>>,
}

impl LogTail {
    /// Returns the tail of `lines`, oldest first.
    pub(crate) fn new(lines: Vec<String>) -> Self {
        let first = lines.iter().rev().fold(None, |next, line| {
            Some(Box::new(TailLine {
                line: line.clone(),
                next,
            }))
        });
        Self {
            lines: lines.into_boxed_slice(),
            first,
        }
    }
}

/// One line of a log tail, whose source is the next line.
#[derive(Debug)]
struct TailLine {
    /// The line.
    line: String,
    /// The next line, or `None` after the last.
    next: Option<Box<TailLine>>,
}

impl fmt::Display for TailLine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.line)
    }
}

impl std::error::Error for TailLine {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.next
            .as_deref()
            .map(|line| line as &(dyn std::error::Error + 'static))
    }
}
