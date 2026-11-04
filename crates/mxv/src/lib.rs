//! The `mxv` command, which creates, archives and restores task workspaces
//! that span several git repositories.
//!
//! The library holds the whole command, and the `mxv` binary calls [`run`].
//! It exports the parser, the guide and the failure codes, so the
//! integration tests can pass them to crumbkit-test's checks beside the
//! built binary. It is not an API for other crates, so each public item is
//! hidden from the documentation and outside the semver promise.
//!
//! The workspace verbs are declared and not built yet. Each one fails with
//! `mxv::command::unbuilt` until the pull request that builds it.

mod cli;
mod unbuilt;

use std::ffi::OsString;
use std::process::ExitCode;

use clap::CommandFactory as _;
use crumbkit::Code;

#[doc(hidden)]
pub use crate::cli::{Cli, GUIDE};
use crate::cli::{CommandsCommand, FiguresCommand, SchemasCommand, SelfCommand, Verb};
#[doc(hidden)]
pub use crate::unbuilt::UNBUILT;
use crate::unbuilt::{UNBUILT_RESULT, fail};

/// Every failure code that mxv declares, which the tests pass to
/// crumbkit-test's codes, recovery and twin checks.
#[doc(hidden)]
pub const CODES: &[&Code] = &[&UNBUILT];

/// Parses `args`, runs the command and returns the process's exit code. The
/// first argument is the program's name.
///
/// `crumbkit::parse_from` prints help or a refused command line and returns
/// the exit code. `crumbkit::finish` prints the result or the failure in
/// the format that the output flags choose, and returns the exit code.
#[doc(hidden)]
#[must_use]
pub fn run<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let (cli, mut output) = match crumbkit::parse_from::<Cli, _, _>(Cli::command(), &GUIDE, args) {
        Ok(parsed) => parsed,
        Err(stop) => return stop.exit_code(),
    };
    let verb = match cli.verb {
        Verb::New(_) => "new",
        Verb::List(_) => "list",
        Verb::Show(_) => "show",
        Verb::Cd => "cd",
        Verb::Run => "run",
        Verb::Archive(_) => "archive",
        Verb::Restore(_) => "restore",
        Verb::Remove(_) => "remove",
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
