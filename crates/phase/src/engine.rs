//! The engine that starts, supervises and stops an app's processes.
//!
//! The `run` module carries out `up`, `down` and `restart`. It reaches host
//! processes only through the `host` module's `HostRunner`, and container
//! apps only through the `containers` module's `ContainerRunner`, so a test
//! supplies fakes of both.

pub mod container;
pub(crate) mod containers;
pub(crate) mod exec;
pub mod host;
pub mod keeper;
pub(crate) mod probe;
pub(crate) mod restart;
pub(crate) mod run;
