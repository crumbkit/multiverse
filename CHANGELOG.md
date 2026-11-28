# Changelog

This file records the changes to multiverse, mxv and mxv-phase that a
workspace may need to act on.

## Unreleased

### Added

- `mxv new` and `mxv add` take an origin for `--repo` as well as a path: a
  git URL, such as `https://github.com/acme/dispatcher` or
  `git@github.com:acme/dispatcher.git`, or the short form
  `github.com/acme/dispatcher`, so no `git clone` is needed first. mxv
  fetches the origin into its mirror and clones the checkout from there, on
  a new branch from the origin's default branch. The short form is fetched
  from the URL that its mirror last fetched from, which the mirror's config
  now keeps as `mxv.origin` with an SSH user name and never a password, or
  else from `ssh://git@<host>/<path>`. A value that names an existing directory is
  still read as a path. Such a checkout gets no ignored directories and no
  `.git/info/exclude` copied in, and an origin without a default branch
  fails with `mxv::origin::no_default_branch`, which names the origin.
- `mxv up` and `mxv restart` give a host process each variable of its
  starmap file whose `source` is `secret` or `generated`. A 1Password
  reference is read through the starmap agent when it runs and with `op`
  otherwise, a secret without a bind is read from the login Keychain, and a
  generated value is created there on first use, with 20 seconds for each
  unit's secrets. The Keychain keeps them under the service
  `starmap.mxv-<workspace id>` and the account `<unit>/<NAME>`, so two
  workspaces of one repository hold separate values, and `mxv remove`
  deletes them before the record; a Keychain that refuses fails the removal
  with `mxv::keychain::refused` and keeps the workspace. `mxv archive` keeps
  them, and `mxv remove --archived` deletes them before the archive, unless
  a restored workspace holds the archive's stable id.
- A secret without a bind that the Keychain lacks fails its unit with
  `mxv::secret::unavailable`, whose `keychain_service` and
  `keychain_account` fields name the item and whose recovery names the
  `security add-generic-password -U -s <service> -a <account> -w` command
  that saves it.
- A repeat `mxv up` reads no secret of a unit that phase leaves running
  from the current plan, so it runs no `op` and raises no prompt when
  nothing needs starting, and a running unit is not reported as not
  started.
- mxv reads a 1Password reference for a unit only after a person at a
  terminal answered yes to a question that names the unit, the variable,
  the store, the reference and the file. The grant is kept per repository,
  unit and reference in `<phase state>/grants/<repository>-<hash>.secrets.json`
  (`phase/secret-grants/v1`), which holds no value, and a changed reference
  asks again. A secret that no person granted, or that its store cannot
  give in time, affects only its unit: an optional variable stays unset
  with a warning, and any other one fails the unit with
  `mxv::secret::not_granted` or `mxv::secret::unavailable`, with the
  `sign_in` of a matching `[secrets.*]` table. The units that depend on it
  do not start and their rows say why, and every other unit starts.
- In the multiverse library, `SecretFailure`, `SecretProblem`,
  `SecretSource`, `AskedSecret`, `SecretAsk`, `Runs::secret_granting`,
  `Runs::grant_secrets`, `UpOutcome::Withheld`, `ProcessNote::SecretDefaulted`,
  `ProcessNote::NotStarted`, `Error::KeychainRefused`, `forget_keychain`,
  `keychain_service`, `KeychainItem`, `SecretFailure::keychain` and
  `Lifecycle::with_forget`; in phase, `SecretGrants`, `SecretRead`,
  `Phase::secrets_granted`, `Phase::grant_secrets` and `Phase::left_alone`,
  which names the host apps that `up` would leave running.

### Changed

- `mxv up` and `mxv restart` print a batch: the `mxv/up/v1` and
  `mxv/restart/v1` documents gain a `failures` list beside `data`, which
  holds a failure for each unit that lacks a secret, and the command exits
  with 1 when the list is not empty, so exit 1 can carry an `mxv/up/v1` or
  `mxv/restart/v1` document in place of a failure document. The `cut`
  property is gone from both documents. `data` is unchanged when every unit
  starts. A unit that did not start because a unit it depends on lacks a
  secret has a row with a `warning` note that names that unit.

- `mxv up`, `restart` and `run` check the starmap file of every selected
  host process before they report a wrong provided bind, and
  `mxv::manifest::provided_unknown` lists every such bind in one failure.
  Before, mxv stopped at the first one, so a person missing several
  checkouts met them one start at a time. The new `binds` field holds one
  object per bind with `unit`, `variable`, `app`, `port`, `checkout` and
  `reason`, and the text output prints one `cause:` line per bind when
  there are several. The fields `unit`, `variable`, `app` and `port` still
  give the first bind. An optional bind to a missing checkout stays unset
  and is not listed, and `mxv::manifest::starmap_invalid` is still reported
  alone. A process whose `cwd`, name or permissions phase cannot run is
  reported only when no bind is wrong.
- In the multiverse library, which is 0.x, so this is a minor-version
  change, `UnrunnableKind::ProvidedUnknown` holds `RefusedBinds`, a
  non-empty list of `RefusedBind`, in place of one variable, unit and port.

## [0.8.0] - 2026-11-27

### Added

- `mxv up`, `restart` and `run` give a host process each variable of its
  starmap file whose bind is `multiverse/port` or `multiverse/url`, such as
  `{ provided = "multiverse/url", app = "catalogue/api" }`, with the host
  port or the URL of the named unit's port. A bare unit name names a unit
  of the process's own checkout, `<checkout>/<unit>` names a unit of
  another checkout by the checkout's folder, and `port` names a port by
  the unit's own name for it. A bind that names a unit or port the
  workspace does not have fails with `mxv::manifest::provided_unknown`, and
  a starmap file that does not parse fails with
  `mxv::manifest::starmap_invalid`, both before any unit starts.
- `mxv up`, `restart` and `run` leave an optional provided variable unset,
  and start the process, when its bind names `<checkout>/<unit>` for a
  checkout the workspace does not have. `up` and `restart` put an `info`
  note on the process's row for each such variable, and `run` prints it on
  standard error before the task starts. A required variable, and an
  optional bind to a present checkout with no such unit or port, still fail
  with `mxv::manifest::provided_unknown`. `ProcessNote::ProvidedSkipped`
  carries the note, with the variable and the checkout, and
  `PreparedTask::notes` returns the plan's notes of each unit.

- `phase::is_variable_name` reports whether a name is one phase passes to
  a process: an ASCII letter or `_`, then ASCII letters, digits and `_`.
- `Runs::stable_id` returns a workspace's stable id, and mints and saves
  one when the record holds none, only while the caller holds the
  workspace's lock.
- `Error::GrantsBusy` reports a grant that waited for all of the lock wait
  for another grant of the same repository. mxv reports it as
  `mxv::grants::busy`. Before, it was `Error::Busy` and
  `mxv::workspace::busy`.

### Changed

- Breaking: `Unrunnable` has a new public field, `kind`, an
  `UnrunnableKind`, which tells a plan that breaks phase's rules from a
  provided variable whose bind names no port of the workspace, a provided
  variable whose port has no host port yet, and a starmap file that does
  not parse. A caller that builds `Unrunnable` with a struct literal adds
  `kind: UnrunnableKind::Plan`.
- `Runs::granting` takes the workspace's stable id in place of a `Store`,
  and writes nothing. A caller gets the id from `Runs::stable_id` first.
- `mxv up` without names after `mxv reclaim` asks a person to grant only
  the permissions of the processes that it starts again.

### Fixed

- `mxv add` refuses with `mxv::workspace::not_found`, and changes
  nothing, when the workspace was archived or removed and another
  workspace of the same name took its place while the command waited for
  the workspace's lock. Before, it added its checkout to the new
  workspace.
