use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use super::{ListingFailure, ListingFailureKind, WalkFailureKind};

#[derive(Debug)]
pub(super) struct FileState {
    path: PathBuf,
}

pub(super) struct Dir {
    path: PathBuf,
}

pub(super) struct Entry {
    native: fs::DirEntry,
    name: OsString,
}

pub(super) struct DirectoryIdentity;

pub(super) enum EntryKind {
    File,
    Directory(DirectoryIdentity),
    Link,
    Other,
}

impl Dir {
    pub(super) fn open_root(root: &Path) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(root)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "walk root must be a directory and cannot be a symbolic link",
            ));
        }
        Ok(Self {
            path: root.to_path_buf(),
        })
    }

    pub(super) fn entries(&self, max: Option<usize>) -> Result<Vec<Entry>, ListingFailure> {
        let listing = fs::read_dir(&self.path).map_err(|error| ListingFailure {
            observed: 0,
            kind: ListingFailureKind::Io(error),
        })?;
        let mut entries = Vec::new();
        for native in listing {
            let native = native.map_err(|error| ListingFailure {
                observed: entries.len(),
                kind: ListingFailureKind::Io(error),
            })?;
            let name = native.file_name();
            if name == OsStr::new(".") || name == OsStr::new("..") {
                continue;
            }
            if max.is_some_and(|max| entries.len() == max) {
                return Err(ListingFailure {
                    observed: entries.len().saturating_add(1),
                    kind: ListingFailureKind::Limit { excess: name },
                });
            }
            entries.push(Entry { native, name });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    pub(super) fn classify(&self, entry: &Entry) -> io::Result<EntryKind> {
        let kind = entry.native.file_type()?;
        Ok(if kind.is_symlink() {
            EntryKind::Link
        } else if kind.is_dir() {
            EntryKind::Directory(DirectoryIdentity)
        } else if kind.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        })
    }

    pub(super) fn open_child(
        &self,
        entry: &Entry,
        _observed: DirectoryIdentity,
    ) -> Result<Self, WalkFailureKind> {
        Ok(Self {
            path: entry.native.path(),
        })
    }

    pub(super) fn file(&self, entry: Entry) -> FileState {
        FileState {
            path: entry.native.path(),
        }
    }
}

impl Entry {
    pub(super) fn name(&self) -> &OsStr {
        &self.name
    }
}

impl FileState {
    pub(super) fn open_read(&self) -> io::Result<File> {
        File::open(&self.path)
    }
}
