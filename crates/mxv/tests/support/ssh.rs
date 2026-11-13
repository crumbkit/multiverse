//! A stand-in for ssh that the end-to-end tests give git, so a fetch from
//! an SSH origin needs no server and no key.

use std::path::{Path, PathBuf};

/// The SSH URL that the tests give an origin. `.invalid` is a name that no
/// network resolves, so only the fake ssh of [`fake_ssh`] can answer it.
pub(crate) const SSH_ORIGIN: &str = "ssh://git@example.invalid/acme/app.git";

/// A stand-in for ssh that answers at once, as the library's test fixture
/// writes it. Its first word chooses how the host answers: `deny` refuses
/// every key, `unknown` shows a host key that the known hosts file lacks,
/// and `accept` serves the repository `ORIGIN` with `git upload-pack`. Each
/// run appends its words and the values of `SSH_ASKPASS` and
/// `SSH_ASKPASS_REQUIRE` to the file `LOG`. The stand-in prints ssh's own
/// line and exits with 255 when those variables send ssh's question to an
/// askpass program, and otherwise prints `fake ssh would prompt` and exits
/// with 97, where a real ssh would wait.
const FAKE_SSH: &str = r#"#!/bin/sh
mode=$1
shift
printf 'args: %s %s\nSSH_ASKPASS=%s\nSSH_ASKPASS_REQUIRE=%s\n' "$mode" "$*" "${SSH_ASKPASS-}" "${SSH_ASKPASS_REQUIRE-}" >> 'LOG'
for word in "$@"; do
  [ "$word" = -G ] && exit 0
done
if [ "$mode" = accept ]; then
  exec git upload-pack 'ORIGIN'
fi
if [ "${SSH_ASKPASS_REQUIRE-}" = force ] && [ -n "${SSH_ASKPASS-}" ]; then
  case $mode in
    deny) echo 'git@example.invalid: Permission denied (publickey).' >&2 ;;
    *) echo 'Host key verification failed.' >&2 ;;
  esac
  exit 255
fi
echo 'fake ssh would prompt' >&2
exit 97
"#;

/// A fake ssh written by [`fake_ssh`].
pub(crate) struct FakeSsh {
    /// The script.
    pub(crate) program: PathBuf,
    /// The file that each run of the script appends to.
    pub(crate) log: PathBuf,
}

impl FakeSsh {
    /// Returns the SSH command that runs the script in `mode`, as a person
    /// would write it in `GIT_SSH_COMMAND`.
    pub(crate) fn command(&self, mode: &str) -> String {
        format!("{} {mode}", self.program.display())
    }

    /// Returns what the runs of the script have logged.
    pub(crate) fn logged(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

/// Writes the fake ssh of [`FAKE_SSH`] into `dir`, serving `origin` in its
/// `accept` mode.
pub(crate) fn fake_ssh(dir: &Path, origin: &Path) -> FakeSsh {
    use std::os::unix::fs::PermissionsExt as _;
    let program = dir.join("fake-ssh");
    let log = dir.join("fake-ssh.log");
    let script = FAKE_SSH
        .replace("'LOG'", &format!("'{}'", log.display()))
        .replace("'ORIGIN'", &format!("'{}'", origin.display()));
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes a program that stands in for ssh"
    )]
    std::fs::write(&program, script).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    FakeSsh { program, log }
}
