# multiverse

multiverse creates task workspaces that span several git repositories. A
workspace holds one checkout for each repository, branched from the freshly
fetched default branch of its `origin`, an `AGENTS.md` that tells an agent
how to work there, and a record in a store. The [`mxv`](../mxv) command is
built on this library.

`Workspaces` holds the operations: create, list, find, plan a removal and
remove. It makes checkouts through the `Backend` trait, and `Git`, which
runs `git worktree`, is the one backend. `Manifest` parses a repository's
`mxv.toml`, and `Runs` starts, stops and reports the processes it declares,
with a host port for each named port that the workspace's record keeps. The
process supervision uses Unix process groups and signals, so it runs on
Linux and macOS. The library reads no environment
variable, and its caller passes the workspace root and the store's
directory.
