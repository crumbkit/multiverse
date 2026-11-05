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
to start, and the engine waits for that line until its deadline. When the
process's first start ends, the keeper writes one more line, the end line,
with the process's exit status or the signal that ended it. The keeper
records each start, restart count and stop of the process in the
run record from a thread of its own, so waiting for the lifecycle lock
never blocks supervision. With each stop it records how the process ended,
as the run record section says. It notes its start, restarts, resolve failures,
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

`up` holds the lifecycle lock while it waits for a process to be ready, so
a keeper cannot record a process that ends during that wait until `up` has
returned. `up` therefore learns how the process ended from the keeper's end
line. When the readiness wait or the start grace sees the process gone,
`up` reads the end line for up to 2 seconds, and the failed start reports
the code or the signal. The keeper writes the line as soon as it has
reaped the process, so the wait is short, and when the line does not come
in time, the failed start reports neither. A cancel ends that wait at
once. A wait that ends first leaves its reader thread behind until the
keeper writes the line or exits. `up` records the end that the line gave
before it stops the process, and the stop keeps that end, so `status`
shows the failed process as exited with its code until `down` or the next
start. When the process becomes ready, `up` closes its end of the
keeper's standard output, and the keeper's later end line is lost, which
is harmless because the record holds the end. The keeper handles
`SIGPIPE` itself, so that write fails with `EPIPE` even when the program
that links phase leaves `SIGPIPE` at its default, and the app's program
starts with `SIGPIPE` at its default.

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

A process with no readiness probe is ready as soon as it starts. The run
record keeps when phase last observed a process ready, as the section on
readiness in the run record says.

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

An `http` or `tcp` probe of a host process connects to 127.0.0.1, and to
::1 when 127.0.0.1 refuses or fails, as mxv's port probe did. A dev server
such as Vite may listen only on ::1, and a probe of 127.0.0.1 alone never
saw it ready. The connection to ::1 gets the part of the probe's timeout
that is left, so one attempt still ends within the timeout. An `http`
probe sends its request to the address that accepted, with the `Host`
header `localhost:<port>`. The port checks before a launch and after
readiness find a listener on ::1 too: the check whether a port is in use
connects to both loopback addresses, and the lookup of the process that
holds a port reads IPv6 sockets as well as IPv4 ones.

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

phase names an app's project `phase-<hash>-<app>`, where `<hash>` is 16
hex digits of a hash of the workspace's stable id, and the container of
each process `<project>.<process>`. The workspace's volumes are named
`phase-<hash>.<volume>`, because every app of the workspace may mount them.
phase creates them before `docker compose up`, the compose file declares
them `external`, and they carry the workspace's labels and no app label.
Every container runs on Docker's default bridge network, so phase creates
no network. `docker compose up` reads the compose file from standard
input, so the file never reaches the disk.

The plan's restart policy maps to Docker's: `no` to `no`, `on-failure` to
`on-failure`, or `on-failure:N` with a maximum of `N`, and `always` to
`unless-stopped`. Docker's backoff replaces the plan's backoff, and the
plan's maximum does not apply to `always`. While phase waits for a
container to be ready, a container that Docker is restarting, or has
restarted since it was created, has ended, so a container that crashes in
a loop fails to start with its exit code.

A process or container that ends before it is ready fails with its exit
code or the signal that ended it, and the failure holds nothing that the
process wrote, because a process may print a secret value, such as a
connection string with a password. The failure's message points to the
log, which the host program's `logs` command shows, such as
`mxv logs <app>`.

The run record keeps, for each container, a hash of the process's service
in the compose file, which holds no value of a variable. `up` leaves a
container alone and reports it as already running when Docker still has
the recorded id, the container's labels name the workspace, app and
process, Docker's status is exactly `running`, and the recorded hash is
the hash of the process's service in the new plan. `up` replaces every
other container.

`up` pulls the app's image before it removes any container, within the
build deadline, and resolves each process's environment before it removes
the process's old container, so a pull or a resolver that fails leaves the
old containers running. It writes each container's entry back to planned
just before it removes or creates that container, so the entries of the
containers it has not reached keep their ids and plan hashes. `docker compose up` then pulls nothing. A pull,
`docker compose up` and the removal of an old container stop when the
caller cancels. A deadline that cuts off the wait for a container leaves
the app's start uncertain, as it does for a host process.

`down` stops an app's containers in reverse dependency order. Each
container gets its own stop grace, cut to what is left before the
deadline. phase first turns off the container's restart policy with
`docker update --restart no` and reads its status again. A running or
paused container gets its stop signal and grace, and any other
container, such as one that Docker was restarting, is removed at once. A Docker failure on one container does not
stop `down` from stopping the others. It records each container that is
gone and then reports the first failure.

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

A Docker failure carries its facts as types, so a program never reads
Docker's text. `PhaseError::Docker` holds a `DockerError` whose
`cause()` is a `DockerCause`, such as `pull_failed` or
`daemon_unavailable`. Its `operation()` names the docker command or step,
such as `pull`, `compose up`, `stop`, `rm`, `container inspect`, `logs`
or `secret write`. The container the command worked on is named in one
of two ways. `service()` gives its compose service, which is its
process's name, for a failure of `up`, `down` or a secret write.
`container_id()` gives its id for a failure of `sweep`, which knows a
container only by the id that the run record keeps. Its
`foreign()` names the kind and name of an object whose labels do not
name phase's scope. `docker_line()` gives the line that Docker printed
for its own error, such as `Error response from daemon: ... port is
already allocated`, apart from phase's sentence around it. All of these
live in a `DockerContext`, which `context()` returns. None of them holds a
value of a variable, and the error's text is unchanged.

An app whose outcome is `errored` because of a Docker failure carries the
same `cause` and `context` in the report. The report's JSON leaves both
out for any other error, and leaves out each field of `context` that is
empty:

```json
{
  "app": "web",
  "outcome": "errored",
  "code": "phase::docker::refused",
  "message": "Docker refused a request of the run: docker compose up failed for the container server: Error response from daemon: ...",
  "cause": "refused",
  "context": {
    "operation": "compose up",
    "service": "server",
    "docker_line": "Error response from daemon: ... port is already allocated"
  }
}
```

A reader that meets a cause it does not know, written by a newer phase,
reads it as `unknown`, and a reader ignores a field of `context` that it
does not know.

