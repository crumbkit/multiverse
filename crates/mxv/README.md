# mxv

mxv is the command of [multiverse](../multiverse). It creates task
workspaces that span several git repositories, with one git worktree for
each repository. A workspace holds one repository for now.

`mxv commands list` lists every command, and `mxv <command> --help`
describes each one. [docs/commands.md](docs/commands.md) is the reference.

## Layout

`mxv new reading-list --repo ~/Code/inbox` fetches `origin` in
`~/Code/inbox`, and creates the workspace directory
`~/mxv/reading-list`. That directory holds:

- `AGENTS.md`, which tells an agent which repository and branch the
  workspace holds and how to list, resume and remove it. It sits above the
  checkout, so git never sees it.
- `inbox/`, a git worktree of `~/Code/inbox` on the new branch
  `reading-list`, which starts at the freshly fetched default branch of
  `origin` (`origin/HEAD`, or else `origin/main` or `origin/master`) and
  tracks no upstream.

A later workspace with several repositories holds one such directory for
each repository, beside the one `AGENTS.md`.

mxv keeps one JSON record for each workspace in
`$XDG_STATE_HOME/mxv/workspaces/<name>.json`, which defaults to
`~/.local/state/mxv/workspaces/<name>.json`. `mxv new` writes the record
before it makes the checkout, so a run that stops partway leaves a record
that `mxv remove` cleans up.

## Removing a workspace

`mxv remove <name>` refuses with `mxv::workspace::dirty` while a checkout has
uncommitted or untracked changes. It then removes each checkout, deletes
each branch whose commits are all on its base, keeps a branch with other
commits, removes the workspace directory and `AGENTS.md`, and deletes the
record. `--force` removes checkouts with changes and deletes unmerged
branches. The merge check reads the base as the repository last fetched it,
so a branch that was merged by squashing reads as unmerged and is kept.

## Environment

mxv reads these variables, and `VARIABLES` in `src/environment.rs` holds the
same catalog:

| Variable | Purpose |
| --- | --- |
| `HOME` | The home directory. The workspace root defaults to `$HOME/mxv`, and the state directory to `$HOME/.local/state`. |
| `XDG_STATE_HOME` | The state directory, an absolute path. Records live in `$XDG_STATE_HOME/mxv/workspaces`. A relative or empty value is ignored. |
| `MXV_ROOT` | The directory that holds one directory for each workspace, an absolute path. A relative value fails with `mxv::environment::no_directory`. |

When neither the variable nor `HOME` names an absolute directory, a
workspace command fails with `mxv::environment::no_directory`. mxv passes
its environment to git, with `GIT_TERMINAL_PROMPT=0` so a fetch that needs a
password fails instead of waiting.

## Not built yet

`mxv run`, `mxv archive` and `mxv restore` are declared and fail with
`mxv::command::unbuilt` until the pull request that builds each one.
