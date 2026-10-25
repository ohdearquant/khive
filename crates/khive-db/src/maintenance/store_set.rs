use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

/// A resolved physical topology with an explicit main-store role.
///
/// Construction validates spelling only. The caller must first resolve physical
/// aliases: this type performs no filesystem I/O or inode-based deduplication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreSet {
    main: PathBuf,
    paths: Vec<PathBuf>,
    digest: [u8; 32],
}

impl StoreSet {
    /// Validate absolute, normalized Unix paths and canonicalize their ordering.
    ///
    /// Files may be absent. Every path must have a filename, and `main` must be
    /// a member. The digest includes exact non-UTF-8 bytes and the main role.
    pub fn from_canonical(main: PathBuf, mut paths: Vec<PathBuf>) -> io::Result<Self> {
        validate_path(&main)?;
        if paths.is_empty() {
            return Err(invalid_path("maintenance store set must not be empty"));
        }
        for path in &paths {
            validate_path(path)?;
        }
        paths.sort_by(|a, b| a.as_os_str().as_bytes().cmp(b.as_os_str().as_bytes()));
        paths.dedup();
        if !paths.contains(&main) {
            return Err(invalid_path("maintenance main store must be a set member"));
        }
        let mut hasher = Sha256::new();
        hasher.update(b"khive-maintenance-store-set-v1\0");
        hash_path(&mut hasher, &main);
        hasher.update((paths.len() as u64).to_le_bytes());
        for path in &paths {
            hash_path(&mut hasher, path);
        }
        let digest = hasher.finalize().into();
        Ok(Self {
            main,
            paths,
            digest,
        })
    }

    /// The actual main database, independent of sorted member order.
    pub fn main(&self) -> &Path {
        &self.main
    }

    /// Deduplicated members in ascending raw Unix pathname-byte order.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// The versioned, length-delimited SHA-256 topology identity.
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

fn hash_path(hasher: &mut Sha256, path: &Path) {
    let bytes = path.as_os_str().as_bytes();
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn invalid_path(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn validate_path(path: &Path) -> io::Result<()> {
    let bytes = path.as_os_str().as_bytes();
    if !path.is_absolute() || path.file_name().is_none() || bytes.contains(&0) {
        return Err(invalid_path(
            "maintenance store path must be absolute with a filename and no NUL",
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Normal(_) => normalized.push(component.as_os_str()),
            _ => return Err(invalid_path("maintenance store path must be normalized")),
        }
    }
    if normalized.as_os_str().as_bytes() != bytes {
        return Err(invalid_path(
            "maintenance store path must use its normalized spelling",
        ));
    }
    Ok(())
}
