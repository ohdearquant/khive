//! Runtime error types.

use std::fmt;

use thiserror::Error;
use uuid::Uuid;

/// Convenience alias for `Result<T, RuntimeError>`.
pub type RuntimeResult<T> = Result<T, RuntimeError>;

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

    #[error("internal: {0}")]
    Internal(String),

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

    /// Gate denied this verb invocation.
    ///
    /// Returned by `VerbRegistry::dispatch` when the configured `Gate` returns
    /// `GateDecision::Deny`. The pack is never invoked. The `reason` field
    /// carries the deny message produced by the gate implementation.
    #[error("permission denied for verb {verb:?}: {reason}")]
    PermissionDenied { verb: String, reason: String },

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
    let shorts: Vec<String> = uuids
        .iter()
        .map(|u| u.to_string()[..8].to_string())
        .collect();
    shorts.join(", ")
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
