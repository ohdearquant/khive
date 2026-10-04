use super::*;

/// Process-wide registry of per-canonical-root write locks.
///
/// A `Mutex` field scoped to one `FsBlobStore` instance does NOT serialize
/// writes across independently constructed stores for the same root — and
/// callers construct fresh stores for the same root routinely
/// (`StorageBackend::blob_store` builds a new `FsBlobStore` on every call).
/// Keying a shared `Arc<tokio::sync::Mutex<()>>` by
/// the filesystem's own canonical path closes that gap: every `FsBlobStore`
/// for the same root, however many separate `new` calls produced them,
/// resolves to the exact same lock.
fn root_write_locks() -> &'static StdMutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>> {
    static REGISTRY: OnceLock<StdMutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// Look up (or create) the shared write lock for `root`'s canonical path.
///
/// `root` must already exist when this is called — `FsBlobStore::new`
/// creates it first, and `Path::canonicalize` requires the path to exist.
/// The lookup-or-insert happens under the registry's own (synchronous, very
/// briefly held) lock, so two `FsBlobStore::new` calls racing for the same
/// root cannot each install a different `Arc` and defeat the sharing this
/// exists for.
pub(super) fn write_lock_for_root(root: &Path) -> std::io::Result<Arc<tokio::sync::Mutex<()>>> {
    let canonical = root.canonicalize()?;
    let mut locks = root_write_locks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok(locks
        .entry(canonical)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone())
}
