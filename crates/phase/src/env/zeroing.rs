//! Memory that phase overwrites with zeros once it no longer needs a value.

use std::io;

/// Overwrites `bytes` with zeros, which the compiler may not leave out,
/// because the zeroed bytes are then read through [`std::hint::black_box`].
pub(crate) fn zero(bytes: &mut [u8]) {
    bytes.fill(0);
    std::hint::black_box(&*bytes);
}

/// The buffer that an environment writes a value into. When a write needs
/// more room, it copies the bytes into a larger buffer and overwrites the
/// old one with zeros, so no copy of the value is left in memory that the
/// allocator freed. It overwrites its last buffer with zeros when it is
/// dropped. The zeroing is best effort: the environment itself, the
/// operating system's pipe buffers and the `docker` program may hold their
/// own copies.
#[derive(Default)]
pub(crate) struct ZeroingBuffer(Vec<u8>);

impl ZeroingBuffer {
    /// Returns the bytes written so far.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.0
    }
}

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
