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

### Changed

- `up` of a container app that runs as another user than root and mounts a
  volume now gives the root directory of each such volume to that user,
  through a helper container. The helper runs busybox pinned by digest, so
  the machine needs that image or access to Docker Hub to pull it. Without
  them `up` fails for that app before it removes any container. Set the
  app's `container.owner` to `keep` to leave its volumes alone and run no
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
- The run record moves to `phase/run/v6`, which records the hash of each
  host process's part of the plan. phase reads every earlier version and
  writes v6. An older mxv or phase refuses a v6 record with
  `phase::record::newer` and asks for an upgrade.
