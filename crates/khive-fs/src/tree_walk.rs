//! Bounded enumeration beneath a pinned directory, without pathname opens of descendants.

use std::collections::{BTreeSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::directory_walk::{open_dir_nofollow, read_link_at};
use crate::fd_relative::{list_names_raw_bounded, open_dir_at, stat_at, stat_fd, ListNamesError};
use crate::opened_file::opened_file_path;

/// Maximum symlink expansions across one entire walk, including link-to-link targets.
pub const TREE_LINK_BUDGET: usize = 40;

/// Explicit traversal limits. Callers choose their own limits and filtering policy.
#[derive(Debug, Clone, Copy)]
pub struct WalkLimits {
    pub max_depth: usize,
    pub max_entries: usize,
    /// False skips every symlink. True permits targets confined to the pinned root.
    pub follow_symlinks_within_root: bool,
}

/// An observed relative name containing only normal components, preserving Unix name bytes.
///
/// This is not an open handle: a later filesystem mutation can change what the name resolves to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RelPath(PathBuf);

impl RelPath {
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for RelPath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

/// The resolved entry kind supplied to the caller's filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkEntryKind {
    RegularFile,
    Directory,
    Other,
}

/// A walk refused its bounds, a changing entry, or a target outside the pinned root.
#[derive(Debug)]
pub enum WalkError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    EntriesExceeded {
        max_entries: usize,
    },
    DepthExceeded {
        path: RelPath,
        max_depth: usize,
    },
    OutsideRoot {
        path: RelPath,
    },
    RootChanged {
        path: PathBuf,
        source: Option<io::Error>,
    },
    EntryChanged {
        path: RelPath,
    },
    LinkBudgetExceeded {
        path: RelPath,
        max_links: usize,
    },
}

impl fmt::Display for WalkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(formatter, "directory walk at {path:?}: {source}"),
            Self::EntriesExceeded { max_entries } => {
                write!(formatter, "tree entry limit {max_entries} exceeded")
            }
            Self::DepthExceeded { path, max_depth } => write!(
                formatter,
                "tree depth limit {max_depth} exceeded at {:?}",
                path.as_path()
            ),
            Self::OutsideRoot { path } => write!(
                formatter,
                "link target escapes pinned root at {:?}",
                path.as_path()
            ),
            Self::RootChanged { path, .. } => write!(
                formatter,
                "named root no longer identifies the pinned root: {path:?}"
            ),
            Self::EntryChanged { path } => write!(
                formatter,
                "entry changed during directory walk: {:?}",
                path.as_path()
            ),
            Self::LinkBudgetExceeded { path, max_links } => write!(
                formatter,
                "tree symlink limit {max_links} exceeded at {:?}",
                path.as_path()
            ),
        }
    }
}

impl std::error::Error for WalkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. }
            | Self::RootChanged {
                source: Some(source),
                ..
            } => Some(source),
            _ => None,
        }
    }
}

