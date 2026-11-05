//! The delivery of a container's secrets as files (decision P3): the
//! memory-only directory that holds them, the entrypoint wrapper that waits
//! for them, the scripts that write them inside the container, the owner
//! of the files, and the image's own command and user.
//!
//! The compose file of a process that receives its secrets as files mounts
//! a tmpfs at [`SECRETS_DIR`] with mode 0700, and replaces the image's
//! entrypoint with `/bin/sh -c <wrapper> phase-secrets <NAME>... --`. The
//! service's command is the image's own entrypoint followed by the plan's
//! command, or by the image's command when the plan has none, so the
//! wrapper ends by running what the container would have run without it.
//!
//! The wrapper needs `/bin/sh` and `sleep`. It waits until
//! `/run/secrets/<NAME>` exists for every name it was given, then replaces
//! itself with the command, so the app runs as the container's first
//! process and receives its stop signal. While it waits, the stop signals
//! that phase may send end it. It pauses a tenth of a second between
//! checks. It exits with [`WRAPPER_FAILED`] and one line on standard error
//! that names the missing file when a file has not appeared after
//! [`WAIT_SECONDS`], when the image has no `sleep`, and when it has no
//! command to run.
//!
//! phase writes the secrets of a container in two steps, each a `docker
//! exec --interactive --user 0` that runs a script of the container's
//! `/bin/sh`. First, [`stage`] runs once per secret, with the value's
//! length in bytes on the first line of its standard input and the value
//! after it. It writes the value to `/run/secrets/.<NAME>.new`, compares
//! the bytes written with the length using `wc -c`, gives the directory
//! and the file to the secrets' owner, and makes the file readable by its
//! owner alone. A staging that fails removes its file. Then [`commit`]
//! renames every staged file over its secret's file. A reader sees the old
//! value of a secret or the new one, and a staging that fails leaves every
//! secret file as it was, because nothing is renamed until each secret is
//! staged. [`discard`] removes the staged files after a failure. The
//! scripts need `/bin/sh`, `cat`, `wc`, `chown`, `chmod`, `mv` and `rm`,
//! which busybox and the base images of the common distributions provide.

use std::io;

use serde::Deserialize;

use crate::engine::container::compose::literal;
use crate::env::AppliedEnvironment;
use crate::plan::{AppPlan, ProcessPlan};

#[cfg(test)]
mod tests;

/// The directory inside a container that holds its secrets, one file per
/// secret, named after the variable.
pub(crate) const SECRETS_DIR: &str = "/run/secrets";

/// The mode of [`SECRETS_DIR`], which only its owner may enter.
pub(crate) const SECRETS_MODE: u32 = 0o700;

/// How long the wrapper waits for the secrets' files, in seconds.
pub(crate) const WAIT_SECONDS: u32 = 60;

/// The status with which the stage script exits when the first line of its
/// input is no length.
pub(crate) const NO_LENGTH: i32 = 65;

/// The status with which the stage script exits when the value's bytes
/// differ from its length.
pub(crate) const SHORT_FEED: i32 = 66;

/// The status with which the commit script exits when the file of a lapsed
/// optional secret remains after its removal failed.
pub(crate) const REMOVAL_REMAINED: i32 = 67;

/// The status with which the wrapper exits when it cannot start the app.
pub(crate) const WRAPPER_FAILED: i32 = 70;

/// The status with which a container exits when Docker cannot run its
/// entrypoint, such as a `/bin/sh` that the image lacks.
pub(crate) const NO_SHELL: i32 = 127;

/// The name the wrapper's shell gives itself, as `$0`.
const WRAPPER_NAME: &str = "phase-secrets";

/// The name the staging shell gives itself, as `$0`.
pub(crate) const STAGE_NAME: &str = "phase-secret-stage";

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
tenths=0
while [ "$#" -gt 0 ] && [ "$1" != '{END_OF_NAMES}' ]; do
  while [ ! -e "$d/$1" ]; do
    if ! command -v sleep >/dev/null 2>&1; then
      echo "phase: the image has no sleep program, which the wrapper needs to wait for $d/$1" >&2
      exit {WRAPPER_FAILED}
    fi
    if [ "$tenths" -ge {tenths} ]; then
      echo "phase: $d/$1 did not appear within {seconds} seconds" >&2
      exit {WRAPPER_FAILED}
    fi
    if sleep 0.1 2>/dev/null; then tenths=$((tenths + 1)); else sleep 1; tenths=$((tenths + 10)); fi
  done
  shift
done
if [ "$#" -gt 0 ]; then shift; fi
if [ "$#" -eq 0 ]; then
  echo 'phase: the container has no command to run' >&2
  exit {WRAPPER_FAILED}
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

