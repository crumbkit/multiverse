//! How the keeper starts its app: directly, or through the launcher, which
//! applies the sandbox that the specification describes before it becomes
//! the app. The sandbox module of the crate describes the sandbox.

use std::process::Command;

use crate::engine::keeper::KeeperSpec;
use crate::sandbox::{SANDBOX_ENVIRONMENT, launcher_command};

/// Returns the command that starts the app of `spec`: its own program and
/// arguments, or the launcher with them when `spec` names a sandbox.
pub(crate) fn app_command(spec: &KeeperSpec) -> Result<Command, String> {
    let (program, arguments) = spec
        .run
        .split_first()
        .ok_or_else(|| "the specification's command is empty".to_owned())?;
    if let Some(sandbox) = &spec.sandbox {
        return launcher_command(sandbox, &spec.run)
            .map_err(|error| format!("the sandbox cannot be passed to the launcher: {error}"));
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/engine/keeper/sandbox.rs builds the keeper's app process command (PROC-1)"
    )]
    let mut command = Command::new(program);
    command.args(arguments);
    Ok(command)
}

/// Sets `TMPDIR` on `command` to the sandbox's temporary directory, and
/// the variables of [`SANDBOX_ENVIRONMENT`], when `spec` names a sandbox.
/// It replaces any value the environment gave, because the sandbox lets
/// the app write no other temporary directory.
pub(crate) fn sandbox_environment(spec: &KeeperSpec, command: &mut Command) {
    if let Some(sandbox) = &spec.sandbox {
        command.env("TMPDIR", &sandbox.tmp);
        command.envs(SANDBOX_ENVIRONMENT.iter().copied());
    }
}