fn io_at(path: &Path, source: io::Error) -> WalkError {
    WalkError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn kind(stat: &libc::stat) -> WalkEntryKind {
    match stat.st_mode & libc::S_IFMT {
        libc::S_IFREG => WalkEntryKind::RegularFile,
        libc::S_IFDIR => WalkEntryKind::Directory,
        _ => WalkEntryKind::Other,
    }
}

type Identity = (libc::dev_t, libc::ino_t);

fn identity(stat: &libc::stat) -> Identity {
    (stat.st_dev, stat.st_ino)
}

fn same_entry(before: &libc::stat, after: &libc::stat) -> bool {
    identity(before) == identity(after) && before.st_mode == after.st_mode
}

/// Check the actual opened object, including intermediate link-target directories.
fn open_checked(
    parent: &File,
    name: &OsStr,
    before: &libc::stat,
    path: &RelPath,
) -> Result<Arc<File>, WalkError> {
    let opened = open_dir_at(parent, name).map_err(|error| io_at(path.as_path(), error))?;
    let after = stat_fd(&opened).map_err(|error| io_at(path.as_path(), error))?;
    if !same_entry(before, &after) {
        return Err(WalkError::EntryChanged { path: path.clone() });
    }
    Ok(Arc::new(opened))
}

struct Root {
    file: Arc<File>,
    identity: Identity,
    spellings: Vec<PathBuf>,
}

impl Root {
    fn open(path: &Path) -> Result<Self, WalkError> {
        if path.as_os_str().is_empty() {
            return Err(io_at(
                path,
                io::Error::new(io::ErrorKind::InvalidInput, "empty tree root"),
            ));
        }
        // A trailing slash or /. must not turn the final root symlink into an ancestor.
        let path: PathBuf = path.components().collect();
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            std::env::current_dir()
                .map_err(|error| io_at(&path, error))?
                .join(&path)
        };
        let file = File::from(open_dir_nofollow(&path).map_err(|error| io_at(&path, error))?);
        let identity = identity(&stat_fd(&file).map_err(|error| io_at(&path, error))?);
        let mut spellings = vec![absolute];
        if let Ok(opened) = opened_file_path(&file) {
            if opened.is_absolute() && !spellings.contains(&opened) {
                spellings.push(opened);
            }
        }
        Ok(Self {
            file: Arc::new(file),
            identity,
            spellings,
        })
    }

    fn absolute_suffix(&self, target: &Path, path: &RelPath) -> Result<PathBuf, WalkError> {
        for spelling in &self.spellings {
            let Ok(suffix) = target.strip_prefix(spelling) else {
                continue;
            };
            // Prefix matching supplies no authority. Revalidate only the root spelling;
            // the suffix will be opened exclusively through the original held root.
            let named = open_dir_nofollow(spelling)
                .map(File::from)
                .map_err(|source| WalkError::RootChanged {
                    path: spelling.clone(),
                    source: Some(source),
                })?;
            let observed = stat_fd(&named).map_err(|error| io_at(spelling, error))?;
            if identity(&observed) != self.identity {
                return Err(WalkError::RootChanged {
                    path: spelling.clone(),
                    source: None,
                });
            }
            return Ok(suffix.to_path_buf());
        }
        Err(WalkError::OutsideRoot { path: path.clone() })
    }
}

fn components(path: &Path) -> VecDeque<OsString> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_os_string()),
            Component::ParentDir => Some(OsString::from("..")),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => None,
        })
        .collect()
}

struct Resolved {
    kind: WalkEntryKind,
    directories: Vec<Arc<File>>,
    identity: Option<Identity>,
}

