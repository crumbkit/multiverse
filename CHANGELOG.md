# Changelog

This file records the changes to multiverse, mxv and mxv-phase that a
workspace may need to act on.

## Unreleased

### Changed

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
  local edits, a file that git does not list, an ignored file, an entry of
  the workspace's directory such as a socket, and a file over
  `archive_max_file`. The failure code is `mxv::archive::unkeepable`, which
  replaces `mxv::archive::unsaved_work`, and each line says what to do.
  `--force` loses only those items.
- The `mxv archive` document is version 2. It replaces `wip` with `kept`,
  which lists what the archive kept of each checkout. The `mxv restore`
  document gains `put_back`, which lists what the restore put back, and
  warns about each submodule that comes back uninitialised.

### Added

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