- A rerun of `mxv restore` that finds the workspace already restored
  serves its routes, and starts the proxy when they need it, as a full
  restore does. Before, a restore that stopped after it wrote the
  workspace's record left the routes out of Caddy until the next
  `mxv up`, `mxv down` or `mxv proxy start`.
- A rerun of `mxv archive` that finds the workspace already archived
  takes its routes away from a running proxy. Before, an archive that
  stopped after it finished left Caddy serving the archived workspace's
  routes, and `mxv proxy check` failed `caddy runs`.
- `mxv up`, `mxv restart` and `mxv run` find a deadband daemon that
  listens at `$XDG_RUNTIME_DIR/deadband/deadband.sock`, which deadband
  uses when `DEADBAND_SOCKET`, `DEADBAND_SOCKET_DIR` and `TMPDIR` are
  unset, as on Linux with a systemd login session. Before, mxv looked in
  `/tmp/deadband-<uid>`, found no daemon and started every build and task
  at once without a place.
- A command that waited for a workspace's lock while an `mxv archive` and
  an `mxv restore` or `mxv new` replaced the workspace with another of the
  same name now fails with `mxv::workspace::not_found` and changes
  nothing. The message names the new workspace's stable id and says to
  run the command again. Before, `mxv up` started the new workspace and
  `mxv down` stopped its processes. `mxv reclaim` keeps such a workspace
  and gives the reason.
- `mxv up` and `mxv restart` at a terminal, on a workspace whose record
  has no stable id, report `mxv::workspace::not_found` when an
  `mxv archive` or `mxv remove` deleted the record while they waited for
  the lock. Before, they reported `mxv::docker::failed`.
  `Runs::stable_id` returns `Error::RecordMissing` in that case.
- `mxv down` of a named process that has exited, while a running process
  depends on it, succeeds and reports it stopped. Before, it failed with
  `mxv::process::still_needed`.
- `Workspaces::check_manifest_source` refuses a manifest that lies in a
  tracked directory shaped like a bare repository, one holding `HEAD`,
  `objects/` and `refs/`, or below one, inside a git work tree. Before
  this fix git took that directory for a bare repository, the check found
  no work tree and trusted the manifest, and `mxv new --manifest` would
  have deleted its `disposable` globs without asking. The check also
  refuses a manifest inside a clone's own `.git` directory, which it
  trusted before.

### Known issues

- A command does not detect the replacement of a workspace whose record
  held no stable id when the command read it, because any write gives
  such a record its first id. Only records that an mxv from before
  stable ids wrote lack an id.

## [0.7.0] - 2026-11-22

### Added

- A workspace record can hold the data of the host profile that owns it,
  `Workspace::host`: the profile's name and up to 64 string values that
  only that host reads, in at most 16,384 bytes. `Workspaces::create_with`
  writes it with the first record through `CreateOptions`, and
  `Workspaces::set_host_data` replaces it. An archive keeps it, and a
  restore writes it back. Data over the limits is refused with
  `Error::HostDataTooLarge` before anything is written.
- `Archives::with_archive_hook` takes an `ArchiveHook`, whose `save`
  writes the host's own files of a workspace into the archive before any
  checkout moves, and whose `check` checks them after a restore puts them
  back in `.<host>-host/` and before the workspace goes live. The archive
  records each file's SHA-256 digest. A damaged file refuses the restore
  with `RestoreRefusal::HostFilesDamaged`, and a failed check with
  `RestoreRefusal::HostCheckFailed`; both undo the restore and keep the
  archive. An archive keeps at most 1,000 host files and 1 GiB of them,
  and stops before any checkout moves when the hook writes more. A
  resumed archive keeps the host files of its earlier run.
- A workspace or archive that another host profile owns is refused with
  `Error::OtherHost` by `remove`, `archive`, `restore` and
  `set_host_data`, before anything is written. `add` and the port writes
  do not check the owner, and keep the record's host data. mxv reports it as
  `mxv::workspace::other_host` and names the owner.
- A restore without a hook refuses an archive that holds host files with
  `RestoreRefusal::HostHookMissing`, which mxv reports as
  `mxv::restore::host_hook_missing`.

### Changed

- A workspace record that holds host data is written at version 5, and an
  archive record that holds host data or files at version 4
  (`HOST_ARCHIVE_VERSION`; one without them stays at
  `NO_HOST_ARCHIVE_VERSION`, 3). Records and archives without them keep
  their versions and bytes, so mxv's own are
  unchanged and an older mxv still reads them. multiverse 0.6.0 and older
  refuse a version-5 record instead of rewriting it without the host's
  data.

## [0.6.0] - 2026-11-22

### Added

- multiverse has a host profile, `multiverse::Host`, which holds every name
  that a program built on multiverse leaves on disk, in git and in docker.
  `Host::mxv()` holds mxv's names. `Host::named(name)` derives another
  profile from a name of at most 32 bytes of lowercase letters, digits and
  dashes: the manifest file `<name>.toml`, the reserved unit prefix
  `<name>-`, the git namespace `refs/<name>/`, the suffixes
  `.<name>-removing` and `.<name>-archiving`, the author and message of the
  work-in-progress commit, and the docker label prefix `<name>.`.
  `with_manifest_file`, `with_proxy_domain` and `with_agents_text` override
  the manifest file, the proxy domain below `.localhost` and the generated
  `AGENTS.md`. A bad name or value returns `BadHost`.
- `Workspaces`, `Archives`, `Runs`, `Proxy` and `Store` have `with_host`,
  and each defaults to `Host::mxv()`. `Runs::with_host` gives phase the
  profile's label prefix. A host lists, prunes, sweeps, restores
  and deletes only its own references, folders and docker objects, so two
  hosts can share a mirror and a docker daemon. `Archives` refuses an
  archive record whose references lie in another host's namespace.
- Each `Workspaces` method that takes an `Archives` refuses one of another
  host with `Error::HostMismatch` before it reads or writes anything. mxv
  reports it as `mxv::workspace::host_mismatch`, a defect to report,
  because mxv gives both `Host::mxv()`.
- `health::classify_for` classifies the records of a given host, so an
  archive that the host started reads under the host's own folder suffix.
  `health::classify` classifies mxv's.

### Changed

- The public constants `MANIFEST_FILE`, `RESERVED_PREFIX`, `proxy::DOMAIN`,
  `AGENTS_FILE`, `KEPT_NAMESPACE` and `WIP_MESSAGE` keep their values, which
  are mxv's profile. mxv's output, references, folders and labels are
  unchanged.
- `Backend::unpushed_commits`, `orphans`, `write_wip`, `concealed` and
  `local_only_commits` take a `&Host`. `write_wip` takes the host in place
  of the commit message, and writes the host's message and, when git has
  no identity of the person, the host's author. A `Backend` outside this
  repository adds the argument to each of these methods.
- A manifest under any host refuses unit and port names that start with
  `mxv-` as well as the host's own prefix, because `Runs` adds its internal
  apps and ledger ports, such as `mxv-services` and `mxv-aws`, under that
  prefix for every host.

## [0.5.1] - 2026-11-22

### Added

- mxv-phase has `LabelPrefix` and `Phase::with_labels`, which set the
  prefix of the labels on phase's docker containers and volumes.
  A prefix is a name of at most 32 bytes that starts with a lowercase
  letter and holds lowercase letters, digits and dashes, followed by a dot.
  `LabelPrefix::new` refuses any other name with `BadLabelPrefix`, whose
  message states this rule.
  `LabelPrefix::mxv()` is `mxv.`, the default, so mxv's labels are
  unchanged. phase's sweep lists, stops and deletes only objects that
  carry its own prefix.

## [0.5.0] - 2026-11-22

### Added

