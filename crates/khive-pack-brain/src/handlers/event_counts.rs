use super::*;

/// Fetch the event window for `brain.event_counts`, each bounded query capped at
/// `page_limit` (`BrainPack::MAX_WINDOW_EVENTS` in production; a free function
/// parameterized on the cap so a unit test can shrink it and exercise the
/// crowd-out fix below deterministically, instead of needing tens of thousands
/// of seeded rows to trigger it).
///
/// #26: the unfiltered default view (`unfiltered = true`, i.e. the caller
/// passed no `kind`) must not let the high-volume `audit` kind crowd every
/// other kind out of the shared budget. `query_events` orders by `created_at
/// DESC`, so a single bounded fetch over "all kinds" returns whichever kind
/// fired most densely in the tail of the window — measured at ~12x undercount
/// for `feedback_explicit` when `audit` alone runs 37k-50k/week. Segregate
/// `audit` into its own bounded query so each half gets the full `page_limit`
/// independently — a quiet kind is never crowded out by a busy one. Only
/// applies to the unfiltered default view: a caller who already passed `kind`
/// has no crowding to fix (the single query is already scoped to one kind).
///
/// Returns the concatenated items, the independent Kind-grouped window total,
/// and whether either half was truncated against its own `page_limit`.
/// Transport-safe per-request page bound for every event read this module
/// issues. The events-split daemon refuses `QueryEvents` pages above its cap
/// (`khive_runtime::events_split::MAX_QUERY_EVENTS_PAGE_ROWS`), and the
/// split store's merged read forwards `offset + limit` as one daemon page —
/// so windows wider than the cap are collected by cursor-walking in pages of
/// at most this many rows at `offset: 0`, never by one wide request.
pub(crate) const TRANSPORT_PAGE_ROWS: u32 = khive_runtime::events_split::MAX_QUERY_EVENTS_PAGE_ROWS;

pub(super) fn event_counts_storage_error(error: khive_storage::StorageError) -> RuntimeError {
    match error {
        error @ (khive_storage::StorageError::Timeout { .. }
        | khive_storage::StorageError::AdmissionTimeout { .. }) => RuntimeError::Storage(error),
        other => RuntimeError::InvalidInput(other.to_string()),
    }
}

/// Read one grouped total; row walks and boundary completeness reads remain
/// independent live observations. Unsupported grouped stores refuse directly.
pub(super) async fn grouped_window_total(
    store: &dyn khive_storage::event::EventStore,
    filter: &EventFilter,
) -> Result<u64, RuntimeError> {
    Ok(store
        .count_events_grouped(filter.clone(), khive_storage::event::EventGroupBy::Kind)
        .await
        .map_err(event_counts_storage_error)?
        .values()
        .copied()
        .sum())
}

pub(crate) async fn fetch_event_counts_window(
    store: &dyn khive_storage::event::EventStore,
    base_filter: &EventFilter,
    unfiltered: bool,
    page_limit: u32,
) -> Result<(Vec<Event>, u64, bool), RuntimeError> {
    if !unfiltered {
        let window_event_total = grouped_window_total(store, base_filter).await?;
        let items = collect_events_cursor_walk(
            store,
            base_filter,
            TRANSPORT_PAGE_ROWS,
            u64::from(page_limit),
        )
        .await?;
        let truncated = window_event_total > items.len() as u64;
        return Ok((items, window_event_total, truncated));
    }

    let non_audit_kinds: Vec<khive_types::EventKind> = khive_types::EventKind::ALL
        .iter()
        .copied()
        .filter(|k| *k != khive_types::EventKind::Audit)
        .collect();
    let audit_filter = EventFilter {
        kinds: vec![khive_types::EventKind::Audit],
        ..base_filter.clone()
    };
    let non_audit_filter = EventFilter {
        kinds: non_audit_kinds,
        ..base_filter.clone()
    };

    // Preserve the old closed-kind union: unknown stored kinds belong to
    // neither half of this unfiltered bounded view.
    let totals = store
        .count_events_grouped(
            EventFilter {
                kinds: khive_types::EventKind::ALL.to_vec(),
                ..base_filter.clone()
            },
            khive_storage::event::EventGroupBy::Kind,
        )
        .await
        .map_err(event_counts_storage_error)?;
    let audit_total = totals
        .get(khive_types::EventKind::Audit.name())
        .copied()
        .unwrap_or(0);
    let non_audit_total: u64 = khive_types::EventKind::ALL
        .iter()
        .filter(|kind| **kind != khive_types::EventKind::Audit)
        .map(|kind| totals.get(kind.name()).copied().unwrap_or(0))
        .sum();
    let audit_items = collect_events_cursor_walk(
        store,
        &audit_filter,
        TRANSPORT_PAGE_ROWS,
        u64::from(page_limit),
    )
    .await?;
    let non_audit_items = collect_events_cursor_walk(
        store,
        &non_audit_filter,
        TRANSPORT_PAGE_ROWS,
        u64::from(page_limit),
    )
    .await?;

    let truncated =
        audit_total > audit_items.len() as u64 || non_audit_total > non_audit_items.len() as u64;
    let window_event_total = audit_total + non_audit_total;
    let mut items = audit_items;
    items.extend(non_audit_items);
    Ok((items, window_event_total, truncated))
}

