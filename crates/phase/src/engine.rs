//! The engine that starts, supervises and stops an app's processes. The
//! container runner, the keeper, the readiness probes and the restart
//! decisions are built, and the host runner is empty until its code lands.

pub mod container;
pub mod host;
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
