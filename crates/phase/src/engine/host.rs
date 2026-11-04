//! The host runner, which starts each host process under its keeper and the
//! sandbox, in its own process group, and stops it with its stop signal and
//! grace. multiverse's process runner lands here. Until the host runner
//! lands, the keeper module builds the `Command`s of keepers and of the app
//! processes they start (PROC-1).
