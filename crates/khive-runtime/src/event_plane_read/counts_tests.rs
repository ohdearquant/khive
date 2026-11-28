use super::test_support::{runtime_with_actor, seed_event};
use crate::{Namespace, RuntimeError};
use khive_storage::event::{Event, EventFilter};
use khive_types::{EventKind, SubstrateKind};
use serde_json::json;

/// Build `count` events sharing one `created_at` for bulk seeding.
fn tied_events(namespace: &str, created_at: i64, count: usize) -> Vec<Event> {
    (0..count)
        .map(|_| {
            let mut event = Event::new(
                namespace,
                "recall",
                EventKind::RecallExecuted,
                SubstrateKind::Note,
                "lambda:a",
            );
            event.created_at = created_at;
            event
        })
        .collect()
}

/// Delegating store that appends one prepared row to the inner store
/// immediately AFTER serving a `count_events` call, reproducing a
/// concurrent boundary-timestamp append landing between the walk's
/// completeness count and its next page query.
struct AppendAfterCountStore {
    inner: std::sync::Arc<dyn khive_storage::event::EventStore>,
    pending: tokio::sync::Mutex<Option<Event>>,
}

#[async_trait::async_trait]
impl khive_storage::event::EventStore for AppendAfterCountStore {
    async fn append_event(&self, event: Event) -> khive_storage::StorageResult<()> {
        self.inner.append_event(event).await
    }
    async fn append_events(
        &self,
        events: Vec<Event>,
    ) -> khive_storage::StorageResult<khive_storage::BatchWriteSummary> {
        self.inner.append_events(events).await
    }
    async fn get_event(&self, id: uuid::Uuid) -> khive_storage::StorageResult<Option<Event>> {
        self.inner.get_event(id).await
    }
    async fn query_events(
        &self,
        filter: EventFilter,
        page: khive_storage::PageRequest,
    ) -> khive_storage::StorageResult<khive_storage::Page<Event>> {
        self.inner.query_events(filter, page).await
    }
    async fn count_events(&self, filter: EventFilter) -> khive_storage::StorageResult<u64> {
        let count = self.inner.count_events(filter).await?;
        if let Some(event) = self.pending.lock().await.take() {
            self.inner.append_event(event).await?;
        }
        Ok(count)
    }
}

/// #26: with a shrunk `page_limit`, a busy `audit` kind must not crowd a
/// quiet non-audit kind out of the unfiltered default view's shared budget.
/// Direct unit test of `fetch_event_counts_window` (rather than the full
/// `brain.event_counts` verb dispatch) because reproducing this at the real
/// `MAX_WINDOW_EVENTS = 50_000` cap would require seeding tens of thousands
/// of rows; the crowd-out mechanism is identical at any cap size.
#[tokio::test]
async fn audit_kind_does_not_crowd_out_non_audit_kinds_in_default_view() {
    use khive_storage::event::EventFilter;

    let rt = runtime_with_actor("brain-test");
    let token = rt.authorize(Namespace::local()).unwrap();

    // 5 audit events (the busy kind) vs. 1 non-audit event (the quiet kind),
    // all in the same window. A shared, unsegregated 3-row budget ordered
    // `created_at DESC` (audit seeded last, so audit sorts first) would
    // return only audit rows and silently drop the lone `search_executed`
    // row — exactly the ~12x undercount pattern reported in #26.
    for i in 0..5 {
        seed_event(
            &rt,
            &token,
            "create",
            EventKind::Audit,
            "lambda:a",
            1_000_000 + i,
            json!({}),
        )
        .await;
    }
    seed_event(
        &rt,
        &token,
        "search",
        EventKind::SearchExecuted,
        "lambda:a",
        999_999,
        json!({}),
    )
    .await;

    let store = rt.events(&token).expect("event store");
    let base_filter = EventFilter {
        after: Some(0),
        ..EventFilter::default()
    };

    let (items, window_event_total, truncated) = super::counts::fetch_event_counts_window(
        store.as_ref(),
        &base_filter,
        /* unfiltered = */ true,
        /* page_limit = */ 3,
    )
    .await
    .expect("fetch_event_counts_window must succeed");

    assert_eq!(
        window_event_total, 6,
        "the true total across both segregated queries must be exact, not \
         page-limited: {items:?}"
    );
    assert!(
        truncated,
        "5 audit rows against a 3-row budget must report truncated=true"
    );
    let search_present = items.iter().any(|e| e.kind == EventKind::SearchExecuted);
    assert!(
        search_present,
        "the lone non-audit event must survive a busy-audit-kind window even \
         under a shared-looking budget: {items:?}"
    );
    let audit_count = items.iter().filter(|e| e.kind == EventKind::Audit).count();
    assert_eq!(
        audit_count, 3,
        "the audit-kind half of the budget is independently capped at \
         page_limit: {items:?}"
    );
}

