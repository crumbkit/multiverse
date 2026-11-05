//! phase's keyed digest of the values of a process's variables that are not
//! secrets, and the key file that it is keyed with.
//!
//! The digest lets `up` notice that a value changed, such as LocalStack's
//! `SERVICES` going from `s3` to `s3,sqs`, without phase keeping the value.
//! It is HMAC-SHA-256, keyed with 32 random bytes that phase keeps in
//! [`KEY_FILE`] under its state directory, over this encoding of the
//! variables in name order:
//!
//! - the domain `phase/env-digest/v1` and a zero byte;
//! - the number of variables, as 8 big-endian bytes;
//! - for each variable, its name's length in 8 big-endian bytes and its
//!   name, then the byte 0 when the environment lacks the variable, or the
//!   byte 1, the value's length in 8 big-endian bytes and the value.
//!
//! Each length comes before its bytes, so no two lists of names and values
//! share an encoding, and an absent variable never encodes as an empty
//! one. A new encoding changes the domain's version.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::Path;

use hmac::{Hmac, KeyInit as _, Mac as _};
use rustix::fs::{Mode, OFlags};
use sha2::Sha256;

use crate::env::AppliedEnvironment;
use crate::env::zeroing::{ZeroingBuffer, zero};
use crate::error::PhaseError;

#[cfg(test)]
mod tests;

/// The number of bytes of a key and of a digest.
const BYTES: usize = 32;

/// The file under phase's state directory that holds the key.
pub(crate) const KEY_FILE: &str = "env-digest.key";

/// The domain of the encoding, which names its version.
const DOMAIN: &[u8] = b"phase/env-digest/v1\0";

/// The byte that marks a variable that the environment lacks.
const ABSENT: u8 = 0;

/// The byte that marks a variable that the environment has.
const PRESENT: u8 = 1;

/// The key of an [`EnvDigest`]: 32 random bytes that phase reads from its
/// state directory. It has no accessor outside phase, its `Debug` prints
/// none of its bytes, and it overwrites its bytes with zeros when it is
/// dropped.
pub struct DigestKey([u8; BYTES]);

impl DigestKey {
    /// Returns the key whose bytes are `bytes`, for a test.
    #[cfg(test)]
    pub(crate) const fn from_bytes(bytes: [u8; BYTES]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for DigestKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DigestKey(<redacted>)")
    }
}

impl Drop for DigestKey {
    fn drop(&mut self) {
        zero(&mut self.0);
    }
}

/// A keyed digest of some of a process's variables and their values, which
/// [`AppliedEnvironment::digest_of`] returns. Equal names and values give
/// equal digests under one key, and without the key the digest reveals
/// nothing about a value. Only phase makes one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct EnvDigest([u8; BYTES]);

impl EnvDigest {
    /// Returns the digest's bytes.
    pub(crate) const fn bytes(&self) -> &[u8; BYTES] {
        &self.0
    }
}

impl fmt::Debug for EnvDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("EnvDigest(")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        formatter.write_str(")")
    }
}

/// Feeds `bytes` to `mac` after their length.
fn framed(mac: &mut Hmac<Sha256>, bytes: &[u8]) {
    mac.update(&(bytes.len() as u64).to_be_bytes());
    mac.update(bytes);
}

/// Returns the digest of the variables `names` of `environment` under
/// `key`, as the module docs describe it.
///
/// # Errors
///
/// Returns the error of [`AppliedEnvironment::write_value`] for a variable
/// that the environment has, with its kind, such as
/// [`io::ErrorKind::Unsupported`], and a message that names the variable.
pub(crate) fn walk<E: AppliedEnvironment + ?Sized>(
    environment: &E,
    names: &BTreeSet<String>,
    key: &DigestKey,
) -> io::Result<EnvDigest> {
    let present = environment.names();
    let mut mac = Hmac::<Sha256>::new_from_slice(&key.0).map_err(io::Error::other)?;
    mac.update(DOMAIN);
    mac.update(&(names.len() as u64).to_be_bytes());
    for name in names {
        framed(&mut mac, name.as_bytes());
        if !present.contains(name) {
            mac.update(&[ABSENT]);
            continue;
        }
        mac.update(&[PRESENT]);
        let mut value = ZeroingBuffer::default();
        environment.write_value(name, &mut value).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("the environment could not write the value of {name} for its digest: {error}"),
            )
        })?;
        framed(&mut mac, value.bytes());
    }
    Ok(EnvDigest(mac.finalize().into_bytes().into()))
}

