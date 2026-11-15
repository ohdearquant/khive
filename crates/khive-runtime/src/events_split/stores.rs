use super::events_count_read_budget::CountReadBudget;
#[cfg(unix)]
use super::events_count_read_budget::{self, restore_count_read_failure};
#[cfg(unix)]
use super::forwarder::EventsSplitClient;
#[cfg(unix)]
use super::paths::EVENTS_PROTOCOL_VERSION;
#[cfg(unix)]
use super::protocol::{EventsRequest, EventsResponse, WireWriterTaskFailure};
use async_trait::async_trait;
use khive_storage::event::{
    EventGroupBy, EventPageQuery, EventPageWindow, IdempotentEventBatchResult,
};
use khive_storage::{
    BatchWriteSummary, Event, EventFilter, EventStore, Page, PageRequest, StorageError,
    StorageResult,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

/// [`EventStore`] implementation the runtime hands out when the events split
/// runs in daemon mode. Appends are fire-and-forget through the client's
/// bounded queue; the ADR-133 idempotent lane and all reads are synchronous
/// round-trips to the events daemon.
#[cfg(unix)]
#[derive(Debug)]
pub struct ForwardingEventStore {
    pub(super) namespace: String,
    pub(super) client: Arc<EventsSplitClient>,
}

#[cfg(unix)]
impl ForwardingEventStore {
    pub fn new(namespace: impl Into<String>, client: Arc<EventsSplitClient>) -> Self {
        Self {
            namespace: namespace.into(),
            client,
        }
    }

    pub(super) fn unexpected_count(
        &self,
        op: &'static str,
        response: EventsResponse,
    ) -> StorageError {
        match restore_count_read_failure(response) {
            Err(error) => error,
            Ok(other) => self.unexpected(op, other),
        }
    }

    pub(super) fn unexpected(&self, op: &'static str, response: EventsResponse) -> StorageError {
        match response {
            EventsResponse::Error {
                message,
                retryable,
                writer_task_failure,
                sqlite_write_failure,
                read_failure,
            } => {
                if read_failure.is_some() {
                    return events_count_read_budget::malformed_read_failure(
                        "non-count response carries read failure evidence",
                    );
                }
                // Reconstruct the same legacy policy first; evidence must not
                // widen retries or turn a known rollback into task termination.
                let source = if retryable {
                    StorageError::Pool {
                        operation: op.into(),
                        message,
                    }
                } else {
                    StorageError::InvalidInput {
                        capability: khive_storage::StorageCapability::Events,
                        operation: op.into(),
                        message,
                    }
                };
                let source = match sqlite_write_failure {
                    Some(failure) => source.with_sqlite_write_failure(failure),
                    None => source,
                };
                match writer_task_failure {
                    Some(WireWriterTaskFailure::RequestFailed { request_state }) => {
                        StorageError::WriterTaskRequestFailed {
                            request_state: request_state.into(),
                            source: Box::new(source),
                        }
                    }
                    Some(WireWriterTaskFailure::TaskTerminated {
                        request_state,
                        sqlite_full_codes,
                    }) => {
                        let terminal = StorageError::WriterTaskTerminated {
                            request_state: request_state.into(),
                            sqlite_full_codes,
                        };
                        match sqlite_write_failure {
                            Some(failure) => terminal.with_sqlite_write_failure(failure),
                            None => terminal,
                        }
                    }
                    None => source,
                }
            }
            other => StorageError::Serialization {
                capability: khive_storage::StorageCapability::Events,
                message: format!("events daemon returned mismatched response for {op}: {other:?}"),
            },
        }
    }
}

#[cfg(unix)]
#[async_trait]
impl EventStore for ForwardingEventStore {
    async fn append_event(&self, event: Event) -> StorageResult<()> {
        self.client.enqueue(&self.namespace, vec![event]);
        Ok(())
    }

    async fn append_events(&self, events: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        let attempted = events.len() as u64;
        self.client.enqueue(&self.namespace, events);
        // Fire-and-forget: the hand-off succeeded or was counted as a drop;
        // either way the caller's contract is "accepted for forwarding".
        Ok(BatchWriteSummary {
            attempted,
            affected: attempted,
            ..BatchWriteSummary::default()
        })
    }

    async fn get_event(&self, id: Uuid) -> StorageResult<Option<Event>> {
        let request = EventsRequest::GetEvent {
            protocol_version: EVENTS_PROTOCOL_VERSION,
            namespace: self.namespace.clone(),
            id,
        };
        match self.client.round_trip(&request).await? {
            EventsResponse::Event { event } => Ok(event),
            other => Err(self.unexpected("get_event", other)),
        }
    }

    async fn query_events(
        &self,
        filter: EventFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Event>> {
        let request = EventsRequest::QueryEvents {
            protocol_version: EVENTS_PROTOCOL_VERSION,
            namespace: self.namespace.clone(),
            filter,
            page,
        };
        match self.client.round_trip(&request).await? {
            EventsResponse::Pageful { page } => Ok(page),
            other => Err(self.unexpected("query_events", other)),
        }
    }

    async fn count_events(&self, filter: EventFilter) -> StorageResult<u64> {
        let budget = CountReadBudget::capture();
        let request = EventsRequest::CountEvents {
            protocol_version: EVENTS_PROTOCOL_VERSION,
            namespace: self.namespace.clone(),
            operator_read_budget_secs: Some(budget.operator_read_budget_secs),
            filter,
        };
        match self.client.round_trip_count(&request, &budget).await? {
            EventsResponse::Count { count } => Ok(count),
            other => Err(self.unexpected_count("count_events", other)),
        }
    }

    async fn count_events_grouped(
        &self,
        filter: EventFilter,
        group_by: EventGroupBy,
    ) -> StorageResult<BTreeMap<String, u64>> {
        let budget = CountReadBudget::capture();
        let request = EventsRequest::CountEventsGrouped {
            protocol_version: EVENTS_PROTOCOL_VERSION,
            namespace: self.namespace.clone(),
            operator_read_budget_secs: Some(budget.operator_read_budget_secs),
            filter,
            group_by,
        };
        match self.client.round_trip_count(&request, &budget).await? {
            EventsResponse::GroupedCount { counts } => Ok(counts),
            other => Err(self.unexpected_count("count_events_grouped", other)),
        }
    }

    async fn query_event_page(&self, query: EventPageQuery) -> StorageResult<EventPageWindow> {
        crate::event_page::validate_page_query(&query)?;
        let request = EventsRequest::QueryEventPage {
            protocol_version: EVENTS_PROTOCOL_VERSION,
            namespace: self.namespace.clone(),
            query: query.clone(),
        };
        match self.client.round_trip(&request).await? {
            EventsResponse::EventPageWindow { window } => {
                crate::event_page::validate_window(&query, Some(&self.namespace), &window)?;
                Ok(window)
            }
            error @ EventsResponse::Error { .. } => Err(self.unexpected("query_event_page", error)),
            _ => Err(crate::event_page::page_error(
                "events page response kind mismatch",
            )),
        }
    }

    fn preflight_event(&self, event: &Event) -> StorageResult<()> {
        // Same validation code path as the daemon-side store, zero I/O.
        self.client.preflight_store.preflight_event(event)
    }

    async fn append_events_idempotent(
        &self,
        events: Vec<Event>,
    ) -> StorageResult<IdempotentEventBatchResult> {
        let request = EventsRequest::AppendEventsIdempotent {
            protocol_version: EVENTS_PROTOCOL_VERSION,
            namespace: self.namespace.clone(),
            events,
        };
        match self.client.round_trip(&request).await? {
            EventsResponse::Idempotent { result } => Ok(result),
            other => Err(self.unexpected("append_events_idempotent", other)),
        }
    }

    fn supports_idempotent_audit_batch(&self) -> bool {
        true
    }
}

/// The store the runtime hands out when the events split is configured:
/// routes by APPEND CLASS rather than moving the whole event plane.
///
/// - The ADR-133 idempotent audit-batch lane — the measured bulk of event
///   write volume (verb-dispatch audit plus the config-lock rows that ride
///   the same flusher) — goes to the events lane (the events database, forwarded or
///   direct).
/// - Plain appends stay on the legacy store, because the legacy `events`
///   table has raw-SQL consumers whose correctness depends on finding those
///   rows there: the schedule drain's creator-provenance fence, the kg
///   projection worker's guarded event INSERT (transactional with main-db
///   state), and GraphQuery's cross-substrate UNION. Those events are
///   low-volume domain facts; the split's contention relief does not need
///   them moved, and moving them breaks the consumers by construction.
/// - Reads merge both stores so trait-level consumers
///   (`brain.event_counts`, event getters) observe one event plane.
pub struct SplitEventStore {
    pub(super) legacy: Arc<dyn EventStore>,
    pub(super) lane: Arc<dyn EventStore>,
}

impl std::fmt::Debug for SplitEventStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SplitEventStore").finish_non_exhaustive()
    }
}

