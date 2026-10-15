//! Storage error types shared across all backend implementations.

use std::borrow::Cow;
use std::error::Error as StdError;
use std::fmt;

use thiserror::Error;

use crate::blob::ContentRef;
use crate::capability::StorageCapability;

/// What is known about one request when its single-writer execution seam
/// terminates or is retired.
///
/// The state is deliberately about the request, not about why the task
/// stopped. Callers need this distinction to decide whether a write is known
/// not to have started, known to have had its SQLite transaction rolled back,
/// or may already have produced side effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterTaskRequestState {
    /// The request's operation closure was never invoked.
    NotStarted,
    /// The request ran inside `BEGIN IMMEDIATE`, and that transaction was
    /// successfully rolled back on the connection that owned it. The request
    /// may have returned an error, failed COMMIT, or panicked; none of its
    /// wrapped SQLite writes committed.
    TransactionRolledBack,
    /// The request was accepted, but its exact outcome cannot be established;
    /// it may already have produced side effects and must not be blindly
    /// retried.
    SideEffectsUnknown,
}

/// The SQLite boundary that returned a native write failure, independently of settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqliteWriteStage {
    #[serde(rename = "sqlite_begin_busy")]
    Begin,
    #[serde(rename = "sqlite_statement_failure")]
    Statement,
    #[serde(rename = "sqlite_commit_failure")]
    Commit,
}

impl SqliteWriteStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Begin => "sqlite_begin_busy",
            Self::Statement => "sqlite_statement_failure",
            Self::Commit => "sqlite_commit_failure",
        }
    }
}

/// Native evidence only; the enclosing error still owns retry and settlement policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SqliteWriteFailure {
    pub stage: SqliteWriteStage,
    pub primary_code: i32,
    pub extended_code: i32,
    /// Existing cleanup evidence; false does not prove rollback or any final outcome.
    #[serde(default)]
    pub settlement_unknown: bool,
}

impl fmt::Display for WriterTaskRequestState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotStarted => "not_started",
            Self::TransactionRolledBack => "transaction_rolled_back",
            Self::SideEffectsUnknown => "side_effects_unknown",
        })
    }
}

/// Which pre-write disk-admission step could not establish safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityUnavailablePhase {
    Identity,
    Lock,
    LockContended,
    Probe,
}

impl CapacityUnavailablePhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Lock => "lock",
            Self::LockContended => "lock_contended",
            Self::Probe => "probe",
        }
    }
}