/// Collect up to `max_rows` events for `base_filter` by walking a strict
/// descending `before` cursor at `offset: 0`, requesting at most
/// [`TRANSPORT_PAGE_ROWS`] rows per query so no request exceeds the
/// events-daemon page cap.
///
/// `before` is a strict `created_at <` bound, so stepping the cursor to the
/// last row's timestamp would drop rows sharing that microsecond beyond the
/// page edge. Step to `last.created_at + 1` instead — which re-admits the
/// boundary microsecond — and drop the re-read rows by id. Deliver each row
/// exactly once in the existing page order; signed saturating cost sums rely
/// on that order. A timestamp tie run wider than the transport cap cannot be
/// paged past (widening the page is refused by the
/// daemon) and is reported as a typed error rather than looping.
///
/// Read consistency: the walk issues independent page (and, at the cap,
/// count) reads against a live event plane with no snapshot spanning them.
/// The result is a best-effort live-window view, not a snapshot — a row
/// appended concurrently with the walk may be excluded or included
/// depending on where the cursor stands when it lands (never duplicated,
/// and never an error), and a total observed by an independent
/// `count_events` read can differ from the collected rows. Callers needing
/// a closed population bound the window with `until` in the past, which is
/// closed only insofar as the event plane appends rows stamped at append
/// time rather than backdated.
pub(crate) async fn collect_events_cursor_walk(
    store: &dyn khive_storage::event::EventStore,
    base_filter: &EventFilter,
    page_size: u32,
    max_rows: u64,
) -> Result<Vec<Event>, RuntimeError> {
    let mut items = Vec::new();
    visit_events_cursor_walk(store, base_filter, page_size, max_rows, |event| {
        items.push(event)
    })
    .await?;
    Ok(items)
}

/// Visit owned rows in cursor order, dropping each page before the next read.
pub(crate) async fn visit_events_cursor_walk<F: FnMut(Event)>(
    store: &dyn khive_storage::event::EventStore,
    base_filter: &EventFilter,
    page_size: u32,
    max_rows: u64,
    visit: F,
) -> Result<u64, RuntimeError> {
    khive_runtime::visit_events_cursor_walk(store, base_filter, page_size, max_rows, visit)
        .await
        .map_err(|cause| match cause {
            khive_runtime::EventCursorWalkError::Storage(error) => {
                event_counts_storage_error(error)
            }
            khive_runtime::EventCursorWalkError::MissingBoundary => RuntimeError::Internal(
                "event cursor walk saw duplicate rows before any boundary".to_string(),
            ),
            khive_runtime::EventCursorWalkError::DenseTimestampTie {
                page_limit: fetch_limit,
            } => RuntimeError::InvalidInput(format!(
                "brain.event_counts cannot page this window: more than {fetch_limit} \
                     events share one created_at microsecond, which exceeds the event \
                     transport's page cap; narrow `since`/`until` or add filters (`actor` \
                     or `kind`)"
            )),
        })
}