fn resolve_link(
    root: &Root,
    mut directories: Vec<Arc<File>>,
    name: OsString,
    initial: &libc::stat,
    path: &RelPath,
    links_left: &mut usize,
) -> Result<Resolved, WalkError> {
    let mut remaining = VecDeque::from([name]);
    let mut first = true;
    while let Some(name) = remaining.pop_front() {
        if name == OsStr::new(".") {
            continue;
        }
        if name == OsStr::new("..") {
            if directories.len() == 1 {
                return Err(WalkError::OutsideRoot { path: path.clone() });
            }
            directories.pop();
            continue;
        }
        let parent = directories.last().expect("root directory is always pinned");
        let before = stat_at(parent, &name).map_err(|error| io_at(path.as_path(), error))?;
        if first && !same_entry(initial, &before) {
            return Err(WalkError::EntryChanged { path: path.clone() });
        }
        first = false;
        if before.st_mode & libc::S_IFMT == libc::S_IFLNK {
            *links_left =
                links_left
                    .checked_sub(1)
                    .ok_or_else(|| WalkError::LinkBudgetExceeded {
                        path: path.clone(),
                        max_links: TREE_LINK_BUDGET,
                    })?;
            let target =
                read_link_at(parent, &name).map_err(|error| io_at(path.as_path(), error))?;
            let after = stat_at(parent, &name).map_err(|error| io_at(path.as_path(), error))?;
            if !same_entry(&before, &after) {
                return Err(WalkError::EntryChanged { path: path.clone() });
            }
            // Capture this before strip_prefix/components normalize away a trailing
            // slash or /.; either spelling requires the endpoint to be a directory.
            let bytes = target.as_os_str().as_bytes();
            let requires_directory = bytes.ends_with(b"/") || bytes.ends_with(b"/.");
            let target = if target.is_absolute() {
                let suffix = root.absolute_suffix(&target, path)?;
                directories = vec![Arc::clone(&root.file)];
                suffix
            } else {
                target
            };
            let mut target_components = components(&target);
            if requires_directory {
                target_components.push_back(OsString::from("."));
            }
            for component in target_components.into_iter().rev() {
                remaining.push_front(component);
            }
        } else if kind(&before) == WalkEntryKind::Directory {
            directories.push(open_checked(parent, &name, &before, path)?);
        } else if remaining.is_empty() {
            return Ok(Resolved {
                kind: kind(&before),
                directories,
                identity: None,
            });
        } else {
            return Err(io_at(
                path.as_path(),
                io::Error::from_raw_os_error(libc::ENOTDIR),
            ));
        }
    }
    let final_stat = stat_fd(directories.last().expect("root directory is always pinned"))
        .map_err(|error| io_at(path.as_path(), error))?;
    Ok(Resolved {
        kind: WalkEntryKind::Directory,
        directories,
        identity: Some(identity(&final_stat)),
    })
}

struct Frame {
    directories: Vec<Arc<File>>,
    path: PathBuf,
    depth: usize,
    names: std::vec::IntoIter<OsString>,
}

fn frame(
    directories: Vec<Arc<File>>,
    path: PathBuf,
    depth: usize,
    entries_left: &mut usize,
    max_entries: usize,
) -> Result<Frame, WalkError> {
    let names = list_names_raw_bounded(directories.last().expect("root is pinned"), *entries_left)
        .map_err(|error| match error {
            ListNamesError::Io(source) => io_at(&path, source),
            ListNamesError::LimitExceeded { .. } => WalkError::EntriesExceeded { max_entries },
        })?;
    *entries_left = entries_left
        .checked_sub(names.len())
        .ok_or(WalkError::EntriesExceeded { max_entries })?;
    Ok(Frame {
        directories,
        path,
        depth,
        names: names.into_iter(),
    })
}

