//! Which file system, or which mount of one, a directory lies on.
//!
//! On macOS a mount always has its own device, so the device number from
//! `stat` tells two file systems apart. On Linux a bind mount of a
//! directory on the same file system shares that file system's device, so
//! mxv also reads the mount ID that `statx` reports with `STATX_MNT_ID`,
//! which Linux 5.8 and later give. Where `statx` or the mount ID is
//! unavailable, the device alone decides.

use std::ffi::OsStr;
use std::os::fd::OwnedFd;

use rustix::fs::{Dev, Stat};

/// The file system and mount that one directory lies on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Volume {
    /// The device number from `stat`.
    pub(crate) device: Dev,
    /// The mount ID from `statx`, or `None` where the system gives none.
    pub(crate) mount: Option<u64>,
}

impl Volume {
    /// Reports whether `self` and `other` are the same mount of the same
    /// file system. Two directories with different devices differ. When
    /// both have a mount ID, the mount IDs must match too, which tells a
    /// bind mount apart from its source.
    pub(crate) fn same_file_system(self, other: Self) -> bool {
        if self.device != other.device {
            return false;
        }
        match (self.mount, other.mount) {
            (Some(mine), Some(theirs)) => mine == theirs,
            _ => true,
        }
    }
}

/// Finds the volume of an entry from the open directory it lies in, the
/// entry's name and its `stat`. The walk takes one as a parameter so that a
/// test can make a directory look like a mount point.
pub(crate) type Volumes = fn(&OwnedFd, &OsStr, &Stat) -> Volume;

/// Returns the volume of the entry `name` of the open directory `dir`,
/// whose `stat` the caller already read. An empty `name` means `dir`
/// itself.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn of(dir: &OwnedFd, name: &OsStr, stat: &Stat) -> Volume {
    use rustix::fs::{AtFlags, StatxFlags, statx};
    let flags = if name.is_empty() {
        AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW
    } else {
        AtFlags::SYMLINK_NOFOLLOW
    };
    let mount = statx(dir, name, flags, StatxFlags::MNT_ID)
        .ok()
        .filter(|found| found.stx_mask & StatxFlags::MNT_ID.bits() != 0)
        .map(|found| found.stx_mnt_id);
    Volume {
        device: stat.st_dev,
        mount,
    }
}

/// Returns the volume of the entry whose `stat` the caller already read.
/// A mount on macOS always has its own device, so the device decides.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) fn of(_dir: &OwnedFd, _name: &OsStr, stat: &Stat) -> Volume {
    Volume {
        device: stat.st_dev,
        mount: None,
    }
}

#[cfg(test)]
mod tests {
    use super::Volume;

    fn volume(device: u8, mount: Option<u64>) -> Volume {
        Volume {
            device: device.into(),
            mount,
        }
    }

    #[test]
    fn directories_on_different_devices_are_on_different_file_systems() {
        assert!(!volume(1, None).same_file_system(volume(2, None)));
        assert!(!volume(1, Some(7)).same_file_system(volume(2, Some(7))));
    }

    #[test]
    fn a_bind_mount_on_the_same_device_differs_by_its_mount_id() {
        assert!(!volume(1, Some(7)).same_file_system(volume(1, Some(8))));
        assert!(volume(1, Some(7)).same_file_system(volume(1, Some(7))));
    }

    #[test]
    fn the_device_alone_decides_when_either_mount_id_is_missing() {
        assert!(volume(1, None).same_file_system(volume(1, Some(8))));
        assert!(volume(1, Some(7)).same_file_system(volume(1, None)));
    }
}