- mxv-phase's `http` readiness probe can expect one exact status and can
  check the response's JSON body. `ProbeCheck::Http` has two optional
  fields: `status`, the one status that passes, and `json`, a list of
  fields named by JSON pointer (RFC 6901) that must be present or equal a
  scalar. `ProbeCheck::http_expecting` builds such a check, and
  `HttpStatus`, `JsonExpectation`, `JsonPointer` and `JsonScalar` are new
  public types. A probe with `json` reads at most 65,536 bytes of body as
  sent, counting a chunked body's framing, and a longer body fails the
  attempt. The plan refuses a status outside 100 to 599, a malformed
  pointer, more than 32 expectations on one probe and an expected string
  longer than 1,024 bytes. A probe that sets neither field
  passes on any 2xx as before, and its plan has the same JSON and digest,
  so an upgrade restarts no running process. mxv's manifest and output are
  unchanged.

### Fixed

- A git command, a docker command, or the check command of a manifest's
  secret whose descendant keeps its output open no longer holds mxv until
  that descendant exits. mxv returns at most about one second after the
  command itself exits.
- `mxv run` and its listing of mise tasks read a checkout's tasks when a
  process that the mise config starts in the background keeps mise's output
  open. Before, mxv waited 30 seconds and then said that mise did not list
  the tasks.
- `mxv up` and `mxv restart` stop a cancelled secret write or readiness
  probe within a few milliseconds. Before, they took up to 50 ms or 20 ms.

## [0.4.0] - 2026-11-20

### Added

- `mxv list`, `mxv list --archived`, `mxv show`, `mxv new` and `mxv add`
  print each workspace's stable id in the field `id`. `id` is a long column,
  so it appears with `--long`, `--full` or `--fields`, such as
  `mxv list --json --fields name,id,path`. Plain `mxv list` and its table
  are unchanged. `id` is null for a record that an mxv before stable ids
  wrote and no mxv has written since, for a record that mxv cannot read,
  for an archive that holds no id, and in a dry run of `mxv new`. Reading a
  record never mints its id.
  The schemas `mxv/list/v1`, `mxv/show/v1`, `mxv/new/v1` and `mxv/add/v1`
  gain the field.
- `mxv sweep` archives each workspace whose change has merged, deletes its
  directory, build output and docker volumes, and reports the disk space it
  freed. A workspace counts as merged when every checkout that holds a commit
  of its own is merged, either because the branch is contained in its base
  after a fetch and origin holds the branch under its own name, or because
  `gh` shows a pull request of that branch merged into its base at its
  `HEAD`. A workspace with uncommitted changes, a stash or a detached
  `HEAD` stays, with a row that says why. `mxv new` runs the same sweep first,
  without a question, unless `--no-sweep` is given, and a failed sweep only
  warns. That sweep starts no new decision after 45 seconds and keeps the
  workspaces it has not reached. `mxv new --board-mount BOARD:PATH` records
  a board mount that the sweep updates with `board -C BOARD mount update
  PATH` after it archives the workspace. The workspace record gains an
  optional `board_mounts` list. Each row lists the checkouts whose
  uncommitted work the archive kept as work-in-progress commits, which
  `mxv restore` puts back, and the archive's warnings.

## [0.3.0] - 2026-11-20

### Added

- A process in `mxv.toml` may name the starmap file that declares its
  variables with `starmap = "<path>"` in its `[processes.<name>]` table, for
  tools such as jumpgate that read the manifest. The path is relative to
  the top of the checkout, whatever the process's `cwd`. An empty path, a
  path that starts with `/` and a path with a `..` part fail with
  `mxv::manifest::invalid` and the line. A file that is missing, is a
  symbolic link, is not a regular file or leaves the checkout through a
  linked directory fails each command that reads the manifest to act,
  such as `mxv up`, with the new code `mxv::manifest::starmap_file`, which
  names the unit, the file and the line. `mxv down` still stops every running
  unit then, and `mxv status` still lists them and reports the code in
  `manifest_error_code`. mxv does not read the file. Before this release
  the key failed with `mxv::manifest::unknown_key`.
- A manifest declares the credentials its work needs as `[secrets.<name>]`
  tables with a `check` command and a `sign_in` command line. `mxv preflight`
  runs each check, which should be the real command that uses the credential,
  and lists every sign-in that is needed in one message. `mxv new`,
  `mxv restore` and `mxv up` print that message as one warning once the
  workspace exists. A check command comes from the repository, so mxv runs it
  only after a person approved that command line at a terminal through
  `mxv preflight`, and the warning names each check that was not run. mxv
  discards each check's output, so no credential value is printed, stored or
  passed on. A manifest that used `[secrets]` before this release
  failed with `mxv::manifest::reserved`, and now needs the new form.

### Changed

- A `dynamodb` service runs in `mxv-services`, a host process that serves
  DynamoDB in memory with Dynoxide 1.7.0, which mxv embeds, in place of the
  DynamoDB Local container `mxv-dynamodb`. A manifest whose services are
  all `dynamodb` needs no Docker. Processes receive the same endpoint,
  region and placeholder-key variables on the same port, `mxv-dynamodb`.
  `mxv up` and `mxv status` list `mxv-services` as a process, and
  `mxv logs mxv-services` prints the address it listens on. Run
  `mxv down` in a workspace whose DynamoDB Local container runs before its
  first `up` with this version, so the container gives up the port
  `mxv-dynamodb` that `mxv-services` listens on. That `up` creates the
  tables again.
- A DynamoDB request needs an `X-Amz-Date` or `Date` header beside its
  `Authorization` header, as the AWS SDKs send them. A hand-written request,
  such as one from curl, that sent only `Authorization` is refused.
- An item whose `ttl` attribute has passed is never deleted, and reads
  return it until `mxv-services` stops.
- Library: `ServiceBackend::Dynamodb` is now `ServiceBackend::Services`,
  whose app is `SERVICES_APP`. `ServiceKind::port` names the ledger port of
  a kind, and `ServiceBackend::is_container` says whether a backend runs in
  a container. `DYNAMODB_IMAGE`, `ServiceBackend::image` and
  `ServiceBackend::container_port` are gone. `Runs::with_provisioner` is
  now `Runs::with_own_command`, whose words stop before `provision`.
- `mxv remove` refuses with `mxv::remove::in_use` when a process that mxv
  did not start, such as a command from `mxv exec` or `mxv run` or an agent
  session, works in the workspace, and names the process. `--force`
  overrides the refusal and names the process in its question. A script that
  removes a workspace while its own child runs inside it needs `--force`.
- Library: `Repaired` is exhaustive, so a match on it names every kind of repair.

### Fixed

- The `mxv proxy` routes listing names the host label that two workspaces
  share in its note on a shadowed route, also when the route's name holds
  `_`, `.` or an upper-case letter, such as `api_v2`. The note used to show
  no label for those names.
- An archive whose restore mark names a reference outside `refs/mxv/kept/`,
  or a repository that is not the source of one of its checkouts, is
  reported as a corrupt archive. `mxv restore`, `mxv repair` and
  `mxv remove --archived` no longer delete such a reference, such as a
  branch of yours.

## [0.2.0] - 2026-11-18

Version 0.2.0 was not released as binaries, and 0.3.0 holds its changes.

### Added

- A manifest's `routes` list names ports that serve HTTP, and mxv serves
  each at `https://<port>.<workspace>.mxv.localhost` through one Caddy for
  all workspaces. `mxv up`, `restart`, `down`, `remove`, `archive`, `restore`
  and `ports reset` keep the routes in line with the workspace records, and a
  failure to do so is a warning with an error code and never fails the verb.
- `mxv proxy status`, `start`, `stop` and `trust` operate the proxy. The
  config keys `proxy_http`, `proxy_https` and `caddy` set its addresses and
  program. `mxv proxy trust` needs a terminal and a confirmation.
