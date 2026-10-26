//! CoordinatorService trait seam — dependency inversion for ADR-029 Phase 2.
//!
//! `khive-mcp` defines the contract; `kkernel` provides the concrete implementation.
//! This avoids a crate-cycle: kkernel depends on khive-mcp, so khive-mcp cannot
//! depend on kkernel. The trait is the stable boundary.

use std::fmt;

use async_trait::async_trait;
use uuid::Uuid;

use khive_pack_kg::handlers::{SearchSubstrate, ValidatedSearchRequest};
use khive_runtime::Namespace;
use khive_runtime::{BackendId, NamespaceToken, NoteSearchHit, RuntimeError, SearchHit};
use khive_storage::{Edge, EdgeRelation, EdgeUpsertDisposition};

/// Result of a cross-backend link operation.
pub struct CoordLinkResult {
    /// The edge that was written (on the source backend).
    pub edge: Edge,
    /// True when source and target are on different backends.
    pub cross_backend: bool,
    /// The target backend id when `cross_backend` is true.
    pub target_backend_id: Option<BackendId>,
    /// Transaction-observed natural-key mutation disposition.
    pub mutation: EdgeUpsertDisposition,
}

/// Error variants the coordinator can produce.
pub enum CoordError {
    /// The given UUID was not found on any registered backend.
    UnknownNode { id: Uuid },
    /// The proposed edge violates ADR-002 endpoint rules.
    EdgeRuleViolation(String),
    /// A backend operation failed.
    Backend(String),
}

impl fmt::Display for CoordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoordError::UnknownNode { id } => write!(f, "node {id} not found on any backend"),
            CoordError::EdgeRuleViolation(msg) => write!(f, "edge rule violation: {msg}"),
            CoordError::Backend(msg) => write!(f, "backend error: {msg}"),
        }
    }
}

impl fmt::Debug for CoordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl From<CoordError> for khive_runtime::RuntimeError {
    fn from(e: CoordError) -> Self {
        match e {
            CoordError::UnknownNode { id } => {
                khive_runtime::RuntimeError::NotFound(format!("node {id} not found on any backend"))
            }
            CoordError::EdgeRuleViolation(msg) => khive_runtime::RuntimeError::InvalidInput(msg),
            CoordError::Backend(msg) => khive_runtime::RuntimeError::Internal(msg),
        }
    }
}

/// Stable classification for a failed fan-out backend leg.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendSearchFailureKind {
    /// The backend operation failed for a non-timeout reason.
    BackendError,
    /// The backend exceeded the coordinator's per-request deadline.
    Timeout,
}

impl BackendSearchFailureKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::BackendError => "backend_error",
            Self::Timeout => "timeout",
        }
    }
}

/// Typed failure for one fan-out backend leg.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendSearchFailure {
    pub kind: BackendSearchFailureKind,
    pub message: String,
}

impl BackendSearchFailure {
    pub fn backend(message: impl Into<String>) -> Self {
        Self {
            kind: BackendSearchFailureKind::BackendError,
            message: message.into(),
        }
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self {
            kind: BackendSearchFailureKind::Timeout,
            message: message.into(),
        }
    }
}

/// Per-backend contribution to a fan-out search.
pub struct BackendSearchResult {
    pub backend_id: BackendId,
    pub entity_hits: Vec<SearchHit>,
    pub note_hits: Vec<NoteSearchHit>,
    /// Whether this backend selected the vector arm for this search.
    pub vector_selected: bool,
    /// Populated when this backend errored during the fan-out. A whole-backend
    /// failure (e.g. the text arm, or a fatal error before either arm ran) —
    /// this backend contributed no hits at all. The typed cause is what the
    /// classifier reads; it never parses a rendered message.
    pub error: Option<BackendSearchFailure>,
    /// Populated when only the vector arm failed and the text arm still ran:
    /// `entity_hits` still carries the text arm's results, and `error` above
    /// stays `None`.
    pub vector_error: Option<String>,
}

