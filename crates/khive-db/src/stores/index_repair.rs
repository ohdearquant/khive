//! Repair only backend-owned indexes explicitly forced by typed reads.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use khive_storage::{StorageCapability, StorageError, StorageResult};

use crate::backend::{StoreSchemaGate, StoreSchemaKind};
use crate::error::SqliteError;
use crate::pool::ConnectionPool;

#[derive(Clone, Copy)]
pub(crate) enum IndexReadKind {
    Graph,
    Notes,
}

#[derive(Clone, Copy)]
struct OwnedIndex {
    kind: StoreSchemaKind,
    ensure: fn(&rusqlite::Connection) -> Result<(), rusqlite::Error>,
}

/// Backend factory handles share readiness with subsequent handles. Standalone
/// store constructors deliberately retain their existing schema responsibility.
#[derive(Clone)]
pub(crate) struct IndexRepairContext {
    pool: Arc<ConnectionPool>,
    gates: [Arc<StoreSchemaGate>; 5],
    read_kind: IndexReadKind,
}

impl IndexRepairContext {
    pub(crate) fn new(
        pool: Arc<ConnectionPool>,
        gates: [Arc<StoreSchemaGate>; 5],
        read_kind: IndexReadKind,
    ) -> Self {
        Self {
            pool,
            gates,
            read_kind,
        }
    }

    pub(crate) fn is_writable(&self) -> bool {
        !self.pool.config().read_only
    }

    fn missing_owned_index(&self, error: &rusqlite::Error) -> Option<OwnedIndex> {
        if !self.is_writable() {
            return None;
        }
        if error
            .sqlite_error()
            .is_none_or(|detail| detail.extended_code != rusqlite::ffi::SQLITE_ERROR)
        {
            return None;
        }
        let message = match error {
            rusqlite::Error::SqliteFailure(_, Some(message)) => message.as_str(),
            rusqlite::Error::SqlInputError { msg, .. } => msg.as_str(),
            _ => return None,
        };
        let kind = match (self.read_kind, message) {
            (
                IndexReadKind::Graph,
                "no such index: idx_graph_edges_ns_src_rel"
                | "no such index: idx_graph_edges_ns_tgt_rel"
                | "no such index: idx_graph_edges_unique_triple",
            ) => StoreSchemaKind::Graph,
            (IndexReadKind::Graph, "no such index: idx_notes_created")
            | (
                IndexReadKind::Notes,
                "no such index: idx_notes_message_recipient_direction"
                | "no such index: idx_notes_unread_probe_recipient_direction"
                | "no such index: idx_notes_unread_probe_recipient_type_direction",
            ) => StoreSchemaKind::Notes,
            _ => return None,
        };
        let ensure: fn(&rusqlite::Connection) -> Result<(), rusqlite::Error> = match kind {
            StoreSchemaKind::Graph => super::graph::ensure_graph_schema,
            StoreSchemaKind::Notes => super::note::ensure_notes_schema,
            _ => unreachable!("only graph and notes own droppable forced indexes"),
        };
        Some(OwnedIndex { kind, ensure })
    }

    async fn repair(
        self,
        index: OwnedIndex,
        capability: StorageCapability,
        operation: &'static str,
    ) -> StorageResult<()> {
        // The first read has returned and released both its connection and
        // admission permit before we wait for the writer. Keep one absolute
        // request deadline; abandoning this await also stops pending admission.
        let (lifetime, cancellation) = tokio::sync::watch::channel(false);
        let result = khive_storage::scope_request_read_cancellation(cancellation, async move {
            let context = khive_storage::capture_request_read_context();
            tokio::task::spawn_blocking(move || {
                let gate = &self.gates[index.kind as usize];
                gate.ready.store(false, Ordering::Release);
                let writer = self
                    .pool
                    .writer_until_for_admitted_operation(|| {
                        context.blocking_stop_reason().is_some()
                    })
                    .map_err(|error| error.into_storage_error(capability, operation))?
                    .ok_or_else(|| StorageError::Timeout {
                        operation: operation.into(),
                    })?;
                if context.blocking_stop_reason().is_some() {
                    return Err(StorageError::Timeout {
                        operation: operation.into(),
                    });
                }
                let writer = writer
                    .admit_autocommit()
                    .map_err(|error| error.into_storage_error(capability, operation))?;
                // Once admitted, schema work retains the constructor's
                // non-interruptible write semantics. No notes_seq backfill.
                gate.ensure(writer.conn(), index.ensure).map_err(|error| {
                    SqliteError::Rusqlite(error).into_storage_error(capability, operation)
                })
            })
            .await
            .map_err(|error| StorageError::driver(capability, operation, error))?
        })
        .await;
        drop(lifetime);
        result
    }
}

enum IndexedRead<R, F> {
    Done(R),
    Retry { read: F, index: OwnedIndex },
}

