# Changelog

This file records the changes to multiverse, mxv and mxv-phase that a
workspace may need to act on.

## Unreleased

### Added

- A manifest's `routes` list names ports that serve HTTP, and mxv serves
  each at `https://<port>.<workspace>.mxv.localhost` through one Caddy for
  all workspaces. `mxv up`, `restart`, `down`, `remove`, `archive`, `restore`
  and `ports reset` keep the routes in line with the workspace records, and a
  failure to do so is a warning with an error code and never fails the verb.
- `mxv proxy status`, `start`, `stop` and `trust` operate the proxy. The
  config keys `proxy_http`, `proxy_https` and `caddy` set its addresses and
  program. `mxv proxy trust` needs a terminal and a confirmation.
- `.github/workflows/proxy.yml` runs the tests that start a real Caddy, with
  the Caddy release pinned by digest. It is not a required check.

### Changed

- `mxv archive`, `mxv restore` and `mxv remove` succeed with nothing to do
  when their goal already holds, and their documents gain a `state` of
  `done`, `planned` or `unchanged`. `mxv archive -w NAME` of a name that
  only a finished archive holds lists the archive's pins and names
  `mxv restore NAME`, where it refused with `mxv::archive::name_taken`.
  `mxv restore NAME` of a name that a workspace holds and no archive needs
  restoring exits 0. When an earlier restore stopped after it rebuilt the
  workspace and before it deleted the archive, so the archive has the
  workspace's stable id, `mxv restore` asks and then deletes the archive
  with its pins, which its new `stale_pins` field names, where it refused
  with `mxv::restore::name_taken`. It still refuses when the workspace's
  directory or a checkout's directory is gone, because the archive may
  then hold the only copies of the workspace's files, and `mxv repair`
  keeps such an archive too. When the records change after that question,
  `mxv restore` deletes nothing and fails with the new code
  `mxv::restore::changed`. `mxv remove NAME` of a name that no
  workspace has, and `mxv remove --archived NAME` of a name that no archive
  has, exit 0, where they failed with `mxv::workspace::not_found` and
  `mxv::archive::not_found`. Their `Effects:` lines now say idempotent. The
  library's `ArchivePlanned::Unchanged` and `RestorePlanned::Unchanged`
  report these cases.
- `mxv archive` keeps a checkout's git state instead of refusing it. It
  keeps the staged and unstaged changes and the untracked files as two
  commits that only its pins name, so the branch and the index never move,
  and `mxv restore` puts them back uncommitted. It also keeps each stash,
  the other local branches of a clone, the tags that origin does not hold,
  the per-worktree references, and each commit that no branch reaches,
  which comes back as `refs/mxv/kept/<commit>`. The pin of `HEAD` is now
  `refs/mxv/archives/<id>/<folder>/head`, and an archive record of version
  1 still restores.
- An archive refuses only what it cannot keep: a paused merge, rebase,
  cherry-pick, revert or bisect, a repository that git does not track, the
  work of a submodule, a file marked skip-worktree or assume-unchanged with
  local edits, a file that git does not list, an ignored path that holds a
  repository or is a socket, an entry of the workspace's directory such as a
  socket, and a file over `archive_max_file`. The failure code is `mxv::archive::unkeepable`, which
  replaces `mxv::archive::unsaved_work`, and each line says what to do.
  `--force` loses only those items.
- `mxv archive` keeps each checkout's ignored files outside the disposable
  set, such as `.env`, in a private tar at
  `$XDG_DATA_HOME/mxv/archives/<id>/ignored.tar`, readable only by the
  person, and `mxv restore` writes them back with their modes. The archive
  record lists each entry's path, kind, size, mode and digest. `mxv restore`
  refuses a damaged entry with `mxv::restore::ignored_damaged`, and
  `--force` restores without it. A path that the rebuilt checkout already
  holds refuses with `mxv::restore::ignored_taken`. The archive result and
  the dry run name each ignored file kept and each disposable directory
  deleted with its size, and `mxv list --archived` has the new field
  `ignored`. The archive document gains the rows `ignored` and
  `disposable`, and the restore document gains `put_back[].ignored` and
  `lost`.
