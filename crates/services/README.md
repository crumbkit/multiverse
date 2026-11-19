# mxv-services

mxv-services serves the local AWS services of an mxv workspace in one
process. Each kind of service listens on its own port on 127.0.0.1 and keeps
its data in memory, so the data is gone when the process stops.

| Kind | Server |
| --- | --- |
| `dynamodb` | [Dynoxide](https://github.com/nubo-db/dynoxide) 1.7.0, embedded as the `dynoxide-rs` 4.0.0 library, on an in-memory SQLite database. |

Dynoxide never deletes an item whose time-to-live attribute has passed. A
table with a `ttl` attribute keeps its expired items until the process
stops, so a program that relies on expiry reads the attribute itself.

The crate is a library with no command of its own. `mxv` links it and runs
it as the host app `mxv-services`, with the command line
`mxv phase services <kind>=<port>...`. `services::serve` binds every
listener, calls back once all of them are bound, and serves until the
future it receives resolves.

## Licence

mxv-services is licensed under either of the Apache License, Version 2.0
([LICENSE-APACHE](LICENSE-APACHE)) or the MIT licence
([LICENSE-MIT](LICENSE-MIT)), at your option.
