//! Event storage capability — append-only operation log.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use khive_types::{EventKind, EventOutcome, SubstrateKind};

use crate::capability::StorageCapability;
use crate::error::StorageError;
use crate::types::{BatchWriteSummary, Page, PageRequest, StorageResult};

/// Storage-level event record. Every verb execution produces one.
/// Immutable once appended; projection rows are written beside it at append time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: Uuid,
    pub namespace: String,
    pub verb: String,
    pub substrate: SubstrateKind,
    pub actor: String,
    pub kind: EventKind,
    pub outcome: EventOutcome,
    pub payload: Value,
    pub payload_schema_version: u32,
    pub profile_state_version: Option<u64>,
    pub duration_us: i64,
    pub target_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub aggregate_kind: Option<String>,
    pub aggregate_id: Option<Uuid>,
    pub created_at: i64,
}

impl Event {
    /// Create a new event with a generated UUID and current timestamp.
    pub fn new(
        namespace: impl Into<String>,
        verb: impl Into<String>,
        kind: EventKind,
        substrate: SubstrateKind,
        actor: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            namespace: namespace.into(),
            verb: verb.into(),
            substrate,
            actor: actor.into(),
            kind,
            outcome: EventOutcome::Success,
            payload: Value::Object(Default::default()),
            payload_schema_version: 1,
            profile_state_version: None,
            duration_us: 0,
            target_id: None,
            session_id: None,
            aggregate_kind: None,
            aggregate_id: None,
            created_at: chrono::Utc::now().timestamp_micros(),
        }
    }

    /// Set the event outcome (success/failure).
    pub fn with_outcome(mut self, o: EventOutcome) -> Self {
        self.outcome = o;
        self
    }

    /// Set the event payload JSON.
    pub fn with_payload(mut self, payload: Value) -> Self {
        self.payload = payload;
        self
    }

    /// Set the payload schema version for forward compatibility.
    pub fn with_payload_schema_version(mut self, version: u32) -> Self {
        self.payload_schema_version = version;
        self
    }

    /// Set the brain profile state version at event time.
    pub fn with_profile_state_version(mut self, version: u64) -> Self {
        self.profile_state_version = Some(version);
        self
    }

    /// Set the operation duration in microseconds.
    pub fn with_duration_us(mut self, us: i64) -> Self {
        self.duration_us = us;
        self
    }

    /// Set the target entity/note ID for this event.
    pub fn with_target(mut self, id: Uuid) -> Self {
        self.target_id = Some(id);
        self
    }

    /// Set the session ID for correlating related events.
    pub fn with_session_id(mut self, id: Uuid) -> Self {
        self.session_id = Some(id);
        self
    }

    /// Set the aggregate kind and ID for event-sourced projections.
    pub fn with_aggregate(mut self, kind: impl Into<String>, id: Uuid) -> Self {
        self.aggregate_kind = Some(kind.into());
        self.aggregate_id = Some(id);
        self
    }
}

/// Which substrate (entity or note) the referent record lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferentKind {
    Entity,
    Note,
}

impl ReferentKind {
    /// Return the lowercase string name for this referent kind.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Entity => "entity",
            Self::Note => "note",
        }
    }
}

/// Role of a referent in a brain observation (candidate, selected, target, signal).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationRole {
    Candidate,
    Selected,
    Target,
    Signal,
}

impl ObservationRole {
    /// Return the lowercase string name for this observation role.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Selected => "selected",
            Self::Target => "target",
            Self::Signal => "signal",
        }
    }
}

/// A single entity observation recorded alongside an event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventObservation {
    pub event_id: Uuid,
    pub entity_id: Uuid,
    pub referent_kind: ReferentKind,
    pub role: ObservationRole,
    pub position: u32,
}

/// An event together with its associated observations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventView {
    pub event: Event,
    pub observations: Vec<EventObservation>,
}

