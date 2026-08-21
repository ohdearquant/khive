//! Runtime error types.

use std::fmt;
use std::time::Duration;

use thiserror::Error;
use uuid::Uuid;

/// Convenience alias for `Result<T, RuntimeError>`.
pub type RuntimeResult<T> = Result<T, RuntimeError>;

/// Stable ADR-135 F6 stage and wire code for a finite-wait pooled writer
/// checkout that expires before SQLite executes.
pub const WRITER_POOL_CHECKOUT_TIMEOUT_STAGE: &str = "writer_pool_checkout_timeout";

/// Stable wire code/stage for a bounded write-queue enqueue that never
/// accepted the request within its configured deadline (#1382, #1643).
pub const WRITER_QUEUE_SATURATED_STAGE: &str = "writer_queue_saturated";

/// Stable wire code/stage for SQLite write-lock contention after the writer
/// queue accepted a request but before its operation closure ran.
pub const WRITER_TASK_BEGIN_BUSY_STAGE: &str = "writer_task_begin_busy";

/// Stable ADR-131:251 `scope` discriminator carried on a
/// [`WRITER_QUEUE_SATURATED_STAGE`] failure — distinguishes write-queue
/// admission saturation from other `unavailable` failure kinds that share
/// the same `retryable: true` shape but are not bounded by the admission
/// deadline (e.g. [`WRITER_POOL_CHECKOUT_TIMEOUT_STAGE`], which has no
/// ADR-131-defined scope and carries `None`).
pub const WRITER_ADMISSION_SCOPE: &str = "writer_admission";

/// Structured context for a pre-execution write-admission failure: either a
/// finite-wait pooled writer checkout timeout or a bounded write-queue
/// enqueue timeout. Both happen before SQLite executes the request, so both
/// are safe to classify as retryable — the request was never accepted, let
/// alone started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionFailureContext {
    /// Stable wire stage/code: one of [`WRITER_POOL_CHECKOUT_TIMEOUT_STAGE`]
    /// or [`WRITER_QUEUE_SATURATED_STAGE`].
    pub stage: &'static str,
    /// The configured deadline that elapsed.
    pub timeout: Duration,
    /// Storage capability the request was scoped to, when known. The write
    /// queue is process-local and unscoped by capability, so this is `None`
    /// for [`WRITER_QUEUE_SATURATED_STAGE`].
    pub capability: Option<khive_storage::StorageCapability>,
    /// Storage operation name, when known.
    pub operation: Option<String>,
    /// ADR-131:251 `scope` discriminator. `Some(`[`WRITER_ADMISSION_SCOPE`]`)`
    /// for [`WRITER_QUEUE_SATURATED_STAGE`]; `None` for
    /// [`WRITER_POOL_CHECKOUT_TIMEOUT_STAGE`], which ADR-131 does not define a
    /// scope for.
    pub scope: Option<&'static str>,
    /// ADR-131:251 `retry_after_ms` hint — set equal to the admission
    /// deadline actually applied to the rejected operation, so a retrying
    /// caller waits at least one full admission window before retrying.
    /// `Some(timeout_ms)` for [`WRITER_QUEUE_SATURATED_STAGE`]; `None`
    /// otherwise.
    pub retry_after_ms: Option<u64>,
}

/// Structured context for a failure that is proven safe to retry.
///
/// This includes the two pre-admission failures above and writer-task BEGIN
/// contention after queue acceptance but before operation execution. Callers
/// must inspect `scope`: only `writer_admission` means the queue never
/// accepted the operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryableFailureContext {
    /// Stable wire stage/code.
    pub stage: &'static str,
    /// Configured wait that ended before execution.
    pub timeout: Duration,
    /// Storage capability when the failure is capability-scoped.
    pub capability: Option<khive_storage::StorageCapability>,
    /// Storage operation when known.
    pub operation: Option<String>,
    /// Admission scope only when the queue never accepted the operation.
    pub scope: Option<&'static str>,
    /// Server backoff hint when a governing contract defines one.
    pub retry_after_ms: Option<u64>,
}

