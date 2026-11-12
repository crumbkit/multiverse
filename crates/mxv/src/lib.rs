//! The `mxv` command, which creates, archives and restores task workspaces
//! that span several git repositories.
//!
//! The library holds the whole command, and the `mxv` binary calls [`run`].
//! It exports the parser, the tool record and the failure codes, so the
//! integration tests can pass them to crumbkit-test's checks beside the
//! built binary. It is not an API for other crates, so each public item is
//! hidden from the documentation and outside the semver promise.

mod admission;
mod archive;
mod clean;
mod cli;
mod config;
mod environment;
mod exec;
mod failures;
mod processes;
mod reclaim;
mod rows;
mod run;
mod run_failures;
#[cfg(test)]
mod testing;
mod units;
mod verbs;

use std::ffi::OsString;
use std::process::ExitCode;

use clap::CommandFactory as _;
use crumbkit::Code;

#[doc(hidden)]
pub use crate::archive::{
    ARCHIVE_CHECKOUT_UNUSABLE, ARCHIVE_IN_USE, ARCHIVE_MANIFEST_DISPOSABLE, ARCHIVE_MIRROR_MISSING,
    ARCHIVE_NAME_TAKEN, ARCHIVE_NOT_FOUND, ARCHIVE_RECORD_TOO_LARGE, ARCHIVE_SOURCE_CHANGED,
    ARCHIVE_UNFINISHED, ARCHIVE_UNKEEPABLE, ARCHIVE_VOLUMES_IN_USE, RESTORE_BRANCH_MOVED,
    RESTORE_COMMIT_MISSING, RESTORE_IGNORED_DAMAGED, RESTORE_IGNORED_TAKEN, RESTORE_NAME_TAKEN,
    RESTORE_NOT_FOUND, RESTORE_ROOT_FILE_CHANGED, RESTORE_UNFINISHED,
};
#[doc(hidden)]
pub use crate::cli::{Cli, TOOL};
use crate::cli::{
    CommandsCommand, CompletionCommand, FiguresCommand, PortsCommand, RunsCommand, SKILL,
    SchemasCommand, SelfCommand, SkillsCommand, Verb,
};
#[doc(hidden)]
pub use crate::environment::{Bases, Environment, VARIABLES, Variable};
#[doc(hidden)]
pub use crate::exec::{EXEC_SANDBOXED_TERMINAL, EXEC_UNSTARTABLE, PROCESS_NOT_RUNNING};
#[doc(hidden)]
pub use crate::failures::{
    BRANCH_EXISTS, CHECKOUT_INCOMPLETE, CHECKOUT_INVALID_NAME, CHECKOUT_LOCKED,
    CHECKOUT_NAME_TAKEN, CHECKOUT_NOT_FOUND, CHECKOUT_ORIGIN_UNREACHABLE, CHECKOUT_RESUME_BLOCKED,
    CHECKOUT_STASHES, CHECKOUT_UNOWNED, CHECKOUT_UNPUSHED_COMMITS, CHECKOUT_UNREACHABLE_COMMITS,
    CONFIG_INVALID, CONFIG_NO_LIMIT, CONFIG_UNREADABLE, DIRECTORY_TAKEN, ENVIRONMENT_NO_DIRECTORY,
    ENVIRONMENT_RELATIVE_ROOT, FILE_UNWRITABLE, GIT_FAILED, GIT_NOT_FOUND, GIT_TIMED_OUT,
    MANIFEST_NEEDED, MANIFEST_UNTRUSTED, MIRROR_FETCH_FAILED, MIRROR_LOCKED, PATH_INACCESSIBLE,
    RECORD_CORRUPT, RECORD_MISMATCHED, RECORD_NEWER, RECORD_UNREADABLE, REPOSITORY_EMPTY_ORIGIN,
    REPOSITORY_NO_DEFAULT_BRANCH, REPOSITORY_NO_ORIGIN, REPOSITORY_NOT_FOUND, TEMPLATE_AMBIGUOUS,
    TEMPLATE_INVALID, WORKSPACE_BAD_NAME, WORKSPACE_CHANGED, WORKSPACE_DIRTY, WORKSPACE_EXISTS,
    WORKSPACE_IGNORED_FILES, WORKSPACE_NOT_FOUND, WORKSPACE_WORKTREE_KIND,
};
#[doc(hidden)]
pub use crate::failures::{
    DOCKER_ADDRESS_POOLS_EXHAUSTED, DOCKER_COMPOSE_MISSING, DOCKER_FAILED, DOCKER_FOREIGN_OBJECT,
    DOCKER_NOT_FOUND, DOCKER_UNAVAILABLE, PHASE_FAILED, PORT_EXHAUSTED, PROCESS_TABLE_UNREADABLE,
    PROCESS_UNSTARTABLE, PROCESS_UNSUPPORTED_OS, WORKSPACE_BUSY,
};
#[doc(hidden)]
pub use crate::run_failures::{
    BUILD_FAILED, BUILD_TIMED_OUT, BUILD_UNSTARTABLE, MANIFEST_INVALID, MANIFEST_NAME_CLASH,
    MANIFEST_NEWER, MANIFEST_NOT_FOUND, MANIFEST_RESERVED, MANIFEST_UNIT_NAME,
    MANIFEST_UNKNOWN_KEY, MANIFEST_UNREADABLE, MANIFEST_UNRUNNABLE, PERMISSIONS_NOT_GRANTED,
    PORT_NOT_FOUND, PORT_TAKEN, PROCESS_DETACHED_LISTENER, PROCESS_EXITED,
    PROCESS_FOREIGN_LISTENER, PROCESS_NOT_FOUND, PROCESS_NOT_READY, PROCESS_STILL_NEEDED,
    PROCESS_UNSTOPPABLE, TASK_AMBIGUOUS, TASK_UNKNOWN, VOLUME_RENAMED, WORKSPACE_NOT_CURRENT,
    WORKSPACE_RUNNING, WORKSPACE_VOLUMES,
};