The Docker line can never hold what a container or an app wrote, because
a container's output can hold a credential. phase takes it only from a
docker command's standard error, and only from a line that starts with
one of Docker's own error forms:

- `Error response from daemon:`
- `error from daemon in stream:`
- `Error: No such container:`
- `Cannot connect to the Docker daemon`
- `failed to connect to the docker API at`, which Docker 29 prints where
  older clients print `Cannot connect to the Docker daemon`

A line that starts with only `Error:`, `docker:` or white space is not one
of them, because containers print such lines too. Docker 29.8 with Compose
5.5 prints a port conflict, an image that the registry does not have and
an image that Docker does not have as an `Error response from daemon:`
line at the end of `docker compose up`'s standard error. Compose's own
errors, such as `no such service:`, have no fixed form, so phase does not
take them.

Each command adds its own rule:

- `docker compose up --detach` passes on nothing that a container writes.
  Its line is the last line of standard error, after Compose's progress
  lines, when that line is in one of the forms.
- `docker logs` passes on the container's standard error. Its line is
  a line in one of the first four forms that is the last line of standard
  error, after which docker exits with code 1, as it does after it prints
  its own error. The same line becomes the failure's message, and phase
  holds it back from the log's lines.
- The `docker exec` of a secret script never gives a line, because phase
  never reads what it prints, as the section on container secrets says.
- Every other command gives a line only when every line of its standard
  error is in one of the forms.

Any other failure leaves the Docker line out.

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

A process receives each variable that its app's permissions name as a
secret this way, unless its plan sets `secret_delivery = "env"`.

The compose file passes the wrapper as the service's `entrypoint`:
`/bin/sh -c <script> phase-secrets <NAME>... --`, followed by the service's
`command`. Replacing an image's entrypoint makes Docker drop the image's
command too, so after the image is pulled phase reads the image's
entrypoint, command and user with `docker image inspect`, and the
service's command is the image's entrypoint followed by the plan's
command, or by the image's command when the plan has none. This holds for
an entrypoint in exec form and in shell form. The container therefore
runs what it would have run without the wrapper. The run record's plan
hash covers the plan's command and `secret_owner`, and not the image's
command.

The wrapper waits until `/run/secrets/<NAME>` exists for each name, then
replaces itself with the command, so the app is the container's first
process and receives its stop signal. While it waits, a stop signal ends
it. It pauses a tenth of a second between checks with `sleep`. It exits
with status 70 and one line on standard error that names the missing file
when the file has not appeared after 60 seconds, when the image has no
`sleep`, so that it never waits by spinning, and when it has no command to
run.

The files belong to one owner, in this order:

1. the process's `secret_owner`, a numeric `uid:gid` such as `999:999`;
2. otherwise the app's run-as user;
3. otherwise the image's own `User`.

phase refuses to start a process whose files would belong to root by the
second or third rule, with `phase::container::secret_owner_unknown`. Many
images start as root and switch to another user before they read their
secrets, and that user could not read files that belong to root. The
error says to set `secret_owner` to the user the app reads as, such as
`999:999`, or to use `secret_delivery = "env"`. An app that runs as root
and reads its files as root sets `secret_owner = "0:0"`.

phase writes the files in two steps, each a `docker exec --interactive
--user 0` that runs a script of the container's `/bin/sh`:

1. For each secret, phase writes the value's length in bytes on one line
   to the script's standard input, then the value, and closes it. The
   script writes the value to `/run/secrets/.<NAME>.new`, compares its
   bytes with the length using `wc -c`, gives the directory and the file
   to the owner, and makes the file readable by the owner alone (mode
   0400). A feed that was cut short, or a script that fails, removes its
   file.
2. Once every secret is staged, one script renames each staged file over
   `/run/secrets/<NAME>`, and removes the file of each optional secret
   that has lost its value.

phase never reads what a `docker exec` into a container prints into an
error's `message` or into the Docker line. A `docker exec` passes on the
standard error of the process it runs, and that process is the image's
own `/bin/sh` with a secret's value on its standard input, so an image
could print the value. The `docker exec` of each secret script writes its
standard output and standard error to `/dev/null`, and a failure's message
is phase's own sentence about its exit status:

- 65 says that the secret's length did not reach the stage script.
- 66 says that the secret arrived with fewer or more bytes than its
  length.
- 67 says that the file of a lapsed optional secret could not be removed.
- 125, 126 and 127 are the statuses of `docker exec` itself, and say that
  Docker could not run the script.
- Any other status, such as 1, may be the script's or Docker's, so phase
  inspects the container. A container that Docker no longer has, or that
  no longer runs, explains the failure. Otherwise the message says that
  the script exited with that status.

A command probe of a container sends its output to `/dev/null` too, and
phase runs no other `docker exec`.

When a staging fails, phase removes every staged file and renames
nothing, so the container keeps its whole old set of files. A reader of
one file sees its old value or its new one. A write that fails during a
start fails the start, and phase removes the container. When the
container has already exited, the error says why in phase's own words.
For status 70 it says that the wrapper gave up waiting for the secret
files, which it names from phase's own list, or that the image has no
`sleep` or no command. When the image has no `/bin/sh`, it points to
`secret_delivery = "env"`. phase never copies the wrapper's line from
the container's log into the error, because the wrapper runs under the
image's own `/bin/sh` and some secret files may already exist by then.

On the Mac, phase asks the `AppliedEnvironment` to write each value's
bytes into memory that it overwrites with zeros afterwards, and when that
memory grows. It reads every value of a process before it removes the
process's old container, so an environment that cannot write a value
leaves the old container running. The zeroing is best effort: the
environment, the operating system's pipe buffers and the `docker` program
may keep copies of their own. phase starts `docker exec` itself, in a
process group of its own. When the host program set
`Phase::with_docker_environment`, `docker exec` receives only the `PATH`,
`HOME` and `DOCKER_*` variables that it passed, and otherwise the host
program's environment, as phase's other `docker` commands do. The value
therefore reaches no shell, no environment and no argument list. When the
deadline passes or the caller cancels, phase kills the group.

An image that runs with secret files must therefore provide `/bin/sh`,
`sleep`, `cat`, `wc`, `chown`, `chmod`, `mv` and `rm`, which busybox and
the base images of the common distributions do. An image that lacks one
of them needs `secret_delivery = "env"`.