- A process or container can list `hosts` in its manifest, and the proxy
  serves the route that it listens on at each extra name too. When two
  workspaces list a name, the workspace whose name sorts first serves it. mxv
  looks each name up after `mxv up`, `restart` and `restore`, and prints the
  `/etc/hosts` line to add when it does not resolve to this machine; it never
  writes the file.
- Host processes and tasks of a workspace with a served route receive
  `MXV_DOMAIN`, `MXV_HOST_<PORT>` and `MXV_URL_<PORT>`, and a process that
  listens on a served route also receives `MXV_HOST` and `MXV_URL`.
- `mxv proxy install` finds Caddy, starts it, with the routes there are or
  with none, and asks a person to trust its certificate authority. Running it
  again changes nothing that is set up, and it never stops a Caddy that runs.
  The check marks `caddy runs` failed when Caddy holds other routes than the
  records give, and every row and recovery that tells a person to edit the
  config file names the file mxv reads, including under `XDG_CONFIG_HOME`.
  mxv refuses a Caddy whose version has no numeric major and minor, such as a
  development build that prints `devel`, with `mxv::proxy::caddy_missing`,
  and the check marks the `caddy` row `missing`.
  `mxv proxy check` reads what the proxy needs, prints a row for each and
  exits with `mxv::proxy::check_failed` when one is missing or failed. The
  library gains `Proxy::check`, `Proxy::ensure_running`, `Proxy::caddy`,
  the `Resolver` trait with `SystemResolver`, `missing_hosts` and `hosts_line`.
  A host lookup that takes more than two seconds is `unknown` in the check and
  prints no `/etc/hosts` line. The check marks an authority whose trust mxv
  cannot read as `unknown`, which does not fail it. `mxv up` prints a note
  for the manifest of the workspace it ran when the routes leave it out, and
  names each running workspace that lost a route to the workspace it started, with the `mxv restart` command that gives it the current
  variables.
- `.github/workflows/proxy.yml` runs the tests that start a real Caddy, with
  the Caddy release pinned by digest. It is not a required check.

### Changed

- mxv and phase write their records and files and take their locks through
  crumbkit-durable. A lock file that mxv creates from now on has mode 0600
  instead of the umask's bits; an existing lock file keeps its mode. A
  symbolic link where a lock file goes is refused with
  `mxv::file::unwritable` instead of followed. A command that waits for a
  lock notices its release within 5 ms instead of 50 ms, and the length of
  the wait does not change. A write that replaces a whole file, such as a
  workspace record, `AGENTS.md`, the manifest, an archive record or a run
  record, now behaves better in three ways. Two such writes of one file at
  once in one mxv both succeed. A temporary file that a crashed mxv with the
  same process id left no longer makes the write fail. A failed write names
  the file it was writing instead of its hidden temporary file. The
  temporary file of such a write is now `.<name>.<pid>.<n>.tmp`, and phase
  leaves one that a crash left in place instead of deleting it on its next
  write. The first write of a workspace record and the writes that copy
  archived files keep their old temporary file, so a leftover of the same
  process id still fails them, and their errors still name that file.
- mxv builds on crumbkit 0.6.0 and deadband 0.4.0, which read and print
  durations with jiff in place of humantime, so RUSTSEC-2026-0542 no longer
  applies to mxv. `mxv self usage --since` now reads `3mo` and `1y` as
  calendar months and years, and refuses a capital `M`, `0`, a fraction such
  as `1.5d` and a negative age such as `-1h`. `mxv self runs list` and
  `mxv self figures list` take `--context`, and `mxv self runs list --long`
  adds the `lock_ms` and `context` columns. The `mxv skills` commands name
  mxv in their help and examples. A command line that names a group without
  its subcommand, such as `mxv skills`, prints the group's help and exits 0.
  The help of a listing that shows only problem rows by default says it also
  shows warning rows. The failure object in every JSON document schema gains
  the optional `sentence` and `causes` members and the recovery step kinds
  `program` and `after`; it drops nothing. deadband 0.4.0 asks its daemon for
  beats, so when a deadband 0.4 daemon queues an mxv build and then sends no
  line for 16 seconds, mxv stops waiting and starts the build; before, it
  waited with no deadline.
- `mxv list` names a workspace that a killed `mxv new` left in the `creating`
  state, with the repair `mxv remove NAME`, and `mxv new` of that name fails
  with `mxv::workspace::creating` instead of `mxv::workspace::exists`.
- `mxv restore` writes a mark into the archive's record, in the optional
  field `restoring`, before each step that creates something. When a restore
  dies, the next `mxv restore` of the name deletes what the mark lists and
  restores again, instead of refusing with `mxv::restore::name_taken`, and
  `mxv repair NAME` deletes it without restoring, with the new state
  `undone` that names the directory. `mxv list` shows the new problem "the
  restore stopped before it finished, and left what it made". mxv deletes a
  directory at the workspace's path only when the mark says the restore made
  it, or when the directory is empty. The archive record stays at version 3.
  `mxv remove --archived NAME` on an archive that holds a mark deletes what
  the stopped restore made before it deletes the record, and a repair, a
  restore or a removal keeps the mark when something it made cannot be
  deleted. When the mark says the restore made the directory and the
  directory holds an entry that the mark does not account for, such as a
  file that the person put there afterwards, `mxv repair NAME`,
  `mxv restore NAME` and `mxv remove --archived NAME` delete nothing, keep
  the mark and fail with `mxv::restore::stopped_holds_other_files`, which
  names each such entry. Their dry runs refuse the same way, and
  `mxv remove --archived` refuses before it asks. The mark accounts for each checkout that the
  restore started, with everything inside it, and for `AGENTS.md`, the
  manifest, the marker and the archive's root files once the restore marked
  them as written. The `directory` field of the repair result and the question name
  the workspace's directory only when the repair deletes it, and the question
  and dry run of `mxv restore` say that the run first deletes what a stopped
  restore left.
- `mxv repair NAME` abandons an archive that stopped before it deleted any
  checkout: it deletes the archive's record, pins and ignored-files tar, and
  the workspace is `live` again with every checkout as it was. The result
  has the new states `abandoned` and `would_abandon`. `mxv list` names
  `mxv repair NAME` for such an archive, with the new problem "the archive
  stopped before it deleted any checkout", where it named
  `mxv archive -w NAME`, which still carries the archive on. `mxv restore`
  and `mxv remove --archived` of the name name `mxv repair NAME` too. A
  refused `mxv archive`, such as one that found a file written while a
  checkout was moved aside, abandons its archive in the same way and still
  reports the refusal. An archive that has deleted or moved aside a
  checkout keeps `mxv archive -w NAME`.
- `mxv up` and every other verb that reads the manifest refuse `ready = { log = "" }`
  with `mxv::manifest::invalid` and the line of the `log` key. Before, phase
  refused the empty pattern only when it checked the plan, with no line.
  `multiverse::LogPattern` is now `phase::LogPattern`, and phase exports
  `LogPattern` and `BadLogPattern`. The `multiverse` crate no longer depends on `regex`.
- `mxv repair NAME` deletes an archive that a restore already rebuilt only
  when it is the archive the person confirmed. When another archive of the
  name needs that repair by the time the lock is held, the repair reports
  `mxv::repair::changed` and deletes nothing.
- The library's `Workspaces::finish_archive` is private to the crate, so a caller outside it cannot mark an archive of any
  state as archived. `Lifecycle::archive` and `Lifecycle::finish` still
  finish archives.
- `CheckoutRemoval` holds what only a worktree or only a clone has in
  `kind`, a `RemovalKind`: a worktree's `stray_commits`, `unlock` and
  `partial`, or a clone's `lost` commits and `stashes`.
  `CheckoutRemoval::lost_commits` returns the count that `stray_commits`
  held for both kinds.