/// Every failure code of `mxv archive`, `mxv restore` and `mxv remove
/// --archived`, which the tests pass to crumbkit-test's codes, recovery and
/// twin checks beside [`CODES`].
#[doc(hidden)]
pub const ARCHIVE_CODES: &[&Code] = archive::CODES;

/// Every failure code of the workspace verbs, which the tests pass to
/// crumbkit-test's codes, recovery and twin checks.
#[doc(hidden)]
pub const CODES: &[&Code] = failures::CODES;

/// Every failure code of the process verbs, which the tests pass to
/// crumbkit-test's codes, recovery and twin checks beside [`CODES`].
#[doc(hidden)]
pub const RUN_CODES: &[&Code] = run_failures::CODES;

/// Every failure code of `mxv exec` that the other process verbs do not
/// have, which the tests pass to crumbkit-test's codes, recovery and twin
/// checks beside [`RUN_CODES`].
#[doc(hidden)]
pub const EXEC_CODES: &[&Code] = exec::CODES;

/// Parses `args`, runs the command with the variables of `environment` and
/// returns the process's exit code. The first argument is the program's
/// name.
///
/// `crumbkit::parse_from` prints help or a refused command line and returns
/// the exit code. `crumbkit::finish` prints the result or the failure in
/// the format that the output flags choose, and returns the exit code.
#[doc(hidden)]
#[must_use]
pub fn run<I, T>(args: I, environment: &Environment) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let (cli, mut output) = match crumbkit::parse_from::<Cli>(&TOOL, args) {
        Ok(parsed) => parsed,
        Err(stop) => return stop.exit_code(),
    };
    match cli.verb {
        Verb::New(args) => {
            let result = verbs::new(&output, environment, &args);
            crumbkit::finish(output, verbs::NEW, result)
        }
        Verb::Add(args) => {
            let result = verbs::add(&output, environment, &args);
            crumbkit::finish(output, verbs::ADD, result)
        }
        Verb::List(args) => crumbkit::finish(output, verbs::LIST, verbs::list(environment, &args)),
        Verb::Show(args) => crumbkit::finish(output, verbs::SHOW, verbs::show(environment, &args)),
        Verb::Cd(args) => crumbkit::finish(output, verbs::CD, verbs::cd(environment, &args)),
        // `remove` reads `--dry-run` and asks for confirmation through
        // `&mut output`, so it runs before `finish`.
        Verb::Remove(args) => {
            let result = verbs::remove(&mut output, environment, &args);
            crumbkit::finish(output, verbs::REMOVE, result)
        }
        // `clean` reads `--dry-run` and asks for confirmation through
        // `&mut output`, so it runs before `finish`.
        Verb::Clean(args) => {
            let result = clean::clean(&mut output, environment, &args);
            crumbkit::finish(output, clean::CLEAN, result)
        }
        // `reclaim` reads `--dry-run` through `&mut output`, so it runs
        // before `finish`.
        Verb::Reclaim(args) => {
            let result = reclaim::reclaim(&mut output, environment, &args);
            crumbkit::finish(output, reclaim::RECLAIM, result)
        }
        Verb::Pin(args) => {
            let result = reclaim::pin(environment, &args, true);
            crumbkit::finish(output, reclaim::PIN, result)
        }
        Verb::Unpin(args) => {
            let result = reclaim::pin(environment, &args, false);
            crumbkit::finish(output, reclaim::UNPIN, result)
        }
        // `up` and `restart` read `--no-input` through `&output` before they
        // ask for wider permissions, so each runs before `finish`.
        Verb::Up(args) => {
            let result = processes::up(&output, environment, &args);
            processes::finish_interruptible(output, processes::UP, result)
        }
        Verb::Down(args) => {
            crumbkit::finish(output, processes::DOWN, processes::down(environment, &args))
        }
        Verb::Restart(args) => {
            let result = processes::restart(&output, environment, &args);
            processes::finish_interruptible(output, processes::RESTART, result)
        }
        Verb::Status(args) => {
            let result = processes::status(environment, &args);
            crumbkit::finish(output, processes::STATUS, result)
        }
        // `logs` prints a stream, so it opens the stream from `output`
        // itself.
        Verb::Logs(args) => processes::logs(output, environment, &args),
        // `exec` hands the terminal to the command and exits with its exit
        // code, so it prints no document.
        Verb::Exec(args) => exec::exec(output, environment, &args),
        // `run` hands the terminal to a task and exits with its exit code,
        // or prints the list of tasks, so it finishes the output itself.
        Verb::Run(args) => run::run(output, environment, &args),
        Verb::Ports {
            command: PortsCommand::Reset(args),
        } => {
            let result = processes::ports_reset(environment, &args);
            crumbkit::finish(output, processes::PORTS_RESET, result)
        }
        // `archive` and `restore` read `--dry-run` and ask for confirmation
        // through `&mut output`, so each runs before `finish`.
        Verb::Archive(args) => {
            let result = archive::archive(&mut output, environment, &args);
            crumbkit::finish(output, archive::ARCHIVE, result)
        }
        Verb::Restore(args) => {
            let result = archive::restore(&mut output, environment, &args);
            crumbkit::finish(output, archive::RESTORE, result)
        }
        // crumbkit's own commands read the command tree through
        // `&mut output`, so each runs before `finish` takes `output`.
        Verb::Commands {
            command: CommandsCommand::List(args),
        } => {
            let result = crumbkit::commands::list(&mut output, &TOOL, &args);
            crumbkit::finish(output, crumbkit::commands::COMMANDS_LIST, result)
        }
        Verb::Schemas { command } => match command {
            SchemasCommand::List(args) => {
                let result = crumbkit::schemas::list(&mut output, &TOOL, &args);
                crumbkit::finish(output, crumbkit::schemas::SCHEMAS_LIST, result)
            }
            SchemasCommand::Show(args) => {
                let result = crumbkit::schemas::show(&mut output, &TOOL, &args);
                crumbkit::finish(output, crumbkit::schemas::SCHEMAS_SHOW, result)
            }
        },
        Verb::SelfCommand { command } => match command {
            SelfCommand::Usage(args) => {
                let result =
                    Ok::<_, crumbkit::Error>(crumbkit_usage::findings(&mut output, &TOOL, &args));
                crumbkit::finish(output, crumbkit_usage::SELF_USAGE, result)
            }
            SelfCommand::Figures {
                command: FiguresCommand::List(args),
            } => {
                let result =
                    Ok::<_, crumbkit::Error>(crumbkit_usage::figures(&mut output, &TOOL, &args));
                crumbkit::finish(output, crumbkit_usage::SELF_FIGURES_LIST, result)
            }
            SelfCommand::Runs {
                command: RunsCommand::List(args),
            } => {
                let result =
                    Ok::<_, crumbkit::Error>(crumbkit_usage::runs(&mut output, &TOOL, &args));
                crumbkit::finish(output, crumbkit_usage::SELF_RUNS_LIST, result)
            }
        },
        Verb::Skills { command } => match command {
            SkillsCommand::Install(args) => {
                let result = Ok::<_, crumbkit::Error>(crumbkit_skills::install(&SKILL, &args));
                crumbkit::finish(output, crumbkit_skills::SKILLS_INSTALL, result)
            }
            SkillsCommand::Check(args) => {
                let result = Ok::<_, crumbkit::Error>(crumbkit_skills::check(&SKILL, &args));
                crumbkit::finish(output, crumbkit_skills::SKILLS_CHECK, result)
            }
            SkillsCommand::Print => {
                let result = Ok::<_, crumbkit::Error>(crumbkit_skills::print(&SKILL));
                crumbkit::finish(output, crumbkit_skills::SKILLS_PRINT, result)
            }
        },
        Verb::Completion { command } => match command {
            CompletionCommand::Install(args) => {
                let result = crumbkit::completion::install(Cli::command, &args);
                crumbkit::finish(output, crumbkit::completion::COMPLETION_INSTALL, result)
            }
            CompletionCommand::Check(args) => {
                let result = crumbkit::completion::check(Cli::command, &args);
                crumbkit::finish(output, crumbkit::completion::COMPLETION_CHECK, result)
            }
            CompletionCommand::Print(args) => {
                let result = crumbkit::completion::print(Cli::command, &args);
                crumbkit::finish(output, crumbkit::completion::COMPLETION_PRINT, result)
            }
        },
    }
}

