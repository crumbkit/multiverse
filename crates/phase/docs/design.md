# phase design

Status: draft, 4 November 2026. Nothing here is built yet. The table at
the end lists each decision, and says which are decided and what this
draft assumes for the rest.

## What phase does

phase runs the apps of one workspace on the local machine. For each app it:

1. gets the app's program, either by downloading and verifying a published
   release or by building the checkout you are editing;
2. resolves the environment variables the app's manifest declares, including
   secrets, and gives them to that app's process alone;
3. starts the process under the operating system's sandbox, with access only
   to the files, ports and hosts its manifest asks for;
4. starts apps in dependency order and waits for each one to pass its
   readiness probe before starting the apps that depend on it;
5. restarts a process that fails, under the restart policy it declares;
6. records every process and container it started, so that it can find and
   stop them after phase itself crashes;
7. reports status and logs as JSON for people, agents and other tools.

Supporting services that are easier to run in a container, such as a
database or a queue, run through Docker Compose. Apps you build and edit run
on the host.

### Out of scope for now

- Running apps in a cloud account. phase is designed so that a later
  backend can stand an app up remotely from a reviewed plan, but the first
  version runs everything locally.
- Running coding agents. Whether an agent runs under phase or under `mxv` is
  decided later.
- Building container images. A container app names an image that already
  exists, or the host program builds one and passes it in the plan (decision
  P14). phase runs that image with the app's checkout bind-mounted, the
  cache volumes the plan names, and the user the plan names.

## How phase fits with multiverse and starmap

| Tool | What it owns |
| --- | --- |
| multiverse (`mxv`) | Workspaces, their checkouts, the per-workspace port ledger, environment formats, and the workspace record. |
| starmap | Declaring, checking and resolving the environment variables an app reads, including secrets from 1Password and the Keychain. |
| phase | Running the workspace's apps: sources, sandbox, environment injection, dependency order, probes, restarts, run records, status and logs. |

multiverse builds a run plan from the workspace record and each app's
manifest, and calls phase's library with it. phase never allocates ports or
creates checkouts; it reads them from the plan.

multiverse already runs a workspace's processes with `mxv up`, `mxv down`,
`mxv restart`, `mxv status` and `mxv logs`. phase replaces those commands
from its first release (decision P7), so phase's first release must run
everything they run today.

## Concepts

- **Workspace.** A directory that multiverse created, holding one checkout
  per repository and a record of the workspace.
- **App.** One program the workspace runs, such as an API server or a
  database. An app has one or more processes. An app is a named entry in a
  manifest, and one manifest can declare several apps. App names are unique
  within a workspace, and phase names each app's Compose project after it.
- **Source.** Where an app's program comes from. It is either a *release*,
  which phase downloads and verifies, or a *checkout*, which phase builds
  from the workspace's working tree. You choose per app, per workspace: the
  apps you are editing use their checkout, and every other app uses a
  release.
- **Runner.** How a process runs. A *host* process runs directly on the
  machine under the sandbox. A *container* process runs through Docker
  Compose.
- **Service.** A supporting app that several apps share, such as one
  Postgres. A service is declared once, in the workspace's manifest, and
  each app that uses it says what it needs from it. See
  [Shared services](#shared-services).
- **Plan.** Plain data describing every app, process, source, port, probe,
  restart policy, dependency and permission of one workspace. A plan holds
  no secret values, so it can be printed, stored and reviewed.

## The app manifest

Each repository describes its app in its manifest. The fields below extend
the `mxv.toml` that multiverse already reads (decision P11).

### Which manifest is read

multiverse sets up a workspace's manifest at `<root>/<name>/mxv.toml`, above
the checkout, when it creates the workspace. It copies the file given with
`mxv new --manifest <file>`, or else a template in
`~/.config/mxv/templates/*.toml` whose origin URL matches the repository's
origin. Otherwise the workspace has no manifest of its own.

multiverse's own keys (`ports`, `disposable`, `clone_on_new`, and
`processes` and `containers` until phase takes them over) come from exactly
one file: the workspace's manifest if it exists, or else the `mxv.toml` at
the root of the checkout. The two files are never merged, and `mxv status`
and `mxv show` name the file used. multiverse checks that the file is valid
TOML and passes every other table to phase untouched, so phase owns the
schema of its own tables.

phase reads each of its tables from one place:

- `[app]`, `[release]`, `[build]`, `[process.*]`, `[permissions]` and
  `[needs.*]` describe one app, and come from that app's repository. For an
  app you are editing, phase reads the `mxv.toml` in its checkout. For an
  app run from a release, phase reads the `mxv.toml` of the repository at
  the release's tag.
- `[service.*]` describes the workspace's shared services, and comes from
  the workspace's manifest. A repository's `mxv.toml` can declare services
  only while it is the workspace's sole manifest, which is the case for a
  workspace with one checkout and no manifest of its own.

