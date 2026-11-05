//! The delivery of a container's secrets as files (decision P3): the
//! memory-only directory that holds them, the entrypoint wrapper that waits
//! for them, the script that writes one inside the container, and the
//! image's own command, which the wrapper runs once the files exist.
//!
//! The compose file of a process that receives its secrets as files mounts
//! a tmpfs at [`SECRETS_DIR`] with mode 0700, and replaces the image's
//! entrypoint with `/bin/sh -c <wrapper> phase-secrets <NAME>... --`. The
//! service's command is the image's own entrypoint followed by the plan's
//! command, or by the image's command when the plan has none, so the
//! wrapper ends by running what the container would have run without it.
//!
//! The wrapper needs only `/bin/sh`. It waits until `/run/secrets/<NAME>`
//! exists for every name it was given, then replaces itself with the
//! command, so the app runs as the container's first process and receives
//! its stop signal. While it waits, the stop signals that phase may send
//! end it. It pauses between checks with `sleep` when the image has that
//! program, and polls without pausing otherwise. It gives up after
//! [`WAIT_SECONDS`] and exits with status 1, naming the file it waited
//! for, which happens when Docker restarts the container under its restart
//! policy, because a restart empties the tmpfs and phase is not there to
//! write the files again. Without `sleep` it cannot count the time, and
//! waits without a limit.
//!
//! phase writes each secret with `docker exec -i --user 0`, which runs
//! [`writer`] in the container with the value on its standard input. The
//! script writes the value to a temporary file in the same tmpfs, gives
//! the directory and the file to the user of the container's first
//! process, which is the user the app runs as, makes the file readable by
//! that user alone, and renames it over `/run/secrets/<NAME>`, so a reader
//! sees the old value or the new one and never part of either. It needs
//! `/bin/sh`, `cat`, `chown`, `chmod`, `mv` and `rm`, which busybox and
//! every common distribution's base image provide, and reads the first
//! process's user from `/proc/1/status`.

use serde::Deserialize;

use crate::engine::container::compose::literal;

#[cfg(test)]
mod tests;

/// The directory inside a container that holds its secrets, one file per
/// secret, named after the variable.
pub(crate) const SECRETS_DIR: &str = "/run/secrets";

/// The mode of [`SECRETS_DIR`], which only its owner may enter.
pub(crate) const SECRETS_MODE: u32 = 0o700;

/// How long the wrapper waits for the secrets' files when the image has a
/// `sleep` program, in seconds.
pub(crate) const WAIT_SECONDS: u32 = 60;

/// The name the wrapper's shell gives itself, as `$0`.
const WRAPPER_NAME: &str = "phase-secrets";

/// The name the writer's shell gives itself, as `$0`.
const WRITER_NAME: &str = "phase-secret-write";

/// The argument that ends the names of the secrets in the wrapper's
/// arguments, before the command that it runs.
const END_OF_NAMES: &str = "--";

/// Returns the wrapper's script, which waits for a file in `dir` for each
/// of its arguments up to `--`, and then runs the arguments after `--`.
pub(crate) fn wrapper(dir: &str) -> String {
    waiting_wrapper(dir, WAIT_SECONDS)
}

/// Returns the wrapper's script as [`wrapper`] does, which gives up after
/// `seconds`.
fn waiting_wrapper(dir: &str, seconds: u32) -> String {
    let tenths = seconds * 10;
    format!(
        r#"d='{dir}'
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 131' QUIT
trap 'exit 138' USR1
trap 'exit 140' USR2
trap 'exit 143' TERM
if command -v sleep >/dev/null 2>&1; then pause=1; else pause=0; fi
tenths=0
while [ "$#" -gt 0 ] && [ "$1" != '{END_OF_NAMES}' ]; do
  while [ ! -e "$d/$1" ]; do
    if [ "$tenths" -ge {tenths} ]; then
      echo "phase: $d/$1 did not appear within {seconds} seconds" >&2
      exit 1
    fi
    if [ "$pause" = 1 ]; then
      if sleep 0.1 2>/dev/null; then tenths=$((tenths + 1)); else sleep 1; tenths=$((tenths + 10)); fi
    fi
  done
  shift
done
if [ "$#" -gt 0 ]; then shift; fi
if [ "$#" -eq 0 ]; then
  echo 'phase: the container has no command to run' >&2
  exit 127
fi
exec "$@"
"#
    )
}

