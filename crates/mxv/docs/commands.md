# Commands of `mxv`

This reference is generated from the help of `mxv`. Edit the help in the tool's code, and not this file.

## `mxv`

Create, archive and restore task workspaces that span several git repositories

### Commands

- [`mxv new`](#mxv-new): Create a workspace with one git worktree for each repository
- [`mxv list`](#mxv-list): List the live workspaces
- [`mxv show`](#mxv-show): Show live workspaces by name, in the order given
- [`mxv cd`](#mxv-cd): Print the directory of a workspace or of one of its apps
- [`mxv run`](#mxv-run): Run an app's processes, or one command in the app's environment
- [`mxv archive`](#mxv-archive): Archive a workspace, so restore can rebuild it at the same commits
- [`mxv restore`](#mxv-restore): Rebuild an archived workspace at its archived commits
- [`mxv remove`](#mxv-remove): Delete a live or archived workspace
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

Create a workspace with one git worktree for each repository

### Arguments

- `--dry-run`: Report what the command would change, and change nothing
- `--yes`: Proceed without asking for confirmation

### Examples

Create a workspace.

```sh
mxv new
```

Effects: additive, not idempotent, open-world.

## `mxv list`

List the live workspaces

### Arguments

- `--limit <N>`: Print at most this many rows, up to 100. Default: `25`.
- `--all`: Print every row

### Examples

List the first page of live workspaces.

```sh
mxv list
```

Effects: read-only, closed-world.

## `mxv show`

Show live workspaces by name, in the order given

### Arguments

- `<NAME>`: The names of the workspaces to show, separated by spaces. Required. Takes more than one value.

### Examples

Show the workspace reading-list.

```sh
mxv show reading-list
```

Effects: read-only, closed-world.

## `mxv cd`

Print the directory of a workspace or of one of its apps

### Examples

Print the directory of the current workspace.

```sh
mxv cd
```

Effects: read-only, closed-world.

## `mxv run`

Run an app's processes, or one command in the app's environment

### Examples

Start the current app's processes.

```sh
mxv run
```

Effects: destructive, not idempotent, open-world.

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

Delete a live or archived workspace

### Arguments

- `--dry-run`: Report what the command would change, and change nothing
- `--yes`: Proceed without asking for confirmation

### Examples

Print what removing would delete.

```sh
mxv remove --dry-run
```

Effects: destructive, not idempotent, closed-world.

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
