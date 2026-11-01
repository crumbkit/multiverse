//! multiverse creates task workspaces that span several git repositories.
//!
//! A workspace holds one git worktree for each repository, branched from
//! the latest main, an `AGENTS.md` that tells an agent how to work there,
//! and a record from which an archived workspace can be restored. The `mxv`
//! command is built on this library.
//!
//! The library holds no API yet. Each item arrives with the command that
//! first needs it.
