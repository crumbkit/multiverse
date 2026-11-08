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
- A sandboxed process that was running before the upgrade keeps the
  sandbox it started with, because `mxv up` leaves a running process as
  it is. Run `mxv restart -w <workspace> <process>` to apply the new rules
  to it. `mxv exec` builds its sandbox afresh and applies them at once.