/// Returns the entrypoint of a service that waits for the files of the
/// secrets `names`, as compose file text.
pub(crate) fn entrypoint<'n>(names: impl Iterator<Item = &'n String>) -> Vec<String> {
    let mut entrypoint = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        literal(&wrapper(SECRETS_DIR)),
        WRAPPER_NAME.to_owned(),
    ];
    entrypoint.extend(names.map(|name| literal(name)));
    entrypoint.push(END_OF_NAMES.to_owned());
    entrypoint
}

/// Returns the script that writes its standard input to the secret file
/// in `dir` named by its first argument, as the module docs say.
pub(crate) fn writer(dir: &str) -> String {
    format!(
        r#"set -eu
d='{dir}'
t="$d/.$1.tmp"
trap 'rm -f "$t"' EXIT
u=0
g=0
while read -r key value rest; do
  case "$key" in
    Uid:) u=$value ;;
    Gid:) g=$value ;;
  esac
done < /proc/1/status
umask 077
cat > "$t"
chown "$u:$g" "$d" "$t"
chmod 0400 "$t"
mv -f "$t" "$d/$1"
"#
    )
}

/// Returns the arguments of `docker` that write the secret `name` into
/// the container `id`, with the value on standard input.
pub(crate) fn write_args(id: &str, name: &str) -> Vec<String> {
    [
        "exec",
        "--interactive",
        "--user",
        "0",
        "--",
        id,
        "/bin/sh",
        "-c",
        &writer(SECRETS_DIR),
        WRITER_NAME,
        name,
    ]
    .map(str::to_owned)
    .to_vec()
}

/// The entrypoint and command of an image, as `docker image inspect`
/// reports them, which a service that waits for its secrets runs after
/// the wrapper.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct ImageCommand {
    /// The image's entrypoint.
    #[serde(default, deserialize_with = "or_empty")]
    entrypoint: Vec<String>,
    /// The image's command.
    #[serde(default, deserialize_with = "or_empty")]
    cmd: Vec<String>,
}

/// Reads a list that Docker prints as `null` when it is empty.
fn or_empty<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<String>>::deserialize(deserializer)?.unwrap_or_default())
}

/// The Go template of `docker image inspect` whose output
/// [`ImageCommand::parse`] reads.
pub(crate) const IMAGE_COMMAND_FORMAT: &str =
    r#"{"entrypoint":{{json .Config.Entrypoint}},"cmd":{{json .Config.Cmd}}}"#;

impl ImageCommand {
    /// Returns the image command that `docker image inspect` printed with
    /// [`IMAGE_COMMAND_FORMAT`], or `None` when it printed something else.
    pub(crate) fn parse(printed: &str) -> Option<Self> {
        serde_json::from_str(printed.trim()).ok()
    }

    /// Returns the command a container of this image runs when its plan's
    /// command is `run`: the image's entrypoint, followed by `run`, or by
    /// the image's command when `run` is empty.
    pub(crate) fn argv(&self, run: &[String]) -> Vec<String> {
        let rest = if run.is_empty() { &self.cmd } else { run };
        self.entrypoint.iter().chain(rest).cloned().collect()
    }
}

#[cfg(test)]
impl ImageCommand {
    /// Returns the image command with `entrypoint` and `cmd`.
    pub(crate) fn new(entrypoint: &[&str], cmd: &[&str]) -> Self {
        Self {
            entrypoint: entrypoint.iter().map(|part| (*part).to_owned()).collect(),
            cmd: cmd.iter().map(|part| (*part).to_owned()).collect(),
        }
    }
}
