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

The config file holds two optional settings:

```toml
root = "~/work"
ports_range = [20000, 29999]
```

`root` is a string that holds an absolute path, or a path that starts with
`~/`, which starts at the home directory. mxv reads `root` only when
`MXV_ROOT` is unset or empty.

`ports_range` is the first and the last host port that `mxv up` allocates
from, and it defaults to `[20000, 29999]`. Both ports must lie from 1024 to
32767, with the first no greater than the last. Ports below 1024 are
privileged, and ports from 32768 up belong to the ephemeral ranges of Linux
and macOS, from which outgoing connections take their local ports. When
every port of the range is held by a workspace or taken, `mxv up` fails
with `mxv::port::exhausted`; a wider range fixes it.

mxv reads the file strictly. Any other key, a value of another type, a
relative path or text that is not TOML fails with `mxv::config::invalid`,
whose fields name the file and the line.

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

## Running an app

A repository declares its app in `mxv.toml` at its root. `mxv up` starts
the processes it declares in each workspace of that repository, in the
background, and returns once each is ready. No mxv process stays running.

```toml
version = 1                                  # optional; a higher version fails

# Named ports. mxv gives each workspace its own free host port for each
# name on the first `mxv up`, from the config file's ports_range, keeps it
# in the workspace's record, and reuses it afterwards.
ports = ["api", "web"]

[processes.api]
argv = ["cargo", "run", "--bin", "api"]      # no shell reads these words
env = { RUST_LOG = "info" }                  # optional extra variables
ports = { PORT = "api" }                     # optional: variable = port name
listen = "api"                               # optional: the port it serves
ready = { timeout_seconds = 120 }            # optional; port defaults to listen

[processes.worker]
argv = ["cargo", "run", "--bin", "worker"]
ports = { API_PORT = "api" }
depends_on = ["api"]                         # optional: start after these are ready

[processes.web]
argv = ["npm", "run", "dev"]
cwd = "web"                                  # optional, relative to the repository root
ports = { PORT = "web", API_PORT = "api" }
listen = "web"                               # ready when web accepts, within 60 seconds
depends_on = ["api"]
```

The schema has these keys, and any other key fails with
`mxv::manifest::unknown_key`, which names the key and its line:

| Key | Meaning |
| --- | --- |
| `version` | The manifest version, which is 1. A higher version fails with `mxv::manifest::newer`. |
| `ports` | The port names, each 1 to 64 letters, digits, `-` and `_`. |
| `processes.<name>.argv` | The program and its arguments, as separate strings. It is required and not empty. |
| `processes.<name>.cwd` | The starting directory, relative to the repository root, which defaults to `.`. It may not leave the repository, also through a symbolic link. |
| `processes.<name>.env` | Variables that the process receives on top of mxv's own environment. |
| `processes.<name>.ports` | Variables that receive a host port, each naming a port of `ports`. |
| `processes.<name>.listen` | The port that the process listens on, which must be one of the ports it receives. A port has at most one listener. |
| `processes.<name>.ready` | `port` and `timeout_seconds`, from 1 to 3600 with a default of 60. `port` defaults to the listen port, and it must be the listen port or a port the process receives. The process is ready when a TCP connection to 127.0.0.1 or `::1` on that port succeeds. |
| `processes.<name>.depends_on` | Processes that start, and are ready, before this one. A cycle fails. |

A listener without a `ready` table is ready when its listen port accepts a
connection within 60 seconds. A process with neither counts as started once
it has run for half a second; when it exits sooner, `up` fails with
`mxv::process::exited`.

The sections `containers`, `secrets` and `actions` are reserved for later
versions and fail with `mxv::manifest::reserved`. A manifest larger than
64 KiB fails with `mxv::manifest::invalid`.

Each process verb works on the workspace that `-w/--workspace` names, or on
the one that holds the current directory when it names none. Its positional
words name processes: `mxv up [-w WORKSPACE] [PROCESS]...`, `mxv down`,
`mxv restart`, `mxv status [-w WORKSPACE]` and
`mxv logs [-w WORKSPACE] PROCESS`.

- `mxv up` starts the named processes, or every process, with their
  dependencies first. It skips a process that already runs. Before it
  starts a listener, it checks that the listen port is free; when it is
  taken, `up` fails with `mxv::port::taken`, whose `holders` field names the
  listener when its own process group still runs, such as a child that it
  left behind, or is empty when a program outside the workspace holds the
  port. Each process runs in its
  own process group, with standard input empty and standard output and
  standard error appended to its log. When a process exits before it is
  ready, `up` fails with `mxv::process::exited` and the log's last 20 lines;
  when it misses its timeout, `up` stops it and fails with
  `mxv::process::not_ready`. A failed `up` stops the processes it started
  before the failure. `up` also drops the ports of names that the manifest
  no longer declares, once no running process received them, so a port
  that the manifest renames stays with its old listener until `down`.
