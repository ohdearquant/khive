use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Mutex as StdMutex, OnceLock};

pub(super) struct Hook {
    pub(super) reached: Sender<()>,
    pub(super) release: Receiver<()>,
}

fn registry() -> &'static StdMutex<HashMap<PathBuf, Hook>> {
    static REGISTRY: OnceLock<StdMutex<HashMap<PathBuf, Hook>>> = OnceLock::new();
    REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()))
}

pub(super) fn install(root: &Path) -> (Receiver<()>, Sender<()>) {
    let canonical = root
        .canonicalize()
        .expect("root must exist before installing a walk_leaf_sync_hook");
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            canonical,
            Hook {
                reached: reached_tx,
                release: release_rx,
            },
        );
    (reached_rx, release_tx)
}

/// `root` need not be pre-canonicalized by the caller -- both `install`
/// and `take` canonicalize.
pub(super) fn take(root: &Path) -> Option<Hook> {
    let canonical = root.canonicalize().ok()?;
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&canonical)
}