- `mxv restore` and `mxv repair NAME` take the workspace lock, plan again
  and act through the library, in `Lifecycle::restore` and
  `Lifecycle::repair`. A second `mxv restore` that waited for the lock
  while the first one brought the workspace back reports `unchanged` and
  exits 0. Before, it failed with `mxv::restore::name_taken`. A restore
  whose archive another archive of the same name replaced after the
  question builds nothing and fails with `mxv::restore::changed`. Before,
  it restored the new archive. The library gains `Lifecycle::restore`,
  `Lifecycle::repair`, `Restoration`, `Repairing` and `RepairDone`.
- `mxv archive` holds the workspace's lock from its second plan, after the
  question, until its last write, and it asks without the lock. Before, it
  took the lock before its first plan and held it through the question, so
  an `mxv up` of the same name failed with `mxv::workspace::busy` until the
  person answered. Now `mxv up` runs while the question is open. When a
  unit runs that the question did not name, when docker holds a volume
  that it did not name, or when the workspace or the archive to finish is
  another one, the command changes nothing and fails with
  `mxv::archive::changed`. A second `mxv archive` of an unfinished archive
  finishes it, and one of a finished archive reports `unchanged`. With
  `--delete-volumes`, every question of an archive names the docker
  volumes that it deletes, and the finish path of an archive that an
  earlier run left unfinished asks before it deletes them, so a script that
  runs `mxv archive -w NAME --delete-volumes` without a terminal needs
  `--yes` there. The library gains `Lifecycle::plan_archive`,
  `Lifecycle::archive`, `Lifecycle::finish`,
  `Lifecycle::with_in_use`, `Archived`, `Finished`, `Finish`, `FinishDone`,
  `RetireDone` and `InUse`, and `VolumesAsked::with_units`.
- `mxv remove` holds the workspace's lock from before it stops the units
  until after it deletes the last docker volume, and `mxv new` holds it from
  before it writes the record until it has copied the ignored directories.
  Before, `mxv remove` took the lock only to stop units and `mxv new` took
  none, so `mxv remove --force` could delete the checkouts under a running
  `mxv up`, and `mxv remove` could delete the checkouts that `mxv new` was
  making. A command that meets the held lock waits 5 seconds and fails with
  `mxv::workspace::busy`, and `mxv new` of a name that a workspace holds
  still fails with `mxv::workspace::exists` at once. `mxv remove` also reads
  the workspace again under the lock and fails with
  `mxv::workspace::changed`, before it stops anything, when the record is
  another one than the question was about or docker holds a volume that the
  question did not name. With `--delete-volumes`, the question of `mxv
  remove` names the docker volumes that it deletes. The library gains
  `Lifecycle`, `RemoveAsked`, `Removed` and `RemovalDone`.
- `mxv remove --archived` reads the archive again once it holds the
  workspace's lock, and deletes only the archive that the question was
  about. Before, it passed the archive that it read before the question to
  the deletion, so a person who restored the workspace and archived it
  again while the question was open lost the record of the new archive, and
  its references and its tar of ignored files stayed with no record. Now,
  when another archive holds the name, when the archive is no longer
  finished, or when docker holds a volume of it that the question did not
  name, the command deletes nothing and fails with the new code
  `mxv::archive::changed`. With `--delete-volumes`, the question names the
  docker volumes that it deletes. The library gains
  `Lifecycle::delete_archive`, `Lifecycle::delete_archive_held`,
  `Lifecycle::volumes_in_use`, `VolumesAsked` and `ArchiveDeleted`;
  `Lifecycle::new` takes the archives, and `Removed` is no longer
  `#[non_exhaustive]`.
- The help of `mxv skills install`, `check` and `print` and of `mxv skills`
  takes crumbkit's wording: "this tool's skills" in place of "mxv's skill",
  and crumbkit's examples.
- On macOS, mxv finds the deadband daemon's socket in the per-user
  temporary directory, where the launchd daemon listens, before `TMPDIR`.
  Before, a shell whose `TMPDIR` differed from that directory made mxv find
  no daemon and build without admission. `DEADBAND_SOCKET` and
  `DEADBAND_SOCKET_DIR` still win.
- A library error that mxv gives no next step of its own now offers
  crumbkit's retry. Its text is `run the same command again later`, and in
  JSON its next step has the kind `retry`. Before, it was a hint whose text
  was `run the command again`.
- mxv builds on crumbkit 0.5.0 and deadband 0.4.0-alpha.1, and the output
  of these commands changes with it:
  - `mxv list`, `mxv list --archived` and `mxv schemas list` print their
    next step on two lines, a sentence and then the command. In JSON, the
    `text` of a `next` entry holds the sentence.
  - The rows of `mxv skills install` and `mxv skills check` gain a `skill`
    column, and `mxv skills print` gains a `skill` field.
  - `mxv self usage`, `mxv self figures list` and `mxv self runs list`
    accept `--exclude-program`, which leaves out the runs that a program
    with the given name marked.
  - The help of `mxv completion install` and `mxv completion check` gives
    `linked to /path/to/target.fish` as the example of a reason.
  - The help of `mxv skills install` and `mxv skills check` describes the
    `path` and `reason` columns in crumbkit-skills 0.5.0's words.
  - The declaration of `root-verbs` in the opt-out report says that root
    help lists each verb under the heading of the root goal that names it.

- The lock that a command takes on a workspace is the file
  `$XDG_STATE_HOME/mxv/locks/<name>.lock`, which no command deletes. Before,
  it was `$XDG_STATE_HOME/mxv/runs/<name>/.lock`, which `mxv archive`,
  `mxv remove` and `mxv repair --orphans` deleted with the run directory, so
  a second command of the name could take the lock while the first still
  held it. A second `mxv archive` of a workspace whose archive is still
  deleting its volumes now fails with `mxv::workspace::busy`. An older mxv
  locks the old file and this one the new file, so a command of each
  version does not wait for the other. Let running mxv commands finish
  before an upgrade or a rollback. `runs/<name>/.lock` files that an older
  mxv left are never opened, and the next archive or removal of the name
  deletes them.
- `Store::create`, `Store::update`, `Store::advance` and `Store::repoint`
  refuse a workspace whose checkout is not made the way its kind says, a
  clone without a mirror or a worktree with one, with the new error
  `MismatchedCheckout`, and change nothing in memory or on disk. Before,
  the store wrote such a record, and the next read refused it as corrupt.
  mxv reports the error as `mxv::workspace::mismatched_checkout`, a defect
  of mxv to report. No command builds such a record.
- `Runs::lock` returns a `WorkspaceLock`, which keeps the name of the
  workspace it locks, in place of `Lock`. `Runs::build`, `Runs::up`, `Runs::down`,
  `Runs::reset_ports`, `Runs::prepare_task`, `Lifecycle::delete_archive_held`
  and `Lifecycle::finish_held` refuse a lock of another name with the new
  error `WrongLock` and do nothing. mxv reports it as
  `mxv::workspace::wrong_lock`, a defect of mxv to report. No command
  passes such a lock.
- A workspace's record stores its state: `creating` from the first write
  of `mxv new` until its last write, then `live`. `mxv restore` writes
  `live`. A record without a state, which an older mxv wrote, reads as
  `creating` when it is a workspace of clones whose checkouts all lack a
  commit, and as `live` otherwise. The next write of the record by any
  command, such as `mxv pin`, stores the state that was read. A worktree
  workspace whose `mxv new` stopped partway now reads as `creating`, where
  before it looked finished. `up`, `restart`, `ports reset`, `run`,
  `exec`, `pin`, `unpin`, `add`, `archive` and `clean` refuse a `creating`
  workspace with `mxv::workspace::creating`, in place of
  `mxv::checkout::incomplete` for a workspace of clones, and its hint names
  `mxv remove NAME` and then `mxv new`. `mxv reclaim` keeps it with a
  reason, and `down`, `status`, `show`, `logs` and `remove` work on it. A
  write fails with `mxv::workspace::state_changed`, and writes nothing,
  when another command changed the record's state after this one read it.
  The library gains `WorkspaceState`, `Workspace::state`, `Store::advance`,
  `AddOutcome::Creating`, `ArchiveRefusal::Creating` and the errors
  `StateChanged` and `TransitionRefused`. `Workspace::new` returns a
  `creating` record.