impl From<AdmissionFailureContext> for RetryableFailureContext {
    fn from(context: AdmissionFailureContext) -> Self {
        Self {
            stage: context.stage,
            timeout: context.timeout,
            capability: context.capability,
            operation: context.operation,
            scope: context.scope,
            retry_after_ms: context.retry_after_ms,
        }
    }
}

/// Typed disposition for a channel message whose `comm.ingest` write failed.
///
/// Every bucket carries the stable [`RuntimeError`] variant name that selected
/// it. Consumers can therefore make retry/quarantine decisions and report a
/// reason without inspecting the error's rendered text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelIngestFailureClass {
    /// A pre-execution admission failure that is safe to retry indefinitely.
    Retryable { reason: &'static str },
    /// A deterministic policy refusal that should quarantine immediately.
    Permanent { reason: &'static str },
    /// An error with no explicit retry or permanent policy classification.
    Unknown { reason: &'static str },
}

impl ChannelIngestFailureClass {
    /// Stable lowercase bucket name persisted on quarantine notifications.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Retryable { .. } => "retryable",
            Self::Permanent { .. } => "permanent",
            Self::Unknown { .. } => "unknown",
        }
    }

    /// Stable typed error-variant name; never derived from `Display` output.
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Retryable { reason } | Self::Permanent { reason } | Self::Unknown { reason } => {
                reason
            }
        }
    }
}

/// Structured context recovered from either a direct SQLite runtime error or
/// a typed SQLite source preserved inside a storage-driver wrapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterPoolCheckoutTimeoutContext {
    /// Finite checkout deadline that elapsed before the pooled mutex was
    /// acquired.
    pub timeout: Duration,
    /// Storage capability that wrapped the SQLite source, if one did.
    pub capability: Option<khive_storage::StorageCapability>,
    /// Storage operation that wrapped the SQLite source, if one did.
    pub operation: Option<String>,
}

/// A guarded edge write (`link`/`link_many`) was refused because one or both
/// endpoints no longer existed at write time. Names the exact endpoint(s)
/// missing instead of a generic "source or target" message, and, for a batch
/// write, which entry in the batch failed first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardedWriteFailure {
    /// Index of the failing entry within a `link_many` batch. `None` for the
    /// singleton `link` path, which has no batch to index into.
    pub entry_index: Option<usize>,
    /// The source endpoint id, present only when it is the (or one of the)
    /// missing endpoint(s).
    pub missing_source: Option<Uuid>,
    /// The target endpoint id, present only when it is the (or one of the)
    /// missing endpoint(s).
    pub missing_target: Option<Uuid>,
}

impl fmt::Display for GuardedWriteFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut missing = Vec::new();
        if let Some(source) = self.missing_source {
            missing.push(format!("source {source}"));
        }
        if let Some(target) = self.missing_target {
            missing.push(format!("target {target}"));
        }
        let missing = if missing.is_empty() {
            "endpoint(s)".to_string()
        } else {
            missing.join(" and ")
        };
        match self.entry_index {
            Some(index) => write!(
                f,
                "batch entry {index}: {missing} no longer exist at write time"
            ),
            None => write!(f, "{missing} no longer exist at write time"),
        }
    }
}

impl std::error::Error for GuardedWriteFailure {}

/// A single missing pack dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingPackDependency {
    pub from: String,
    pub requires: String,
}

impl fmt::Display for MissingPackDependency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "pack '{}' requires '{}', but '{}' is not in the loaded pack set",
            self.from, self.requires, self.requires
        )
    }
}

impl std::error::Error for MissingPackDependency {}

/// Multiple missing pack dependencies collected into one error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingPackDependencies {
    pub missing: Vec<MissingPackDependency>,
}

