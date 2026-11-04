# Working on multiverse

multiverse is a Cargo workspace of three crates: `crates/multiverse`, the
library, `crates/mxv`, the command built on it, and `crates/phase`, the
library that will run a workspace's apps. phase is published as
`mxv-phase`, imported as `phase`, and described by
[crates/phase/docs/design.md](crates/phase/docs/design.md). The rules are
the Rust standards that [rust-standards.toml](rust-standards.toml) adopts.

A change is ready when the pull-request lane passes:

```sh
export CARGO_BUILD_WARNINGS=deny
cargo fmt --check
zizmor .
cargo deny --locked check bans licenses sources
cargo shear
cargo clippy --locked --workspace --all-targets --all-features
cargo nextest run --locked --workspace --all-features
cargo test --locked --doc --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps
cargo hack clippy --locked --workspace --each-feature --all-targets
cargo +1.89.0 check --locked --workspace --all-targets --all-features
```

- Write the test first, named as a sentence (BASE-36).
- Give each new dependency a reason in `Cargo.toml` (BASE-23).
- `SNAPSHOTS=overwrite cargo nextest run` rewrites the golden files under
  `crates/mxv/tests/golden` and `crates/mxv/docs/commands.md`. Read the diff
  before you commit it.
- A pull request that builds a workspace verb removes that verb's opt-outs
  from `crates/mxv/tests/conventions.rs`.

## Writing

Docs, comments and commit messages use plain declarative sentences with a
named subject. Avoid "not X but Y", fragments, metaphors and closing
flourishes. A stated limit says what it counts and what happens when it is
hit. Commit messages describe changed behaviour.