/// The words that follow the program on the command line of phase's
/// keeper, which `run` never sees: `mxv phase keep ...`. The command is
/// hidden from help, because only phase starts it.
#[doc(hidden)]
pub const KEEPER_WORDS: [&str; 2] = ["phase", "keep"];

/// Reports whether `args`, the whole command line, starts phase's keeper.
#[doc(hidden)]
#[must_use]
pub fn is_keeper(args: &[OsString]) -> bool {
    args.get(1..3).is_some_and(|words| {
        words
            .iter()
            .zip(KEEPER_WORDS)
            .all(|(word, keeper)| word == keeper)
    })
}

/// The words that follow the program on the command line of the app
/// `mxv-provision`, which creates the resources of a workspace's services:
/// `mxv phase provision <spec>`. The command is hidden from help, because
/// only phase starts it, as a oneshot of the workspace's plan.
#[doc(hidden)]
pub const PROVISION_WORDS: [&str; 2] = ["phase", "provision"];

/// How long the provisioning retries a backend that is still starting.
/// phase stops the oneshot at its own timeout first.
const PROVISION_DEADLINE: std::time::Duration = std::time::Duration::from_secs(110);

/// Reports whether `args`, the whole command line, starts the
/// provisioning of a workspace's services.
#[doc(hidden)]
#[must_use]
pub fn is_provisioner(args: &[OsString]) -> bool {
    args.get(1..3).is_some_and(|words| {
        words
            .iter()
            .zip(PROVISION_WORDS)
            .all(|(word, provision)| word == provision)
    })
}