/// #21: a small injected page size makes this exercise three storage pages
/// while proving every page contributes to the final per-verb counts.
#[tokio::test]
async fn exhaustive_fetch_accumulates_counts_across_pages() {
    let rt = runtime_with_actor("brain-test");
    let token = rt.authorize(Namespace::local()).unwrap();

    for i in 0..7 {
        seed_event(
            &rt,
            &token,
            "recall",
            EventKind::RecallExecuted,
            "lambda:a",
            1_000_000 + i,
            json!({}),
        )
        .await;
    }
    for i in 0..3 {
        seed_event(
            &rt,
            &token,
            "search",
            EventKind::SearchExecuted,
            "lambda:a",
            2_000_000 + i,
            json!({}),
        )
        .await;
    }

    let store = rt.events(&token).expect("event store");
    let base_filter = EventFilter {
        after: Some(0),
        ..EventFilter::default()
    };
    let (items, window_event_total, truncated) =
        super::counts::fetch_event_counts_window_exhaustive(
            store.as_ref(),
            &base_filter,
            /* page_size = */ 4,
            /* max_events = */ 20,
        )
        .await
        .expect("multi-page exhaustive fetch must succeed");

    let mut counts_by_verb = std::collections::BTreeMap::new();
    for event in &items {
        *counts_by_verb.entry(event.verb.as_str()).or_insert(0_u64) += 1;
    }
    assert_eq!(items.len(), 10);
    assert_eq!(window_event_total, 10);
    assert!(!truncated);
    assert_eq!(counts_by_verb.get("recall"), Some(&7));
    assert_eq!(counts_by_verb.get("search"), Some(&3));
}

/// The exhaustive walk pages with a strict `before` cursor; rows sharing
/// one `created_at` microsecond across a page edge must arrive exactly
/// once — neither dropped by the strict bound nor double-counted by the
/// boundary re-read.
#[tokio::test]
async fn exhaustive_fetch_survives_timestamp_ties_across_page_edges() {
    let rt = runtime_with_actor("brain-test");
    let token = rt.authorize(Namespace::local()).unwrap();

    // A tie run wider than the page size, plus one older row below it.
    for _ in 0..9 {
        seed_event(
            &rt,
            &token,
            "recall",
            EventKind::RecallExecuted,
            "lambda:a",
            5_000_000,
            json!({}),
        )
        .await;
    }
    seed_event(
        &rt,
        &token,
        "search",
        EventKind::SearchExecuted,
        "lambda:a",
        1_000_000,
        json!({}),
    )
    .await;

    let store = rt.events(&token).expect("event store");
    let base_filter = EventFilter {
        after: Some(0),
        ..EventFilter::default()
    };
    let (items, window_event_total, truncated) =
        super::counts::fetch_event_counts_window_exhaustive(
            store.as_ref(),
            &base_filter,
            /* page_size = */ 4,
            /* max_events = */ 20,
        )
        .await
        .expect("tie-heavy exhaustive fetch must succeed");

    assert_eq!(window_event_total, 10);
    assert_eq!(items.len(), 10, "each tied row must arrive exactly once");
    let distinct: std::collections::HashSet<uuid::Uuid> =
        items.iter().map(|event| event.id).collect();
    assert_eq!(distinct.len(), 10, "no row may be double-counted");
    assert!(!truncated);
}

/// A page holding MORE than one timestamp group must not cycle: the
/// cursor steps to `oldest-in-page + 1` and the strict `created_at <`
/// bound excludes every newer group already emitted, so the walk is
/// monotone. Three timestamp groups with a page edge inside a group
/// exercise the mixed-page shape directly: page one carries two groups
/// (2×T3, 2×T2), the boundary set tracks only T2, and the next page must
/// deliver the remaining T2 row and the T1 group exactly once each.
#[tokio::test]
async fn exhaustive_fetch_terminates_across_multiple_timestamp_groups_per_page() {
    let rt = runtime_with_actor("brain-test");
    let token = rt.authorize(Namespace::local()).unwrap();

    // Three groups, newest first in delivery order: 2 @ T3, 3 @ T2,
    // 2 @ T1, with page_size 4 splitting the T2 group across pages.
    for _ in 0..2 {
        seed_event(
            &rt,
            &token,
            "recall",
            EventKind::RecallExecuted,
            "lambda:a",
            9_000_000,
            json!({}),
        )
        .await;
    }
    for _ in 0..3 {
        seed_event(
            &rt,
            &token,
            "search",
            EventKind::SearchExecuted,
            "lambda:a",
            5_000_000,
            json!({}),
        )
        .await;
    }
    for _ in 0..2 {
        seed_event(
            &rt,
            &token,
            "recall",
            EventKind::RecallExecuted,
            "lambda:a",
            1_000_000,
            json!({}),
        )
        .await;
    }

    let store = rt.events(&token).expect("event store");
    let base_filter = EventFilter {
        after: Some(0),
        ..EventFilter::default()
    };
    let (items, window_event_total, truncated) =
        super::counts::fetch_event_counts_window_exhaustive(
            store.as_ref(),
            &base_filter,
            /* page_size = */ 4,
            /* max_events = */ 20,
        )
        .await
        .expect("multi-group exhaustive fetch must terminate and succeed");

    assert_eq!(window_event_total, 7);
    assert_eq!(items.len(), 7, "every group must be reached exactly once");
    let distinct: std::collections::HashSet<uuid::Uuid> =
        items.iter().map(|event| event.id).collect();
    assert_eq!(distinct.len(), 7, "no row may be double-counted");
    let mut counts_by_ts = std::collections::BTreeMap::new();
    for event in &items {
        *counts_by_ts.entry(event.created_at).or_insert(0_u64) += 1;
    }
    assert_eq!(counts_by_ts.get(&9_000_000), Some(&2));
    assert_eq!(counts_by_ts.get(&5_000_000), Some(&3));
    assert_eq!(
        counts_by_ts.get(&1_000_000),
        Some(&2),
        "the oldest group must be reached"
    );
    assert!(!truncated);
}

