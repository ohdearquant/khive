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
    operation: Option<khive_types::OperationAttribution>,
}

impl EventAttribution {
    /// Resolve the canonical namespace and actor stamp from `token`, capturing
    /// the current operation for explicitly deferred transactional event work.
    pub fn from_token(token: &NamespaceToken) -> Self {
        Self {
            namespace: token.namespace().as_str().to_owned(),
            actor: format!("{}:{}", token.actor().kind, token.actor().id),
            operation: khive_storage::operation_context::current_operation_attribution(),
        }
    }

    /// Replace both attribution fields while preserving semantic event data.
    pub fn stamp(&self, mut event: Event) -> Event {
        event.namespace.clone_from(&self.namespace);
        event.actor.clone_from(&self.actor);
        // Explicit capture carries the originating operation through writer-task
        // closures. Reusable store decorators disable this capture below.
        if let Some(operation) = self.operation {
            event.op_index = Some(operation.op_index);
            event.ref_resolution = Some(operation.ref_resolution);
        }
        event
    }
}

pub(crate) struct AttributedEventStore {
    inner: Arc<dyn EventStore>,
    attribution: EventAttribution,
}

impl AttributedEventStore {
    pub(crate) fn wrap(inner: Arc<dyn EventStore>, token: &NamespaceToken) -> Arc<dyn EventStore> {
        let mut attribution = EventAttribution::from_token(token);
        // A store may outlive the operation that obtained it. Its authority
        // remains token-bound, but each event captures its own operation when
        // constructed; reusing the store must not reuse an earlier position.
        attribution.operation = None;
        Arc::new(Self { inner, attribution })
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

#[cfg(test)]
mod operation_tests {
    use super::*;
    use khive_storage::operation_context::scope_operation_attribution;
    use khive_types::{EventKind, OperationAttribution, RefResolution, SubstrateKind};

    fn event() -> Event {
        Event::new(
            "local",
            "test.operation",
            EventKind::Audit,
            SubstrateKind::Event,
            "fixture",
        )
    }

    #[tokio::test]
    async fn reusable_store_does_not_reuse_operation_but_explicit_capture_survives_defer() {
        let runtime = crate::KhiveRuntime::memory().unwrap();
        let token = runtime.authorize(crate::Namespace::local()).unwrap();
        let operation = OperationAttribution {
            op_index: 2,
            ref_resolution: RefResolution::Resolved,
        };
        let (store, captured) = scope_operation_attribution(operation, async {
            (
                runtime.events(&token).unwrap(),
                EventAttribution::from_token(&token),
            )
        })
        .await;

        let outside = event();
        store.append_event(outside.clone()).await.unwrap();
        let stored = store.get_event(outside.id).await.unwrap().unwrap();
        assert_eq!((stored.op_index, stored.ref_resolution), (None, None));

        let deferred = tokio::spawn(async move { captured.stamp(event()) })
            .await
            .unwrap();
        assert_eq!(
            (deferred.op_index, deferred.ref_resolution),
            (Some(2), Some(RefResolution::Resolved))
        );
        store.append_event(deferred.clone()).await.unwrap();
        let stored = store.get_event(deferred.id).await.unwrap().unwrap();
        assert_eq!(
            (stored.op_index, stored.ref_resolution),
            (Some(2), Some(RefResolution::Resolved))
        );

        let next = scope_operation_attribution(
            OperationAttribution {
                op_index: 7,
                ref_resolution: RefResolution::Literal,
            },
            async { event() },
        )
        .await;
        store.append_event(next.clone()).await.unwrap();
        let stored = store.get_event(next.id).await.unwrap().unwrap();
        assert_eq!(
            (stored.op_index, stored.ref_resolution),
            (Some(7), Some(RefResolution::Literal))
        );
    }
}
