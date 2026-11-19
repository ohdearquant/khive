use super::super::StorageBackend;
use super::map_open_error;
use crate::stores::event::event_insert_statements;
use async_trait::async_trait;
use khive_storage::event::{
    Event, EventFilter, EventGroupBy, EventPageQuery, EventPageWindow, EventStore,
    IdempotentEventBatchResult,
};
use khive_storage::{
    BatchWriteSummary, Page, PageRequest, StorageCapability, StorageError, StorageResult,
};
use std::collections::BTreeMap;
use uuid::Uuid;

#[async_trait]
impl EventStore for StorageBackend {
    async fn append_event(&self, event: Event) -> StorageResult<()> {
        self.events()
            .map_err(|error| map_open_error(error, StorageCapability::Events, "events"))?
            .append_event(event)
            .await
    }

    async fn append_events(&self, events: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        self.events()
            .map_err(|error| map_open_error(error, StorageCapability::Events, "events"))?
            .append_events(events)
            .await
    }

    async fn get_event(&self, id: Uuid) -> StorageResult<Option<Event>> {
        self.events()
            .map_err(|error| map_open_error(error, StorageCapability::Events, "events"))?
            .get_event(id)
            .await
    }

    async fn query_events(
        &self,
        filter: EventFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Event>> {
        self.events()
            .map_err(|error| map_open_error(error, StorageCapability::Events, "events"))?
            .query_events(filter, page)
            .await
    }

    async fn query_event_page(&self, query: EventPageQuery) -> StorageResult<EventPageWindow> {
        self.events()
            .map_err(|error| map_open_error(error, StorageCapability::Events, "events"))?
            .query_event_page(query)
            .await
    }

    async fn count_events(&self, filter: EventFilter) -> StorageResult<u64> {
        self.events()
            .map_err(|error| map_open_error(error, StorageCapability::Events, "events"))?
            .count_events(filter)
            .await
    }

    async fn count_events_grouped(
        &self,
        filter: EventFilter,
        group_by: EventGroupBy,
    ) -> StorageResult<BTreeMap<String, u64>> {
        self.events()
            .map_err(|error| map_open_error(error, StorageCapability::Events, "events"))?
            .count_events_grouped(filter, group_by)
            .await
    }

    fn preflight_event(&self, event: &Event) -> StorageResult<()> {
        event_insert_statements(event).map(|_| ()).map_err(|error| {
            StorageError::driver(StorageCapability::Events, "preflight_event", error)
        })
    }

    async fn append_events_idempotent(
        &self,
        events: Vec<Event>,
    ) -> StorageResult<IdempotentEventBatchResult> {
        self.events()
            .map_err(|error| map_open_error(error, StorageCapability::Events, "events"))?
            .append_events_idempotent(events)
            .await
    }

    fn supports_idempotent_audit_batch(&self) -> bool {
        true
    }
}
