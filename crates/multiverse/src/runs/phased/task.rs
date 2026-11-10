//! `mxv run`: lists a checkout's mise tasks, and runs one task under one
//! place from the machine's job limiter.
//!
//! [`Runs::prepare_task`] adds the task to the workspace's plan under the
//! reserved app [`TASK_APP`](crate::phase_plan::TASK_APP), while the caller
//! holds the workspace's lock. [`PreparedTask::run`] then runs it after the
//! lock is released, so a long test run never stops `mxv up` or `mxv down`
//! of the workspace.
//!
//! A task has no sandbox and no timer, and it holds the caller's standard
//! streams, as `mxv exec` gives them to a command.
//!
//! mxv never trusts a mise config on the person's behalf. It gives mise no
//! `MISE_YES` and no `MISE_TRUSTED_CONFIG_PATHS`, and its standard input is
//! empty, so mise cannot ask. A config that mise refuses leaves its tasks
//! unknown to mxv, and mise's message reaches the person.
//!
//! These are the limits that this design accepts. Each says what it misses
//! and what would replace it.
//!
//! - `mise tasks ls --json` also lists the person's global tasks and those
//!   of parent directories, so a global task of a workspace with several
//!   checkouts needs `--repo`, because it is found in each of them. Reading
//!   each entry's `global` field would replace this, and mxv reads only
//!   `name`.
//! - A task shares the caller's process group, so a Ctrl-C at a terminal
//!   reaches the task directly, and mxv sets no cancel flag, as phase's
//!   `ExecIo::Inherit` asks. A signal sent to mxv alone, such as `kill -INT`
//!   of mxv, therefore does not reach the task, and mxv waits for the task
//!   to end, as `mxv exec` does. Running a task in a process group of its
//!   own would let mxv end it, and would take the terminal from it.
//! - A process that a mise config starts in the background and that keeps
//!   mise's pipes open outlives the listing. mxv stops waiting at
//!   `LIST_WAIT` and treats the listing as refused, and the thread that
//!   reads the pipe lasts until the pipe closes or mxv ends. mxv does not
//!   end that process, because it did not start it.
//! - A `SIGTERM` or `SIGKILL` of mxv frees the place while the task runs on,
//!   uncounted by the limiter, as for a build.

use std::collections::BTreeMap;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use phase::{AppName, Cancel, ExecIo, ExecRequest, PhaseError, ProcessName, WorkspaceId};
use serde::Deserialize;
use wait_timeout::ChildExt as _;

use super::exec::EXEC_WAIT;
use crate::error::Error;
use crate::manifest::Manifest;
use crate::name::WorkspaceName;
use crate::phase_plan::{self, Built, BuiltTask, TaskEntry, Unrunnable};
use crate::record::Workspace;
use crate::runs::{Admission, Asked, Runs};
use crate::service::Service;
use crate::store::{Lock, Store};

/// The program that lists and runs a checkout's mise tasks.
const MISE: &str = "mise";

/// How long mise may take to list a checkout's tasks and close its pipes.
/// When it passes, mxv ends mise and treats the listing as refused.
const LIST_WAIT: Duration = Duration::from_secs(30);

/// The most bytes of mise's listing that mxv reads. A longer listing does
/// not parse and counts as refused.
const MOST_LISTING_BYTES: u64 = 4 * 1024 * 1024;

/// The most bytes of mise's refusal that mxv keeps.
const MOST_MESSAGE_BYTES: u64 = 4 * 1024;

/// The name of a mise task, which may hold `:`, as `lint:fix` does. It is
/// one that mise listed, or one that a person typed to compare with those.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MiseTaskName(String);

impl MiseTaskName {
    /// Returns the name `text`, or `None` when it is empty, holds a space
    /// or a control character, or starts with `-`, which `mise run` would
    /// read as a flag.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        let usable = !text.is_empty()
            && !text.starts_with('-')
            && !text
                .chars()
                .any(|character| character.is_whitespace() || character.is_control());
        usable.then(|| Self(text.to_owned()))
    }

    /// Returns the name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MiseTaskName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// What `mise tasks ls --json` answered in a checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MiseListing {
    /// mise listed these tasks.
    Listed(Vec<MiseTaskName>),
    /// mise is not installed, or the checkout's directory is gone.
    Missing,
    /// mise refused, or answered something that mxv cannot read.
    Refused {
        /// What mise wrote, or why mxv could not read its answer.
        message: String,
    },
}