/// A timestamp tie that EXACTLY fills a transport-cap page is pageable:
/// once the at-the-cap page comes back all-duplicates, the walk proves
/// the tie complete against `count_events` and steps the strict bound to
/// the boundary itself, reaching the older rows instead of failing. A
/// tie WIDER than the cap must still return the typed dense-tie error.
#[tokio::test]
async fn exact_cap_timestamp_tie_pages_past_while_wider_tie_errors() {
    let rt = runtime_with_actor("brain-test");
    let token = rt.authorize(Namespace::local()).unwrap();
    let store = rt.events(&token).expect("event store");
    let cap = usize::try_from(super::counts::TRANSPORT_PAGE_ROWS).unwrap();

    let mut batch = tied_events(token.namespace().as_str(), 5_000_000, cap);
    batch.extend(tied_events(token.namespace().as_str(), 1_000_000, 1));
    store.append_events(batch).await.expect("seed cap tie");

    let base_filter = EventFilter {
        after: Some(0),
        ..EventFilter::default()
    };
    let items = super::counts::collect_events_cursor_walk(
        store.as_ref(),
        &base_filter,
        super::counts::TRANSPORT_PAGE_ROWS,
        (cap as u64) + 1,
    )
    .await
    .expect("an exactly-cap tie must page past, not error");
    assert_eq!(items.len(), cap + 1, "the older row must be reached");
    let distinct: std::collections::HashSet<uuid::Uuid> =
        items.iter().map(|event| event.id).collect();
    assert_eq!(distinct.len(), cap + 1, "no row may be double-counted");

    // One more row at the tied microsecond pushes the run past the cap:
    // the walk must now fail with the typed dense-tie error, not loop.
    store
        .append_events(tied_events(token.namespace().as_str(), 5_000_000, 1))
        .await
        .expect("seed over-cap tie");
    let err = super::counts::collect_events_cursor_walk(
        store.as_ref(),
        &base_filter,
        super::counts::TRANSPORT_PAGE_ROWS,
        (cap as u64) + 2,
    )
    .await
    .expect_err("a tie wider than the cap is unpageable");
    assert!(
        err.to_string().contains("share one created_at microsecond"),
        "unexpected error: {err}"
    );
}

/// The walk's at-cap completeness proof and its next page query are
/// independent reads of a live plane: a boundary-timestamp row appended
/// between them is a point-in-time exclusion, never an error and never a
/// duplicate. This pins the documented read-consistency contract.
#[tokio::test]
async fn concurrent_boundary_append_between_count_and_next_page_is_excluded_cleanly() {
    let rt = runtime_with_actor("brain-test");
    let token = rt.authorize(Namespace::local()).unwrap();
    let store = rt.events(&token).expect("event store");
    let cap = usize::try_from(super::counts::TRANSPORT_PAGE_ROWS).unwrap();

    let mut batch = tied_events(token.namespace().as_str(), 5_000_000, cap);
    batch.extend(tied_events(token.namespace().as_str(), 1_000_000, 1));
    store.append_events(batch).await.expect("seed cap tie");
    let racer = tied_events(token.namespace().as_str(), 5_000_000, 1)
        .pop()
        .expect("one racer event");
    let racer_id = racer.id;

    let wrapped = AppendAfterCountStore {
        inner: store.clone(),
        pending: tokio::sync::Mutex::new(Some(racer)),
    };
    let base_filter = EventFilter {
        after: Some(0),
        ..EventFilter::default()
    };
    let items = super::counts::collect_events_cursor_walk(
        &wrapped,
        &base_filter,
        super::counts::TRANSPORT_PAGE_ROWS,
        (cap as u64) + 2,
    )
    .await
    .expect("a concurrent boundary append must not error the walk");
    assert_eq!(
        items.len(),
        cap + 1,
        "the point-in-time view holds the pre-append population"
    );
    let distinct: std::collections::HashSet<uuid::Uuid> =
        items.iter().map(|event| event.id).collect();
    assert_eq!(distinct.len(), cap + 1, "no row may be double-counted");
    assert!(
        !distinct.contains(&racer_id),
        "the racing row is excluded from this walk's view, not half-included"
    );
    assert!(
        wrapped.pending.lock().await.is_none(),
        "the completeness count must have fired and injected the racing append; \
         otherwise this test exercised nothing"
    );
    assert!(
        store
            .get_event(racer_id)
            .await
            .expect("racer lookup")
            .is_some(),
        "the racing row must actually be in the store"
    );
}