- The `mxv archive` document is version 2. It replaces `wip` with `kept`,
  which lists what the archive kept of each checkout. The `mxv restore`
  document gains `put_back`, which lists what the restore put back, and
  warns about each submodule that comes back uninitialised.

### Added

- `mxv list` and `mxv list --archived` gain a `repair` column, which names
  the command that fixes each broken row, and list the broken rows first. A
  workspace whose checkout directories are all gone names `mxv remove NAME`,
  unless the directory that holds the workspace is gone too, as on a disk
  that is not mounted.
  An archive that stopped before it finished names `mxv archive -w NAME`,
  and its problem now reads "the archive stopped before it finished". A
  record that mxv cannot read says to move the file aside by hand, and the
  row of an archive record that mxv cannot read is named by the archive's
  directory instead of `archive`. An
  unfinished archive whose name a newer workspace holds shows only in
  `mxv list --archived`, and no longer marks the newer workspace's row.
  The library's `multiverse::health::classify` finds these records.
- `mxv repair NAME` repairs the broken records that `mxv list` names it
  for, and `--dry-run` prints what it would do. It asks before it changes
  anything, and `--yes` repairs without asking. An unfinished archive whose
  pins are gone moves aside to the hidden directory `.NAME.<id>`, and
  nothing is deleted; that directory and the archive's
  `$XDG_DATA_HOME/mxv/archives/<id>/ignored.tar` stay until
  `mxv repair --orphans --delete-aside` deletes them. An older unfinished archive beside a newer workspace of the same name is
  finished under the name `NAME-YYYY-MM-DD`, so `mxv restore` of that name
  brings it back. An archive left beside the workspace that a restore of it
  rebuilt is deleted. The new problems read "the archive stopped before it
  finished, and its pins are gone" and "the restore stopped before it
  deleted the archive". `mxv archive -w` refuses to finish an archive whose
  pins are gone, with `mxv::archive::pins_gone`, and the recovery of
  `mxv::archive::name_taken` for an older archive now names `mxv repair`.
  A repair that stopped while it moved an archive to its dated name shows
  as "a repair stopped while it moved the archive to its dated name" on
  both records, with `mxv repair` of the older name, and `mxv archive -w`
  of either name refuses with `mxv::archive::rename_unfinished`. A repair
  whose records change after its question repairs nothing and fails with
  `mxv::repair::changed`, and an older archive whose dated names are all
  taken fails with `mxv::repair::names_taken`. A repair of a name whose
  archive record cannot be read fails with the read error. A repair that
  finishes a stopped rename holds the locks of both names.
  The library's `classify` now takes the backend that reads the pins, and
  `Workspaces::repair` carries out the repairs. phase exports its calendar
  functions as `phase::calendar`.
- `mxv repair --orphans` lists, with their sizes, the archives that a
  repair moved aside and the directories of ignored files whose archive no
  record names. Each holds the only copies of its files, so it is deleted
  only with `--delete-aside`, and `--yes` only skips the question. The
  listing and `--dry-run` write nothing. When an archive record cannot be
  read, the command fails with that record's error and deletes nothing.
  The `mxv/repair/v1` document gains an `orphans` list, and its `name` is
  null with `--orphans`. The library's `Workspaces::orphans` and
  `Workspaces::sweep_orphan` find and delete these items.
- `mxv repair --orphans` also lists the run files of a name that no
  workspace has and phase's run record and logs of a workspace id that no
  record holds, and deletes them after the person confirms. phase's sweep
  first stops the processes and removes the containers, networks and run
  volumes that the run record names. `--delete-volumes` also deletes the
  docker volumes that hold the data of such an id, before its logs, so a
  docker failure leaves the logs and the next sweep lists the id again.
  With `--delete-volumes` the listing also finds, by docker's workspace-id
  label, the volumes of an id that phase keeps nothing of, such as those
  that a removal stopped after it forgot the id leaves. Each is a
  `run_record` row whose path is its first volume's name, and a row's
  `volumes` names the volumes of its id. It counts only the volumes whose
  run-record label names this state directory, so it never lists those of
  another state directory, and it asks docker once for the whole listing.
  A docker that does not answer then fails the listing before anything is
  deleted, run files included. A directory of the
  archives without its own record is listed as kept and never deleted. A
  deletion that fails partway has the row state `partly_deleted`, and its
  `deleted` field names the paths that are gone. A row now carries `name`,
  `id`, `deleted` and `volumes`.
