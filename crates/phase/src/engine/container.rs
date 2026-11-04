//! The container runner, which runs each container app as one Docker
//! Compose project and each of its processes as one Compose service. The
//! Compose file maps each environment variable to a private `MXV_ENV_*`
//! reference, which only that service's `docker compose up` child receives.
//! multiverse's `docker` module lands here. This module owns the `Command`s
//! of `docker` (PROC-1).
