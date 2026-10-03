//! Counter parity at all feedback atomic-write callers.
use khive_runtime::{Namespace, PackRuntime};
use khive_storage::usage::{scope, UsageContext};
use khive_storage::EventFilter;
use serde_json::json;

#[tokio::test]
async fn feedback_usage_matches_actual_events_for_direct_and_fold_gate_paths() {
    for signal in ["explicit_positive", "correction", "implicit_positive"] {
        let (pack, runtime) = crate::tests::make_pack();
        let registry = crate::tests::empty_registry();
        let token = runtime.authorize(Namespace::local()).unwrap();
        let target = crate::tests::create_test_entity(&runtime, &token).await;
        // Bootstrap precedes the measured operation.
        pack.ensure_loaded(&token).await.unwrap();
        let store = runtime.events(&token).unwrap();
        let before = store.count_events(EventFilter::default()).await.unwrap();
        let context = UsageContext::new();
        let result = scope(
            context.clone(),
            pack.dispatch(
                "brain.feedback",
                json!({"target_id": target, "signal": signal}),
                &registry,
                &token,
            ),
        )
        .await
        .unwrap();
        assert_eq!(result["emitted"], true);
        let after = store.count_events(EventFilter::default()).await.unwrap();
        assert_eq!(after - before, 1, "{signal}");
        assert_eq!(
            context.shipping_snapshot().unwrap()["event_rows"],
            json!(after - before),
            "{signal}"
        );
    }
}

#[tokio::test]
async fn section_feedback_usage_matches_actual_committed_event_rows() {
    let (pack, runtime) = crate::tests::make_pack();
    let token = runtime.authorize(Namespace::local()).unwrap();
    pack.ensure_loaded(&token).await.unwrap();
    let store = runtime.events(&token).unwrap();
    let before = store.count_events(EventFilter::default()).await.unwrap();
    let context = UsageContext::new();
    let result = scope(
        context.clone(),
        pack.apply_section_feedback(
            &token,
            "balanced-recall-v1",
            json!({"overview": "useful"}),
            Some("opaque:section-target".into()),
        ),
    )
    .await
    .unwrap();
    assert_eq!(result["emitted"], true);
    let after = store.count_events(EventFilter::default()).await.unwrap();
    assert_eq!(after - before, 1);
    assert_eq!(
        context.shipping_snapshot().unwrap()["event_rows"],
        json!(after - before)
    );
}