- `Runs::forget` deletes the logs of a workspace id only once phase's sweep
  removed its run record, so the logs stay while phase keeps the record.
  `mxv remove` and `mxv archive` then report a warning for each process
  that phase kept the record for, which says to stop that process.
- The config key `archive_max_file` sets the largest file of a checkout's
  uncommitted work that `mxv archive` keeps, such as `"1GiB"`. It defaults
  to 100 MiB.

### Removed

- `mxv archive --wip` is gone, because an archive keeps uncommitted work
  without a commit on the branch.

## [0.1.1] - 2026-11-11

### Fixed

- The release binaries build again, because mxv no longer depends on an
  unmaintained TOML crate. Version 0.1.0 was tagged but never released.

## [0.1.0] - 2026-11-11

### Added

- `deadband status` and `deadband top` show the workspace of each job that
  mxv admits and of each job that a person runs by hand inside a workspace.
  mxv names the workspace in every build and task it asks deadband for, and
  `mxv new`, `mxv add` and `mxv restore` write an empty file
  `.deadband-workspace` in the workspace's directory, which deadband finds
  by walking up from a job's directory. `mxv up`, `mxv restart`, `mxv run`
  and `mxv exec` write the file when it is missing, so a workspace made
  earlier gains it, and warn when they cannot. `mxv remove` and
  `mxv archive` delete an empty marker, and `mxv restore` writes it again.
  `.deadband-workspace` is no longer allowed as a checkout's folder. mxv
  now needs the deadband commit that names workspaces, crumbkit/deadband#19.

- `mxv completion install` writes mxv's shell completion where the shell loads
  it, for bash, zsh and fish, with `--shell` naming the shell when `SHELL`
  does not. `mxv completion check` exits with 1 when the installed file is
  missing or differs, and `mxv completion print` prints the registration
  script. mxv also answers a shell's completion request, which a shell makes
  by running mxv with `COMPLETE` set to its name. mxv now builds on crumbkit
  0.3.1 with its `complete` feature.
- `mxv skills install` writes mxv's agent skill into each agent skills
  directory that exists, `~/.claude/skills`, `~/.agents/skills` and
  `$CODEX_HOME/skills`, or into the directory that `--dir` names.
  `mxv skills check` exits with 1 when an installed copy is missing or
  differs from the installed mxv's, and `mxv skills print` prints the skill.
  The skill is crates/mxv/skill/SKILL.md, embedded in the binary.
- `mxv self runs list` prints each run in mxv's usage log, grouped by
  session, as the `mxv/self-runs-list/v1` document. mxv now builds on
  crumbkit 0.3, so the figures of `mxv self figures list` also give each
  command `p50_ms`, `p50_out_bytes` and `cut_short`, and a failure's JSON
  recovery can have the kind `terminal`, which names a command that a person
  must run in a terminal.
- `mxv exec` takes no UNIT to run a one-off command on the host in a
  workspace, as `mxv exec -- COMMAND`. The command needs no running unit and
  no `mxv.toml`. It receives the workspace's ports and every service's
  variables, and mxv starts no services or units. `--repo CHECKOUT` chooses
  the checkout it runs at the top of, and otherwise it runs in the current
  directory when that is inside a checkout. It takes one place from deadband,
  and `--no-gate` asks deadband nothing. `mxv exec UNIT` behaves as before and
  refuses both flags.