impl fmt::Display for MissingPackDependencies {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.missing.iter().map(ToString::to_string).collect();
        write!(f, "{}", parts.join("; "))
    }
}

impl std::error::Error for MissingPackDependencies {}

/// Circular pack dependency detected during topological sort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CircularPackDependency {
    pub cycle: Vec<String>,
}

impl fmt::Display for CircularPackDependency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "circular dependency detected among packs: {}",
            self.cycle.join(" -> ")
        )
    }
}

impl std::error::Error for CircularPackDependency {}

/// All errors produced by the khive-runtime layer.
///
/// Variants cover storage, query, validation, namespace isolation, and permission failures.
/// Callers should match on `InvalidInput` for bad arguments, `NotFound` for missing records,
/// and `NamespaceMismatch` (reported as not-found) for cross-namespace access attempts.
#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("storage: {0}")]
    Storage(#[from] khive_storage::StorageError),

    #[error("sqlite: {0}")]
    Sqlite(#[from] khive_db::SqliteError),

    #[error("query: {0}")]
    Query(#[from] khive_query::QueryError),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("invalid input: {0}")]
    UnknownVerb(String),

    #[error("unconfigured: {0} is not set")]
    Unconfigured(String),

    #[error("unknown embedding model: {0}")]
    UnknownModel(String),

    #[error("embedding: {0}")]
    Embedding(#[from] lattice_embed::EmbedError),

    #[error("ambiguous: {0}")]
    Ambiguous(String),

    #[error("fusion: {0}")]
    Fusion(#[from] khive_fusion::FuseError),

    /// `FusionStrategy::Custom { name, .. }` named a strategy no pack has
    /// registered via `KhiveRuntime::register_fusion_strategy` (ADR-012).
    /// Fails closed — never falls back to RRF or any other default.
    #[error("unknown fusion strategy: {0}")]
    UnknownFusionStrategy(String),

    #[error("internal: {0}")]
    Internal(String),

    /// An `EventStore` was configured via `with_event_store` but does not
    /// implement ADR-133's `preflight_event`/`append_events_idempotent`
    /// pair (`EventStore::supports_idempotent_audit_batch` reports
    /// `false`). Raised at `build()` time rather than left to fail every
    /// audited dispatch silently.
    #[error("audit batch incompatible event store: {0}")]
    IncompatibleEventStore(String),

    #[error("guarded edge write refused: {0}")]
    GuardedWriteFailed(GuardedWriteFailure),

    #[error("missing pack dependency: {0}")]
    MissingPackDependency(MissingPackDependency),

    #[error("missing pack dependencies: {0}")]
    MissingPackDependencies(MissingPackDependencies),

    #[error("{0}")]
    CircularPackDependency(CircularPackDependency),

    #[error("pack '{name}' registered twice (indices {first_idx} and {second_idx})")]
    PackRedeclared {
        name: String,
        first_idx: usize,
        second_idx: usize,
    },

    /// Two packs declared the same `Visibility::Verb` handler name.
    /// `Visibility::Subhandler` entries are pack-prefixed and do not
    /// participate in cross-pack collision checks.
    #[error(
        "verb collision: verb {verb:?} declared by both pack {first_pack:?} and pack \
         {second_pack:?}; rename one handler or use Visibility::Subhandler for internal verbs"
    )]
    VerbCollision {
        verb: String,
        first_pack: String,
        second_pack: String,
    },

    /// A handler advertised a parameter that request parsing owns at the envelope level.
    #[error(
        "pack {pack:?} handler {verb:?} declares request-envelope parameter {param:?}; rename the verb argument"
    )]
    ReservedEnvelopeParam {
        pack: String,
        verb: String,
        param: String,
    },

    /// Gate denied this verb invocation.
    ///
    /// Returned by `VerbRegistry::dispatch` when the configured `Gate` returns
    /// `GateDecision::Deny`. The pack is never invoked. The `reason` field
    /// carries the deny message produced by the gate implementation.
    #[error("permission denied for verb {verb:?}: {reason}")]
    PermissionDenied { verb: String, reason: String },

    /// The configured gate could not produce an authorization decision.
    ///
    /// This is distinct from [`Self::PermissionDenied`]: the pack or
    /// intercepted operation is never invoked, but the gate did not answer
    /// with an explicit denial.
    #[error("gate unavailable for verb {verb:?}: {reason}")]
    GateUnavailable { verb: String, reason: String },

    /// A structured [`khive_types::KhiveError`] converted into the runtime
    /// layer. The full structured error is preserved so callers can inspect
    /// `kind`, `code`, `details`, and `retry_hint` without information loss.
    #[error("{0}")]
    Khive(khive_types::KhiveError),

    /// Record exists but belongs to a different namespace than the provided token.
    ///
    /// Externally reported as "not found in this namespace" to avoid leaking
    /// cross-namespace existence information (timing-oracle mitigation).
    #[error("not found in this namespace")]
    NamespaceMismatch { id: uuid::Uuid },

    /// A short-prefix lookup matched more than one record.
    ///
    /// `prefix` is the 8+ hex-char prefix supplied by the caller.
    /// `matches` holds the full UUIDs of all matching records (at most 2 are
    /// reported to bound the scan — callers must supply the full UUID to disambiguate).
    #[error("ambiguous prefix {prefix:?}: matches {}", format_uuid_list(matches))]
    AmbiguousPrefix {
        prefix: String,
        matches: Vec<uuid::Uuid>,
    },

    /// Cross-backend `merge_entity` is unsupported in v1.
    ///
    /// Both entities must reside on the same backend. To merge entities on different
    /// backends, manually export `from_id`, delete it, and re-import on `into_id`'s backend.
    #[error(
        "cross-backend merge is not supported: \
         into_id {into_id} is on backend '{into_backend}', \
         from_id {from_id} is on backend '{from_backend}'. \
         Both entities must be on the same backend to merge."
    )]
    CrossBackendMergeUnsupported {
        into_id: uuid::Uuid,
        from_id: uuid::Uuid,
        into_backend: String,
        from_backend: String,
    },

    // ── Remote Resolution and Content-Hash Verification ──────────────────────
    /// A `kg://` ref names a remote not declared in `schema.yaml`.
    #[error("unknown remote: {name:?}")]
    UnknownRemote { name: String },

    /// A remote cache entry is absent and `--fetch` was not requested.
    #[error("remote cache missing for remote={remote:?} namespace={namespace:?}")]
    RemoteCacheMissing { remote: String, namespace: String },

    /// A short ID matches multiple entities in the same namespace or remote cache.
    #[error("ambiguous id {id:?}: matched {count} records")]
    AmbiguousId { id: String, count: usize },

    /// A write operation targeted a remote namespace, which is read-only.
    #[error("cross-namespace write denied: cannot write to remote namespace {namespace:?}")]
    CrossNamespaceWrite { namespace: String },

    /// A remote fetch failed (network error, authentication failure, etc.).
    #[error("remote fetch error for remote={remote:?}: {message}")]
    RemoteFetchError { remote: String, message: String },

    /// A caller-supplied write budget was exceeded during a Compound apply.
    ///
    /// `max_new_entries` is the limit passed by the caller. `attempted_new_entries`
    /// is `consumed + 1`, i.e. the create that would have exceeded the cap.
    /// `None` budget never produces this error (unlimited path).
    #[error(
        "write budget exceeded: max_new_entries={max_new_entries}, \
         attempted_new_entries={attempted_new_entries}"
    )]
    WriteBudgetExceeded {
        max_new_entries: u64,
        attempted_new_entries: u64,
    },

    /// Write blocked: content matches a secret pattern.
    ///
    /// The `SecretMatch` carries the detector name and a masked excerpt
    /// (`first6...Nchars`). The full candidate is never stored in the error.
    /// Store a pointer (env-var name, keychain item) rather than the raw value.
    #[error("write blocked: {0}")]
    SecretDetected(crate::secret_gate::SecretMatch),

    /// A bounded per-operation deadline elapsed before the operation
    /// completed (#889). The operation may still be running in the
    /// background (this is a client-observable timeout, not a cancellation
    /// signal to the underlying work) — callers should treat this as "no
    /// answer within budget", not "the operation failed or was rolled back".
    ///
    /// Distinct from `#836`'s narrower `ann_ready_timeout_ms`, which bounds
    /// only a single cold-miss ANN-build wait inside the recall vector leg
    /// and degrades to an in-band FTS-only result. This variant bounds the
    /// *entire* operation end-to-end and is surfaced as a typed error so a
    /// caller under sustained contention gets a fast, clear answer instead
    /// of hanging until an upstream client-side ceiling (observed at 300s in
    /// production, #889) fires instead.
    #[error("{operation} exceeded its {budget_ms}ms deadline (elapsed {elapsed_ms}ms)")]
    DeadlineExceeded {
        operation: String,
        budget_ms: u64,
        elapsed_ms: u64,
    },
}