A workspace manifest can also declare an app, for a repository that has no
`mxv.toml` of its own, such as someone else's repository or one you cannot
commit to. It names the checkout the app runs from and holds the same
tables. You can also make a workspace manifest replace a repository's own
app tables by setting `local = true` on that app; otherwise a repository's
`mxv.toml` wins wherever one exists, and phase rejects a workspace manifest
that declares the same app without `local = true`, naming the file and app.

You are the publisher of an app that a workspace manifest declares. phase
shows its permissions for your approval when it first runs and whenever
they grow, exactly as for any other app. Such an app always runs from its
checkout, because it has no publisher's release to verify.

Each app's tables come from one file, so nothing is merged.

```toml
[app]
name = "api"
depends = ["auth"]                        # other apps that must be ready first

# Where the published program comes from.
[release]
github = "crumbkit/example-api"           # owner/repository
asset = "example-api-{target}.tar.gz"     # {target} is the Rust target triple
binary = "example-api"                    # path of the program inside the asset
verify = "attestation"                    # "attestation", "sha256" or "minisign"

# How to build the program from a checkout.
[build]
run = ["cargo", "build", "--release", "--locked"]
output = "target/release/example-api"

[process.server]
run = ["{program}", "serve"]
port = "http"                             # a named port from multiverse's ledger
ready = { http = "/healthz" }             # any 2xx status passes
restart = { policy = "on-failure", max = 5, backoff = "1s..30s" }
stop = { signal = "TERM", grace = "10s" }

# What the process may do inside the sandbox.
[permissions]
write = ["$STATE"]                        # the app's own state directory
listen = ["http"]
connect = ["api.stripe.com"]              # other apps by name, or external hosts
secrets = ["STRIPE_KEY"]

# What the app needs from the workspace's shared services.
[needs.postgres]
database = "api"                          # the app's own database and role
```

The environment variables an app reads are declared by starmap, not here.
`secrets` in `[permissions]` lists which of those variables are secrets, so
that phase can show you when an app starts asking for a new one.

## Shared services

Decided 4 November 2026 (P13): a service such as Postgres is declared once
per workspace and shared by the apps that need it. Each workspace runs its
own copy, so workspaces cannot affect each other, and removing a workspace
removes its data.

The workspace's manifest declares the service:

```toml
[service.postgres]
kind = "postgres"                         # phase knows how to provision this kind
runner = "container"
image = "postgres:17"
port = "pg"                               # a named port from multiverse's ledger
ready = { tcp = "pg" }
```

An app's `[needs.<service>]` table says what it needs from the service. A
need makes the app depend on the service, and lets the app's sandbox
connect to the service's port. An app that declares no need cannot reach
the service.

### One database and role per app

For a service of kind `postgres`, phase gives each app that needs it its
own database and role. Before the app starts, phase creates the database
and role if they do not exist, and gives the role access to that database
alone. phase then adds `DATABASE_URL`, naming that role and database, to the
app's environment. The app's starmap declaration still names
`DATABASE_URL`, and records phase as its source. One app cannot read
another app's data.

phase generates the password of the service's superuser and of each role.
It keeps them in the operating system's credential store, the Keychain on
macOS, and never in a file, the plan, the run record or a log. A role's
password reaches only its app's process, and the superuser's password
reaches only the service's compose child and phase's provisioning step.

A service without a `kind` is shared as it is: every app that needs it gets
the same access, and phase provisions nothing.

### Lifecycle and data

- `mxv phase up api` starts the services api needs before it starts api.
- `mxv phase down api` leaves a service running while another running app
  needs it. `mxv phase down` with no app stops every app and then the
  services.
- A service's data lives in a Docker volume labelled with the workspace and
  service. It survives `down` and restarts. `mxv phase reset SERVICE`
  removes it after asking, and removing the workspace removes it too.
- Schema migrations are each app's job. An app runs them as a process that
  must finish before its server starts.

## Starting a workspace

1. multiverse builds the plan and calls `Phase::up` with it.
2. For each app, phase settles its source. For a release, it downloads the
   asset for the pinned version, verifies it, and stores the program in a
   content-addressed cache. For a checkout, it runs the build command in the
   checkout. If a release is pinned but has no asset for this machine,
   phase stops with an error that names the app and the missing asset.
