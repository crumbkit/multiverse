# Security

mxv runs on one person's machine, as that person.

## Full-document conditions

| Condition | Holds | Evidence |
| --- | --- | --- |
| Holds tenant data | No | mxv has no tenants, server or shared store. |
| Holds personal data | Only on the person's own machine | The next section describes it. |
| Is deployed | No | `release.yml` publishes command-line archives. |

## What mxv stores

- Workspace records hold names, paths (usually with the user name), branches, commits and ports.
- Mirrors hold each origin's branches and tags. `AGENTS.md` holds origins without credentials.
- Archives (mode 0700) hold workspace root files and origins, which keep an SSH user name.
- Run records (0600) hold process ids and no environment values. `env-digest.key` keys digests of non-secret values.
- App logs (0600) and keeper diagnostic logs hold what processes print, unrotated.
- Docker volumes hold what containers wrote.
- `usage.jsonl` holds command paths, flags and exit codes.

## Credentials

mxv has no secret store, and a manifest's `secrets` key fails. A manifest `env` value is plain text there; `Redacted` keeps it out of output.

A password in an origin URL reaches git's command line and the clone's `.git/config`. mxv removes it from messages and records.

Host processes receive `PATH`, `HOME`, manifest variables and the `PASSED_THROUGH` allowlist, which includes `SSH_AUTH_SOCK` and excludes `AWS_*`.

## What mxv trusts

A repository's `mxv.toml` runs as code. `mxv up` and `restart` run its `argv` and `build`, run its container `image` with `command`, `env`, volumes and ports, and start its services' backends and the `mxv-provision` oneshot. `mxv new` copies ignored directories its `clone_on_new` globs name (`clone_globs`), unprompted.

- `check_manifest_source` refuses a `--manifest` file under the root of the workspaces or inside a git work tree. A template must be a regular file outside that root.
- Clones and mirrors start without hooks (`--template=`), but a global `core.hooksPath` still applies. A worktree workspace uses its repository's hooks.
- A workspace manifest is trusted only as a regular one-link file that no checkout holds (`manifest_trusted`).

Built-in, config-file and trusted workspace-manifest disposable globs need only the prompt or `--yes`. A repository manifest's glob needs the prompt or `--force`: `clean --yes` holds it back and `remove --yes` refuses. `reclaim --need disk` asks nothing and holds it back.

## What mxv deletes

Guards are in order.

- `clean` deletes disposable directories. Guards: unlocked `busy`; workspace lock; fresh measure; `busy`; `undeletable` (gone, links, changed kind, outside the checkout, mount points, nested repositories, tracked).
- `reclaim` stops units, and disk needs use `clean`. Guard: `reclaim::recheck` under the lock.
- `remove` deletes checkouts, a worktree repository's merged branches (unmerged with `--delete-unmerged`) and volumes. Guards: running units need `--force`; volumes need `--delete-volumes`; `plan_removal`; prompt; `loses_more` on a fresh plan. No workspace lock covers `Workspaces::remove`.
- `archive` deletes checkouts, root originals and, with `--delete-volumes`, volumes. Guards: workspace lock; `admit`; `refuse_outsider`; snapshot comparison in `retire`.
- `remove --archived` deletes the archive, its pins and, with `--delete-volumes`, volumes. Guards: refuses `Retiring`; prompt; workspace lock; `refuse_live_volumes`.

## Sockets and ports

mxv keeps no listener open. `ports.rs` and phase bind loopback ports briefly to test them. The ledger allocates from `ports_range`, by default 20000 to 29999, and fails when none is free. Containers publish on 127.0.0.1. `admission.rs` connects to deadband's Unix socket; with no daemon, builds start at once.

## Sandbox

A process runs under nono only when it declares `[permissions]`. phase reports Seatbelt as partial and Landlock as unsandboxed.

## Reporting a problem

Private vulnerability reporting is off. Open an issue at <https://github.com/crumbkit/multiverse/issues>, and leave out secrets.
