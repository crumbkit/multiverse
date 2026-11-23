# Architecture

multiverse gives each piece of work its own workspace of git checkouts, ports and processes. This document describes the code at this commit. [SECURITY.md](SECURITY.md) covers trust and deletion guards, and [LIFECYCLE.md](LIFECYCLE.md) covers states and crash recovery.

## Crates

The dependency direction is `mxv` to `multiverse` to `phase`. `phase` never depends on the other two. `mxv` also depends on `mxv-services`, which depends on no other crate of the workspace.

| Crate | Package | Owns |
| --- | --- | --- |
| `crates/mxv` | `mxv` | The command line, output, environment snapshot, confirmations, deadband admission, `clean`, `reclaim`, and the `mxv phase keep`, `mxv phase provision` and `mxv phase services` entry points. |
| `crates/multiverse` | `multiverse` | Workspace records, git operations, mirrors, manifests, the port ledger, archives, restores, and the translation of a manifest into a phase plan. |
| `crates/phase` | `mxv-phase`, library name `phase` | Starting and stopping processes and containers, keepers, readiness, run records, logs, the sandbox and the process table. |
| `crates/services` | `mxv-services`, library name `services` | Serving a workspace's dynamodb services in memory on loopback ports with the embedded Dynoxide, for the host app `mxv-services`. |

`mxv` also uses `crumbkit` for its command conventions and `crumbkit-usage` for its local usage log. `phase` uses `nono` for the sandbox behind its default `sandbox` feature. `services` uses `dynoxide-rs` and `tokio`.

Three modules have a single job that the rest of `multiverse` goes through. `crates/multiverse/src/git.rs` starts git. `crates/multiverse/src/store.rs` writes durable files. `crates/multiverse/src/ports.rs` opens sockets to allocate ports.

## `mxv new`

1. `main` in `crates/mxv/src/main.rs` reads the environment once into an `Environment`, and `mxv::run` selects the verb.
2. `verbs::new` in `crates/mxv/src/verbs.rs` parses the name, works out the manifest source through `copied`, and calls `Workspaces::create_repos`.
3. `Workspaces::create_repos` in `crates/multiverse/src/workspaces.rs` calls `create_clone`, which writes the record first through `Store::create`, with no commit on any checkout, and then calls `make_clones`. `Store::create` reports `Created::Taken` when the name is in use.
4. `make_clones` makes the directory and calls `Workspaces::make_clone_at` for each checkout. It takes the mirror's lock, then calls `prepare_mirror`, `fetch_mirror` and `clone_mirror` on the `Backend`. `Git` in `git.rs` implements `Backend`.
5. `make_clones` writes `AGENTS.md`, the marker `.deadband-workspace` (`crates/multiverse/src/marker.rs`) and the workspace manifest, and `Store::update` records each commit.
6. A failed step calls `Workspaces::undo`, which removes what this run made.
7. `verbs::new` then calls `clone_into`, which copies ignored directories, and a failure there becomes a warning.

## `mxv up`

1. `processes::up` in `crates/mxv/src/processes.rs` loads the context, takes the workspace lock through `Runs::lock`, and reads the reclaim mark.
2. `processes::build` calls `Runs::build` in `crates/multiverse/src/runs/phased/build.rs`, which runs each `build` command. `Admission` in `crates/mxv/src/admission.rs` asks deadband for a place first. A passed build yields a `BuildsPassed` token that borrows the lock.
3. `processes::start` calls `Runs::up` in `crates/multiverse/src/runs/phased.rs`. `Runs::prepared` gives the workspace its stable id and ports through `Runs::assign_ports`, and `phase_plan::build` produces a `phase::Plan`.
4. `Runs::up` calls `Phase::up` in `crates/phase/src/phase.rs`. The plan names variables and holds no value. Values reach phase through an `EnvironmentSource`.
5. The engine in `crates/phase/src/engine/run.rs` takes phase's lifecycle lock, writes every planned object through `Journal::plan`, and walks the apps in dependency order. `start_process` handles a host process and `start_containers` handles a container app.
6. `start_process` calls `launch`, and `HostRunner::launch` spawns the keeper as `mxv phase keep`. `crates/phase/src/engine/keeper.rs` documents the protocol. The engine writes a specification to the keeper's standard input, records the keeper, writes `go`, and reads the `Spawned` line and the handshake from its standard output.
7. The keeper starts its own session with `setsid` and starts the app in a process group of its own, with the app's output going to its log file. `supervise.rs` waits for the app, restarts it as the plan's policy says, and stops it on `TERM` or `INT`. The keeper outlives the `mxv` command that started it.
8. The engine records the keeper and the process through `Journal::started`, waits for readiness, and returns a `Report`.
9. `Runs::failure` turns a failed report into an outcome, and `Runs::roll_back_phase` stops the apps that this `up` started.

Later commands find the processes through the run record. The keeper's `Recorder` in `crates/phase/src/engine/keeper/recorder.rs` updates that record as the app restarts or ends.

## `mxv run`

1. `run::run` in `crates/mxv/src/run.rs` finds the task, which a manifest's `[tasks.<name>]` declares or `Runs::mise_tasks` lists.
2. Under the workspace lock, `Runs::prepare_task` in `crates/multiverse/src/runs/phased/task.rs` assigns ports and builds the plan with one more app, `mxv-task`. The lock ends before the task starts.
3. `PreparedTask::run` asks `Admission` for a place and runs the task through `Phase::exec`, with the caller's standard streams and no timer. No run record names the task.

## `mxv proxy`

