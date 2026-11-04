//! The host runner, which starts each host process under its keeper and the
//! sandbox, in its own process group, and stops it with its stop signal and
//! grace. multiverse's process runner lands here. This module owns the
//! `Command`s of host processes and keepers (PROC-1).
