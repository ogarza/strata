// SPDX-License-Identifier: GPL-3.0-or-later

use std::{os::unix::fs::MetadataExt, path::Path};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SourceRevision {
    device: u64,
    inode: u64,
    pub(crate) size: u64,
    pub(crate) modified: i64,
    modified_ns: i64,
    changed: i64,
    changed_ns: i64,
}

impl SourceRevision {
    pub(crate) fn read(path: &Path) -> Result<Self, String> {
        let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
        Self::from_metadata(&metadata)
    }

    pub(crate) fn from_metadata(metadata: &std::fs::Metadata) -> Result<Self, String> {
        if !metadata.is_file() {
            return Err("Thumbnail input is not a regular file".to_owned());
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            modified: metadata.mtime(),
            modified_ns: metadata.mtime_nsec(),
            changed: metadata.ctime(),
            changed_ns: metadata.ctime_nsec(),
        })
    }

    pub(crate) fn cache_stamp(self) -> String {
        format!(
            "{}:{}:{}:{}:{}:{}:{}",
            self.device,
            self.inode,
            self.size,
            self.modified,
            self.modified_ns,
            self.changed,
            self.changed_ns
        )
    }

    pub(crate) fn matches(self, path: &Path) -> bool {
        Self::read(path).is_ok_and(|current| current == self)
    }
}

#[cfg(test)]
mod tests;
