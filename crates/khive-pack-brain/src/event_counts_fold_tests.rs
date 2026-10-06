use crate::event_counts_group_tests::fold_controls::GeneratedEvents;
use crate::event_counts_grouping::EventCountGroupBy;
use crate::handlers::{
    fetch_event_counts_window_exhaustive, fold_event_counts_window_exhaustive,
    visit_events_cursor_walk, EventCountsAccumulator,
};
use khive_storage::EventFilter;
use serde_json::json;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn exhaustive_fold_admits_only_final_prefix_in_original_saturating_order() {
    // Initial count is stale: four live rows now exist, but the row budget
    // is three. The +999 tail must not reach any aggregate.
    let store = GeneratedEvents::new(&[9, 9, 9, 8], 2, false);
    let mut counts =
        EventCountsAccumulator::new(Some("lambda:caller"), Some(EventCountGroupBy::VerbActor));
    let result =
        fold_event_counts_window_exhaustive(&store, &EventFilter::default(), 2, 3, &mut counts)
            .await
            .unwrap();
    assert_eq!(result, (3, 2, false));
    assert_eq!(
        store.queries().len(),
        3,
        "stop immediately after the widened final page"
    );
    let mut result = json!({});
    counts.add_to_result(&mut result, false);
    assert_eq!(
        result,
        json!({
            "counts_by_kind": {"feedback_explicit": 3},
            "counts_by_actor": {"lambda:caller": 3},
            "counts_by_verb": {"memory.recall": 3},
            "counts_by_verb_and_actor": {"memory.recall": {"lambda:caller": 3}},
            "by_profile": {"p": 1, "unspecified": 2},
            "feedback_by_originating_verb": {"search": 1, "memory.recall": 2},
            "counts_by_signal": {"useful": 1, "wrong": 1, "unspecified": 1},
            "by_profile_and_signal": {
                "p": {"useful": 1},
                "unspecified": {"wrong": 1, "unspecified": 1}
            },
            "counts_by_work_class": {"top": 1, "nested": 1},
            "total_cost_unit": i64::MAX - 20,
            "cost_unit_by_verb": {"memory.recall": i64::MAX - 20},
        })
    );
}

#[tokio::test]
async fn cursor_fold_and_collecting_facade_keep_tied_id_sequence_and_budgets() {
    for budget in [0, 1, 4, 20] {
        let store = GeneratedEvents::new(&[9, 8, 8, 8, 5, 4], 6, false);
        let mut ids = Vec::new();
        let admitted =
            visit_events_cursor_walk(&store, &EventFilter::default(), 2, budget, |event| {
                ids.push(event.id.as_u128())
            })
            .await
            .unwrap();
        let expected: Vec<u128> = (1..=6).take(budget as usize).collect();
        assert_eq!(ids, expected);
        assert_eq!(admitted as usize, expected.len());
        if budget == 0 {
            assert!(store.queries().is_empty());
        }
        let collected = crate::handlers::collect_events_cursor_walk(
            &GeneratedEvents::new(&[9, 8, 8, 8, 5, 4], 6, false),
            &EventFilter::default(),
            2,
            budget,
        )
        .await
        .unwrap();
        assert_eq!(
            collected
                .iter()
                .map(|event| event.id.as_u128())
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[tokio::test]
async fn fold_preflight_refusal_and_live_shortfall_keep_exact_semantics() {
    let store = GeneratedEvents::new(&[9], 4, false);
    let mut counts = EventCountsAccumulator::new(None, None);
    let err =
        fold_event_counts_window_exhaustive(&store, &EventFilter::default(), 2, 3, &mut counts)
            .await
            .unwrap_err();
    assert!(
        matches!(err, khive_runtime::RuntimeError::InvalidInput(ref message) if message ==
        "brain.event_counts window exceeds the exhaustive limit of 3 events (4 matched); \
         narrow `since`/`until` or add filters (`actor` or `kind`)")
    );
    assert_eq!(store.counts.load(Ordering::Relaxed), 1);
    assert!(store.queries().is_empty());
    let mut empty = json!({});
    counts.add_to_result(&mut empty, false);
    assert_eq!(
        empty,
        json!({"counts_by_kind": {}, "counts_by_actor": {}, "counts_by_verb": {}})
    );

    let store = GeneratedEvents::new(&[9, 8], 3, false);
    let mut counts = EventCountsAccumulator::new(None, Some(EventCountGroupBy::VerbActor));
    assert_eq!(
        fold_event_counts_window_exhaustive(&store, &EventFilter::default(), 2, 4, &mut counts,)
            .await
            .unwrap(),
        (2, 3, true)
    );
    let mut result = json!({});
    counts.add_to_result(&mut result, true);
    assert_eq!(
        result["counts_by_actor"],
        json!({"actor:legacy": 1, "lambda:caller": 1})
    );
    assert_eq!(
        result["counts_by_verb_and_actor_page_scoped"],
        json!({"memory.recall": {"actor:legacy": 1, "lambda:caller": 1}})
    );
    assert!(result.get("counts_by_verb_and_actor").is_none());
    assert_eq!(result["total_cost_unit_page_scoped"], i64::MAX);
    assert!(result.get("total_cost_unit").is_none());

    let (items, total, truncated) = fetch_event_counts_window_exhaustive(
        &GeneratedEvents::new(&[9, 8], 3, false),
        &EventFilter::default(),
        2,
        4,
    )
    .await
    .unwrap();
    assert_eq!((items.len(), total, truncated), (2, 3, true));
}

#[test]
fn incremental_group_and_cost_parsing_keep_empty_and_malformed_behavior() {
    let mut empty = json!({});
    EventCountsAccumulator::new(None, Some(EventCountGroupBy::VerbActor))
        .add_to_result(&mut empty, false);
    assert_eq!(empty["counts_by_verb_and_actor"], json!({}));
    let mut counts = EventCountsAccumulator::new(None, None);
    for payload in [
        json!({}),
        json!({"resource": {"cost_unit": "12"}}),
        json!({"work_class": 8, "resource": {"work_class": "fallback", "cost_unit": 1.5}}),
    ] {
        let event = khive_storage::Event::new(
            "local",
            "v",
            khive_types::EventKind::Audit,
            khive_types::SubstrateKind::Note,
            "a",
        )
        .with_payload(payload);
        counts.observe(&event);
    }
    let mut result = json!({});
    counts.add_to_result(&mut result, false);
    assert_eq!(
        result,
        json!({"counts_by_kind": {"audit": 3}, "counts_by_actor": {"a": 3},
        "counts_by_verb": {"v": 3}, "counts_by_work_class": {"fallback": 1}})
    );
}