A Docker restart empties the tmpfs, and phase is not running to write
the files again. phase therefore gives a process with secret files
Docker's `no` restart policy, whatever its plan's restart policy says, and
a crash leaves its container exited until the next `up` or `restart`.
Restarting such a process after a crash needs phase. The follow-up is a
watcher per app that rewrites the files after a restart and before
`AppliedEnvironment::expires_at`, which would let such a process keep its
plan's restart policy. Until then, `up` treats a container with
secret files that Docker has restarted as not running and replaces it,
and `status` shows it as restarting. A container with secret files that
someone stopped and started again by hand, with `docker start`, also
waits for files that its restart removed, but Docker does not count that
start as a restart, so `up` and `status` read it as running. An `up`
within the wrapper's 60 seconds rewrites the files, and the wrapper then
starts the app. After that, the wrapper has exited with status 70, and
the next `up` replaces the container because it no longer runs. The
wrapper waits for every secret that had a value when the container was
created. When one of those is an optional secret that has since lost its
value, the rewrite writes no file for it, so the wrapper waits for a file
that never comes, exits with status 70 after its 60 seconds, and the
next `up` replaces the container. The run
record keeps no start time to compare with, so `status` cannot tell such
a container from one that runs.

Each `up` rewrites the secret files of every container that it leaves
running. For each such process that receives secret files, `up` first
inspects the container. A container that no longer runs, or that has
secret files and that Docker has restarted, is not running as planned,
and `up` replaces it without resolving anything. Otherwise `up` resolves
the process's environment again and writes the files in the two steps
above, and the container keeps running. The files keep the owner that
the container was created with: the process's `secret_owner`, or else
the user that the container runs as, which Docker fixed when it created
the container. A tag that has since moved to an image with another user
changes nothing. `up` resolves no environment for a process without
secret files. Each rewrite that writes adds a note to the app's report,
such as `rewrote 2 secret files of db/server`.

A reader of one file sees its old value or its new one. The set of
files is not swapped as one unit: the commit script renames the staged
files one after another, so a reader that reads two files while the
commit runs can see the new value of one and the old value of the other.

A rewrite that fails while the container still runs does not fail the
app. When the environment cannot be resolved, a value cannot be read, or
Docker refuses the inspection or the write, the container keeps its
earlier files and keeps running. A failure of Docker itself, a missing
`docker` program, a daemon that does not answer or a command that
missed its deadline, fails the app instead, because phase cannot tell
what the container holds. `up` reports the app as already running or started, with
a note such as `db/server still runs with its earlier secret files:
vault could not supply the variable TOKEN`, which names no value. `up`
goes on to the app's other containers and processes, and the apps that
depend on it start as usual. The next `up` tries the rewrite again. A
rewrite that fails before the commit leaves every earlier file whole. A
removal of a lapsed optional secret's file that fails after the renames
ends the commit script with status 67 and a line that names the secret,
so the note says which file remains. An app fails only when a container
that must be replaced cannot be, as with any start.

A short-lived value that a resolver renews therefore reaches a running
container at the next `up`, and nothing rewrites the files between two
runs of `up` yet. Decision P15 settles the watcher that will: a
detached `phase watch` process per app that re-resolves the environment
through the host program's `EnvironmentSource` before
`AppliedEnvironment::expires_at` and after a Docker restart, and holds
no value between rewrites. It waits for two things: a resolver that
works in a process that outlives `up`, which starmap's resolver will
provide, and a later version of the run record, which records the
watcher so that `down`
and `sweep` stop it.

An optional secret, one that the process names in `optional_env`, gets
no file when the environment has no value for it, and the wrapper's names
leave it out, so the wrapper does not wait for it. phase refuses nothing
for it: the app reads a file that is absent as a secret it was not given,
which is what marking it optional asks for, and refusing would make the
mark useless for a secret. When every secret file of a process is
optional and none has a value, the process starts as one without secret
files: its service has no tmpfs, no wrapper and no secrets label, it
keeps its plan's restart policy, and phase does not ask who owns the
files. The plan hash comes from the service as the plan describes it, so
it does not depend on which values are there.

A rewrite writes the optional secrets that then have a value, and
removes the file of one that has lost its value in the same script that
renames the staged files, so the container never keeps an old value of a
secret that the source no longer supplies. Only a rewrite removes a
file, because a start makes a new container. A container without the
secrets label cannot receive a file, because it has no tmpfs and no
wrapper. When a rewrite finds that such a container's fresh environment
has a value for one of its secrets, `up` replaces the container, and the
new container starts with the tmpfs, the wrapper and the files. While
none of its secrets has a value, `up` leaves it running.

An image that can read a secret only from an environment variable falls
back to environment references when its process sets
`secret_delivery = "env"`. The Compose file maps the variable to a
private reference, such as `POSTGRES_PASSWORD: "${MXV_ENV_postgres_1:-}"`,
and phase sets the `MXV_ENV_*` values only on that service's
`docker compose up` child process. phase never lists a variable by its
bare name, because Compose would then read it from the child's own
environment, and a container variable named `PATH` or `DOCKER_HOST` would
change which docker program or daemon the child uses. With this fallback,
`docker inspect` and every process in the container can read the value, and
a new value means recreating the container. Compose also hashes each
service's interpolated values into the `com.docker.compose.config-hash`
label of its container, so that label changes with a value. The hash is
unsalted, so anyone who can read the container's labels can test a guess
of a low-entropy value against it. The fallback is opt-in for that reason,
and a host process that sets it is refused by the plan.

The eval `/evals/container-secret-delivery-orbstack` in the owner's board
measured each option on OrbStack 2.2.5 with test values. Compose secrets
copy the value onto the virtual machine's disk, readable by every user in
the container. A file bind-mounted from the Mac breaks when it is replaced.
The tmpfs file was found nowhere else, and rewriting it took under 100 ms.

### Run record and sweep

phase writes a run record (`phase/run/v5`) for each workspace. It names every
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

