//! The `mxv` command, which creates, archives and restores task workspaces
//! that span several git repositories.
//!
//! The library holds the whole command, and the `mxv` binary calls [`run`].
//! It exports the parser, the guide and the failure codes, so the
//! integration tests can pass them to crumbkit-test's checks beside the
//! built binary. It is not an API for other crates, so each public item is
//! hidden from the documentation and outside the semver promise.
//!
//! `new`, `add`, `list`, `show`, `cd`, `remove`, `clean` and the process verbs `up`,
//! `down`, `restart`, `status` and `logs` are built. `archive` and
//! `restore` are declared and fail with `mxv::command::unbuilt` until the
//! pull request that builds each one.

mod clean;
mod cli;
mod config;
mod environment;
mod failures;
mod processes;
mod rows;
mod run_failures;
#[cfg(test)]
mod testing;
mod unbuilt;
mod units;
mod verbs;

use std::ffi::OsString;
use std::process::ExitCode;

use clap::CommandFactory as _;
use crumbkit::Code;

#[doc(hidden)]
pub use crate::cli::{Cli, GUIDE};
use crate::cli::{
    CommandsCommand, FiguresCommand, PortsCommand, SchemasCommand, SelfCommand, Verb,
};
#[doc(hidden)]
pub use crate::environment::{Bases, Environment, VARIABLES, Variable};
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
    MANIFEST_INVALID, MANIFEST_NAME_CLASH, MANIFEST_NEWER, MANIFEST_NOT_FOUND, MANIFEST_RESERVED,
    MANIFEST_UNIT_NAME, MANIFEST_UNKNOWN_KEY, MANIFEST_UNREADABLE, MANIFEST_UNRUNNABLE,
    PORT_NOT_FOUND, PORT_TAKEN, PROCESS_DETACHED_LISTENER, PROCESS_EXITED,
    PROCESS_FOREIGN_LISTENER, PROCESS_NOT_FOUND, PROCESS_NOT_READY, PROCESS_UNSTOPPABLE,
    VOLUME_NOT_MOVED, VOLUME_RENAMED, WORKSPACE_LEGACY_RUNNING, WORKSPACE_NOT_CURRENT,
    WORKSPACE_RUNNING, WORKSPACE_VOLUMES,
};
#[doc(hidden)]
pub use crate::unbuilt::UNBUILT;
use crate::unbuilt::{UNBUILT_RESULT, fail};

/// The failure code of the unbuilt verbs, which the tests pass to
/// crumbkit-test's codes, recovery and twin checks beside [`CODES`].
#[doc(hidden)]
pub const UNBUILT_CODES: &[&Code] = &[&UNBUILT];

/// Every failure code of the workspace verbs, which the tests pass to
/// crumbkit-test's codes, recovery and twin checks.
#[doc(hidden)]
pub const CODES: &[&Code] = failures::CODES;

/// Every failure code of the process verbs, which the tests pass to
/// crumbkit-test's codes, recovery and twin checks beside [`CODES`].
#[doc(hidden)]
pub const RUN_CODES: &[&Code] = run_failures::CODES;

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
    let (cli, mut output) = match crumbkit::parse_from::<Cli, _, _>(Cli::command(), &GUIDE, args) {
        Ok(parsed) => parsed,
        Err(stop) => return stop.exit_code(),
    };
    let verb = match cli.verb {
        Verb::New(args) => {
            let result = verbs::new(&output, environment, &args);
            return crumbkit::finish(output, verbs::NEW, result);
        }
        Verb::Add(args) => {
            let result = verbs::add(&output, environment, &args);
            return crumbkit::finish(output, verbs::ADD, result);
        }
        Verb::List(args) => {
            return crumbkit::finish(output, verbs::LIST, verbs::list(environment, &args));
        }
        Verb::Show(args) => {
            return crumbkit::finish(output, verbs::SHOW, verbs::show(environment, &args));
        }
        Verb::Cd(args) => {
            return crumbkit::finish(output, verbs::CD, verbs::cd(environment, &args));
        }
        // `remove` reads `--dry-run` and asks for confirmation through
        // `&mut output`, so it runs before `finish`.
        Verb::Remove(args) => {
            let result = verbs::remove(&mut output, environment, &args);
            return crumbkit::finish(output, verbs::REMOVE, result);
        }
        // `clean` reads `--dry-run` and asks for confirmation through
        // `&mut output`, so it runs before `finish`.
        Verb::Clean(args) => {
            let result = clean::clean(&mut output, environment, &args);
            return crumbkit::finish(output, clean::CLEAN, result);
        }
        Verb::Up(args) => {
            return crumbkit::finish(output, processes::UP, processes::up(environment, &args));
        }
        Verb::Down(args) => {
            return crumbkit::finish(output, processes::DOWN, processes::down(environment, &args));
        }
        Verb::Restart(args) => {
            let result = processes::restart(environment, &args);
            return crumbkit::finish(output, processes::RESTART, result);
        }
        Verb::Status(args) => {
            let result = processes::status(environment, &args);
            return crumbkit::finish(output, processes::STATUS, result);
        }
        // `logs` prints a stream, so it opens the stream from `output`
        // itself.
        Verb::Logs(args) => return processes::logs(output, environment, &args),
        Verb::Ports {
            command: PortsCommand::Reset(args),
        } => {
            let result = processes::ports_reset(environment, &args);
            return crumbkit::finish(output, processes::PORTS_RESET, result);
        }
        Verb::Archive(_) => "archive",
        Verb::Restore(_) => "restore",
        // crumbkit's own commands read the command tree through
        // `&mut output`, so each runs before `finish` takes `output`.
        Verb::Commands {
            command: CommandsCommand::List(args),
        } => {
            let result = crumbkit::commands::list(&mut output, &GUIDE, &args);
            return crumbkit::finish(output, crumbkit::commands::COMMANDS_LIST, result);
        }
        Verb::Schemas { command } => {
            return match command {
                SchemasCommand::List(args) => {
                    let result = crumbkit::schemas::list(&mut output, &GUIDE, &args);
                    crumbkit::finish(output, crumbkit::schemas::SCHEMAS_LIST, result)
                }
                SchemasCommand::Show(args) => {
                    let result = crumbkit::schemas::show(&mut output, &GUIDE, &args);
                    crumbkit::finish(output, crumbkit::schemas::SCHEMAS_SHOW, result)
                }
            };
        }
        Verb::SelfCommand { command } => {
            return match command {
                SelfCommand::Usage(args) => {
                    let result = Ok::<_, crumbkit::Error>(crumbkit_usage::findings(
                        &mut output,
                        &GUIDE,
                        &args,
                    ));
                    crumbkit::finish(output, crumbkit_usage::SELF_USAGE, result)
                }
                SelfCommand::Figures {
                    command: FiguresCommand::List(args),
                } => {
                    let result = Ok::<_, crumbkit::Error>(crumbkit_usage::figures(
                        &mut output,
                        &GUIDE,
                        &args,
                    ));
                    crumbkit::finish(output, crumbkit_usage::SELF_FIGURES_LIST, result)
                }
            };
        }
    };
    crumbkit::finish(output, UNBUILT_RESULT, fail(verb))
}

/// Prints each of `warnings` on standard error as a line of its own, for
/// a command such as `mxv up` whose result has no field for them.
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