3. phase checks each app's permissions against what you have granted (see
   [Trust](#trust)). Apps whose permissions grew wait for your answer; the
   rest continue.
4. phase starts the apps in dependency order. For each process it resolves
   the app's environment, generates the sandbox profile, starts a keeper,
   and waits for the readiness probe.
   Before it starts any process or container, phase writes the run
   record with every object the plan will create, and it updates the
   record as each keeper and container starts. See
   [Lifecycle rules](#lifecycle-rules).
5. phase returns a report.

## Engine

### Keeper per process

Each host process has a small keeper, a hidden `phase keep` subcommand of
the same program. The keeper is the parent of exactly one app process. It
holds only that process's environment, applies the sandbox, starts the
process in its own process group, gives it its log file as a file
descriptor, restarts it under its policy, and exits when the process is
stopped.

The engine starts a keeper, and the keeper's first action is `setsid`, so
it leads a session and a process group of its own, and a hangup or Ctrl-C
at the engine's terminal never reaches it. The keeper's arguments name the
workspace's stable id, the app, the process and the state directory, and
hold no value. The keeper's environment and the app's hold only `PATH`,
`HOME` and the process's resolved environment. The keeper's
standard input carries a `phase/keeper/v1` specification with the command,
directory, log, restart policy and stop handling. The keeper writes one
handshake line to standard output once the process has started or failed
to start, and the engine waits for that line until its deadline. The
keeper records each start, restart count and stop of the process in the
run record from a thread of its own, so waiting for the lifecycle lock
never blocks supervision. It notes its start, restarts, resolve failures,
record-write failures and exit in `<log dir>/<app>.<process>.keeper.log`,
which never holds a value and also receives its standard error.

The keeper's record notes are written asynchronously, after the keeper has
acted, so the record can lag the process. The write-ahead writes are the
engine's own, which it makes under the lifecycle lock before it acts. A
crash between the keeper's action and its note still leaves the record
naming the running pid, which `phase sweep` checks. A keeper's note applies
only while the record names the keeper's own process, so it never
overwrites a process that another operation recorded meanwhile.

When the keeper's handshake misses the deadline or cannot be read, the
engine sends the keeper `TERM`, so it stops any process it started, waits
the process's grace and kill wait, and then kills the keeper's group. When
the keeper's start of the process takes longer than that wait, the engine
kills the keeper before it can stop the process, which then runs with no
keeper and no entry in the run record. A stop request during a backoff or
while the keeper resolves an environment ends the keeper without a
restart.

A keeper that a stop request (`TERM` or `INT`) ends does not wait for the
lifecycle lock. It tries the lock once more without waiting. When the lock
is free, as it is after a `TERM` from outside phase, the keeper writes the
record changes still queued. When another process holds the lock, as the
`down` or `sweep` that stopped the keeper usually does, the keeper drops
those changes and notes each one in its diagnostic log, and that operation
records the stop itself. A keeper that waited would only delay that
operation. A keeper whose app ended any other way waits for its writes as
long as the writer waits for the lock, and a stop request during that wait
ends it in the same way.

The library never reads the environment. The program that links phase
reads its own environment in `main` into an opaque `KeeperEnvironment`,
which shows no value, and passes it to the keeper, which keeps only the
resolved names that its specification lists and sets them on the
process's command.

When a restart needs a fresh environment, the keeper resolves it through
the `EnvironmentSource` that the program linking phase passes to it, and
applies it to the restarted process. When resolving fails, the keeper
notes it, waits the next backoff and tries again, up to the policy's most
restarts, and then exits with status 3. The backoff starts again from its
first wait after a run longer than its longest wait or a minute, whichever
is longer.

No long-lived process holds the secrets of every app. If a keeper is killed,
its app process keeps running, and `phase sweep` or the next `phase up`
finds it from the run record.

### Probes and restarts

phase uses the probe and restart vocabulary of process-compose:

- probes: `http` (any 2xx passes, and no redirect is followed), `tcp`,
  `command` and `log` (a line written since the start matches a regular
  expression), each with a period, a timeout, and success and failure
  thresholds;
- restart policies: `no`, `on-failure` and `always`, with exponential
  backoff and a maximum count.

A process with no readiness probe is ready as soon as it starts.

A readiness wait during `up` attempts the probe every 100 ms, or every
period when the period is shorter, until the passes in a row reach the
success threshold. Decided 4 November 2026: mxv polled a starting process
every 20 ms, and the first release must not make `up` slower, so readiness
does not wait a whole period between attempts. Failures do not end a
readiness wait, and the process's ready timeout bounds it. A log probe's
match stays matched, so its later attempts pass at once and reach the
success threshold without an interval between them. The wait checks the
log for a match before it checks whether the process ended, so a process
that writes its ready line and then exits counts as ready.

A liveness wait in a keeper attempts the probe once each period, and ends
when the failures in a row reach the failure threshold.

A command probe of a host process runs in its own process group, and phase
kills that group however the command ends. A command probe of a container
process runs inside its container through `docker exec`. When it times
out, phase kills only the local `docker` client, and the command inside
the container may keep running. The container module may wrap the command
in the image's `timeout` program where the image has one.

### Containers

Each container app is one Docker Compose project, and each of its processes
is one Compose service. Docker's restart policy restarts container
processes.

phase labels every Compose project, container, network and volume it
creates with the workspace and app, and records each container by its
immutable id.

The labels are `mxv.workspace-id`, the workspace's stable id, which is 32
lowercase hex digits that mxv mints when it creates the workspace and keeps
in the workspace's record; `mxv.run-record`, the absolute path of the
workspace's run record, `<state>/runs/<id>.json` under phase's state
directory, which mxv sets to `$XDG_STATE_HOME/mxv/phase`; and `mxv.app`, the app, which is the workspace's
name until mxv names apps. Objects also keep mxv's older labels,
`mxv.store`, `mxv.workspace`, `mxv.container` and `mxv.volume`, so the
objects that mxv created before it wrote the new labels stay manageable.
The container engine finds an object by `mxv.workspace-id`. It matches an
object that carries only the older labels by `mxv.store` and
`mxv.workspace` only when the host names the workspace's older scope, and
`sweep` names none, so `sweep` leaves such an object alone. The engine
never removes an object without an mxv label. It compares each label's
exact value, read as JSON, so a value that holds a tab, a newline or a
trailing space never matches another value.

A container gains the new labels when mxv recreates it. Docker cannot
change a volume's labels, and mxv never recreates a kept volume, so a
volume that mxv created before the new labels keeps only the older labels
until a migration copies it into a newly labelled volume. Until then it is
found only through the older labels.

The `mxv.run-record` label holds an absolute path. A run of mxv with
another `XDG_STATE_HOME` keeps the run record elsewhere, so the label of
an object that an earlier run created then names a path that this run
does not use. The label also shows the home directory's path, which
usually holds the user's name, to anyone who can read the object's
Docker labels.

phase treats an object as gone only when Docker's daemon answered that it
does not have it. An unknown Docker context, a daemon that does not
answer and a missing `docker` program are errors, so neither `sweep` nor
mxv's own commands report a running container as gone because Docker
could not be asked.

### Container secrets

Decided 4 November 2026 (P3): phase delivers each secret to a container as
a file in a memory-only directory, and never through the container's
environment.

- The Compose file mounts a tmpfs at `/run/secrets` with mode 0700. The
  directory lives in the container's memory, so a secret never reaches the
  disk of the Mac or of the Docker virtual machine.
- After the container starts, phase writes each secret to
  `/run/secrets/<NAME>` by running `docker exec -i` with the value on
  standard input. The value never appears in an argument list, the Compose
  file, `docker inspect`, `docker compose config` or a container label.
- The image's entrypoint is a small wrapper that phase supplies. It waits
  until every file the app needs exists, then starts the app. The app reads
  its secrets from those files.
- To rotate a secret, such as a short-lived cloud key that its resolver
  renews before it expires, phase rewrites the file the same way. The
  container keeps running. An app that reads the file each time it uses the
  value picks up the new value.
- phase never serves a secret on the Mac's loopback, because OrbStack lets
  every container reach the Mac's `127.0.0.1`.

An image that can read a secret only from an environment variable falls
back to environment references. The Compose file maps the variable to a
private reference, such as `POSTGRES_PASSWORD: "${MXV_ENV_postgres_1:-}"`,
and phase sets the `MXV_ENV_*` values only on that service's
`docker compose up` child process. phase never lists a variable by its
bare name, because Compose would then read it from the child's own
environment, and a container variable named `PATH` or `DOCKER_HOST` would
change which docker program or daemon the child uses. With this fallback,
`docker inspect` and every process in the container can read the value, and
a new value means recreating the container.

The eval `/evals/container-secret-delivery-orbstack` in the owner's board
measured each option on OrbStack 2.2.5 with test values. Compose secrets
copy the value onto the virtual machine's disk, readable by every user in
the container. A file bind-mounted from the Mac breaks when it is replaced.
The tmpfs file was found nowhere else, and rewriting it took under 100 ms.

### Run record and sweep

phase writes a run record (`phase/run/v1`) for each workspace. It names every
keeper and app process by pid, process start time and process group, every
container by id, and the hash of the plan that started them. It holds no
environment values.

`phase sweep` reads the record and stops whatever is still running from a
run that phase no longer supervises. It matches a process by pid and start
time together, so a reused pid is never mistaken for an app.

The record of a workspace is `<state_dir>/runs/<workspace-id>.json`, with
mode 0600. phase writes it to a temporary file, syncs it and renames it over
the record, so a reader sees a whole record. The lifecycle lock is an
exclusive `flock` on `<state_dir>/runs/<workspace-id>.lock`. Each object in
the record has a state: `planned`, `running`, `stopping` or `stopped`.

A process's start time is in microseconds: since the Unix epoch on macOS,
where `proc_pidinfo` reports it, and since boot on Linux, where
`/proc/<pid>/stat` reports it in clock ticks. On Linux the record also holds
the boot id from `/proc/sys/kernel/random/boot_id`, and a process recorded
in an earlier boot is gone. phase only compares a start time with one it
read on the same machine.

When phase may not read the process that has a recorded pid, as inside an
agent's sandbox, `sweep` reports the process as unknown, leaves it alone,
and keeps its entry unchanged.

A process that leads its group can end and leave members in the group.
The group keeps its id while it has a member, but once the last member
ends, another program can start a group with the same id. So when no
process has the recorded pid and a group with that id still has a member,
phase counts the group as its own only when something proves it:

- The keeper that started the app is still the one phase started. It
  recorded the app's group and still supervises it, so it vouches for it.
- Within one stop, phase already found the leader to be its own before it
  ended. The group cannot have emptied since, because phase saw the leader
  in it.

Without that proof, phase never signals the group. `down` and `restart`
keep the entry, report the app as left, and add a note that names the
group. `up` starts nothing in its place and reports it the same way.
`sweep` reports the process as unknown, and `status` shows it as `unknown`
with the reason `leader-gone`.

A member of a group that has ended and that its parent has not reaped yet,
a zombie, runs nothing, and no signal ends it. A group whose only members
are zombies counts as gone. macOS refuses a signal to such a group with
`EPERM`, which phase treats as nothing left to signal.

Each volume in the record has a lifetime. A `run` volume, such as a scratch
cache, belongs to one run, and `sweep` removes it. A `workspace` volume
holds data that outlives a run, such as a database's, and `sweep` leaves
it; only `reset` removes it.

`sweep` stops keepers first, so no keeper restarts its process, then host
processes, containers, networks and volumes. For a host process it sends
`TERM` to the process group when the process leads its group, and to the
process alone otherwise. On Linux it signals a single process through a
pidfd after checking the start time again. It waits up to the kill wait, sends `KILL`, and
waits again. A process that is still there is reported as unstoppable, and
the record keeps it. `sweep` removes the record once every object in it is
stopped. `sweep --dry-run` reads the record without the lock and changes
nothing.

`sweep` finds a container or network whose id the record does not hold yet
by its name, and only when it carries phase's workspace label with the
workspace's stable id. When Docker does not answer, `sweep` fails with
`phase::docker::unavailable`; when Docker refuses a removal, it fails with
`phase::docker::refused`. Either way the record keeps what is left.

A keeper that started but whose pid phase had not yet recorded when it
crashed is missing from the record. A later change will let `sweep` find
such keepers by their command line, which names the workspace's stable id.

### Status and logs

`phase status --json` prints a `phase/status/v1` document with one row per
process: its app, runner, source, state, pid or container, address, start
time, restart count and log path. `phase logs` prints a process's log, and
`--follow` keeps printing it. Container logs come from `docker logs`.

`status` reads the run record and checks every entry that is not stopped:
a host process and its keeper in the process table, by pid and start
time, and a container with Docker, by its recorded id. It never trusts a
stored state alone, because a keeper that an outside `TERM` stops while
another operation holds the lock drops its record notes. A process that
phase cannot prove is its own, such as one it may not read or an entry
with a group of 1, which `sweep` leaves until someone removes it, shows
as `unknown` with its reason. A container shows as `unknown` when Docker
does not answer or reports a state that phase does not know, such as
`removing`, and never as stopped. A container that the record plans and
Docker has not started shows as `starting`. The workspace's state comes
from the rows that are not unknown. It is `unknown` when the unknown rows
decide it: every other row is running, or none of the others runs. With the workspace's plan, the rows
follow the plan and carry its sources and addresses, and record entries
that the plan lacks still show. Without a plan, as when the manifest is
invalid, the rows are the record's live entries and leave the plan's
fields empty.

## Environment and secrets

phase resolves an app's environment through starmap. starmap's resolution
produces an `AppliedEnvironment`, a type that can list variable names and
apply values to a `Command` but cannot return a value. It has no `Debug`,
`Display` or `Serialize` implementation.

The plan names each process's variables and holds no value, literal or
secret. The host program's `EnvironmentSource` supplies every value, and
mxv's renders its manifest's literal values and their `{port.NAME}`
placeholders before phase sees them. phase resolves each process's
environment separately, so two processes of one app can receive different
values for one name, such as `PORT`.

A secret value takes only these paths:

- for a host process, from starmap's resolvers into the keeper's
  environment, and from the keeper into the app process when it starts;
- for a host process's `command` readiness probe, from starmap's
  resolvers into the environment of the probe command that phase runs
  during `up`'s readiness wait;
- for a container, from starmap's resolvers into phase, and from phase
  through `docker exec -i` standard input into a file in the container's
  tmpfs at `/run/secrets` (see [Container secrets](#container-secrets));
  only an image that can read nothing but environment variables receives
  it through the `docker compose up` child instead.

No value is written to a file, an argument list, a log, the run record or a
status document. When a resolver cannot supply a value, for example because
a 1Password vault is locked, `Phase::up` returns that error for the app
before starting anything, and names the variable and the resolver.

starmap can keep 1Password values in a per-user agent that holds them in
memory for a limited time after one unlock, so that starting another
workspace does not ask for 1Password again. mxv's `EnvironmentSource`
reaches the agent only through starmap's resolver, which falls back to
reading 1Password directly. starmap never caches a value resolved in
production mode, or one whose binding says `cache = false`. A keeper that
resolves a fresh environment for a restart uses the same resolver. The
resolver's API is still being decided in starmap, and phase builds against
it once it lands.

Any process running as the same user can read another process's environment
on macOS. The keeper design limits how many processes hold a secret and for
how long; it does not hide a secret from the user's own processes.

## Sandbox

phase runs every host process under the operating system's sandbox:
Seatbelt on macOS and Landlock on Linux. The profile is generated from the
app's `[permissions]` and allows:

- reading the system paths a program needs, the app's program and its
  checkout or cached release;
- writing only the app's own state directory and temporary directory;
- listening only on the app's own ports;
- connecting only to the localhost ports of the apps it depends on, and to
  external hosts through an allowlist proxy that phase runs outside the
  sandbox.

The profile never lets an app write these paths, even when they lie inside
a directory it may otherwise write:

- the workspace directory `<root>/<workspace>/` outside the app's own
  checkout, including the workspace manifest `mxv.toml` and `AGENTS.md`.
  multiverse and phase trust that manifest as written by the person: phase
  reads shared services and workspace-declared apps from it with the person
  as their publisher, and multiverse deletes its `disposable` globs without
  asking;
- multiverse's state and data directories, including its workspace records
  and the mirrors in `~/.local/share/mxv/mirrors` that clones borrow
  objects from;
- phase's own state directory, including run records and granted
  permissions, so that an app cannot grant itself more.

The profile always denies an app the socket of starmap's secret agent and
the 1Password and Keychain services. Only phase, outside the sandbox,
resolves secrets, and an app receives only the values its own manifest
declares.

phase takes the agent's directory from starmap's resolver, so it denies
exactly the path the agent uses. The agent ignores `TMPDIR` and uses the
per-user temporary directory that macOS reports through
`confstr(_CS_DARWIN_USER_TEMP_DIR)`, and `/run/user/<uid>` on Linux. On
macOS the Seatbelt profile denies reading, writing and connecting to any
socket below that directory, which is the control.

Landlock on released Linux kernels cannot stop a `connect()` to an existing
Unix socket by path; the access right that would cover it is not yet in a
released kernel. On Linux, starmap therefore leaves the agent off unless the
kernel's Landlock ABI has that right, so a sandboxed app has no agent to
reach. If the agent is wanted on older kernels later, phase would mount an
empty tmpfs over the agent's directory in the app's own mount namespace.
An eval checks on both platforms that a process under phase's generated
profile fails to connect to the agent's socket.

phase and multiverse will share one generator for Seatbelt profiles.

Tested on macOS 27 (`/evals/nono-library-for-phase` in the owner's board):

- Seatbelt restricts listening by port. It does not restrict the interface:
  an app allowed to listen on port 8080 can listen on 0.0.0.0:8080 as well
  as 127.0.0.1:8080. phase tells each app to listen on 127.0.0.1 through
  its environment, but cannot enforce it on macOS.
- Outbound access must be allowed by remote address. A broad local-address
  rule lets external traffic through.
- Seatbelt cannot nest, so a program that applies its own sandbox fails
  inside phase's.
- Symlinked system files, such as `/etc/ssl/cert.pem`, must be allowed by
  their resolved path.

Decision P9 chooses between building the sandbox on the `nono` library and
generating the profile in phase.

## Trust

phase asks you two questions, and only when the answer could change what
runs.

**Who published the program?** You trust a publisher once, such as
everything under `github.com/crumbkit`. A release must carry a signature or
a build attestation that ties it to a trusted repository's CI. A new
version from a trusted publisher needs no prompt. A program built from your
own checkout needs none, because you are its publisher.

**What may the app do?** phase stores the permissions you granted to each
app. When an app's manifest asks for more than you granted, phase stops
that app and shows the difference in plain terms:

```text
api asks for new permissions:
  read the secret STRIPE_KEY
  connect to api.stripe.com
Grant them? [y/N]
```

A manifest change that keeps permissions the same, or reduces them, needs
no prompt. Edits to code, lockfiles or build scripts need no prompt either,
because the sandbox limits what any of that code can do.

The aim is that every prompt names exactly what you are agreeing to, and
that prompts are rare enough to be read.

## Commands

```sh
mxv phase up [APP[/PROCESS]]...      # start the selection and what it depends on
mxv phase down [APP]...              # stop in reverse dependency order
mxv phase restart APP[/PROCESS]...   # resolve the environment again, then restart
mxv phase status [--json]
mxv phase logs APP/PROCESS [--follow] [--since 5m] [--json]
mxv phase sweep [--dry-run]          # stop what a crashed run left behind
mxv phase reset SERVICE              # remove a shared service's data, after asking
mxv phase plan check FILE            # validate a plan without running it
mxv phase grants [APP]               # show and revoke granted permissions
mxv phase grants approve APP         # grant an app's pending permission growth
mxv phase exec APP -- COMMAND...     # run a command with the app's environment and sandbox
```

Every command takes `--workspace DIR` and defaults to the workspace that
contains the current directory.

## Library

```rust
// A plan holds no secret value.
pub struct Plan { pub workspace: WorkspaceName, pub apps: Vec<AppPlan> }
pub struct AppPlan {
    pub name: AppName, pub runner: RunnerKind, pub source: Source,
    pub processes: Vec<ProcessPlan>, pub depends: Vec<AppName>,
    pub permissions: Permissions,
}
pub enum Source { Release { version: Version, spec: ReleaseSpec }, Checkout { dir: PathBuf, build: BuildSpec } }
pub enum RunnerKind { Host, Container }

pub trait EnvironmentSource {
    fn resolve(&self, app: &AppName, process: &ProcessName, deadline: Instant)
        -> Result<Box<dyn AppliedEnvironment>, EnvironmentError>;
}

pub struct Phase;
impl Phase {
    pub fn up(&self, plan: &Plan, select: &Selection, env: &dyn EnvironmentSource, deadline: Instant) -> Result<Report, PhaseError>;
    pub fn down(&self, workspace: &Path, select: &Selection, deadline: Instant) -> Result<Report, PhaseError>;
    pub fn status(&self, workspace: &WorkspaceId, plan: Option<&Plan>) -> Result<Status, PhaseError>;
    pub fn logs(&self, workspace: &Path, request: &LogRequest, sink: &mut dyn LineSink) -> Result<(), PhaseError>;
    pub fn sweep(&self, workspace: &Path, dry_run: bool) -> Result<Swept, PhaseError>;
}
```

`mxv` links this library. Other hosts can link it too, giving phase their
own plan and `EnvironmentSource`.

## Lifecycle rules

These rules come from the history of an earlier local runner, whose bugs
and redesigns are the evidence for each one.

### Records

- phase writes the run record before it creates anything, naming each
  planned keeper, process, container, network and volume, and updates it as
  each one starts. Stopping writes `stopping` before it acts and `stopped`
  only after it has checked that the object is gone. A crash at any point
  leaves a record that names everything that may be running.
- phase labels every container, network and volume with the workspace's
  stable id and the path of its run record. `sweep` removes what the record
  names; `sweep --all` also removes labelled objects whose workspace no
  longer exists. When Docker does not answer, `sweep` and `status` fail
  with an error; they never report that nothing is running.
- Docker networks are recorded and removed like containers. A workspace
  uses one network for all its containers, since host processes reach
  containers through published ports.
- The run record is keyed by the workspace's stable id, so moving or
  renaming the workspace keeps it.
- A run record that phase cannot read blocks only its own workspace. A
  record written by a newer phase asks you to upgrade. `sweep --unreadable
  [--dry-run]` removes the rest. A phase upgrade leaves running workspaces
  running, and the next `up` adopts them.

### Starting and stopping

- Apps that do not depend on each other start at the same time. When one
  fails, phase stops starting the others and reports which started.
- `up` on a running workspace compares each process's recorded inputs (the
  plan hash, the program's digest, and a keyed hash of its environment
  values, never the values) with the new ones. It restarts a process whose
  inputs changed and leaves the rest alone without probing them again.
- `up` has separate deadlines for building and for readiness. When a
  deadline cuts off a start or stop, phase reports the outcome as
  uncertain and does not retry it.
- macOS checks a newly written program the first time it runs, which can
  take several seconds. phase runs each new program once with `--version`
  or equivalent before starting it, or excludes that first launch from the
  probe timeout.
- Before it starts a process, phase checks that its port is free, and when
  it is not, names the process that holds it.
- A process can be a `oneshot`, such as a migration. It is ready when it
  exits 0, and the processes that depend on it wait for that. It runs on
  every `up` unless it declares `once = true`.
- Ctrl-C during a foreground `up` stops what that `up` started. Status
  shows each workspace as starting, running, partly running, stopped, or
  unknown when processes phase cannot read decide which of those it is.

### Restarts and expiring credentials

- `AppliedEnvironment` reports the earliest time any of its values
  expires. `status` shows a process whose credentials have expired.
- A restart policy states whether a crash restart resolves the environment
  again. The default resolves it again when a value has expired.
- phase recreates a container to restart it; it never uses
  `docker restart`, which keeps the environment the container was created
  with.

### Concurrency

- `up`, `down` and `restart` take the workspace's lifecycle lock. `status`
  and `logs` open records read-only, take no lock, and never hold one while
  they stream, so they also work inside an agent's sandbox.
- `exec` and the host program's tests wait for an `up` in progress to
  finish, up to a deadline, before they run.

### Logs

- A keeper gives its app a file descriptor for the log file, not a pipe, so
  the app keeps running and logging if the keeper dies.
- Keeper logs have a maximum line length and rotate at a size limit. A slow
  log reader never blocks supervision or Ctrl-C.

### Shared service credentials

- phase provisions roles through `docker exec` as the container's local
  superuser, and sets each role's password on every `up`. Losing a stored
  password therefore never makes a service's data unusable. `reset SERVICE`
  removes the data and the stored passwords together.

### Unattended runs

- When an app's permissions grew and nobody is at a terminal, `up` refuses
  that app with a stable error code and the permission difference.
  `mxv phase grants approve APP` grants it.
- `up --env KEY=VALUE` sets a value that is not a secret for one run. It
  needs no grant, and the plan records the name only.

## Releases

The first release replaces mxv's process commands (decision P7), so it must
run everything they run today. It contains:

- the library crate and `mxv phase up`, `down`, `restart`, `status`, `logs`
  and `sweep`;
- host processes from a checkout, each with a keeper, probes, restart
  policies and stop handling;
- container apps through Docker Compose, lifted from multiverse's container
  code (crumbkit/multiverse#6);
- the run record, `sweep`, and the status document;
- environment resolution through starmap's `AppliedEnvironment`;
- `mxv up` and the other old commands calling phase, so that existing
  workspaces keep working.

Later releases add, in this order (decision P8): release sources with
verification, shared services with a database and role per app, the
sandbox, and trust grants.


Each decision states what this draft assumes until it is decided.

| # | Decision | Draft assumes |
| --- | --- | --- |
| P1 | What restarts a crashed host process: a keeper per process, one supervisor per workspace, or nothing until the next restart. | Decided 4 November 2026: a keeper per process. |
| P2 | How phase runs containers: Docker Compose with one project per app, or the Docker Engine API directly. | Decided 4 November 2026: Docker Compose, one project per app, with no environment variable value in the Compose file. Each variable maps to a private `MXV_ENV_*` reference set only on that service's compose child, as in crumbkit/multiverse#6. |
| P3 | How a container receives a secret. | Decided 4 November 2026: as a file in a tmpfs at `/run/secrets`, written by `docker exec -i` on standard input and rewritten to rotate it. Environment references remain only for images that can read nothing else. See [Container secrets](#container-secrets). |
| P4 | How workspaces avoid each other on the network: allocated ports on 127.0.0.1, or one loopback address per workspace. | Decided 4 November 2026: allocated ports on 127.0.0.1 from multiverse's ledger. |
| P5 | How `mxv` uses phase: by linking the library, or by running a `phase` program and reading its JSON. | Decided 4 November 2026: `mxv` links the library; other tools read state as JSON. |
| P6 | Where the per-app choice between release and checkout lives. | Decided 4 November 2026: in multiverse's workspace record, changed with `mxv app edit` and `mxv app release`. |
| P7 | When multiverse's process commands move into phase. | Decided 4 November 2026: immediately. phase takes over `up`, `down`, `restart`, `status` and `logs` from its first release, and multiverse's own process commands are removed. |
| P8 | The order of the work. | Decided 4 November 2026: host runner and probes first, then sources, then containers and shared services, then the sandbox. |
| P9 | The sandbox mechanism: the `nono` library, or a profile generator in phase. | Open; to be decided by the owner. |
| P10 | The trust model described above. | Decided 4 November 2026: as described in [Trust](#trust). |
| P11 | Where an app's release, build and permission fields live: in `mxv.toml`, or in a separate file. | Decided 4 November 2026: in `mxv.toml`. |
| P12 | The command verbs. | Decided 4 November 2026: `up` and `down`. |
| P13 | How apps share a service such as Postgres: one copy per workspace, or one per machine. | Decided 4 November 2026: one copy per workspace, with a database and role per app. See [Shared services](#shared-services). |
| P14 | Whether phase builds container images, or runs images that the host program builds. | Decided 4 November 2026: the host program builds images. phase runs a host-supplied image with the checkout bind-mounted, cache volumes, and a run-as user. |
