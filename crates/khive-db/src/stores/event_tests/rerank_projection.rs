#[tokio::test]
async fn rerank_executed_falls_back_to_reranked_when_final_scores_field_is_absent() {
    // Regression test (#831): legacy
    // events emitted before `final_scores` existed carry only `reranked`.
    // The decoder must still fall through from the absent `final_scores`
    // field to the tuple-shaped `reranked` field instead of silently
    // projecting zero `selected` rows. Constructed via a hand-rolled
    // `json!` (not the typed struct) so the `final_scores` key is
    // genuinely absent, not present-and-empty.
    let store = setup_memory_store();
    let candidate = Uuid::new_v4();
    let reranked_winner = Uuid::new_v4();

    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    event.payload = json!({
        "candidates": [candidate.to_string()],
        "reranked": [[reranked_winner.to_string(), [["relevance", 0.9]]]],
    });
    let event_id = event.id;

    store.append_event(event).await.unwrap();

    let selected = selected_uuids_for(&store, event_id).await;
    assert_eq!(
        selected,
        vec![reranked_winner.to_string()],
        "selected observation must decode the UUID leading the `reranked` tuple \
         when `final_scores` is absent"
    );
}

#[tokio::test]
async fn rerank_executed_prefers_final_scores_order_over_reranked_when_both_present() {
    // #831: ADR-042 §5 defines
    // `final_scores` as the ordered rerank output and `reranked` as
    // unordered per-reranker audit/debug data. When both are present and
    // their orderings differ, `final_scores`' order must win.
    let store = setup_memory_store();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();

    let payload = khive_types::RerankExecutedPayload {
        served_by_profile_id: Some("profile-a".to_string()),
        reranker: khive_types::RerankerKind::Native,
        model_id: Some(khive_types::Id128::from_u128(1)),
        query_id: None,
        tiers: Vec::new(),
        ignored_weights: Vec::new(),
        unidentified_candidates: 0,
        candidates: vec![
            khive_types::Id128::from_bytes(*a.as_bytes()),
            khive_types::Id128::from_bytes(*b.as_bytes()),
        ],
        // audit-only ordering: b before a
        reranked: vec![
            (
                khive_types::Id128::from_bytes(*b.as_bytes()),
                vec![("relevance".to_string(), 0.4)],
            ),
            (
                khive_types::Id128::from_bytes(*a.as_bytes()),
                vec![("relevance".to_string(), 0.9)],
            ),
        ],
        // authoritative ordered output: a before b
        final_scores: vec![
            (khive_types::Id128::from_bytes(*a.as_bytes()), 0.9),
            (khive_types::Id128::from_bytes(*b.as_bytes()), 0.4),
        ],
        latency_us: 1200,
        hook_applied: false,
        hook_target_match: false,
    };

    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    event.payload = serde_json::to_value(&payload).unwrap();
    let event_id = event.id;

    store.append_event(event).await.unwrap();

    let selected = selected_uuids_for(&store, event_id).await;
    assert_eq!(
        selected,
        vec![a.to_string(), b.to_string()],
        "selected order must follow `final_scores`, not `reranked`"
    );
}

#[tokio::test]
async fn rerank_executed_ignores_stray_selected_field_and_uses_final_scores() {
    // Regression test (#831):
    // `RerankExecutedPayload` (khive_types::event::RerankExecutedPayload)
    // has no `selected` field at all. The serde helper does not deny
    // unknown fields, so a payload carrying the typed fields plus a stray
    // `selected` key must still be projected from `final_scores` only —
    // the stray field must never be consulted, even when its order
    // differs completely from `final_scores`.
    let store = setup_memory_store();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let stray_selected_winner = Uuid::new_v4();

    let payload = khive_types::RerankExecutedPayload {
        served_by_profile_id: Some("profile-a".to_string()),
        reranker: khive_types::RerankerKind::Native,
        model_id: Some(khive_types::Id128::from_u128(1)),
        query_id: None,
        tiers: Vec::new(),
        ignored_weights: Vec::new(),
        unidentified_candidates: 0,
        candidates: vec![
            khive_types::Id128::from_bytes(*a.as_bytes()),
            khive_types::Id128::from_bytes(*b.as_bytes()),
        ],
        reranked: vec![],
        // authoritative ordered output: a before b
        final_scores: vec![
            (khive_types::Id128::from_bytes(*a.as_bytes()), 0.9),
            (khive_types::Id128::from_bytes(*b.as_bytes()), 0.4),
        ],
        latency_us: 1200,
        hook_applied: false,
        hook_target_match: false,
    };

    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    let mut payload_value = serde_json::to_value(&payload).unwrap();
    // Inject a stray `selected` field with an entirely different order
    // than `final_scores` — this field is not part of the typed contract
    // and must not affect projection.
    payload_value.as_object_mut().unwrap().insert(
        "selected".to_string(),
        json!([stray_selected_winner.to_string()]),
    );
    event.payload = payload_value;
    let event_id = event.id;

    store.append_event(event).await.unwrap();

    let selected = selected_uuids_for(&store, event_id).await;
    assert_eq!(
        selected,
        vec![a.to_string(), b.to_string()],
        "stray `selected` field must be ignored for RerankExecuted; \
         projection must follow `final_scores` only"
    );
}

