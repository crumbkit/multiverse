//! The `mxv` command's command-line grammar and help guide.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use crumbkit::commands::{COMMANDS_LIST, CommandsList};
use crumbkit::help::{Effects, Example, Goal, Group, Guide, Leaf};
use crumbkit::schemas::{SCHEMAS_LIST, SCHEMAS_SHOW, SchemasList, SchemasShow};
use crumbkit::{DryRun, NoInput, Page, ViewFlags, Yes};
use crumbkit_usage::{SELF_FIGURES_LIST, SELF_USAGE, SelfFigures, SelfUsage};

use crate::verbs::{CD, LIST, NEW, REMOVE, SHOW};

/// Create, archive and restore task workspaces that span several git
/// repositories.
#[derive(Debug, Parser)]
#[command(name = "mxv", version)]
pub struct Cli {
    /// The command to run.
    #[command(subcommand)]
    pub(crate) verb: Verb,

    /// The flags that choose how the result prints.
    #[command(flatten)]
    pub(crate) view: ViewFlags,

    /// The flag that tells mxv that nobody can answer a question.
    #[command(flatten)]
    pub(crate) no_input: NoInput,
}

/// The `mxv` command's root verbs, goal headings and leaves. The workspace
/// verbs sit directly under the root, and every other command keeps
/// crumbkit's resource-verb form.
pub const GUIDE: Guide = Guide::new()
    .root_verbs(
        &[
            "new", "list", "show", "cd", "run", "archive", "restore", "remove",
        ],
        "Workspaces are mxv's everyday resource, and people type these verbs many times a day.",
    )
    .groups(&[Group::new(
        &[],
        &[
            Goal::new("Work in a workspace", &["new", "list", "show", "cd", "run"]),
            Goal::new("Put a workspace away", &["archive", "restore", "remove"]),
            Goal::new("Explore the commands", &["commands", "schemas"]),
            Goal::new("Review how mxv is used", &["self"]),
        ],
    )
    .compact()])
    .leaves(&[
        Leaf::new(&["new"])
            .examples(&[Example::new(
                &["triage", "--repo", "inbox", "--dry-run"],
                "Print the workspace that a checkout of the repository inbox would get",
            )])
            .effects(Effects::additive().open_world())
            .document(NEW),
        Leaf::new(&["list"])
            .examples(&[Example::new(&[], "List the first page of workspaces")])
            .effects(Effects::read_only())
            .document(LIST),
        Leaf::new(&["show"])
            .examples(&[Example::new(
                &["reading-list"],
                "Show the workspace reading-list",
            )])
            .effects(Effects::read_only())
            .document(SHOW),
        Leaf::new(&["cd"])
            .examples(&[Example::new(
                &["reading-list"],
                "Print the directory of the workspace reading-list",
            )])
            .effects(Effects::read_only())
            .document(CD),
        Leaf::new(&["run"])
            .examples(&[Example::new(&[], "Start the current app's processes")])
            .effects(Effects::destructive().open_world())
            .prints_no_result(),
        Leaf::new(&["archive"])
            .examples(&[Example::new(
                &["--dry-run"],
                "Print what archiving would do",
            )])
            .effects(Effects::destructive()),
        Leaf::new(&["restore"])
            .examples(&[Example::new(
                &["--dry-run"],
                "Print what restoring would do",
            )])
            .effects(Effects::additive().open_world()),
        Leaf::new(&["remove"])
            .examples(&[Example::new(
                &["reading-list", "--dry-run"],
                "Print what removing the workspace reading-list would delete",
            )])
            .effects(Effects::destructive())
            .document(REMOVE),
        Leaf::new(&["commands", "list"])
            .examples(&[Example::new(&[], "List every command")])
            .effects(Effects::read_only())
            .document(COMMANDS_LIST),
        Leaf::new(&["schemas", "list"])
            .examples(&[Example::new(&[], "List every document that mxv prints")])
            .effects(Effects::read_only())
            .document(SCHEMAS_LIST),
        Leaf::new(&["schemas", "show"])
            .examples(&[Example::new(
                &["mxv/schemas-list/v1"],
                "Print the JSON Schema of the document that schemas list prints",
            )])
            .effects(Effects::read_only())
            .document(SCHEMAS_SHOW),
        Leaf::new(&["self", "usage"])
            .examples(&[Example::new(&[], "Report the findings of the last 7 days")])
            .effects(Effects::read_only())
            .document(SELF_USAGE),
        Leaf::new(&["self", "figures", "list"])
            .examples(&[Example::new(
                &[],
                "Print the figures of each command over the last 7 days",
            )])
            .effects(Effects::read_only())
            .document(SELF_FIGURES_LIST),
    ]);

