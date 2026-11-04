# mxv

mxv is the command of [multiverse](../multiverse). It creates task
workspaces that span several git repositories, with one git worktree for
each repository. A workspace holds one repository for now.

`mxv commands list` lists every command, and `mxv <command> --help`
describes each one. [docs/commands.md](docs/commands.md) is the reference.

## Layout

`mxv new reading-list --repo ~/src/app` fetches `origin` in
`~/src/app`, and creates the workspace directory `<root>/reading-list`,
which is `~/.local/share/mxv/workspaces/reading-list` by default. That
directory holds:

- `AGENTS.md`, which tells an agent which repository and branch the
  workspace holds and how to list, resume and remove it. It sits above the
  checkout, so git never sees it.
- `app/`, a git worktree of `~/src/app` on the new branch
  `reading-list`, which starts at the freshly fetched default branch of
  `origin` (the branch that `origin/HEAD` names, or else `origin/main` or
  `origin/master`, recorded as a full reference such as
  `refs/remotes/origin/main`) and tracks no upstream.

A later workspace with several repositories holds one such directory for
each repository, beside the one `AGENTS.md`.

## The workspace root

The workspace root is the directory that holds one directory for each
workspace. mxv takes the first of these that is set:

1. `MXV_ROOT`, an absolute path. A relative value fails with
   `mxv::environment::relative_root`.
2. `root` in the config file `$XDG_CONFIG_HOME/mxv/config.toml`, which is
   `~/.config/mxv/config.toml` when `XDG_CONFIG_HOME` is unset, empty or
   relative.
3. `$XDG_DATA_HOME/mxv/workspaces`, which is `~/.local/share/mxv/workspaces`
   when `XDG_DATA_HOME` is unset, empty or relative.

The config file holds one setting:

```toml
root = "~/work"
```

`root` is a string that holds an absolute path, or a path that starts with
`~/`, which starts at the home directory. mxv reads the file strictly. Any
other key, a value that is not a string, a relative path or text that is not
TOML fails with `mxv::config::invalid`, whose fields name the file and the
line. mxv reads the file only when `MXV_ROOT` is unset or empty.

`mxv new` and `mxv show` print the root they used in `root`, and where it came
from in `root_source`: `env`, `config` or `default`. Each record keeps its
workspace's own directory, so changing the root later moves no workspace,
and `show`, `cd` and `remove` still find each workspace where it was created.

## Records

mxv keeps one JSON record for each workspace in
`$XDG_STATE_HOME/mxv/workspaces/<name>.json`, which defaults to
`~/.local/state/mxv/workspaces/<name>.json`. `mxv new` writes the record
before it makes the checkout, so a run that stops partway leaves a record
that `mxv remove` cleans up. `mxv new` creates the branch in one step that
fails when the branch already exists, so a branch that another process
creates meanwhile is reported as `mxv::branch::exists` and left alone. When
`git worktree add` fails partway, `mxv new` removes the checkout, unlocks it
first if git left it locked, deletes the branch only while it still points
where the run created it, removes the `AGENTS.md` and directory that the run
created, and deletes the record once all of them are gone.

`mxv list` lists the records it can read, and adds a row for each record it
cannot read, such as one that is not JSON or that a newer mxv wrote. Such a
row names the record without `.json` and says what is wrong in `problem`, and the run
exits with 1.

## Removing a workspace

`mxv remove <name>` removes each checkout, deletes each branch whose commits
are all on its base, removes the workspace directory and `AGENTS.md`, and
deletes the record. It refuses, and changes nothing, in these cases:

- A checkout has uncommitted changes or untracked files. The refusal is
  `mxv::workspace::dirty`, and `--force` discards the changes.
- A checkout holds ignored files outside the disposable set, such as
  `.env`. The refusal is `mxv::workspace::ignored_files`, which lists the
  paths, and `--force` deletes them. The disposable set is each directory
  that git reports as ignored as a whole and whose own name is `target`,
  `node_modules`, `dist`, `build` or `.cache`, such as `target/` or
  `web/node_modules/`, because a build makes those again. mxv deletes them
  without asking and lists them in `disposable`. An ignored file is never
  disposable, even inside a tracked `dist/` directory.
- A checkout's `HEAD` holds commits that no branch, tag or remote branch
  holds, such as commits on a detached `HEAD`. The refusal is
  `mxv::checkout::unreachable_commits`, and `--delete-unmerged` deletes the
  commits. mxv also checks this when the checkout's directory is already
  gone, from the `HEAD` that the repository still holds for it.
- git lists a checkout as locked for a reason other than `initializing`.
  The refusal is `mxv::checkout::locked`, and its hint names the
  `git worktree unlock` command to run when nothing else uses the checkout.
  A lock with the reason `initializing`, which a killed `git worktree add`
  leaves, is removed without asking.

A branch with commits that are not on its base stays, and the result reports
`kept_unmerged` with the number of commits in `unmerged_commits`.
`--delete-unmerged` deletes it and reports `deleted_unmerged`. When the
base that the record names is gone, mxv cannot count the commits, so it
treats the branch as unmerged and reports `unmerged_commits` as null. `--force`
never deletes an unmerged branch. The merge check reads the base as the
repository last fetched it, so a branch that was merged by squashing reads as
unmerged and is kept.

mxv refuses with `mxv::checkout::unowned` when the workspace directory or a
checkout directory is a symbolic link, or when git does not list a checkout
directory as a worktree of its repository.

## Environment

mxv depends on these variables, and `VARIABLES` in `src/environment.rs`
holds the same catalog. mxv finds the base directories with etcetera's base
strategy, as crumbkit does, so an `XDG_*_HOME` variable that is unset, empty
or relative falls back to its default below the home directory.

| Variable | Purpose |
| --- | --- |
| `HOME` | The home directory. Each base directory defaults to a directory in it. When `HOME` is unset or empty, mxv reads the home directory from the user database. |
| `XDG_CONFIG_HOME` | The config directory, which holds `mxv/config.toml`. It defaults to `~/.config`. |
| `XDG_DATA_HOME` | The data directory, which holds the default workspace root `mxv/workspaces`. It defaults to `~/.local/share`. |
| `XDG_STATE_HOME` | The state directory, which holds the records in `mxv/workspaces`. It defaults to `~/.local/state`. |
| `MXV_ROOT` | The workspace root, an absolute path, which wins over the config file. A relative value fails with `mxv::environment::relative_root`. |

When mxv finds no home directory at all, a workspace command fails with
`mxv::environment::no_directory`. mxv passes its environment to git, with
`GIT_TERMINAL_PROMPT=0` so a fetch that needs a password fails instead of
waiting.

## Not built yet

`mxv run`, `mxv archive` and `mxv restore` are declared and fail with
`mxv::command::unbuilt` until the pull request that builds each one.
