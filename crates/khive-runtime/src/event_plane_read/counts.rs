use chrono::Utc;
use khive_storage::event::{Event, EventFilter};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{micros_to_iso, KhiveRuntime, NamespaceToken, RuntimeError};

use super::grouping::EventCountsAccumulator;
use super::parsing::{
    event_actor_read_scope, parse_event_kind, parse_handler_params, parse_rfc3339_micros,
};

// ── brain.event_counts ──────────────────────────────────────────────────
//
// ADR-103 Stage 1 (#724 Ask A): a windowed, per-actor, per-kind, per-verb
// read verb over the event plane, additionally surfacing `work_class`
// (ADR-103-resource-attribution-model.md:303) via `counts_by_work_class`.
// `work_class` is read from `payload.work_class` first — the shape the
// three Phase* events (`PhaseStarted`/`PhaseCompleted`/`PhaseCancelled`,
// `khive_storage::telemetry`) use today — falling back to
// `payload.resource.work_class` for the not-yet-emitted future shape
// where a generic audit row carries a `resource` sub-object (ADR-103
// Stage 1's "resource payload enrichment", still open). Events with
// neither path present are simply not counted in the split.
// `cost_unit` (ADR-103 Amendment 1, `payload.resource.cost_unit`,
// stamped on every successful verb dispatch since PR #927 via
// `khive_runtime::cost_unit::resource_payload`) sums into
// `total_cost_unit` and `cost_unit_by_verb`, both omitted rather than
// zero-filled when no event in the window carries it. Amendment 1's
// "absence has exactly two meanings" rule means a missing
// `resource.cost_unit` is either a pre-Amendment-1 event or an
// errored/denied dispatch — this verb does not distinguish the two, it
// simply excludes the event from the sum, matching the `work_class`
// split's existing exclusion convention.
//
// Named `brain.event_counts` rather than the literal
// `brain.events(...)` shape sketched on the issue — that name is already
// taken by the `Visibility::Subhandler` debug-listing verb above, which an
// MCP-boundary test (`subhandler_verbs_are_blocked_at_mcp_boundary`)
// requires to stay internal-only, and which several existing tests depend
// on for its `{count, events}` shape. Reusing the name would either break
// that invariant or overload one verb with two incompatible response
// shapes; a second, public verb name keeps both surfaces intact.

/// Per-part row cap for the default bounded `brain.event_counts` view.
/// A kind-filtered call folds at most this many events; an unfiltered call
/// independently admits up to this many audit and this many non-audit events,
/// for up to twice this cap in one call. Truncation is checked per part.
/// Storage-side Kind grouping counts `window_event_total` independently of
/// the admitted rows folded into profile, cross and cost aggregates.
pub(crate) const MAX_WINDOW_EVENTS: u32 = 50_000;
pub(crate) const EXHAUSTIVE_PAGE_SIZE: u32 = 10_000;
pub(crate) const MAX_EXHAUSTIVE_EVENTS: u64 = 40 * MAX_WINDOW_EVENTS as u64;

/// Issue #14: when the window's aggregation ran over a bounded page
/// rather than the full window (`truncated`), a bare scalar total next
/// to complete-looking `counts_by_*` maps reads as authoritative even
/// though it is only a page-scoped estimate. Storage-side admission totals
/// do not make the admitted-row aggregates complete: when truncated, the misleading key is
/// never emitted; the same page-scoped value is returned under an
/// explicitly incomplete name instead. A missing `total` gets noticed;
/// a `total` that is wrong by an order of magnitude does not.
/// `window_event_total` (the true count) and `truncated` (the marker)
/// are unaffected -- this only renames the page-scoped scalar's own key.
pub(crate) fn truncatable_total_key(base_key: &str, truncated: bool) -> String {
    if truncated {
        format!("{base_key}_page_scoped")
    } else {
        base_key.to_string()
    }
}

