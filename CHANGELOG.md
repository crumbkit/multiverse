# Changelog

This file records the changes to multiverse, mxv and mxv-phase that a
workspace may need to act on.

## Unreleased

### Added

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