- `mxv archive` removes a workspace's directory and name and keeps each
  checkout's commit in a reference under `refs/mxv/archives/`, and
  `mxv restore` rebuilds the workspace at those commits with the same stable
  id, so its docker volumes attach again. `mxv list --archived` lists the
  archives and `mxv remove --archived NAME` deletes one. mxv writes archives
  to `$XDG_STATE_HOME/mxv/archives`, and a tool that read the failure code
  `mxv::command::unbuilt` no longer sees it. The JSON document of `mxv list`
  has the new fields `archived` and `local_only`, and the document of
  `mxv remove` has the new field `pins`. An archive refuses and names an
  untracked nested repository, a submodule's unsaved work, a
  skip-worktree or assume-unchanged file with local edits, a file that git
  does not list, a commit that only an unusual reference keeps, and a paused
  git operation. The new failure
  codes `mxv::archive::record_too_large` and
  `mxv::archive::volumes_in_use` report an archive record over 1 MiB and
  docker volumes that a live workspace uses.
- mxv-phase has a `sandbox` feature, on by default, that applies the
  operating system's sandbox through nono. A program that links mxv-phase
  with `default-features = false` builds without nono, and with it without
  serde_json's `preserve_order`, which nono turns on for the whole build.
  In such a build an app that declares permissions runs unsandboxed, `exec`
  runs its command unsandboxed, the launcher refuses `--sandboxed`, and
  `status` gives the reason "phase was built without its sandbox feature".
- `[containers.<name>] owner` in `mxv.toml` sets the owner of the
  container's named volumes. Its value is a numeric `uid` or `uid:gid`, such
  as `"999:999"`, or `"keep"`. mxv refuses a name, a malformed value, the id
  4294967295 and an `owner` on a container that mounts no volume when it
  reads the manifest. mxv-phase has a public `is_volume_owner` function that
  says which values a plan accepts.

### Removed

- mxv no longer reads, stops or reports what a version from before phase
  started. On each workspace that such a version started, run
  `mxv down -w <WORKSPACE>` with that earlier mxv before you upgrade. After
  the upgrade mxv ignores the run files under
  `$XDG_STATE_HOME/mxv/runs/<WORKSPACE>/` and `mxv remove` deletes them, but
  a process or container that still runs is no longer stopped or reported.
  A process left running keeps its ports. When it holds the port that a
  process declares as its `listen` port, `mxv up` fails with
  `mxv::port::taken`. When it holds a port that the process only receives,
  the new copy cannot bind it, and `mxv up` reports `mxv::process::exited`
  or a readiness failure. A container left running keeps its published
  ports, and `mxv up` fails with `mxv::port::taken`. Stop what is left by
  hand.
- A container whose data sits in a volume named
  `mxv-<id8>-<WORKSPACE>.<volume>` starts on an empty volume, because
  `mxv up` no longer checks for that name, and `mxv remove --delete-volumes`
  no longer deletes it. To keep the data, follow these steps in order. Do
  not create the new volume yourself, because `mxv up` refuses a volume
  without the workspace's labels.
  1. Run `mxv up -w <WORKSPACE>` and then `mxv down -w <WORKSPACE>`, which
     create the new volume with its labels and leave no container using it.
  2. Find the new volume's name, `phase-<hash>.<volume>`, with
     `docker volume ls --filter label=mxv.workspace-id=<ID> --filter label=mxv.volume=<volume> --format '{{.Name}}'`.
     `<ID>` is the `id` field of
     `$XDG_STATE_HOME/mxv/workspaces/<WORKSPACE>.json`.
  3. Empty the new volume, because the container may have initialised it:
     `docker run --rm -v <NEW>:/to busybox:1.37.0 find /to -mindepth 1 -delete`.
  4. Copy the old data:
     `docker run --rm -v <OLD>:/from -v <NEW>:/to busybox:1.37.0 cp -a /from/. /to/`.
  5. Run `mxv up -w <WORKSPACE>`. Delete the old volume with
     `docker volume rm <OLD>` when you no longer need it.