impl KhiveRuntime {
    /// Read the brain event-count projection under the caller's existing read budget.
    pub async fn read_event_counts(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let allowance = khive_storage::request_read_timeout_from_env();
        let deadline = khive_storage::effective_request_read_deadline(
            khive_storage::RequestReadDeadline::after(allowance),
        );
        khive_storage::scope_request_read_deadline_at(deadline, async {
            let operation = format!(
                "brain.event_counts (operator read budget {}s; earlier caller deadline retained)",
                allowance.as_secs()
            );
            if khive_storage::request_read_is_cancelled() {
                return Err(RuntimeError::Storage(
                    khive_storage::StorageError::Timeout {
                        operation: operation.into(),
                    },
                ));
            }
            let result = self.read_event_counts_in_read_scope(token, params).await;
            if result.is_ok() && khive_storage::request_read_is_cancelled() {
                return Err(RuntimeError::Storage(
                    khive_storage::StorageError::Timeout {
                        operation: operation.into(),
                    },
                ));
            }
            result
        })
        .await
    }

    async fn read_event_counts_in_read_scope(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct EventCountsParams {
            actor: Option<String>,
            all_actors: Option<bool>,
            kind: Option<String>,
            group_by: Option<super::grouping::EventCountGroupBy>,
            // `Option`, not a required `String`: a bare-missing `since` must go through
            // the same named-field-plus-example-format error as a malformed one, not
            // serde's generic "missing field `since`" message.
            since: Option<String>,
            until: Option<String>,
            // #21: opt into full-window aggregation (paginates through every
            // matching event instead of a single bounded page) so a coverage
            // panel gets non-sampled per-verb/kind/actor counts in one call,
            // without stitching sampled windows client-side. The window is a
            // best-effort live view, not a snapshot (see the walk's docs). Default false — the
            // bounded page is cheaper and sufficient for most callers.
            exhaustive: Option<bool>,
        }
        let p: EventCountsParams =
            parse_handler_params(params).map_err(RuntimeError::InvalidInput)?;

        let actor_scope = event_actor_read_scope(
            self,
            token,
            p.actor.as_deref(),
            p.all_actors.unwrap_or(false),
        )?;

        let since_raw = p.since.as_deref().ok_or_else(|| {
            RuntimeError::InvalidInput(
                "missing `since`: required ISO-8601/RFC-3339 datetime; expected e.g. \
                 \"2026-07-01T00:00:00Z\""
                    .to_string(),
            )
        })?;
        let display_tz = self.config().display_timezone;
        let since_us = parse_rfc3339_micros("since", since_raw, false, display_tz)?;
        let until_us = match p.until.as_deref() {
            Some(u) => parse_rfc3339_micros("until", u, true, display_tz)?,
            None => Utc::now().timestamp_micros(),
        };

        let kind_filter = p.kind.as_deref().map(parse_event_kind).transpose()?;

        // The event plane is read through a store scoped to exactly one
        // namespace (`Runtime::events` opens `token.namespace()` and the SQL
        // pins `namespace = ?`), so a visibility set does not widen this
        // window the way it widens a note read. Name the namespace the answer
        // covers, and name the others this caller may ask for by passing
        // `namespace=`, so an undercount is legible from the result itself.
        let scope_namespace = token.namespace().as_str().to_string();
        let mut scope_other: Vec<String> = token
            .visible_namespace_strs()
            .iter()
            .map(|n| n.to_string())
            .filter(|n| n != &scope_namespace)
            .collect();
        scope_other.sort();
        scope_other.dedup();

        let store = self.events(token)?;
        let base_filter = EventFilter {
            actors: actor_scope.actors,
            kinds: kind_filter.into_iter().collect(),
            // Half-open window [since, until): `EventFilter.after` is a strict
            // `created_at > after`, so subtracting one microsecond from `since`
            // makes the boundary itself inclusive; `before` is already a strict
            // `created_at < before`, which is exactly the exclusive `until` we want.
            after: Some(since_us - 1),
            before: Some(until_us),
            ..EventFilter::default()
        };

        let exhaustive = p.exhaustive.unwrap_or(false);
        let mut counts =
            EventCountsAccumulator::new(actor_scope.default_caller.as_deref(), p.group_by);
        let (total, window_event_total, truncated) = if exhaustive {
            fold_event_counts_window_exhaustive(
                store.as_ref(),
                &base_filter,
                EXHAUSTIVE_PAGE_SIZE,
                MAX_EXHAUSTIVE_EVENTS,
                &mut counts,
            )
            .await?
        } else {
            let (items, window_event_total, truncated) = fetch_event_counts_window(
                store.as_ref(),
                &base_filter,
                p.kind.is_none(),
                MAX_WINDOW_EVENTS,
            )
            .await?;
            for event in &items {
                counts.observe(event);
            }
            (items.len() as u64, window_event_total, truncated)
        };

        let mut result = json!({
            "since": micros_to_iso(since_us),
            "until": micros_to_iso(until_us),
            "actor": p.actor,
            "kind": p.kind,
            "truncated": truncated,
            "window_event_total": window_event_total,
            "exhaustive": exhaustive,
            // The scope the answer was computed under, stated rather than
            // implied. `namespace` is the single namespace this window covers;
            // `other_namespaces` are the ones this caller may read by asking
            // for them explicitly, and whose rows are absent here. A caller can
            // therefore tell a real zero from an out-of-scope population, and
            // knows what to pass to reach the rest.
            "scope": {
                "namespace": scope_namespace,
                "other_namespaces": scope_other,
            },
        });
        result[truncatable_total_key("total", truncated)] = json!(total);
        counts.add_to_result(&mut result, truncated);
        Ok(result)
    }
}