#[cfg(test)]
pub(crate) async fn fetch_event_counts_window_exhaustive(
    store: &dyn khive_storage::event::EventStore,
    base_filter: &EventFilter,
    page_size: u32,
    max_events: u64,
) -> Result<(Vec<Event>, u64, bool), RuntimeError> {
    let mut items = Vec::new();
    let (_, total, truncated) =
        visit_event_counts_window_exhaustive(store, base_filter, page_size, max_events, |event| {
            items.push(event)
        })
        .await?;
    Ok((items, total, truncated))
}

/// Production exhaustive aggregation retains counts but no completed page.
pub(crate) async fn fold_event_counts_window_exhaustive(
    store: &dyn khive_storage::event::EventStore,
    base_filter: &EventFilter,
    page_size: u32,
    max_events: u64,
    counts: &mut EventCountsAccumulator,
) -> Result<(u64, u64, bool), RuntimeError> {
    visit_event_counts_window_exhaustive(store, base_filter, page_size, max_events, |event| {
        counts.observe(&event)
    })
    .await
}

/// #21: full-window aggregation for `brain.event_counts(exhaustive=true)`.
///
/// Kind-grouped storage counts establish the admission total. Payload-derived
/// aggregates still paginate `query_events` with a fixed page size until
/// exhausted, folding every page instead of stopping at a bounded page.
/// The caller still makes exactly ONE
/// `brain.event_counts` call — the pagination loop is internal to this
/// function — so a coverage panel that needed 19 stitched sampled windows
/// becomes one exhaustive call.
///
/// `window_event_total` sums one Kind-grouped count read with the same filter.
/// It is independent of pagination and does not require the walk to complete.
/// The subsequent row reads remain a best-effort live view, not a snapshot.
///
/// `max_events` is a safety bound, not a design limit: without one,
/// a caller could open a pathological (multi-year, unfiltered) window and this
/// function could spend unbounded work reading and aggregating the window.
/// Set to 40x the bounded-mode cap (`BrainPack::MAX_WINDOW_EVENTS`)
/// — comfortably above the multi-million-event/month volumes `exhaustive` is
/// meant to serve — so it only binds on something pathological. A window above
/// the bound is rejected before aggregation; exhaustive mode never returns a
/// partial result.
pub(super) async fn visit_event_counts_window_exhaustive<F: FnMut(Event)>(
    store: &dyn khive_storage::event::EventStore,
    base_filter: &EventFilter,
    page_size: u32,
    max_events: u64,
    visit: F,
) -> Result<(u64, u64, bool), RuntimeError> {
    let window_event_total = grouped_window_total(store, base_filter).await?;
    if window_event_total > max_events {
        return Err(RuntimeError::InvalidInput(format!(
            "brain.event_counts window exceeds the exhaustive limit of {max_events} events \
             ({window_event_total} matched); narrow `since`/`until` or add filters (`actor` or \
             `kind`)"
        )));
    }

    // Walk the window with a descending `before` cursor instead of a growing
    // offset (see `collect_events_cursor_walk`): every query runs at
    // `offset: 0` in transport-cap-sized pages, so the walk is linear on a
    // single-store backend and stays inside the merged events-split store's
    // bounded materialization window — and under the events daemon's
    // per-request page cap — at any depth. The count check above already
    // bounds the window, so the walk runs to exhaustion.
    let admitted =
        visit_events_cursor_walk(store, base_filter, page_size, max_events, visit).await?;

    let truncated = admitted < window_event_total;
    Ok((admitted, window_event_total, truncated))
}

