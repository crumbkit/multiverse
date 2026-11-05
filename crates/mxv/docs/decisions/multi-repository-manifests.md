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
`<prefix>-<name>`: each process, each container, each port and each
volume. mxv rewrites every reference inside that file to the prefixed
name: the values of a unit's `ports`, its `listen`, its `ready.port`, each
`{port.<name>}` placeholder in its `env`, each name in its `depends_on`,
and the volume of each of a container's `volumes`. In the checkout `api`,
the process `server` becomes `api-server`, the port `http` becomes
`api-http`, `{port.http}` becomes `{port.api-http}`, and the volume `data`
becomes `api-data`. phase names a volume `<workspace>.<volume>`, after the
workspace alone, so without the prefix the containers of two checkouts
that each mount `data` would share one volume and its data. Containers of
one checkout that mount one volume still share it. Variable names, such
as `PORT`, keep their names, because a variable belongs to its process.

A checkout's manifest can refer only to its own names. The workspace
manifest's names carry no prefix, and its units can refer to any merged
name, such as `depends_on = ["api-server"]` or
`API_URL = "http://127.0.0.1:{port.api-http}"`.

### Clashes and limits

The merge fails with `mxv::manifest::name_clash` when two of its names are
equal: two units, two ports or two volumes, from any two of its files. A prefix can
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
origin. `--manifest` copies a file with either form. With several
`--repo`, `mxv new` checks the file against the merged manifest of the new
clones, which come from origin, after it clones them. When the check
fails, it deletes the workspace and fails with the manifest's error, such
as `mxv::manifest::invalid` for a reference to a name that no file
declares.

A checkout's folder in a workspace with several checkouts must follow the
folder rule of `--as`, whether `--as` gives it or the repository's name
does, because its prefix starts the names of phase apps and ports, and
phase refuses a name that starts with `_`. `mxv new` with several
`--repo` and `mxv add` refuse a repository named `_infra` with
`mxv::checkout::invalid_name`, whose hint says to give the checkout a
folder with `--as`. `mxv new` with one `--repo` gives its checkout no
prefix, so it still accepts any repository's name. `mxv add` to such a
workspace checks that the first checkout's folder gives a prefix that
starts with an ASCII letter or digit, because that folder becomes a
prefix too. It refuses a folder such as `_infra` or `.dotfiles` with
`mxv::checkout::invalid_name`, and accepts a folder such as `MyApp`,
whose prefix is `myapp`. A name that is longer than 64 bytes once
prefixed fails when `add` reads the merged manifest, which rolls the
`add` back. mxv cannot rename a checkout's
folder, so the hint says to create the workspace again with `mxv new`,
giving each repository a valid folder with `--as`.

### Adding a checkout to a workspace with one checkout

`mxv add` turns a workspace with one checkout into one with several. Its
units, ports and volumes then gain their prefix, so `mxv add` refuses with
`mxv::workspace::running` while the workspace runs.

The first checkout's containers then mount their volumes under the
prefixed names, so a volume that holds data under its old name would be
left unused while the container starts on an empty one. Before it clones
anything, `mxv add` therefore refuses with `mxv::volume::renamed` while
phase holds a volume under the old name of any volume of the first
checkout's `mxv.toml`. mxv checks only when the workspace reads that
file. When the workspace has a workspace manifest, the workspace reads
that file in place of the checkout's, its names keep no prefix, and its
volumes keep their names after the `add`, so mxv does not check. The failure lists each old and new docker volume.
Its hint gives, for each volume, the `docker volume create` with phase's
labels, the `docker run` that copies the data, and the
`docker volume rm` of the old volume, as the hint of
`mxv::volume::not_moved` does, and says that deleting the old volumes
alone drops their data. Once no old volume exists, `mxv add` goes ahead.

`mxv add` keeps the number of each host port whose name the merged
manifest declares. A port of the first checkout's `mxv.toml`, such as
`http`, moves with its number to its prefixed name, `api-http`. `mxv add`
releases every port that the merged manifest does not declare. The next
`mxv up` allocates a number only for a declared name that holds none.

When such a workspace has a workspace manifest, that file's paths are
relative to the checkout, and after the `add` they are relative to the
workspace's directory. mxv cannot tell which paths the person means: the
glob `coverage/` matches the checkout's `coverage/` before and every
checkout's `coverage/` after, and `web/coverage/` may name a directory
inside the first checkout or the new checkout's `coverage/`. So `mxv add`
without `--manifest` refuses with `mxv::manifest::needed` and changes
nothing. `mxv add --manifest <FILE>` takes the workspace manifest that the
workspace reads afterwards, with its paths relative to the workspace's
directory. `mxv add` clones the new checkout, checks the merged manifest
with that file, and only then copies the file to `<root>/<ws>/mxv.toml`
and writes the record. When the check fails, it deletes the new clone,
leaves the workspace as it was, and fails with the manifest's error. mxv
never rewrites the person's file. `--manifest` also replaces the workspace
manifest of a workspace that already has several checkouts.

### An `add` that stops partway

`mxv add` writes the record with the new checkout before it clones, with
no commit for that checkout. A checkout without a commit in a workspace of
clones is unfinished. A run that is killed before it writes the commit
leaves such a checkout, and its directory may be missing or partial.

The same `mxv add`, with the same repository and the same folder, resumes
it. The run looks at the checkout's directory:

- When nothing is there, it clones afresh.
- When the directory is a git clone of its own on the workspace's branch,
  the clone finished and only the record and `AGENTS.md` were left, so it
  keeps the clone, whatever work it holds, and records its commit.
- When git finds no clone of its own in the directory and the directory
  holds nothing but `.git`, or it is a clone of its own that is not on the
  workspace's branch and holds no stash, no commit that origin lacks, no
  uncommitted change or untracked path, and no ignored path outside its
  disposable set, it is what a clone that stopped partway leaves. The
  rule for ignored paths is the one that `mxv remove` follows, so a
  `.env` file stops the run while a `target/` directory does not. A clone that holds nothing but `.git` needs no
  clean working tree, because it never checked out a file. The
  run deletes it and clones afresh.
- Anything else, such as files outside a clone or a clone with a commit of
  its own, fails with `mxv::checkout::resume_blocked`, which names the
  directory and what it holds, and the run changes nothing.

The run then writes `AGENTS.md` and the record with the checkout's commit.
Any other `mxv add` to the workspace fails with
`mxv::checkout::incomplete`, whose hint names the unfinished folder and
the `mxv add -w <ws> --repo <repository> --as <folder>` that resumes it.

The record already lists the unfinished checkout, so until the `add`
resumes, the workspace counts as one of several checkouts while one of
them has no clone. Every command that reads the manifest to act on it,
such as `mxv up`, `mxv restart`, `mxv ports reset` and `mxv clean`,
therefore fails with `mxv::checkout::incomplete` and the same hint.
`mxv status`, `mxv show`, `mxv down`, `mxv logs` and `mxv remove` keep
working, because they only read the workspace, stop it or delete it.
`mxv remove` deletes a partial clone that holds nothing but `.git`
without fetching its origin. It treats a partial clone that git cannot
read and that holds other files as a change, which it deletes only with
`--force`.

When a resumed `add` fails, such as on a merged manifest that does not
check, it restores the record that the workspace had before the `add`
and deletes the new clone. It keeps a finished clone that holds work, by
the rule above, and then keeps the record that names it unfinished, and
its failure, `mxv::checkout::resume_blocked`, says what work the clone
holds.

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