impl RuntimeError {
    /// Classify a failed inbound channel write without inspecting rendered
    /// error text.
    ///
    /// Existing typed safe-retry failures remain retryable. Secret detection is
    /// the first deterministic policy refusal and is permanent. `None` from
    /// [`Self::retryable_failure_context`] is deliberately not interpreted as
    /// permanent: every other variant starts in the bounded `Unknown` bucket.
    pub fn channel_ingest_failure_class(&self) -> ChannelIngestFailureClass {
        let reason = self.variant_name();
        if self.retryable_failure_context().is_some() {
            ChannelIngestFailureClass::Retryable { reason }
        } else if matches!(self, Self::SecretDetected(_)) {
            ChannelIngestFailureClass::Permanent { reason }
        } else {
            ChannelIngestFailureClass::Unknown { reason }
        }
    }

    /// Stable top-level variant name used by typed policy classifiers.
    const fn variant_name(&self) -> &'static str {
        match self {
            Self::Storage(_) => "Storage",
            Self::Sqlite(_) => "Sqlite",
            Self::Query(_) => "Query",
            Self::NotFound(_) => "NotFound",
            Self::InvalidInput(_) => "InvalidInput",
            Self::UnknownVerb(_) => "UnknownVerb",
            Self::Unconfigured(_) => "Unconfigured",
            Self::UnknownModel(_) => "UnknownModel",
            Self::Embedding(_) => "Embedding",
            Self::Ambiguous(_) => "Ambiguous",
            Self::Fusion(_) => "Fusion",
            Self::UnknownFusionStrategy(_) => "UnknownFusionStrategy",
            Self::Internal(_) => "Internal",
            Self::GuardedWriteFailed(_) => "GuardedWriteFailed",
            Self::MissingPackDependency(_) => "MissingPackDependency",
            Self::MissingPackDependencies(_) => "MissingPackDependencies",
            Self::CircularPackDependency(_) => "CircularPackDependency",
            Self::PackRedeclared { .. } => "PackRedeclared",
            Self::VerbCollision { .. } => "VerbCollision",
            Self::ReservedEnvelopeParam { .. } => "ReservedEnvelopeParam",
            Self::PermissionDenied { .. } => "PermissionDenied",
            Self::GateUnavailable { .. } => "GateUnavailable",
            Self::Khive(_) => "Khive",
            Self::NamespaceMismatch { .. } => "NamespaceMismatch",
            Self::AmbiguousPrefix { .. } => "AmbiguousPrefix",
            Self::CrossBackendMergeUnsupported { .. } => "CrossBackendMergeUnsupported",
            Self::UnknownRemote { .. } => "UnknownRemote",
            Self::RemoteCacheMissing { .. } => "RemoteCacheMissing",
            Self::AmbiguousId { .. } => "AmbiguousId",
            Self::CrossNamespaceWrite { .. } => "CrossNamespaceWrite",
            Self::RemoteFetchError { .. } => "RemoteFetchError",
            Self::WriteBudgetExceeded { .. } => "WriteBudgetExceeded",
            Self::SecretDetected(_) => "SecretDetected",
            Self::DeadlineExceeded { .. } => "DeadlineExceeded",
            Self::IncompatibleEventStore(_) => "IncompatibleEventStore",
        }
    }

    /// Recover a finite-wait pool-checkout timeout without inspecting rendered
    /// error text.
    ///
    /// Store implementations retain [`khive_db::SqliteError`] as the typed
    /// source of `StorageError::Driver`; this method carries that structure
    /// through the runtime wrapper for the MCP wire serializer. A direct
    /// `RuntimeError::Sqlite` follows the same classification path.
    pub fn writer_pool_checkout_timeout_context(&self) -> Option<WriterPoolCheckoutTimeoutContext> {
        let (sqlite_error, capability, operation) = match self {
            Self::Sqlite(error) => (error, None, None),
            Self::Storage(khive_storage::StorageError::Driver {
                capability,
                operation,
                source,
            }) => (
                source.downcast_ref::<khive_db::SqliteError>()?,
                Some(*capability),
                Some(operation.to_string()),
            ),
            _ => return None,
        };

        let khive_db::SqliteError::WriterPoolCheckoutTimeout { timeout } = sqlite_error else {
            return None;
        };
        Some(WriterPoolCheckoutTimeoutContext {
            timeout: *timeout,
            capability,
            operation,
        })
    }

    /// Recover either pre-execution write-admission failure this process can
    /// produce, by typed variant rather than rendered message text (#1643).
    /// Both are safe to classify as retryable: the request was never
    /// accepted, so no partial side effect can exist to roll back.
    pub fn admission_failure_context(&self) -> Option<AdmissionFailureContext> {
        if let Some(context) = self.writer_pool_checkout_timeout_context() {
            return Some(AdmissionFailureContext {
                stage: WRITER_POOL_CHECKOUT_TIMEOUT_STAGE,
                timeout: context.timeout,
                capability: context.capability,
                operation: context.operation,
                scope: None,
                retry_after_ms: None,
            });
        }
        if let Self::Storage(khive_storage::StorageError::WriteQueueFull { timeout_ms }) = self {
            return Some(AdmissionFailureContext {
                stage: WRITER_QUEUE_SATURATED_STAGE,
                timeout: Duration::from_millis(*timeout_ms),
                capability: None,
                operation: None,
                scope: Some(WRITER_ADMISSION_SCOPE),
                retry_after_ms: Some(*timeout_ms),
            });
        }
        None
    }

    /// Recover every typed failure for which this process can prove that
    /// retrying the one failed operation cannot duplicate a side effect.
    pub fn retryable_failure_context(&self) -> Option<RetryableFailureContext> {
        if let Some(context) = self.admission_failure_context() {
            return Some(context.into());
        }
        let Self::Storage(khive_storage::StorageError::WriterTaskBusy { timeout_ms }) = self else {
            return None;
        };
        Some(RetryableFailureContext {
            stage: WRITER_TASK_BEGIN_BUSY_STAGE,
            timeout: Duration::from_millis(*timeout_ms),
            capability: None,
            operation: Some("writer_task_begin".to_string()),
            scope: None,
            retry_after_ms: None,
        })
    }
}

