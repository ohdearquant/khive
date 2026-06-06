//! Event storage capability — append-only operation log.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use khive_types::{EventKind, EventOutcome, SubstrateKind};

use crate::types::{BatchWriteSummary, Page, PageRequest, StorageResult};

/// Storage-level event record. Every verb execution produces one.
/// Immutable once appended; projection rows are written beside it at append time.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

    pub fn with_outcome(mut self, o: EventOutcome) -> Self {
        self.outcome = o;
        self
    }

    pub fn with_payload(mut self, payload: Value) -> Self {
        self.payload = payload;
        self
    }

    pub fn with_payload_schema_version(mut self, version: u32) -> Self {
        self.payload_schema_version = version;
        self
    }

    pub fn with_profile_state_version(mut self, version: u64) -> Self {
        self.profile_state_version = Some(version);
        self
    }

    pub fn with_duration_us(mut self, us: i64) -> Self {
        self.duration_us = us;
        self
    }

    pub fn with_target(mut self, id: Uuid) -> Self {
        self.target_id = Some(id);
        self
    }

    pub fn with_session_id(mut self, id: Uuid) -> Self {
        self.session_id = Some(id);
        self
    }

    pub fn with_aggregate(mut self, kind: impl Into<String>, id: Uuid) -> Self {
        self.aggregate_kind = Some(kind.into());
        self.aggregate_id = Some(id);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferentKind {
    Entity,
    Note,
}

impl ReferentKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Entity => "entity",
            Self::Note => "note",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationRole {
    Candidate,
    Selected,
    Target,
    Signal,
}

impl ObservationRole {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Selected => "selected",
            Self::Target => "target",
            Self::Signal => "signal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventObservation {
    pub event_id: Uuid,
    pub entity_id: Uuid,
    pub referent_kind: ReferentKind,
    pub role: ObservationRole,
    pub position: u32,
}

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

#[async_trait]
pub trait EventStore: Send + Sync + 'static {
    async fn append_event(&self, event: Event) -> StorageResult<()>;
    async fn append_events(&self, events: Vec<Event>) -> StorageResult<BatchWriteSummary>;
    async fn get_event(&self, id: Uuid) -> StorageResult<Option<Event>>;
    async fn query_events(
        &self,
        filter: EventFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Event>>;
    async fn count_events(&self, filter: EventFilter) -> StorageResult<u64>;
}
