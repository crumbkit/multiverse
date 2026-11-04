//! The `mxv` command's command-line grammar and help guide.

use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand};
use crumbkit::commands::{COMMANDS_LIST, CommandsList};
use crumbkit::help::{Effects, Example, Goal, Group, Guide, Leaf};
use crumbkit::schemas::{SCHEMAS_LIST, SCHEMAS_SHOW, SchemasList, SchemasShow};
use crumbkit::{DryRun, NoInput, Page, ViewFlags, Yes};
use crumbkit_usage::{SELF_FIGURES_LIST, SELF_USAGE, SelfFigures, SelfUsage};

use crate::clean::CLEAN;
use crate::processes::{DOWN, LOGS, PORTS_RESET, RESTART, STATUS, UP};
use crate::units::parse_age;
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
            "new", "list", "show", "cd", "up", "down", "restart", "status", "logs", "archive",
            "restore", "remove", "clean",
        ],
        "Workspaces are mxv's everyday resource, and people type these verbs many times a day.",
    )
    .groups(&[Group::new(
        &[],
        &[
            Goal::new("Work in a workspace", &["new", "list", "show", "cd"]),
            Goal::new(
                "Run a workspace's app",
                &["up", "down", "restart", "status", "logs", "ports"],
            ),
            Goal::new("Put a workspace away", &["archive", "restore", "remove"]),
            Goal::new("Free disk space", &["clean"]),
            Goal::new("Explore the commands", &["commands", "schemas"]),
            Goal::new("Review how mxv is used", &["self"]),
        ],
    )
    .compact()])
    .leaves(&[
        Leaf::new(&["new"])
            .examples(&[Example::new(
                &["triage", "--repo", "app", "--dry-run"],
                "Print the workspace that a checkout of the repository app would get",
            )])
            .effects(Effects::additive().open_world())
            .document(NEW),
        Leaf::new(&["list"])
            .examples(&[Example::new(&[], "List the first page of workspaces")])
            .effects(Effects::read_only())
            .document(LIST),
        Leaf::new(&["show"])
            .examples(&[Example::new(&["reading-list"], "Show the workspace reading-list")])
            .effects(Effects::read_only())
            .document(SHOW),
        Leaf::new(&["cd"])
            .examples(&[Example::new(
                &["reading-list"],
                "Print the directory of the workspace reading-list",
            )])
            .effects(Effects::read_only())
            .document(CD),
        Leaf::new(&["up"])
            .examples(&[Example::new(
                &["-w", "reading-list", "web"],
                "Start the process web of the workspace reading-list and the processes it depends on",
            )])
            .effects(Effects::additive().idempotent().open_world().all_or_none())
            .document(UP),
        Leaf::new(&["down"])
            .examples(&[Example::new(
                &["-w", "reading-list"],
                "Stop every process of the workspace reading-list",
            )])
            .effects(Effects::destructive().idempotent().per_item())
            .document(DOWN),
        Leaf::new(&["restart"])
            .examples(&[Example::new(
                &["-w", "reading-list", "web"],
                "Stop the process web of the workspace reading-list and start it again",
            )])
            .effects(Effects::destructive().open_world().all_or_none())
            .document(RESTART),
        Leaf::new(&["status"])
            .examples(&[Example::new(
                &["-w", "reading-list"],
                "Report the state of each process of the workspace reading-list",
            )])
            .effects(Effects::read_only())
            .document(STATUS),
        Leaf::new(&["logs"])
            .examples(&[Example::new(
                &["-w", "reading-list", "web", "--follow"],
                "Print the last lines of the log of web, then each new line",
            )])
            .effects(Effects::read_only())
            .stream(LOGS),
        Leaf::new(&["ports", "reset"])
            .examples(&[Example::new(
                &["-w", "reading-list", "api"],
                "Give the workspace reading-list a new host port for api",
            )])
            .effects(Effects::destructive().all_or_none())
            .document(PORTS_RESET),
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
        Leaf::new(&["clean"])
            .examples(&[Example::new(
                &["--all", "--dry-run"],
                "Print what cleaning every idle workspace would delete",
            )])
            .effects(Effects::destructive().idempotent().per_item())
            .document(CLEAN),
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
    /// Start a workspace's processes from its mxv.toml, in dependency order, and wait until each is ready.
    Up(ProcessesArgs),
    /// Stop a workspace's processes with SIGTERM, then SIGKILL after 10 seconds.
    Down(ProcessesArgs),
    /// Stop a workspace's processes and start them again.
    Restart(ProcessesArgs),
    /// Report whether each process of a workspace is running, exited or stopped.
    Status(StatusArgs),
    /// Print the last lines of a process's log, and follow it with --follow.
    Logs(LogsArgs),
    /// Archive a workspace, so restore can rebuild it at the same commits.
    Archive(WriteArgs),
    /// Rebuild an archived workspace at its archived commits.
    Restore(WriteArgs),
    /// Remove a workspace's checkouts, its merged branches, its directory and its record.
    // `rm` is a hidden alias, so a person can type it, and help, `Next:`
    // lines and `commands list` name only `remove`.
    #[command(alias = "rm")]
    Remove(RemoveArgs),
    /// Delete the disposable directories, such as target/ and node_modules/, of idle workspaces.
    ///
    /// A workspace is idle when none of its processes or containers run and no file or directory of its checkout outside the disposable directories, and no commit or staged change, changed within the idle age. The disposable directories are the ignored directories named target, node_modules, dist, build and .cache, and the paths that the `disposable` globs of the checkout's mxv.toml and of the config file match while git reports them as ignored. A directory that holds a .git entry at any depth is kept. The command deletes nothing else: no tracked file, no git data and no other ignored file. It takes each workspace's lock and checks again that the workspace is idle just before it deletes anything. A workspace that mxv cannot find, measure or clean fails alone, the others are still cleaned, and the command exits with 1. Nothing runs in the background, so a cron job or a hook runs `mxv clean --over-limit --yes` to keep the disk under the config file's `max_disposable`.
    Clean(CleanArgs),
    /// Manage the host ports that a workspace holds.
    Ports {
        /// What to do with the ports.
        #[command(subcommand)]
        command: PortsCommand,
    },
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

/// The commands for a workspace's host ports.
#[derive(Debug, Subcommand)]
pub(crate) enum PortsCommand {
    /// Give a stopped workspace new host ports, each different from the port it held.
    Reset(PortsResetArgs),
}

/// The arguments of `mxv ports reset`.
#[derive(Debug, Args)]
pub(crate) struct PortsResetArgs {
    /// The workspace, which defaults to the one that holds the current directory
    #[arg(short, long, value_name = "WORKSPACE")]
    pub(crate) workspace: Option<String>,

    /// The ports of the manifest to reallocate, which default to every port
    #[arg(value_name = "PORT")]
    pub(crate) ports: Vec<String>,
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

    /// Clone none of the repository's ignored directories, such as node_modules/, into the new checkout
    #[arg(long)]
    pub(crate) no_clone: bool,

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

    /// Discard uncommitted changes, untracked files and ignored files such as .env, and stop running processes and containers; an unmerged branch and the workspace's volumes are still kept
    #[arg(long)]
    pub(crate) force: bool,

    /// Delete the workspace's docker volumes and the data in them
    #[arg(long)]
    pub(crate) delete_volumes: bool,

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

    /// Measure each workspace's size on disk and its disposable size, which reads every file of every workspace on the page
    #[arg(long)]
    pub(crate) sizes: bool,
}

/// The arguments of `mxv clean`.
#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("selection")
        .required(true)
        .args(["names", "all", "over_limit"])
))]
pub(crate) struct CleanArgs {
    /// The names of the workspaces to clean, separated by spaces
    #[arg(value_name = "NAME", num_args = 1..=100)]
    pub(crate) names: Vec<String>,