/// The commands of `mxv`.
#[derive(Debug, Subcommand)]
pub(crate) enum Verb {
    /// Create a workspace with a git worktree of a repository, on a new branch from the freshly fetched origin.
    ///
    /// The workspace's directory is <ROOT>/<NAME>. The root is $MXV_ROOT when it is set, or else the `root` that the config file $XDG_CONFIG_HOME/mxv/config.toml sets, such as `root = "~/work"`, or else $XDG_DATA_HOME/mxv/workspaces, which is ~/.local/share/mxv/workspaces when XDG_DATA_HOME is unset. The result names the root and where it came from. Each record keeps its workspace's own directory, so changing the root later moves no workspace.
    New(NewArgs),
    /// List the workspaces.
    List(ListArgs),
    /// Show workspaces by name, in the order given.
    Show(ShowArgs),
    /// Print the directory of a workspace, so `cd "$(mxv cd NAME)"` resumes it.
    Cd(CdArgs),
    /// Run an app's processes, or one command in the app's environment.
    Run,
    /// Archive a workspace, so restore can rebuild it at the same commits.
    Archive(WriteArgs),
    /// Rebuild an archived workspace at its archived commits.
    Restore(WriteArgs),
    /// Remove a workspace's checkouts, its merged branches, its directory and its record.
    // `rm` is a hidden alias, so a person can type it, and help, `Next:`
    // lines and `commands list` name only `remove`.
    #[command(alias = "rm")]
    Remove(RemoveArgs),
    /// Explore mxv's commands.
    Commands {
        /// What to do with the commands.
        #[command(subcommand)]
        command: CommandsCommand,
    },
    /// Explore the JSON documents that mxv prints.
    Schemas {
        /// What to do with the documents.
        #[command(subcommand)]
        command: SchemasCommand,
    },
    /// Review mxv's own use.
    #[command(name = "self")]
    SelfCommand {
        /// What to review.
        #[command(subcommand)]
        command: SelfCommand,
    },
}

/// The commands for mxv's own commands.
#[derive(Debug, Subcommand)]
pub(crate) enum CommandsCommand {
    /// List a branch of mxv's commands with their arguments, examples and columns.
    List(CommandsList),
}

/// The commands for the JSON documents that mxv prints.
#[derive(Debug, Subcommand)]
pub(crate) enum SchemasCommand {
    /// List the JSON documents that mxv prints, with a description of each.
    List(SchemasList),
    /// Print the JSON Schema of each document.
    Show(SchemasShow),
}

/// The commands that review mxv itself.
#[derive(Debug, Subcommand)]
pub(crate) enum SelfCommand {
    /// Report the patterns in mxv's usage log where callers waste effort.
    Usage(SelfUsage),
    /// Work with the figures of each command in mxv's usage log.
    Figures {
        /// What to do with the figures.
        #[command(subcommand)]
        command: FiguresCommand,
    },
}

/// The commands for the figures in mxv's usage log.
#[derive(Debug, Subcommand)]
pub(crate) enum FiguresCommand {
    /// List the figures of each command in mxv's usage log.
    List(SelfFigures),
}

/// The flags of a verb that changes workspaces. The pull request that
/// builds each verb adds its own arguments.
#[derive(Debug, Args)]
pub(crate) struct WriteArgs {
    /// Print what the command would do, and change nothing.
    #[command(flatten)]
    pub(crate) dry_run: DryRun,

    /// Run without asking.
    #[command(flatten)]
    pub(crate) yes: Yes,
}

/// The arguments of `mxv new`.
#[derive(Debug, Args)]
pub(crate) struct NewArgs {
    /// The workspace's name, which also names its directory and its branch
    pub(crate) name: String,

    /// The git repository to check out, or any directory inside it
    #[arg(long, value_name = "PATH")]
    pub(crate) repo: PathBuf,

    /// Print the workspace that the command would create, and create nothing.
    #[command(flatten)]
    pub(crate) dry_run: DryRun,
}

/// The arguments of `mxv cd`.
#[derive(Debug, Args)]
pub(crate) struct CdArgs {
    /// The name of the workspace
    pub(crate) name: String,
}

/// The arguments of `mxv remove`.
#[derive(Debug, Args)]
pub(crate) struct RemoveArgs {
    /// The name of the workspace to remove
    pub(crate) name: String,

    /// Discard uncommitted changes, untracked files and ignored files such as .env; an unmerged branch is still kept
    #[arg(long)]
    pub(crate) force: bool,

    /// Delete branches with unmerged commits, and commits on a detached HEAD
    #[arg(long)]
    pub(crate) delete_unmerged: bool,

    /// Print what the command would remove, and remove nothing.
    #[command(flatten)]
    pub(crate) dry_run: DryRun,

    /// Remove the workspace without asking.
    #[command(flatten)]
    pub(crate) yes: Yes,
}

/// The arguments of `mxv list`.
#[derive(Debug, Args)]
pub(crate) struct ListArgs {
    /// How many workspaces to print.
    #[command(flatten)]
    pub(crate) page: Page<25, 100>,
}

/// The arguments of `mxv show`.
#[derive(Debug, Args)]
pub(crate) struct ShowArgs {
    /// The names of the workspaces to show, separated by spaces
    #[arg(required = true, value_name = "NAME", num_args = 1..=100)]
    pub(crate) names: Vec<String>,
}