- `mxv archive` writes the workspace's record as `retiring` with the
  archive's id before it writes the archive record, and writes the archive
  record as `retired` once every checkout is retired, before it deletes
  the workspace's record. Every archive record is now version 3, which an
  older mxv refuses as newer. A version 2 record that says `retiring`
  counts as retired when no workspace of its name is the one it archives.
  An archive that stopped while `retiring`, whose workspace was then
  removed, can no longer be finished: `mxv archive -w` refuses with
  `mxv::archive::workspace_removed`, `mxv list --archived` names the
  problem, and `mxv repair` moves it aside. `mxv remove` of a `retiring`
  workspace warns about the archive record that stays. An archive that
  lost the head pin of any checkout now counts as pins gone, where before
  it needed to lose every one. `mxv remove --archived` refuses a `retired`
  archive. A record and a version 3 archive that both lack a stable id are
  two workspaces. A write of a record without a stable id gives it none
  while a version 2 `retiring` archive without a workspace id, or an
  archive record that cannot be read, holds its name, and `up`, `restart`
  and `run` then refuse with `mxv::workspace::archive_unfinished`. An
  archive id that is not 32 hexadecimal digits makes its record corrupt.
  The library gains `ArchiveId`, `ArchiveState::Retired`,
  `WorkspaceState::Retiring`, `Store::repoint`,
  `ArchiveRefusal::WorkspaceRemoved`, `Problem::ArchiveWorkspaceRemoved`
  and the error `ArchiveUnfinished`, and `Archive::id` is an `ArchiveId`.
- A workspace record whose checkout does not match the record's kind, a
  checkout of a workspace of clones without a `mirror` or a checkout of a
  workspace of worktrees with one, now reads as corrupt and lists as a
  record that cannot be read. An archive record whose checkout does not
  match the archive's kind reads as corrupt in the same way. The library's
  `Checkout::mirror` field becomes `Checkout::made`, a `Made` that is
  `Clone` with its mirror or `Worktree`; `Checkout::mirror()` returns the
  mirror, and `Checkout::new` takes the `Made`. The record on disk is
  unchanged.
- `mxv restore` and `mxv remove --archived` of an unfinished archive name
  `mxv repair` in their hint when `mxv list --archived` names it as the
  repair, such as for an archive whose workspace was removed before it
  retired every checkout, and `mxv archive -w` otherwise.
  `mxv::archive::name_taken` beside a newer workspace says that `mxv
  repair` moves aside an archive that stopped before it retired every
  checkout. `mxv remove` also warns about a stopped archive whose workspace
  id matches the removed record, as when an older mxv dropped the record's
  state. The library's `health::same_workspace` is public.
- A fetch from an SSH origin never waits on a question from ssh. mxv sets
  `SSH_ASKPASS_REQUIRE=force` and `SSH_ASKPASS=false` beside
  `GIT_TERMINAL_PROMPT=0` on every git command, so ssh fails at once where it
  would have asked for a passphrase or about a host key. The SSH command that
  a person set in `GIT_SSH_COMMAND`, `core.sshCommand` or `GIT_SSH` is kept.
  A person's own `SSH_ASKPASS` program no longer runs for mxv's git commands.
- A fetch that ssh refuses fails with `mxv::ssh::authentication_failed`,
  `mxv::ssh::host_key_unverified` or `mxv::ssh::host_key_changed`, whose
  fields name the remote, the host and ssh's line, in place of
  `mxv::mirror::fetch_failed` or `mxv::git::failed`. A script that matched
  `mxv::mirror::fetch_failed` for such a failure reads the new codes. The
  library's `Error` has the variant `SshRefused`, the method `ssh_host`, and
  the enum `SshRefusal`.
- No lifecycle verb fetches unless a person passes `--from-origin`. `mxv remove`
  and `mxv archive` no longer fetch origin before they count a clone's
  unpushed commits, so they work offline, and a refusal for unpushed commits
  names how long ago the clone's mirror last fetched. The count treats a head or tag that the mirror holds as pushed, and a note as unpushed, because the mirror holds no notes. Clones made by earlier versions keep their `refs/mxv/origin/` copies of tags and notes, which still count as pushed. The code
  `mxv::checkout::origin_unreachable` is gone.
- `mxv restore` refuses with the new code `mxv::restore::mirror_missing` when a
  clone's mirror is gone, lists what only that mirror held, and changes
  nothing. `mxv restore NAME --from-origin` makes a new mirror and rebuilds
  each checkout at the archived commit when origin holds it, else at the
  archived branch's tip on origin, else at origin's default branch. It lists
  each pin it could not restore as lost, in the new `lost_pins` field of the
  result, and then deletes the archive.
- `mxv archive`, `mxv restore` and `mxv remove` succeed with nothing to do
  when their goal already holds, and their documents gain a `state` of
  `done`, `planned` or `unchanged`. `mxv archive -w NAME` of a name that
  only a finished archive holds lists the archive's pins and names
  `mxv restore NAME`, where it refused with `mxv::archive::name_taken`.
  `mxv restore NAME` of a name that a workspace holds and no archive needs
  restoring exits 0. When an earlier restore stopped after it rebuilt the
  workspace and before it deleted the archive, so the archive has the
  workspace's stable id, `mxv restore` asks and then deletes the archive
  with its pins, which its new `stale_pins` field names, where it refused
  with `mxv::restore::name_taken`. It still refuses when the workspace's
  directory or a checkout's directory is gone, because the archive may
  then hold the only copies of the workspace's files, and `mxv repair`
  keeps such an archive too. When the records change after that question,
  `mxv restore` deletes nothing and fails with the new code
  `mxv::restore::changed`. `mxv remove NAME` of a name that no
  workspace has, and `mxv remove --archived NAME` of a name that no archive
  has, exit 0, where they failed with `mxv::workspace::not_found` and
  `mxv::archive::not_found`. Their `Effects:` lines now say idempotent. The
  library's `ArchivePlanned::Unchanged` and `RestorePlanned::Unchanged`
  report these cases.
- `mxv archive` keeps a checkout's git state instead of refusing it. It
  keeps the staged and unstaged changes and the untracked files as two
  commits that only its pins name, so the branch and the index never move,
  and `mxv restore` puts them back uncommitted. It also keeps each stash,
  the other local branches of a clone, the tags that origin does not hold,
  the per-worktree references, and each commit that no branch reaches,
  which comes back as `refs/mxv/kept/<commit>`. The pin of `HEAD` is now
  `refs/mxv/archives/<id>/<folder>/head`, and an archive record of version
  1 still restores.
- An archive refuses only what it cannot keep: a paused merge, rebase,
  cherry-pick, revert or bisect, a repository that git does not track, the
  work of a submodule, a file marked skip-worktree or assume-unchanged with
  local edits, a file that git does not list, an ignored path that holds a
  repository or is a socket, an entry of the workspace's directory such as a
  socket, and a file over `archive_max_file`. The failure code is `mxv::archive::unkeepable`, which
  replaces `mxv::archive::unsaved_work`, and each line says what to do.
  `--force` loses only those items.
