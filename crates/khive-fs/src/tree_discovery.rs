//! Breadth-first, no-follow discovery with progress and deferred file admission.
//!
//! Unix results retain the observed containing directory and open one leaf with
//! no-follow and nonblocking flags. Other platforms retain a pathname and offer
//! best-effort link observation only. Neither mode freezes leaf identity/content.

#[cfg(not(unix))]
mod portable;
#[cfg(unix)]
mod unix;
#[cfg(not(unix))]
use portable as platform;
#[cfg(unix)]
use unix as platform;

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// Refusal limits for the complete walk; the root has depth zero and is uncounted.
#[derive(Debug, Clone, Copy)]
pub struct ReportedWalkLimits {
    pub max_depth: usize,
    pub max_entries: usize,
    pub max_files: usize,
}

/// Saturating counts, including the attempted entry/file on a limit refusal.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WalkProgress {
    pub entries_visited: usize,
    pub files_discovered: usize,
    pub files_skipped: usize,
    pub links_skipped: usize,
}

/// A complete successful discovery, sorted by relative path.
#[derive(Debug)]
pub struct WalkReport {
    pub files: Vec<WalkFile>,
    pub progress: WalkProgress,
}

/// A selected relative name together with its private platform admission state.
#[derive(Debug)]
pub struct WalkFile {
    relative: PathBuf,
    state: platform::FileState,
}

impl WalkFile {
    /// The observed, normal-component-only name below the root.
    pub fn relative_path(&self) -> &Path {
        &self.relative
    }

    /// Open a fresh read handle. A replacement regular leaf is permitted.
    ///
    /// Unix refuses a final symlink and uses NONBLOCK; other platforms use an
    /// ordinary pathname open. Inspect the returned handle before reading: this
    /// method does not require the opened object to remain a regular file.
    pub fn open_read(&self) -> io::Result<File> {
        self.state.open_read()
    }
}

/// Native I/O failure or a refusal to return an incomplete discovery.
#[derive(Debug)]
pub enum WalkFailureKind {
    Io(io::Error),
    EntryLimit,
    DepthLimit,
    FileLimit,
    EntryChanged,
}

/// Failure context; an empty relative path identifies the root itself.
#[derive(Debug)]
pub struct WalkFailure {
    pub path: PathBuf,
    pub depth: usize,
    pub progress: WalkProgress,
    pub kind: WalkFailureKind,
}

