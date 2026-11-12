# Commands of `mxv`

This reference is generated from the help of `mxv`. Edit the help in the tool's code, and not this file.

## `mxv`

Create, archive and restore task workspaces that span several git repositories

### Commands

- [`mxv new`](#mxv-new): Create a workspace with its own clone of each repository, on a new branch from the freshly fetched origin
- [`mxv add`](#mxv-add): Add a clone of a repository to a workspace, on the workspace's branch
- [`mxv list`](#mxv-list): List the workspaces
- [`mxv show`](#mxv-show): Show workspaces by name, in the order given
- [`mxv cd`](#mxv-cd): Print the directory of a workspace or of one of its checkouts, so `cd "$(mxv cd NAME)"` resumes it
- [`mxv up`](#mxv-up): Build a workspace's units through deadband, then start them from its mxv.toml, in dependency order, and wait until each is ready
- [`mxv down`](#mxv-down): Stop a workspace's units with SIGTERM, then SIGKILL after 10 seconds
- [`mxv restart`](#mxv-restart): Build a workspace's units through deadband, then stop them and start them again
- [`mxv status`](#mxv-status): Report whether each unit of a workspace is running, exited or stopped
- [`mxv logs`](#mxv-logs): Print the last lines of a unit's log, and follow it with --follow
- [`mxv exec`](#mxv-exec): Run a one-off command in a workspace or in the context of a running unit, and exit with its exit code
- [`mxv run`](#mxv-run): Run a task of a workspace through deadband, or list its tasks
- [`mxv archive`](#mxv-archive): Archive a workspace, so restore can rebuild it at the same commits
- [`mxv restore`](#mxv-restore): Rebuild an archived workspace at its archived commits
- [`mxv repair`](#mxv-repair): Repair the broken records of one name that `mxv list` names `mxv repair NAME` for
- [`mxv remove`](#mxv-remove): Remove a workspace's checkouts, its merged branches, its directory and its record
- [`mxv clean`](#mxv-clean): Delete the disposable directories, such as target/ and node\_modules/, of idle workspaces
- [`mxv reclaim`](#mxv-reclaim): Stop idle workspaces to free memory, or delete their build output to free disk, until the need is met
- [`mxv pin`](#mxv-pin): Keep mxv reclaim from stopping a workspace
- [`mxv unpin`](#mxv-unpin): Let mxv reclaim stop a workspace again while it is idle
- [`mxv ports`](#mxv-ports): Manage the host ports that a workspace holds
- [`mxv commands`](#mxv-commands): Explore mxv's commands
- [`mxv schemas`](#mxv-schemas): Explore the JSON documents that mxv prints
- [`mxv self`](#mxv-self): Review mxv's own use
- [`mxv skills`](#mxv-skills): Give coding agents mxv's skill
- [`mxv completion`](#mxv-completion): Set up mxv's completion in a shell

### Arguments

- `--json`: Print the result as a JSON document. The default prints text. Works after every subcommand.
- `--fields <FIELD>`: Print only these comma-separated fields, in this order. The default prints the compact columns. Takes more than one value. Works after every subcommand.
- `--long`: Add the long columns after the compact ones. The default prints only the compact columns. Works after every subcommand.
- `--full`: With --json, print every field of every row. The default prints only the fields of the columns. Works after every subcommand.
- `--no-input`: Fail instead of asking a question. Works after every subcommand.

## `mxv new`

Create a workspace with its own clone of each repository, on a new branch from the freshly fetched origin.

Each --repo adds one clone, in the folder \<ROOT\>/\<NAME\>/\<FOLDER\>, where the folder is the repository's name or the one that an --as right after that --repo gives. Two folders with the same name, compared without case, are refused. Every clone is on the branch NAME. The clone has a .git of its own, with no hooks, and its origin is the repository's origin, so nothing in it can change the repository or another workspace. It borrows the objects of a bare mirror of the origin in $XDG\_DATA\_HOME/mxv/mirrors, which mxv fetches first and never deletes. The repository itself is only read, and its .git/info/exclude is copied into the clone.

The workspace's directory is \<ROOT\>/\<NAME\>. The root is $MXV\_ROOT when it is set, or else the `root` that the config file $XDG\_CONFIG\_HOME/mxv/config.toml sets, such as `root = "~/work"`, or else $XDG\_DATA\_HOME/mxv/workspaces, which is \~/.local/share/mxv/workspaces when XDG\_DATA\_HOME is unset. The result names the root and where it came from. Each record keeps its workspace's own directory, so changing the root later moves no workspace.

### Arguments

- `<NAME>`: The workspace's name, which also names its directory and its branch. Required.
- `--repo <PATH>`: A git repository to check out, or a directory inside it; repeat it or use commas for several. Required. Takes more than one value.
- `--as <FOLDER>`: The folder of the checkout of the --repo right before it, which defaults to the repository's name. Takes more than one value.
- `--no-clone`: Clone none of the repositories' ignored directories, such as node\_modules/, into the new checkouts
- `--manifest <FILE>`: Copy FILE into the new workspace as its workspace manifest, \<ROOT\>/\<NAME\>/mxv.toml, in place of a template
- `--dry-run`: Report what the command would change, and change nothing

### Examples

Print the workspace that a checkout of the repository app would get.

```sh
mxv new triage --repo app --dry-run
```

Print the workspace that checkouts of the repositories app and web would get.

```sh
mxv new triage --repo app --repo web --dry-run
```

### Columns

- `name`: The workspace's name, which is also the name of its directory and of its checkouts' branch.
- `branch`: The branch that the workspace's checkouts were created on.
- `path`: The workspace's directory, which `mxv cd` prints. It comes from the workspace's record, so it stays where the workspace was created when the root changes later.
- `root`: The workspace root that the run used, in which `new` creates a workspace's directory.
- `root_source`: Where the root came from: `env` for `MXV_ROOT`, `config` for the config file's `root`, or `default` for `$XDG_DATA_HOME/mxv/workspaces`.
- `ports`: The host port that the workspace holds for each named port of its manifest, which the first `mxv up` allocates.
- `cloned`: Each ignored directory of the repository that `new` cloned into the checkout, or would clone on a dry run, with the checkout's name in front, such as `app: web/node_modules/`. A clone shares its disk with the repository's copy until a file in either changes.
- `clone_warning`: Why `new` cloned fewer directories than the globs name, such as a file system that cannot clone, or null. `new` never copies the directories in full.
- `manifest`: Which manifest the workspace's commands read: `workspace` for the workspace manifest above the checkout, `repository` for the checkout's own `mxv.toml`, `merged` for the files of a workspace with several checkouts, or `none` for a bare workspace.
- `manifest_from`: The file that `new` copied into the workspace manifest: the `--manifest` file, or the template whose `[template]` table lists the repository's origin, or null when it copied none.
- `warnings`: The sentences, joined with `; `, for each template that `new` skipped because it cannot use it, and for each table of the copied manifest that mxv ignores although its name is close to one of mxv's own, or null.

### Columns that `--long` adds

- `kind`: How the workspace's checkouts are made: `clone` for a clone with a `.git` of its own, or `worktree` for a git worktree of the repository, which mxv made before clones.
- `manifest_path`: The path of the manifest that the workspace's commands read, or null when they read none. For a merged manifest, it is the workspace manifest when it exists, or else the first checkout's `mxv.toml`.
- `repositories`: The top-level directory of each repository that the workspace checks out.

Effects: additive, not idempotent, open-world.

## `mxv add`

Add a clone of a repository to a workspace, on the workspace's branch.

The clone lives in \<ROOT\>/\<NAME\>/\<FOLDER\>, where the folder is the repository's name or the one that --as gives, and is made as `mxv new` makes each clone. The workspace's AGENTS.md and record name it afterwards. A workspace with two or more checkouts merges each checkout's mxv.toml with the workspace manifest and puts the checkout's folder in front of each name that the checkout's mxv.toml declares, so adding a second repository renames the first repository's units and ports, such as `server` to `app-server`. The command therefore refuses while the workspace's units run, and moves the host ports that the workspace holds to the new names.

### Arguments

- `-w`, `--workspace <WORKSPACE>`: The workspace, which defaults to the one that holds the current directory
- `--repo <PATH>`: The git repository to check out, or any directory inside it. Required.
- `--as <FOLDER>`: The checkout's folder, which defaults to the repository's name
- `--no-clone`: Clone none of the repository's ignored directories, such as node\_modules/, into the new checkout
- `--manifest <FILE>`: Copy FILE into the workspace as its workspace manifest once it has the new checkout, with paths relative to the workspace's directory; a workspace of one checkout with a workspace manifest needs it
- `--dry-run`: Report what the command would change, and change nothing

### Examples

Print the checkout that adding the repository web to the workspace triage would make.

```sh
mxv add -w triage --repo web --dry-run
```

### Columns

- `name`: The workspace's name, which is also the name of its directory and of its checkouts' branch.
- `branch`: The branch that the workspace's checkouts were created on.
- `path`: The workspace's directory, which `mxv cd` prints. It comes from the workspace's record, so it stays where the workspace was created when the root changes later.
- `root`: The workspace root that the run used, in which `new` creates a workspace's directory.
- `root_source`: Where the root came from: `env` for `MXV_ROOT`, `config` for the config file's `root`, or `default` for `$XDG_DATA_HOME/mxv/workspaces`.
- `ports`: The host port that the workspace holds for each named port of its manifest, which the first `mxv up` allocates.
- `cloned`: Each ignored directory of the repository that `new` cloned into the checkout, or would clone on a dry run, with the checkout's name in front, such as `app: web/node_modules/`. A clone shares its disk with the repository's copy until a file in either changes.
- `clone_warning`: Why `new` cloned fewer directories than the globs name, such as a file system that cannot clone, or null. `new` never copies the directories in full.
- `manifest`: Which manifest the workspace's commands read: `workspace` for the workspace manifest above the checkout, `repository` for the checkout's own `mxv.toml`, `merged` for the files of a workspace with several checkouts, or `none` for a bare workspace.
- `manifest_from`: The file that `new` copied into the workspace manifest: the `--manifest` file, or the template whose `[template]` table lists the repository's origin, or null when it copied none.
- `warnings`: The sentences, joined with `; `, for each template that `new` skipped because it cannot use it, and for each table of the copied manifest that mxv ignores although its name is close to one of mxv's own, or null.

### Columns that `--long` adds

- `kind`: How the workspace's checkouts are made: `clone` for a clone with a `.git` of its own, or `worktree` for a git worktree of the repository, which mxv made before clones.
- `manifest_path`: The path of the manifest that the workspace's commands read, or null when they read none. For a merged manifest, it is the workspace manifest when it exists, or else the first checkout's `mxv.toml`.
- `repositories`: The top-level directory of each repository that the workspace checks out.

Effects: additive, not idempotent, open-world.

## `mxv list`

List the workspaces

### Arguments

- `--limit <N>`: Print at most this many rows, up to 100. Default: `25`.
- `--all`: Print every row
- `--sizes`: Measure each workspace's size on disk and its disposable size, which reads every file of every workspace on the page
- `--archived`: List the archived workspaces instead

### Examples

List the first page of workspaces.

```sh
mxv list
```

### Columns

- `name`: The workspace's name, which is also the name of its directory and of its checkouts' branch.
- `branch`: The branch that the workspace's checkouts were created on.
- `path`: The workspace's directory, which `mxv cd` prints.
- `size`: The bytes that the workspace's directory takes on disk, with each file that has several hard links counted once and no symbolic link followed, or null when `--sizes` was not given.
- `disposable`: The bytes that the workspace's disposable directories take on disk, such as `target/` and `node_modules/`, which `mxv clean` deletes, or null when `--sizes` was not given.
- `archived`: How long ago the workspace was archived, or null in a row of a workspace that is not archived.
- `local_only`: The number of commits of an archived workspace that exist only on this machine, or null in a row of a workspace that is not archived.
- `ignored`: The number of ignored files and links, such as `.env`, that the archive keeps in its private tar, or null in a row of a workspace that is not archived.
- `ignored_bytes`: The bytes that the ignored files and links in the archive's private tar take, or null in a row of a workspace that is not archived.
- `ports`: The host port that the workspace holds for each named port of its manifest, which the first `mxv up` allocates.
- `problem`: What is wrong with the workspace or its archive, or empty when nothing is: a record that mxv cannot read, such as one that is not JSON or that a newer mxv wrote, an archive that stopped before it finished, one that stopped and whose pins are gone, an older archive beside a newer workspace of the same name, which only the archive listing shows, a restore that stopped before it deleted the archive, a workspace whose checkout directories are all gone, or a size that mxv cannot measure. A row of a record that mxv cannot read names the record in `name`, a workspace record without `.json` and an archive record by its directory, and leaves the other fields empty.
- `repair`: The command that repairs the problem, such as `mxv archive -w old-api` or `mxv repair old-api`, what the person does by hand when no command can, or empty when no repair is known.

### Columns that `--long` adds

- `kind`: How the workspace's checkouts are made: `clone` for a clone with a `.git` of its own, or `worktree` for a git worktree of the repository, which mxv made before clones. Null in a row whose record mxv cannot read.
- `manifest`: Which manifest the workspace's commands read: `workspace` for the workspace manifest above the checkout, `repository` for the checkout's own `mxv.toml`, `merged` for the files of a workspace with several checkouts, or `none` for a bare workspace. Null in a row whose record mxv cannot read.
- `manifest_path`: The path of the manifest that the workspace's commands read, or null when they read none. For a merged manifest, it is the workspace manifest when it exists, or else the first checkout's `mxv.toml`.
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
- `size`: The bytes that the workspace's directory takes on disk, with each file that has several hard links counted once and no symbolic link followed.
- `disposable`: The bytes that the workspace's disposable directories take on disk, such as `target/` and `node_modules/`, which `mxv clean` deletes.
- `ports`: The host port that the workspace holds for each named port of its manifest, which the first `mxv up` allocates.
- `warnings`: Why a checkout's disposable set holds fewer globs than its `mxv.toml` declares, such as an invalid manifest, or why its paths wait for a confirmation, and each table of the manifest that mxv ignores although its name is close to one of mxv's own, or null.

### Columns that `--long` adds

- `kind`: How the workspace's checkouts are made: `clone` for a clone with a `.git` of its own, or `worktree` for a git worktree of the repository, which mxv made before clones.
- `manifest`: Which manifest the workspace's commands read: `workspace` for the workspace manifest above the checkout, `repository` for the checkout's own `mxv.toml`, `merged` for the files of a workspace with several checkouts, or `none` for a bare workspace.
- `manifest_path`: The path of the manifest that the workspace's commands read, or null when they read none. For a merged manifest, it is the workspace manifest when it exists, or else the first checkout's `mxv.toml`.
- `repositories`: The top-level directory of each repository that the workspace checks out.
- `disposable_paths`: Each disposable path with its checkout and the bytes it takes.
- `kept_paths`: Each path that the disposable set matches and that mxv keeps, with the reason, such as a directory that holds a `.git` entry.
- `services`: Each local AWS service of the workspace's manifest, by name, with the variables that a process that needs it receives, such as `AWS_ENDPOINT_URL_DYNAMODB` and the service's own `env`. A value holds `<port>` until the first `mxv up` gives the service's backend a host port. No value is a secret: the access keys are the literal `test`.

Effects: read-only, closed-world.

## `mxv cd`

Print the directory of a workspace or of one of its checkouts, so `cd "$(mxv cd NAME)"` resumes it

### Arguments

- `<NAME>`: The name of the workspace. Required.
- `<CHECKOUT>`: The folder of one of the workspace's checkouts, whose directory to print in place of the workspace's

### Examples

Print the directory of the workspace reading-list.

```sh
mxv cd reading-list
```

Effects: read-only, closed-world.

## `mxv up`

Build a workspace's units through deadband, then start them from its mxv.toml, in dependency order, and wait until each is ready.

Before anything builds, stops or starts, mxv compares the permissions of the selected host units and the units they depend on with what a person has granted to the repository. A unit asks for more when it is new, when its permissions hold an entry that the grant lacks, or when it would run outside the sandbox and its grant is sandboxed. At a terminal, mxv lists what each unit asks for and asks before it goes on, and a yes adds it to the grant that every workspace of the repository shares. Without a terminal, or with --no-input, it fails with mxv::permissions::not\_granted and builds, stops and starts nothing. No flag or variable approves a grant.

### Arguments

- `-w`, `--workspace <WORKSPACE>`: The workspace, which defaults to the one that holds the current directory
- `<UNIT>`: The units, which default to every unit. Takes more than one value.

### Examples

Start the unit web of the workspace reading-list and the units it depends on.

```sh
mxv up -w reading-list web
```

### Columns

- `name`: The unit's name in the manifest.
- `kind`: `process` or `container`.
- `state`: `running`, `exited` when it ended on its own or something other than `mxv down` stopped it, `created` for a container that docker created and did not start, `stopped`, `starting` while it is not ready yet, `stopping`, `restarting` while docker restarts a container, or `unknown` when mxv cannot tell.
- `ready`: Whether the unit has been ready since it last started: true once its readiness check passed, or once a unit without one outlasted its start grace, and false while it runs and has not, such as after its keeper or docker restarted it. It is null when it does not run.
- `pid`: The process id, which is also its process group's id, while a process runs, and null for a container.
- `code`: The exit code of an exited unit, or null when a signal ended it or it has not exited. A container's code is the one docker reports, such as 137 for a container that SIGKILL ended.
- `signal`: The number of the signal that ended an exited unit, such as 15 for SIGTERM, or null.
- `reason`: Why mxv cannot tell the state of an `unknown` unit, or why it does not know the exit code of an exited unit, or null when it knows.
- `ports`: The ports that a process receives, as `VARIABLE=port`, or that a container publishes, as `port-name=port`.
- `sandbox`: Whether phase runs a host process under its sandbox, as phase describes it: `seatbelt (partial: <what it leaves open>)` on macOS, or `unsandboxed (<reason>)`, such as `unsandboxed (its app declares no permissions)` for a process whose manifest has no `permissions` table. It is null for a container and for a process that phase does not run.

### Columns that `--long` adds

- `log`: The unit's log file, which `mxv logs` prints, or null for a container, whose output docker keeps and `mxv logs` also prints.

Effects: additive, idempotent, open-world.

Writes: applies all items or none.

Children: one indented line under a unit for each note of `up` or `restart`, which starts with `warning:` or `info:`.

## `mxv down`

Stop a workspace's units with SIGTERM, then SIGKILL after 10 seconds.

A named unit that a running unit outside the stopped set depends on keeps running, and so does what it depends on. Each kept unit fails the command with mxv::process::still\_needed, which names the units that need it, and the other named units stop. Name the dependants too to stop them all.

### Arguments

- `-w`, `--workspace <WORKSPACE>`: The workspace, which defaults to the one that holds the current directory
- `<UNIT>`: The units, which default to every unit. Takes more than one value.

### Examples

Stop every unit of the workspace reading-list.

```sh
mxv down -w reading-list
```

### Columns

- `name`: The unit's name in the manifest.
- `kind`: `process` or `container`.
- `state`: `running`, `exited` when it ended on its own or something other than `mxv down` stopped it, `created` for a container that docker created and did not start, `stopped`, `starting` while it is not ready yet, `stopping`, `restarting` while docker restarts a container, or `unknown` when mxv cannot tell.
- `ready`: Whether the unit has been ready since it last started: true once its readiness check passed, or once a unit without one outlasted its start grace, and false while it runs and has not, such as after its keeper or docker restarted it. It is null when it does not run.
- `pid`: The process id, which is also its process group's id, while a process runs, and null for a container.
- `code`: The exit code of an exited unit, or null when a signal ended it or it has not exited. A container's code is the one docker reports, such as 137 for a container that SIGKILL ended.
- `signal`: The number of the signal that ended an exited unit, such as 15 for SIGTERM, or null.
- `reason`: Why mxv cannot tell the state of an `unknown` unit, or why it does not know the exit code of an exited unit, or null when it knows.
- `ports`: The ports that a process receives, as `VARIABLE=port`, or that a container publishes, as `port-name=port`.
- `sandbox`: Whether phase runs a host process under its sandbox, as phase describes it: `seatbelt (partial: <what it leaves open>)` on macOS, or `unsandboxed (<reason>)`, such as `unsandboxed (its app declares no permissions)` for a process whose manifest has no `permissions` table. It is null for a container and for a process that phase does not run.

### Columns that `--long` adds

- `log`: The unit's log file, which `mxv logs` prints, or null for a container, whose output docker keeps and `mxv logs` also prints.

Effects: destructive, idempotent, closed-world.

Writes: reports each item and exits 1 when any item fails.

Children: one indented line under a unit for each note of `up` or `restart`, which starts with `warning:` or `info:`.

## `mxv restart`

Build a workspace's units through deadband, then stop them and start them again.

Before anything builds, stops or starts, mxv checks the selected host units' permissions against the repository's grant and asks at a terminal, as `mxv up` does. A unit that a running unit depends on still restarts.

### Arguments

- `-w`, `--workspace <WORKSPACE>`: The workspace, which defaults to the one that holds the current directory
- `<UNIT>`: The units, which default to every unit. Takes more than one value.

### Examples

Stop the unit web of the workspace reading-list and start it again.

```sh
mxv restart -w reading-list web
```

### Columns

- `name`: The unit's name in the manifest.
- `kind`: `process` or `container`.
- `state`: `running`, `exited` when it ended on its own or something other than `mxv down` stopped it, `created` for a container that docker created and did not start, `stopped`, `starting` while it is not ready yet, `stopping`, `restarting` while docker restarts a container, or `unknown` when mxv cannot tell.
- `ready`: Whether the unit has been ready since it last started: true once its readiness check passed, or once a unit without one outlasted its start grace, and false while it runs and has not, such as after its keeper or docker restarted it. It is null when it does not run.
- `pid`: The process id, which is also its process group's id, while a process runs, and null for a container.
- `code`: The exit code of an exited unit, or null when a signal ended it or it has not exited. A container's code is the one docker reports, such as 137 for a container that SIGKILL ended.
- `signal`: The number of the signal that ended an exited unit, such as 15 for SIGTERM, or null.
- `reason`: Why mxv cannot tell the state of an `unknown` unit, or why it does not know the exit code of an exited unit, or null when it knows.
- `ports`: The ports that a process receives, as `VARIABLE=port`, or that a container publishes, as `port-name=port`.
- `sandbox`: Whether phase runs a host process under its sandbox, as phase describes it: `seatbelt (partial: <what it leaves open>)` on macOS, or `unsandboxed (<reason>)`, such as `unsandboxed (its app declares no permissions)` for a process whose manifest has no `permissions` table. It is null for a container and for a process that phase does not run.

### Columns that `--long` adds

- `log`: The unit's log file, which `mxv logs` prints, or null for a container, whose output docker keeps and `mxv logs` also prints.

Effects: destructive, not idempotent, open-world.

Writes: applies all items or none.

Children: one indented line under a unit for each note of `up` or `restart`, which starts with `warning:` or `info:`.

## `mxv status`

Report whether each unit of a workspace is running, exited or stopped

### Arguments

- `-w`, `--workspace <WORKSPACE>`: The workspace, which defaults to the one that holds the current directory

### Examples

Report the state of each unit of the workspace reading-list.

```sh
mxv status -w reading-list
```

### Columns

- `name`: The unit's name in the manifest.
- `kind`: `process` or `container`.
- `state`: `running`, `exited` when it ended on its own or something other than `mxv down` stopped it, `created` for a container that docker created and did not start, `stopped`, `starting` while it is not ready yet, `stopping`, `restarting` while docker restarts a container, or `unknown` when mxv cannot tell.
- `ready`: Whether the unit has been ready since it last started: true once its readiness check passed, or once a unit without one outlasted its start grace, and false while it runs and has not, such as after its keeper or docker restarted it. It is null when it does not run.
- `pid`: The process id, which is also its process group's id, while a process runs, and null for a container.
- `code`: The exit code of an exited unit, or null when a signal ended it or it has not exited. A container's code is the one docker reports, such as 137 for a container that SIGKILL ended.
- `signal`: The number of the signal that ended an exited unit, such as 15 for SIGTERM, or null.
- `reason`: Why mxv cannot tell the state of an `unknown` unit, or why it does not know the exit code of an exited unit, or null when it knows.
- `ports`: The ports that a process receives, as `VARIABLE=port`, or that a container publishes, as `port-name=port`.
- `sandbox`: Whether phase runs a host process under its sandbox, as phase describes it: `seatbelt (partial: <what it leaves open>)` on macOS, or `unsandboxed (<reason>)`, such as `unsandboxed (its app declares no permissions)` for a process whose manifest has no `permissions` table. It is null for a container and for a process that phase does not run.

### Columns that `--long` adds

- `log`: The unit's log file, which `mxv logs` prints, or null for a container, whose output docker keeps and `mxv logs` also prints.

Effects: read-only, closed-world.

Children: one indented line under a unit for each note of `up` or `restart`, which starts with `warning:` or `info:`.

## `mxv logs`

Print the last lines of a unit's log, and follow it with --follow

### Arguments

- `-w`, `--workspace <WORKSPACE>`: The workspace, which defaults to the one that holds the current directory
- `<UNIT>`: The unit whose log to print. Required.
- `-f`, `--follow`: Keep printing each line that the unit writes, until interrupted
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

## `mxv exec`

Run a one-off command in a workspace or in the context of a running unit, and exit with its exit code.

With no UNIT, the command runs on the host in the workspace, and it needs no running unit and no mxv.toml. It runs at the top of the checkout that --repo names; or else in the current directory when that is inside a checkout; or else at the top of a workspace's only checkout; or else in the workspace's directory. It receives what a mise task of `mxv run` receives: the workspace's ports as MXV\_PORT\_\<NAME\>, with those of the checkout it runs in short and the others as MXV\_PORT\_\<CHECKOUT\>\_\<NAME\>, the variables of every service that the manifest declares, and the variables that a host process receives from your environment. A variable outside that list does not reach the command, so write `mxv exec -- env NAME=value COMMAND` to set one. mxv starts no services and no units, so run `mxv up` first when the command talks to them. The command has no sandbox and no time limit. It takes one place from deadband, and --no-gate asks deadband nothing. Inside `deadband run`, deadband gives the command no thread limits, so let `mxv exec` take the place itself. A Ctrl-C at a terminal reaches the command itself, and mxv waits for it to end. mxv holds the workspace's lock only until the command starts, so `mxv up` and `mxv down` still work, and `mxv down` does not stop the command. `mxv remove` and `mxv archive` do not wait for the command, so end it before you run them.

With a UNIT, a host process's command runs in the process's directory, with the variables that the process receives and under its sandbox, as `mxv up` starts the process. A container's command runs inside the container through `docker exec`, with a terminal of its own when mxv's standard input and output are both terminals. The command is the words after `--`, which no shell reads. mxv exits with the command's exit code, or with 128 plus the number of the signal that ended it. A sandboxed command exits with 78 when its sandbox cannot be applied. A sandboxed command runs in a session of its own and cannot have the terminal, so mxv refuses it while its standard input, output or error is a terminal, and on Ctrl-C mxv stops the command's process group. The command keeps running when something sends SIGTERM to mxv alone. A host command is not one of the workspace's processes, so `mxv down` does not stop it.

### Arguments

- `-w`, `--workspace <WORKSPACE>`: The workspace, which defaults to the one that holds the current directory
- `--repo <CHECKOUT>`: The folder of the checkout to run the command at the top of, when the command line names no UNIT
- `--no-gate`: Ask deadband for no place, when the command line names no UNIT
- `<UNIT>`: The running unit in whose context the command runs, which defaults to running it on the host in the workspace
- `<COMMAND>`: The command's program and its arguments, after `--`. Required. Takes more than one value.

### Examples

Print the variables that a command receives in the context of web.

```sh
mxv exec -w reading-list web -- env
```

Print the variables that a command receives in the workspace, with no unit running.

```sh
mxv exec -w reading-list -- env
```

Effects: destructive, not idempotent, open-world.

Writes: applies all items or none.

## `mxv run`

Run a task of a workspace through deadband, or list its tasks.

A task is declared under `[tasks.<name>]` in mxv.toml, or is a mise task of a checkout, which `mise tasks ls --json` lists and `mise run` runs. A repository with mise tasks needs no mxv.toml. mxv never trusts a mise config for the person, so a config that mise refuses leaves its tasks unknown and mise's message is shown. A declared task wins over a mise task of the same name, with a note. A name found in several checkouts is refused, and --repo chooses.

Each task takes one place from deadband, and `gate = false` opts a declared task out. A task has no sandbox and no time limit, and receives its own checkout's ports as MXV\_PORT\_\<NAME\> and other checkouts' ports as MXV\_PORT\_\<CHECKOUT\>\_\<NAME\>. A task of the workspace manifest receives that manifest's ports as MXV\_PORT\_\<NAME\> and every checkout's in the longer form. A declared task receives the variables of the services that it needs, and a mise task those of every service. mxv adds MISE\_JOBS=1 beside deadband's job variables, so a mise task runs one step at a time under its place. mxv exits with the task's exit code, or with 128 plus the number of the signal that ended it. A Ctrl-C at a terminal reaches the task itself, and mxv waits for it to end. A task runs on, no longer counted by deadband, when something sends SIGTERM or SIGKILL to mxv alone.

### Arguments

- `-w`, `--workspace <WORKSPACE>`: The workspace, which defaults to the one that holds the current directory
- `--repo <CHECKOUT>`: The folder of the checkout whose task to run, which a name found in several checkouts needs
- `<TASK>`: The task, which defaults to listing the tasks, then the words to add to its command. Every word after the task belongs to the task, flags too. Takes more than one value.

### Examples

List the tasks of the workspace reading-list.

```sh
mxv run -w reading-list
```

### Columns

- `name`: The name that `mxv run` takes.
- `source`: Where the task comes from: `mxv.toml` or `mise`.
- `checkout`: The folder of the checkout that holds it, or null for the workspace manifest of a workspace with several checkouts.
- `gate`: Whether it takes a place from deadband.

Effects: destructive, not idempotent, open-world.

Writes: applies all items or none.

## `mxv archive`

Archive a workspace, so restore can rebuild it at the same commits

### Arguments

- `-w`, `--workspace <WORKSPACE>`: The workspace, which defaults to the one that holds the current directory
- `--force`: Lose what the archive cannot keep, such as a paused merge or a file over archive\_max\_file, and list it in the archive
- `--delete-volumes`: Delete the workspace's docker volumes and the data in them; without this flag the volumes stay for the restore
- `--dry-run`: Report what the command would change, and change nothing
- `--yes`: Proceed without asking for confirmation

### Examples

Print what archiving the workspace reading-list would do.

```sh
mxv archive -w reading-list --dry-run
```

Effects: destructive, not idempotent, closed-world.

## `mxv restore`

Rebuild an archived workspace at its archived commits

### Arguments

- `<NAME>`: The name of the archived workspace. Required.
- `--force`: Restore without the ignored files whose copy in the archive is missing or damaged, and list them as lost
- `--dry-run`: Report what the command would change, and change nothing
- `--yes`: Proceed without asking for confirmation

### Examples

Print what restoring the archived workspace sample would do.

```sh
mxv restore sample --dry-run
```

Effects: additive, not idempotent, open-world.

## `mxv repair`

Repair the broken records of one name that `mxv list` names `mxv repair NAME` for.

An archive that stopped before it finished, and whose pins are gone, moves aside to a hidden directory beside the other archives, which `mxv list --archived` does not list, and its private tar of ignored files stays where it is. mxv deletes neither. An older archive that stopped before it finished, beside a newer workspace of the same name, takes the name NAME-YYYY-MM-DD, from the day the archive started, or that name with -2 and on when it is taken, and mxv finishes it, so `mxv restore` of the dated name brings it back. When its pins are gone, it moves aside instead. An archive beside the workspace that a restore of it rebuilt, because the restore stopped before it deleted the archive, is deleted with its pins. mxv asks before each repair that changes something, and --yes repairs without asking. A name with nothing broken, or with a problem that another command repairs, changes nothing, and the result names that command. Each repair can run again after it stops partway.

### Arguments

- `<NAME>`: The name of the workspace or archive whose records to repair. Required.
- `--dry-run`: Report what the command would change, and change nothing
- `--yes`: Proceed without asking for confirmation

### Examples

Print what repairing the records of sample would do.

```sh
mxv repair sample --dry-run
```

Effects: destructive, idempotent, closed-world.

## `mxv remove`

Remove a workspace's checkouts, its merged branches, its directory and its record

### Arguments

- `<NAME>`: The name of the workspace to remove. Required.
- `--archived`: Delete the archive of this name and its pins, instead of a workspace
- `--force`: Discard uncommitted changes, untracked files, ignored files such as .env and a clone's stashes, and stop running units; an unmerged branch and the workspace's volumes are still kept
- `--delete-volumes`: Delete the workspace's docker volumes and the data in them
- `--delete-unmerged`: Delete branches with unmerged commits, commits on a detached HEAD, and a clone's commits that origin does not hold
- `--dry-run`: Report what the command would change, and change nothing
- `--yes`: Proceed without asking for confirmation

### Examples

Print what removing the workspace reading-list would delete.

```sh
mxv remove reading-list --dry-run
```

Effects: destructive, not idempotent, closed-world.

## `mxv clean`

Delete the disposable directories, such as target/ and node\_modules/, of idle workspaces.

A workspace is idle when none of its units run, no other process, such as a cargo build, a shell or an editor, has its working directory inside the workspace, and no file or directory of its checkout outside the disposable directories, and no commit or staged change, changed within the idle age. The disposable directories are the ignored directories named target, node\_modules, dist, build and .cache, and the paths that the `disposable` globs of the checkout's mxv.toml and of the config file match while git reports them as ignored. A directory that holds a .git entry at any depth is kept. The command deletes nothing else: no tracked file, no git data and no other ignored file. It takes each workspace's lock and checks again that the workspace is idle just before it deletes anything. A workspace that mxv cannot find, measure or clean fails alone, the others are still cleaned, and the command exits with 1. Nothing runs in the background, so a cron job or a hook runs `mxv clean --over-limit --yes` to keep the disk under the config file's `max_disposable`.

### Arguments

- `<NAME>`: The names of the workspaces to clean, separated by spaces. Required, or give one of `--all` and `--over-limit` in its place. Takes more than one value.
- `--all`: Clean every idle workspace. Required, or give one of `<NAME>` and `--over-limit` in its place.
- `--over-limit`: Clean idle workspaces, oldest first, until the disposable directories of every workspace take no more than the config file's max\_disposable. Required, or give one of `<NAME>` and `--all` in its place.
- `--idle <AGE>`: How long a checkout must go unmodified to count as idle, such as 7d, 36h or 0s; it defaults to the config file's clean\_idle, or else 7d
- `--disposable`: With --yes, also delete the paths that only mxv.toml declares disposable

  \--yes alone keeps each path that only a repository's own mxv.toml declares disposable, because no person confirmed it.
- `--dry-run`: Report what the command would change, and change nothing
- `--yes`: Proceed without asking for confirmation

### Examples

Print what cleaning every idle workspace would delete.

```sh
mxv clean --all --dry-run
```

### Columns

- `name`: The workspace's name.
- `state`: `cleaned` when the run deleted the workspace's disposable directories, `would_clean` on a dry run, or `kept`.
- `freed`: The bytes that the run freed, measured before and after each deletion, or the bytes that the disposable directories take on a dry run, which is 0 for a workspace that the run kept. A file with a hard link outside the deleted directory frees nothing.
- `disposable`: The bytes that the workspace's disposable directories took on disk before the run.
- `modified`: When a file or directory of the workspace's checkouts outside the disposable directories, or a commit or staged change, last changed, or null when none exists.
- `reason`: Why the run kept the workspace's disposable directories, or some of them, or null.
- `warnings`: Why a checkout's disposable set holds fewer globs than its `mxv.toml` declares, such as an invalid manifest, or null.

### Columns that `--long` adds

- `paths`: Each disposable path that the run deleted, or would delete, with its checkout's name in front, such as `app: target/`.
- `skipped`: Each path that the disposable set matches and that the run kept, with the reason, such as a directory that holds a `.git` entry.
- `manifest_paths`: The paths of `paths` that only the repository's own `mxv.toml` declares disposable, which the prompt named.

Effects: destructive, idempotent, closed-world.

Writes: reports each item and exits 1 when any item fails.

## `mxv reclaim`

Stop idle workspaces to free memory, or delete their build output to free disk, until the need is met.

With --need memory=SIZE, a running workspace is idle when it is not pinned, no process outside phase has its working directory inside the workspace's directory, and neither a file, commit or staged change of its checkouts nor an mxv command that named it changed within the idle age. `mxv exec` and `mxv logs --follow` name the workspace again every few minutes while they run. mxv stops the idle workspaces that were active least recently, one by one, until the resident memory of their processes, measured before the stop, covers the need, or until none is left. On Linux the memory of their containers counts too; on macOS mxv reports it separately and leaves it out, because Docker Desktop's virtual machine keeps the memory that stopped containers free for a while. Each stop is the stop of `mxv down`, which mxv makes while it holds the workspace's lock, after it checks again that the workspace is still idle. mxv marks each stopped workspace, `mxv status` reports the mark, and `mxv up` without names starts the same units again. mxv measures idle time on the wall clock, so time that the machine spends asleep counts as idle, and after the clock moves back a workspace whose recorded times lie ahead of it counts as in use until the clock passes them. With --need disk=SIZE, mxv deletes the disposable directories of idle workspaces as `mxv clean --over-limit` does, oldest first, until it frees the need, and keeps each path that only a repository's own mxv.toml declares disposable. Nothing runs in the background, so a load manager runs the command when the machine runs short.

### Arguments

- `--need <NEED>`: What to free: memory=SIZE or disk=SIZE, such as memory=2GiB. Required.
- `--idle <AGE>`: How long a workspace must go unused to count as idle, at least 10m, such as 2h; it defaults to the config file's stop\_idle, or else 2h, for memory, and to clean\_idle, or else 7d, for disk
- `--disposable`: With a disk need, also delete the paths that only mxv.toml declares disposable

  Without it, reclaim keeps each path that only a repository's own mxv.toml declares disposable, because no person confirmed it. A memory need deletes nothing, so it ignores this flag.
- `--dry-run`: Report what the command would change, and change nothing

### Examples

Print which idle workspaces stopping would free 2 GiB of memory.

```sh
mxv reclaim --need memory=2GiB --dry-run
```

### Columns

- `name`: The workspace's name.
- `state`: `stopped`, `would_stop`, `cleaned`, `would_clean` or `kept`.
- `freed`: The bytes that the run freed, or would free on a dry run, which is 0 for a workspace that it kept. For memory it is the resident memory of the workspace's processes, measured before the stop, and on Linux of its containers too.
- `containers`: For memory, the resident memory of the workspace's containers, measured before the stop, and 0 for a workspace that the run kept or a disk need. On macOS Docker Desktop's virtual machine keeps the memory that stopped containers free for a while, so freed leaves it out there.
- `active`: How long ago the workspace was last active: its checkouts changed or an mxv command named it. It is null when neither happened, and for a workspace that the run kept without measuring.
- `reason`: Why the workspace counts as idle, for one that the run stopped or cleaned, or why the run kept it.

Effects: destructive, idempotent, closed-world.

Writes: reports each item and exits 1 when any item fails.

## `mxv pin`

Keep mxv reclaim from stopping a workspace

### Arguments

- `-w`, `--workspace <WORKSPACE>`: The workspace, which defaults to the one that holds the current directory

### Examples

Keep mxv reclaim from stopping the workspace reading-list.

```sh
mxv pin -w reading-list
```

Effects: additive, idempotent, closed-world.

## `mxv unpin`

Let mxv reclaim stop a workspace again while it is idle

### Arguments

- `-w`, `--workspace <WORKSPACE>`: The workspace, which defaults to the one that holds the current directory

### Examples

Let mxv reclaim stop the workspace reading-list while it is idle.

```sh
mxv unpin -w reading-list
```

Effects: additive, idempotent, closed-world.

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
- [`mxv self runs`](#mxv-self-runs): Work with each run in mxv's usage log

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
- `p50_out_bytes`: The median of the bytes that the runs wrote to standard output, by the nearest-rank method, or null when no run printed a result.
- `p95_out_bytes`: The 95th percentile of the bytes that the runs wrote to standard output, or null when no run printed a result.

### Columns that `--long` adds

- `refused`: The runs whose command line was refused.
- `p50_ms`: The median of the runs' wall time in milliseconds, by the nearest-rank method, or null when every run printed help.
- `p95_ms`: The 95th percentile of the runs' wall time in milliseconds, or null when every run printed help.
- `cut_short`: The runs whose output a listing or stream cut short, because it showed fewer rows than its total or the output budget stopped it.
- `typed_full`: The runs that typed `--full`.
- `typed_all`: The runs that typed `--all`.
- `typed_long`: The runs that typed `--long`.

Effects: read-only, closed-world.

## `mxv self runs`

Work with each run in mxv's usage log

### Commands

- [`mxv self runs list`](#mxv-self-runs-list): List each run in mxv's usage log, grouped by session

## `mxv self runs list`

List each run in mxv's usage log, grouped by session

### Arguments

- `--since <DURATION>`: How far back to read the usage log, such as 12h, 7d or 2w. Default: `7d`.
- `--log <FILE>`: Read this usage log instead of the tool's own
- `--limit <N>`: Print at most this many rows. Default: `25`.
- `--all`: Print every row

### Examples

Print each run of the last 7 days, grouped by session.

```sh
mxv self runs list
```

### Columns

- `session`: The run's session, numbered from 1 in the order of each session's first run in the listing's window.
- `step`: The run's place in its session, from 1.
- `command`: The command path below the tool's name, such as `books show`, or the tool's name for its root command.
- `exit`: The run's exit code, or null when the run ended before it exited.
- `code`: The code of the run's failure or refusal, such as `shelf::books::not_found`, or null when the run recorded none.
- `ms`: The run's wall time in milliseconds.
- `out_bytes`: The bytes that the run wrote to standard output, or null when the run did not reach `finish`.

### Columns that `--long` adds

- `at`: When the run ended, in RFC 3339 to the second in UTC, such as `2026-10-11T11:56:40Z`.
- `caller`: The coding agent's id, such as claude-code, person or unknown.
- `flags`: The names of the flags that the caller typed, never their values.
- `help`: Whether the run printed a help page.
- `truncated`: Whether a listing or stream cut the run's output short.
- `trace`: The id of the W3C trace that the run belongs to, as 32 lowercase hex digits.
- `run`: The run's own span id, as 16 lowercase hex digits, which its child runs name as their parent.
- `parent`: The span id of the run's caller, as 16 lowercase hex digits, or null when the run started a new trace.

Effects: read-only, closed-world.

## `mxv skills`

Give coding agents mxv's skill

### Commands

- [`mxv skills install`](#mxv-skills-install): Install mxv's skill into each agent skills directory that exists
- [`mxv skills check`](#mxv-skills-check): Check that each installed copy of mxv's skill matches this mxv
- [`mxv skills print`](#mxv-skills-print): Print mxv's skill

## `mxv skills install`

Install mxv's skill into each agent skills directory that exists

### Arguments

- `--dir <DIR>`: Install into this skills directory in place of the agent skills directories
- `--force`: Replace a file or a link that install did not write with the skill's own folder

### Examples

Install mxv's skill into each agent skills directory that exists.

```sh
mxv skills install
```

Install it, replacing a file or a link that install did not write.

```sh
mxv skills install --force
```

### Columns

- `state`: What install did: failed, skipped, written or unchanged.
- `dirs`: The skills directories that share this copy, as named. Directories that resolve to one place through a symbolic link share one copy.
- `reason`: Why install skipped or failed, or what it replaced under --force, such as `linked to /src/shelf/skill`, or null.

### Columns that `--long` adds

- `path`: The SKILL.md file that install wrote or left, at its real path.

Effects: additive, idempotent, closed-world.

## `mxv skills check`

Check that each installed copy of mxv's skill matches this mxv

### Arguments

- `--dir <DIR>`: Check this skills directory in place of the agent skills directories

### Examples

Report each agent skills directory whose copy is missing or differs.

```sh
mxv skills check
```

### Columns

- `state`: Whether the copy is the binary's: missing, differs, unreadable, linked or current.
- `dirs`: The skills directories that share this copy, as named. Directories that resolve to one place through a symbolic link share one copy.
- `reason`: Why the copy differs, cannot be read or is linked, such as `written by shelf 0.2.0` or `linked to /src/shelf/skill`, or null.

### Columns that `--long` adds

- `path`: The SKILL.md file that was checked, at its real path.

Effects: read-only, closed-world.

## `mxv skills print`

Print mxv's skill

### Examples

Print mxv's skill.

```sh
mxv skills print
```

Effects: read-only, closed-world.

## `mxv completion`

Set up mxv's completion in a shell

### Commands

- [`mxv completion install`](#mxv-completion-install): Install mxv's completion where the shell loads it
- [`mxv completion check`](#mxv-completion-check): Check that mxv's installed completion file matches this mxv
- [`mxv completion print`](#mxv-completion-print): Print mxv's completion registration script for a shell

## `mxv completion install`

Install mxv's completion where the shell loads it

### Arguments

- `--shell <SHELL>`: Install for this shell in place of the one that SHELL names. One of `bash`, `elvish`, `fish`, `powershell`, `zsh`.
- `--force`: Replace a file or a link that install did not write; a link's target stays as it is

### Examples

Install mxv's completion for fish.

```sh
mxv completion install --shell fish
```

Install it for zsh, replacing a file or a link that install did not write.

```sh
mxv completion install --shell zsh --force
```

### Columns

- `state`: What install did: failed, skipped, written or unchanged.
- `shell`: The shell: bash, elvish, fish, powershell or zsh.
- `path`: The completion file that install wrote or left. When the file is a link to a file that install wrote, the path of that file.
- `reason`: Why install skipped or failed, or what it replaced under --force, such as `linked to /src/shelf/shelf.fish`, or null.

Effects: additive, idempotent, closed-world.

## `mxv completion check`

Check that mxv's installed completion file matches this mxv

### Arguments

- `--shell <SHELL>`: Check the file of this shell in place of the one that SHELL names. One of `bash`, `elvish`, `fish`, `powershell`, `zsh`.

### Examples

Report zsh's completion file when it is missing or differs.

```sh
mxv completion check --shell zsh
```

### Columns

- `state`: Whether the file holds the script that install writes: missing, differs, unreadable, linked or current.
- `shell`: The shell: bash, elvish, fish, powershell or zsh.
- `path`: The completion file that was checked. When the file is a link to a file that install wrote, the path of that file.
- `reason`: Why the file differs, cannot be read or is linked, such as `linked to /src/shelf/shelf.fish`, or null.

Effects: read-only, closed-world.

## `mxv completion print`

Print mxv's completion registration script for a shell

### Arguments

- `--shell <SHELL>`: Print the script of this shell in place of the one that SHELL names. One of `bash`, `elvish`, `fish`, `powershell`, `zsh`.

### Examples

Print the registration script for fish.

```sh
mxv completion print --shell fish
```

Effects: read-only, closed-world.