- An anonymous volume of a container's image that an earlier `mxv down`
  kept stays in docker, and nothing names its workspace any more, because
  `mxv remove` no longer lists it, refuses for it or deletes it, and it
  deletes the `.volumes` file that held its name. If you are removing the
  workspace and its data before you upgrade, run
  `mxv remove --delete-volumes` with the earlier mxv. Otherwise, before
  `mxv remove` deletes the file, and only when you no longer need its data,
  run `docker volume rm` on each name in
  `$XDG_STATE_HOME/mxv/runs/<WORKSPACE>/.volumes` that does not start with
  `mxv-`. An image with a `VOLUME` path keeps its only copy of that data in
  such a volume. That file also lists the workspace's named volumes, and a name
  that starts with `mxv-` is one of them, so deleting it destroys the data
  that the previous item keeps. This command prints only the anonymous
  volumes' names:
  `jq -r '.volumes[] | select(startswith("mxv-") | not)' <FILE>`. After
  `mxv remove` has deleted the file,
  `docker volume ls --filter label=com.docker.volume.anonymous --filter dangling=true`
  lists every anonymous volume that no container uses, across all of
  docker, so check each one before you delete it.
- The failure codes `mxv::workspace::legacy_running` and
  `mxv::volume::not_moved` and the `group_alive` field of the documents of
  the unit verbs are gone. The field was null for every unit that phase
  runs.
- mxv-phase's `AppOutcome::AwaitingGrant`, `AppOutcome::awaiting_grant` and
  the code `phase::grant::awaiting` are gone. The permission check runs
  before `Phase::up`, so nothing could return them.

### Changed

- `mxv up` and `mxv restart` compare the permissions of the selected host
  units and their dependencies with what a person has granted to the
  repository, before anything builds, stops or starts. A unit that is new,
  that holds a permission entry the grant lacks, or that would leave the
  sandbox asks for more. At a terminal, mxv lists what each unit asks for
  and asks `Grant them? [y/N]`. Without a terminal, or with `--no-input`,
  the command fails with `mxv::permissions::not_granted`, whose recovery is
  `mxv up` run by a person in a terminal, and every running unit keeps
  running. `mxv exec UNIT` refuses with the same code and never asks. No
  flag or variable approves a grant. Each repository already in use asks
  once after the upgrade, so an agent's `mxv up` there refuses until a
  person answers in a terminal. The grant lives in
  `$XDG_STATE_HOME/mxv/phase/grants/`, is shared by every workspace of the
  repository, only grows, and is never deleted by mxv. Move its file away
  to be asked about every unit again.
- `mxv down` with names keeps a named unit that a running unit outside the
  stopped set depends on, and what that unit depends on in turn. Each kept
  unit fails the command with `mxv::process::still_needed`, which names the
  units that need it and the `mxv down` that stops them all. The other
  named units stop, and the command exits 1. `mxv down` with no names and
  `mxv restart` stop what they stopped before.

- `mxv clean --force` is now `mxv clean --disposable`. It deletes, with
  `--yes`, the paths that only a repository's `mxv.toml` declares
  disposable. The old spelling is gone and is not accepted, so update any
  script or cron job that passes it. The `--force` flags of `remove` and
  `archive` keep their names. `mxv reclaim --need disk=<SIZE>` takes the
  same `--disposable` flag and deletes those paths too when it is given.
  Without the flag, reclaim keeps them, as before. The `reason` of a row
  that kept such a path now names the command that deletes it: rerunning
  reclaim with `--disposable`, or for `clean`, answering its prompt or
  running `mxv clean <NAME> --yes --disposable`.
- A workspace with several checkouts now reads its workspace manifest
  strictly: while one checkout's `mxv.toml` cannot be parsed, the read
  fails with that file's error. Before this change the read skipped the
  broken file and checked the workspace manifest against the names of the
  other checkouts, so it could report an undeclared name with a different
  message. The commands that read the workspace manifest for its
  `disposable` globs, which are `remove`, `archive` and `clean`, now warn
  and use no `disposable` globs of the workspace manifest in any checkout
  while one checkout's manifest is broken. `remove` and `archive` without
  `--force` can then refuse because of ignored files that only those
  globs cover, and `clean` reclaims fewer paths. `list` and `show` reach the
  same read through `disk_usage`, so their disposable byte figures shrink
  too, and `show` carries the same warning. Fix the broken file to
  restore the globs.
