# Decision: the environment of a host process

Status: decided, 5 November 2026, for the change that makes phase the only
runner of `mxv up`, `down`, `restart`, `status` and `logs`.

## What mxv did before

The runner that mxv used before phase started each host process with the
whole environment of the `mxv` command that started it, plus the process's
manifest `env` and port variables. A process therefore received every
variable of the user's shell, including cloud credentials, tokens and build
overrides such as `RUSTFLAGS` and `CARGO_TARGET_DIR`.

## What phase requires

phase starts each host process from an empty environment. It gives the
process `PATH` and `HOME` from its own caller, and every other variable
comes from the host program's `EnvironmentSource`. phase's plan names each
variable that each process receives, and holds no value. phase's keeper
refuses to start a process whose resolved environment names `PATH` or
`HOME`, because it sets those two itself.

mxv therefore has to choose which of its caller's variables a host process
receives, and has to name them in the plan.

## The decision

A host process receives:

- `PATH` and `HOME`, which phase passes itself;
- its manifest `env`, with `{port.NAME}` placeholders rendered, and its
  port variables;
- each variable of the allowlist `PASSED_THROUGH` in
  `crates/multiverse/src/phase_plan.rs`, and each variable whose name
  starts with `LC_`, when the `mxv` command's environment holds it.

The allowlist covers the locale, the terminal, the user, the temporary and
XDG directories, the tool managers that `cargo run`, `pnpm run dev` and
`sh -c` rely on, `SSH_AUTH_SOCK`, `RUST_LOG` and `RUST_BACKTRACE`. No other
variable passes. A credential or a build override reaches a process only
when its manifest `env` sets it, and a manifest can set only a literal
value or a port placeholder.

A manifest that sets `PATH` or `HOME`, in `env` or `ports`, fails with
`mxv::manifest::unrunnable` and the line of the unit, before anything
starts.

A container receives only its manifest `env`, as it did before. The
docker commands that phase runs receive `PATH`, `HOME` and each `DOCKER_*`
variable of the `mxv` command's environment, so that `DOCKER_HOST` and
`DOCKER_CONTEXT` still choose the daemon. A host process never receives a
`DOCKER_*` variable unless its manifest sets it.

`main` reads the environment once, with `std::env::vars_os`, before it
does anything else, and both the plan and the docker environment come
from that one snapshot.

## Why

An allowlist keeps credentials and build overrides of the user's shell out
of every process that a workspace runs, which matters more once phase runs
apps that the user did not write. A host process still finds its tools,
because `PATH`, `HOME` and the tool managers' directories pass.

The other choice was to name every variable of the caller in the plan. That
would put the names of the user's credentials into phase's run record, and
it would make the plan change whenever any variable of the shell changed.

## Consequences

The plan names each passed-through variable that is present, and holds no
value. So the plan, and the digest that phase computes from it, depend on
which allowlisted variables the `mxv` command's environment holds. They do
not depend on the values. Two `mxv up` commands run from shells that differ
only in whether `TZ` is set produce different plans and different digests.
The test `the_plan_and_its_json_depend_on_which_passed_through_variables_the_caller_holds`
in `crates/multiverse/src/phase_plan/tests.rs` checks this.

Today phase records the digest of the whole plan in its run record and in
each host process's entry, and compares it with nothing for a host process.
`mxv up` starts a host process only when it is not running, so an `up` from
a shell with a different set of variables leaves the running processes as
they are, with the variables they started with. `mxv restart` starts them
with the current set. A container's digest covers only that container's
compose service, which holds only its manifest `env`, so the caller's
variables never cause phase to recreate a container.

phase's design says that `up` will compare each process's recorded inputs,
the plan hash among them, and restart a process whose inputs changed. When
that lands, an `up` from a shell with a different set of allowlisted
variables will restart every host process of the workspace. mxv will then
need either a plan that names a fixed set of variables, or a digest that
leaves the passed-through names out.

These are the changes that users will see:

- A host process no longer receives variables of the user's shell outside
  the allowlist. A process that relied on one, such as `AWS_PROFILE` or
  `DATABASE_URL` exported in the shell, has to set it in its manifest
  `env`.
- A manifest that sets `PATH` or `HOME` for a process now fails with
  `mxv::manifest::unrunnable`.
- The plan, and its digest, depend on which allowlisted variables are set
  when `mxv up` runs.