    /// Clean every idle workspace
    #[arg(long, conflicts_with_all = ["names", "over_limit"])]
    pub(crate) all: bool,

    /// Clean idle workspaces, oldest first, until the disposable directories of every workspace take no more than the config file's max_disposable
    #[arg(long, conflicts_with = "names")]
    pub(crate) over_limit: bool,

    /// How long a checkout must go unmodified to count as idle, such as 7d, 36h or 0s; it defaults to the config file's clean_idle, or else 7d
    #[arg(long, value_name = "AGE", value_parser = parse_age)]
    pub(crate) idle: Option<std::time::Duration>,

    /// Print what the command would delete, and delete nothing.
    #[command(flatten)]
    pub(crate) dry_run: DryRun,

    /// Delete without asking.
    #[command(flatten)]
    pub(crate) yes: Yes,
}

/// The arguments of `mxv show`.
#[derive(Debug, Args)]
pub(crate) struct ShowArgs {
    /// The names of the workspaces to show, separated by spaces
    #[arg(required = true, value_name = "NAME", num_args = 1..=100)]
    pub(crate) names: Vec<String>,
}

/// The arguments of `mxv up`, `mxv down` and `mxv restart`.
#[derive(Debug, Args)]
pub(crate) struct ProcessesArgs {
    /// The workspace, which defaults to the one that holds the current directory
    #[arg(short, long, value_name = "WORKSPACE")]
    pub(crate) workspace: Option<String>,

    /// The processes, which default to every process
    #[arg(value_name = "PROCESS")]
    pub(crate) processes: Vec<String>,
}

/// The arguments of `mxv status`.
#[derive(Debug, Args)]
pub(crate) struct StatusArgs {
    /// The workspace, which defaults to the one that holds the current directory
    #[arg(short, long, value_name = "WORKSPACE")]
    pub(crate) workspace: Option<String>,
}

/// The arguments of `mxv logs`.
#[derive(Debug, Args)]
pub(crate) struct LogsArgs {
    /// The workspace, which defaults to the one that holds the current directory
    #[arg(short, long, value_name = "WORKSPACE")]
    pub(crate) workspace: Option<String>,

    /// The process whose log to print
    #[arg(value_name = "PROCESS")]
    pub(crate) process: String,

    /// Keep printing each line that the process writes, until interrupted
    #[arg(short, long)]
    pub(crate) follow: bool,

    /// How many of the log's last lines to print first
    #[arg(long, value_name = "N", default_value_t = 50)]
    pub(crate) lines: usize,
}
