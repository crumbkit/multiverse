# phase

phase runs the apps of a workspace on your machine. It starts each app from a
published release or from a checkout you are editing, gives it only the
environment variables its manifest declares, runs it under the operating
system's sandbox, waits for it to become ready, and restarts it when it fails.

phase is a Rust library with a command. The command is run through `mxv`, the
multiverse command, as `mxv phase …`, and the crate is published as
`mxv-phase`. Other programs can link the library instead.

phase is being designed. [docs/design.md](docs/design.md) describes what it
will do and lists the decisions that are still open. The library defines
its interface, and every operation reports that it is not built yet.

## Licence

phase is licensed under either of the Apache License, Version 2.0
([LICENSE-APACHE](LICENSE-APACHE)) or the MIT licence
([LICENSE-MIT](LICENSE-MIT)), at your option.
