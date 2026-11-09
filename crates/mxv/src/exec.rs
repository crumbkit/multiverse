//! `mxv exec`, which runs a one-off command in the context of a running
//! process or container of a workspace, as `docker compose exec` does for
//! a compose service, and its failures.
//!
//! A host process's command runs in the process's directory with its
//! variables and its sandbox. A container's command runs inside the
//! container through `docker exec`. mxv exits with the command's exit
//! code, or with 128 plus the number of the signal that ended it.

use std::fmt;
use std::os::unix::process::ExitStatusExt as _;
use std::process::{ExitCode, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crumbkit::{Call, Code, Failure, Field, Output, Recovery, Sink};
use multiverse::phase::Cancel;
use multiverse::{Exec, ExecEnd, ExecTarget};

use crate::cli::ExecArgs;
use crate::environment::Environment;
use crate::failures::MxvFailure;
use crate::processes;
use crate::run_failures::RunFailure;

#[cfg(test)]
mod tests;

/// The code of a command for a process or container that does not run.
pub const PROCESS_NOT_RUNNING: Code = Code::new("mxv", "process", "not_running");
/// The code of a command that could not start in a process's context.
pub const EXEC_UNSTARTABLE: Code = Code::new("mxv", "exec", "unstartable");
/// The code of a command for a sandboxed process whose standard input,
/// output or error is a terminal.
pub const EXEC_SANDBOXED_TERMINAL: Code = Code::new("mxv", "exec", "sandboxed_terminal");

/// Every code that an [`ExecFailure`] returns.
pub(crate) const CODES: &[&Code] = &[
    &PROCESS_NOT_RUNNING,
    &EXEC_UNSTARTABLE,
    &EXEC_SANDBOXED_TERMINAL,
];

/// Why `--json` and `--fields` are refused, which ends each refusal's
/// message.
const TEXT_ONLY: &str = "mxv exec hands the terminal to the command";

/// A failure of `mxv exec` that the other process verbs do not have.
#[derive(Debug)]
pub(crate) enum ExecFailure {
    /// The process or container does not run, so no command runs in its
    /// context.
    NotRunning {
        /// The workspace's name.
        workspace: String,
        /// The process or container.
        process: String,
    },
    /// The command could not start in the process's context.
    Unstartable {
        /// The workspace's name.
        workspace: String,
        /// The process or container.
        process: String,
        /// Why, in phase's words.
        reason: String,
    },
    /// The process runs under the sandbox, and the command's standard
    /// input, output or error is a terminal.
    SandboxedTerminal {
        /// The workspace's name.
        workspace: String,
        /// The process.
        process: String,
    },
    /// The caller passed `--json`, and the command's output is the
    /// command's own.
    JsonRefused,
}

impl Failure for ExecFailure {
    fn code(&self) -> &'static Code {
        match self {
            Self::NotRunning { .. } => &PROCESS_NOT_RUNNING,
            Self::Unstartable { .. } => &EXEC_UNSTARTABLE,
            Self::SandboxedTerminal { .. } => &EXEC_SANDBOXED_TERMINAL,
            Self::JsonRefused => &crumbkit::codes::CLI_USAGE,
        }
    }

    fn recovery(&self) -> Recovery {
        match self {
            Self::NotRunning { workspace, process } => Recovery::Run(
                Call::new(&["up"])
                    .value("workspace", workspace)
                    .value("processes", process),
            ),
            Self::Unstartable { process, .. } => Recovery::Hint(format!(
                "check that the command's program exists on the PATH of {process}, or give its full path, and run the command again"
            )),
            Self::SandboxedTerminal { workspace, process } => Recovery::Hint(format!(
                "run the command with no terminal on its standard input, output and error, such as `mxv exec -w {workspace} {process} -- COMMAND </dev/null 2>&1 | cat`, or run it in the context of a process whose manifest gives it no permissions table"
            )),
            Self::JsonRefused => {
                Recovery::Hint("run the command without --json to read its output".to_owned())
            }
        }
    }

    fn fields(&self) -> Vec<Field<'_>> {
        match self {
            Self::NotRunning { workspace, process }
            | Self::SandboxedTerminal { workspace, process } => vec![
                Field::new("workspace", workspace),
                Field::new("process", process),
            ],
            Self::Unstartable {
                workspace,
                process,
                reason,
            } => vec![
                Field::new("workspace", workspace),
                Field::new("process", process),
                Field::new("reason", reason),
            ],
            Self::JsonRefused => vec![Field::new("argument", "--json")],
        }
    }
}