/// Creates the resources of the spec that follows `mxv phase provision`,
/// writes one line for each resource to standard output, which phase
/// keeps in the app's log, and returns 0 once every resource exists. It
/// writes the failure to standard error and returns 1 otherwise. It reads
/// no environment variable.
#[doc(hidden)]
#[must_use]
pub fn provision(args: &[OsString]) -> ExitCode {
    use std::io::Write as _;
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/mxv/src/lib.rs owns standard output, and the provisioning writes its log there"
    )]
    let mut stdout = std::io::stdout().lock();
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/mxv/src/lib.rs owns standard error, and the provisioning writes its failure there"
    )]
    let mut stderr = std::io::stderr();
    let words = args.get(1 + PROVISION_WORDS.len()..).unwrap_or_default();
    let result = match words {
        [spec] => multiverse::ProvisionSpec::parse(&spec.to_string_lossy()).and_then(|spec| {
            multiverse::provision(
                &spec,
                std::time::Instant::now() + PROVISION_DEADLINE,
                &mut |line| drop(writeln!(stdout, "mxv-provision: {line}")),
            )
        }),
        _ => {
            drop(writeln!(
                stderr,
                "mxv-provision: the command takes one argument, the spec of the resources"
            ));
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => {
            drop(writeln!(stdout, "mxv-provision: every resource exists"));
            ExitCode::SUCCESS
        }
        Err(error) => {
            drop(writeln!(stderr, "mxv-provision: {error}"));
            ExitCode::FAILURE
        }
    }
}

