//! Explicit, cancellation-resumable ownership of diagnostic writer threads.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::Mutex;

use super::{
    heartbeat_interval_from_env, mpsc, thread, writer_thread_loop, SinkHandle, QUEUE_CAPACITY,
};
use crate::error::SqliteError;

type SharedRegistry = Arc<Mutex<Registry>>;

#[derive(Default)]
struct Registry {
    directories: HashMap<PathBuf, Arc<Entry>>,
    routes: HashMap<String, Arc<SinkHandle>>,
}

struct Entry {
    directory: PathBuf,
    state: Mutex<State>,
}

struct State {
    pools: usize,
    writers: usize,
    handle: Option<Arc<SinkHandle>>,
    join: Option<JoinHandle<()>>,
    result: Option<Result<(), TimeoutSinkCloseError>>,
}

static REGISTRY: OnceLock<SharedRegistry> = OnceLock::new();
static ROUTING_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Failure to finish an explicitly enrolled diagnostic sink shutdown.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TimeoutSinkCloseError {
    /// Another pool or a writer task still owns the shared sink.
    #[error("diagnostic sink is busy: {pools} live pools, {writers} writer tasks")]
    Busy { pools: usize, writers: usize },
    /// The writer exited by unwinding instead of draining normally.
    #[error("diagnostic sink writer thread panicked")]
    ThreadPanicked,
}

/// A retained completion witness for a pool's diagnostic sink.
///
/// Obtain this before releasing the pool. Clones share the same retained
/// thread join and completion result; none of them keeps pool ownership alive.
#[derive(Clone)]
pub struct TimeoutSinkCloseHandle {
    registry: SharedRegistry,
    entry: Arc<Entry>,
}