/// One entry of mise's listing. mxv reads only the name, and ignores every
/// other field.
#[derive(Deserialize)]
struct Listed {
    /// The task's name.
    name: String,
}

/// Returns the listing that mise answered with the exit status `status`,
/// standard output `stdout` and standard error `stderr`.
fn listing(status: ExitStatus, stdout: &[u8], stderr: &[u8]) -> MiseListing {
    if !status.success() {
        return MiseListing::Refused {
            message: String::from_utf8_lossy(stderr).trim().to_owned(),
        };
    }
    match serde_json::from_slice::<Vec<Listed>>(stdout) {
        Ok(listed) => MiseListing::Listed(
            listed
                .iter()
                .filter_map(|entry| MiseTaskName::new(&entry.name))
                .collect(),
        ),
        Err(error) => MiseListing::Refused {
            message: format!("mxv cannot read what `mise tasks ls --json` printed: {error}"),
        },
    }
}

/// Reads `stream` on a thread of its own: the first `most` bytes into a
/// buffer, and the rest into nothing, so mise never blocks on a full pipe.
/// The buffer arrives on the returned channel once the pipe closes.
fn drain<R: io::Read + Send + 'static>(mut stream: R, most: u64) -> Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut kept = Vec::new();
        drop((&mut stream).take(most).read_to_end(&mut kept));
        drop(io::copy(&mut stream, &mut io::sink()));
        drop(sender.send(kept));
    });
    receiver
}

/// Returns what `command`, a `mise tasks ls --json`, answers within `wait`,
/// which covers both its exit and the closing of its pipes.
fn listing_within(command: &mut std::process::Command, wait: Duration) -> MiseListing {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/runs/phased/task.rs starts mise to list a checkout's tasks, and owns its deadline and cleanup (PROC-1)"
    )]
    let spawned = command.spawn();
    #[expect(
        clippy::disallowed_types,
        reason = "crates/multiverse/src/runs/phased/task.rs holds mise until its deadline (PROC-1)"
    )]
    let mut child: std::process::Child = match spawned {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return MiseListing::Missing,
        Err(error) => {
            return MiseListing::Refused {
                message: format!("mxv cannot start mise: {error}"),
            };
        }
    };
    let deadline = Instant::now() + wait;
    let stdout = child
        .stdout
        .take()
        .map(|stream| drain(stream, MOST_LISTING_BYTES));
    let stderr = child
        .stderr
        .take()
        .map(|stream| drain(stream, MOST_MESSAGE_BYTES));
    let late = MiseListing::Refused {
        message: format!(
            "mise did not list the tasks within {} seconds",
            wait.as_secs()
        ),
    };
    let received = |stream: Option<Receiver<Vec<u8>>>| match stream {
        Some(stream) => stream
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .ok(),
        None => Some(Vec::new()),
    };
    match child.wait_timeout(wait) {
        Ok(Some(status)) => match (received(stdout), received(stderr)) {
            (Some(stdout), Some(stderr)) => listing(status, &stdout, &stderr),
            _ => late,
        },
        Ok(None) | Err(_) => {
            // The child is killed and reaped whatever the wait reported,
            // so no zombie remains.
            drop(child.kill());
            drop(child.wait());
            late
        }
    }
}

/// Which services' variables a task receives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskServices {
    /// A declared task receives those of the services that it needs.
    Needs(Vec<String>),
    /// A mise task names no services, so it receives those of every service
    /// that the manifest declares.
    All,
}

/// A task that [`Runs::prepare_task`] prepares.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct TaskStart {
    /// The directory that `dir` is relative to, an absolute path.
    pub root: PathBuf,
    /// The directory that the task runs in, relative to `root`.
    pub dir: PathBuf,
    /// The task's program and arguments.
    pub argv: Vec<String>,
    /// The services whose variables the task receives.
    pub services: TaskServices,
    /// The ports of the task's own checkout whose names differ in the
    /// merged manifest, as [`TaskHome::own_ports`](crate::TaskHome) holds
    /// them.
    pub own_ports: BTreeMap<String, String>,
}

impl TaskStart {
    /// Returns the task that runs `argv` in the directory `dir` of `root`
    /// with the variables of `services`.
    #[must_use]
    pub const fn new(
        root: PathBuf,
        dir: PathBuf,
        argv: Vec<String>,
        services: TaskServices,
    ) -> Self {
        Self {
            root,
            dir,
            argv,
            services,
            own_ports: BTreeMap::new(),
        }
    }

