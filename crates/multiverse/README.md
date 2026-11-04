# multiverse

multiverse creates task workspaces that span several git repositories. A
workspace holds one checkout for each repository, branched from the freshly
fetched default branch of its `origin`, an `AGENTS.md` that tells an agent
how to work there, and a record in a store. The [`mxv`](../mxv) command is
built on this library.

`Workspaces` holds the operations: create, list, find, plan a removal and
remove. It makes checkouts through the `Backend` trait, and `Git`, which
runs `git worktree`, is the one backend. The library reads no environment
variable, and its caller passes the workspace root and the store's
directory.
