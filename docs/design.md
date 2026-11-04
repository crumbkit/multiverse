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
  exists.

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
5. phase writes the run record and returns a report.

## Engine

### Keeper per process

Each host process has a small keeper, a hidden `phase keep` subcommand of
the same program. The keeper is the parent of exactly one app process. It
holds only that process's environment, applies the sandbox, starts the
process in its own process group, writes its log a line at a time, restarts
it under its policy, and exits when the process is stopped.

No long-lived process holds the secrets of every app. If a keeper is killed,
its app process keeps running, and `phase sweep` or the next `phase up`
finds it from the run record.

### Probes and restarts

phase uses the probe and restart vocabulary of process-compose:

- probes: `http` (any 2xx passes), `tcp` and `command`, each with a period,
  a timeout, and success and failure thresholds;
- restart policies: `no`, `on-failure` and `always`, with exponential
  backoff and a maximum count.

A process with no readiness probe is ready as soon as it starts.

### Containers

Each container app is one Docker Compose project, and each of its processes
is one Compose service. The Compose file holds no value. It maps each of a
service's environment variables to a private reference, such as
`POSTGRES_PASSWORD: "${MXV_ENV_postgres_1:-}"`, and phase sets the `MXV_ENV_*` values
only on that service's `docker compose up` child process, which drops them
when it exits. phase does not list a variable by its bare name, because
Compose would then read it from the child's own environment, and a
container variable named `PATH` or `DOCKER_HOST` would change which docker
program or daemon the child uses. multiverse uses the same rule
(crumbkit/multiverse#6), so its code can move into phase. The Docker engine
still keeps the values in the container's configuration for the container's
life, where `docker inspect` shows them; every way of passing an
environment variable to a container does this. Docker's restart policy
restarts container processes.

phase labels every Compose project, container, network and volume it
creates with the workspace and app, and records each container by its
immutable id.

### Run record and sweep

phase writes a run record (`phase/run/v1`) for each workspace. It names every
keeper and app process by pid, process start time and process group, every
container by id, and the hash of the plan that started them. It holds no
environment values.

`phase sweep` reads the record and stops whatever is still running from a
run that phase no longer supervises. It matches a process by pid and start
time together, so a reused pid is never mistaken for an app.

### Status and logs

`phase status --json` prints a `phase/status/v1` document with one row per
process: its app, runner, source, state, pid or container, address, start
time, restart count and log path. `phase logs` prints a process's log, and
`--follow` keeps printing it. Container logs come from `docker logs`.

## Environment and secrets

phase resolves an app's environment through starmap. starmap's resolution
produces an `AppliedEnvironment`, a type that can list variable names and
apply values to a `Command` but cannot return a value. It has no `Debug`,
`Display` or `Serialize` implementation.

A secret value takes only these paths:

- for a host process, from starmap's resolvers into the keeper's
  environment, and from the keeper into the app process when it starts;
- for a container, from starmap's resolvers into the `docker compose up`
  child, and from there into the Docker engine.

No value is written to a file, an argument list, a log, the run record or a
status document. When a resolver cannot supply a value, for example because
a 1Password vault is locked, `Phase::up` returns that error for the app
before starting anything, and names the variable and the resolver.

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
    fn resolve(&self, app: &AppName, deadline: Instant)
        -> Result<Box<dyn AppliedEnvironment>, EnvironmentError>;
}

pub struct Phase;
impl Phase {
    pub fn up(&self, plan: &Plan, select: &Selection, env: &dyn EnvironmentSource, deadline: Instant) -> Result<Report, PhaseError>;
    pub fn down(&self, workspace: &Path, select: &Selection, deadline: Instant) -> Result<Report, PhaseError>;
    pub fn status(&self, workspace: &Path) -> Result<Status, PhaseError>;
    pub fn logs(&self, workspace: &Path, request: &LogRequest, sink: &mut dyn LineSink) -> Result<(), PhaseError>;
    pub fn sweep(&self, workspace: &Path, dry_run: bool) -> Result<Swept, PhaseError>;
}
```

`mxv` links this library. Other hosts can link it too, giving phase their
own plan and `EnvironmentSource`.

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
| P3 | How a container receives a secret. | The Compose file lists the name; the value is set on the `docker compose up` child only. |
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
