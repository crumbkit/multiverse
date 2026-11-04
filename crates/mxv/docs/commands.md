# Commands of `mxv`

This reference is generated from the help of `mxv`. Edit the help in the tool's code, and not this file.

## `mxv`

Create, archive and restore task workspaces that span several git repositories

### Commands

- [`mxv new`](#mxv-new): Create a workspace with a git worktree of a repository, on a new branch from the freshly fetched origin
- [`mxv list`](#mxv-list): List the workspaces
- [`mxv show`](#mxv-show): Show workspaces by name, in the order given
- [`mxv cd`](#mxv-cd): Print the directory of a workspace, so `cd "$(mxv cd NAME)"` resumes it
- [`mxv up`](#mxv-up): Start a workspace's processes from its mxv.toml, in dependency order, and wait until each is ready
- [`mxv down`](#mxv-down): Stop a workspace's processes with SIGTERM, then SIGKILL after 10 seconds
- [`mxv restart`](#mxv-restart): Stop a workspace's processes and start them again
- [`mxv status`](#mxv-status): Report whether each process of a workspace is running, exited or stopped
- [`mxv logs`](#mxv-logs): Print the last lines of a process's log, and follow it with --follow
- [`mxv archive`](#mxv-archive): Archive a workspace, so restore can rebuild it at the same commits
- [`mxv restore`](#mxv-restore): Rebuild an archived workspace at its archived commits
- [`mxv remove`](#mxv-remove): Remove a workspace's checkouts, its merged branches, its directory and its record
- [`mxv ports`](#mxv-ports): Manage the host ports that a workspace holds
- [`mxv commands`](#mxv-commands): Explore mxv's commands
- [`mxv schemas`](#mxv-schemas): Explore the JSON documents that mxv prints
- [`mxv self`](#mxv-self): Review mxv's own use

### Arguments

- `--json`: Print the result as a JSON document. The default prints text. Works after every subcommand.
- `--fields <FIELD>`: Print only these comma-separated fields, in this order. The default prints the compact columns. Takes more than one value. Works after every subcommand.
- `--long`: Add the long columns after the compact ones. The default prints only the compact columns. Works after every subcommand.
- `--full`: With --json, print every field of every row. The default prints only the fields of the columns. Works after every subcommand.
- `--no-input`: Fail instead of asking a question. Works after every subcommand.

## `mxv new`

Create a workspace with a git worktree of a repository, on a new branch from the freshly fetched origin.

The workspace's directory is \<ROOT\>/\<NAME\>. The root is $MXV\_ROOT when it is set, or else the `root` that the config file $XDG\_CONFIG\_HOME/mxv/config.toml sets, such as `root = "~/work"`, or else $XDG\_DATA\_HOME/mxv/workspaces, which is \~/.local/share/mxv/workspaces when XDG\_DATA\_HOME is unset. The result names the root and where it came from. Each record keeps its workspace's own directory, so changing the root later moves no workspace.

### Arguments

- `<NAME>`: The workspace's name, which also names its directory and its branch. Required.
- `--repo <PATH>`: The git repository to check out, or any directory inside it. Required.
- `--dry-run`: Report what the command would change, and change nothing

### Examples

Print the workspace that a checkout of the repository app would get.

```sh
mxv new triage --repo app --dry-run
```

### Columns

- `name`: The workspace's name, which is also the name of its directory and of its checkouts' branch.
- `branch`: The branch that the workspace's checkouts were created on.
- `path`: The workspace's directory, which `mxv cd` prints. It comes from the workspace's record, so it stays where the workspace was created when the root changes later.
- `root`: The workspace root that the run used, in which `new` creates a workspace's directory.
- `root_source`: Where the root came from: `env` for `MXV_ROOT`, `config` for the config file's `root`, or `default` for `$XDG_DATA_HOME/mxv/workspaces`.
- `ports`: The host port that the workspace holds for each named port of its manifest, which the first `mxv up` allocates.

### Columns that `--long` adds

- `repositories`: The top-level directory of each repository that the workspace checks out.

Effects: additive, not idempotent, open-world.

## `mxv list`

List the workspaces

### Arguments

- `--limit <N>`: Print at most this many rows, up to 100. Default: `25`.
- `--all`: Print every row

### Examples

List the first page of workspaces.

```sh
mxv list
```

### Columns

- `name`: The workspace's name, which is also the name of its directory and of its checkouts' branch.
- `branch`: The branch that the workspace's checkouts were created on.
- `path`: The workspace's directory, which `mxv cd` prints.
- `ports`: The host port that the workspace holds for each named port of its manifest, which the first `mxv up` allocates.
- `problem`: Why mxv cannot read the workspace's record, such as a record that is not JSON or that a newer mxv wrote, or empty when it can. A row with a problem names its record, without `.json`, in `name`, and leaves the other fields empty.

### Columns that `--long` adds

- `repositories`: The top-level directory of each repository that the workspace checks out.

Effects: read-only, closed-world.

## `mxv show`

Show workspaces by name, in the order given

### Arguments

- `<NAME>`: The names of the workspaces to show, separated by spaces. Required. Takes more than one value.

### Examples

Show the workspace reading-list.

```sh
mxv show reading-list
```

### Columns

- `name`: The workspace's name, which is also the name of its directory and of its checkouts' branch.
- `branch`: The branch that the workspace's checkouts were created on.
- `path`: The workspace's directory, which `mxv cd` prints. It comes from the workspace's record, so it stays where the workspace was created when the root changes later.
- `root`: The workspace root that the run used, in which `new` creates a workspace's directory.
- `root_source`: Where the root came from: `env` for `MXV_ROOT`, `config` for the config file's `root`, or `default` for `$XDG_DATA_HOME/mxv/workspaces`.
- `ports`: The host port that the workspace holds for each named port of its manifest, which the first `mxv up` allocates.

### Columns that `--long` adds

- `repositories`: The top-level directory of each repository that the workspace checks out.

Effects: read-only, closed-world.

## `mxv cd`

Print the directory of a workspace, so `cd "$(mxv cd NAME)"` resumes it

### Arguments

- `<NAME>`: The name of the workspace. Required.

### Examples

Print the directory of the workspace reading-list.

```sh
mxv cd reading-list
```

Effects: read-only, closed-world.

## `mxv up`

Start a workspace's processes from its mxv.toml, in dependency order, and wait until each is ready

### Arguments

- `-w`, `--workspace <WORKSPACE>`: The workspace, which defaults to the one that holds the current directory
- `<PROCESS>`: The processes, which default to every process. Takes more than one value.

### Examples

Start the process web of the workspace reading-list and the processes it depends on.

```sh
mxv up -w reading-list web
```

### Columns

- `name`: The process's name in the manifest.
- `state`: `running`, `exited` when it ended on its own or something other than `mxv down` stopped it, or `stopped`.
- `pid`: The process id, which is also its process group's id, while it runs.
- `code`: The exit code of an exited process, or null when a signal ended it or it has not exited.
- `signal`: The number of the signal that ended an exited process, such as 15 for SIGTERM, or null.
- `ports`: The ports that the process receives, as `VARIABLE=port`.

### Columns that `--long` adds

- `log`: The process's log file, which `mxv logs` prints.

Effects: additive, idempotent, open-world.

Writes: applies all items or none.

## `mxv down`

Stop a workspace's processes with SIGTERM, then SIGKILL after 10 seconds

### Arguments

- `-w`, `--workspace <WORKSPACE>`: The workspace, which defaults to the one that holds the current directory
- `<PROCESS>`: The processes, which default to every process. Takes more than one value.

### Examples

Stop every process of the workspace reading-list.

```sh
mxv down -w reading-list
```

### Columns

- `name`: The process's name in the manifest.
- `state`: `running`, `exited` when it ended on its own or something other than `mxv down` stopped it, or `stopped`.
- `pid`: The process id, which is also its process group's id, while it runs.
- `code`: The exit code of an exited process, or null when a signal ended it or it has not exited.
- `signal`: The number of the signal that ended an exited process, such as 15 for SIGTERM, or null.
- `ports`: The ports that the process receives, as `VARIABLE=port`.

### Columns that `--long` adds

- `log`: The process's log file, which `mxv logs` prints.

Effects: destructive, idempotent, closed-world.

Writes: reports each item and exits 1 when any item fails.

## `mxv restart`

Stop a workspace's processes and start them again

### Arguments

- `-w`, `--workspace <WORKSPACE>`: The workspace, which defaults to the one that holds the current directory
- `<PROCESS>`: The processes, which default to every process. Takes more than one value.

### Examples

Stop the process web of the workspace reading-list and start it again.

```sh
mxv restart -w reading-list web
```

### Columns

- `name`: The process's name in the manifest.
- `state`: `running`, `exited` when it ended on its own or something other than `mxv down` stopped it, or `stopped`.
- `pid`: The process id, which is also its process group's id, while it runs.
- `code`: The exit code of an exited process, or null when a signal ended it or it has not exited.
- `signal`: The number of the signal that ended an exited process, such as 15 for SIGTERM, or null.
- `ports`: The ports that the process receives, as `VARIABLE=port`.

### Columns that `--long` adds

- `log`: The process's log file, which `mxv logs` prints.

Effects: destructive, not idempotent, open-world.

Writes: applies all items or none.

## `mxv status`

Report whether each process of a workspace is running, exited or stopped

### Arguments

- `-w`, `--workspace <WORKSPACE>`: The workspace, which defaults to the one that holds the current directory

### Examples

Report the state of each process of the workspace reading-list.

```sh
mxv status -w reading-list
```

### Columns

- `name`: The process's name in the manifest.
- `state`: `running`, `exited` when it ended on its own or something other than `mxv down` stopped it, or `stopped`.
- `pid`: The process id, which is also its process group's id, while it runs.
- `code`: The exit code of an exited process, or null when a signal ended it or it has not exited.
- `signal`: The number of the signal that ended an exited process, such as 15 for SIGTERM, or null.
- `ports`: The ports that the process receives, as `VARIABLE=port`.

### Columns that `--long` adds

- `log`: The process's log file, which `mxv logs` prints.

Effects: read-only, closed-world.

## `mxv logs`

Print the last lines of a process's log, and follow it with --follow

### Arguments

- `-w`, `--workspace <WORKSPACE>`: The workspace, which defaults to the one that holds the current directory
- `<PROCESS>`: The process whose log to print. Required.
- `-f`, `--follow`: Keep printing each line that the process writes, until interrupted
- `--lines <N>`: How many of the log's last lines to print first. Default: `50`.

### Examples

Print the last lines of the log of web, then each new line.

```sh
mxv logs -w reading-list web --follow
```

### Columns

- `line`: The line's text, without its line feed.

Effects: read-only, closed-world.

Output: one row per line as each arrives, and --json prints one JSON document per line.

Text: one line per row, `<LINE>`; --fields prints a table.

## `mxv archive`

Archive a workspace, so restore can rebuild it at the same commits

### Arguments

- `--dry-run`: Report what the command would change, and change nothing
- `--yes`: Proceed without asking for confirmation

### Examples

Print what archiving would do.

```sh
mxv archive --dry-run
```

Effects: destructive, not idempotent, closed-world.

## `mxv restore`

Rebuild an archived workspace at its archived commits

### Arguments

- `--dry-run`: Report what the command would change, and change nothing
- `--yes`: Proceed without asking for confirmation

### Examples

Print what restoring would do.

```sh
mxv restore --dry-run
```

Effects: additive, not idempotent, open-world.

## `mxv remove`

Remove a workspace's checkouts, its merged branches, its directory and its record

### Arguments

- `<NAME>`: The name of the workspace to remove. Required.
- `--force`: Discard uncommitted changes, untracked files and ignored files such as .env, and stop running processes; an unmerged branch is still kept
- `--delete-unmerged`: Delete branches with unmerged commits, and commits on a detached HEAD
- `--dry-run`: Report what the command would change, and change nothing
- `--yes`: Proceed without asking for confirmation

### Examples

Print what removing the workspace reading-list would delete.

```sh
mxv remove reading-list --dry-run
```

Effects: destructive, not idempotent, closed-world.

## `mxv ports`

Manage the host ports that a workspace holds

### Commands

- [`mxv ports reset`](#mxv-ports-reset): Give a stopped workspace new host ports, each different from the port it held

## `mxv ports reset`

Give a stopped workspace new host ports, each different from the port it held

### Arguments

- `-w`, `--workspace <WORKSPACE>`: The workspace, which defaults to the one that holds the current directory
- `<PORT>`: The ports of the manifest to reallocate, which default to every port. Takes more than one value.

### Examples

Give the workspace reading-list a new host port for api.

```sh
mxv ports reset -w reading-list api
```

### Columns

- `name`: The port's name in the manifest.
- `port`: The host port that the workspace holds for it.

Effects: destructive, not idempotent, closed-world.

Writes: applies all items or none.

## `mxv commands`

Explore mxv's commands

### Commands

- [`mxv commands list`](#mxv-commands-list): List a branch of mxv's commands with their arguments, examples and columns

## `mxv commands list`

List a branch of mxv's commands with their arguments, examples and columns

### Arguments

- `<PATH>`: The names of the commands below the root that lead to the branch, such as `books list`. Takes more than one value.
- `--limit <N>`: Print at most this many rows. Default: `25`.
- `--all`: Print every row

### Examples

List every command.

```sh
mxv commands list
```

### Columns

- `path`: The command line that runs the command, such as `shelf books list`.
- `about`: The command's one-line description, or its full description when it has no one-line description.

Effects: read-only, closed-world.

## `mxv schemas`

Explore the JSON documents that mxv prints

### Commands

- [`mxv schemas list`](#mxv-schemas-list): List the JSON documents that mxv prints, with a description of each
- [`mxv schemas show`](#mxv-schemas-show): Print the JSON Schema of each document

## `mxv schemas list`

List the JSON documents that mxv prints, with a description of each

### Arguments

- `--limit <N>`: Print at most this many rows. Default: `25`.
- `--all`: Print every row

### Examples

List every document that mxv prints.

```sh
mxv schemas list
```

### Columns

- `id`: The document's id, such as `shelf/books-list/v1`, which `schemas show` takes.
- `kind`: output when a command prints the document with its schema id, unwrapped when a command prints it without the envelope that holds its schema id, and file when it is a file format that users write and no command prints.
- `form`: stream when a command prints the document as a stream, one document on each line as each row arrives, and document when a command prints it once or it is a file format.
- `description`: What the document holds, in one line.

### Columns that `--long` adds

- `commands`: The command lines that print the document. It is empty for the failure document, which every command prints when it fails with --json, and for a file format.

Effects: read-only, closed-world.

## `mxv schemas show`

Print the JSON Schema of each document

### Arguments

- `<ID>`: The ids of the documents that `schemas list` lists, separated by spaces. Required. Takes more than one value.

### Examples

Print the JSON Schema of the document that schemas list prints.

```sh
mxv schemas show mxv/schemas-list/v1
```

### Columns

- `id`: The document's id, such as `shelf/books-list/v1`.
- `schema`: The document's 2020-12 JSON Schema.

Effects: read-only, closed-world.

## `mxv self`

Review mxv's own use

### Commands

- [`mxv self usage`](#mxv-self-usage): Report the patterns in mxv's usage log where callers waste effort
- [`mxv self figures`](#mxv-self-figures): Work with the figures of each command in mxv's usage log

## `mxv self usage`

Report the patterns in mxv's usage log where callers waste effort

### Arguments

- `--since <DURATION>`: How far back to read the usage log, such as 12h, 7d or 2w. Default: `7d`.
- `--log <FILE>`: Read this usage log instead of the tool's own

### Examples

Report the findings of the last 7 days.

```sh
mxv self usage
```

### Columns

- `finding`: What the pattern is.
- `runs`: The runs that show the pattern.
- `command`: The command path below the tool's name, such as `books show`, or the tool's name for its root command.
- `suggestion`: What the pattern suggests, or the figure that raised it.

Effects: read-only, closed-world.

## `mxv self figures`

Work with the figures of each command in mxv's usage log

### Commands

- [`mxv self figures list`](#mxv-self-figures-list): List the figures of each command in mxv's usage log

## `mxv self figures list`

List the figures of each command in mxv's usage log

### Arguments

- `--since <DURATION>`: How far back to read the usage log, such as 12h, 7d or 2w. Default: `7d`.
- `--log <FILE>`: Read this usage log instead of the tool's own
- `--limit <N>`: Print at most this many rows. Default: `25`.
- `--all`: Print every row

### Examples

Print the figures of each command over the last 7 days.

```sh
mxv self figures list
```

### Columns

- `command`: The command path below the tool's name, such as `books show`, or the tool's name for its root command.
- `runs`: The runs that did not print a help page.
- `help_pages`: The runs that printed the command's help page.
- `help_before`: The help pages that each run's session read since its last run that printed no help page, summed over the runs.
- `failed`: The runs that failed.
- `p95_out_bytes`: The 95th percentile of the bytes that the runs wrote to standard output, or null when no run printed a result.

### Columns that `--long` adds

- `refused`: The runs whose command line was refused.
- `p95_ms`: The 95th percentile of the runs' wall time in milliseconds, or null when every run printed help.
- `typed_full`: The runs that typed `--full`.
- `typed_all`: The runs that typed `--all`.
- `typed_long`: The runs that typed `--long`.

Effects: read-only, closed-world.