/// Resolve an FTS text-leg search result, failing loud on parser syntax
/// errors instead of silently degrading to vector-only fusion.
///
/// A genuine backend outage (pool exhaustion, connection failure, etc.) is
/// NOT a bad query and is returned as-is via the fallthrough `Err(e)` arm;
/// `is_fts5_syntax_error` is the narrow gate that tells the two apart.
pub fn fts_text_leg_or_err<T>(
    result: Result<Vec<T>, RuntimeError>,
    context: &'static str,
    query: &str,
) -> RuntimeResult<Vec<T>> {
    match result {
        Ok(hits) => Ok(hits),
        Err(RuntimeError::Storage(se)) if se.is_fts5_syntax_error() => {
            tracing::warn!(
                error = %se,
                query = %query,
                context,
                "FTS text leg failed on a parser syntax error; failing loud (#569)"
            );
            Err(RuntimeError::InvalidInput(format!(
                "{context}: FTS query could not be parsed: {se}"
            )))
        }
        Err(e) => Err(e),
    }
}

fn format_uuid_list(uuids: &[uuid::Uuid]) -> String {
    uuids
        .iter()
        .map(uuid::Uuid::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Maps the dependency-light `khive-types` entity-type resolution error onto
/// `RuntimeError::InvalidInput` at the pack boundary: `khive-types` cannot
/// depend on `khive-runtime`, so it cannot produce `RuntimeError` directly.
impl From<khive_types::EntityTypeError> for RuntimeError {
    fn from(e: khive_types::EntityTypeError) -> Self {
        Self::InvalidInput(e.to_string())
    }
}

impl From<khive_types::KhiveError> for RuntimeError {
    fn from(e: khive_types::KhiveError) -> Self {
        Self::Khive(e)
    }
}

#[cfg(test)]
mod channel_ingest_failure_class_tests {
    use super::{ChannelIngestFailureClass, RuntimeError};
    use crate::secret_gate::SecretMatch;
    use std::time::Duration;

    #[test]
    fn secret_detected_is_permanent_by_typed_variant_not_display_text() {
        let first = RuntimeError::SecretDetected(SecretMatch {
            detector: "fixture",
            masked: "first-rendering".to_string(),
        });
        let second = RuntimeError::SecretDetected(SecretMatch {
            detector: "fixture",
            masked: "completely-different-rendering".to_string(),
        });

        assert_ne!(first.to_string(), second.to_string());
        assert_eq!(
            first.channel_ingest_failure_class(),
            ChannelIngestFailureClass::Permanent {
                reason: "SecretDetected"
            }
        );
        assert_eq!(
            second.channel_ingest_failure_class(),
            ChannelIngestFailureClass::Permanent {
                reason: "SecretDetected"
            },
            "rendered error details must not participate in ingest classification"
        );
    }

    #[test]
    fn admission_failures_are_retryable_and_unclassified_errors_are_unknown() {
        let retryable =
            RuntimeError::Storage(khive_storage::StorageError::WriteQueueFull { timeout_ms: 25 });
        assert_eq!(
            retryable.channel_ingest_failure_class(),
            ChannelIngestFailureClass::Retryable { reason: "Storage" }
        );

        let begin_busy =
            RuntimeError::Storage(khive_storage::StorageError::WriterTaskBusy { timeout_ms: 175 });
        let context = begin_busy
            .retryable_failure_context()
            .expect("contended BEGIN must remain typed and retryable");
        assert_eq!(context.stage, "writer_task_begin_busy");
        assert_eq!(context.timeout, Duration::from_millis(175));
        assert_eq!(context.operation.as_deref(), Some("writer_task_begin"));
        assert_eq!(context.scope, None);
        assert_eq!(context.retry_after_ms, None);
        assert_eq!(
            begin_busy.channel_ingest_failure_class(),
            ChannelIngestFailureClass::Retryable { reason: "Storage" }
        );

        let unknown = RuntimeError::InvalidInput(
            "write blocked: SecretDetected text must not affect classification".to_string(),
        );
        assert_eq!(
            unknown.channel_ingest_failure_class(),
            ChannelIngestFailureClass::Unknown {
                reason: "InvalidInput"
            },
            "a rendered message resembling SecretDetected must remain Unknown unless its typed variant is SecretDetected"
        );
    }
}