/// Merged fan-out search result.
pub struct CoordSearchResult {
    /// RRF-merged entity hits across all backends.
    pub entity_hits: Vec<SearchHit>,
    /// RRF-merged note hits across all backends.
    pub note_hits: Vec<NoteSearchHit>,
    /// Per-backend detail (for diagnostics).
    pub per_backend: Vec<BackendSearchResult>,
    /// True when at least one backend errored (results may be incomplete).
    pub partial: bool,
    /// Kind string for each entity hit, keyed by entity UUID.
    /// Populated by the coordinator after the RRF merge. Missing entries mean
    /// the kind could not be resolved (e.g. the owning backend errored).
    pub entity_kinds: std::collections::HashMap<uuid::Uuid, String>,
    /// Kind string for each note hit, keyed by note UUID.
    /// Populated by the coordinator after the RRF merge.
    pub note_kinds: std::collections::HashMap<uuid::Uuid, String>,
    /// `created_at` (micros) for each entity hit, keyed by entity UUID —
    /// row-shape parity with the KG pack's single-backend search serializer
    /// (`crates/khive-pack-kg/src/handlers/search.rs`). Populated alongside
    /// `entity_kinds`; missing entries follow the same resolution rule.
    pub entity_created_at: std::collections::HashMap<uuid::Uuid, i64>,
    /// `updated_at` (micros) for each entity hit, keyed by entity UUID. Same
    /// parity purpose and resolution rule as `entity_created_at`.
    pub entity_updated_at: std::collections::HashMap<uuid::Uuid, i64>,
    /// Persisted revision for each entity hit, keyed by entity UUID. Same
    /// parity purpose and resolution rule as `entity_created_at`.
    pub entity_versions: std::collections::HashMap<uuid::Uuid, i64>,
    /// `created_at` (micros) for each note hit, keyed by note UUID. Same
    /// parity purpose and resolution rule as `entity_created_at`.
    pub note_created_at: std::collections::HashMap<uuid::Uuid, i64>,
    /// `updated_at` (micros) for each note hit, keyed by note UUID. Same
    /// parity purpose and resolution rule as `entity_created_at`.
    pub note_updated_at: std::collections::HashMap<uuid::Uuid, i64>,
    /// Persisted note revisions; a hit without hydrated metadata is not returned.
    pub note_versions: std::collections::HashMap<uuid::Uuid, i64>,
    /// Stored `name` for each note hit, keyed by note UUID — distinct from
    /// `title` (the search-hit display title). Same parity purpose and
    /// resolution rule as `entity_created_at`.
    pub note_names: std::collections::HashMap<uuid::Uuid, Option<String>>,
}

/// Cross-backend coordinator seam visible to `khive-mcp`.
///
/// Implemented by `kkernel::coordinator::SubstrateCoordinatorService`.
/// `khive-mcp` holds an `Option<Arc<dyn CoordinatorService>>` and calls through
/// when in multi-backend mode; single-backend servers hold `None` and dispatch
/// through the `VerbRegistry` unchanged (zero-change invariant).
#[async_trait]
pub trait CoordinatorService: Send + Sync {
    /// Resolve the owning backend for a UUID.
    ///
    /// Namespace-agnostic per ADR-007 Rev 3: presence of the record in a backend
    /// is sufficient — the record's stored namespace is not compared to the caller.
    async fn locate(&self, id: Uuid) -> Option<BackendId>;

    /// Prewarm the locator cache after a successful create so the first
    /// `locate()` for the new record is a cache hit rather than a backend scan.
    fn record_created(&self, id: Uuid, backend_id: BackendId);

    /// The primary backend id (used to prewarm after create).
    fn primary_backend_id(&self) -> Option<BackendId>;

    /// Cross-backend link (D3). Locates both endpoints, validates the relation,
    /// and writes the edge on the source backend with `target_backend` stamped
    /// when the endpoints are on different backends.
    #[allow(clippy::too_many_arguments)]
    async fn link(
        &self,
        namespace: &Namespace,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
        weight: f64,
        metadata: Option<serde_json::Value>,
        resurrect: bool,
    ) -> Result<CoordLinkResult, CoordError>;

    /// Fan-out search across all registered backends (D4).
    ///
    /// `request` is the KG handler's canonical validated search contract. It
    /// carries the resolved substrate plus every supported filter, so this
    /// boundary cannot silently narrow the public wire shape.
    ///
    /// `extra_visible` is the caller's resolved extra read-visibility
    /// namespaces beyond `namespace` itself — the same
    /// `['local'] ∪ visible_namespaces` set the normal registry dispatch path
    /// authorizes with, or empty when the caller named an explicit
    /// `namespace=` (which intentionally narrows visibility). Implementations
    /// must authorize each backend's search token with this widened set
    /// rather than `namespace` alone, or a namespace visible only through
    /// `visible_namespaces` silently drops out of coordinator search results.
    async fn fan_out_search(
        &self,
        request: &ValidatedSearchRequest,
        namespace: &Namespace,
        extra_visible: &[Namespace],
    ) -> CoordSearchResult;

    /// Search with the gate-authorized caller token and original arguments.
    /// Older coordinators can still serve entity searches. Note searches
    /// require an implementation that preserves the caller's mailbox view.
    async fn fan_out_search_scoped(
        &self,
        request: &ValidatedSearchRequest,
        token: &NamespaceToken,
        _args: &serde_json::Value,
        extra_visible: &[Namespace],
    ) -> Result<CoordSearchResult, RuntimeError> {
        if request.substrate() == SearchSubstrate::Note {
            return Err(RuntimeError::InvalidInput(
                "this coordinator does not support scoped note search; use comm.inbox for messages"
                    .into(),
            ));
        }
        Ok(self
            .fan_out_search(request, token.gate_namespace(), extra_visible)
            .await)
    }

    /// True when only one backend is registered (zero-change invariant check).
    fn is_single_backend(&self) -> bool;
}

#[cfg(test)]
#[path = "coordinator_tests.rs"]
pub(crate) mod tests;
