# multiverse

multiverse creates task workspaces that span several git repositories. A
workspace holds one clone for each repository, with a `.git` of its own,
branched from the latest main, an `AGENTS.md` that tells an agent how to work there, and a
record from which an archived workspace can be restored.

The repository holds three crates:

- [`multiverse`](crates/multiverse), the library.
- [`mxv`](crates/mxv), the command built on the library.
- [`phase`](crates/phase), published as `mxv-phase`, the library that will
  run a workspace's apps for `mxv`.

## Quick start

`mxv` builds with `cargo install --path crates/mxv`. A workspace of one
repository starts from a local clone that has a remote named `origin`:

```sh
mxv new reading-list --repo ~/src/app   # fetch origin, branch reading-list from origin/main
cd "$(mxv cd reading-list)"             # resume the workspace
mxv list                                # list the workspaces
mxv show reading-list                   # print its record
mxv up -w reading-list                  # start the app's processes from its mxv.toml
mxv status -w reading-list              # report each process as running, exited or stopped
mxv logs -w reading-list web --follow   # print a process's log and follow it
mxv down -w reading-list                # stop the processes
mxv clean --all --dry-run               # list the build output that idle workspaces would free
mxv remove reading-list                 # remove it, and its branch when merged
```

Workspaces live in `~/.local/share/mxv/workspaces` by default. `MXV_ROOT`,
or `root = "~/work"` in `~/.config/mxv/config.toml`, moves the root for new
workspaces. [crates/mxv/README.md](crates/mxv/README.md) describes the layout
of a workspace, the root and its config file, the `mxv.toml` manifest, where
mxv keeps its records, how mxv measures and frees disk space, and the
variables it reads.

multiverse is under construction. A workspace holds one repository for
now. `mxv archive` and `mxv restore` are declared and fail with
`mxv::command::unbuilt` until each one is built.

## Licence

multiverse is licensed under either of the Apache License, Version 2.0
([LICENSE-APACHE](LICENSE-APACHE)) or the MIT licence
([LICENSE-MIT](LICENSE-MIT)), at your option.