/// The environment source of a keeper, which mxv's plans never ask for:
/// each process of an mxv plan is never restarted, so its keeper never
/// resolves a fresh environment.
struct NoFreshEnvironment;

impl multiverse::phase::EnvironmentSource for NoFreshEnvironment {
    fn resolve(
        &self,
        app: &multiverse::phase::AppName,
        process: &multiverse::phase::ProcessName,
        _deadline: std::time::Instant,
    ) -> Result<Box<dyn multiverse::phase::AppliedEnvironment>, multiverse::phase::EnvironmentError>
    {
        Err(multiverse::phase::EnvironmentError::new(
            format!("{app}/{process}"),
            "mxv".to_owned(),
            "mxv never restarts a process, so its keeper resolves no fresh environment".into(),
        ))
    }
}

/// Runs phase's keeper with the arguments after `mxv phase keep` and the
/// environment `vars` that `main` read, and returns its exit status. The
/// keeper reads its specification from standard input and writes its
/// handshake to standard output. When the arguments start with
/// `--sandboxed`, it runs phase's launcher instead, which applies a
/// sandboxed app's sandbox and replaces this process with the app.
#[doc(hidden)]
#[must_use]
pub fn keep(args: Vec<OsString>, vars: Vec<(OsString, OsString)>) -> ExitCode {
    use multiverse::phase::keeper::{KeeperArgs, KeeperEnvironment, KeeperExit, launch, run};
    let rest: Vec<OsString> = args.into_iter().skip(1 + KEEPER_WORDS.len()).collect();
    // A keeper starts a sandboxed app through `mxv phase keep --sandboxed`,
    // which applies the sandbox and becomes the app. It reads nothing else.
    if let Some(code) = launch(&rest) {
        return code;
    }
    let environment = KeeperEnvironment::from_vars(vars);
    let Ok(parsed) = KeeperArgs::parse(rest) else {
        return KeeperExit::BadInput.into();
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/mxv/src/lib.rs owns standard input, and phase's keeper reads its specification there"
    )]
    let input = std::io::stdin().lock();
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/mxv/src/lib.rs owns standard output, and phase's keeper writes its handshake there"
    )]
    let output = std::io::stdout().lock();
    run(&parsed, environment, &NoFreshEnvironment, input, output).into()
}

