# mxv

mxv is the command of [multiverse](../multiverse). It creates task
workspaces that span several git repositories, with one clone for each
repository.

`mxv commands list` lists every command, and `mxv <command> --help`
describes each one. [docs/commands.md](docs/commands.md) is the reference.

## Layout

`mxv new reading-list --repo ~/src/app` fetches the origin of `~/src/app`
into mxv's mirror of it, and creates the workspace directory
`<root>/reading-list`, which is `~/.local/share/mxv/workspaces/reading-list`
by default. That directory holds:

- `AGENTS.md`, which tells an agent which repository and branch the
  workspace holds and how to list, resume and remove it. It sits above the
  checkout, so git never sees it.
- `app/`, a clone of the origin of `~/src/app` on the new branch
  `reading-list`, which starts at the freshly fetched default branch of
  `origin` and tracks no upstream. The default branch is the one that
  `origin/HEAD` names in `~/src/app`, or else `origin/main` or
  `origin/master`, recorded as a full reference such as
  `refs/remotes/origin/main`.

- `mxv.toml`, the workspace manifest, when `mxv new` copied one from
  `--manifest` or from a template, as
  [The workspace manifest](#the-workspace-manifest) describes. It also sits
  above the checkout.

## Several repositories

`mxv new triage --repo ~/src/api --repo ~/src/web` makes one clone of each
repository, at `<root>/triage/api/` and `<root>/triage/web/`, each on the
branch `triage`. `--repo ~/src/api,~/src/web` is the same. The folder of a
checkout is the name of its repository's top-level directory. Two
repositories with the same name would share a folder, so `new` refuses
them with `mxv::checkout::name_taken` and creates nothing, unless `--as`
gives one of them another folder, as in
`--repo ~/src/api --repo ~/old/api --as api-v1`. Each `--as` names the
folder of the `--repo` right before it. A folder cannot be `AGENTS.md` or
`mxv.toml`, which the workspace's own files use, and `new` refuses such a
folder with `mxv::checkout::invalid_name`. When the clone of one
repository fails, `new` deletes every clone, the `AGENTS.md`, the
directory and the record that the run created.

`mxv add -w triage --repo ~/src/docs` adds a clone to an existing
workspace under the same rules, on the workspace's branch, and takes
`--as` as `new` does. `add` without `-w` adds to the workspace that holds
the current directory. `add` refuses a workspace of git worktrees with
`mxv::workspace::worktree_kind`. It also refuses while the workspace's
processes or containers run, because a second checkout renames the first
checkout's processes, containers and ports, as
[Manifests of several checkouts](#manifests-of-several-checkouts)
describes.

`mxv cd triage web` prints the directory of the checkout `web`. It fails
with `mxv::checkout::not_found`, which lists the workspace's folders, when
the workspace has no such checkout.

`AGENTS.md` lists every checkout with its path and its origin. `show`,
`status`, `clean`, `remove` and `cd` cover every checkout. `remove`
refuses unpushed commits, commits on a detached head, stashes, submodule
commits and an origin that cannot be reached in any checkout, and names the
checkout that holds them.

## Clones and mirrors

Each checkout is a clone with a `.git` of its own, so an agent that works
in one workspace cannot change the person's repository or another
workspace. The clone has its own branches, config and hooks. It starts with
no hooks and with git's default config, and its `origin` is the origin of
the person's repository, so `git push` and `git fetch` in the clone go
there and use the person's own credential helper, as any git command does.
`mxv new` only reads the person's repository: it reads the origin's URL,
the default branch and the repository's `.git/info/exclude`, which it
copies into the clone so that a path ignored there, such as `.env`, stays
ignored. It adds no worktree and no branch to the person's repository.

A clone borrows the objects of a bare mirror of the origin through
`git clone --shared`, so a new workspace does not download the whole
history again. mxv keeps one mirror for each origin in
`$XDG_DATA_HOME/mxv/mirrors`, which is `~/.local/share/mxv/mirrors` by
default. A mirror's path below that directory is the origin's host and
path, such as `github.com/acme/app.git`, so an `https` URL and an `ssh` URL
of the same repository share a mirror. The host is in lower case and keeps
its port only when the port is not the scheme's default, so
`ssh://git@github.com:22/acme/app` and `git@github.com:acme/app` share a
mirror. The path keeps its case, because some hosts tell `Acme/App` and
`acme/app` apart, so two spellings of one repository on a host that does
not have two mirrors. An origin that is a path on this machine has a
mirror below `local/`, named by the path with each symbolic link resolved
and with `.git` added, so `/srv/app` and `/srv/app.git` have mirrors of
their own. `mxv new` takes the mirror's
lock, makes the mirror when it does not exist, fetches every branch and tag
of the origin into it, deletes the mirror's branches that the origin no
longer has, and clones it, all under the lock. Another `mxv new` of the same
origin waits up to 150 seconds for the lock, which covers a whole fetch,
and then fails with `mxv::mirror::locked`. A fetch that fails, such as one
whose credentials the origin refuses or one on a machine that is offline,
fails with `mxv::mirror::fetch_failed` and leaves no workspace behind. At
a terminal, `mxv new` prints a line on standard error when it has to wait
for the lock.

A user name and password in an origin URL, such as
`https://someone:token@example.com/app.git`, never appear in mxv's output.
mxv leaves them out of each URL it prints in a failure, in text and in
JSON, and out of each line of git's standard error that it quotes. The
clone's `origin` keeps the URL as the person's repository has it, so
`git push` in the clone works as it does there.

A clone breaks when its mirror loses an object that the clone borrows. mxv
therefore sets `gc.auto=0`, `gc.pruneExpire=never` and
`maintenance.auto=false` on each mirror, never runs `git gc` or
`git prune` in it, and never deletes a mirror, not even when `mxv remove`
removes the last workspace that uses it. A mirror holds only the origin's
branches and tags, so it grows only as the origin grows. Never run
`git gc`, `git prune`, `git repack -d` or `git maintenance` inside
`~/.local/share/mxv/mirrors` yourself. A command such as
`git gc --prune=now` deletes objects that no branch of the mirror holds
but that a clone still borrows, such as the commits of a branch that the
origin deleted, and every clone that borrows them breaks.

A workspace that an mxv before clones made holds a git worktree of the
person's repository instead. Its record has no `kind`, and `mxv show
--long` reports its `kind` as `worktree`. Every command keeps working on
such a workspace as it did, and `mxv remove` removes it as
[Removing a workspace](#removing-a-workspace) describes for worktrees. mxv
does not convert it to a clone.

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

The config file holds these optional settings:

```toml
root = "~/work"
ports_range = [20000, 29999]
disposable = ["coverage/"]          # more disposable globs for every repository
clone_on_new = ["node_modules/"]    # the ignored directories that `mxv new` clones
clean_idle = "7d"                   # the idle age that `mxv clean` waits for
max_disposable = "50GiB"            # the limit that `mxv clean --over-limit` cleans down to
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

[Disk space](#disk-space) describes `disposable`, `clone_on_new`,
`clean_idle` and `max_disposable`.

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
`~/.local/state/mxv/workspaces/<name>.json`. A record of a workspace of
clones has the `kind` `clone`, and names each clone's mirror in `mirror`. A
record of a workspace of worktrees has no `kind`. Every record also holds
the workspace's stable `id`, 32 lowercase hex digits that mxv mints when it
creates the workspace and never changes, and that labels the workspace's
docker objects. A record that holds an `id` has version 3, so an mxv from
before stable ids refuses it as one that a newer mxv wrote, rather than
rewriting it without the id. A record of a workspace with two or more
checkouts has version 4. An mxv from before several repositories refuses
it with `mxv::record::newer` and says that the record has version 4, which
that version of multiverse cannot read, so it never acts on the first
checkout alone. A workspace of one checkout keeps version 3. mxv still
reads the records that earlier versions wrote, version 1 for worktrees and
version 2 for clones, and gives such a record an id the first time it
writes it. `mxv new` writes the record before it
makes the checkout, so a run that stops partway leaves a record that
`mxv remove` cleans up. When the fetch, the clone or the checkout fails,
`mxv new` deletes the clone, the `AGENTS.md` and the directory that the run
created, and deletes the record once all of them are gone. `mxv new`
refuses with `mxv::branch::exists` when the person's repository already has
a local branch with the workspace's name.

`mxv list` lists the records it can read, and adds a row for each record it
cannot read, such as one that is not JSON or that a newer mxv wrote. Such a
row names the record without `.json` and says what is wrong in `problem`, and the run
exits with 1.

## Running an app

A repository declares its app in `mxv.toml` at its root, or a workspace
declares it in its own workspace manifest, as
[The workspace manifest](#the-workspace-manifest) describes. `mxv up` starts
the processes and containers it declares in each workspace of that
repository, in the background, and returns once each is ready. No mxv
process stays running.

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
env = { API_URL = "http://127.0.0.1:{port.api}" }   # {port.<name>} is a host port
ready = { log = "^worker ready", timeout_seconds = 300 }   # ready on a matching log line
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
| `processes.<name>.env` | Variables that the process receives beside the ones that [The environment of a host process](#the-environment-of-a-host-process) lists. A value may name a host port as `{port.<name>}`, where the name is a port of `ports`, and writes a literal brace as `{{` or `}}`. Any other brace, or a placeholder of an undeclared port, fails with `mxv::manifest::invalid` and the value's line. `status` lists such a port as `{port.api}=<number>`. |
| `processes.<name>.ports` | Variables that receive a host port, each naming a port of `ports`. |
| `processes.<name>.listen` | The port that the process listens on, which must be one of the ports it receives through `ports` or an `env` placeholder. A port has at most one listener. |
| `processes.<name>.ready` | `port` or `log`, and `timeout_seconds`, from 1 to 3600 with a default of 60. `port` defaults to the listen port, and it must be the listen port or a port the process receives; the process is ready when a TCP connection to 127.0.0.1 or `::1` on that port succeeds. `log` is a regular expression; the process is ready when it writes a log line that matches, after this `up` started it. A table may not set both. |
| `processes.<name>.depends_on` | Processes that start, and are ready, before this one. A cycle fails. |
| `processes.<name>.permissions` | Runs the process under phase's sandbox, with the grants of `write`, `listen` and `connect`. An empty table runs it sandboxed with no grants, and a process without the table runs unsandboxed. [Sandboxed processes](#sandboxed-processes) describes it. |
| `processes.<name>.permissions.write` | Paths that the process may write: `$STATE`, the process's own state directory, or a path relative to the repository root without `..`, such as `target`. An absolute path, a path with `..`, the root itself, a path inside `.git` in any case, and any other `$` name fail with `mxv::manifest::invalid` and the path's line. |
| `processes.<name>.permissions.listen` | Ports that the process may listen on. Each is its `listen` port or a port it receives, and a process with a `listen` port must name it here. A port that another process listens on or a container publishes fails with `mxv::manifest::invalid` and its line. |
| `processes.<name>.permissions.connect` | Processes and containers that the process may connect to. Each must be a unit that the process depends on through `depends_on`. |
| `disposable` | Globs of more disposable directories of this repository, such as `["web/test-results/"]`. Each glob must end with `/` and have a literal last name. [Disk space](#disk-space) describes them. |
| `clone_on_new` | Globs of more ignored directories that `mxv new` clones from this repository, such as `["web/.venv/"]`, under the same rule. |

A listener without a `ready` table is ready when its listen port accepts a
connection within 60 seconds. A process that listens on no port, such as a
worker that reads a queue, can still tell `up` when it is ready: give it
`ready = { log = "<pattern>" }` and have it print a line that the pattern
matches once it has connected to what it needs. The line must end with a
line break. When no line matches within the timeout, `up` stops the
process and fails with `mxv::process::not_ready`, whose `pattern` field
holds the pattern in place of `port` and `number`. A process with neither counts as started once
it has run for half a second; when it exits sooner, `up` fails with
`mxv::process::exited`.

The sections `secrets` and `actions` are reserved for later versions and
fail with `mxv::manifest::reserved`. A manifest larger than 64 KiB fails
with `mxv::manifest::invalid`.

Another tool may keep its own tables in the same file. mxv checks that the
whole file is valid TOML, reads only its own top-level keys (`version`,
`ports`, `disposable`, `clone_on_new`, `processes` and `containers`), and
leaves every other top-level table alone, such as `[app]`,
`[service.web]`, `[needs.db]`, `[release]` or `[[steps]]`. An unknown
top-level key that is not a table, such as `port = ["api"]`, fails with
`mxv::manifest::unknown_key`. A table named like one of mxv's own, such as
`[process.api]`, belongs to another tool and is left alone, so a
misspelling of `[processes.api]` declares no process. Inside mxv's own
tables, such as
`[processes.api]`, an unknown key still fails with
`mxv::manifest::unknown_key`.

### Containers

A manifest may declare containers beside its processes. mxv runs them with
the `docker` program on `PATH`. This manifest runs DynamoDB Local and
LocalStack for an `api` process:

```toml
ports = ["api", "dynamodb", "localstack"]

[containers.dynamodb]
image = "amazon/dynamodb-local:2.5.4"        # pinned by a tag or a digest
command = ["-jar", "DynamoDBLocal.jar", "-sharedDb", "-dbPath", "/data"]
ports = { 8000 = "dynamodb" }                # container port = port name
volumes = { data = "/data" }                 # volume name = mount path

[containers.localstack]
image = "localstack/localstack:4.0.3"
env = { SERVICES = "s3,sqs" }
ports = { 4566 = "localstack" }
ready = { timeout_seconds = 120 }

[processes.api]
argv = ["cargo", "run", "--bin", "api"]
ports = { PORT = "api", DYNAMODB_PORT = "dynamodb", LOCALSTACK_PORT = "localstack" }
listen = "api"
depends_on = ["dynamodb", "localstack"]
```

| Key | Meaning |
| --- | --- |
| `containers.<name>.image` | The image. It is required, and it must be pinned by a tag other than `latest` or by a digest. |
| `containers.<name>.command` | Arguments that replace the image's command. It is optional. |
| `containers.<name>.env` | Variables that the container receives. |
| `containers.<name>.ports` | The TCP ports that mxv publishes: each key is a container port from 1 to 65535, and each value names a port of `ports`. A container that publishes a port is that port's listener, and a port has at most one listener, process or container. |
| `containers.<name>.volumes` | Named volumes: each key names a volume, and each value is the absolute path it mounts at inside the container. mxv mounts no directory of the host. |
| `containers.<name>.ready` | `port` and `timeout_seconds`, as for a process. `port` must be a port that the container publishes, and it defaults to that port when the container publishes exactly one. A container's readiness check waits only for a port, so `log` is invalid here, and a container's `env` values hold no port placeholders. |
| `containers.<name>.depends_on` | Processes or containers that start, and are ready, before this one. |

A container takes no `permissions`, because Docker runs it and phase's
sandbox holds only host processes. `permissions` under `[containers.*]`
fails with `mxv::manifest::invalid` and its line.

Processes and containers share one set of names, and either may depend on
the other. A process receives a container's host port through its own
`ports`, as `api` receives `DYNAMODB_PORT` above.

mxv runs a workspace's app with phase, the library in `crates/phase`.
Each process and each container of the manifest is one phase app, named
after the unit, and each app has one process, named `main`. A unit's
`depends_on` becomes its app's dependencies. A unit name must therefore be
a name phase accepts: 1 to 64 lowercase letters, digits, `-` and `_`,
starting with a letter or a digit. A manifest with any other unit name fails
with `mxv::manifest::unit_name`, and one that breaks another of phase's plan
rules fails with `mxv::manifest::unrunnable`, in both cases before anything
starts.

phase keys a workspace by the stable id in its record, which `mxv new`
mints and `mxv up` adds to a record that an earlier mxv wrote. phase keeps
its run record and the logs of host processes under
`$XDG_STATE_HOME/mxv/phase`. A host process's log is
`$XDG_STATE_HOME/mxv/phase/logs/<id>/<unit>.main.log`.

#### Sandboxed processes

A process whose manifest has a `permissions` table runs under phase's
sandbox, which is Seatbelt on macOS. A process without the table runs
unsandboxed, as every process did before. The sandbox lets a process read
the system's directories, the toolchains on `PATH` and its checkout, and
write its own state and temporary directories. `permissions` adds:

- `write`: the directories that the process may write, relative to the
  repository root, such as `target`. phase creates a missing directory
  when it starts the process. `$STATE` names the process's own state
  directory, which it may always write. The checkout's `.git` stays
  read-only, so a path inside it fails.
- `listen`: the ports that the process may listen on, on the loopback
  address. A process with a `listen` port must name it here, and the
  manifest fails without it. A port that another process listens on or a
  container publishes fails, so a process cannot take a dependency's port.
- `connect`: the processes and containers that the process may connect to
  over the loopback address.

phase's sandbox opens the ports of every unit that a process depends on,
and no other. A unit of `connect` must therefore be one of the process's
`depends_on`, and the manifest fails with `mxv::manifest::invalid` and a
hint to add it to `depends_on` otherwise. A sandboxed process can also
reach each unit of its `depends_on` that `connect` leaves out. A sandboxed
process cannot reach any host off the machine until phase runs its
allowlist proxy, so a name in `connect` that is not a unit of the manifest
fails. `permissions` has no `secrets` key, because mxv has no secrets yet,
and that key fails with `mxv::manifest::unknown_key`.

A write grant is only as safe as whatever later runs the directory's
contents outside the sandbox. For example, a sandboxed process that may
write `target` can leave a build script there, and an unsandboxed
`cargo build` then runs it.

A change to a process's `permissions` takes effect when the process
restarts. `mxv up` leaves a running process as it is, so a process that
was running when its manifest gained or lost its `permissions` table keeps
running as it started. `up` then prints a `warning:` line under that
process, which names the `mxv restart` that applies the change. `up` cannot
see a change to the grants of a process that already runs sandboxed, so
restart such a process after you change its grants.

This manifest runs four sandboxed processes beside two containers:

```toml
ports = ["auth", "api", "web", "dynamodb", "localstack"]

[containers.dynamodb]
image = "amazon/dynamodb-local:2.5.4"
ports = { 8000 = "dynamodb" }

[containers.localstack]
image = "localstack/localstack:4.0.3"
ports = { 4566 = "localstack" }

[processes.auth]
argv = ["cargo", "run", "--bin", "auth"]
ports = { PORT = "auth" }
listen = "auth"
permissions = { write = ["target"], listen = ["auth"] }

[processes.server]
argv = ["cargo", "run", "--bin", "server"]
ports = { PORT = "api", AUTH_PORT = "auth", DYNAMODB_PORT = "dynamodb", LOCALSTACK_PORT = "localstack" }
listen = "api"
depends_on = ["auth", "dynamodb", "localstack"]
permissions = { write = ["target"], listen = ["api"], connect = ["auth", "dynamodb", "localstack"] }

[processes.worker]
argv = ["cargo", "run", "--bin", "worker"]
ports = { DYNAMODB_PORT = "dynamodb", LOCALSTACK_PORT = "localstack" }
ready = { log = "^worker ready" }
depends_on = ["dynamodb", "localstack"]
permissions = { write = ["target"], connect = ["dynamodb", "localstack"] }

[processes.web]
argv = ["pnpm", "run", "dev"]
cwd = "web"
ports = { PORT = "web", API_PORT = "api", AUTH_PORT = "auth" }
listen = "web"
depends_on = ["server", "auth"]
permissions = { write = ["web/node_modules/.vite", "web/dist"], listen = ["web"], connect = ["server", "auth"] }
```

The `sandbox` column of `up`, `down`, `restart` and `status` shows how
phase runs each host process: `seatbelt (partial: ...)` on macOS, where
the sandbox cannot limit the process's listening ports to the loopback
interface, so other machines on the network can connect to them, or `unsandboxed (<reason>)`, such as
`unsandboxed (its app declares no permissions)`. It is null for a
container. A process that phase started is shown as its run record
recorded it.

#### Workspaces that an earlier mxv started

An earlier mxv ran processes and containers itself and kept run files under
`$XDG_STATE_HOME/mxv/runs/<workspace>`. mxv still reads those files, so
that a workspace started before the change keeps working until it is
stopped:

- `mxv status` and `mxv logs` report the processes and containers that the
  run files name, as the earlier mxv did.
- `mxv down` stops them under the earlier mxv's rules, and deletes the run
  files once nothing they name runs.
- `mxv up` and `mxv restart` refuse with `mxv::workspace::legacy_running`
  while anything that the run files name may still run, and the recovery
  runs `mxv down -w <workspace>`. When everything they name has ended,
  `up` deletes the run files and starts the workspace under phase.
- A named volume that the earlier mxv created, `mxv-<id8>-<workspace>.<volume>`,
  is not taken over. When it exists and phase's volume of that name does
  not, `up` starts nothing and fails with `mxv::volume::not_moved`. Its
  hint gives the commands that create phase's volume with the labels phase
  gives it and copy the data into it, or that delete the old volume when
  its data is not needed.

#### The environment of a host process

phase starts each host process from an empty environment. The process
receives `PATH` and `HOME` from the environment that `mxv` itself was run
with, its manifest `env`, its port variables, and these variables of mxv's
environment when they are set: `LANG`, `LANGUAGE`, `LC_ALL`, the twelve standard `LC_` categories, such as
`LC_CTYPE` and `LC_TIME`, `TZ`,
`TERM`, `COLORTERM`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR`, the `XDG_*`
directories, `CARGO_HOME`, `RUSTUP_HOME`, `RUSTUP_TOOLCHAIN`, `NVM_DIR`,
`PNPM_HOME`, `COREPACK_HOME`, `NPM_CONFIG_PREFIX`, `NODE_EXTRA_CA_CERTS`,
`NODE_USE_SYSTEM_CA`, `SSL_CERT_FILE`, `SSL_CERT_DIR`, `MISE_DATA_DIR`,
`MISE_CONFIG_DIR`, `MISE_CACHE_DIR`, `SSH_AUTH_SOCK`, `RUST_LOG` and
`RUST_BACKTRACE`. No other variable of mxv's environment reaches it. A
credential or a build override such as `RUSTFLAGS` that is exported in the
shell does not reach the process, and a process that needs one has to set
its value in its manifest `env`. A manifest may not set `PATH` or `HOME`,
which fails with `mxv::manifest::unrunnable`.

The plan that phase runs declares every variable above for each host
process and marks it optional, so the plan is the same whichever shell runs
`mxv up`. An `up` from a shell with other variables leaves running
processes as they are, and `mxv restart` starts them with the variables of
the current shell. A container receives only its manifest `env`.
The decision record
[host-process-environment.md](docs/decisions/host-process-environment.md)
says why mxv passes these variables and what follows from it.

#### Containers

phase runs each container as a Docker Compose project of its own, so it
needs docker's `compose` plugin; without it `up` fails with
`mxv::docker::compose_missing`. The project of the container `<unit>` is
`phase-<hash>-<unit>`, where `<hash>` is 16 hex digits of a hash of the
workspace's stable id, and its container is `phase-<hash>-<unit>.main`. The
named volume `<volume>` of a workspace is `phase-<hash>.<volume>`, which
every container of the workspace that names it shares. phase labels each
container and volume with `mxv.workspace-id=<id>` and the path of its run
record in `mxv.run-record`, a container also with `mxv.app=<unit>`, and a
volume with `mxv.volume=<volume>`. phase writes no compose file: a value
such as a token is only in the environment of the `docker compose` process
and in the container. phase publishes each port on 127.0.0.1 only, and
every container runs on docker's default bridge network, so a container
reaches another container, or a process on the host, through that one's
published port on the host's 127.0.0.1.

Before it stops, removes or reuses a container or volume, phase reads the
object's labels, and when they name another workspace it leaves the object
alone and `up` fails with `mxv::docker::foreign_object`. `status`, `down`
and `logs` run docker only for a workspace whose run record holds a
container, and `remove` only for a workspace that phase has run a container
of, so a missing docker or a daemon that does not answer never stops a
workspace that has never run a container.

- `mxv up` pulls a missing image, starts the container, and waits for its
  readiness port. Docker's port proxy accepts a connection before anything
  in the container listens, so phase counts the port as answering only
  when a connection stays open for 0.3 seconds or the container sends data.
  When the container stops first, `up` fails with `mxv::process::exited`
  and its exit code; when it misses its timeout, `up` fails with
  `mxv::process::not_ready`. Either way, and after any other failure, `up`
  removes the containers and stops the processes it started. phase replaces
  a running container whose image, command, ports or variable names
  changed. A change to a variable's value alone waits for `mxv restart`.
- `mxv down` stops each container, which sends SIGTERM and then SIGKILL
  after the grace, and removes it. Its named volumes stay, with their data,
  until `mxv remove --delete-volumes` deletes them.
- `mxv status` reports a container's state as docker reports it, with
  `kind` set to `container` and `pid` and `log` null. An exited container's
  `code` is the code that docker reports, such as 137 for a container that
  SIGKILL ended, and its `signal` is null. When docker does not answer, the
  state is `unknown` and `reason` says why.
- `mxv logs` prints `docker logs` of the container, its standard output and
  standard error together. When the container has no log, `logs` prints
  nothing and a warning on standard error that says why, and exits 0: the
  container has not started yet, `down` has stopped and removed it, docker
  no longer has it, as after a removal by hand, or phase's run record
  names it by a name or id that phase cannot use. Without `-f`, `logs`
  stops docker and fails with `mxv::docker::failed` when docker prints
  nothing for 30 seconds.
- docker prints its own errors on standard error among the container's
  lines. mxv recognises only the lines that start with
  `Error response from daemon:`, `Error: No such container:`,
  `error from daemon in stream:` or `Cannot connect to the Docker daemon`.
  When docker exits with code 1 just after such a line, the line becomes
  the message of `mxv::docker::failed` and is not printed as a line of the
  container. Otherwise the line is printed as the container's, at most
  0.25 seconds late. Any other failure of `docker logs`, such as a signal
  that ended it, gives its exit status as the message, so a line that the
  container wrote becomes the message only when it starts like one of
  those lines and docker then exits with code 1.

When a workspace needs docker and no `docker` program is on `PATH`, its
command fails with `mxv::docker::not_found`. When docker's daemon does not
answer, `up`, `down` and `remove --force` fail with
`mxv::docker::unavailable`, so they never report a workspace as stopped
while its containers may still run. A docker command that fails, such as a
pull of an image that does not exist, fails with `mxv::docker::failed`,
whose fields name the operation, the container, and docker's message. A
failure of phase that mxv has no code for fails with `mxv::phase::failed`,
whose `phase_code` field holds phase's code.

#### The process verbs

Each process verb works on the workspace that `-w/--workspace` names, or on
the one that holds the current directory when it names none. Its positional
words name processes: `mxv up [-w WORKSPACE] [PROCESS]...`, `mxv down`,
`mxv restart`, `mxv status [-w WORKSPACE]` and
`mxv logs [-w WORKSPACE] PROCESS`.

- `mxv up` starts the named processes, or every process, with their
  dependencies first. It skips a process that already runs. phase starts
  each host process under a keeper, a small `mxv` process that runs the
  process in a process group of its own, with standard input empty and
  standard output and standard error appended to its log, and stops what
  is left of the group when the process ends. Before it starts a listener,
  phase checks that the listen port is free; when it is taken, `up` fails
  with `mxv::port::taken`. When a process exits before it is ready, `up`
  fails with `mxv::process::exited` and the log's last lines; when it
  misses its timeout, `up` stops it and fails with
  `mxv::process::not_ready`. When the readiness port answers and a process
  outside the process's group holds it, `up` stops the process and fails
  with `mxv::process::foreign_listener` and that program's process id. A
  failed `up` stops the processes it started before the failure. `up` also
  drops the ports of names that the manifest no longer declares, once no
  process of the workspace runs, so a port that the manifest renames stays
  with its old listener until `down`.
- `mxv down` stops each process's group with SIGTERM, waits up to 10
  seconds, then sends SIGKILL. When phase cannot prove that a group is the
  one it started, it signals nothing and `down` fails that process with
  `mxv::process::unstoppable`. When `up` meets such a group while it stops
  what it started, it fails with the same error, starts nothing more, and
  names the units it did not start.
- `mxv restart` stops the named processes, or every process, and starts them
  again.
- `mxv status` reports each process as `running`, `starting`, `stopping`,
  `restarting`, `exited`, `stopped`, or `unknown` with a `reason`. An
  exited process has its exit `code`, or the number of the `signal` that
  ended it and a null `code`. A process that runs a shell, such as
  `sh -c`, records a program that a signal ended as code 128 plus the
  signal's number, so `status` reports a code from 129 to 192 as `code`
  null and `signal` set; a program that exits with such a code by itself
  reads the same way. When the process ended while its keeper was not
  running, as after something killed the keeper or the machine
  restarted, both are null and `reason` says why. An exited process
  whose `code` is anything but 0, including null, makes `status` exit with
  code 1. `group_alive` is null for every process that phase runs.
  `ready` is true for a running process once its readiness check has
  passed since it last started, or once a process without one has outlasted
  its start grace, and false while it runs and has not, such as after its
  keeper or docker restarted it. `sandbox` says whether phase runs a
  host process under its sandbox, as
  [Sandboxed processes](#sandboxed-processes) describes.
- `mxv up` and `mxv restart` print a line under a container whose secret
  files they found running: `info:` when `up` rewrote them, and `warning:`
  when it could not, so the container still runs with its earlier files.
  mxv's manifest declares no secrets yet, so neither line appears until it
  does. `mxv up` also prints a `warning:` line under a running host
  process whose manifest gained or lost its `permissions` table since it
  started, as [Sandboxed processes](#sandboxed-processes) describes. The
  JSON holds these lines in each row's `notes`.
- `mxv status` and `mxv down` read phase's run record when the manifest does
  not parse, so a broken manifest never keeps processes running.
- `mxv logs` prints the last 50 lines of a log, or `--lines N`. With `-f`
  it prints each new line until the process has exited. A line longer than
  1 MiB comes out in parts. An unfinished last line counts as one of the
  lines, and `-f` prints it once it is finished, also with `--lines 0`. An
  unfinished last line longer than 64 KiB loses its start, so `-f`, even
  with `--lines 0`, starts in the middle of it.
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
crash is reported without a long-running mxv. The shell's redirection
creates the exit file empty before `printf` writes the code, so mxv counts
the file as written only once it ends in a line feed. The shell writes the
file before it exits, so `status` reads the file once more after it finds
the shell gone. A shell that something such as SIGKILL ended writes no
file, and `status` then reports the process as exited with a null code and
signal and a `reason` that says the shell ended without writing its code.

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

## The workspace manifest

A workspace can hold a manifest of its own, the workspace manifest, at
`<root>/<name>/mxv.toml`, beside `AGENTS.md` and above the checkout. It has
the same schema as a repository's `mxv.toml`, and its paths, such as a
process's `cwd` or a `disposable` glob, are relative to the checkout. It
lets a person run an app that the repository declares no manifest for, or
declares differently, without changing the repository.

`mxv new` writes the workspace manifest from the first source that applies:

1. `--manifest <FILE>`, which it copies in. The file must parse as a
   manifest, or `new` fails with the manifest's code, such as
   `mxv::manifest::invalid`, and creates nothing. The file must also be
   the person's own, as the next section describes.
2. The template in `$XDG_CONFIG_HOME/mxv/templates/*.toml`, which is
   `~/.config/mxv/templates` by default, whose `[template]` table lists the
   repository's origin.
3. Nothing, and the workspace reads the repository's `mxv.toml`, if any.

The result's `manifest_from` names the file that `new` copied, and
`manifest` says which manifest the workspace reads. A dry run reports the
same and writes nothing.

Every command afterwards reads exactly one manifest, from the first file
that exists:

1. the workspace manifest;
2. otherwise, the `mxv.toml` at the root of the checkout;
3. otherwise, none, and the workspace is bare.

mxv never merges the two files. A workspace manifest that does not parse
fails as any manifest does, and mxv does not fall back to the checkout's
file. `list --long`, `show --long` and `status` report the manifest that
the workspace reads in `manifest`, as `workspace`, `repository` or `none`,
and its path in `manifest_path`. `status` also prints it above the
processes.

mxv reads only its own top-level keys and leaves every other table to the
tool that owns it. A table whose name is close to one of mxv's own, such as
`[proceses.web]`, `[Processes.web]`, `[container.db]` or `[disposible]`,
would declare nothing, so mxv warns about it: `up` and `restart` print the
warning on standard error, `status` puts it in the summary's `warnings`,
and `show` and `new` put it in the row's `warnings`. phase's own tables,
`[app]`, `[release]`, `[build]`, `[needs.*]` and `[service.*]`, get no
warning. phase's `[process.*]` gets a warning that mxv runs only
`[processes.*]`. A top-level `[permissions]` table would sandbox nothing,
because mxv sandboxes each process on its own, so it fails with
`mxv::manifest::invalid` and a hint to move its keys to
`[processes.<name>.permissions]`. `up` also warns when the manifest declares
no processes and no containers, and starts nothing.

A workspace manifest that is a symbolic link leading nowhere cannot be
read. `up` fails with `mxv::manifest::unreadable`, which names the link and
where it leads. `status` reports the same error and its code in
`manifest_error` and `manifest_error_code`, and `show` reports it in
`warnings`. mxv never falls back to the checkout's file.

`mxv remove` deletes the workspace manifest with the workspace. A person
may have edited it by hand, so the question, the dry run and the text
result name it, and the JSON result gives its path in `manifest_deleted`.

### Manifests of several checkouts

A workspace with one checkout reads one manifest, as the section above
describes. A workspace with two or more checkouts merges the workspace
manifest with the `mxv.toml` at the root of each checkout, and `manifest`
reports `merged`, with every file it read in `manifest_paths`.

- Each checkout's file has the checkout's folder put in front of every
  name it declares, so the process `server` of the checkout `api` is
  `api-server`, its port `http` is `api-http`, and the volume `data` of
  its containers is `api-data`. Each reference inside the file, such as
  `depends_on` or `{port.http}`, follows the new name. The prefix is the
  folder in lower case, with each character other than a letter, a digit,
  `-` or `_` written as `-`. Because the prefix starts names, a folder in
  a workspace of several checkouts follows the rule of `--as` even when
  it is the repository's name, so a repository named `_infra` needs
  `--as infra`. `mxv add` to a workspace of one checkout checks only that
  the checkout's prefix starts with a letter or a digit, so it accepts
  `MyApp`, whose prefix is `myapp`, and refuses `_infra` with
  `mxv::checkout::invalid_name`. mxv cannot rename the folder, so the hint
  says to create the workspace again with `mxv new` and `--as`.
- The prefixes appear only once a workspace has two checkouts. Adding a
  second repository with `mxv add` therefore renames the first
  repository's processes, containers, ports and volumes. `add` moves the
  host ports that the workspace holds to the new names, so `mxv up` gives
  each process the port it had before. The first repository's containers
  mount volumes under the new names, such as `api-data` in place of
  `data`. So that no data is left behind in the old volume, `add` refuses
  with `mxv::volume::renamed` while such an old volume exists, before it
  clones anything. It checks only when the workspace reads the first
  repository's `mxv.toml`; a workspace manifest keeps its volume names. Its hint gives the commands that copy each old
  volume's data into the new one and delete the old one; deleting the old
  volume alone drops its data.
- The workspace manifest keeps its names as written, and its units may
  depend on the checkouts' prefixed names and use their prefixed ports.
  Its paths, such as a process's `cwd` or a `disposable` glob, are
  relative to the workspace directory, so a process that runs in the
  checkout `api` has `cwd = "api"`. A write path of a process's
  `permissions` is relative to the workspace directory too, and it must
  lie inside the checkout that the process runs in, so that process
  writes `api/target`. A write path that lies in no checkout, or that
  names a whole checkout, such as `api`, fails with
  `mxv::manifest::invalid` and its line.
- The `listen` ports and `connect` units of a checkout's `permissions`
  follow the new names too, and its write paths stay relative to its own
  checkout.
- Two names that are the same once prefixed, such as the process `b-c` of
  the checkout `a` and the process `c` of the checkout `a-b`, or two
  volumes of the same name in two files, fail with
  `mxv::manifest::name_clash`, which names both files. `up` fails with
  it, `status` reports it in `manifest_error`, and `show` reports it in
  `warnings`. A name longer than
  64 bytes once prefixed fails with `mxv::manifest::invalid`.

The workspace manifest of a one-checkout workspace has paths relative to
that checkout, which would point at the wrong directory once the workspace
has two, and mxv cannot tell which directory each path means. `mxv add`
therefore refuses such a workspace with `mxv::manifest::needed` unless
`--manifest <FILE>` gives the workspace manifest that the workspace reads
afterwards, with paths relative to the workspace's directory, such as
`cwd = "app"` in place of `cwd = "."`. `add` clones the new checkout,
checks the merged manifest with that file, and only then copies it into
the workspace. When the check fails, `add` deletes the new clone, changes
nothing else, and fails with the manifest's error. mxv never changes the
file that `--manifest` names.

`mxv add` writes the record with the new checkout before it clones. When
a run is killed partway, running the same `mxv add` again resumes it: it
clones again in place of a partial clone, keeps a clone that reached the
workspace's branch, and refuses with `mxv::checkout::resume_blocked`
when the directory holds work: a stash, a commit that origin lacks, an
uncommitted change or untracked path, or an ignored path outside its
disposable set, such as `.env`. Any other `mxv add` to that workspace,
and `up`, `restart`, `ports reset` and `clean`, fail with
`mxv::checkout::incomplete`, whose hint gives the command that resumes
the unfinished checkout. `status`, `show`, `down`, `logs` and `remove`
keep working. When a resumed `add` fails, it restores the workspace's
earlier record and deletes the new clone, unless the clone finished and
holds work; then it keeps both and fails with
`mxv::checkout::resume_blocked`, which names the work.

`mxv new` copies no template into a workspace of several repositories,
because a template belongs to one origin. `--manifest` still copies a
file, whose names are the workspace's own. `new` checks it against the
merged manifest of the new clones, and deletes the workspace when the
check fails.

The workspace manifest stays the only file that mxv trusts as the
person's own, as the next section describes. A checkout's `mxv.toml`
comes from its repository.
[docs/decisions/multi-repository-manifests.md](docs/decisions/multi-repository-manifests.md)
records the reasons. `mxv up` runs every process of the merged manifest
through phase, each from the checkout that holds its directory, so the
process `api-server` starts in `<root>/<name>/api/`.

`mxv status` lists each checkout in its summary's `checkouts`, with the
checkout's folder, its path, the branch and commit it has checked out, and
how many uncommitted changes or untracked paths it holds. The text output
prints one line for each checkout below the manifest's lines.

### Trusting the workspace manifest

The globs in the workspace manifest's `disposable` count as the person's
own, as the config file's do. `remove` and `clean` delete the paths they
match without `--force`, and `show` gives them the source `workspace`. They
follow the manifest's rules for globs, so each names a directory, such as
`data/`, and none may name `.git`. The globs of the repository's `mxv.toml`
keep the rule that [Disposable paths](#disposable-paths) describes, but only
while the workspace has no workspace manifest, because the checkout's file
is not read then.

mxv trusts the workspace manifest because only the person writes it. mxv
copies the file into `<root>/<name>/` itself, from a file that the person
named or from the person's templates, and nothing in the checkout writes
there. **A sandbox that runs agents in a checkout must not let them write
the workspace's directory, `<root>/<name>/`.** That is a requirement on the
sandbox. An agent that could write `<root>/<name>/mxv.toml` could make mxv
delete any ignored directory of the checkout without asking.

mxv checks the rest itself:

- A workspace manifest that is a symbolic link, or a hard link of another
  file, could be the repository's own `mxv.toml`. mxv still reads it, but
  its `disposable` globs count as the repository's, so their paths wait
  for a confirmation at the prompt or `--force`, and `remove` and `show`
  warn about the link.
- `--manifest` refuses a file that lies, once symbolic links are resolved,
  inside a git work tree or under the workspace root, with
  `mxv::manifest::untrusted`, because a repository or an agent could have
  written it. That includes the repository's own `mxv.toml` and a file in
  another workspace's checkout. Copy such a file to a file of your own
  first, such as a template.
- A template must be a regular file, never a symbolic link, and must not
  lie under the workspace root. A template in a git repository of your
  own, such as a repository of dotfiles that holds `~/.config`, is
  allowed, because it is not a checkout that an agent works in.
- A record whose checkout is the workspace's directory, or holds the path
  `<root>/<name>/mxv.toml`, has no workspace manifest, because the
  repository writes that path.

### Templates

A template is a manifest with one more top-level table, `[template]`, that
lists the origins it is for. A key after a table header belongs to that
table, so the manifest's own top-level keys come before every table, and
`[template]` usually comes last:

```toml
ports = ["web"]

[processes.web]
argv = ["npm", "run", "dev"]
ports = { PORT = "web" }

[template]
origins = ["github.com/acme/app", "git@gitlab.example.com:team/app.git"]
```

An origin is a URL that git accepts, such as `https://github.com/acme/app`
or `git@github.com:acme/app.git`, or a host and path without a scheme, such
as `github.com/acme/app`. In an origin without a scheme, digits after the
colon are a port, so `github.com:443/acme/app` is
`https://github.com:443/acme/app`, and anything else after the colon is
git's scp-like form, as in `github.com:acme/app`. mxv compares origins by
the key of their mirror, as [Clones and mirrors](#clones-and-mirrors)
describes, so the `https` and `ssh` forms of one repository match. The
host is compared in lower case and the path as written, on every host, so
`github.com/Acme/app` does not match `github.com/acme/app`.

`[template]` holds only `origins`. When mxv copies the template, it removes
the table's header and keys, from the header to the next table header, and
keeps every comment and blank line, so the workspace manifest holds the
rest of the file as it was written.

`mxv new` reads every `*.toml` file in the directory, in name order, and
ignores other files. mxv never copies a template that it cannot use: one
that is not valid TOML, has no valid `[template]` table, writes the table
inline as `template = { ... }`, is not a valid manifest without the table,
is a symbolic link, or lies under the workspace root. When such a template
lists the repository's origin, `new` fails with `mxv::template::invalid`,
which names the file, and creates nothing. When it does not, or when mxv
cannot read the origins it lists, `new` skips it and names it in the
result's `warnings`. When more than one template lists the origin, `new`
fails with `mxv::template::ambiguous`, which names each of them, and
creates nothing. `--manifest` chooses a file in either case.

## Removing a workspace

`mxv remove <name>` removes each checkout, deletes each branch whose commits
are all on its base, removes the workspace directory and `AGENTS.md`, and
deletes the record. A clone holds its own branch, so removing the clone
deletes the branch with it, and the person's repository is not touched.
mxv deletes a clone by file descriptor, as `mxv clean` deletes a disposable
path, so a directory in it that another process swaps for a symbolic link
is never followed. It refuses, and changes nothing, in these cases:

- The workspace's processes or containers run. The refusal is
  `mxv::workspace::running`, and `--force` stops them before it removes
  the workspace.
- The workspace has docker volumes: named volumes, or anonymous volumes
  of its containers' images. A volume may hold the only copy of a
  database, and docker cannot tell cheaply whether a volume holds data, so
  mxv treats every volume as holding data. The refusal is
  `mxv::workspace::volumes`, which lists the volumes, and its hint repeats
  the refused command's flags with `--delete-volumes`, which deletes them
  after the workspace is removed. `--force` does not delete volumes. mxv
  also removes the workspace's stopped containers. A dry run lists the
  containers that the removal removes and, with `--delete-volumes`, the
  volumes that it deletes. When the workspace has run containers and
  docker's daemon does not answer, the removal fails with
  `mxv::docker::unavailable` and changes nothing.
- A checkout has uncommitted changes or untracked files. The refusal is
  `mxv::workspace::dirty`, and `--force` discards the changes.
- A checkout holds ignored files outside the disposable set, such as
  `.env`. The refusal is `mxv::workspace::ignored_files`, which lists the
  paths, and `--force` deletes them. The disposable set is each directory
  that git reports as ignored as a whole and whose own name is `target`,
  `node_modules`, `dist`, `build` or `.cache`, such as `target/` or
  `web/node_modules/`, because a build makes those again. The repository's
  `mxv.toml` and the config file can add globs to the set, as
  [Disk space](#disk-space) describes. mxv deletes the disposable paths
  without asking and lists them in `disposable`. A tracked file is never
  disposable, and neither is an ignored file inside a directory that holds
  tracked files, such as an ignored file in a tracked `dist/` directory.
  A disposable directory that holds a nested repository at any depth, such
  as one with unpushed commits, also stops the removal and is listed with
  the ignored paths. A nested repository is an entry named `.git` in any
  case, or a directory that holds `HEAD`, `objects/` and `refs/`, as a bare
  repository does. A path that only the repository's own `mxv.toml` makes
  disposable, such as `data/`, is deleted only when the person confirms it
  at the prompt, which names it, or passes `--force`. With `--yes` alone,
  the removal refuses with `mxv::workspace::ignored_files` and changes
  nothing. The result lists such paths in `manifest_paths`. When a checkout's `mxv.toml` is invalid,
  mxv uses the built-in set and the config file's globs for it, and the
  result, or the refusal's `warnings` field, says so.
- A clone holds commits that no branch, tag or note of its `origin` holds.
  Before it counts them, mxv fetches the clone's `origin`: its branches go
  to the remote branches, and a copy of its tags and notes goes below
  `refs/mxv/origin/` in the clone. The fetch deletes what the origin no
  longer has, so a commit counts as pushed only while the origin still
  holds it. The count includes the commits of the clone's `HEAD`,
  branches, tags and notes, and the commits made on a detached `HEAD` that
  nothing else holds, such as a commit made there before switching back to
  the branch, which git warns about when `HEAD` leaves it. A commit counts
  as detached when only the reflog of `HEAD` holds it, and the reflog of no
  local branch does. A commit that was rewritten or undone on a branch,
  such as by `git rebase`, `git commit --amend` or `git reset`, stays in the
  branch's reflog, was given up on purpose, and does not count. A detached
  commit whose patch matches a commit that the origin or a local branch
  holds, such as one that was cherry-picked onto the branch, does not count
  either. A tag that the origin also holds counts as pushed. Each submodule that the
  clone has initialised, whose git directory git keeps below
  `.git/modules`, is fetched and counted the same way, because its commits
  are deleted with the clone. The refusal is
  `mxv::checkout::unpushed_commits`, and `--delete-unmerged` deletes the
  commits with the clone. It also skips the fetch, so a removal with it
  works offline.
- The fetch of a clone's `origin`, or of a submodule's, fails, such as on a
  machine that is offline. The failure is
  `mxv::checkout::origin_unreachable`, it changes nothing, and its hint
  says to go online and run the command again, or to pass
  `--delete-unmerged` to remove the workspace without the fetch.
- A clone holds stash entries. The refusal is `mxv::checkout::stashes`, and
  `--force` deletes them with the clone. The prompt says how many stash
  entries the removal deletes.
- A worktree's `HEAD` holds commits that no branch, tag or remote branch
  holds, such as commits on a detached `HEAD`. The refusal is
  `mxv::checkout::unreachable_commits`, and `--delete-unmerged` deletes the
  commits. mxv also checks this when the checkout's directory is already
  gone, from the `HEAD` that the repository still holds for it.
- git lists a worktree as locked for a reason other than `initializing`.
  The refusal is `mxv::checkout::locked`, and its hint names the
  `git worktree unlock` command to run when nothing else uses the checkout.
  A lock with the reason `initializing`, which a killed `git worktree add`
  leaves, is removed without asking.

A clone's branch is deleted with the clone, and the result reports
`deleted`, or `deleted_unmerged` with `--delete-unmerged` when it held
unpushed commits. A branch that was merged into the origin's default branch
by squashing, and then deleted on the origin, holds commits that no remote
branch holds any more, so mxv counts them as unpushed and refuses the
removal. This is the safe side of the check, and `--delete-unmerged`
removes such a clone once you know that the work was merged. A commit that
a force-push removed from the origin, such as after a rebase, does not
count as unpushed, unless it was made on a detached `HEAD` and no branch
or patch on the origin holds it.

When the person confirmed a removal at the prompt, mxv plans it again
before it deletes anything, and stops with `mxv::workspace::changed` when
a clone holds a commit or a stash entry that the confirmed plan did not
list, even when the numbers are the same.

A worktree's branch with commits that are not on its base stays, and the result reports
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
checkout directory is a symbolic link, when git does not list a worktree's
directory as a worktree of its repository, or when a clone's `.git` is not a
directory of its own.

A checkout's disposable directories, such as a large `target/`, are deleted
first, by file descriptor and with no deadline, as `mxv clean` deletes them.
Each must still pass `clean`'s checks at that moment. Only then does mxv
hand a worktree to `git worktree remove`, or delete a clone. Git's commands
keep their deadlines, so `git worktree remove` has 30 seconds for the
checkout's other files, and a checkout whose other files take longer fails
with `mxv::git::timed_out`.

A removal that fails or stops partway keeps the record, and keeps each
branch that it had not deleted yet, so `mxv remove` can run again:

- A clone is moved aside to `.<name>.mxv-removing` in the workspace
  directory in one rename before mxv deletes it. A deletion that stops
  partway leaves only that directory, and the next removal deletes it.
- A worktree that `git worktree remove` left partly deleted, with its
  `.git` file still in place, is checked again as any checkout is. A
  tracked file that git had already deleted counts as a change, as git
  counts it, so the next removal needs `--force`.
- A worktree whose `.git` file git had already deleted cannot be read by
  git, so mxv cannot tell what is left in it. Without `--force`, the
  removal refuses with `mxv::workspace::dirty`, which says so. With
  `--force`, mxv deletes what is left by file descriptor and runs
  `git worktree prune`, which drops the repository's entry for it.

## Disk space

Build output, such as Rust's `target/` and Node's `node_modules/`, takes
most of a workspace's disk. mxv measures it, deletes it from idle
workspaces, and lets a new workspace share `node_modules/` with its
repository.

### Sizes

`mxv show` prints each workspace's `size`, the bytes its directory takes on
disk, and `disposable`, the bytes its disposable paths take. `--long` and
`--full` add `disposable_paths`, each disposable path with its checkout, its
size and its `source`: `built_in` for a built-in name, `config` for a glob
of the config file, or `manifest` for a glob of the repository's own
`mxv.toml` and nothing else. They also add `kept_paths`, each path that
the disposable set matches and that mxv keeps, with the reason. A size counts the blocks the file system
allocated, so a sparse file counts only what it holds. A file with several
hard links counts once, and mxv never follows a symbolic link. A
copy-on-write clone counts in full, although it shares its blocks with its
source until either changes, so the sizes of the workspaces can add up to
more than the disk holds.

`mxv list` prints the sizes only with `--sizes`. Measuring reads the
metadata of every file of a workspace, which takes seconds for a checkout
with a large `node_modules/` or `target/`, and `list` must stay fast for the
completions and scripts that call it. `--sizes` measures only the
workspaces on the page that `--limit` prints. A workspace that mxv cannot
measure, such as one whose checkout git no longer knows, keeps its row
with the failure in `problem`, and the other rows are still measured.
`show` always measures, because it prints one workspace at a time.

### Disposable paths

The disposable set is the built-in directories that
[Removing a workspace](#removing-a-workspace) names, together with the globs
that the repository's `mxv.toml` and the config file declare in
`disposable`. The globs from both add to the built-in set, and a glob from
either applies to every checkout of that repository. A path is disposable
only while `git status` reports it as ignored and a glob matches it, so a
tracked file is never disposable. A directory that holds a nested
repository at any depth, an entry named `.git` in any case or a directory
that holds `HEAD`, `objects/` and `refs/`, is never disposable, because it
may hold commits that exist nowhere else.

mxv also keeps a directory that it cannot check in full. A disposable
directory that is itself the mount point of another file system, or that
holds one, such as a disk image mounted at or below `target/`, is kept, and
its reason names the mount point. mxv compares each directory with the
checkout's root. On macOS it compares the device numbers. On Linux it also
compares the mount IDs that `statx` reports, so a bind mount of a directory
on the same file system counts as another mount, and on a kernel older
than 5.8, which reports no mount ID, the device numbers alone decide. A directory
with an entry that mxv cannot read is kept, because mxv cannot tell whether
the entry holds a repository. A directory more than 100 levels deep below
the disposable path is kept, and its reason names the level where the check
stopped. The check opens each directory by file descriptor relative to its
parent, so it reads a tree whose paths are longer than the system's path
limit, and it holds at most one open descriptor for each level.

A glob is a path relative to the repository's root, written as `git status`
prints it:

- A glob that ends with `/`, such as `coverage/`, matches only a directory.
- A glob with no other `/`, such as `coverage/` or `*.log`, matches that
  name at any depth. Any other glob, such as `web/test-results/`, starts at
  the repository's root.
- `*` matches any characters inside one path component, `?` matches one
  character, and a component that is `**` matches any number of components.

A glob matches a whole path that `git status` reports, and never a path
below it. git reports an ignored directory as one path only when it holds
no tracked file. Inside a directory that holds tracked files, git reports
each ignored file on its own, so a glob that matches files, such as
`*.log`, matches ignored files there too.

A repository is shared, and its `mxv.toml` can change with a pull, so mxv
does not let it widen what `clean` and `remove` delete. Each glob in
`mxv.toml` must end with `/` and have a literal last name, without `*` or
`?`, such as `web/test-results/` or `**/coverage/`. A glob such as `*`,
`**/*`, `*/` or `.env` fails with `mxv::manifest::invalid` and its line.
Only the person's own config file may declare a glob that names files, such
as `*.log`, or a wildcard in the last name. A literal directory name in
`mxv.toml` can still name a directory that holds data, such as `data/`, so
mxv marks each path that only the manifest makes disposable. `show` gives
it the source `manifest`, the prompts of `clean` and `remove` name it and
say that it comes from the repository's `mxv.toml`, `remove --yes`
refuses it without `--force`, and `clean --yes` keeps it without `--force`
and lists it in `skipped`. Any glob that is empty, starts
with `/`, holds a `..`, `.` or empty component, or names `.git` in any
case, as in `**/.git/`, fails, with
`mxv::manifest::invalid` in `mxv.toml` or `mxv::config::invalid` in the
config file, and the failure names the glob's line.

When a repository's `mxv.toml` is invalid, `show`, `clean` and `remove` use
the built-in set and the config file's globs for that checkout, and print
why in `warnings`.

### Cleaning idle workspaces

`mxv clean <workspace>...` deletes the disposable paths of the named
workspaces that are idle, and `mxv clean --all` does so for every workspace.
It deletes nothing else. A workspace is idle when none of its processes or
containers run, and no file or directory of its checkouts outside the
disposable paths and `.git` has changed for the idle age. mxv reads the
later of each entry's modification time and its status-change time
(`ctime`), which no tool can set back, so a file whose modification time a
tool restored still counts as changed. A directory's times change when an
entry in it is created, renamed or deleted, so a deleted file counts as a
change. A commit, a staged change and a checkout of another branch count
too. A time later than the current time makes the workspace busy, because
mxv cannot tell how long it has been idle. The idle age is `--idle`, such
as `--idle 3d`, or else the config file's `clean_idle`, or else 7 days. An
age is a whole number followed by `w`, `d`, `h`, `m` or `s`. Measuring a
workspace changes none of its times, because mxv runs `git status` with
`--no-optional-locks`, which keeps git from rewriting the index.

`clean` prints a row for each workspace it considered: `cleaned` with the
bytes it freed in `freed`, or `kept` with the reason in `reason`, such as a
running process or a recent change. A dry run prints `would_clean` and
deletes nothing. `skipped` lists each matched path that the run kept, such
as a directory that holds a `.git` entry, and `reason` names them too. A
workspace whose every disposable path was kept has the state `kept`, and its
`reason` names the kept paths and why. The note after the rows gives the
total freed. `clean` asks before it deletes, and `--yes` answers for a
script. With `--yes`, `clean` keeps each path that only the repository's
`mxv.toml` declares disposable, because no person confirmed it, and
`--force` deletes those paths too. `--over-limit --yes` keeps them in the
same way, so they do not count toward the bytes that the run frees.

Before it deletes anything in a workspace, `clean` takes the workspace's
lock, which `mxv up` and `mxv down` also take, so no process starts while
it deletes. It then measures the workspace again and checks that no process
or container runs and nothing changed within the idle age. A workspace that
became busy keeps every path. For each path, it checks again that git
reports the path as ignored, that the repository tracks no file in it, that
it holds no `.git` entry and that it is not a symbolic link. A path that
fails a check stays and is listed in `skipped`. When another mxv command
holds the lock for more than 5 seconds, the workspace is kept with that
reason.

`clean` deletes by file descriptor, as Rust's `std::fs::remove_dir_all`
does. It opens the checkout's root once, then opens each component of the
path, such as `web` and then `node_modules` for `web/node_modules/`,
relative to the one before it with `O_NOFOLLOW`. Below the path, it opens
each directory relative to its parent in the same way and deletes the
directory's entries relative to it. A component that another process swaps
for a symbolic link before or during the deletion is never followed:
`clean` deletes the link or fails on it, and touches nothing outside the
checkout. The deletion also refuses a directory on another file system or mount and
a directory more than 100 levels below the path, and fails the path with
`mxv::file::unwritable` while it deletes the path's other entries.

A workspace that mxv cannot find, read or measure fails alone, the other
workspaces are still cleaned, and `clean` exits with 1. When a path cannot
be deleted in full, such as a directory without write permission, `clean`
goes on with the path's other entries and with the workspace's other paths.
The workspace then fails with `mxv::file::unwritable`, whose `path` names
the first entry that could not be deleted, and whose hint names the
directory that needs write permission, `deleted` lists the paths that
were deleted, and `freed` gives the bytes freed.

`freed` is measured, from each path's size before and after its deletion.
A file whose other hard link lies outside the deleted directory frees
nothing, so a `node_modules/` that a package manager filled with hard links
to its shared store frees little space, and `freed` says so. A dry run
reports the paths' sizes, which can be more than a run frees.

`mxv clean --over-limit` reads `max_disposable` from the config file, a
size such as `"50GiB"` or `"40GB"`. It adds up the disposable bytes of every
workspace, and when the total exceeds the limit, it cleans idle workspaces,
the one unchanged longest first, until the total is within the limit. The
reason of each idle workspace it keeps says whether the total was within
the limit or whether cleaning the workspaces unchanged longer brought it
there. When the config file sets no `max_disposable`, it fails with
`mxv::config::no_limit`. `--over-limit` is meant for a cron job or a git
hook, such as this daily crontab line:

```sh
17 3 * * * mxv clean --over-limit --yes
```

`clean` measures every workspace it considers, and each workspace it cleans
twice, so a run takes as long as `show` takes for each of them.

### Cloning `node_modules/`

`mxv new` clones each ignored directory of the repository that a
`clone_on_new` glob matches into the new checkout, with a copy-on-write
clone. The clone shares its blocks with the repository's directory until
either copy of a file changes, so a new workspace starts with installed
packages and costs almost no disk at first. The config file's
`clone_on_new` replaces the default, `["node_modules/"]`, and the
repository's `mxv.toml` adds globs to it. `clone_on_new = []` in the config
file turns off the default. `--no-clone` clones nothing for one `new`.

Every `clone_on_new` glob, in the config file and in `mxv.toml`, must end
with `/` and have a literal last name, as the disposable globs of
`mxv.toml` must. mxv never clones `target/`, because a build of the
checkout's own sources writes it, and a glob that names `target` fails as
invalid. mxv clones only a directory that `git status` reports as ignored
as a whole, that is not a symbolic link, and whose place in the checkout is
empty. `new` prints the cloned directories in `cloned`.

A clone copies symbolic links as they are. An absolute link inside a
cloned `node_modules/`, such as one that a package manager wrote for a
package of the repository's own workspace, still points into the
repository's checkout. The new checkout reads that package from the
repository until the next install rewrites the link.

Cloning needs APFS on macOS. When the file system cannot clone, such as
HFS+ or a workspace root on another volume, `new` clones nothing, copies
nothing, and prints why in `clone_warning`. On Linux, `new` always prints
that warning for now. The workspace is created either way.

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
