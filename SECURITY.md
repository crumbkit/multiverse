# Security

mxv runs locally as one person.

## Full-document conditions

| Condition | Holds | Evidence |
| --- | --- | --- |
| Holds tenant data | No | mxv has no tenants or server. |
| Holds personal data | Only locally | Described next. |
| Is deployed | No | `release.yml` publishes command-line archives. |
| Uses OpenTofu | No | No tracked `.tf` or `.tofu` file. |

## What mxv stores

- Workspace records hold names, paths (usually with the user name), branches, commits and ports.
- Mirrors hold each origin's branches and tags, and archive pins, which can be unpushed or `--wip` commits.
- Archives (mode 0700) hold workspace root files and origins, which keep an SSH user name.
- Run records (0600) hold process ids and no environment values. `env-digest.key` keys digests of non-secret values.
- App logs (0600) hold what apps print, unrotated. Keeper diagnostic logs hold the keeper's notes and standard error.
- Docker volumes hold what containers wrote.

## Credentials

mxv has no secret store, and a manifest's `secrets` key fails.

A password in an origin URL reaches git's command line and the clone's `.git/config`. mxv removes it from messages and records.

Host processes receive `PATH`, `HOME`, manifest-derived variables and the `PASSED_THROUGH` allowlist, which includes `SSH_AUTH_SOCK` and excludes `AWS_*`.

## What mxv trusts

A repository's `mxv.toml` runs as code. `mxv up` and `restart` run its `argv` and `build`, run its container `image` with `command`, `env`, volumes and ports, and start its services' backends and the `mxv-provision` oneshot. `mxv new` copies ignored directories its `clone_on_new` globs name unprompted. `mxv run` runs a task's `argv` without a shell, or `mise run <name>`, unsandboxed, untimed, on the caller's terminal. Listing runs `mise tasks ls --json` in each checkout, and mxv never trusts a mise config.

- `check_manifest_source` refuses a `--manifest` file under the root of the workspaces or inside a git work tree. Templates must be regular files outside that root.
- Clones and mirrors start without hooks (`--template=`), but a global `core.hooksPath` still applies.
- A workspace manifest is trusted only as a regular one-link file that no checkout holds.

Built-in, config-file and trusted workspace-manifest disposable globs need only the prompt or `--yes`. A repository manifest's glob needs the prompt or `--force` (`--disposable` for `clean` and `reclaim`): `clean --yes` and `reclaim --need disk` hold it back; `remove --yes` and `archive --yes` refuse.

## What mxv deletes

Guards are in order.

- `clean` deletes disposable directories. Guards: unlocked `busy`; prompt; workspace lock; fresh measure; `busy`; `undeletable`.
- `reclaim` stops units; disk needs use `clean`. Guard: `reclaim::recheck` under the lock.
- `remove` deletes checkouts, a worktree repository's merged branches (unmerged with `--delete-unmerged`) and volumes. Guards: running units need `--force`; volumes need `--delete-volumes`; `plan_removal`; prompt; `loses_more` on a fresh plan. No workspace lock covers `Workspaces::remove`.
- `archive` deletes checkouts, root originals and, with `--delete-volumes`, volumes. Guards: workspace lock; `admit`; `refuse_outsider`; prompt; snapshot comparison in `retire`.
- `remove --archived` deletes the archive, its pins and, with `--delete-volumes`, volumes. Guards: refuses `Retiring`; prompt; workspace lock; `refuse_live_volumes`.

## Sockets and ports

mxv keeps no listener open. The ledger allocates from `ports_range`, by default 20000 to 29999, and fails when none is free. Containers publish on 127.0.0.1. mxv connects to deadband's Unix socket.

## Sandbox

A process runs under nono only when it declares `[permissions]`. Seatbelt is partial, and Linux reports Landlock as unsandboxed. On an older kernel or a build without the `sandbox` feature, a declaring app runs unsandboxed.

## Reporting a problem

Private vulnerability reporting is off. Open an issue at <https://github.com/crumbkit/multiverse/issues> without secrets.