/// Reports whether mxv's standard input is a terminal, which `mxv exec`
/// passes to phase so a container's command gets a terminal of its own.
pub(crate) fn stdin_is_terminal() -> bool {
    use std::io::IsTerminal as _;
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/mxv/src/lib.rs owns standard input, and `mxv exec` asks only whether it is a terminal"
    )]
    let stdin = std::io::stdin();
    stdin.is_terminal()
}

/// Reports whether mxv's standard output is a terminal, which `mxv exec`
/// checks with standard input before it gives a container's command a
/// terminal.
pub(crate) fn stdout_is_terminal() -> bool {
    use std::io::IsTerminal as _;
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/mxv/src/lib.rs owns standard output, and `mxv exec` asks only whether it is a terminal"
    )]
    let stdout = std::io::stdout();
    stdout.is_terminal()
}

/// Reports whether a person can answer a question: mxv's standard input
/// and standard error are both terminals. `mxv up` and `mxv restart` ask
/// for wider permissions only then.
pub(crate) fn person_can_answer() -> bool {
    use std::io::IsTerminal as _;
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/mxv/src/lib.rs owns standard error, and `mxv up` asks only whether it is a terminal"
    )]
    let stderr = std::io::stderr();
    stdin_is_terminal() && stderr.is_terminal()
}

/// Returns a reader of mxv's standard input, where a person answers the
/// question of `mxv up` about wider permissions.
pub(crate) fn stdin_reader() -> std::io::StdinLock<'static> {
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/mxv/src/lib.rs owns standard input, where a person answers the question of `mxv up`"
    )]
    let stdin = std::io::stdin();
    stdin.lock()
}

/// Returns a writer to mxv's standard error, which receives a build's
/// output from the pipe that the build writes to.
pub(crate) fn stderr_writer() -> Box<dyn std::io::Write + Send> {
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/mxv/src/lib.rs owns standard error, and a build's output goes there beside the command's result"
    )]
    let stderr = std::io::stderr();
    Box::new(stderr)
}

/// Prints `line`, which deadband wrote about a build's wait for a place,
/// on standard error.
pub(crate) fn notice(line: &str) {
    use std::io::Write as _;
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/mxv/src/lib.rs owns standard error, and this line goes there beside the command's result"
    )]
    let mut stderr = std::io::stderr();
    // A line that cannot be written changes nothing the command does, so
    // its failure is ignored.
    drop(writeln!(stderr, "mxv: {line}."));
}

/// Prints each of `warnings` on standard error as a line of its own, for
/// a command such as `mxv up` whose result has no field for them.
pub(crate) fn warn(warnings: &[String]) {
    use std::io::Write as _;
    if warnings.is_empty() {
        return;
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/mxv/src/lib.rs owns standard error, and a warning goes there beside the command's result"
    )]
    let mut stderr = std::io::stderr();
    for warning in warnings {
        // A warning that cannot be written changes nothing the command
        // does, so its failure is ignored.
        drop(writeln!(stderr, "warning: {warning}."));
    }
}

/// Tells the person at a terminal that `mxv new` waits up to `wait` for
/// another mxv command to release the lock of `mirror`. `mxv new` passes
/// it to the library only when crumbkit's `Output::progress_allowed` is
/// true, so a pipe, a file and `--json` receive no progress line.
pub(crate) fn mirror_wait_notice(mirror: &std::path::Path, wait: std::time::Duration) {
    use std::io::Write as _;
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/mxv/src/lib.rs owns standard error, and this progress line is drawn only when crumbkit allows progress"
    )]
    let mut stderr = std::io::stderr();
    // A progress line that cannot be written changes nothing the command
    // does, so its failure is ignored.
    drop(writeln!(
        stderr,
        "mxv is waiting up to {} seconds for another mxv command to finish with the mirror {}.",
        wait.as_secs(),
        mirror.display()
    ));
}