/// Returns the script that stages the secret named by its first argument
/// in `dir` for the owner named by its second, from its standard input,
/// as the module docs say. It exits with [`NO_LENGTH`] when the first line
/// is no length, and [`SHORT_FEED`] when the value's bytes differ from the
/// length, with one line on standard error for a person who runs it by
/// hand. phase discards that line, as it discards everything an exec into
/// a container prints.
pub(crate) fn stage_script(dir: &str) -> String {
    format!(
        r#"set -eu
d='{dir}'
t="$d/.$1.new"
trap 'rm -f "$t"' EXIT
IFS= read -r n
case "$n" in
  ''|*[!0-9]*) echo "phase: the length of the secret $1 is not a number" >&2; exit {NO_LENGTH} ;;
esac
umask 077
cat > "$t"
m=$(wc -c < "$t")
if [ "$((m))" -ne "$n" ]; then
  echo "phase: the secret $1 arrived with $((m)) of $n bytes" >&2
  exit {SHORT_FEED}
fi
chown "$2" "$d" "$t"
chmod 0400 "$t"
trap - EXIT
"#
    )
}

/// The argument of the commit script that separates the secrets whose
/// staged files it renames from those whose files it removes. No variable
/// name is `--`, so it never names a secret.
const REMOVALS: &str = "--";

/// Returns the script that renames each staged file in `dir` named by its
/// arguments over its secret's file, and removes the file of each secret
/// named after [`REMOVALS`]. A removal that fails after the renames exits
/// with [`REMOVAL_REMAINED`], so the failure reaches the caller, and with
/// one line on standard error that phase discards.
fn commit_script(dir: &str) -> String {
    format!(
        r#"set -eu
d='{dir}'
r=
for n do
if [ "$n" = '{REMOVALS}' ]; then r=1; continue; fi
if [ -z "$r" ]; then mv -f "$d/.$n.new" "$d/$n"; continue; fi
rm -f "$d/$n" || {{ echo "phase: the file of the lapsed optional secret $n could not be removed and remains in $d" >&2; exit {REMOVAL_REMAINED}; }}
done
"#
    )
}

/// Returns the script that removes every staged file in `dir`.
fn discard_script(dir: &str) -> String {
    format!("rm -f '{dir}'/.*.new\n")
}

/// Returns the start of every `docker exec` that runs a script as root in
/// the container `id` with `/bin/sh -c`.
fn exec_as_root(id: &str, script: String) -> Vec<String> {
    let mut args: Vec<String> = ["exec", "--interactive", "--user", "0", "--", id, "/bin/sh"]
        .map(str::to_owned)
        .to_vec();
    args.push("-c".to_owned());
    args.push(script);
    args
}

/// Returns the arguments of `docker` that stage the secret `name` in the
/// container `id` for `owner`, with the framed value on standard input.
pub(crate) fn stage(id: &str, name: &str, owner: &str) -> Vec<String> {
    let mut args = exec_as_root(id, stage_script(SECRETS_DIR));
    args.extend([STAGE_NAME, name, owner].map(str::to_owned));
    args
}

/// Returns the arguments of `docker` that rename the staged files of
/// `names` in the container `id` over their secrets' files, and remove
/// the files of `removed`, in one script.
pub(crate) fn commit<'n>(
    id: &str,
    names: impl Iterator<Item = &'n String>,
    removed: impl Iterator<Item = &'n String>,
) -> Vec<String> {
    let mut args = exec_as_root(id, commit_script(SECRETS_DIR));
    args.push("phase-secret-commit".to_owned());
    args.extend(names.cloned());
    args.push(REMOVALS.to_owned());
    args.extend(removed.cloned());
    args
}

/// Returns the arguments of `docker` that remove the staged files in the
/// container `id`.
pub(crate) fn discard(id: &str) -> Vec<String> {
    let mut args = exec_as_root(id, discard_script(SECRETS_DIR));
    args.push("phase-secret-discard".to_owned());
    args
}

/// The bytes of one secret's value, framed for [`stage`]: its length in
/// decimal and a newline, then the value. It implements neither `Debug`
/// nor `Display`, has no accessor but [`SecretBytes::framed`], and
/// overwrites its bytes with zeros when it is dropped.
pub(crate) struct SecretBytes(Vec<u8>);

impl SecretBytes {
    /// Returns the framed value of the secret `name` of `environment`.
    ///
    /// # Errors
    ///
    /// Returns the error of [`AppliedEnvironment::write_value`].
    pub(crate) fn of(environment: &dyn AppliedEnvironment, name: &str) -> io::Result<Self> {
        let mut value = ZeroingBuffer(Vec::new());
        let written = environment.write_value(name, &mut value);
        let prefix = format!("{}\n", value.0.len());
        let mut framed = Vec::with_capacity(prefix.len() + value.0.len());
        framed.extend_from_slice(prefix.as_bytes());
        framed.extend_from_slice(&value.0);
        let framed = Self(framed);
        drop(value);
        written.map(|()| framed)
    }

