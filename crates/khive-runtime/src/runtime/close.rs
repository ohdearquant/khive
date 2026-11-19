//! Host-owned close state for a quiescent, single-backend runtime slot.

use std::sync::{Arc, Weak};

use khive_db::pool::{TimeoutSinkCloseError, TimeoutSinkCloseHandle};
use khive_db::{ConnectionPool, StorageBackend};

use super::KhiveRuntime;
use crate::audit_batch::AuditTerminalReason;
use crate::VerbRegistry;

/// Host resources that are outside this runtime's join ownership.
///
/// Hosts must stop request admission before constructing a close owner and
/// accurately declare these tasks. This API never cancels process-wide tasks.
#[derive(Clone, Copy, Debug, Default)]
pub struct RuntimeCloseHostTasks {
    pub daemon_checkpoint: bool,
    pub mcp_session_sweep: bool,
}

/// The reversible preflight and irreversible, resumable teardown boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeClosePhase {
    Ready,
    Closing,
    Closed,
}

/// A root for which the close owner retained a weak completion witness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeCloseRootKind {
    Pool,
    Backend,
    AuditStore,
    TimeoutSinkPool,
    TimeoutSinkWriter,
}

/// Strong references still keeping an observed root alive after teardown commits.
/// Counts are snapshots, not an inventory of the identities holding references.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeCloseRoot {
    pub kind: RuntimeCloseRootKind,
    pub strong_count: usize,
}

/// A close refusal or incomplete release. None of these means eviction is safe.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeCloseError {
    #[error("runtime close cannot settle owner: {0}")]
    Refused(&'static str),
    #[error("committed runtime close is incomplete; surviving roots: {roots:?}")]
    Incomplete { roots: Vec<RuntimeCloseRoot> },
    #[error("runtime close cannot prove writer completion: its join was taken by another owner")]
    WriterJoinUnavailable,
    #[error("runtime writer did not complete normally: {0}")]
    WriterFailed(String),
    #[error("runtime timeout sink did not complete normally")]
    SinkFailed,
    #[error("audit drained physically but reported terminal failure: {0:?}")]
    AuditFailed(AuditTerminalReason),
}

struct WriterClose {
    pool: Weak<ConnectionPool>,
    join: Option<tokio::task::JoinHandle<()>>,
    witness: Option<tokio::task::AbortHandle>,
    result: Option<Result<(), RuntimeCloseError>>,
    sink: Option<TimeoutSinkCloseHandle>,
}

/// Own every host-supplied runtime clone and its matching registry during eviction.
///
/// The host must first stop admitting requests and wait for its active request
/// counter. Transfer the registry built from these runtimes and *all* host-held
/// clones. A forgotten pool/capability returns `Incomplete` after teardown has
/// committed; drop that holder and retry. Do not reopen the root until `Ok`.
/// Configuration `Refused` results preserve the fully usable Ready state. The bundle retains writer joins across cancelled `close` futures.
/// Dropping the bundle itself is still best effort, not successful close.
///
/// The supported scope is a single backend without embeddings, ANN, blob
/// hydration, split events, or host background tasks. Those owners currently
/// lack a tenant-scoped physical completion barrier and are explicitly refused.
/// A successful return covers the owned audit tasks, SQLite pool/writer and
/// last-store timeout sink; it is not a census of arbitrary host/provider files.
///
/// Registry packs and its audit store must belong to the supplied runtime slot.
/// Custom packs must not hide independent file or task owners in this bundle.
pub struct RuntimeCloseOwner {
    registry: Option<VerbRegistry>,
    phase: RuntimeClosePhase,
    prepared_pools: Vec<Arc<ConnectionPool>>,
    backend_roots: Vec<Weak<StorageBackend>>,
    audit_store_root: Option<Weak<dyn khive_storage::EventStore>>,
    runtimes: Vec<KhiveRuntime>,
    host_tasks: RuntimeCloseHostTasks,
    writers: Option<Vec<WriterClose>>,
    audit_error: Option<AuditTerminalReason>,
    completed: Option<Result<(), RuntimeCloseError>>,
}

impl RuntimeCloseOwner {
    pub fn new(
        registry: VerbRegistry,
        runtimes: Vec<KhiveRuntime>,
        host_tasks: RuntimeCloseHostTasks,
    ) -> Self {
        Self {
            registry: Some(registry),
            phase: RuntimeClosePhase::Ready,
            prepared_pools: Vec::new(),
            backend_roots: Vec::new(),
            audit_store_root: None,
            runtimes,
            host_tasks,
            writers: None,
            audit_error: None,
            completed: None,
        }
    }