- `mxv down` sends SIGTERM to the process group that a run record names,
  waits up to 10 seconds, then sends SIGKILL. It signals the group only
  when it is the group that `up` started: the process with the recorded id
  has the recorded start time, or no process has that id and a group with
  it still exists. A group keeps its id while any member lives, so in the
  second case the group holds the children that an exited process left
  behind. When another process has taken the recorded id, `down` signals
  nothing and deletes the record. `running`, `remove` and `logs -f` apply
  the same rule. On macOS the start time has a resolution of one second,
  so a process that takes the id within the second that the original
  started in reads as the original. A descendant that leaves its process
  group is not reached.
- `mxv restart` stops the named processes, or every process, and starts them
  again.
- `mxv status` reports each process as `running`, `exited` with its code,
  or `stopped`. A process counts as running only while the process table
  lists its process id with the start time that `up` recorded, so a reused
  id reads as exited. The start time does not depend on the time zone: on
  Linux it is the boot-relative start time in `/proc/<pid>/stat`, and on
  macOS it is what `ps` prints in UTC. The shell records a program that a
  signal ended as code 128 plus the signal's number, so `status` reports a
  code from 129 to 192 as `code` null and `signal` set; a program that
  exits with such a code by itself reads the same way. A process that
  exited with a code other than 0 makes `status` exit with code 1.
  `group_alive` is true for an exited process whose group still has a
  member, which `down` stops.
- `mxv status` and `mxv down` read the run records when the manifest does
  not parse, so a broken manifest never keeps processes running.
- `mxv logs` prints the last 50 lines of a log, or `--lines N`. With `-f`
  it prints each new line, and it ends once the process has exited and no
  member of its group remains, or the log is deleted. A line longer than
  1 MiB comes out in parts.
- `mxv ports reset [-w WORKSPACE] [PORT]...` gives the named ports, or
  every port, new host ports that differ from the old ones. It refuses with
  `mxv::workspace::running` while the workspace's processes run, and it is
  the recovery of `mxv::port::taken` when no process of the workspace holds
  the port.
- `mxv show` lists the host port that the workspace holds for each port.

mxv keeps the run files of each workspace's processes in
`$XDG_STATE_HOME/mxv/runs/<name>/`: `<process>.json` with the process id
and its start time, `<process>.log`, `<process>.log.1`, and
`<process>.exit`, which holds the exit code once the process ends. Each
process runs under `/bin/sh`, which waits for it and writes that code, so a
crash is reported without a long-running mxv.

Each `up` of a process moves its previous log to `<process>.log.1`, which
replaces the log before it, so a process keeps at most two logs. No mxv
process runs between commands, so mxv checks the size of the logs only
when a command runs: `up`, `down`, `status` and `logs` copy a log that
holds more than 32 MiB to `<process>.log.1` and empty it. The process keeps
writing to the emptied log, and lines that it writes between the copy and
the emptying are lost. Between two of those commands a log has no bound,
so a process that writes a lot while nobody runs mxv can fill the disk.

`mxv up`, `down` and `restart`, `mxv ports reset`, and `remove --force`
while processes run, hold an exclusive lock on
`$XDG_STATE_HOME/mxv/runs/<name>/.lock` while they start or stop the
workspace's processes. Each reads the workspace's record again once it
holds the lock, so it uses the ports that a command before it stored. Allocating ports holds a second lock,
`$XDG_STATE_HOME/mxv/runs/.ports.lock`, so two workspaces never take the
same port. A command that finds a lock held tries again for 5 seconds,
then fails with `mxv::workspace::busy`. The operating system releases a
lock when its command ends, so a crashed mxv leaves no lock behind.

## Removing a workspace

`mxv remove <name>` removes each checkout, deletes each branch whose commits
are all on its base, removes the workspace directory and `AGENTS.md`, and
deletes the record. It refuses, and changes nothing, in these cases:

- The workspace's processes run. The refusal is `mxv::workspace::running`,
  and `--force` stops the processes before it removes the workspace.
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

Each refusal's hint names the `mxv remove` command to run after fixing the
cause, and the refusals that a flag overrides also offer that command as an
alternative, such as `mxv remove <name> --force --yes`. Each command keeps
the flags that the refused run gave and has `--yes`, so it runs without a
terminal. After a removal that kept an unmerged branch, the record is gone,
so the result names the `git branch -D` command that deletes the branch; a
dry run names the `mxv remove` command with `--delete-unmerged` instead.

`mxv remove` checks the workspace again after the person confirms, because
the workspace may change while mxv waits for the answer. When a new check
refuses the removal, mxv reports that refusal and removes nothing. When the
removal would lose more than mxv showed, such as a new untracked file under
`--force` or a new commit on a detached `HEAD` under `--delete-unmerged`, it
refuses with `mxv::workspace::changed`, whose recovery is the dry run that
shows the new plan. mxv deletes a branch only while it still points at the
commit that mxv checked. A branch that moved, such as one that gained a
commit while mxv waited, stays, and the result reports `kept_moved`.

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

`mxv archive` and `mxv restore` are declared and fail with
`mxv::command::unbuilt` until the pull request that builds each one.