impl fmt::Display for CapacityUnavailablePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Unified error type for all storage operations.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("{source}")]
    SqliteWrite {
        failure: SqliteWriteFailure,
        #[source]
        source: Box<StorageError>,
    },
    #[error("{capability:?} resource not found: {resource} ({key})")]
    NotFound {
        capability: StorageCapability,
        resource: &'static str,
        key: String,
    },

    #[error("{capability:?} resource already exists: {resource} ({key})")]
    AlreadyExists {
        capability: StorageCapability,
        resource: &'static str,
        key: String,
    },

    #[error("conflict in {capability:?} during {operation}: {message}")]
    Conflict {
        capability: StorageCapability,
        operation: Cow<'static, str>,
        message: String,
    },

    #[error("invalid input for {capability:?} during {operation}: {message}")]
    InvalidInput {
        capability: StorageCapability,
        operation: Cow<'static, str>,
        message: String,
    },

    #[error("unsupported operation for {capability:?}: {operation} ({message})")]
    Unsupported {
        capability: StorageCapability,
        operation: Cow<'static, str>,
        message: String,
    },

    /// The authoritative object is larger than the caller's declared
    /// whole-buffer limit. `observed_at_least` is either opened-object
    /// metadata used for early refusal or the running byte count that first
    /// crossed the limit; metadata is not treated as the actual-byte bound.
    #[error(
        "blob {content_ref} exceeds the {max_bytes}-byte read limit (observed at least {observed_at_least} bytes)"
    )]
    BlobTooLarge {
        content_ref: ContentRef,
        max_bytes: u64,
        observed_at_least: u64,
    },

    /// The complete bounded body disagreed with metadata obtained from the
    /// same opened object / GET response.
    #[error(
        "blob {content_ref} metadata reports {metadata_bytes} bytes but the complete body contains {actual_bytes} bytes"
    )]
    BlobSizeMismatch {
        content_ref: ContentRef,
        metadata_bytes: u64,
        actual_bytes: u64,
    },

    /// The complete, size-consistent bounded body did not hash to the
    /// requested content-addressed reference.
    #[error("blob digest mismatch: expected {expected}, computed {actual}")]
    BlobDigestMismatch {
        expected: ContentRef,
        actual: ContentRef,
    },

    #[error("pool failure during {operation}: {message}")]
    Pool {
        operation: Cow<'static, str>,
        message: String,
    },

    #[error("timeout during {operation}")]
    Timeout { operation: Cow<'static, str> },

    /// A bounded wait for storage admission (a reader/writer handle slot or a
    /// pooled reader checkout) elapsed before anything was acquired. The
    /// operation never started, so retrying cannot duplicate a side effect —
    /// distinct from [`StorageError::Timeout`], which makes no claim about
    /// whether work was in flight when the deadline expired.
    #[error("admission timeout during {operation} after {timeout_ms}ms{pool}", pool = match .pool_identity {
        Some(identity) => format!(" (pool: {identity})"),
        None => String::new(),
    })]
    AdmissionTimeout {
        operation: Cow<'static, str>,
        /// The configured admission deadline that elapsed, in milliseconds.
        timeout_ms: u64,
        /// Canonical database path display, or `:memory:`, when the refusing pool is known.
        pool_identity: Option<String>,
    },

    #[error("sql transaction failure during {operation}: {message}")]
    Transaction {
        operation: Cow<'static, str>,
        message: String,
    },

    /// A cached read-only handle's admitted transaction pinned a WAL
    /// snapshot past the configured `read_tx_max_age` bound and was
    /// proactively rolled back so the next call can open a fresh snapshot
    /// (#1846). Distinct from the generic [`StorageError::Transaction`]
    /// variant — which also covers failed-cleanup and write-side ambiguity
    /// cases that are not uniformly safe to retry — so callers (and MCP
    /// dispatch) can recognize this specific, always-safe-to-retry
    /// condition by variant rather than by parsing rendered text.
    #[error(
        "cached read-only transaction exceeded the maximum read-transaction age \
         ({max_age_secs}s) during {operation} and was rolled back; retry to open a fresh \
         read snapshot"
    )]
    ReadTransactionAgeEvicted {
        operation: Cow<'static, str>,
        max_age_secs: u64,
    },

    /// A cached read-only handle's admitted transaction pinned a WAL
    /// snapshot past `read_tx_max_age`, and the proactive rollback used to
    /// end the eviction (#1846) did not restore autocommit, or the rollback
    /// itself failed. The connection is discarded either way rather than
    /// returned to the pool. The age check that triggered this still ran
    /// before any read on the connection, so — exactly like
    /// [`StorageError::ReadTransactionAgeEvicted`] — retrying the caller's
    /// operation on a fresh connection is always safe; this variant exists
    /// only to keep that guarantee distinguishable from a clean eviction in
    /// the rendered message and to keep [`StorageError::Transaction`] (whose
    /// other cases are not uniformly safe to retry) out of this path.
    #[error(
        "cached read-only transaction exceeded the maximum read-transaction age \
         ({max_age_secs}s) during {operation} but could not be cleanly rolled back \
         ({message}); the connection was discarded, retry to open a fresh read snapshot"
    )]
    ReadTransactionAgeEvictionCleanupFailed {
        operation: Cow<'static, str>,
        max_age_secs: u64,
        message: String,
    },

    #[error("serialization failure in {capability:?}: {message}")]
    Serialization {
        capability: StorageCapability,
        message: String,
    },

    #[error("index maintenance failure in {capability:?}: {message}")]
    IndexMaintenance {
        capability: StorageCapability,
        message: String,
    },

    #[error("backend driver error in {capability:?} during {operation}: {source}")]
    Driver {
        capability: StorageCapability,
        operation: Cow<'static, str>,
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },

    /// The bounded write-queue channel (ADR-067 Component A) did not free
    /// capacity within the caller-supplied deadline. Returned only when a
    /// caller wraps `WriterTaskHandle::send`'s `channel.send().await` in a
    /// `tokio::time::timeout`; there is no immediate-error `try_send` path.
    #[error("write queue full: timed out after {timeout_ms}ms waiting for writer task capacity")]
    WriteQueueFull { timeout_ms: u64 },

    /// SQLite refused the writer task's `BEGIN IMMEDIATE` with
    /// `SQLITE_BUSY`/`SQLITE_LOCKED` until the configured busy timeout. The
    /// queue accepted the request, but its operation closure was never
    /// invoked, so retrying that one failed operation is safe.
    #[error(
        "writer task could not begin within {timeout_ms}ms because SQLite remained busy; request was not executed"
    )]
    WriterTaskBusy { timeout_ms: u64 },

    /// One transaction-wrapped writer request returned an error, and the
    /// writer then proved that request's SQLite transaction was rolled back.
    /// Unlike [`StorageError::WriterTaskTerminated`], this does not retire the
    /// writer seam. `source` preserves the operation or COMMIT error so its
    /// capability and retry policy remain independently inspectable.
    #[error("writer task request failed (request_state={request_state}): {source}")]
    WriterTaskRequestFailed {
        request_state: WriterTaskRequestState,
        #[source]
        source: Box<StorageError>,
    },

    /// A single-writer execution seam has terminated permanently. This is the
    /// historical writer-task variant and display name; the fail-closed
    /// pool-mutex fallback also uses it when transaction finalization becomes
    /// terminal. The state identifies what is known about this request at
    /// that boundary. Retrying on the same pool cannot recover the retired
    /// writer seam.
    #[error("writer task terminated (request_state={request_state})")]
    WriterTaskTerminated {
        request_state: WriterTaskRequestState,
        /// Original SQLITE_FULL primary/extended codes, when the terminal cause supplied them.
        /// Evidence only: it does not change finality, retryability or the error source chain.
        sqlite_full_codes: Option<(i32, i32)>,
    },

    /// An internal storage failure not attributable to a specific storage
    /// capability.
    #[error("internal storage error: {0}")]
    Internal(String),

    /// `KHIVE_WRITE_QUEUE=1` is set but the calling thread has no Tokio
    /// runtime context, so the writer task cannot be spawned (ADR-067
    /// Component A). Returned instead of panicking.
    /// See `crates/khive-storage/docs/api/error-taxonomy.md#writertasknoruntime`.
    #[error(
        "KHIVE_WRITE_QUEUE=1 but no Tokio runtime context is available to spawn the writer task"
    )]
    WriterTaskNoRuntime,

    /// A filesystem-backed capability refused a write because its free-space
    /// floor would be violated. Blob writes account for the known byte count;
    /// SQLite admission cannot know the next transaction's size.
    #[error(
        "refusing write on {capability:?} at {volume}: {available_bytes} bytes available, \
         the {floor_bytes}-byte free-space floor plus {required_headroom_bytes} bytes of \
         operation headroom would be violated"
    )]
    CapacityFloor {
        capability: StorageCapability,
        volume: String,
        available_bytes: u64,
        floor_bytes: u64,
        required_headroom_bytes: u64,
    },

    /// A new logical write could not establish disk-admission safety.
    #[error("capacity admission unavailable for {capability:?} in {phase} phase: {message}")]
    CapacityUnavailable {
        capability: StorageCapability,
        phase: CapacityUnavailablePhase,
        message: String,
    },
}

