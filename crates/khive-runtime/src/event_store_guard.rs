//! Attribution-enforcing decorator for token-scoped event stores.
//!
//! Runtime and pack callers construct the semantic portion of an event, but
//! the authorization boundary owns its namespace and actor attribution. A
//! caller reaching [`crate::KhiveRuntime::events`] therefore cannot select
//! either persisted field: this decorator replaces them from the sealed
//! [`crate::NamespaceToken`] on every append path.

use std::sync::Arc;

use async_trait::async_trait;
use khive_storage::event::IdempotentEventBatchResult;
use khive_storage::{
    BatchWriteSummary, Event, EventFilter, EventStore, Page, PageRequest, StorageResult,
};
use uuid::Uuid;

use crate::NamespaceToken;

/// Runtime-resolved event attribution derived from a sealed authorization
/// token.
///
/// This value is the construction helper for event writes that must share a
/// larger SQL transaction and therefore cannot use [`crate::KhiveRuntime::events`].
/// Its fields are private, so linked code can obtain one only from an already
/// authorized token and cannot select a different namespace or actor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventAttribution {
    namespace: String,
    actor: String,
}

impl EventAttribution {
    /// Resolve the canonical namespace and actor stamp from `token`.
    pub fn from_token(token: &NamespaceToken) -> Self {
        Self {
            namespace: token.namespace().as_str().to_owned(),
            actor: format!("{}:{}", token.actor().kind, token.actor().id),
        }
    }

    /// Replace both attribution fields while preserving semantic event data.
    pub fn stamp(&self, mut event: Event) -> Event {
        event.namespace.clone_from(&self.namespace);
        event.actor.clone_from(&self.actor);
        event
    }
}

pub(crate) struct AttributedEventStore {
    inner: Arc<dyn EventStore>,
    attribution: EventAttribution,
}

impl AttributedEventStore {
    pub(crate) fn wrap(inner: Arc<dyn EventStore>, token: &NamespaceToken) -> Arc<dyn EventStore> {
        Arc::new(Self {
            inner,
            attribution: EventAttribution::from_token(token),
        })
    }

    fn attribute(&self, event: Event) -> Event {
        self.attribution.stamp(event)
    }

    fn attribute_many(&self, events: Vec<Event>) -> Vec<Event> {
        events
            .into_iter()
            .map(|event| self.attribute(event))
            .collect()
    }
}

#[async_trait]
impl EventStore for AttributedEventStore {
    async fn append_event(&self, event: Event) -> StorageResult<()> {
        self.inner.append_event(self.attribute(event)).await
    }

    async fn append_events(&self, events: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        self.inner.append_events(self.attribute_many(events)).await
    }

    async fn get_event(&self, id: Uuid) -> StorageResult<Option<Event>> {
        self.inner.get_event(id).await
    }

    async fn query_events(
        &self,
        filter: EventFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Event>> {
        self.inner.query_events(filter, page).await
    }

    async fn count_events(&self, filter: EventFilter) -> StorageResult<u64> {
        self.inner.count_events(filter).await
    }

    fn preflight_event(&self, event: &Event) -> StorageResult<()> {
        self.inner.preflight_event(&self.attribute(event.clone()))
    }

    async fn append_events_idempotent(
        &self,
        events: Vec<Event>,
    ) -> StorageResult<IdempotentEventBatchResult> {
        self.inner
            .append_events_idempotent(self.attribute_many(events))
            .await
    }

    fn supports_idempotent_audit_batch(&self) -> bool {
        self.inner.supports_idempotent_audit_batch()
    }
}