impl fmt::Display for ExecFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRunning { workspace, process } => write!(
                formatter,
                "{process} of the workspace {workspace} is not running, so mxv runs no command in its context"
            ),
            Self::Unstartable {
                process, reason, ..
            } => write!(
                formatter,
                "the command could not start in the context of {process}: {reason}"
            ),
            Self::SandboxedTerminal { process, .. } => write!(
                formatter,
                "{process} runs under the sandbox, and a command in its context cannot have the terminal, so mxv refuses a command whose standard input, output or error is a terminal"
            ),
            Self::JsonRefused => write!(formatter, "--json is not available, because {TEXT_ONLY}"),
        }
    }
}

impl std::error::Error for ExecFailure {}

/// Returns the exit number that mxv ends with after a command that ended
/// with `status`: 128 plus the number of the signal that ended it, as a
/// shell reports it, and otherwise its exit code.
pub(crate) fn exit_number(status: ExitStatus) -> u8 {
    if let Some(signal) = status.signal() {
        return u8::try_from(128 + signal).unwrap_or(u8::MAX);
    }
    status
        .code()
        .map_or(1, |code| u8::try_from(code).unwrap_or(1))
}

/// Returns the failure of a command whose run in the context of `process`
/// of `workspace` ended with `end` without an exit status, or the status.
pub(crate) fn ended(
    workspace: &str,
    process: &str,
    end: ExecEnd,
) -> Result<ExitStatus, ExecFailure> {
    let workspace = workspace.to_owned();
    let process = process.to_owned();
    match end {
        ExecEnd::Exited(status) => Ok(status),
        ExecEnd::NotRunning => Err(ExecFailure::NotRunning { workspace, process }),
        ExecEnd::Unstartable { reason } => Err(ExecFailure::Unstartable {
            workspace,
            process,
            reason,
        }),
        ExecEnd::NeedsTerminal => Err(ExecFailure::SandboxedTerminal { workspace, process }),
    }
}

/// Runs the command that `args` names in the context of its process, and
/// returns mxv's exit code: the command's own, or that of mxv's failure.
pub(crate) fn exec(mut output: Output, environment: &Environment, args: &ExecArgs) -> ExitCode {
    let mut sink = Sink::standard();
    if output.is_json() {
        return fail(&mut sink, &mut output, &ExecFailure::JsonRefused);
    }
    if let Some(code) = output.refuse_fields_for_text(&mut sink, TEXT_ONLY) {
        return code;
    }
    match run(environment, args) {
        Ok((status, caught)) => {
            // Only a signal that mxv caught counts as an interruption, so a
            // command that crashed with a signal of its own does not.
            if caught {
                crumbkit::events::interrupted();
            }
            crumbkit::events::ended(exit_number(status), None)
        }
        Err(error) => fail(&mut sink, &mut output, error.failure()),
    }
}

/// Prints `failure` as `finish` prints a failure, reports its exit code to
/// the observer, and returns it.
fn fail<O: std::io::Write, W: std::io::Write>(
    sink: &mut Sink<O, W>,
    output: &mut Output,
    failure: &dyn Failure,
) -> ExitCode {
    output.print_failure(sink, failure);
    crumbkit::events::ended(output.failure_exit(failure.code()), None)
}

