# Decision: the manifests and ports of a workspace with several checkouts

Status: decided, 5 November 2026, for the change that lets a workspace
hold several repositories.

## What mxv does today

A workspace holds one checkout. Every command reads exactly one manifest:
the workspace manifest `<root>/<ws>/mxv.toml` when it exists, or else the
checkout's own `mxv.toml`, or else none. The two files are never merged.
The paths in either file, such as a process's `cwd` or a `disposable`
glob, are relative to the checkout. Only the workspace manifest is
trusted, and only while it is a regular file with one link that no
checkout holds.

## The decision

### A workspace with one checkout

A workspace with exactly one checkout keeps every rule above. Its names
carry no prefix, so the ports that its record already holds keep their
names, and nothing about it changes.

### A workspace with several checkouts

A workspace with two or more checkouts reads a merged manifest, which
holds:

1. the units, ports and globs of the workspace manifest, when it exists,
   with their names as written;
2. the units, ports and globs of each checkout's own `mxv.toml`, when it
   exists, with each name prefixed as the next section describes.

A checkout without an `mxv.toml` adds nothing. The workspace manifest no
longer hides the checkouts' files: each checkout's file joins it.

### Prefixes

The prefix of a checkout is its folder name in lower case, with each
character other than an ASCII letter, a digit, `-` or `_` written as `-`.
The folder `Web.App` has the prefix `web-app`.

Every name that a checkout's manifest declares becomes
`<prefix>-<name>`: each process, each container, and each port. mxv
rewrites every reference inside that file to the prefixed name: the values
of a unit's `ports`, its `listen`, its `ready.port`, each `{port.<name>}`
placeholder in its `env`, and each name in its `depends_on`. In the
checkout `api`, the process `server` becomes `api-server`, the port `http`
becomes `api-http`, and `{port.http}` becomes `{port.api-http}`. Variable
names, such as `PORT`, and volume names keep their names, because a
variable belongs to its process and phase scopes a volume to the app that
mounts it.

A checkout's manifest can refer only to its own names. The workspace
manifest's names carry no prefix, and its units can refer to any merged
name, such as `depends_on = ["api-server"]` or
`API_URL = "http://127.0.0.1:{port.api-http}"`.

### Clashes and limits

The merge fails with `mxv::manifest::name_clash` when two of its names are
equal: two units, or two ports, from any two of its files. A prefix can
make a clash, such as the checkout `a-b` with the port `c` and the
checkout `a` with the port `b-c`, which both give `a-b-c`. The failure
names the merged name and the two files that declare it. Renaming one
checkout's folder with `--as` fixes it.

A merged name longer than 64 bytes, the limit of a manifest name, fails
with `mxv::manifest::invalid`, which names the name and its file.

Each checkout's file is checked on its own first, as today, so its own
errors keep their codes and lines. A file that does not parse fails the
merge with its own error. mxv does not fall back to the other files.

### Paths

The paths of a checkout's `mxv.toml` stay relative to that checkout, and
may not leave it. The paths of the workspace manifest of a workspace with
several checkouts are relative to the workspace's directory, so a
process's `cwd = "web"` runs in the checkout `web`, and the `cwd` may not
leave the workspace's directory. A `disposable` or `clone_on_new` glob of
the workspace manifest then matches the path `<folder>/<path>` of each
checkout: `api/data/` matches `data/` in the checkout `api`, and
`coverage/` or `**/coverage/` matches in every checkout.

### Lookup

`mxv up`, `down`, `restart`, `status` and `logs` name units by their
merged name, such as `mxv logs api-server`. `status`, `show` and `list
--long` report the manifest as `merged` for a workspace with several
checkouts, and list each file that joined in `manifest_paths`.

### Trust

The workspace manifest stays the only trusted manifest, under the rules it
has today. Its `disposable` globs count as the person's own, with the
source `workspace`. A checkout's `mxv.toml` is never trusted: its globs
apply only to its own checkout, with the source `manifest`, so their paths
wait for a confirmation or `--force`, as they do today. A workspace
manifest that is a link counts as a checkout's file for its globs, as
today.

### Templates

`mxv new` with one `--repo` copies a template as it does today. `mxv new`
with several `--repo` copies no template, because a template names one
origin. `--manifest` copies a file with either form.

### Adding a checkout to a workspace with one checkout

`mxv add` turns a workspace with one checkout into one with several. Its
units and ports then gain their prefix, so `mxv add` refuses with
`mxv::workspace::running` while the workspace runs, and the next `mxv up`
allocates host ports for the new names and drops the old ones. When such a
workspace has a workspace manifest, its paths would change from relative
to the checkout to relative to the workspace's directory, so `mxv add`
refuses with `mxv::manifest::checkout_relative`, whose hint names the file
to edit, and changes nothing.

### Phase plans

Each unit of the merged manifest is one phase app, named by its merged
name, and each port is one phase port, named by its merged name. Because
the merge refuses equal names, no two apps and no two ports of the plan
share a name. The merge makes each process's `cwd` absolute, and the
plan turns it back into a source and a relative `cwd`: the source is the
checkout that holds the directory, or the workspace's directory when no
checkout does, and the `cwd` is the rest of the path. A process of the
workspace manifest whose `cwd` is `web` therefore runs from the checkout
`web`. A `cwd` outside the workspace makes the manifest unrunnable.

phase does not yet check that the processes of two merged files never
listen on one port, because each file's ports carry its prefix and only
the workspace manifest can name another file's port. A later change can
add that check, and can run one app's processes from several
repositories.

## Record versions

A record of a workspace with several checkouts has version 4. An mxv from
before this change refuses it as a record that a newer mxv wrote, because
that mxv would read the workspace manifest's globs relative to each
checkout and could delete the wrong paths. A record with one checkout
keeps the version it has, so records written by earlier versions keep
working without a migration.