- `mxv archive` keeps each checkout's ignored files outside the disposable
  set, such as `.env`, in a private tar at
  `$XDG_DATA_HOME/mxv/archives/<id>/ignored.tar`, readable only by the
  person, and `mxv restore` writes them back with their modes. The archive
  record lists each entry's path, kind, size, mode and digest. `mxv restore`
  refuses a damaged entry with `mxv::restore::ignored_damaged`, and
  `--force` restores without it. A path that the rebuilt checkout already
  holds refuses with `mxv::restore::ignored_taken`. The archive result and
  the dry run name each ignored file kept and each disposable directory
  deleted with its size, and `mxv list --archived` has the new field
  `ignored`. The archive document gains the rows `ignored` and
  `disposable`, and the restore document gains `put_back[].ignored` and
  `lost`.
- The `mxv archive` document is version 2. It replaces `wip` with `kept`,
  which lists what the archive kept of each checkout. The `mxv restore`
  document gains `put_back`, which lists what the restore put back, and
  warns about each submodule that comes back uninitialised.

### Added

- `mxv list` and `mxv list --archived` gain a `repair` column, which names
  the command that fixes each broken row, and list the broken rows first. A
  workspace whose checkout directories are all gone names `mxv remove NAME`,
  unless the directory that holds the workspace is gone too, as on a disk
  that is not mounted.
  An archive that stopped before it finished names `mxv archive -w NAME`,
  and its problem now reads "the archive stopped before it finished". A
  record that mxv cannot read says to move the file aside by hand, and the
  row of an archive record that mxv cannot read is named by the archive's
  directory instead of `archive`. An
  unfinished archive whose name a newer workspace holds shows only in
  `mxv list --archived`, and no longer marks the newer workspace's row.
  The library's `multiverse::health::classify` finds these records.
- `mxv repair NAME` repairs the broken records that `mxv list` names it
  for, and `--dry-run` prints what it would do. It asks before it changes
  anything, and `--yes` repairs without asking. An unfinished archive whose
  pins are gone moves aside to the hidden directory `.NAME.<id>`, and
  nothing is deleted; that directory and the archive's
  `$XDG_DATA_HOME/mxv/archives/<id>/ignored.tar` stay until
  `mxv repair --orphans --delete-aside` deletes them. An older unfinished archive beside a newer workspace of the same name is
  finished under the name `NAME-YYYY-MM-DD`, so `mxv restore` of that name
  brings it back. An archive left beside the workspace that a restore of it
  rebuilt is deleted. The new problems read "the archive stopped before it
  finished, and its pins are gone" and "the restore stopped before it
  deleted the archive". `mxv archive -w` refuses to finish an archive whose
  pins are gone, with `mxv::archive::pins_gone`, and the recovery of
  `mxv::archive::name_taken` for an older archive now names `mxv repair`.
  A repair that stopped while it moved an archive to its dated name shows
  as "a repair stopped while it moved the archive to its dated name" on
  both records, with `mxv repair` of the older name, and `mxv archive -w`
  of either name refuses with `mxv::archive::rename_unfinished`. A repair
  whose records change after its question repairs nothing and fails with
  `mxv::repair::changed`, and an older archive whose dated names are all
  taken fails with `mxv::repair::names_taken`. A repair of a name whose
  archive record cannot be read fails with the read error. A repair that
  finishes a stopped rename holds the locks of both names.
  The library's `classify` now takes the backend that reads the pins, and
  `Workspaces::repair` carries out the repairs. phase exports its calendar
  functions as `phase::calendar`.
- `mxv repair --orphans` lists, with their sizes, the archives that a
  repair moved aside and the directories of ignored files whose archive no
  record names. Each holds the only copies of its files, so it is deleted
  only with `--delete-aside`, and `--yes` only skips the question. The
  listing and `--dry-run` write nothing. When an archive record cannot be
  read, the command fails with that record's error and deletes nothing.
  The `mxv/repair/v1` document gains an `orphans` list, and its `name` is
  null with `--orphans`. The library's `Workspaces::orphans` and
  `Workspaces::sweep_orphan` find and delete these items.
- `mxv repair --orphans` also lists the run files of a name that no
  workspace has and phase's run record and logs of a workspace id that no
  record holds, and deletes them after the person confirms. phase's sweep
  first stops the processes and removes the containers, networks and run
  volumes that the run record names. `--delete-volumes` also deletes the
  docker volumes that hold the data of such an id, before its logs, so a
  docker failure leaves the logs and the next sweep lists the id again.
  With `--delete-volumes` the listing also finds, by docker's workspace-id
  label, the volumes of an id that phase keeps nothing of, such as those
  that a removal stopped after it forgot the id leaves. Each is a
  `run_record` row whose path is its first volume's name, and a row's
  `volumes` names the volumes of its id. It counts only the volumes whose
  run-record label names this state directory, so it never lists those of
  another state directory, and it asks docker once for the whole listing.
  A docker that does not answer then fails the listing before anything is
  deleted, run files included. A directory of the
  archives without its own record is listed as kept and never deleted. A
  deletion that fails partway has the row state `partly_deleted`, and its
  `deleted` field names the paths that are gone. A row now carries `name`,
  `id`, `deleted` and `volumes`.
- `Runs::forget` deletes the logs of a workspace id only once phase's sweep
  removed its run record, so the logs stay while phase keeps the record.
  `mxv remove` and `mxv archive` then report a warning for each process
  that phase kept the record for, which says to stop that process.
- The config key `archive_max_file` sets the largest file of a checkout's
  uncommitted work that `mxv archive` keeps, such as `"1GiB"`. It defaults
  to 100 MiB.

### Removed

- `mxv archive --wip` is gone, because an archive keeps uncommitted work
  without a commit on the branch.

### Fixed

- An `mxv` command that is killed while it starts a process no longer leaves
  a keeper and an app that the run record does not name. The engine records
  the keeper before it tells the keeper to start the app, and a keeper that
  never hears that the engine recorded it starts nothing. An `up` whose keeper
  misses its handshake deadline now stops the app that the keeper had started,
  and reports `AppOutcome::StartUncertain` when it cannot stop it, with the
  app named in the record so that `down` finds it. While an old and a new
  `mxv` binary run side by side during an upgrade, one start can fail,
  because a keeper and an engine of different versions do not speak the same
  protocol; the next `up` starts the process.

- A command that waited for a workspace's lock while another command
  archived the workspace no longer writes the workspace's record back.
  `mxv up`, `down`, `restart`, `ports reset`, `pin`, `unpin`, `exec` and
  `run` fail with `mxv::workspace::not_found` when the record is gone once
  they hold the lock, and start or stop nothing. `mxv reclaim` keeps such a
  workspace with a reason and carries on, and `mxv remove --force` exits
  with the state `unchanged`. `Store::update` returns the new
  `Error::RecordMissing` when no record of the name exists, so only
  `Store::create` makes a record.
- phase treats a container or volume as its own only when its `mxv.run-record`
  label names its own run record. A second state directory that holds the
  same workspace id, such as a copy, no longer lists, deletes, mounts or
  replaces the first one's volumes and containers; `mxv up` fails with
  `mxv::docker::foreign_object` instead. A state directory reached by another
  path than the one that made its objects (a moved `XDG_STATE_HOME`, or one
  set through a symbolic link) now finds them foreign and keeps them; run mxv
  with the old path to reach them again.
- phase's sweep, `status` and volume-owner helper removal also match an object
  by its `mxv.run-record` label, as `up` does. `mxv remove`, `mxv archive` and
  `mxv repair --orphans` in a second state directory that holds the same
  workspace id no longer remove the first directory's containers, volumes or
  volume-owner helpers; the sweep reports them as gone, and `mxv status`
  there shows them as not running. A state directory reached by another path
  than the one that made its objects now leaves its containers running when
  `mxv remove` deletes its run record. `docker rm --force` of the containers
  that `docker ps --filter label=mxv.workspace-id=<id>` lists removes them.

## [0.1.1] - 2026-11-11

### Fixed

- The release binaries build again, because mxv no longer depends on an
  unmaintained TOML crate. Version 0.1.0 was tagged but never released.