/// Finds the process that `args` names and runs the command in its
/// context, and returns the command's exit status and whether mxv caught
/// `SIGINT`, `SIGHUP` or `SIGQUIT` while it ran.
fn run(environment: &Environment, args: &ExecArgs) -> Result<(ExitStatus, bool), crumbkit::Error> {
    let mut context = processes::context(environment, args.workspace.as_ref(), &[])?;
    // `up`, `down` and `restart` hold mxv's lock for their whole run. exec
    // takes it and lets it go at once, so it waits for them here, where
    // Ctrl-C still ends mxv, and builds the plan from the ports that they
    // stored.
    drop(context.lock()?);
    let manifest = processes::required_manifest(&context.workspace)?;
    let workspace = context.workspace.name.to_string();
    let target = context
        .runs
        .exec_target(&context.workspace, &manifest, &args.process)
        .map_err(MxvFailure::from)?;
    let exec = match target {
        ExecTarget::Ready(exec) => exec,
        ExecTarget::UnknownProcess => {
            return Err(RunFailure::UnknownProcess {
                workspace,
                process: args.process.clone(),
            }
            .into());
        }
        ExecTarget::NotRunning => {
            return Err(ExecFailure::NotRunning {
                workspace,
                process: args.process.clone(),
            }
            .into());
        }
        ExecTarget::Unrunnable(unrunnable) => {
            return Err(RunFailure::Unrunnable {
                workspace,
                unit: unrunnable.unit,
                message: unrunnable.message,
            }
            .into());
        }
    };
    // A person who runs a command in a workspace is using it, so mxv
    // touches it while the command runs.
    let _toucher = context
        .runs
        .keep_touched(&context.workspace.name, processes::TOUCH_EVERY);
    let (end, caught) = hand_over(exec, args.command.clone()).map_err(MxvFailure::from)?;
    Ok((ended(&workspace, &args.process, end)?, caught))
}

/// Runs `command` in the context that `exec` holds, with mxv's standard
/// streams, and handles Ctrl-C while it runs.
///
/// mxv tells crumbkit that a child holds the terminal, so the usage log's
/// signal thread no longer ends mxv on `SIGINT`, and catches `SIGINT`,
/// `SIGHUP` and `SIGQUIT` itself, except a signal that
/// [`interrupts`](crate::admission::interrupts) leaves alone because it
/// started out ignored, which stays ignored in the command. A caught signal
/// resets to its default action in the command. An unsandboxed command shares mxv's process group, so the terminal's
/// signals reach it directly, and mxv waits for it to end. A sandboxed
/// command runs in a session of its own, which the terminal's signals do
/// not reach, so mxv sets the cancel flag and phase stops the command's
/// process group.
///
/// The usage log's thread still ends mxv on `SIGTERM`, and nothing passes
/// that signal on, so the command outlives an mxv that something sent
/// `SIGTERM` alone. A `SIGTERM` rule in crumbkit's usage log like its
/// `SIGINT` rule would close this.
///
/// A container's command gets a terminal only when mxv's standard input
/// and output are both terminals, so piped output carries no terminal
/// line endings.
///
/// Returns how the command ended and whether mxv caught one of the
/// signals while it ran.
fn hand_over(exec: Exec<'_>, command: Vec<String>) -> Result<(ExecEnd, bool), multiverse::Error> {
    let cancel = Cancel::new();
    // mxv's handlers go in before crumbkit stops ending mxv on `SIGINT`,
    // so no signal falls between the two.
    let caught = catch_interrupts(exec.sandboxed().then(|| cancel.clone()));
    crumbkit::events::handed_over();
    let terminal = crate::stdin_is_terminal() && crate::stdout_is_terminal();
    let end = exec.run(command, terminal, &cancel)?;
    Ok((end, caught.load(Ordering::SeqCst)))
}

/// Catches the signals of [`interrupts`](crate::admission::interrupts) for
/// the rest of mxv's run, and sets `cancel` on each one when it is given. Returns a flag that
/// each caught signal sets inside its handler, so the flag is set before
/// mxv sees the command end. When the handlers cannot be installed, no
/// signal cancels a sandboxed command, and the flag stays clear.
fn catch_interrupts(cancel: Option<Cancel>) -> Arc<AtomicBool> {
    use signal_hook::iterator::Signals;
    let caught = Arc::new(AtomicBool::new(false));
    let watched = crate::admission::interrupts();
    let Ok(mut signals) = Signals::new(&watched) else {
        return caught;
    };
    for signal in watched {
        // A flag that cannot be registered leaves the run counted as
        // uninterrupted in the usage log, and changes nothing else.
        drop(signal_hook::flag::register(signal, Arc::clone(&caught)));
    }
    // The thread ends with mxv, which ends once the command has.
    drop(
        std::thread::Builder::new()
            .name("mxv-exec-interrupts".to_owned())
            .spawn(move || {
                for _ in signals.forever() {
                    if let Some(cancel) = &cancel {
                        cancel.cancel();
                    }
                }
            }),
    );
    caught
}