    /// Returns this task with the ports `own_ports` of its own checkout.
    #[must_use]
    pub fn with_own_ports(mut self, own_ports: BTreeMap<String, String>) -> Self {
        self.own_ports = own_ports;
        self
    }

    /// Returns the task that runs the mise task `name` with `args` at the
    /// top of the checkout `checkout`: `mise run <name> [args]`.
    #[must_use]
    pub fn mise(checkout: PathBuf, name: &MiseTaskName, args: &[String]) -> Self {
        let mut argv = vec![MISE.to_owned(), "run".to_owned(), name.as_str().to_owned()];
        argv.extend_from_slice(args);
        Self::new(checkout, PathBuf::from("."), argv, TaskServices::All)
    }
}

/// How a task that [`PreparedTask::run`] ran, or tried to run, ended.
#[derive(Debug)]
pub enum TaskEnd {
    /// The task ran and ended with this status.
    Exited(ExitStatus),
    /// The task could not start, such as when its program does not exist.
    Unstartable {
        /// Why, in phase's words, which hold no variable's value.
        reason: String,
    },
}

/// A task in the workspace's plan, ready to run.
pub struct PreparedTask<'a> {
    /// The run files and the phase that run the task.
    runs: &'a Runs,
    /// The workspace's stable id.
    id: WorkspaceId,
    /// The plan that holds the task, and its variables.
    built: Built,
    /// The app that holds the task.
    app: AppName,
    /// The process that runs the task.
    process: ProcessName,
    /// The task's program and arguments.
    argv: Vec<String>,
    /// The directory that the task runs in.
    directory: PathBuf,
    /// The workspace's name, which the limiter shows for the job as its
    /// label and as its workspace.
    workspace: WorkspaceName,
}

impl std::fmt::Debug for PreparedTask<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedTask")
            .field("id", &self.id)
            .field("argv", &self.argv)
            .field("directory", &self.directory)
            .finish_non_exhaustive()
    }
}

impl PreparedTask<'_> {
    /// Waits for a place from `admission`, runs the task with the caller's
    /// standard streams and the place's variables, ends the place once, and
    /// returns how the task ended. `terminal` is what `mxv exec` passes for
    /// a command. It sets no deadline on the task.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Phase`] for a failure of phase other than a task
    /// that cannot start, and [`Error::Busy`] when another command holds
    /// phase's lock for longer than `exec` waits.
    pub fn run(self, admission: &mut dyn Admission, terminal: bool) -> Result<TaskEnd, Error> {
        let cancel = Cancel::new();
        let place = admission.admit(&Asked {
            cwd: &self.directory,
            label: self.workspace.as_str(),
            workspace: &self.workspace,
            command: &self.argv.join(" "),
            cancel: &cancel,
        });
        let request = ExecRequest::new(
            self.app,
            self.process,
            self.argv,
            ExecIo::Inherit { terminal },
        )
        .with_variables(place.variables());
        let ended = self.runs.phase.exec(
            &self.built.plan,
            request,
            &self.built.environment,
            Instant::now() + EXEC_WAIT,
            &cancel,
        );
        match ended {
            Ok(status) => {
                place.end(status.code());
                Ok(TaskEnd::Exited(status))
            }
            Err(PhaseError::ExecUnstartable { reason, .. }) => {
                place.end(None);
                Ok(TaskEnd::Unstartable { reason })
            }
            Err(error) => {
                place.end(None);
                Err(self.runs.phase_error(&self.id, error))
            }
        }
    }
}

