//! The engine that starts, supervises and stops an app's processes. The
//! container runner and the keeper are built, and each other submodule is
//! empty until the code it names lands.

pub mod container;
mod host;
pub mod keeper;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        unused_imports,
        reason = "the host and container runners wait for readiness through `probe` when `up` lands"
    )
)]
mod probe;
mod restart;