impl fmt::Display for WalkFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match &self.kind {
            WalkFailureKind::Io(_) => "I/O error",
            WalkFailureKind::EntryLimit => "entry limit",
            WalkFailureKind::DepthLimit => "depth limit",
            WalkFailureKind::FileLimit => "file limit",
            WalkFailureKind::EntryChanged => "entry changed",
        };
        write!(
            f,
            "tree discovery {kind} at {:?}, depth={}, progress={:?}",
            self.path, self.depth, self.progress
        )?;
        if let WalkFailureKind::Io(error) = &self.kind {
            write!(f, ": {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for WalkFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            WalkFailureKind::Io(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(super) struct ListingFailure {
    pub(super) observed: usize,
    pub(super) kind: ListingFailureKind,
}

#[derive(Debug)]
pub(super) enum ListingFailureKind {
    Io(io::Error),
    Limit { excess: OsString },
}

fn remaining_entry_budget(max: usize, visited: usize) -> Option<usize> {
    // The consumer's saturating count can never exceed usize::MAX.
    (max != usize::MAX).then(|| max.saturating_sub(visited))
}

fn failure(
    path: PathBuf,
    depth: usize,
    progress: WalkProgress,
    kind: WalkFailureKind,
) -> WalkFailure {
    WalkFailure {
        path,
        depth,
        progress,
        kind,
    }
}

/// Discover regular files without following observed links, or refuse the whole walk.
///
/// Each directory is completely listed before its entries are classified in sorted
/// name order. All raw non-dot names count. The filter runs once per observed
/// regular file, including one that then exceeds max_files; directories, links,
/// special files and partially listed directories never reach it. A refusal
/// returns no partial file collection and stops further callbacks.
///
/// Root spelling is normalized lexically so a trailing separator or `/.` cannot
/// bypass final-root-link refusal. Earlier root-path links resolve normally.
/// Unix pins child directories when enqueued, which may surface open errors
/// before their later FIFO visit. Non-Unix retains best-effort pathname traversal.
pub fn walk_tree_reported<F>(
    root: &Path,
    limits: ReportedWalkLimits,
    mut accept_file: F,
) -> Result<WalkReport, WalkFailure>
where
    F: FnMut(&Path) -> bool,
{
    let root: PathBuf = root.components().collect();
    let mut progress = WalkProgress::default();
    if root.as_os_str().is_empty() {
        return Err(failure(
            PathBuf::new(),
            0,
            progress,
            WalkFailureKind::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty walk root",
            )),
        ));
    }
    let root = platform::Dir::open_root(&root)
        .map_err(|error| failure(PathBuf::new(), 0, progress, WalkFailureKind::Io(error)))?;
    let mut directories = VecDeque::from([(PathBuf::new(), 0usize, root)]);
    let mut files = Vec::new();
    while let Some((relative, depth, directory)) = directories.pop_front() {
        let entries = match directory.entries(remaining_entry_budget(
            limits.max_entries,
            progress.entries_visited,
        )) {
            Ok(entries) => entries,
            Err(error) => {
                progress.entries_visited = progress.entries_visited.saturating_add(error.observed);
                let (path, kind) = match error.kind {
                    ListingFailureKind::Io(error) => (relative, WalkFailureKind::Io(error)),
                    ListingFailureKind::Limit { excess } => {
                        (relative.join(excess), WalkFailureKind::EntryLimit)
                    }
                };
                return Err(failure(path, depth, progress, kind));
            }
        };
        progress.entries_visited = progress.entries_visited.saturating_add(entries.len());
        for entry in entries {
            let path = relative.join(entry.name());
            let kind = directory.classify(&entry).map_err(|error| {
                failure(path.clone(), depth, progress, WalkFailureKind::Io(error))
            })?;
            match kind {
                platform::EntryKind::Directory(observed) => {
                    let child_depth = depth.saturating_add(1);
                    if child_depth > limits.max_depth {
                        return Err(failure(
                            path,
                            child_depth,
                            progress,
                            WalkFailureKind::DepthLimit,
                        ));
                    }
                    let child = directory
                        .open_child(&entry, observed)
                        .map_err(|kind| failure(path.clone(), depth, progress, kind))?;
                    directories.push_back((path, child_depth, child));
                }
                platform::EntryKind::Link => {
                    progress.links_skipped = progress.links_skipped.saturating_add(1);
                    progress.files_skipped = progress.files_skipped.saturating_add(1);
                }
                platform::EntryKind::File if accept_file(&path) => {
                    progress.files_discovered = progress.files_discovered.saturating_add(1);
                    if progress.files_discovered > limits.max_files {
                        return Err(failure(path, depth, progress, WalkFailureKind::FileLimit));
                    }
                    files.push(WalkFile {
                        relative: path,
                        state: directory.file(entry),
                    });
                }
                platform::EntryKind::File | platform::EntryKind::Other => {
                    progress.files_skipped = progress.files_skipped.saturating_add(1);
                }
            }
        }
    }
    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(WalkReport { files, progress })
}

#[cfg(test)]
mod tests {
    use super::remaining_entry_budget;

    #[test]
    fn finite_entry_caps_keep_zero_and_last_representable_budget() {
        assert_eq!(remaining_entry_budget(0, 0), Some(0));
        assert_eq!(remaining_entry_budget(7, 3), Some(4));
        assert_eq!(remaining_entry_budget(7, 7), Some(0));
        assert_eq!(
            remaining_entry_budget(usize::MAX - 1, usize::MAX - 2),
            Some(1)
        );
        assert_eq!(
            remaining_entry_budget(usize::MAX - 1, usize::MAX - 1),
            Some(0)
        );
    }

    #[test]
    fn maximum_entry_cap_preserves_saturating_progress_without_local_refusal() {
        for visited in [0, 1, usize::MAX - 1, usize::MAX] {
            assert_eq!(remaining_entry_budget(usize::MAX, visited), None);
        }
    }
}
