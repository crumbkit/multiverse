//! The engine that starts, supervises and stops an app's processes. Each
//! submodule is empty until the code it names lands.

mod container;
mod host;
pub mod keeper;
mod probe;
mod restart;