impl Runs {
    /// Asks mise for the tasks of `checkout`, with the variables that a
    /// task receives from mxv's own environment and no terminal.
    #[must_use]
    pub fn mise_tasks(&self, checkout: &Path) -> MiseListing {
        #[expect(
            clippy::disallowed_methods,
            reason = "crates/multiverse/src/runs/phased/task.rs starts mise to list a checkout's tasks, and owns its deadline and cleanup (PROC-1)"
        )]
        let mut command = std::process::Command::new(MISE);
        command
            .args(["tasks", "ls", "--json"])
            .current_dir(checkout)
            .env_clear()
            .envs(self.caller.inherited())
            .envs(
                self.caller
                    .passed()
                    .map(|(name, value)| (name.as_str(), value.reveal())),
            );
        listing_within(&mut command, LIST_WAIT)
    }

    /// Adds `task` to the plan of `workspace`'s `manifest`, after giving
    /// the workspace its ports as `up` does. The caller holds the
    /// workspace's lock until this returns, and may release it before it
    /// runs the task.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Store::update`] and [`Runs::status`], and
    /// [`Error::NoFreePort`] when no port can be allocated. A plan that
    /// phase cannot run returns `Ok(Err(_))`.
    pub fn prepare_task(
        &self,
        _lock: &Lock,
        store: &Store,
        workspace: &mut Workspace,
        manifest: &Manifest,
        task: TaskStart,
    ) -> Result<Result<PreparedTask<'_>, Unrunnable>, Error> {
        // `prepared` also builds the plan of the units alone. That plan is
        // not used, and its failure would be the same one as below.
        let (id, _) = self.prepared(store, workspace, manifest)?;
        let services: Vec<&Service> = match &task.services {
            TaskServices::All => manifest.services.values().collect(),
            TaskServices::Needs(names) => names
                .iter()
                .filter_map(|name| manifest.services.get(name))
                .collect(),
        };
        let entry = TaskEntry {
            root: &task.root,
            dir: &task.dir,
            argv: task.argv.clone(),
            services,
            own_ports: &task.own_ports,
        };
        let inputs = self.inputs(workspace, &id, manifest);
        let BuiltTask {
            built,
            app,
            process,
        } = match phase_plan::build_with_task(&inputs, &entry) {
            Ok(built) => built,
            Err(unrunnable) => return Ok(Err(unrunnable)),
        };
        Ok(Ok(PreparedTask {
            runs: self,
            id,
            built,
            app,
            process,
            directory: task.root.join(&task.dir),
            argv: task.argv,
            workspace: workspace.name.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::ExitStatus;
    use std::time::{Duration, Instant};

    use super::{MiseListing, MiseTaskName, listing, listing_within};

    fn passed() -> ExitStatus {
        ExitStatus::from_raw(0)
    }

    fn name(text: &str) -> MiseTaskName {
        MiseTaskName::new(text).unwrap()
    }

    #[test]
    fn a_listing_is_read_for_its_names_alone() {
        let answer = br#"[{"name":"lint:fix","run":[1],"aliases":{"odd":true}},{"name":"test"}]"#;
        assert_eq!(
            listing(passed(), answer, b""),
            MiseListing::Listed(vec![name("lint:fix"), name("test")])
        );
    }

    #[test]
    fn a_listed_name_that_mise_would_read_as_a_flag_is_left_out() {
        let answer = br#"[{"name":"--version"},{"name":"test"}]"#;
        assert_eq!(
            listing(passed(), answer, b""),
            MiseListing::Listed(vec![name("test")])
        );
    }

    #[test]
    fn a_failing_listing_keeps_what_mise_wrote() {
        assert_eq!(
            listing(ExitStatus::from_raw(256), b"", b"not trusted\n"),
            MiseListing::Refused {
                message: "not trusted".to_owned()
            }
        );
    }

    #[test]
    fn a_listing_that_is_not_the_expected_json_counts_as_refused() {
        assert!(matches!(
            listing(passed(), b"{}", b""),
            MiseListing::Refused { .. }
        ));
    }

    /// Returns a command that runs the shell script `script`.
    #[expect(
        clippy::disallowed_methods,
        reason = "the test stands a shell script in for mise"
    )]
    fn script(script: &str) -> std::process::Command {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn a_listing_within_the_wait_is_read() {
        assert_eq!(
            listing_within(
                &mut script("echo '[{\"name\":\"test\"}]'"),
                Duration::from_secs(30)
            ),
            MiseListing::Listed(vec![name("test")])
        );
    }

    #[test]
    fn the_wait_covers_a_background_process_that_keeps_the_pipes_open() {
        let begun = Instant::now();
        let answer = listing_within(
            &mut script("sleep 5 & echo '[]'"),
            Duration::from_millis(200),
        );
        assert!(matches!(answer, MiseListing::Refused { .. }), "{answer:?}");
        assert!(begun.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn a_mise_name_may_hold_a_colon_and_never_starts_with_a_dash() {
        assert!(MiseTaskName::new("lint:fix").is_some());
        assert!(MiseTaskName::new("-j").is_none());
        assert!(MiseTaskName::new("").is_none());
        assert!(MiseTaskName::new("a b").is_none());
    }
}
