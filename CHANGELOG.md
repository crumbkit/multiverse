# Changelog

This file records the changes to multiverse, mxv and mxv-phase that a
workspace may need to act on.

## Unreleased

### Added

- `mxv archive` removes a workspace's directory and name and keeps each
  checkout's commit in a reference under `refs/mxv/archives/`, and
  `mxv restore` rebuilds the workspace at those commits with the same stable
  id, so its docker volumes attach again. `mxv list --archived` lists the
  archives and `mxv remove --archived NAME` deletes one. mxv writes archives
  to `$XDG_STATE_HOME/mxv/archives`, and a tool that read the failure code
  `mxv::command::unbuilt` no longer sees it. The JSON document of `mxv list`
  has the new fields `archived` and `local_only`, and the document of
  `mxv remove` has the new field `pins`. An archive refuses and names an
  untracked nested repository, a submodule's unsaved work, and a
  skip-worktree or assume-unchanged file with local edits. The new failure
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
  `mxv remove` deletes the file, run `docker volume rm` on each name in
  `$XDG_STATE_HOME/mxv/runs/<WORKSPACE>/.volumes` that does not start with
  `mxv-`. That file also lists the workspace's named volumes, and a name
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

### Changed

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
