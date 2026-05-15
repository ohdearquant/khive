//! Event storage capability — append-only operation log (ADR-004).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use khive_types::{EventOutcome, SubstrateKind};

use crate::types::{BatchWriteSummary, Page, PageRequest, StorageResult};

/// Storage-level event record. Every verb execution produces one.
/// Immutable once appended — no update or soft-delete.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: Uuid,
    pub namespace: String,
    pub verb: String,
    pub substrate: SubstrateKind,
    pub actor: String,
    pub outcome: EventOutcome,
    pub data: Option<Value>,
    pub duration_us: i64,
    pub target_id: Option<Uuid>,
    pub created_at: i64,
}

impl Event {
    pub fn new(
        namespace: impl Into<String>,
        verb: impl Into<String>,
        substrate: SubstrateKind,
        actor: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            namespace: namespace.into(),
            verb: verb.into(),
            substrate,
            actor: actor.into(),
            outcome: EventOutcome::Success,
            data: None,
            duration_us: 0,
            target_id: None,
            created_at: chrono::Utc::now().timestamp_micros(),
        }
    }

    pub fn with_outcome(mut self, o: EventOutcome) -> Self {
        self.outcome = o;
        self
    }

    pub fn with_data(mut self, d: Value) -> Self {
        self.data = Some(d);
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
}

/// Filter for querying events.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EventFilter {
    pub ids: Vec<Uuid>,
    pub verbs: Vec<String>,
    pub substrates: Vec<SubstrateKind>,
    pub actors: Vec<String>,
    pub namespaces: Vec<String>,
    pub after: Option<i64>,
    pub before: Option<i64>,
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