    pub fn phase(&self) -> RuntimeClosePhase {
        self.phase
    }

    /// The registry remains usable after reversible refusal. It is unavailable
    /// after teardown commits, including while a close future is cancelled.
    pub fn open_registry(&self) -> Option<&VerbRegistry> {
        if self.phase == RuntimeClosePhase::Ready {
            self.registry.as_ref()
        } else {
            None
        }
    }

    /// Transferred handles remain usable after reversible refusal only.
    pub fn open_runtimes(&self) -> Option<&[KhiveRuntime]> {
        (self.phase == RuntimeClosePhase::Ready).then_some(self.runtimes.as_slice())
    }

    /// Whether close has successfully completed; errors never mark it closed.
    pub fn is_closed(&self) -> bool {
        self.completed.as_ref().is_some_and(Result::is_ok)
    }

    /// Observe actual writer task completion without aborting or taking a join.
    pub fn writer_tasks_finished(&self) -> bool {
        self.writers.as_ref().is_some_and(|writers| {
            writers.iter().all(|writer| {
                writer.witness.as_ref().map_or_else(
                    || writer.result.as_ref().is_some_and(Result::is_ok),
                    tokio::task::AbortHandle::is_finished,
                )
            })
        })
    }

    fn pools_and_preflight(&self) -> Result<Vec<Arc<ConnectionPool>>, RuntimeCloseError> {
        use RuntimeCloseError::Refused;
        if self.host_tasks.daemon_checkpoint {
            return Err(Refused("daemon checkpoint loop"));
        }
        if self.host_tasks.mcp_session_sweep {
            return Err(Refused("MCP session sweep"));
        }
        if self.runtimes.is_empty() {
            return Err(Refused("empty runtime bundle"));
        }
        let mut pools = Vec::new();
        for runtime in &self.runtimes {
            if runtime.config.events_split.is_some() {
                return Err(Refused("split/socket events registry"));
            }
            // Read the actual registry, not the request-filtered public model
            // list: request embedder exclusions must not hide a live worker.
            let registry = runtime
                .embedder_registry
                .read()
                .map_err(|_| Refused("poisoned embedder registry"))?;
            if !registry.names().is_empty() || runtime.core_embedders.is_some() {
                return Err(Refused("embedder initialization and worker thread"));
            }
            drop(registry);
            if runtime
                .note_search_ann_provider
                .read()
                .map_err(|_| Refused("poisoned ANN provider registry"))?
                .is_some()
                || runtime
                    .note_mutation_hook
                    .read()
                    .map_err(|_| Refused("poisoned note mutation hook"))?
                    .is_some()
            {
                return Err(Refused("pack ANN tasks and mapped indexes"));
            }
            if runtime.blob_hydrator.get().is_some() {
                return Err(Refused("blob hydration supervisors and blocking work"));
            }
            if runtime.core_backend.is_some() {
                return Err(Refused("secondary backend runtime"));
            }
            let main = runtime.backend.pool_arc();
            for opened in runtime.diagnostic_backends().iter() {
                if !Arc::ptr_eq(&main, &opened.pool) {
                    return Err(Refused("additional diagnostic backend"));
                }
            }
            if main.checkpoint_owner_present() {
                return Err(Refused("claimed daemon checkpoint owner"));
            }
            if !pools.iter().any(|pool| Arc::ptr_eq(pool, &main)) {
                pools.push(main);
            }
        }
        if pools.len() != 1 {
            return Err(Refused("multiple backend pools"));
        }
        Ok(pools)
    }