- mxv uses crumbkit 0.2.0, which changes some documents that a tool may
  read. `mxv list --json` and `mxv list --archived --json` now print a
  top-level `next` array of next steps when there are any, and the schemas
  of `mxv list` and `mxv status` gain the `next` field, although `mxv status`
  never prints it. `mxv commands list --json --full` gives the root row a
  `goals` field that holds the headings of the help and the commands under
  each. The descriptions in the schemas of the failure code and the output
  budget no longer say "in the tool's spelling".
- `up` of a container app that runs as another user than root and mounts a
  volume now gives the root directory of each such volume to that user,
  through a helper container. The helper runs busybox pinned by digest, so
  the machine needs that image or access to Docker Hub to pull it. Without
  them `up` fails for that app before it removes any container. Set
  `owner = "keep"` in the container's table of `mxv.toml`, or the app's
  `container.owner` in a phase plan, to leave its volumes alone and run no
  helper.
- A sandboxed process can connect over loopback only to the units that its
  `permissions.connect` names. Before this change the sandbox opened the
  ports of every unit in the process's `depends_on`, and `connect` was only
  checked to name units of `depends_on`. A workspace whose sandboxed
  process reaches a dependency that its `connect` leaves out must add that
  unit to `connect`; otherwise the process's connections to it fail with
  "Operation not permitted" on macOS.
- A sandboxed process can no longer reach the ports that another unit
  only receives. Naming a unit in `connect` opens the port that unit
  listens on, the ports its `permissions.listen` names, or the ports it
  publishes if it is a container. Before this change it also opened every
  port that the unit received through `ports` or an `env` placeholder. An
  unsandboxed process that serves a port without declaring it as its
  `listen` port must declare it, or sandboxed processes cannot reach it.
- `mxv up` restarts a running host process whose part of the plan changed,
  such as its `argv`, its ports, its `permissions` or its sandbox, and
  prints an `info:` line under it that says so. A changed variable value
  alone does not restart it; `mxv restart` applies that. The first `up`
  after this upgrade restarts every running host process once, because
  their run records do not say which part of the plan each one started
  from. That restart also applies the sandbox rules above. `mxv exec` builds
  its sandbox afresh and applies them at once.
  An `up` that fails later leaves each process it restarted running and
  prints a `warning:` line for it.
- `up` replaces a running container when the value of one of its variables
  that is not a secret changes, such as LocalStack's `SERVICES` going from
  `s3` to `s3,sqs`. Before this change the new value waited for
  `mxv restart`. A renewed secret never replaces a container. phase compares
  an HMAC-SHA-256 digest of the values, keyed by 32 random bytes that it
  creates in `env-digest.key` in its state directory with mode 0600. phase
  refuses a key file that others can read and fails `up` for the app,
  leaving its containers running. Remove the file to make a new key.
- The first `up` after this upgrade replaces every running container once,
  because their recorded hashes hold no digest. A replaced container loses
  everything outside its named volumes, so LocalStack loses its in-memory
  data and `mxv-provision` creates its resources again. Later `up`s keep
  each container, and the run record stays at `phase/run/v6`.
- `up` now resolves the environment of each running container on every run.
  An `EnvironmentSource` that cannot resolve a container without secret
  files makes `up` report the app as errored, and leaves the container
  running.
- The run record moves to `phase/run/v6`, which records the hash of each
  host process's part of the plan. phase reads every earlier version and
  writes v6. An older mxv or phase refuses a v6 record with
  `phase::record::newer` and asks for an upgrade.

### Fixed

- mxv works on a machine where git's `safe.bareRepository` setting is
  `explicit`. Before this fix, `mxv new`, `mxv add` and `mxv archive` failed
  there with "not in a git directory" or "cannot use bare repository",
  `mxv restore` refused and said that the commit was missing, and
  `mxv remove --archived` exited 0, printed a warning and left the archive's
  pin in the mirror. A pin that an earlier `mxv remove --archived` left
  behind stays in the mirror.
