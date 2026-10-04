use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Mutex as StdMutex, OnceLock};

pub(super) struct Hook {
    pub(super) reached: Sender<()>,
    pub(super) release: Receiver<()>,
    pub(super) done: Sender<()>,
    #[cfg(unix)]
    pub(super) publication: Option<Publication>,
}

fn registry() -> &'static StdMutex<HashMap<PathBuf, VecDeque<Hook>>> {
    static REGISTRY: OnceLock<StdMutex<HashMap<PathBuf, VecDeque<Hook>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// Queue a one-shot hook for the next instrumented operation against
/// `root`'s canonical path. Consumed exactly once, FIFO.
pub(super) fn install(root: &Path) -> (Receiver<()>, Sender<()>, Receiver<()>) {
    let canonical = root
        .canonicalize()
        .expect("root must exist before installing a sync_hook");
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(canonical)
        .or_default()
        .push_back(Hook {
            reached: reached_tx,
            release: release_rx,
            done: done_tx,
            #[cfg(unix)]
            publication: None,
        });
    (reached_rx, release_tx, done_rx)
}

/// Pop the next queued hook for `root`'s canonical path, if any (`None`
/// for every ordinary, non-instrumented test -- `put` runs completely
/// unaffected). `root` need not be pre-canonicalized by the caller --
/// both `install` and `take` canonicalize, matching how
/// `write_lock_for_root` keys the shared lock registry.
pub(super) fn take(root: &Path) -> Option<Hook> {
    let canonical = root.canonicalize().ok()?;
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_mut(&canonical)
        .and_then(VecDeque::pop_front)
}

#[cfg(unix)]
type StepAction = (&'static str, Box<dyn FnOnce() + Send>);

#[cfg(unix)]
type DirectorySync = (&'static str, u64, u64);

#[cfg(unix)]
#[derive(Clone)]
pub(super) struct Publication {
    pub(super) completed: std::sync::Arc<StdMutex<Vec<&'static str>>>,
    pub(super) directories: std::sync::Arc<StdMutex<Vec<DirectorySync>>>,
    fail_at: Option<&'static str>,
    action: std::sync::Arc<StdMutex<Option<StepAction>>>,
}

#[cfg(unix)]
impl Publication {
    pub(super) fn before(&self, operation: &'static str) -> std::io::Result<()> {
        if self.fail_at == Some(operation) {
            return Err(std::io::Error::other("injected publication failure"));
        }
        let action = {
            let mut slot = self.action.lock().unwrap();
            if slot.as_ref().is_some_and(|(at, _)| *at == operation) {
                slot.take()
            } else {
                None
            }
        };
        if let Some((_, action)) = action {
            action();
        }
        Ok(())
    }

    pub(super) fn on_step(&self, operation: &'static str, action: impl FnOnce() + Send + 'static) {
        *self.action.lock().unwrap() = Some((operation, Box::new(action)));
    }

    pub(super) fn completed(&self, operation: &'static str) {
        self.completed.lock().unwrap().push(operation);
    }

    pub(super) fn directory_synced(
        &self,
        operation: &'static str,
        directory: &std::fs::File,
    ) -> std::io::Result<()> {
        use std::os::unix::fs::MetadataExt;

        let metadata = directory.metadata()?;
        self.directories
            .lock()
            .unwrap()
            .push((operation, metadata.dev(), metadata.ino()));
        Ok(())
    }
}

/// Use the same one-shot FIFO as the cancellation controls. Disconnected
/// lifecycle channels make a publication-only hook observe without pausing.
#[cfg(unix)]
pub(super) fn install_publication(root: &Path, fail_at: Option<&'static str>) -> Publication {
    let publication = Publication {
        completed: std::sync::Arc::default(),
        directories: std::sync::Arc::default(),
        fail_at,
        action: std::sync::Arc::default(),
    };
    let (reached, _) = std::sync::mpsc::channel();
    let (_, release) = std::sync::mpsc::channel();
    let (done, _) = std::sync::mpsc::channel();
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(root.canonicalize().unwrap())
        .or_default()
        .push_back(Hook {
            reached,
            release,
            done,
            publication: Some(publication.clone()),
        });
    publication
}