## [0.1.0] - 2026-11-11

### Added

- `deadband status` and `deadband top` show the workspace of each job that
  mxv admits and of each job that a person runs by hand inside a workspace.
  mxv names the workspace in every build and task it asks deadband for, and
  `mxv new`, `mxv add` and `mxv restore` write an empty file
  `.deadband-workspace` in the workspace's directory, which deadband finds
  by walking up from a job's directory. `mxv up`, `mxv restart`, `mxv run`
  and `mxv exec` write the file when it is missing, so a workspace made
  earlier gains it, and warn when they cannot. `mxv remove` and
  `mxv archive` delete an empty marker, and `mxv restore` writes it again.
  `.deadband-workspace` is no longer allowed as a checkout's folder. mxv
  now needs the deadband commit that names workspaces, crumbkit/deadband#19.

- `mxv completion install` writes mxv's shell completion where the shell loads
  it, for bash, zsh and fish, with `--shell` naming the shell when `SHELL`
  does not. `mxv completion check` exits with 1 when the installed file is
  missing or differs, and `mxv completion print` prints the registration
  script. mxv also answers a shell's completion request, which a shell makes
  by running mxv with `COMPLETE` set to its name. mxv now builds on crumbkit
  0.3.1 with its `complete` feature.
- `mxv skills install` writes mxv's agent skill into each agent skills
  directory that exists, `~/.claude/skills`, `~/.agents/skills` and
  `$CODEX_HOME/skills`, or into the directory that `--dir` names.
  `mxv skills check` exits with 1 when an installed copy is missing or
  differs from the installed mxv's, and `mxv skills print` prints the skill.
  The skill is crates/mxv/skill/SKILL.md, embedded in the binary.
- `mxv self runs list` prints each run in mxv's usage log, grouped by
  session, as the `mxv/self-runs-list/v1` document. mxv now builds on
  crumbkit 0.3, so the figures of `mxv self figures list` also give each
  command `p50_ms`, `p50_out_bytes` and `cut_short`, and a failure's JSON
  recovery can have the kind `terminal`, which names a command that a person
  must run in a terminal.
- `mxv exec` takes no UNIT to run a one-off command on the host in a
  workspace, as `mxv exec -- COMMAND`. The command needs no running unit and
  no `mxv.toml`. It receives the workspace's ports and every service's
  variables, and mxv starts no services or units. `--repo CHECKOUT` chooses
  the checkout it runs at the top of, and otherwise it runs in the current
  directory when that is inside a checkout. It takes one place from deadband,
  and `--no-gate` asks deadband nothing. `mxv exec UNIT` behaves as before and
  refuses both flags.
- `mxv archive` removes a workspace's directory and name and keeps each
  checkout's commit in a reference under `refs/mxv/archives/`, and
  `mxv restore` rebuilds the workspace at those commits with the same stable
  id, so its docker volumes attach again. `mxv list --archived` lists the
  archives and `mxv remove --archived NAME` deletes one. mxv writes archives
  to `$XDG_STATE_HOME/mxv/archives`, and a tool that read the failure code
  `mxv::command::unbuilt` no longer sees it. The JSON document of `mxv list`
  has the new fields `archived` and `local_only`, and the document of
  `mxv remove` has the new field `pins`. An archive refuses and names an
  untracked nested repository, a submodule's unsaved work, a
  skip-worktree or assume-unchanged file with local edits, a file that git
  does not list, a commit that only an unusual reference keeps, and a paused
  git operation. The new failure
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
  `mxv remove` deletes the file, and only when you no longer need its data,
  run `docker volume rm` on each name in
  `$XDG_STATE_HOME/mxv/runs/<WORKSPACE>/.volumes` that does not start with
  `mxv-`. An image with a `VOLUME` path keeps its only copy of that data in
  such a volume. That file also lists the workspace's named volumes, and a name
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
- mxv-phase's `AppOutcome::AwaitingGrant`, `AppOutcome::awaiting_grant` and
  the code `phase::grant::awaiting` are gone. The permission check runs
  before `Phase::up`, so nothing could return them.

### Changed

- `mxv up` and `mxv restart` compare the permissions of the selected host
  units and their dependencies with what a person has granted to the
  repository, before anything builds, stops or starts. A unit that is new,
  that holds a permission entry the grant lacks, or that would leave the
  sandbox asks for more. At a terminal, mxv lists what each unit asks for
  and asks `Grant them? [y/N]`. Without a terminal, or with `--no-input`,
  the command fails with `mxv::permissions::not_granted`, whose recovery is
  `mxv up` run by a person in a terminal, and every running unit keeps
  running. `mxv exec UNIT` refuses with the same code and never asks. No
  flag or variable approves a grant. Each repository already in use asks
  once after the upgrade, so an agent's `mxv up` there refuses until a
  person answers in a terminal. The grant lives in
  `$XDG_STATE_HOME/mxv/phase/grants/`, is shared by every workspace of the
  repository, only grows, and is never deleted by mxv. Move its file away
  to be asked about every unit again.
- `mxv down` with names keeps a named unit that a running unit outside the
  stopped set depends on, and what that unit depends on in turn. Each kept
  unit fails the command with `mxv::process::still_needed`, which names the
  units that need it and the `mxv down` that stops them all. The other
  named units stop, and the command exits 1. `mxv down` with no names and
  `mxv restart` stop what they stopped before.

- `mxv clean --force` is now `mxv clean --disposable`. It deletes, with
  `--yes`, the paths that only a repository's `mxv.toml` declares
  disposable. The old spelling is gone and is not accepted, so update any
  script or cron job that passes it. The `--force` flags of `remove` and
  `archive` keep their names. `mxv reclaim --need disk=<SIZE>` takes the
  same `--disposable` flag and deletes those paths too when it is given.
  Without the flag, reclaim keeps them, as before. The `reason` of a row
  that kept such a path now names the command that deletes it: rerunning
  reclaim with `--disposable`, or for `clean`, answering its prompt or
  running `mxv clean <NAME> --yes --disposable`.
- A workspace with several checkouts now reads its workspace manifest
  strictly: while one checkout's `mxv.toml` cannot be parsed, the read
  fails with that file's error. Before this change the read skipped the
  broken file and checked the workspace manifest against the names of the
  other checkouts, so it could report an undeclared name with a different
  message. The commands that read the workspace manifest for its
  `disposable` globs, which are `remove`, `archive` and `clean`, now warn
  and use no `disposable` globs of the workspace manifest in any checkout
  while one checkout's manifest is broken. `remove` and `archive` without
  `--force` can then refuse because of ignored files that only those
  globs cover, and `clean` reclaims fewer paths. `list` and `show` reach the
  same read through `disk_usage`, so their disposable byte figures shrink
  too, and `show` carries the same warning. Fix the broken file to
  restore the globs.
- mxv uses crumbkit 0.2.0, which changes some documents that a tool may
  read. `mxv list --json` and `mxv list --archived --json` now print a
  top-level `next` array of next steps when there are any, and the schemas
  of `mxv list` and `mxv status` gain the `next` field, although `mxv status`
  never prints it. `mxv commands list --json --full` gives the root row a
  `goals` field that holds the headings of the help and the commands under
  each. The descriptions in the schemas of the failure code and the output
  budget no longer say "in the tool's spelling".
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

### Fixed

- mxv works on a machine where git's `safe.bareRepository` setting is
  `explicit`. Before this fix, `mxv new`, `mxv add` and `mxv archive` failed
  there with "not in a git directory" or "cannot use bare repository",
  `mxv restore` refused and said that the commit was missing, and
  `mxv remove --archived` exited 0, printed a warning and left the archive's
  pin in the mirror. A pin that an earlier `mxv remove --archived` left
  behind stays in the mirror.
