//! The `mxv` command, which creates, archives and restores task workspaces
//! that span several git repositories.
//!
//! The library holds the whole command, and the `mxv` binary calls [`run`].
//! It exports the parser, the guide and the failure codes, so the
//! integration tests can pass them to crumbkit-test's checks beside the
//! built binary. It is not an API for other crates, so each public item is
//! hidden from the documentation and outside the semver promise.
//!
//! `new`, `list`, `show`, `cd` and `remove` are built. `run`, `archive` and
//! `restore` are declared and fail with `mxv::command::unbuilt` until the
//! pull request that builds each one.

mod cli;
mod config;
mod environment;
mod failures;
mod rows;
#[cfg(test)]
mod testing;
mod unbuilt;
mod verbs;

use std::ffi::OsString;
use std::process::ExitCode;

use clap::CommandFactory as _;
use crumbkit::Code;

#[doc(hidden)]
pub use crate::cli::{Cli, GUIDE};
use crate::cli::{CommandsCommand, FiguresCommand, SchemasCommand, SelfCommand, Verb};
#[doc(hidden)]
pub use crate::environment::{Bases, Environment, VARIABLES, Variable};
#[doc(hidden)]
pub use crate::failures::{
    BRANCH_EXISTS, CHECKOUT_LOCKED, CHECKOUT_UNOWNED, CHECKOUT_UNREACHABLE_COMMITS, CONFIG_INVALID,
    CONFIG_UNREADABLE, DIRECTORY_TAKEN, ENVIRONMENT_NO_DIRECTORY, ENVIRONMENT_RELATIVE_ROOT,
    FILE_UNWRITABLE, GIT_FAILED, GIT_NOT_FOUND, GIT_TIMED_OUT, PATH_INACCESSIBLE, RECORD_CORRUPT,
    RECORD_MISMATCHED, RECORD_NEWER, RECORD_UNREADABLE, REPOSITORY_EMPTY_ORIGIN,
    REPOSITORY_NO_DEFAULT_BRANCH, REPOSITORY_NO_ORIGIN, REPOSITORY_NOT_FOUND, WORKSPACE_BAD_NAME,
    WORKSPACE_CHANGED, WORKSPACE_DIRTY, WORKSPACE_EXISTS, WORKSPACE_IGNORED_FILES,
    WORKSPACE_NOT_FOUND,
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
        Verb::Run => "run",
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