#[tokio::test]
async fn rerank_executed_uses_final_scores_when_reranked_is_empty() {
    // Companion case: `reranked` present-but-empty, `final_scores`
    // populated. `final_scores` must still be used (not skipped just
    // because `reranked` happens to come first lexically in the payload).
    let store = setup_memory_store();
    let winner = Uuid::new_v4();

    let payload = khive_types::RerankExecutedPayload {
        served_by_profile_id: None,
        reranker: khive_types::RerankerKind::Native,
        model_id: Some(khive_types::Id128::from_u128(1)),
        query_id: None,
        tiers: Vec::new(),
        ignored_weights: Vec::new(),
        unidentified_candidates: 0,
        candidates: vec![khive_types::Id128::from_bytes(*winner.as_bytes())],
        reranked: vec![],
        final_scores: vec![(khive_types::Id128::from_bytes(*winner.as_bytes()), 0.75)],
        latency_us: 800,
        hook_applied: false,
        hook_target_match: false,
    };

    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    event.payload = serde_json::to_value(&payload).unwrap();
    let event_id = event.id;

    store.append_event(event).await.unwrap();

    let selected = selected_uuids_for(&store, event_id).await;
    assert_eq!(
        selected,
        vec![winner.to_string()],
        "final_scores must be used even when reranked is present-but-empty"
    );
}

#[tokio::test]
async fn rerank_executed_final_scores_single_element_tuple_rejected() {
    // #831: the typed contract
    // (`RerankExecutedPayload::final_scores: Vec<(Id128, f32)>`) requires
    // exact two-element tuples. A one-element tuple must error, and the
    // event insert must roll back rather than silently accepting it.
    let store = setup_memory_store();
    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    event.payload = json!({
        "candidates": [],
        "final_scores": [[Uuid::new_v4().to_string()]],
    });
    let event_id = event.id;

    let result = store.append_event(event).await;
    assert!(result.is_err(), "single-element tuple must be rejected");

    let fetched = store.get_event(event_id).await.unwrap();
    assert!(fetched.is_none(), "event row must not exist after rollback");
}

#[tokio::test]
async fn rerank_executed_final_scores_extra_element_tuple_rejected() {
    // Extra-element tuples must also be rejected, not
    // silently truncated to the first two elements.
    let store = setup_memory_store();
    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    event.payload = json!({
        "candidates": [],
        "final_scores": [[Uuid::new_v4().to_string(), 0.5, "extra"]],
    });
    let event_id = event.id;

    let result = store.append_event(event).await;
    assert!(result.is_err(), "extra-element tuple must be rejected");

    let fetched = store.get_event(event_id).await.unwrap();
    assert!(fetched.is_none(), "event row must not exist after rollback");
}

#[tokio::test]
async fn rerank_executed_final_scores_non_numeric_second_element_rejected() {
    // The second element of a `final_scores` tuple must be the
    // typed contract's `f32` score, not an arbitrary value.
    let store = setup_memory_store();
    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    event.payload = json!({
        "candidates": [],
        "final_scores": [[Uuid::new_v4().to_string(), "not-a-number"]],
    });
    let event_id = event.id;

    let result = store.append_event(event).await;
    assert!(result.is_err(), "non-numeric score must be rejected");

    let fetched = store.get_event(event_id).await.unwrap();
    assert!(fetched.is_none(), "event row must not exist after rollback");
}

#[tokio::test]
async fn rerank_executed_reranked_malformed_sub_scores_rejected() {
    // `reranked`'s second element must be the typed contract's
    // `Vec<(String, f32)>` sub-score list, not an arbitrary value.
    let store = setup_memory_store();
    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    event.payload = json!({
        "candidates": [],
        "reranked": [[Uuid::new_v4().to_string(), "not-an-array"]],
    });
    let event_id = event.id;

    let result = store.append_event(event).await;
    assert!(
        result.is_err(),
        "malformed reranked sub-scores must be rejected"
    );

    let fetched = store.get_event(event_id).await.unwrap();
    assert!(fetched.is_none(), "event row must not exist after rollback");
}