    /// Stop audit admission, await physical append/reaper completion, drop all
    /// transferred roots, join the writer, then close the final store's sink.
    ///
    /// Cancellation only interrupts this wait. Retain this owner and call again
    /// to resume; no accepted append or native write is aborted. A second call
    /// after success returns `Ok(())`. A permanently stuck native operation can
    /// keep close pending; a caller timeout is not evidence of settlement.
    pub async fn close(&mut self) -> Result<(), RuntimeCloseError> {
        if let Some(result) = &self.completed {
            return result.clone();
        }
        if self.writers.is_none() {
            // Refuse unsupported configurations before stopping audit admission
            // or consuming any roots/joins, so the host retains its owners.
            if self.phase == RuntimeClosePhase::Ready {
                self.prepared_pools = self.pools_and_preflight()?;
                for runtime in &self.runtimes {
                    let weak = Arc::downgrade(&runtime.backend);
                    if !self
                        .backend_roots
                        .iter()
                        .any(|root| Weak::ptr_eq(root, &weak))
                    {
                        self.backend_roots.push(weak);
                    }
                }
                self.audit_store_root = self
                    .registry
                    .as_ref()
                    .and_then(VerbRegistry::event_store)
                    .map(|store| Arc::downgrade(&store));
                self.phase = RuntimeClosePhase::Closing;
            }
            if let Some(batch) = self
                .registry
                .as_ref()
                .and_then(VerbRegistry::audit_batch_handle)
            {
                self.audit_error = batch.close_and_wait_for_physical_completion().await.err();
            }
            let pools = std::mem::take(&mut self.prepared_pools);
            let writers = pools
                .iter()
                .map(|pool| {
                    let join = pool.take_writer_task_join();
                    let result = if join.is_some() {
                        None
                    } else if pool.writer_task_join_was_stored() {
                        Some(Err(RuntimeCloseError::WriterJoinUnavailable))
                    } else {
                        Some(Ok(()))
                    };
                    WriterClose {
                        pool: Arc::downgrade(pool),
                        witness: join.as_ref().map(tokio::task::JoinHandle::abort_handle),
                        join,
                        result,
                        sink: pool.timeout_sink_close_handle(),
                    }
                })
                .collect();
            // Publish all wait state before releasing the roots. No await may
            // intervene between taking a join and retaining it on this owner.
            self.writers = Some(writers);
            self.registry = None;
            self.runtimes.clear();
            drop(pools);
        }
        let writers = self.writers.as_mut().expect("close state initialized");
        let mut roots = Vec::new();
        for writer in writers.iter() {
            add_surviving_root(
                &mut roots,
                RuntimeCloseRootKind::Pool,
                writer.pool.strong_count(),
            );
        }
        for backend in &self.backend_roots {
            add_surviving_root(
                &mut roots,
                RuntimeCloseRootKind::Backend,
                backend.strong_count(),
            );
        }
        if let Some(store) = &self.audit_store_root {
            add_surviving_root(
                &mut roots,
                RuntimeCloseRootKind::AuditStore,
                store.strong_count(),
            );
        }
        if !roots.is_empty() {
            return Err(RuntimeCloseError::Incomplete { roots });
        }
        for writer in writers.iter_mut() {
            if let Some(join) = writer.join.as_mut() {
                let result = join
                    .await
                    .map_err(|error| RuntimeCloseError::WriterFailed(error.to_string()));
                writer.result = Some(result);
                writer.join = None;
            }
        }
        // A missing/failed writer join is never promoted to physical success.
        // Preserve the evidence on repeated calls rather than mistaking None
        // in the already-consumed pool slot for a completed writer.
        for writer in writers.iter() {
            writer
                .result
                .as_ref()
                .expect("writer outcome recorded")
                .clone()?;
        }
        for writer in writers.iter_mut() {
            if let Some(sink) = writer.sink.as_mut() {
                sink.close().await.map_err(|error| match error {
                    TimeoutSinkCloseError::Busy { pools, writers } => {
                        let mut roots = Vec::new();
                        add_surviving_root(
                            &mut roots,
                            RuntimeCloseRootKind::TimeoutSinkPool,
                            pools,
                        );
                        add_surviving_root(
                            &mut roots,
                            RuntimeCloseRootKind::TimeoutSinkWriter,
                            writers,
                        );
                        RuntimeCloseError::Incomplete { roots }
                    }
                    _ => RuntimeCloseError::SinkFailed,
                })?;
            }
        }
        let result = self
            .audit_error
            .map_or(Ok(()), |error| Err(RuntimeCloseError::AuditFailed(error)));
        if result.is_ok() {
            self.phase = RuntimeClosePhase::Closed;
        }
        self.completed = Some(result.clone());
        result
    }
}

fn add_surviving_root(
    roots: &mut Vec<RuntimeCloseRoot>,
    kind: RuntimeCloseRootKind,
    strong_count: usize,
) {
    if strong_count != 0 {
        roots.push(RuntimeCloseRoot { kind, strong_count });
    }
}

#[cfg(test)]
#[path = "close_tests.rs"]
mod tests;
