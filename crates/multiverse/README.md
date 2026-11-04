# multiverse

multiverse creates task workspaces that span several git repositories. A
workspace holds one checkout for each repository, branched from the freshly
fetched default branch of its `origin`, an `AGENTS.md` that tells an agent
how to work there, and a record in a store. The [`mxv`](../mxv) command is
built on this library.

`Workspaces` holds the operations: create, list, find, plan a removal and
remove. It makes checkouts through the `Backend` trait, and `Git` is the
one backend. With `Workspaces::with_mirrors`, each checkout is a
`git clone --shared` of a bare mirror of the origin, with a `.git` of its
own. A workspace that an earlier version made holds git worktrees of the
person's repository, and the library still lists and removes it. `Manifest` parses a repository's
`mxv.toml` or a workspace manifest. `Workspace::manifest` reads the one
manifest that a workspace uses, the workspace manifest at
`<workspace>/mxv.toml` or else the checkout's `mxv.toml`, and
`Workspace::manifest_inputs` returns both paths as separate inputs for a
tool that reads them both. `Workspace::manifest_trusted` reports whether
the workspace manifest is a regular file that no repository can have
written, which is the only case in which its `disposable` globs count as
the person's own. `find_template` picks the template of workspace
manifests that lists a repository's origin, and
`Workspaces::check_manifest_source` refuses a `--manifest` file inside a
git work tree or under the workspace root. `Runs` starts, stops and reports the processes it declares,
with a host port for each named port that the workspace's record keeps. The
process supervision uses Unix process groups and signals, so it runs on
Linux and macOS. The library reads no environment
variable, and its caller passes the workspace root and the store's
directory.