/// Returns the key under `state_dir`, after creating it on first use.
///
/// The key file is [`KEY_FILE`]. phase creates it with mode 0600 from 32
/// bytes of the operating system's random device: it writes a temporary
/// file that it creates new, without following a symbolic link, and links
/// it to the key file's name, which fails when another `up` linked its own
/// first, so every reader sees a whole key. phase refuses a key file that
/// is a symbolic link, is not a regular file, belongs to another user, can
/// be read or written by its group or by others, or does not hold exactly
/// 32 bytes. It never repairs or replaces such a file.
///
/// # Errors
///
/// Returns [`PhaseError::StateUnwritable`] that names the key file, whose
/// source says why phase cannot use it.
pub(crate) fn load_key(state_dir: &Path) -> Result<DigestKey, PhaseError> {
    let path = state_dir.join(KEY_FILE);
    let unusable = |source| PhaseError::StateUnwritable {
        path: path.clone(),
        source,
    };
    match open_key(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_key(state_dir, &path).map_err(unusable)?;
            open_key(&path).map_err(unusable)
        }
        opened => opened.map_err(unusable),
    }
}

/// Reads the key file at `path`, as [`load_key`] says.
fn open_key(path: &Path) -> io::Result<DigestKey> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut file = match rustix::fs::open(path, flags, Mode::empty()) {
        Ok(fd) => File::from(fd),
        Err(rustix::io::Errno::LOOP) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the key file is a symbolic link, which phase never follows",
            ));
        }
        Err(errno) => return Err(errno.into()),
    };
    check_key_file(&file.metadata()?, rustix::process::geteuid().as_raw())?;
    let mut bytes = [0; BYTES];
    let read = file.read_exact(&mut bytes);
    let key = DigestKey(bytes);
    zero(&mut bytes);
    read.map(|()| key)
}

/// Checks the metadata of an open key file against [`load_key`]'s rules,
/// for the user `uid` that runs phase.
fn check_key_file(metadata: &std::fs::Metadata, uid: u32) -> io::Result<()> {
    let refuse = |reason: String| Err(io::Error::new(io::ErrorKind::PermissionDenied, reason));
    if !metadata.file_type().is_file() {
        return refuse("the key file is not a regular file".to_owned());
    }
    if metadata.uid() != uid {
        return refuse(format!(
            "the key file belongs to the user {} and phase runs as the user {uid}",
            metadata.uid()
        ));
    }
    let mode = metadata.mode() & 0o777;
    if mode & 0o077 != 0 {
        return refuse(format!(
            "the key file has mode {mode:04o}, which lets its group or others read or write it, and phase needs 0600"
        ));
    }
    if metadata.len() != BYTES as u64 {
        return refuse(format!(
            "the key file holds {} bytes and a key is {BYTES} bytes",
            metadata.len()
        ));
    }
    Ok(())
}

/// Creates the key file at `path` in `state_dir`, as [`load_key`] says. A
/// key file that another `up` created first is kept.
fn create_key(state_dir: &Path, path: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state_dir)?;
    let mut bytes = crate::random::bytes::<BYTES>()?;
    let temp = state_dir.join(format!("{KEY_FILE}.{}.new", std::process::id()));
    // A temporary file that a crashed `up` of this process id left is
    // removed, so the exclusive create below never meets it.
    match std::fs::remove_file(&temp) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let written = rustix::fs::open(&temp, flags, Mode::from_raw_mode(0o600))
        .map_err(io::Error::from)
        .and_then(|fd| {
            let mut file = File::from(fd);
            file.write_all(&bytes)?;
            file.sync_all()
        });
    zero(&mut bytes);
    let linked = written.and_then(|()| match std::fs::hard_link(&temp, path) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => Err(error),
        _ => Ok(()),
    });
    let removed = std::fs::remove_file(&temp);
    linked.and(removed)
}