impl SplitEventStore {
    /// Bound on `offset + limit` for a merged `query_events` window. Offset
    /// pagination over two stores materializes the whole prefix in memory
    /// (see `query_events`), so an unbounded offset would let a single
    /// request buffer both stores wholesale. The bound comfortably admits
    /// the largest legitimate bounded window in the tree
    /// (`brain.event_counts`' 50k page); deep walks page with a `before`
    /// cursor at `offset: 0`, which never grows the materialized prefix.
    pub const MAX_MERGED_WINDOW_ROWS: u64 = 100_000;

    pub fn new(legacy: Arc<dyn EventStore>, lane: Arc<dyn EventStore>) -> Self {
        Self { legacy, lane }
    }
}

#[async_trait]
impl EventStore for SplitEventStore {
    async fn append_event(&self, event: Event) -> StorageResult<()> {
        self.legacy.append_event(event).await
    }

    async fn append_events(&self, events: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        self.legacy.append_events(events).await
    }

    async fn get_event(&self, id: Uuid) -> StorageResult<Option<Event>> {
        // Domain events (legacy) are the likelier and cheaper hit; fall back
        // to the lane for audit-batch rows.
        if let Some(event) = self.legacy.get_event(id).await? {
            return Ok(Some(event));
        }
        self.lane.get_event(id).await
    }