/// Parse an ISO-8601/RFC-3339 datetime string into a microsecond epoch,
/// naming the offending field/value in the error rather than a bare parse
/// failure (`brain.event_counts`, ADR-103 Stage 1).
///
/// A bare `YYYY-MM-DD` date (no time-of-day component) is accepted rather
/// than rejected — RFC-3339 requires a time component, but a date-only value
/// is unambiguous and a common caller shorthand; the alternative is silently
/// returning nothing until the caller happens to probe the full timestamp
/// form (#984). It anchors to the earliest instant of that calendar date in
/// `tz`, the configured display timezone (ADR-169 D1) — the same rule
/// `gtd`'s date-only `due` follows. The earlier form anchored to midnight
/// UTC, which put the window's edges up to a day off the calendar the caller
/// was naming.
///
/// `roll_to_next_day` handles the `until` bound: the window is applied as
/// half-open `[since, until)` (see `EventCounts` handler), so a date-only
/// `since` correctly means "that day's start" but a date-only `until`
/// must mean "the end of that day" — i.e. the *next* day's earliest
/// instant — otherwise the exclusive upper bound drops the entire named
/// day (#994).
pub(super) fn parse_rfc3339_micros(
    field: &'static str,
    value: &str,
    roll_to_next_day: bool,
    tz: chrono_tz::Tz,
) -> Result<i64, RuntimeError> {
    let trimmed = value.trim();
    if let Ok(date) = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        let date = if roll_to_next_day {
            // Fallible at chrono's representable max (NaiveDate::MAX,
            // 262142-12-31): the roll would overflow. Return the named
            // validation error rather than panicking on caller input (#994).
            date.checked_add_days(chrono::Days::new(1)).ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "invalid `{field}`: {value:?} is past the maximum representable date"
                ))
            })?
        } else {
            date
        };
        return anchor_date_to_earliest_instant(date, tz)
            .map(|dt| dt.timestamp_micros())
            .ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "invalid `{field}`: {value:?} does not exist in the configured display \
                     timezone {tz} (no representable instant carries that local date)"
                ))
            });
    }
    chrono::DateTime::parse_from_rfc3339(trimmed)
        .map(|dt| dt.with_timezone(&Utc).timestamp_micros())
        .map_err(|e| {
            RuntimeError::InvalidInput(format!(
                "invalid `{field}`: {value:?} is not a valid ISO-8601/RFC-3339 datetime ({e}); \
                 expected e.g. \"2026-07-01T00:00:00Z\""
            ))
        })
}

/// Extract `work_class` from an event payload for `brain.event_counts`'
/// `counts_by_work_class` split (ADR-103 Stage 1, ADR-103-resource-attribution-model.md:303).
///
/// Checks two paths, phase-payload first:
/// 1. `payload.work_class` — the shape `PhaseStarted`/`PhaseCompleted`/`PhaseCancelled`
///    events use today (`khive_storage::telemetry::{PhaseStartedPayload, ...}`).
/// 2. `payload.resource.work_class` — the shape successful-dispatch audit rows write:
///    `khive_runtime::cost_unit` constructs the resource object with `work_class` and the
///    runtime persists it on successful audit rows. Phase payloads keep precedence when
///    both paths are present.
///
/// Returns `None` (silently excluded from the split, not an error) when neither path is
/// present — the overwhelming majority of event kinds today.
pub(crate) fn event_work_class(payload: &Value) -> Option<&str> {
    payload
        .get("work_class")
        .and_then(Value::as_str)
        .or_else(|| {
            payload
                .get("resource")
                .and_then(|resource| resource.get("work_class"))
                .and_then(Value::as_str)
        })
}

/// Extract `cost_unit` from an event payload for `brain.event_counts`'
/// `total_cost_unit` / `cost_unit_by_verb` aggregation (ADR-103 Amendment 1).
///
/// Reads `payload.resource.cost_unit` only — unlike `work_class`, `cost_unit`
/// has no top-level payload shape: it is emitted exclusively by
/// `khive_runtime::cost_unit::resource_payload` on the per-dispatch audit
/// row's `resource` sub-object, never by the Phase* background-work events.
///
/// Returns `None` (silently excluded from the sum, not an error or a zero)
/// when the field is absent, which per Amendment 1's "absence has exactly
/// two meanings" rule is either a pre-Amendment-1 event or an
/// errored/denied dispatch.
pub(crate) fn event_cost_unit(payload: &Value) -> Option<i64> {
    payload
        .get("resource")
        .and_then(|resource| resource.get("cost_unit"))
        .and_then(Value::as_i64)
}
