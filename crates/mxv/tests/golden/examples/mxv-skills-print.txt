---
name: mxv
description: Create and operate mxv task workspaces. Use when starting isolated work across one or more repositories, running or testing an app in a workspace, resuming a workspace, or getting a workspace's ports or logs.
---

# Working in mxv workspaces

An mxv workspace gives a task its own clone of each repository, on a new branch
from the freshly fetched origin, and its own host ports. Work in one workspace
cannot change the source repository or another workspace.

Create a workspace when a task changes code in one or more repositories, or
when it runs an app that another session may also be running. A task that only
reads a repository can use the existing checkout.

Create one with `mxv new NAME --repo PATH`, repeating `--repo` for each
repository. Each repository becomes a folder inside the workspace directory.
`mxv add -w NAME --repo PATH` adds a repository to an existing workspace.

`mxv up`, `mxv run`, `mxv exec`, `mxv status` and `mxv logs` act on the
workspace that holds the current directory. Pass `-w NAME` from anywhere else.

## Choose the command that matches the job

| Job | Command | Behaviour |
| --- | --- | --- |
| Run the app | `mxv up [UNIT]...` | mxv builds and starts the processes and containers in `mxv.toml`, in dependency order, and returns once each is ready. `mxv restart [UNIT]...` builds them, stops them and starts them again, and `mxv down` stops them. |
| Run a job that finishes, such as tests | `mxv run TASK` | mxv runs a task from `[tasks.<name>]` in `mxv.toml` or a mise task of a checkout, and exits with its exit code. A repository with mise tasks needs no `mxv.toml`. `mxv run` with no task lists the tasks. |
| Run an ad hoc command, such as one test or a script | `mxv exec -- COMMAND` | mxv runs the command on the host with the workspace's ports and every service's variables, in the current directory when that is inside a checkout. It needs no running unit and no `mxv.toml`, and it starts no services, so run `mxv up` first when the command talks to them. `mxv exec --repo FOLDER -- COMMAND` runs it at the top of that checkout. |
| Run one command inside a running unit | `mxv exec UNIT -- COMMAND` | The command gets a host process's directory, variables and sandbox, or runs inside the container. The unit must already be running. |

## Resume a workspace and read its state

- `mxv list` names the workspaces, and `cd "$(mxv cd NAME)"` resumes one.
  `mxv cd NAME FOLDER` prints the directory of one checkout.
- `mxv status` reports each unit's state and the ports it receives or
  publishes. `mxv show NAME` reports the host port for each named port.
- `mxv logs UNIT` prints a unit's last 50 log lines, and
  `mxv logs UNIT --follow` keeps printing new ones.

## Finish with a workspace

- `mxv archive -w NAME` puts a finished workspace away and keeps each
  checkout's commit, and `mxv restore NAME` rebuilds it at those commits.
  `mxv list --archived` lists the archives.
- `mxv remove NAME` removes a workspace's checkouts, its merged branches, its
  directory and its record. It refuses while a checkout holds unpushed or
  uncommitted work, so push that work first.

## Complete mxv's commands in a shell

`mxv completion install` writes the shell completion for mxv's commands where
your shell loads it, with `--shell bash|zsh|fish` when `SHELL` does not name it.

## Run heavy jobs through deadband

`mxv up`, `mxv restart`, `mxv run` and `mxv exec -- COMMAND` ask deadband for a
place themselves, so never wrap them in `deadband run`. When mxv runs inside
`deadband run`, its builds lose deadband's thread limits and can use every
core. `mxv exec --no-gate -- COMMAND`
asks deadband nothing, for a light or interactive command.
`mxv exec UNIT -- COMMAND` does not ask deadband, so run a heavy command with
`mxv exec -- COMMAND` instead. On a machine that runs deadband, start every
other heavy job with `deadband run --label <label> -- <command>`.

## When mxv up asks for permissions

`mxv up` and `mxv restart` refuse with `mxv::permissions::not_granted` when a
unit is new or asks for more permissions than a person has granted to the
repository, and they change nothing. Only a person can grant, by running
`mxv up` in a terminal and answering yes, so hand that step to the person and
quote the permissions the failure lists. `mxv exec UNIT` refuses the same way.
`mxv down UNIT` fails with `mxv::process::still_needed` when a running unit
depends on UNIT; name the dependants too to stop them all.

## Keep this skill current

mxv embeds this skill, so the installed copy matches the mxv that wrote it.
`mxv skills install` writes it into each agent skills directory that exists,
and `mxv skills check` exits with 1 when a copy is missing or differs from
this mxv's. `mxv skills print` prints it.

## Find everything else in the tool

`mxv commands list --all` lists every command, and `mxv <command> --help`
describes one. `--json` prints JSON, and `mxv schemas list` and
`mxv schemas show <ID>` describe its documents.

## Stable names for HTTP ports

When the manifest lists `routes`, `mxv up` serves each at
`https://<port>.<workspace>.mxv.localhost`, and `mxv proxy status` lists the
URLs. Open those names instead of `localhost:<port>`. A warning from `up` that
carries a `mxv::proxy::` code leaves the units running; report it. Never run
`mxv proxy trust`: it raises a system password prompt that only the person
can answer, so tell the person to run it.
