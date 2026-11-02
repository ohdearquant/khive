#[test]
fn entity_signal_for_hit_is_none() {
    let id = Uuid::new_v4();
    let sig = BrainSignal::RecallHit {
        target_id: id,
        latency_us: 100,
        served_by_profile_id: None,
        serve_attribution: ServeAttribution::Unspecified,
    };
    // A returned result is exposure, not a judgment.
    assert_eq!(entity_signal(&sig), None);
}

#[test]
fn entity_signal_for_non_judgment_signals_is_none() {
    let id = Uuid::new_v4();
    assert_eq!(
        entity_signal(&BrainSignal::NoteAccessed { target_id: id }),
        None
    );
    assert_eq!(
        entity_signal(&BrainSignal::SearchCompleted { latency_us: 0 }),
        None
    );
    assert_eq!(entity_signal(&BrainSignal::Irrelevant), None);
}

#[test]
fn entity_signal_for_miss() {
    assert_eq!(entity_signal(&BrainSignal::RecallMiss), None);
}

#[test]
fn recall_positive_classification() {
    let hit = BrainSignal::RecallHit {
        target_id: Uuid::new_v4(),
        latency_us: 0,
        served_by_profile_id: None,
        serve_attribution: ServeAttribution::Unspecified,
    };
    assert_eq!(is_recall_positive(&hit), Some(true));
    assert_eq!(is_recall_positive(&BrainSignal::RecallMiss), Some(false));
    assert_eq!(
        is_recall_positive(&BrainSignal::SearchCompleted { latency_us: 0 }),
        None
    );
}

#[test]
fn feedback_not_useful_is_negative_entity_signal() {
    let id = Uuid::new_v4();
    let sig = BrainSignal::Feedback {
        target_id: id,
        signal: FeedbackSignal::NotUseful,
        served_by_profile_id: None,
        section_signals: None,
    };
    assert_eq!(entity_signal(&sig), Some((id, false)));
}

#[test]
fn feedback_wrong_is_negative_entity_signal() {
    let id = Uuid::new_v4();
    let sig = BrainSignal::Feedback {
        target_id: id,
        signal: FeedbackSignal::Wrong,
        served_by_profile_id: None,
        section_signals: None,
    };
    assert_eq!(entity_signal(&sig), Some((id, false)));
}