impl StorageError {
    /// Return native write evidence through a request-finality envelope.
    pub fn sqlite_write_failure(&self) -> Option<SqliteWriteFailure> {
        match self {
            Self::SqliteWrite { failure, .. } => Some(*failure),
            Self::WriterTaskRequestFailed { source, .. } => source.sqlite_write_failure(),
            _ => None,
        }
    }

    /// Borrow the original policy error without discarding any request-finality envelope.
    pub fn without_sqlite_write_stage(&self) -> &Self {
        match self {
            Self::SqliteWrite { source, .. } => source.without_sqlite_write_stage(),
            _ => self,
        }
    }

    /// Attach native evidence once; the original error still owns Display and retry policy.
    pub fn with_sqlite_write_failure(self, failure: SqliteWriteFailure) -> Self {
        if self.sqlite_write_failure().is_some() {
            self
        } else {
            Self::SqliteWrite {
                failure,
                source: Box::new(self),
            }
        }
    }
    /// Construct a terminal writer outcome without native SQLite evidence.
    ///
    /// The request state and existing retry policy are unchanged.
    pub fn writer_task_terminated(request_state: WriterTaskRequestState) -> Self {
        Self::WriterTaskTerminated {
            request_state,
            sqlite_full_codes: None,
        }
    }

    /// Construct a `Driver` error wrapping a backend-specific error source.
    pub fn driver(
        capability: StorageCapability,
        operation: impl Into<Cow<'static, str>>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self::Driver {
            capability,
            operation: operation.into(),
            source: Box::new(source),
        }
    }

    /// Return the storage capability surface that produced this error, if any.
    pub fn capability(&self) -> Option<StorageCapability> {
        match self {
            Self::NotFound { capability, .. }
            | Self::AlreadyExists { capability, .. }
            | Self::Conflict { capability, .. }
            | Self::InvalidInput { capability, .. }
            | Self::Unsupported { capability, .. }
            | Self::Serialization { capability, .. }
            | Self::IndexMaintenance { capability, .. }
            | Self::Driver { capability, .. }
            | Self::CapacityFloor { capability, .. }
            | Self::CapacityUnavailable { capability, .. } => Some(*capability),
            Self::BlobTooLarge { .. }
            | Self::BlobSizeMismatch { .. }
            | Self::BlobDigestMismatch { .. } => Some(StorageCapability::Blob),
            Self::WriterTaskRequestFailed { source, .. } | Self::SqliteWrite { source, .. } => {
                source.capability()
            }
            Self::Pool { .. }
            | Self::Timeout { .. }
            | Self::AdmissionTimeout { .. }
            | Self::Transaction { .. }
            | Self::ReadTransactionAgeEvicted { .. }
            | Self::ReadTransactionAgeEvictionCleanupFailed { .. }
            | Self::WriteQueueFull { .. }
            | Self::WriterTaskBusy { .. }
            | Self::WriterTaskTerminated { .. }
            | Self::Internal(..)
            | Self::WriterTaskNoRuntime => None,
        }
    }

    /// Whether this error is transient and the operation may succeed on retry.
    pub fn is_retryable(&self) -> bool {
        if let Self::WriterTaskRequestFailed { source, .. } | Self::SqliteWrite { source, .. } =
            self
        {
            return source.is_retryable();
        }
        matches!(
            self,
            Self::Pool { .. }
                | Self::Timeout { .. }
                | Self::AdmissionTimeout { .. }
                | Self::Transaction { .. }
                | Self::ReadTransactionAgeEvicted { .. }
                | Self::ReadTransactionAgeEvictionCleanupFailed { .. }
                | Self::WriteQueueFull { .. }
                | Self::WriterTaskBusy { .. }
                | Self::CapacityUnavailable {
                    phase: CapacityUnavailablePhase::LockContended,
                    ..
                }
        )
    }

    /// Whether this error is an FTS5 query-parser rejection of the MATCH
    /// expression itself, as opposed to a connection/pool/driver-level
    /// failure of the text-search backend.
    ///
    /// True only for `Driver` errors from the `Text` capability at the
    /// `fts_search` operation whose message names one of SQLite's FTS5
    /// parser failure modes (syntax error, stack overflow, unsupported
    /// column/phrase/NEAR query); all other errors return `false`.
    ///
    /// Callers that fail-open the FTS leg of a hybrid search (degrading to
    /// vector-only results on a bad query string) MUST gate on this
    /// predicate rather than on `StorageError` broadly — treating every
    /// `Err` as degradable turns a real backend outage into a silently-empty
    /// "successful" search (issue #389).
    /// See `crates/khive-storage/docs/api/error-taxonomy.md#is_fts5_syntax_error`.
    pub fn is_fts5_syntax_error(&self) -> bool {
        if let Self::WriterTaskRequestFailed { source, .. } | Self::SqliteWrite { source, .. } =
            self
        {
            return source.is_fts5_syntax_error();
        }
        let Self::Driver {
            capability,
            operation,
            source,
        } = self
        else {
            return false;
        };
        if *capability != StorageCapability::Text || operation.as_ref() != "fts_search" {
            return false;
        }
        let msg = source.to_string();
        msg.contains("fts5: syntax error")
            || msg.contains("fts5: parser stack overflow")
            || msg.contains("fts5: column queries are not supported")
            || msg.contains("fts5: phrase queries are not supported (detail")
            || msg.contains("fts5: NEAR queries are not supported (detail")
    }

    /// Whether this error is a UNIQUE constraint violation from a raw SQL
    /// `execute` (e.g. an `INSERT` racing an existing row under a natural
    /// key). True only for `Driver` errors from the `Sql` capability whose
    /// `operation` is one of `execute`, `pool_writer.execute`, or
    /// `tx.execute`, and whose message contains `UNIQUE constraint failed`.
    /// Batch/script operations are intentionally excluded.
    ///
    /// Callers that treat exact-key duplicates as a tolerated no-op
    /// (ADR-081 §4 serve-ledger idempotency) MUST gate on this predicate
    /// rather than swallowing every `Driver` error at `execute` — that would
    /// also hide genuine write failures (disk full, corruption).
    /// See `crates/khive-storage/docs/api/error-taxonomy.md#is_unique_constraint_violation`.
    pub fn is_unique_constraint_violation(&self) -> bool {
        if let Self::WriterTaskRequestFailed { source, .. } | Self::SqliteWrite { source, .. } =
            self
        {
            return source.is_unique_constraint_violation();
        }
        let Self::Driver {
            capability,
            operation,
            source,
        } = self
        else {
            return false;
        };
        if *capability != StorageCapability::Sql {
            return false;
        }
        if !matches!(
            operation.as_ref(),
            "execute" | "pool_writer.execute" | "tx.execute"
        ) {
            return false;
        }
        source.to_string().contains("UNIQUE constraint failed")
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