/// Fetch the event window for `brain.event_counts`, each bounded query capped at
/// `page_limit` (`MAX_WINDOW_EVENTS` in production; a free function
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
/// (`crate::events_split::MAX_QUERY_EVENTS_PAGE_ROWS`), and the
/// split store's merged read forwards `offset + limit` as one daemon page —
/// so windows wider than the cap are collected by cursor-walking in pages of
/// at most this many rows at `offset: 0`, never by one wide request.
pub const TRANSPORT_PAGE_ROWS: u32 = crate::events_split::MAX_QUERY_EVENTS_PAGE_ROWS;

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

pub async fn fetch_event_counts_window(
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
    crate::visit_events_cursor_walk(store, base_filter, page_size, max_rows, visit)
        .await
        .map_err(|cause| match cause {
            crate::EventCursorWalkError::Storage(error) => event_counts_storage_error(error),
            crate::EventCursorWalkError::MissingBoundary => RuntimeError::Internal(
                "event cursor walk saw duplicate rows before any boundary".to_string(),
            ),
            crate::EventCursorWalkError::DenseTimestampTie {
                page_limit: fetch_limit,
            } => RuntimeError::InvalidInput(format!(
                "brain.event_counts cannot page this window: more than {fetch_limit} \
                     events share one created_at microsecond, which exceeds the event \
                     transport's page cap; narrow `since`/`until` or add filters (`actor` \
                     or `kind`)"
            )),
        })
}

#[cfg(any(test, feature = "test-internals"))]
pub async fn fetch_event_counts_window_exhaustive(
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
/// Set to 40x the bounded-mode cap (`MAX_WINDOW_EVENTS`)
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

/// Extract `work_class` from an event payload for `brain.event_counts`'
/// `counts_by_work_class` split (ADR-103 Stage 1, ADR-103-resource-attribution-model.md:303).
///
/// Checks two paths, phase-payload first:
/// 1. `payload.work_class` — the shape `PhaseStarted`/`PhaseCompleted`/`PhaseCancelled`
///    events use today (`khive_storage::telemetry::{PhaseStartedPayload, ...}`).
/// 2. `payload.resource.work_class` — the shape successful-dispatch audit rows write:
///    `crate::cost_unit` constructs the resource object with `work_class` and the
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
/// `crate::cost_unit::resource_payload` on the per-dispatch audit
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
