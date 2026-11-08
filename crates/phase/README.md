# phase

phase runs the apps of a workspace on your machine. It starts each app from a
checkout that has no build step, or from an image, gives it only the
environment variables its manifest declares, runs it under the operating
system's sandbox, waits for it to become ready, and restarts it when it fails.

phase is a Rust library with no command of its own. `mxv`, the multiverse
command, links it and calls it for `mxv up`, `down`, `restart`, `status`,
`logs` and `exec`, and for `mxv remove`. The crate is published as
`mxv-phase`, and other programs can link it too.

The library offers `Phase::up`, `restart`, `down`, `status`, `logs`, `sweep`,
`exec` and `exec_sandboxed`. `up` refuses an app from a release, and a
checkout with a build step, with `PhaseError::NotRunnable`
(`NotRunnable::Release` and `NotRunnable::CheckoutBuild`). `logs` with
`since` returns `PhaseError::NotBuilt`. Trust grants are not built.
[docs/design.md](docs/design.md) is the record of the design and its
decisions.

## Licence

phase is licensed under either of the Apache License, Version 2.0
([LICENSE-APACHE](LICENSE-APACHE)) or the MIT licence
([LICENSE-MIT](LICENSE-MIT)), at your option.