    async fn query_events(
        &self,
        filter: EventFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Event>> {
        // Offset pagination cannot be split across two stores: fetch each
        // store's prefix covering the requested window, merge in the stores'
        // shared order (created_at DESC, id DESC), then window in memory.
        // Cost is O(offset + limit) rows materialized from each store — the
        // floor for offset semantics over two sources, since the split point
        // is unknowable without both prefixes. The sort below is a single
        // merge pass in practice (std's stable sort is adaptive on the
        // concatenation of two sorted runs). That materialization is why the
        // window is bounded below: without a bound, one authenticated
        // request with a pathological offset would make both stores buffer
        // and sort every matching row. Deep walks page with a `before`
        // cursor (strict `created_at <` bound in `EventFilter`) at
        // `offset: 0`, which stays inside the bound at any depth.
        let window = page.offset.saturating_add(u64::from(page.limit));
        if window > Self::MAX_MERGED_WINDOW_ROWS {
            return Err(StorageError::InvalidInput {
                capability: khive_storage::StorageCapability::Events,
                operation: "query_events".into(),
                message: format!(
                    "offset+limit ({window}) exceeds the merged event plane's window bound \
                     of {}; page deep windows with a `before` cursor at offset 0, or narrow \
                     the filter",
                    Self::MAX_MERGED_WINDOW_ROWS
                ),
            });
        }
        let prefix = PageRequest {
            offset: 0,
            limit: window.min(u64::from(u32::MAX)) as u32,
        };
        let legacy = self
            .legacy
            .query_events(filter.clone(), prefix.clone())
            .await?;
        let lane = self.lane.query_events(filter, prefix).await?;
        let total = match (legacy.total, lane.total) {
            (Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
        let mut items = legacy.items;
        items.extend(lane.items);
        items.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        let items = items
            .into_iter()
            .skip(page.offset as usize)
            .take(page.limit as usize)
            .collect();
        Ok(Page { items, total })
    }

    async fn count_events(&self, filter: EventFilter) -> StorageResult<u64> {
        let budget = CountReadBudget::capture();
        budget
            .scope(async {
                let legacy = self
                    .legacy
                    .count_events(filter.clone())
                    .await
                    .map_err(|error| budget.attribute(error))?;
                let lane = self.lane.count_events(filter).await?;
                Ok(legacy + lane)
            })
            .await
    }

    async fn count_events_grouped(
        &self,
        filter: EventFilter,
        group_by: EventGroupBy,
    ) -> StorageResult<BTreeMap<String, u64>> {
        let budget = CountReadBudget::capture();
        budget
            .scope(async {
                let mut counts = self
                    .legacy
                    .count_events_grouped(filter.clone(), group_by)
                    .await
                    .map_err(|error| budget.attribute(error))?;
                for (key, count) in self.lane.count_events_grouped(filter, group_by).await? {
                    let total = counts.entry(key).or_default();
                    *total =
                        total
                            .checked_add(count)
                            .ok_or_else(|| StorageError::InvalidInput {
                                capability: khive_storage::StorageCapability::Events,
                                operation: "count_events_grouped".into(),
                                message: "grouped event count overflow".into(),
                            })?;
                }
                Ok(counts)
            })
            .await
    }

    async fn query_event_page(&self, query: EventPageQuery) -> StorageResult<EventPageWindow> {
        crate::event_page::split_page(self.legacy.as_ref(), self.lane.as_ref(), query).await
    }

    fn preflight_event(&self, event: &Event) -> StorageResult<()> {
        // Validation is store-independent; the lane's implementation is the
        // local zero-I/O validator in forwarding mode.
        self.lane.preflight_event(event)
    }

    async fn append_events_idempotent(
        &self,
        events: Vec<Event>,
    ) -> StorageResult<IdempotentEventBatchResult> {
        // A retried batch may have landed — fully or partially — on the
        // legacy store before the split cutover, and merged reads stay
        // duplicate-free only while an id lives in exactly one store. Probe
        // the legacy store for the batch's ids and route resident rows back
        // through the legacy store's own idempotent machinery, which compares
        // every persisted column without re-inserting; only genuinely new
        // rows reach the lane. Without the probe, a legacy-resident id would
        // be inserted a second time into the lane and every merged query and
        // count would double-count it.
        if events.is_empty() {
            return self.lane.append_events_idempotent(events).await;
        }
        let ids: Vec<Uuid> = events.iter().map(|event| event.id).collect();
        let probe_limit = u32::try_from(ids.len()).map_err(|_| StorageError::InvalidInput {
            capability: khive_storage::StorageCapability::Events,
            operation: "append_events_idempotent".into(),
            message: format!(
                "batch of {} rows exceeds the legacy probe window",
                ids.len()
            ),
        })?;
        let existing = self
            .legacy
            .query_events(
                EventFilter {
                    ids,
                    ..EventFilter::default()
                },
                PageRequest {
                    offset: 0,
                    limit: probe_limit,
                },
            )
            .await?;
        let legacy_resident: std::collections::HashSet<Uuid> =
            existing.items.iter().map(|event| event.id).collect();
        if legacy_resident.is_empty() {
            return self.lane.append_events_idempotent(events).await;
        }
        let mut legacy_rows = Vec::new();
        let mut lane_rows = Vec::new();
        let mut routed_to_legacy = Vec::with_capacity(events.len());
        for event in events {
            if legacy_resident.contains(&event.id) {
                routed_to_legacy.push(true);
                legacy_rows.push(event);
            } else {
                routed_to_legacy.push(false);
                lane_rows.push(event);
            }
        }
        let legacy_expected = legacy_rows.len();
        let lane_expected = lane_rows.len();
        let legacy_result = self.legacy.append_events_idempotent(legacy_rows).await?;
        let lane_result = if lane_expected == 0 {
            IdempotentEventBatchResult { rows: Vec::new() }
        } else {
            self.lane.append_events_idempotent(lane_rows).await?
        };
        if legacy_result.rows.len() != legacy_expected || lane_result.rows.len() != lane_expected {
            return Err(StorageError::Driver {
                capability: khive_storage::StorageCapability::Events,
                operation: "append_events_idempotent".into(),
                source: format!(
                    "idempotent sub-batch result length mismatch: legacy {}/{}, lane {}/{}",
                    legacy_result.rows.len(),
                    legacy_expected,
                    lane_result.rows.len(),
                    lane_expected
                )
                .into(),
            });
        }
        let mut legacy_iter = legacy_result.rows.into_iter();
        let mut lane_iter = lane_result.rows.into_iter();
        let rows = routed_to_legacy
            .into_iter()
            .map(|to_legacy| {
                if to_legacy {
                    legacy_iter.next().expect("length checked above")
                } else {
                    lane_iter.next().expect("length checked above")
                }
            })
            .collect();
        Ok(IdempotentEventBatchResult { rows })
    }

    fn supports_idempotent_audit_batch(&self) -> bool {
        // The legacy-resident probe above routes retried rows through the
        // legacy store's idempotent path, so the split supports the audit
        // batch only when BOTH stores do.
        self.lane.supports_idempotent_audit_batch() && self.legacy.supports_idempotent_audit_batch()
    }
}
