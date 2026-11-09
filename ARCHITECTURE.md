# Architecture

multiverse gives each piece of work its own workspace of git checkouts, ports and processes. This document describes the code at this commit. [SECURITY.md](SECURITY.md) covers trust and deletion guards, and [LIFECYCLE.md](LIFECYCLE.md) covers states and crash recovery.

## Crates

The dependency direction is `mxv` to `multiverse` to `phase`. `phase` never depends on the other two.

| Crate | Package | Owns |
| --- | --- | --- |
| `crates/mxv` | `mxv` | The command line, output, environment snapshot, confirmations, deadband admission, `clean`, `reclaim`, and the `mxv phase keep` and `mxv phase provision` entry points. |
| `crates/multiverse` | `multiverse` | Workspace records, git operations, mirrors, manifests, the port ledger, archives, restores, and the translation of a manifest into a phase plan. |
| `crates/phase` | `mxv-phase`, library name `phase` | Starting and stopping processes and containers, keepers, readiness, run records, logs, the sandbox and the process table. |

`mxv` also uses `crumbkit` for its command conventions and `crumbkit-usage` for its local usage log. `phase` uses `nono` for the sandbox behind its default `sandbox` feature.

Three modules have a single job that the rest of `multiverse` goes through. `crates/multiverse/src/git.rs` starts git. `crates/multiverse/src/store.rs` writes durable files. `crates/multiverse/src/ports.rs` opens sockets to allocate ports.

## `mxv new`

1. `main` in `crates/mxv/src/main.rs` reads the environment once into an `Environment`, and `mxv::run` selects the verb.
2. `verbs::new` in `crates/mxv/src/verbs.rs` parses the name, works out the manifest source through `copied`, and calls `Workspaces::create_repos`.
3. `Workspaces::create_repos` in `crates/multiverse/src/workspaces.rs` calls `create_clone`, which writes the record first through `Store::create`, with no commit on any checkout. `Store::create` reports `Created::Taken` when the name is in use.
4. `Workspaces::make_clones` makes the directory and calls `Workspaces::make_clone_at` for each checkout. It takes the mirror's lock, then calls `prepare_mirror`, `fetch_mirror` and `clone_mirror` on the `Backend`. `Git` in `git.rs` implements `Backend`.
5. `create_clone` writes `AGENTS.md` and the workspace manifest, and `Store::update` records each commit.
6. A failed step calls `Workspaces::undo`, which removes what this run made.
7. `verbs::new` then calls `clone_into`, which copies ignored directories, and a failure there becomes a warning.

## `mxv up`

1. `processes::up` in `crates/mxv/src/processes.rs` loads the context, takes the workspace lock through `Runs::lock`, and reads the reclaim mark.
2. `processes::build` calls `Runs::build` in `crates/multiverse/src/runs/phased/build.rs`, which runs each `build` command. `Admission` in `crates/mxv/src/admission.rs` asks deadband for a place first. A passed build yields a `BuildsPassed` token that borrows the lock.
3. `processes::start` calls `Runs::up` in `crates/multiverse/src/runs/phased.rs`. `Runs::prepared` gives the workspace its stable id and ports through `Runs::assign_ports`, and `phase_plan::build` produces a `phase::Plan`.
4. `Runs::up` calls `Phase::up` in `crates/phase/src/phase.rs`. The plan names variables and holds no value. Values reach phase through an `EnvironmentSource`.
5. The engine in `crates/phase/src/engine/run.rs` takes phase's lifecycle lock, writes every planned object through `Journal::plan`, and walks the apps in dependency order. `start_process` handles a host process and `start_containers` handles a container app.
6. `start_process` calls `launch`, and `HostRunner::launch` spawns the keeper as `mxv phase keep`. `crates/phase/src/engine/keeper.rs` documents the protocol. The engine writes a specification to the keeper's standard input and reads a handshake from its standard output.
7. The keeper starts its own session with `setsid` and starts the app in a process group of its own, with the app's output going to its log file. `supervise.rs` waits for the app, restarts it as the plan's policy says, and stops it on `TERM` or `INT`. The keeper outlives the `mxv` command that started it.
8. The engine records the keeper and the process through `Journal::started`, waits for readiness, and returns a `Report`.
9. `Runs::failure` turns a failed report into an outcome, and `Runs::roll_back_phase` stops the apps that this `up` started.

Later commands find the processes through the run record. The keeper's `Recorder` in `crates/phase/src/engine/keeper/recorder.rs` updates that record as the app restarts or ends.

## Key decisions

Decision records in `crates/mxv/docs/decisions/`:

- [The environment of a host process](crates/mxv/docs/decisions/host-process-environment.md) sets the allowlist of variables that a process inherits.
- [The manifests and ports of a workspace with several checkouts](crates/mxv/docs/decisions/multi-repository-manifests.md) sets how manifests merge and how names are prefixed.

The design of the process runner is in [crates/phase/docs/design.md](crates/phase/docs/design.md). That document also describes parts that the code does not have, such as the allowlist proxy, so check a claim against the source.

[rust-standards.toml](rust-standards.toml) records these decisions:

- `lanes` lists the continuous integration lanes and what each runs.
- `release-profile` sets the release profile.
- `unsafe-libproc` limits unsafe code to three shipped modules and five test modules.
- `workspace-clones` makes each checkout a `git clone --shared` of a bare mirror that mxv owns, so that a sandbox can allow writes to one workspace alone.
- `workspace-manifest` sets where the workspace manifest comes from and how it is read.

Records and files that must survive a crash go through `store::replace_file`, which writes a temporary file, syncs it, renames it and syncs the directory. Locks are `flock` locks, so the operating system releases them when a process dies.