1. `processes::sync_proxy` in `crates/mxv/src/processes.rs` runs after `up`, `restart`, `down` and `ports reset`, and `verbs::remove` and `archive` do the same after they change the records.
2. `proxy::sync` in `crates/mxv/src/proxy.rs` calls `Proxy::sync` in `crates/multiverse/src/proxy.rs` and turns a failure into a warning with an error code.
3. `Proxy::sync` derives `Route` values from the records (`proxy/routes.rs`), renders Caddy's JSON configuration (`proxy/caddy_config.rs`), starts Caddy through phase when needed (`proxy/process.rs`) and talks to its admin socket (`proxy/admin.rs`).

## Host profiles

A host is a program built on `multiverse`, such as `mxv`. `multiverse::Host` (`crates/multiverse/src/host.rs`) holds every name that a host leaves on disk, in git or in docker.

- `Host::mxv()` is mxv's profile. Its values are those that the public constants `MANIFEST_FILE`, `RESERVED_PREFIX`, `proxy::DOMAIN`, `AGENTS_FILE`, `KEPT_NAMESPACE` and `WIP_MESSAGE` hold.
- `Host::named(name)` derives another profile from a name of at most 32 bytes of lowercase letters, digits and dashes.
- The profile derives the manifest file `<name>.toml`, the reserved unit prefix `<name>-`, the git namespace `refs/<name>/`, the archive pins under `refs/<name>/archives/`, the kept commits under `refs/<name>/kept/`, the origin copies under `refs/<name>/origin/`, the work-in-progress commit's author and message, the suffixes `.<name>-removing` and `.<name>-archiving` of folders set aside, and phase's docker label prefix `<name>.`.
- `with_manifest_file`, `with_proxy_domain` and `with_agents_text` override the manifest file name, the proxy domain below `.localhost` and the text of a workspace's generated `AGENTS.md`.
- `Workspaces`, `Archives`, `Runs` and `Proxy` each take a profile with `with_host`, and default to `Host::mxv()`. `Runs::with_host` passes the label prefix to `phase::Phase::with_labels`.
- `Store::with_host` sets the manifest file, the unit prefix and the proxy domain that a record's manifest reads use. The proxy domain is the domain that a manifest's `hosts` entries may not lie below. A record on disk does not store these names, and two records that differ only in them compare equal. A record may store the data of the host that owns it, as the next section says.
- Each `Workspaces` method that takes an `Archives` refuses one of another host with `Error::HostMismatch` before it reads or writes anything, so one host names every archive reference, folder and commit.
- A manifest under any host refuses unit names that start with `mxv-` as well as the host's own prefix, because `Runs` adds its internal apps and ledger ports under `mxv-` for every host.
- mxv passes `Host::mxv()` to each of these in `crates/mxv/src/environment.rs`, `crates/mxv/src/verbs.rs` and `crates/mxv/src/archive.rs`.
- The state directory, the `MXV_*` environment variables, the names of mxv's own apps (including the proxy's phase workspace `mxv-proxy`), the Caddy server name `mxv` in the generated Caddy configuration and the text of error messages do not change with the profile.

## Host data and the archive hook

A host may keep its own facts and files for each workspace, and multiverse carries them without reading them.

- `Workspace::host` (`crates/multiverse/src/record/host_data.rs`) holds `HostData`: the name of the profile that owns the workspace and at most 64 string values under keys of 1 to 64 bytes of `[a-z0-9_.-]`, in at most 16,384 bytes of JSON. `Workspaces::create_with` takes it through `CreateOptions`, and `Workspaces::set_host_data` replaces it. Data over the limits is refused with `Error::HostDataTooLarge` before anything is written.
- A record that holds host data is written at record version 5, and an archive record that holds host data or host files at archive version 4. A record or archive without either keeps the version and the bytes that 0.6.0 writes, so mxv's own records do not change, as `a_0_6_0_record_and_archive_are_byte_identical_after_a_0_7_0_rewrite` checks against records that 0.6.0 wrote, and multiverse 0.6.0 refuses a version-5 record rather than rewrite it without the data.
- `Workspaces::remove`, `archive` and `set_host_data` refuse a workspace whose `host.profile` names another profile than their own with `Error::OtherHost`, and `Archives::read` and `Archives::list` refuse such an archive the same way, before anything is written. `restore` and `Archives::rename` read through `Archives::read`, so they refuse it too. Other writes of a workspace record do not check the owner: `Workspaces::add` and the port writes in `runs.rs` and `merged.rs` rewrite another profile's record, and keep its `host` field as it was. A workspace without host data keeps the behaviour of every profile.
- `Archives::with_archive_hook` takes an `ArchiveHook` (`crates/multiverse/src/archive/host_files.rs`). An archive calls `save` after it copies the root files and before it moves any checkout, records each file that the hook wrote with its SHA-256 digest in `Archive::host_files`, and keeps the files in `host/` in the archive's data directory. A resumed archive keeps the files that an earlier run listed when `host/` still matches the list, and calls `save` again only while no checkout has moved. A restore of an archive that holds host data or host files copies them into `<workspace>/.<host>-host/`, checks each digest, and calls `check` before it writes the workspace's record as live. A damaged file or a failed check undoes the restore and keeps the archive. An archive keeps at most 1,000 host files, 100 MiB of one file and 1 GiB in all.
- A restore whose `Archives` has no hook refuses an archive that holds host files with `RestoreRefusal::HostHookMissing`, so mxv, which registers no hook, never drops another host's files.
- Host data and host files are plain files with mode 0600 or 0700 in the caller's directories, so a host must not keep a secret in them.

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

Records and files that must survive a crash go through `store::replace_file`, which calls crumbkit-durable's `write_atomically` to write a temporary file, sync it, rename it and sync the directory. Locks are crumbkit-durable's `PathLock`, an exclusive `flock`, so the operating system releases them when a process dies.
