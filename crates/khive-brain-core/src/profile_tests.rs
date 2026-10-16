use super::*;

/// ADR-058 amendment (#542): each variant's wire string is exact and stable.
#[test]
fn consumer_kind_as_str_matches_wire_vocabulary() {
    assert_eq!(ConsumerKind::Recall.as_str(), "recall");
    assert_eq!(ConsumerKind::KnowledgeCompose.as_str(), "knowledge_compose");
    assert_eq!(ConsumerKind::Rerank.as_str(), "rerank");
}

/// ADR-058 amendment (#542): `as_str()` and `FromStr` round-trip for every variant.
#[test]
fn consumer_kind_from_str_round_trips() {
    for kind in [
        ConsumerKind::Recall,
        ConsumerKind::KnowledgeCompose,
        ConsumerKind::Rerank,
    ] {
        assert_eq!(kind.as_str().parse::<ConsumerKind>().unwrap(), kind);
    }
}

#[test]
fn consumer_kind_from_str_rejects_unknown() {
    assert!("bogus".parse::<ConsumerKind>().is_err());
}

/// ADR-048 Phase-1 gate: profile save/load round-trip must produce identical
/// posteriors (snapshot == restored state).
#[test]
fn adr048_snapshot_roundtrip_equality() {
    let mut state = BalancedRecallState::new(100);
    let id = Uuid::new_v4();
    for _ in 0..50 {
        state.apply_signal(&crate::brain_signal::BrainSignal::RecallHit {
            target_id: id,
            latency_us: 10_000,
            served_by_profile_id: None,
            serve_attribution: crate::brain_signal::ServeAttribution::Unspecified,
        });
    }
    let snap = state.to_snapshot();
    let restored = BalancedRecallState::from_snapshot(snap.clone(), 100);
    let restored_snap = restored.to_snapshot();

    // Full structural equality — covers every field including entity_posteriors.
    // If any field (including a newly-added one) is not preserved by the
    // round-trip, this assertion will catch it.
    assert_eq!(
        snap, restored_snap,
        "ADR-048 Phase-1: snapshot != restored state"
    );
}

/// ADR-048 Phase-1 gate: ESS cap convergence must shift the mean by ≥0.3.
/// See crates/khive-brain-core/docs/testing-strategy.md#profilersadr048_ess_cap_mean_shift_ge_0_3-adr-048-phase-1-gate
#[test]
fn adr048_ess_cap_mean_shift_ge_0_3() {
    let mut state = BalancedRecallState::new(200);
    let id = Uuid::new_v4();

    for _ in 0..200 {
        state.apply_signal(&BrainSignal::Feedback {
            target_id: id,
            signal: FeedbackSignal::Useful,
            served_by_profile_id: None,
            section_signals: None,
        });
    }
    let mean_after_positive = state.salience.mean();

    for _ in 0..200 {
        state.apply_signal(&BrainSignal::Feedback {
            target_id: id,
            signal: FeedbackSignal::NotUseful,
            served_by_profile_id: None,
            section_signals: None,
        });
    }
    let mean_after_opposing = state.salience.mean();

    let shift = (mean_after_positive - mean_after_opposing).abs();
    assert!(
        shift >= 0.3,
        "ESS cap convergence: mean shift {shift:.4} < 0.3 (positive={mean_after_positive:.4}, opposing={mean_after_opposing:.4})"
    );
}

/// BRAINCORE-AUD-001: legacy (version 0) oversized restore is bounded.
/// See crates/khive-brain-core/docs/testing-strategy.md#profilerslegacy_entity_posteriors_restore_uses_deterministic_order_and_capacity-braincore-aud-001
#[test]
fn legacy_entity_posteriors_restore_uses_deterministic_order_and_capacity() {
    let capacity = 2;
    let mut ids = vec![
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    ];
    ids.sort();

    let mut entity_posteriors = HashMap::new();
    for id in &ids {
        entity_posteriors.insert(*id, BetaPosterior::default());
    }

    let legacy = BalancedRecallSnapshot {
        relevance: BetaPosterior::default(),
        salience: BetaPosterior::default(),
        temporal: BetaPosterior::default(),
        entity_posteriors,
        entity_posteriors_version: 0,
        entity_posterior_order: Vec::new(),
        total_events: 0,
        exploration_epoch: 0,
    };

    let restored = BalancedRecallState::from_snapshot(legacy, capacity);

    assert_eq!(restored.entity_posteriors.len(), capacity);
    assert_eq!(restored.entity_posteriors.order(), ids[..capacity].to_vec());
    for id in ids.iter().take(capacity) {
        assert!(
            restored.entity_posteriors.get(id).is_some(),
            "deterministic sort prefix id {id} must be retained"
        );
    }
    for id in ids.iter().skip(capacity) {
        assert!(
            restored.entity_posteriors.get(id).is_none(),
            "id {id} beyond the deterministic sort prefix must be dropped"
        );
    }
}
