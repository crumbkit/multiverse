# multiverse

multiverse creates task workspaces that span several git repositories. A
workspace holds one git worktree for each repository, branched from the
latest main, an `AGENTS.md` that tells an agent how to work there, and a
record from which an archived workspace can be restored.

The repository holds two crates:

- [`multiverse`](crates/multiverse), the library.
- [`mxv`](crates/mxv), the command built on the library.

## Quick start

`mxv` builds with `cargo install --path crates/mxv`. A workspace of one
repository starts from a local clone that has a remote named `origin`:

```sh
mxv new reading-list --repo ~/Code/inbox   # fetch origin, branch reading-list from origin/main
cd "$(mxv cd reading-list)"                # resume the workspace
mxv list                                   # list the workspaces
mxv show reading-list                      # print its record
mxv remove reading-list                    # remove it, and its branch when merged
```

[crates/mxv/README.md](crates/mxv/README.md) describes the layout of a
workspace, where mxv keeps its records, and the variables it reads.

multiverse is under construction. A workspace holds one repository for
now. `mxv run`, `mxv archive` and `mxv restore` are declared and fail with
`mxv::command::unbuilt` until each one is built.

## Licence

multiverse is licensed under either of the Apache License, Version 2.0
([LICENSE-APACHE](LICENSE-APACHE)) or the MIT licence
([LICENSE-MIT](LICENSE-MIT)), at your option.