/// Rows at `created_at == i64::MAX` admit no exclusive bound above them:
/// the cursor must not saturate past them (which would silently skip the
/// uncollected remainder of the group) — the walk re-reads, dedups, and
/// still reaches every row exactly once, including the older rows.
#[tokio::test]
async fn max_timestamp_group_is_fully_collected_not_skipped() {
    let rt = runtime_with_actor("brain-test");
    let token = rt.authorize(Namespace::local()).unwrap();
    let store = rt.events(&token).expect("event store");

    let mut batch = tied_events(token.namespace().as_str(), i64::MAX, 5);
    batch.extend(tied_events(token.namespace().as_str(), 1_000_000, 2));
    store.append_events(batch).await.expect("seed max group");

    let base_filter = EventFilter {
        after: Some(0),
        ..EventFilter::default()
    };
    let items = super::counts::collect_events_cursor_walk(
        store.as_ref(),
        &base_filter,
        /* page_size = */ 4,
        /* max_rows = */ 100,
    )
    .await
    .expect("the max-timestamp group must be pageable");
    assert_eq!(items.len(), 7, "all seven rows must arrive");
    let distinct: std::collections::HashSet<uuid::Uuid> =
        items.iter().map(|event| event.id).collect();
    assert_eq!(distinct.len(), 7, "no row may be double-counted");
    assert_eq!(
        items
            .iter()
            .filter(|event| event.created_at == i64::MAX)
            .count(),
        5,
        "every max-timestamp row must be collected, none skipped"
    );
}

#[tokio::test]
async fn exhaustive_fetch_rejects_windows_over_the_cap() {
    let rt = runtime_with_actor("brain-test");
    let token = rt.authorize(Namespace::local()).unwrap();

    for i in 0..6 {
        seed_event(
            &rt,
            &token,
            "recall",
            EventKind::RecallExecuted,
            "lambda:a",
            1_000_000 + i,
            json!({}),
        )
        .await;
    }

    let store = rt.events(&token).expect("event store");
    let base_filter = EventFilter {
        after: Some(0),
        ..EventFilter::default()
    };
    let err = super::counts::fetch_event_counts_window_exhaustive(
        store.as_ref(),
        &base_filter,
        /* page_size = */ 2,
        /* max_events = */ 5,
    )
    .await
    .expect_err("an exhaustive window above the safety cap must be rejected");

    match err {
        RuntimeError::InvalidInput(msg) => {
            assert!(msg.contains("window exceeds the exhaustive limit of 5 events"));
            assert!(msg.contains("narrow `since`/`until` or add filters"));
        }
        other => panic!("expected InvalidInput, got {other:?}"),
    }
}

/// Issue #14: the untruncated case is unaffected -- `total`/
/// `total_cost_unit` keep their existing bare names. This is the shape
/// every other test in this module already exercises via the real
/// handler (window sizes here never approach `MAX_WINDOW_EVENTS`).
#[test]
fn truncatable_total_key_is_unchanged_when_not_truncated() {
    assert_eq!(
        super::counts::truncatable_total_key("total", false),
        "total"
    );
    assert_eq!(
        super::counts::truncatable_total_key("total_cost_unit", false),
        "total_cost_unit"
    );
}

/// Issue #14: when the window was truncated, the misleading bare key
/// must never be emitted -- it is renamed to an explicitly
/// page-scoped/incomplete key instead, so a caller reading `total`
/// directly gets nothing rather than a number that may be off by an
/// order of magnitude.
#[test]
fn truncatable_total_key_is_renamed_when_truncated() {
    assert_eq!(
        super::counts::truncatable_total_key("total", true),
        "total_page_scoped"
    );
    assert_eq!(
        super::counts::truncatable_total_key("total_cost_unit", true),
        "total_cost_unit_page_scoped"
    );
}