/// Filter for querying events. Namespace is implicit in the scoped EventStore.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EventFilter {
    pub ids: Vec<Uuid>,
    pub kinds: Vec<EventKind>,
    pub verbs: Vec<String>,
    pub substrates: Vec<SubstrateKind>,
    pub actors: Vec<String>,
    pub after: Option<i64>,
    pub before: Option<i64>,
    pub session_id: Option<Uuid>,
    pub observed: Vec<Uuid>,
    pub selected: Vec<Uuid>,
    pub payload_proposal_id: Option<Uuid>,
}

/// Per-row outcome of an [`EventStore::append_events_idempotent`] call, in
/// input order. Distinguishes a fresh insert from a retry that reproduced an
/// identical row from a retry whose identity now disagrees with what is
/// already stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventAppendDisposition {
    /// No prior row with this id existed; it was inserted.
    Inserted,
    /// A prior row with this id existed and every persisted column plus the
    /// ordered observation projection matched exactly. Not re-inserted.
    AlreadyPresentIdentical,
    /// A prior row with this id existed but disagreed with the submitted
    /// row. Not inserted; unrelated rows in the same batch are unaffected.
    IdentityConflict,
}

/// Result of [`EventStore::append_events_idempotent`]. `rows` preserves the
/// input order and length of the submitted batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdempotentEventBatchResult {
    pub rows: Vec<EventAppendDisposition>,
}

/// Append-only operation log for verb executions.
#[async_trait]
pub trait EventStore: Send + Sync + 'static {
    /// Append a single event to the log.
    async fn append_event(&self, event: Event) -> StorageResult<()>;
    /// Append a batch of events to the log.
    async fn append_events(&self, events: Vec<Event>) -> StorageResult<BatchWriteSummary>;
    /// Fetch an event by UUID, returning `None` if absent.
    async fn get_event(&self, id: Uuid) -> StorageResult<Option<Event>>;
    /// Query events matching a filter with pagination.
    async fn query_events(
        &self,
        filter: EventFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Event>>;
    /// Count events matching a filter.
    async fn count_events(&self, filter: EventFilter) -> StorageResult<u64>;

    /// Validate `event` against the exact insert/observation shape the
    /// backend would build at append time, performing no I/O. Rejects a
    /// malformed row before it is ever enqueued for a write, so one bad
    /// producer input cannot poison a batch shared with other callers.
    ///
    /// Backends that do not implement pre-enqueue validation return
    /// [`StorageError::Unsupported`].
    fn preflight_event(&self, event: &Event) -> StorageResult<()> {
        let _ = event;
        Err(StorageError::Unsupported {
            capability: StorageCapability::Events,
            operation: "preflight_event".into(),
            message: "this EventStore backend does not implement preflight_event".into(),
        })
    }

    /// Append a batch of events with idempotent retry semantics: a row
    /// carrying an id that already exists is compared against every
    /// persisted column and its ordered observation projection rather than
    /// treated as a write conflict. Exact equality reports
    /// [`EventAppendDisposition::AlreadyPresentIdentical`] instead of
    /// re-inserting; any mismatch reports
    /// [`EventAppendDisposition::IdentityConflict`] for that row alone,
    /// while unrelated rows in the same batch still commit.
    ///
    /// Backends that do not implement idempotent batching return
    /// [`StorageError::Unsupported`].
    async fn append_events_idempotent(
        &self,
        events: Vec<Event>,
    ) -> StorageResult<IdempotentEventBatchResult> {
        let _ = events;
        Err(StorageError::Unsupported {
            capability: StorageCapability::Events,
            operation: "append_events_idempotent".into(),
            message: "this EventStore backend does not implement append_events_idempotent".into(),
        })
    }

    /// Whether this backend implements `preflight_event` and
    /// `append_events_idempotent` for real, rather than inheriting their
    /// `Unsupported`-returning defaults above.
    ///
    /// A caller that builds an ADR-133 audit-batch seam over a backend that
    /// answers `false` here would have every audited row rejected at
    /// preflight while the dispatch it audits still reports success — the
    /// exact silent-loss failure mode the batch exists to prevent. Defaults
    /// to `false` so an unmodified legacy backend is caught at registry
    /// build time instead of appearing healthy.
    fn supports_idempotent_audit_batch(&self) -> bool {
        false
    }
}
