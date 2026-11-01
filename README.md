# multiverse

multiverse creates task workspaces that span several git repositories. A
workspace holds one git worktree for each repository, branched from the
latest main, an `AGENTS.md` that tells an agent how to work there, and a
record from which an archived workspace can be restored.

The repository holds two crates:

- [`multiverse`](crates/multiverse), the library.
- [`mxv`](crates/mxv), the command built on the library.

multiverse is under construction. `mxv` declares its workspace verbs, and
each one fails with `mxv::command::unbuilt` until it is built.

## Licence

multiverse is licensed under either of the Apache License, Version 2.0
([LICENSE-APACHE](LICENSE-APACHE)) or the MIT licence
([LICENSE-MIT](LICENSE-MIT)), at your option.
