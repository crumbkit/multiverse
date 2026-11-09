# Security

mxv runs on one person's machine, as that person. This document describes the code at this commit.

## Conditions for a full security document

| Condition | Holds | Evidence |
| --- | --- | --- |
| Holds tenant data | No | mxv has no tenants, server or shared store. |
| Holds personal data | Only on the person's own machine | The next section lists it. |
| Is deployed | No | `release.yml` publishes command-line archives. |

## What mxv stores

- Workspace records (`record.rs`) hold names, paths, branches, commits and ports. Paths usually include the user name. Records hold no origin URL.
- `AGENTS.md` holds origins with credentials removed (`mirror::redacted`).
- Run records (mode 0600) hold process ids, start times and plan hashes, and hold no environment values.
- App logs (mode 0600) hold whatever an app prints. phase neither rotates nor limits them.
- Archives (`store::private_dir`, mode 0700) hold copies of workspace root files and origins (`recorded_origin`), which keep an SSH user name and lose any password.
- `usage.jsonl` (`crumbkit-usage`) holds command paths, flags and exit codes, without typed values.

mxv copies no git config into a clone. `LocalNames::config` lists key names only.

## Credentials

mxv has no secret store, and a manifest's `secrets` key fails. A manifest `env` value is plain text in that file; `Redacted` keeps it out of output. Local AWS service keys are the literal `test` (`service.rs`).

A password in an origin URL appears on git's command line during a fetch and stays in the clone's `.git/config` (`clone_mirror` in `git.rs`). mxv removes it from messages and records.

Host processes receive `PATH`, `HOME`, manifest variables and the `PASSED_THROUGH` allowlist (`phase_plan.rs`), which includes `SSH_AUTH_SOCK` and excludes `AWS_*`.

## What mxv trusts

`mxv up` runs `argv` and `build` from a checkout's `mxv.toml` without asking, so a checkout you start is code you trust.

- `check_manifest_source` refuses a `--manifest` file under the workspace root or inside a git work tree (`mxv::manifest::untrusted`).
- A template must be a regular file outside the root (`template.rs`).
- Clones and mirrors use `--template=`, so they start with no hooks. A worktree workspace uses its repository's hooks.

mxv deletes built-in, config-file and trusted workspace-manifest disposable globs without asking. A workspace manifest is trusted only as a regular file with one link that no checkout holds (`manifest_trusted`). A glob from a repository's manifest needs a confirmation or `--force`.

## What mxv deletes

| Command | Guard before deletion |
| --- | --- |
| `clean` | Workspace lock, `clean::busy`, a fresh measure, and `undeletable` (links, paths outside the checkout, mount points, nested repositories). |
| `reclaim` | `reclaim::recheck` under the lock; disk needs use `clean`. |
| `remove` | `plan_removal`, then `loses_more` on a fresh plan. No workspace lock covers `Workspaces::remove`. |
| `archive` | `admit`, `refuse_outsider`, and a snapshot comparison in `retire`. |

## Sockets and ports

mxv keeps no listener open. `ports.rs` and phase bind loopback ports briefly to test them. The port ledger allocates 20000 to 29999 and fails when none is free. Containers publish on 127.0.0.1. Keepers use pipes and signals.

`admission.rs` connects to deadband's Unix socket. When no daemon answers, the build starts at once.

## Sandbox

A process runs under nono only when it declares `[permissions]`. phase reports Seatbelt as partial and Landlock as unsandboxed (`sandbox.rs`). The sandbox protects mxv's state and mirrors (`Environment::runs`). Unsafe code is limited as the `unsafe-libproc` decision in `rust-standards.toml` states.

## Reporting a problem

Private vulnerability reporting is off. Open an issue at <https://github.com/crumbkit/multiverse/issues>, and leave out secrets.