impl TimeoutSinkCloseHandle {
    /// Wait for the shared writer thread to drain its accepted queue, close
    /// its file, and join. Live pools or writer tasks return
    /// [`TimeoutSinkCloseError::Busy`]; retry after they settle.
    /// Cancellation retains the join and Closing state, so
    /// a clone can resume this operation. New pool registration is refused
    /// while Closing. Ordinary pool Drop does not request this shutdown.
    pub async fn close(&self) -> Result<(), TimeoutSinkCloseError> {
        loop {
            if let Some(result) = self.try_close() {
                return result;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    fn try_close(&self) -> Option<Result<(), TimeoutSinkCloseError>> {
        let mut registry = self.registry.lock();
        let mut state = self.entry.state.lock();
        if let Some(result) = &state.result {
            return Some(result.clone());
        }
        if state.pools != 0 || state.writers != 0 {
            return Some(Err(TimeoutSinkCloseError::Busy {
                pools: state.pools,
                writers: state.writers,
            }));
        }
        if let Some(handle) = state.handle.take() {
            registry
                .routes
                .retain(|_, route| !Arc::ptr_eq(route, &handle));
            // Last sender closes admission. In-flight emit calls may still own
            // a sender; the receiver drains them before its thread can finish.
            drop(handle);
        }
        if !state.join.as_ref().is_some_and(JoinHandle::is_finished) {
            return None;
        }
        // Never take a running join: cancellation cannot detach it. is_finished
        // includes destruction of the thread's captured values and file owner.
        let result = state
            .join
            .take()
            .expect("unfinished sink retains its join")
            .join()
            .map_err(|_| TimeoutSinkCloseError::ThreadPanicked);
        state.result = Some(result.clone());
        registry.directories.remove(&self.entry.directory);
        Some(result)
    }
}

/// One writable pool's registration, retained until the pool's fields drop.
/// Dropping it deliberately leaves the ordinary process-lifetime sink running.
pub(crate) struct Registration {
    registry: SharedRegistry,
    entry: Arc<Entry>,
}

impl Registration {
    pub(crate) fn close_handle(&self) -> TimeoutSinkCloseHandle {
        TimeoutSinkCloseHandle {
            registry: self.registry.clone(),
            entry: self.entry.clone(),
        }
    }

    pub(crate) fn writer_lease(&self) -> WriterLease {
        self.entry.state.lock().writers += 1;
        WriterLease {
            _owner: Arc::new(WriterOwner(self.entry.clone())),
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.entry.state.lock().pools -= 1;
    }
}

/// Retained by the async writer and each native connection-owning child.
#[derive(Clone)]
pub(crate) struct WriterLease {
    _owner: Arc<WriterOwner>,
}

struct WriterOwner(Arc<Entry>);

impl Drop for WriterOwner {
    fn drop(&mut self) {
        self.0.state.lock().writers -= 1;
    }
}

pub(super) fn init(dir: PathBuf, db: &str) -> Result<Option<Registration>, SqliteError> {
    let registry = REGISTRY.get_or_init(|| Arc::new(Mutex::new(Registry::default())));
    init_in(registry, dir, db)
}

fn init_in(
    registry: &SharedRegistry,
    dir: PathBuf,
    db: &str,
) -> Result<Option<Registration>, SqliteError> {
    let mut guard = registry.lock();
    let entry = if let Some(entry) = guard.directories.get(&dir) {
        if entry.state.lock().handle.is_none() {
            return Err(SqliteError::InvalidConfig(format!(
                "diagnostic sink is closing for {}",
                dir.display()
            )));
        }
        entry.clone()
    } else {
        if guard.routes.contains_key(db) {
            return Err(SqliteError::InvalidConfig(
                "database diagnostic sink is already registered in another directory".into(),
            ));
        }
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let (go_sender, go_receiver) = mpsc::sync_channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let thread_dropped = dropped.clone();
        let thread_dir = dir.clone();
        let identity = db.to_owned();
        let heartbeat = heartbeat_interval_from_env();
        let join = match thread::Builder::new()
            .name("khive-writer-timeout-sink".into())
            .spawn(move || {
                writer_thread_loop(
                    go_receiver,
                    receiver,
                    thread_dir,
                    identity,
                    thread_dropped,
                    heartbeat,
                )
            }) {
            Ok(join) => join,
            // Preserve fail-open thread-allocation behavior; later opens retry.
            Err(_) => return Ok(None),
        };
        let entry = Arc::new(Entry {
            directory: dir.clone(),
            state: Mutex::new(State {
                pools: 0,
                writers: 0,
                handle: Some(Arc::new(SinkHandle { sender, dropped })),
                join: Some(join),
                result: None,
            }),
        });
        guard.directories.insert(dir, entry.clone());
        let _ = go_sender.send(());
        entry
    };
    let mut state = entry.state.lock();
    let handle = state.handle.as_ref().expect("registered sink is open");
    if guard
        .routes
        .get(db)
        .is_some_and(|route| !Arc::ptr_eq(route, handle))
    {
        return Err(SqliteError::InvalidConfig(
            "database diagnostic sink is already registered in another directory".into(),
        ));
    }
    guard.routes.insert(db.to_owned(), handle.clone());
    state.pools += 1;
    drop(state);
    Ok(Some(Registration {
        registry: registry.clone(),
        entry,
    }))
}

/// Producer routing never waits for initialization or shutdown registry locks.
pub(super) fn route(db: &str) -> Option<Arc<SinkHandle>> {
    let registry = REGISTRY.get()?;
    route_in(registry, &ROUTING_DROPPED, db)
}

fn route_in(
    registry: &SharedRegistry,
    routing_dropped: &AtomicU64,
    db: &str,
) -> Option<Arc<SinkHandle>> {
    let Some(guard) = registry.try_lock() else {
        routing_dropped.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    let handle = guard.routes.get(db)?.clone();
    handle.dropped.fetch_add(
        routing_dropped.swap(0, Ordering::Relaxed),
        Ordering::Relaxed,
    );
    Some(handle)
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