/// Enumerate accepted regular files beneath a held root, returning sorted relative names.
///
/// The root is depth zero and is not counted or filtered. Files in it are allowed at depth
/// zero; an accepted subdirectory exceeding `max_depth` refuses the whole walk. Every listed
/// non-dot name counts before filtering, including non-UTF-8 names, links and hidden entries.
/// A false directory filter prunes descent. No hidden-name or file-extension policy is built in.
///
/// Never-follow mode skips links. Follow mode resolves links before invoking the filter once
/// with their resolved kind; outside-root targets therefore refuse before filtering. A global
/// link budget bounds link chains, and accepted directory identities are visited only once.
/// Filtering an alias out does not suppress another accepted alias. Depth admission happens
/// before identity deduplication: an accepted over-depth alias, even to the root, refuses.
/// Link resolution pins its endpoint before the filter; renaming that endpoint in the filter
/// does not redirect descent from the held directory to its replacement.
///
/// Descendants open only relative to held directories. Relative links stay within that pinned
/// object tree even if directories are renamed; this does not assert their current pathname
/// ancestry. Absolute links must use the captured caller/root-kernel spelling, revalidated by
/// root identity before the suffix is resolved through the held root. A renamed or replaced
/// root spelling refuses; arbitrary alternate ancestor aliases are not inferred.
///
/// Root ancestors use normal kernel resolution; the final root itself may not be a symlink.
/// The returned names are observations, not capabilities for a subsequent unguarded open.
pub fn walk_tree<F>(
    root: &Path,
    limits: WalkLimits,
    mut filter: F,
) -> Result<Vec<RelPath>, WalkError>
where
    F: FnMut(&RelPath, WalkEntryKind) -> bool,
{
    let root = Root::open(root)?;
    let mut entries_left = limits.max_entries;
    let mut links_left = TREE_LINK_BUDGET;
    let mut visited = BTreeSet::from([root.identity]);
    let mut frames = vec![frame(
        vec![Arc::clone(&root.file)],
        PathBuf::new(),
        0,
        &mut entries_left,
        limits.max_entries,
    )?];
    let mut result = Vec::new();
    while let Some(current) = frames.last_mut() {
        let Some(name) = current.names.next() else {
            frames.pop();
            continue;
        };
        let path = RelPath(current.path.join(&name));
        let parent = current.directories.last().expect("root is pinned");
        let before = stat_at(parent, &name).map_err(|error| io_at(path.as_path(), error))?;
        let is_link = before.st_mode & libc::S_IFMT == libc::S_IFLNK;
        if is_link && !limits.follow_symlinks_within_root {
            continue;
        }
        let resolved = if is_link {
            Some(resolve_link(
                &root,
                current.directories.clone(),
                name.clone(),
                &before,
                &path,
                &mut links_left,
            )?)
        } else {
            None
        };
        let entry_kind = resolved
            .as_ref()
            .map_or_else(|| kind(&before), |resolved| resolved.kind);
        if !filter(&path, entry_kind) {
            continue;
        }
        match entry_kind {
            WalkEntryKind::RegularFile => result.push(path),
            WalkEntryKind::Other => {}
            WalkEntryKind::Directory => {
                let depth = current
                    .depth
                    .checked_add(1)
                    .filter(|depth| *depth <= limits.max_depth)
                    .ok_or_else(|| WalkError::DepthExceeded {
                        path: path.clone(),
                        max_depth: limits.max_depth,
                    })?;
                let (directories, id) = match resolved {
                    Some(resolved) => (
                        resolved.directories,
                        resolved.identity.expect("resolved directory identity"),
                    ),
                    None => {
                        let directory = open_checked(parent, &name, &before, &path)?;
                        let mut directories = current.directories.clone();
                        directories.push(directory);
                        (directories, identity(&before))
                    }
                };
                if visited.insert(id) {
                    frames.push(frame(
                        directories,
                        path.0,
                        depth,
                        &mut entries_left,
                        limits.max_entries,
                    )?);
                }
            }
        }
    }
    result.sort();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn dot_root_pins_the_current_directory() {
        let root = Root::open(Path::new(".")).unwrap();
        let current = File::open(".").unwrap();
        assert_eq!(root.identity, identity(&stat_fd(&current).unwrap()));
    }

    #[test]
    fn checked_open_used_for_link_intermediates_refuses_a_replaced_directory() {
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let scratch = Scratch(
            std::env::temp_dir().join(format!("khive-tree-intermediate-{}", std::process::id())),
        );
        fs::create_dir(&scratch.0).unwrap();
        fs::create_dir(scratch.0.join("child")).unwrap();
        fs::create_dir(scratch.0.join("replacement")).unwrap();
        let parent = File::open(&scratch.0).unwrap();
        let name = OsStr::new("child");
        let path = RelPath(PathBuf::from("alias/file"));
        let before = stat_at(&parent, name).unwrap();
        let accepted = open_checked(&parent, name, &before, &path).unwrap();
        assert_eq!(identity(&stat_fd(&accepted).unwrap()), identity(&before));
        fs::rename(scratch.0.join("child"), scratch.0.join("original")).unwrap();
        fs::rename(scratch.0.join("replacement"), scratch.0.join("child")).unwrap();
        assert!(matches!(open_checked(&parent, name, &before, &path),
            Err(WalkError::EntryChanged { path: changed }) if changed == path));
        assert_eq!(identity(&stat_fd(&accepted).unwrap()), identity(&before));
    }
}