/// The reusable closures here are read-only operations. A recognized failure
/// unwinds their statements/transactions before repair; writes never use this
/// route. The second attempt is returned directly with its original label and
/// SQLite cause, without another repair or generic retry.
pub(crate) async fn run_indexed_read<F, R>(
    pool: Arc<ConnectionPool>,
    repair: Option<IndexRepairContext>,
    capability: StorageCapability,
    operation: &'static str,
    mut read: F,
) -> StorageResult<R>
where
    F: FnMut(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
    R: Send + 'static,
{
    let first_repair = repair.clone();
    let first =
        super::run_pooled_store_read(Arc::clone(&pool), capability, operation, move |conn| {
            match read(conn) {
                Ok(value) => Ok(IndexedRead::Done(value)),
                Err(error) => {
                    if let Some(index) = first_repair
                        .as_ref()
                        .and_then(|repair| repair.missing_owned_index(&error))
                    {
                        Ok(IndexedRead::Retry { read, index })
                    } else {
                        Err(StorageError::driver(capability, operation, error))
                    }
                }
            }
        })
        .await?;
    match first {
        IndexedRead::Done(value) => Ok(value),
        IndexedRead::Retry { mut read, index } => {
            repair
                .expect("recognized index requires a repair context")
                .repair(index, capability, operation)
                .await?;
            super::run_pooled_store_read(pool, capability, operation, move |conn| {
                read(conn).map_err(|error| StorageError::driver(capability, operation, error))
            })
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::PoolConfig;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn fixture(path: &std::path::Path, kind: IndexReadKind) -> IndexRepairContext {
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(path.to_path_buf()),
                write_queue_enabled: Some(false),
                ..PoolConfig::for_test()
            })
            .unwrap(),
        );
        let gates: [Arc<StoreSchemaGate>; 5] =
            std::array::from_fn(|_| Arc::new(StoreSchemaGate::default()));
        {
            let writer = pool.writer().unwrap();
            gates[StoreSchemaKind::Notes as usize]
                .ensure(writer.conn(), super::super::note::ensure_notes_schema)
                .unwrap();
        }
        IndexRepairContext::new(pool, gates, kind)
    }

    #[tokio::test]
    async fn second_owned_index_failure_surfaces_once_with_original_sqlite_cause() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("second-failure.db");
        let repair = fixture(&path, IndexReadKind::Graph);
        let external = Arc::new(Mutex::new(rusqlite::Connection::open(path).unwrap()));
        external
            .lock()
            .unwrap()
            .execute_batch("DROP INDEX idx_notes_created")
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed_calls = Arc::clone(&calls);
        let read_external = Arc::clone(&external);
        let result = run_indexed_read(
            Arc::clone(&repair.pool),
            Some(repair.clone()),
            StorageCapability::Graph,
            "second_index_failure",
            move |conn| {
                let attempt = calls.fetch_add(1, Ordering::Relaxed);
                if attempt == 1 {
                    // Remove the actual repaired index again before the retry's
                    // real statement prepares; do not synthesize its error.
                    read_external
                        .lock()
                        .unwrap()
                        .execute_batch("DROP INDEX idx_notes_created")
                        .unwrap();
                }
                conn.query_row(
                    "SELECT COUNT(*) FROM notes INDEXED BY idx_notes_created",
                    [],
                    |row| row.get::<_, i64>(0),
                )
            },
        )
        .await;
        let error = result.unwrap_err();
        let StorageError::Driver {
            capability,
            operation,
            source,
        } = error
        else {
            panic!("{error}");
        };
        assert_eq!(capability, StorageCapability::Graph);
        assert_eq!(operation.as_ref(), "second_index_failure");
        let sqlite = source.downcast_ref::<rusqlite::Error>().unwrap();
        let message = match sqlite {
            rusqlite::Error::SqliteFailure(_, Some(message)) => message,
            rusqlite::Error::SqlInputError { msg, .. } => msg,
            _ => panic!("{sqlite}"),
        };
        assert_eq!(message, "no such index: idx_notes_created");
        assert_eq!(
            observed_calls.load(Ordering::Relaxed),
            2,
            "no third statement attempt"
        );
        assert_eq!(
            repair.gates[StoreSchemaKind::Notes as usize]
                .attempts
                .load(Ordering::Relaxed),
            2,
            "one initialization and one repair"
        );
    }

    #[tokio::test]
    async fn actual_unknown_or_wrong_owner_missing_index_errors_are_not_retried() {
        for (kind, index) in [
            (IndexReadKind::Graph, "idx_notes_created_suffix"),
            (IndexReadKind::Graph, "IDX_NOTES_CREATED"),
            (IndexReadKind::Notes, "idx_notes_created"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("exact-match.db");
            let repair = fixture(&path, kind);
            // SQLite identifiers are case-insensitive. Remove the lowercase
            // index so the uppercase SQL spelling produces a real failure.
            if index != "idx_notes_created_suffix" {
                rusqlite::Connection::open(path)
                    .unwrap()
                    .execute_batch("DROP INDEX idx_notes_created")
                    .unwrap();
            }
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&calls);
            let sql = format!("SELECT COUNT(*) FROM notes INDEXED BY {index}");
            let capability = match kind {
                IndexReadKind::Graph => StorageCapability::Graph,
                IndexReadKind::Notes => StorageCapability::Notes,
            };
            let error = run_indexed_read(
                Arc::clone(&repair.pool),
                Some(repair.clone()),
                capability,
                "unknown_index",
                move |conn| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    conn.query_row(&sql, [], |row| row.get::<_, i64>(0))
                },
            )
            .await
            .unwrap_err();
            assert!(matches!(error, StorageError::Driver { .. }));
            assert_eq!(observed.load(Ordering::Relaxed), 1);
            assert_eq!(
                repair.gates[StoreSchemaKind::Notes as usize]
                    .attempts
                    .load(Ordering::Relaxed),
                1
            );
            assert_eq!(
                repair.gates[StoreSchemaKind::Graph as usize]
                    .attempts
                    .load(Ordering::Relaxed),
                0
            );
        }
    }
}
