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
- Mirrors hold each origin's branches and tags, and archive pins, which can be unpushed commits, stashes, or commits of a checkout's uncommitted and untracked files. An ignored file such as `.env` never enters a pin; the archive keeps it in its private tar instead (see "Archived ignored files").
- Archives (mode 0700) hold workspace root files and origins, which keep an SSH user name. Each archive's directory under `$XDG_DATA_HOME/mxv/archives/<id>/` also holds `ignored.tar`, a copy of the checkouts' ignored files, which can include secrets.
- Run records (0600) hold process ids and no environment values. `env-digest.key` keys digests of non-secret values.
- App logs (0600) hold what apps print, unrotated. Keeper diagnostic logs hold the keeper's notes and standard error.
- Docker volumes hold what containers wrote.
- Grants (`<phase state>/grants/*.json`) hold each repository's checkout paths and what its host units may do, with secret names and never values. Nothing deletes them.

## Credentials

mxv has no secret store, and a manifest's `secrets` key fails.

A password in an origin URL reaches git's command line and the clone's `.git/config`. mxv removes it from messages and records.

Host processes receive `PATH`, `HOME`, manifest-derived variables and the `PASSED_THROUGH` allowlist, which includes `SSH_AUTH_SOCK` and excludes `AWS_*`.

## Archived ignored files

`mxv archive` copies every ignored file and symbolic link that is outside the disposable set (the set `mxv clean` uses) into one tar. These files often hold secrets, such as `.env`, so the archive holds copies of those secrets.

- The tar is at `$XDG_DATA_HOME/mxv/archives/<id>/ignored.tar`. It has mode 0600 in a 0700 directory, so only the same user can read it. It is not compressed or encrypted, and it stays on this machine's disk until it is deleted.
- The archive record holds each entry's path, kind, size, mode and SHA-256 digest, and never any contents. No command prints the tar's contents, and `mxv list --archived` prints only the number of ignored entries and their total size.
- `mxv restore` checks every entry against its digest before it writes any. It never writes over a path the rebuilt checkout already holds, and never writes through a link. A damaged tar refuses with `mxv::restore::ignored_damaged` unless `--force` is given.
- `mxv restore` and `mxv remove --archived` delete the tar. Deleting an archive removes the tar's directory before the record.
- A single ignored file over `archive_max_file` (100 MiB by default) is refused with `mxv::archive::unkeepable` and is never tarred.
- Known limit: the tar keeps files and symbolic links only. An empty directory below an ignored directory is not kept, and the mode of an ignored directory is not kept. `mxv restore` makes a directory such as a 0700 `secrets/` again with the default mode, usually 0755, so other users may list the names of the files inside it. Each file gets its own kept mode back, so a 0600 file inside it stays unreadable to them.

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
- `remove --archived` deletes the archive, its pins, its ignored-files tar and, with `--delete-volumes`, volumes. Guards: refuses `Retiring`; prompt; workspace lock; `refuse_live_volumes`.

## Sockets and ports

mxv keeps no listener open. The ledger allocates from `ports_range`, by default 20000 to 29999, and fails when none is free. Containers publish on 127.0.0.1. mxv connects to deadband's Unix socket.

Caddy listens on `proxy_http` and `proxy_https`, by default 127.0.0.1:80 and 127.0.0.1:443, and its admin API is a Unix socket in the proxy state directory that only the person can open. Each address is bound only on loopback. A route forwards to 127.0.0.1 and the host port of its workspace, so it opens no port that the workspace did not already expose. Caddy's certificate authority key lives under `$XDG_DATA_HOME/mxv/proxy/caddy`, which `mxv clean` and `sweep` protect. `mxv proxy trust` installs that authority in the system trust store, which gives it the power to vouch for any name, so mxv asks first, refuses without a terminal, and the skill tells agents never to run it.

## Sandbox

A process runs under nono only when it declares `[permissions]`. Seatbelt is partial, and Linux reports Landlock as unsandboxed. On an older kernel or a build without the `sandbox` feature, a declaring app runs unsandboxed.

`up`, `restart` and `exec` refuse a host unit whose permissions exceed its repository's grant, with `mxv::permissions::not_granted`, and change nothing. Only a person who answers yes at a terminal widens a grant; no flag or variable does. The check makes a widening visible. It does not stop a process that runs with the person's own access, outside the sandbox, from editing a grant file. The sandbox denies phase's state, so a sandboxed process cannot.

## Reporting a problem

Private vulnerability reporting is off. Open an issue at <https://github.com/crumbkit/multiverse/issues> without secrets.