A host process's entry also records how the process last ended, as
`ended`: its `exit`, which is `{"code": 3}` or `{"signal": 9}` and is left
out when nobody could read it, and `requested`, which says whether a stop
request caused the end. The keeper records it when its app ends, unless
another operation has already recorded the entry stopped. `down`,
`restart` and `sweep` record the end as requested, with no exit, when they
begin to stop the process, because the keeper that reads the exit drops
its notes while they hold the lock. They keep an end that no stop request
caused when the entry is already `stopping` or `stopped`, because that
process had ended by itself first, as after a failed start. `sweep`
records a process that the record names as running and that it finds gone
as ended with no exit and not requested, because nothing recorded how it
ended. A keeper's restart keeps the end, so the record shows the last
crash of a process that restarts, and phase's own start of the process
clears it. The entry also records the restart policy that phase started
the process with, as `restart`, so `status` can tell whether the keeper
restarts a process that ended.

`up` keeps the record when nothing in it runs but a process's entry
records an end that no stop request caused, so `status` can show it.
A `down` or a sweep that leaves everything stopped removes the record, and
with it those ends, unless the `down` did not select a process that holds
such an end. mxv's `down` of the apps that a failed `up` started is such a
`down`, so `status` still shows the process that failed.

The record is `phase/run/v5`. Version 2 added `ended` and `restart` to
host entries, version 3 added `ready` to host process and container
entries, as the next section says, version 4 added `sandbox` to host
process entries, which records how phase launched the process, as [The
sandbox as built](#the-sandbox-as-built) says, and version 5 adds
`oneshot` and `completed` to host process entries. `oneshot` records what
phase launched a oneshot from: the hash of its part of the plan and, for
a `once = false` oneshot, the run of each process and container that it
depends on. `completed` records the oneshot's completion. [Oneshot
processes](#oneshot-processes) says how phase uses both. phase reads a
`phase/run/v1`, `phase/run/v2`, `phase/run/v3` or `phase/run/v4` record
with the fields it lacks at their defaults, and writes it back as v5: a
v1 record reads with no ends, no restart policies, no readiness, no
sandbox and no oneshot launches or completions, a v2 record reads with
no readiness, no sandbox and no oneshot launches or completions, a v3
record reads with no sandbox and no oneshot launches or completions, and
a v4 record reads with no oneshot launches or completions. A
phase that reads only an older version refuses a newer record as written
by a newer phase and asks for an upgrade. Each version only adds fields
that read as their defaults when they are missing, so the next version,
v6, which records P15's watcher, adds a schema name and needs
no other read path.

### Readiness in the run record

The run record keeps when phase last observed each host process and each
container ready, as `ready`, so `status` can tell a process that has
become ready from one that only runs. `ready` holds the time in
milliseconds since the Unix epoch, the number of restarts the process
had then, and for a container the start that Docker reported, exactly as
`docker container inspect` printed `State.StartedAt`. It holds no
environment value.

- `up` records it when a process passes its readiness probe, after the
  port check that follows. For a container, the restarts and the start
  are the ones Docker reported when phase inspected the started
  container.
- A process with no readiness probe counts as ready once it runs for its
  start grace. `up` records it once the process has outlasted its start
  grace. The keeper's specification carries the grace of such a process,
  and the keeper records a process it restarts as ready once the process
  has outlasted the grace again.
- phase's own start of a process or a container clears it, and so does a
  keeper's restart. A keeper does not run the readiness probe, so a
  process that has one counts as not ready after a restart until phase
  starts it again. A later change may let the keeper probe a restarted
  process.
- `up` records the first start of a host process itself while it holds
  the lifecycle lock, and may record readiness after it. The keeper skips
  its own note of that start, which it would otherwise write once `up`
  releases the lock, so the note does not clear the readiness.
- `status` counts the readiness of a host process only while the
  keeper's count of restarts matches the one it recorded. It counts the
  readiness of a container only while Docker's `State.StartedAt` is
  exactly the start that the readiness records. Every start changes it:
  a restart under Docker's restart policy, `docker restart`, a `docker
  start` after a stop, and a restart of the Docker daemon. Comparing the
  two texts uses no clock, so a Docker clock that differs from the host's,
  as a virtual machine's can after the host sleeps, changes nothing. A
  start time that Docker does not print counts as not ready.

A keeper that an older phase started reads the record before each write.
Once a newer phase rewrites the record in a newer version, that keeper
refuses the record, notes each refused write in its diagnostic log, and
records nothing more, until phase starts its process again.

### Status and logs

`phase status --json` prints a `phase/status/v1` document with one row per
process: its app, runner, source, state, pid or container, address, start
time, restart count, last exit and log path. `phase logs` prints a
process's log, and
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
`removing`, and never as stopped. A host process whose entry records
that it ended by itself, and that its keeper did not restart, shows as
`exited` with its code or signal. One that a stop request ended shows as
`stopped`. A oneshot whose entry records its completion shows as
`completed`. A process that the record names as running, while neither it
nor its keeper is there, shows as `exited` with the reason `unrecorded`,
because its keeper ended before it recorded the end. An entry that reads
`stopping` with no end shows the same way. A stop that the keeper could
not record also shows as `unrecorded`: a `TERM` from outside phase while
another operation holds the lifecycle lock makes the keeper stop its app
and drop its notes, and phase does not retry them. An entry that reads
`stopping` with an end that no stop request caused shows as `exited` with
that end, as after something killed the keeper while it stopped what a
crash left. A process whose
keeper runs and whose app has gone shows as `restarting` only when its
recorded restart policy restarts it: before the keeper records the end,
only a policy that restarts however the process ended counts, and
afterwards the policy decides with the recorded exit and the restarts so
far. Otherwise it shows as `stopping` while the keeper stops what is
left, and as `exited` once the keeper has recorded it. A host process
that its keeper restarted shows its last crash as `last_exit`, with its
code or signal, while it runs or waits to restart. A `down` while the
keeper waits out a backoff replaces that crash with a requested end. A running process or container shows as
`ready` when the record holds readiness that phase observed since its
last start or restart, and as `running` otherwise: before its readiness
probe has passed, after a restart that the probe has not seen, and in a
record that an older phase wrote. A process with no readiness probe shows
as `ready` once it has outlasted its start grace since its last start or
restart. A container that the record plans and
Docker has not started shows as `starting`. A container that Docker is
restarting under its restart policy shows as `restarting`, and a paused
container shows as `paused`, which counts as partly running. The workspace's state comes
from the rows that are not unknown. It is `unknown` when the unknown rows
decide it: every other row is running, or none of the others runs. With the workspace's plan, the rows
follow the plan and carry its sources and addresses, and record entries
that the plan lacks still show. Without a plan, as when the manifest is
invalid, the rows are the record's live entries and leave the plan's
fields empty.

## Environment and secrets

phase resolves an app's environment through starmap. starmap's resolution
produces an `AppliedEnvironment`, a type that can list variable names,
apply values to a `Command`, and write one named value's bytes to a
writer for a container's secret file, but cannot return a value. It has
no `Debug`, `Display` or `Serialize` implementation. Its `write_value`
returns an `Unsupported` error by default, so an implementation that
predates it compiles, and phase reports a container secret file that it
then cannot write.

The plan names each process's variables and holds no value, literal or
secret. A process may mark some of its variables optional in
`optional_env`, a subset of `env`, such as `LANG` or `CARGO_HOME`, which
the caller's shell may or may not set. The plan therefore names the same
variables whatever the shell sets, so its hash, and each process's plan
hash, does not change with the shell. The hash covers the names and the
optional flag, and no value. When the source has no value for an
optional variable, it leaves the variable out of
`AppliedEnvironment::names`. phase then starts a host process with the
variable unset, and leaves the variable out of the service in the compose
file that a container process's `docker compose up` child reads, so the
container does not receive it, and does not receive an empty string in
its place. A required variable without a value still makes the source
fail, and `up` reports that error as before. phase also checks each
resolved environment against the process's required variables, those in
`env` and not in `optional_env`: before a host process's keeper starts,
when a keeper resolves a fresh environment for a restart, and before a
container starts. An environment whose names lack a required variable
fails with the same environment error, which names the variable and no
value, so a source that leaves one out fails the start, and the process
never runs with the variable unset or empty. A keeper counts such a fresh
environment as one it could not resolve. The host program's `EnvironmentSource` supplies every value, and
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
- for a container, from starmap's resolvers through
  `AppliedEnvironment::write_value` into phase's memory, which phase
  overwrites with zeros afterwards as best it can, and from there through
  the standard input of a `docker exec` that phase starts into a file in
  the container's tmpfs at `/run/secrets` (see
  [Container secrets](#container-secrets)); only a process that sets
  `secret_delivery = "env"` receives it through the `docker compose up`
  child instead.

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
On macOS a test of phase, recorded in `/evals/phase-sandbox-first-stage`
in the owner's board, checks that a process under phase's sandbox fails to
connect to a socket in the per-user temporary directory, which a process
outside the sandbox reaches. The sandbox denies every Unix socket on macOS,
since its only network rules are TCP ports. Linux has no such test yet,
because the directory `/run/user/<uid>` may not exist on a test runner.

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

### The sandbox as built

Decision P9 builds the sandbox on the `nono` library, pinned to an exact
version, in two stages. The first stage, built now, applies Seatbelt on
macOS and Landlock on Linux. The second adds `nono-proxy` for the
allowlist of external hosts.

Only a host app that declares `[permissions]` runs under the sandbox
(decision P17). That includes an app that declares `[permissions]` only to
mark some of its variables as secrets: it now runs sandboxed, so it needs
the `listen` and `write` entries its processes use. A plan writes an app
without permissions as `"permissions": null`, so the plan's hash, which
each process's run record entry keeps, records that the app runs
unsandboxed. An app that declares permissions also runs unsandboxed on a
platform where phase has no sandbox, and on a Linux kernel without Landlock
or whose Landlock ABI is older than 4, which is the first that restricts
TCP ports.

The run record entry of each host process records how phase launched it:
`seatbelt` with what it leaves open, or `unsandboxed` with the reason. On
Linux a process under Landlock is recorded as `unsandboxed`, as described
below. `status` shows that recorded value, and for a process
that phase has not launched it shows how phase would launch it now.

The keeper applies the sandbox in the child before the app runs, so each
restart is sandboxed too. Its specification names the sandbox: the paths
and ports below, and never an environment value. The keeper starts the
program that links phase as the launcher, `mxv phase keep --sandboxed
<sandbox> -- <command>`. The launcher checks the sandbox again, applies it
to itself and replaces itself with the app, which keeps the launcher's pid,
environment and log. A launcher that cannot check or apply the sandbox, or
start the program, writes why to the app's log and exits with status 78.

The sandbox allows (decision P18):

- reading the system's directories, the toolchain directories that `PATH`
  names and the app's checkout. On macOS the system directories include
  only `/Library/Developer`, `/Library/Frameworks` and `/Library/Apple` of
  `/Library`. For Cargo the toolchain directories are its `bin`,
  `registry` and `git` directories and rustup's home, never Cargo's
  credentials. For mise they are its data directory. For Homebrew they are
  the prefix's `bin`, `sbin`, `Cellar`, `opt`, `lib`, `libexec`, `share`,
  `include` and `Frameworks`, never its `var` or `etc`, with one exception
  inside `etc`. A sandboxed process may read the prefix's `etc/openssl`
  and each `etc/openssl@<version>`, which hold OpenSSL's configuration and
  the CA bundle that Homebrew copies from the system keychain. Neither is
  secret, and every TLS client that links Homebrew's OpenSSL opens them
  when it starts: Homebrew's node exits with "OpenSSL configuration error"
  when it cannot read `openssl.cnf`. Each OpenSSL directory also holds
  `private`, OpenSSL's conventional directory for private keys, which a
  user may fill, so no sandboxed process may read it. phase therefore
  grants only the directory's `openssl.cnf`, `cert.pem` and `certs`, on
  macOS and Linux alike, and the macOS profile also denies each `private`.
  Homebrew's `cert.pem` is a link to the bundle in `etc/ca-certificates`.
  phase grants the file the link names, and the macOS profile lets the
  app read the metadata of the link itself, as it does for `/var`, so the
  app can follow it. The rest of `etc` stays unreadable. A `PATH` entry that
  would reach a protected path, such as the home directory, adds nothing.
  On macOS the system directories also include the developer directory
  that `/var/select/developer_dir` names, which `xcode-select --switch`
  sets and which `xcode-select`, `xcrun` and the developer tools' shims in
  `/usr/bin`, such as `cc`, read. When that directory is the
  `Contents/Developer` of an app bundle, such as
  `/Applications/Xcode_26.6.app` on GitHub's runner, phase grants the whole
  bundle, because `xcodebuild` reads the bundle's `Info.plist` and loads
  its `SharedFrameworks`. It also grants the one file
  `/Library/Preferences/com.apple.dt.Xcode.plist`, which records whether
  the Xcode license was accepted. phase grants a bundle only when it lies
  inside `/Applications` or `~/Applications`, and grants any other
  developer directory only when it lies inside `/Library/Developer`, such
  as the Command Line Tools. A relative or dangling link, or a link to any
  other place such as `/` or `/Users/Shared`, grants nothing. Seatbelt
  checks the metadata of every name of a path as the kernel resolves it, so
  reading
  the link through `/var` also needs the metadata of the `/var` link
  itself, and the profile allows that one literal path. nono allows a
  link's metadata only when a granted path or a `PATH` entry is spelt
  through it. Without the rule, `xcode-select` failed with "unable to read
  data link" on a machine whose `PATH` names no directory under `/var`,
  such as GitHub's macOS runner. The profile allows again each grant that
  lies inside a denied path, such as an OpenSSL file inside the denied
  `/usr/local/etc`, and Seatbelt applies the last rule that matches, so
  the profile writes each deny that lies inside such a grant after the
  grant's allow;
- writing the app's state directory `<phase state>/apps/<workspace
  id>/<app>/state`, which `$STATE` names, its temporary directory `.../tmp`,
  which becomes the app's `TMPDIR`, and the paths of `write`. phase sets
  both directories to mode 0700 at each launch, and refuses either when a
  name of its path is a symbolic link. Each entry of `write` is `$STATE`
  or a path inside the checkout, relative or absolute without `..`, and it
  must still resolve inside the checkout once its symbolic links are
  resolved. phase cannot yet compare the writes of apps in different
  workspaces, which would need it to read every workspace's run record, so
  the first stage refuses an absolute write path outside the checkout.
  That limit is lifted once phase can compare writes across run records.
  No entry may lie inside another, and no app of a plan may write a path
  strictly inside a path that another app may write once their symbolic
  links are resolved, so one app cannot plant a symbolic link in another's
  directory. Two apps may write the same path, such as two apps of one
  checkout that both write `target`: neither may write the directory that
  holds it, so neither can replace it with a link, and a link planted
  inside it resolves outside the other app's grant. The checkout and an
  absolute entry are compared with their symbolic links resolved, so an
  entry spelled through the checkout's real path passes when the checkout
  is named through a link. phase refuses a write
  path inside a sticky, group-writable or world-writable directory, such
  as `/tmp`. The launcher creates a missing write directory with mode 0700
  before it checks the spec, one name at a time without following a
  symbolic link, so nothing is created through a link. The
  directories carry the workspace's id because app names are unique only
  within a workspace;
- listening on the loopback ports that `listen` names, and connecting over
  loopback to the ports of the apps it depends on (decision P16).

The sandbox always denies these paths, which a sandboxed app may neither
read nor write:

- in the home directory: `~/.ssh`, `~/.aws`, `~/.config/gh`, `~/.docker`,
  `~/.netrc`, `~/.git-credentials`, `~/.gitconfig`, `~/.config/git`,
  `~/.gnupg`, Cargo's credentials and `~/.cargo/config.toml`, `~/.npmrc`,
  `~/.pypirc`, `~/.kube`, `~/.config/gcloud`, `~/.azure`, `~/.config/mise`,
  `~/.claude`, `~/.codex`, the shells' startup files and `~/.config/fish`,
  and on macOS `~/Library/Keychains` and `~/Library/LaunchAgents`;
- `/Library/Keychains` on macOS;
- the per-user temporary directory that holds the agent's socket: all of
  `/private/var/folders` on macOS, and `/run/user/<uid>` on Linux;
- phase's state directory, apart from the app's own `state` and `tmp`
  directories. Their parent stays denied, so the app cannot replace either
  with a symbolic link;
- the paths that the host program protects, apart from the app's own
  `state` and `tmp` directories when they lie inside one. mxv protects its state directory,
  which holds the workspace records and phase's state, and its mirrors;
- the workspace's own directory outside the app's checkout, which holds
  every other checkout, and the workspace's manifest and `AGENTS.md`, which
  mxv names. phase refuses to sandbox an app whose checkout is the
  workspace's own directory, because its checkout would hold all of them.

The sandbox lets an app read, but never write, the launcher program, each
link of its chain of symbolic links and each one's directory, the
directories of `PATH`, the toolchain directories above and the checkout's
`.git`. A `PATH` directory that reaches a protected path, such as the
home directory, lets the app look nothing up in it. A sandboxed app therefore cannot replace a
program that a later process runs, and git cannot commit or fetch in a
sandboxed checkout. git also cannot read objects that a clone borrows from
mxv's mirrors, which are protected.

phase refuses to start a sandboxed app whose read or write paths cover or
lie inside a protected path, or whose write paths cover or lie inside a
read-only path. That check is the only control that Landlock can express.
The launcher repeats it on the paths as they resolve just before it
applies the sandbox, and refuses to start the app when a path resolves
somewhere else than when phase built the sandbox, such as a write
directory that the app replaced with a symbolic link. On Linux the
sandbox reads `/usr` by its entries and `/usr/local` by its entries apart
from `etc` and `var`, because Landlock cannot deny a path inside a granted
one. On macOS the profile also denies the protected paths and Homebrew's
`/usr/local/var` and `/usr/local/etc` explicitly, denies writes to the read-only paths, and
then allows again the checkout and the app's directories that lie inside a
denied path.

On macOS the profile also denies the Mach services of the pasteboard and
LaunchServices, sending Apple events and asking LaunchServices to open an
app. A macOS test checks that a sandboxed process cannot open an app, send
an Apple event, submit a launchd job, read the pasteboard or read its
keeper's environment with `ps -E`. Other macOS tests check that `cargo
build` of a crate whose build script runs `cc` through xcrun, `xcrun
--show-sdk-path`, `python3` and `node` still run under the sandbox where
they exist. xcrun warns that it cannot write its cache in the per-user
temporary directory, and succeeds. The sandboxed cargo uses the user's own `~/.cargo`,
which the sandbox lets it read and never write, so an app cannot poison
the registry cache that unsandboxed builds share. cargo builds when the
lock file and the crates it needs are already in that cache: a manual run
with a registry dependency built, with or without `--offline`. A
sandboxed app cannot download a crate, because its network is loopback
only, and it cannot write the cache. An app that must fetch crates needs
the proxy of the second stage and its own `CARGO_HOME` under `$STATE`.

The plan refuses a `connect` that names an external host, with an error
that says the proxy is not built yet, a `connect` that names an app the
app does not depend on, and a process that listens on a port its `listen`
omits. A process may not reach the host's DNS resolver on macOS, so it
reaches its dependencies by `127.0.0.1` and cannot resolve `localhost`.

On macOS phase writes its own Seatbelt rules for the ports, because
nono's port rules allow binding any port, as
`/evals/nono-library-for-phase` found. Seatbelt applies the last rule
that matches, so phase's rules work only because nono writes every
platform rule that starts with `(allow network` after its `(deny
network*)`. nono renders the profile in a private function, so no test
can read the profile that nono writes. A unit test, which runs on the
Linux pull-request lane too, checks that each of phase's network rules
starts with that prefix. A macOS test runs a sandboxed process that may
listen on one loopback port and connect to one, and checks that it is
refused another port for each and its dependency's port on another
host. A nono release that changed the order
would fail the macOS test, and the exact pin means only a pull request
that moves the pin can bring one in.

On macOS the sandbox is partial, and `status` shows it as `seatbelt
(partial: its listening ports accept connections from other machines on
the network)`.
Seatbelt restricts a bind by port and never by address, and its
`localhost` matches every address of the machine. An inbound rule that
also names the remote side refuses the `listen` itself, because `listen`
is checked with no remote address. A process that may listen on a port can
therefore listen on it on every address, and a macOS test shows that a
peer on the machine's `en0` address reaches it. A peer on another machine
was not tested. A connection out to another host stays refused.
Seatbelt's `localhost` covers ::1 as well as 127.0.0.1, so an app may
listen on its own port on ::1 and connect to a dependency that listens
only on ::1. The macOS test checks both on a machine with an IPv6
loopback address. On Linux, Landlock restricts TCP ports with no address,
so the same holds there.

On Linux `status` shows a process under Landlock as `unsandboxed
(Landlock cannot restrict Unix sockets (nono does not yet enforce
ResolveUnix), so a process could leave the sandbox through Docker or
D-Bus; phase still applies Landlock to files and TCP ports)`. Luke chose
this report on 5 November 2026 (P19). Landlock before its ABI 9 cannot restrict `connect` to a Unix
socket, and nono 0.81 does not enforce one, so a sandboxed app can reach
any Unix socket it can name, such as Docker's socket, the user's D-Bus
bus, which `systemd-run --user` uses to start a process outside the
sandbox, and the ssh and gpg agents. Landlock does not restrict UDP, and
it restricts a TCP port on every address, so an app that may connect to a
dependency's port may also connect to that port on another host. phase
still applies Landlock, because it confines the app's files and TCP
ports, as a speed bump. A Linux sandbox that `status` reports as one
waits for nono to enforce Unix-socket restrictions, or for a seccomp
filter in a later pull request. A Linux test records that a process under
Landlock can still connect to a Unix socket, so it fails once nono
restricts them.

When a sandboxed process ends before it is ready, `up` adds a note that
says the sandbox may have denied an access the process needed, and where
the denials show: as "Operation not permitted" in its log, and on macOS in
`log show --predicate 'sender == "Sandbox"'`.

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
- A process can be a `oneshot`, such as a migration, or a provisioning
  step that creates tables after a local database is ready. It is ready
  when it exits 0, and the processes that depend on it wait for that. See
  [Oneshot processes](#oneshot-processes).
- Ctrl-C during a foreground `up` stops what that `up` started. Status
  shows each workspace as starting, running, partly running, stopped, or
  unknown when processes phase cannot read decide which of those it is.

### Oneshot processes

Built 5 November 2026 for host apps. A container app with a oneshot is
still refused with `NotRunnable::Oneshot`.

- A oneshot runs under a keeper like any other host process. Its app's
  `[permissions]` sandbox it, it receives the variables that its `env`
  names, required and optional alike, and its output goes to its log
  file.
- The plan refuses a oneshot with a readiness probe, with a port, or with
  a restart policy other than `no`. phase never restarts a oneshot: a
  oneshot that fails fails its app, and the next `up` or `restart` runs it
  again.
- `up` starts a oneshot in the dependency order that it uses for every
  process, and waits until the oneshot exits, within the oneshot's
  `ready_timeout` and the readiness deadline of `up`. A cancel stops it
  as it stops any process that `up` started. A oneshot that does not exit
  within its `ready_timeout` fails as not ready, and one that the deadline
  of `up` cuts off is uncertain.
- When phase launches a oneshot, it records in the oneshot's run record
  entry, as `oneshot`, the hash of the oneshot's own part of the plan.
  The hash covers the app's runner, source, ports, permissions and
  override names, and the oneshot's own plan, such as its command,
  variable names and working directory. It holds no environment value,
  so a changed value alone does not count as a change. For a
  `once = false` oneshot, phase also records the run of each process and
  container that the oneshot depends on and that runs at that moment:
  - The oneshot depends on every process and container of each app that
    its app depends on, directly or through another app, and on each
    long-running process of its own app that phase starts before it.
  - A host process's run is the pid and start time of its keeper and of
    the process that the keeper runs. phase's own start of the process
    starts a new keeper, and the keeper's restart of a process that ended
    starts a new process, so either changes the run. While the keeper
    runs and its process does not run as the record says, such as just
    after a restart that the keeper has not recorded yet, phase cannot
    tell.
  - A container's run is its id and the `State.StartedAt` that Docker
    reports for it. Docker changes `StartedAt` on every start, including
    one under its restart policy that phase did not ask for.
- An exit with status 0 completes the oneshot, and whoever records the
  exit records the completion in the entry as `completed`, the hash from
  `oneshot`. When `up` watched the exit, `up` records it, stops the
  keeper, and then starts the processes and apps that depend on it. When
  the deadline of `up` or a cancel ended the wait first, the keeper
  records the exit once it takes the lifecycle lock, and the oneshot
  counts as completed from then on.
- Any other end fails the oneshot's app with the exit code or signal, as
  for a process that ends before it is ready. The failure holds nothing
  that the oneshot wrote, and its message points to the log. `up` starts
  no app that depends on it.
- A later `up` leaves a completed oneshot alone when its recorded hash
  matches its part of the new plan and either of these holds:
  - the oneshot declares `once = true`;
  - each process and container that the oneshot depends on runs now as
    the same run that phase recorded when it launched the oneshot, and
    none runs now that did not run then. phase reads each keeper and its
    process from the run record and checks that they still run, and asks
    Docker for each container's `StartedAt`.

  Otherwise `up` runs it again. A `once = false` oneshot therefore runs
  again after anything started again what it depends on, whichever
  operation did it: a `down db` followed by `up db`, a `restart db`, or
  Docker's own restart of the `db` container. It also runs again when
  phase cannot tell, such as when Docker does not answer. An `up` that
  finds everything it depends on running as before leaves it alone, even
  when a process that phase starts after the oneshot was started again.
- `restart` runs each selected oneshot that has completed again, before
  the processes of its app that depend on it, whatever `once` says. It
  does not run a oneshot that failed, which the next `up` runs again.
- `down` forgets the completion of each oneshot that it selects, so the
  next `up` runs it again. A `down` or sweep that leaves everything
  stopped removes the run record, and with it every completion.
- A oneshot that the run record names as still running, which an
  operation that ended before it saw the exit left, is reported as left,
  and `up` starts nothing that depends on it.
- `status` shows a oneshot that completed as `completed`, which counts as
  running for the workspace's state, and one that failed as `exited`
  with its code or signal.

### Restarts and expiring credentials

- `AppliedEnvironment` reports the earliest time any of its values
  expires. `status` shows a process whose credentials have expired.
- A restart policy states whether a crash restart resolves the environment
  again. The default resolves it again when a value has expired.
- phase recreates a container to restart it; it never uses
  `docker restart`, which keeps the environment the container was created
  with. A crash restart of a container is Docker's, under the restart
  policy, so it keeps the environment the container was created with, and
  only `phase restart` or a changed plan resolves it again. The secret
  files of a running container are the exception: each `up` resolves the
  process's environment again and rewrites them in place, and a rewrite
  that fails leaves the container running with a note.

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
sandbox, and trust grants. The sandbox's first stage is built ahead of
that order, and runs only an app from its checkout.


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
| P9 | The sandbox mechanism: the `nono` library, or a profile generator in phase. | Decided 5 November 2026: the `nono` library for both platforms, staged. The first stage applies Seatbelt on macOS and Landlock on Linux through the keeper, in the child before the app runs, so restarts are sandboxed too. The second stage adds `nono-proxy` for the allowlist of external hosts. See [The sandbox as built](#the-sandbox-as-built). |
| P10 | The trust model described above. | Decided 4 November 2026: as described in [Trust](#trust). |
| P11 | Where an app's release, build and permission fields live: in `mxv.toml`, or in a separate file. | Decided 4 November 2026: in `mxv.toml`. |
| P12 | The command verbs. | Decided 4 November 2026: `up` and `down`. |
| P13 | How apps share a service such as Postgres: one copy per workspace, or one per machine. | Decided 4 November 2026: one copy per workspace, with a database and role per app. See [Shared services](#shared-services). |
| P14 | Whether phase builds container images, or runs images that the host program builds. | Decided 4 November 2026: the host program builds images. phase runs a host-supplied image with the checkout bind-mounted, cache volumes, and a run-as user. |
| P15 | How the watcher that keeps a container's secret files current between runs of `up` gets fresh values: a detached `phase watch` process per app that resolves them through the host program's `EnvironmentSource`, or values that `up` hands to a long-lived process. | Decided 5 November 2026: a detached `phase watch` process per app that re-resolves through the environment source itself, so no long-lived process holds values. It waits for a resolver that works in a detached process, which starmap's will provide, and for a later version of the run record (v6, since v4 records the sandbox and v5 records oneshot completions), which records the watcher. Until then, each `up` rewrites the files of the containers it leaves running, and a process with secret files keeps Docker's `no` restart policy. |
| P16 | What network a sandboxed app has until the allowlist proxy exists. | Decided 5 November 2026: loopback only. An app may listen on its own ports and connect over loopback only to the ports of the apps it depends on. A plan whose `connect` names an external host is refused with an error that says the proxy is not built yet. phase writes its own Seatbelt port rules in place of nono's, which allow binding any port. |
| P17 | Which apps run under the sandbox. | Decided 5 November 2026: opt-in. Only an app that declares `[permissions]` is sandboxed, and the plan's hash records an app without them as unsandboxed. `status` shows each host process as sandboxed or unsandboxed, with the reason. A platform without a sandbox is sandbox-less rather than unsupported: an app with permissions runs there unsandboxed, and `status` shows it. On Linux, a kernel without Landlock, or with a Landlock ABI older than 4, falls back in the same way. Landlock leaves Unix sockets and UDP unrestricted, so on Linux `status` reports a process under Landlock as unsandboxed (P19), and a Linux sandbox waits for nono to restrict Unix sockets or for a seccomp filter. Seatbelt lets an app's listening ports accept connections from other machines on the network, so on macOS `status` shows the sandbox as partial. Each process's run record entry records how it was launched, and `status` shows that. |
| P18 | What a sandboxed app may read and write. | Decided 5 November 2026: an allowlist. It may read the system's directories, the toolchain directories on `PATH` (Cargo, rustup, mise and Homebrew) and its checkout. It may write `<phase state>/apps/<workspace id>/<app>/state` and `.../tmp`, which becomes its `TMPDIR` outside the per-user temporary directory, and what its permissions add. It is always denied the agent's directory and socket, `~/.ssh` and the other credential directories in the home directory, the Keychains, mxv's state and mirrors, phase's state, the workspace manifest and the workspace outside its checkout. The security review of the first stage added the read-only paths (the launcher, `PATH`, the toolchains and the checkout's `.git`), the narrower `/Library` and Homebrew grants, the refusal of an app that runs at the workspace's top, and the launcher's second check. |
| P19 | How `status` reports Linux, where Landlock cannot restrict Unix sockets. | Decided by Luke on 5 November 2026: as `unsandboxed`, with the reason that Landlock cannot restrict Unix sockets (nono does not yet enforce `ResolveUnix`), so a process could leave the sandbox through Docker or D-Bus. phase still applies Landlock as a speed bump, because it confines files and TCP ports. macOS keeps reporting Seatbelt as partial. |
