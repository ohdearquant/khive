//! Live-pool registry that labels each open database by its final file name.

use super::*;

struct OpenPoolIdentity {
    count: usize,
    basename: String,
    suffix: String,
}

/// Only final file names and the first eight SHA-256 hex digits enter errors.
/// Canonical paths remain internal to the live-pool registry.
#[derive(Default)]
struct PoolIdentityRegistry {
    paths: HashMap<PathBuf, OpenPoolIdentity>,
}

fn pool_identity_registry() -> &'static Mutex<PoolIdentityRegistry> {
    static REGISTRY: OnceLock<Mutex<PoolIdentityRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(PoolIdentityRegistry::default()))
}

/// SHA-256 over raw Unix path bytes, or Windows UTF-16 code units in little
/// endian order. Encoding and digest are explicit so toolchain upgrades and
/// process restarts cannot change a given canonical path's suffix.
pub(super) fn pool_identity_suffix(path: &Path) -> String {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    };
    #[cfg(windows)]
    let bytes = {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    };
    #[cfg(not(any(unix, windows)))]
    let bytes = path.to_string_lossy().as_bytes().to_vec();
    let digest = Sha256::digest(&bytes);
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

pub(super) struct PoolIdentityRegistration(PathBuf);

impl PoolIdentityRegistration {
    pub(super) fn new(path: &Path) -> Self {
        let mut registry = pool_identity_registry().lock();
        if let Some(entry) = registry.paths.get_mut(path) {
            entry.count += 1;
        } else {
            let basename = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let suffix = pool_identity_suffix(path);
            registry.paths.insert(
                path.to_path_buf(),
                OpenPoolIdentity {
                    count: 1,
                    basename,
                    suffix,
                },
            );
        }
        Self(path.to_path_buf())
    }

    pub(super) fn label(&self) -> String {
        let registry = pool_identity_registry().lock();
        let entry = &registry.paths[&self.0];
        let collides = registry
            .paths
            .iter()
            .any(|(path, other)| path != &self.0 && other.basename == entry.basename);
        if collides {
            format!("{}#{}", entry.basename, entry.suffix)
        } else {
            entry.basename.clone()
        }
    }
}

impl Drop for PoolIdentityRegistration {
    fn drop(&mut self) {
        let mut registry = pool_identity_registry().lock();
        if let Some(entry) = registry.paths.get_mut(&self.0) {
            entry.count -= 1;
            if entry.count == 0 {
                registry.paths.remove(&self.0);
            }
        }
    }
}