    /// Returns the bytes to write to the staging script's standard input.
    pub(crate) fn framed(&self) -> &[u8] {
        &self.0
    }

    /// Returns the bytes with the value cut short by `missing` bytes,
    /// which a test feeds to show that a cut feed fails.
    #[cfg(test)]
    pub(crate) fn cut(mut self, missing: usize) -> Self {
        let keep = self.0.len().saturating_sub(missing);
        self.0.truncate(keep);
        self
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        zero(&mut self.0);
    }
}

/// Overwrites `bytes` with zeros, which the compiler may not leave out,
/// because the zeroed bytes are then read through [`std::hint::black_box`].
fn zero(bytes: &mut [u8]) {
    bytes.fill(0);
    std::hint::black_box(&*bytes);
}

/// The buffer that an environment writes a secret's value into. When a
/// write needs more room, it copies the bytes into a larger buffer and
/// overwrites the old one with zeros, so no copy of the value is left in
/// memory that the allocator freed. It overwrites its last buffer with
/// zeros when it is dropped. The zeroing is best effort: the environment
/// itself, the operating system's pipe buffers and the `docker` program
/// may hold their own copies.
struct ZeroingBuffer(Vec<u8>);

impl io::Write for ZeroingBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let needed = self.0.len() + bytes.len();
        if needed > self.0.capacity() {
            let mut larger = Vec::with_capacity(needed.max(self.0.capacity() * 2).max(64));
            larger.extend_from_slice(&self.0);
            zero(&mut self.0);
            self.0 = larger;
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for ZeroingBuffer {
    fn drop(&mut self) {
        zero(&mut self.0);
    }
}

/// The entrypoint, command and user of an image, as `docker image
/// inspect` reports them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct ImageConfig {
    /// The image's entrypoint.
    #[serde(default, deserialize_with = "or_empty")]
    entrypoint: Vec<String>,
    /// The image's command.
    #[serde(default, deserialize_with = "or_empty")]
    cmd: Vec<String>,
    /// The image's user, in Docker's `USER[:GROUP]` form, which is empty
    /// for root.
    #[serde(default)]
    user: String,
}

/// Reads a list that Docker prints as `null` when it is empty.
fn or_empty<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<String>>::deserialize(deserializer)?.unwrap_or_default())
}

/// The Go template of `docker image inspect` whose output
/// [`ImageConfig::parse`] reads.
pub(crate) const IMAGE_CONFIG_FORMAT: &str = r#"{"entrypoint":{{json .Config.Entrypoint}},"cmd":{{json .Config.Cmd}},"user":{{json .Config.User}}}"#;

impl ImageConfig {
    /// Returns the image config that `docker image inspect` printed with
    /// [`IMAGE_CONFIG_FORMAT`], or `None` when it printed something else.
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
impl ImageConfig {
    /// Returns the image config with `entrypoint`, `cmd` and `user`.
    pub(crate) fn new(entrypoint: &[&str], cmd: &[&str], user: &str) -> Self {
        Self {
            entrypoint: entrypoint.iter().map(|part| (*part).to_owned()).collect(),
            cmd: cmd.iter().map(|part| (*part).to_owned()).collect(),
            user: user.to_owned(),
        }
    }
}

/// Reports whether `user`, in Docker's `USER[:GROUP]` form, names root:
/// an empty user, `0` or `root`.
fn is_root(user: &str) -> bool {
    let name = user.split(':').next().unwrap_or_default().trim();
    name.is_empty() || name == "0" || name == "root"
}

/// Returns the owner of the secret files of `process` of `app`, in the
/// form that `chown` reads: the process's `secret_owner`, or else the
/// app's run-as user, or else the user of `image` when the app names
/// none. It returns `None` when the files would belong to
/// root and the plan did not say so with `secret_owner`.
pub(crate) fn owner(app: &AppPlan, process: &ProcessPlan, image: &ImageConfig) -> Option<String> {
    container_owner(
        process,
        app.container.user.as_deref().unwrap_or(&image.user),
    )
}

/// Returns the owner of the secret files of `process` in a container that
/// runs as `user`, in Docker's `USER[:GROUP]` form: the process's
/// `secret_owner`, or else `user`. It returns `None` when the files would
/// belong to root and the plan did not say so with `secret_owner`.
///
/// A rewrite passes the user that the running container was created
/// with, so the files keep the owner that the start gave them, even when
/// the image's tag has since moved to an image with another user.
pub(crate) fn container_owner(process: &ProcessPlan, user: &str) -> Option<String> {
    if let Some(owner) = &process.secret_owner {
        return Some(owner.clone());
    }
    (!is_root(user)).then(|| user.to_owned())
}
