//! Unix retained-parent implementation for the common reported discovery loop.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;

use super::{ListingFailure, ListingFailureKind, WalkFailureKind};
use crate::directory_walk::open_dir_nofollow;
use crate::fd_relative::{
    open_at, open_dir_at, read_names_detailed, stat_at, stat_fd, DetailedListErrorKind,
};

#[derive(Debug)]
pub(super) struct FileState {
    parent: Arc<File>,
    leaf: OsString,
}

#[derive(Debug)]
pub(super) struct Dir {
    file: Arc<File>,
}

pub(super) struct Entry {
    name: OsString,
}

pub(super) struct DirectoryIdentity {
    dev: libc::dev_t,
    ino: libc::ino_t,
    mode: libc::mode_t,
}

pub(super) enum EntryKind {
    File,
    Directory(DirectoryIdentity),
    Link,
    Other,
}

impl Dir {
    pub(super) fn open_root(root: &Path) -> io::Result<Self> {
        Ok(Self {
            file: Arc::new(File::from(open_dir_nofollow(root)?)),
        })
    }

    pub(super) fn entries(&self, max: Option<usize>) -> Result<Vec<Entry>, ListingFailure> {
        read_names_detailed(&self.file, max)
            .map(|names| names.into_iter().map(|name| Entry { name }).collect())
            .map_err(|error| ListingFailure {
                observed: error.observed,
                kind: match error.kind {
                    DetailedListErrorKind::Io(source) => ListingFailureKind::Io(source),
                    DetailedListErrorKind::LimitExceeded { excess, .. } => {
                        ListingFailureKind::Limit { excess }
                    }
                },
            })
    }

    pub(super) fn classify(&self, entry: &Entry) -> io::Result<EntryKind> {
        let observed = stat_at(&self.file, &entry.name)?;
        Ok(match observed.st_mode & libc::S_IFMT {
            libc::S_IFREG => EntryKind::File,
            libc::S_IFDIR => EntryKind::Directory(DirectoryIdentity {
                dev: observed.st_dev,
                ino: observed.st_ino,
                mode: observed.st_mode,
            }),
            libc::S_IFLNK => EntryKind::Link,
            _ => EntryKind::Other,
        })
    }

    pub(super) fn open_child(
        &self,
        entry: &Entry,
        observed: DirectoryIdentity,
    ) -> Result<Self, WalkFailureKind> {
        let opened = open_dir_at(&self.file, &entry.name).map_err(WalkFailureKind::Io)?;
        let actual = stat_fd(&opened).map_err(WalkFailureKind::Io)?;
        if actual.st_dev != observed.dev
            || actual.st_ino != observed.ino
            || actual.st_mode != observed.mode
        {
            return Err(WalkFailureKind::EntryChanged);
        }
        Ok(Self {
            file: Arc::new(opened),
        })
    }

    pub(super) fn file(&self, entry: Entry) -> FileState {
        FileState {
            parent: Arc::clone(&self.file),
            leaf: entry.name,
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
        open_at(&self.parent, &self.leaf, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn child_open_compares_the_original_classification_not_a_fresh_stat() {
        let scratch = Scratch(std::env::temp_dir().join(format!(
            "khive-reported-child-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        )));
        fs::create_dir(&scratch.0).unwrap();
        fs::create_dir(scratch.0.join("child")).unwrap();
        let root = Dir::open_root(&scratch.0).unwrap();
        let entry = root.entries(None).unwrap().pop().unwrap();
        let repeated = root.entries(Some(1)).unwrap();
        assert_eq!(
            repeated.len(),
            1,
            "listing must not consume the held directory position"
        );
        assert_eq!(repeated[0].name(), entry.name());
        let EntryKind::Directory(observed) = root.classify(&entry).unwrap() else {
            panic!("fixture child must classify as a directory");
        };
        fs::rename(scratch.0.join("child"), scratch.0.join("original")).unwrap();
        fs::create_dir(scratch.0.join("child")).unwrap();
        assert!(matches!(
            root.open_child(&entry, observed),
            Err(WalkFailureKind::EntryChanged)
        ));
    }
}
