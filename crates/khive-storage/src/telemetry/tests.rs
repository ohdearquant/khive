use super::*;

#[test]
fn lifecycle_event_roundtrips_through_json() {
    for kind in [
        LifecycleEvent::ChannelPollStarted,
        LifecycleEvent::ChannelPollSucceeded,
        LifecycleEvent::ChannelPollFailed,
        LifecycleEvent::ChannelBackoffArmed,
        LifecycleEvent::ChannelBackoffReset,
        LifecycleEvent::ChannelHeartbeatPersistFailed,
        LifecycleEvent::ConfigLocked,
        LifecycleEvent::CheckpointOutcomeRecorded,
        LifecycleEvent::PhaseStarted,
        LifecycleEvent::PhaseCompleted,
        LifecycleEvent::PhaseCancelled,
    ] {
        let json = serde_json::to_string(&kind).expect("serialize");
        let parsed: LifecycleEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, kind);
    }
}

#[test]
fn checkpoint_outcome_recorded_payload_roundtrips() {
    let payload = CheckpointOutcomeRecordedPayload {
        wal_pages: 2500,
        warn_pages: 2000,
        high_water_pages: 6000,
        truncate_high_water_pages: 20_000,
        above_warn: true,
        above_high_water: false,
        above_truncate_high_water: false,
        episode_elevated_ticks: Some(7),
        episode_peak_wal_pages: Some(3100),
    };
    let json = serde_json::to_value(&payload).expect("serialize");
    let parsed: CheckpointOutcomeRecordedPayload =
        serde_json::from_value(json).expect("deserialize");
    assert_eq!(parsed, payload);
}

#[test]
fn checkpoint_outcome_recorded_payload_accepts_legacy_rows_without_episode_summary() {
    let parsed: CheckpointOutcomeRecordedPayload = serde_json::from_value(serde_json::json!({
        "wal_pages": 2500,
        "warn_pages": 2000,
        "high_water_pages": 6000,
        "truncate_high_water_pages": 20000,
        "above_warn": true,
        "above_high_water": false,
        "above_truncate_high_water": false
    }))
    .expect("legacy payload must remain readable");

    assert_eq!(parsed.episode_elevated_ticks, None);
    assert_eq!(parsed.episode_peak_wal_pages, None);
}

#[test]
fn phase_started_payload_roundtrips() {
    let payload = PhaseStartedPayload {
        work_class: "warm".into(),
        phase: "ann_warm".into(),
        corpus_size: Some(553_000),
    };
    let json = serde_json::to_value(&payload).expect("serialize");
    let parsed: PhaseStartedPayload = serde_json::from_value(json).expect("deserialize");
    assert_eq!(parsed, payload);
}

#[test]
fn phase_completed_payload_roundtrips_with_absent_cpu_us() {
    let payload = PhaseCompletedPayload {
        work_class: "warm".into(),
        phase: "ann_warm".into(),
        wall_us: 41_000_000,
        cpu_us: None,
    };
    let json = serde_json::to_value(&payload).expect("serialize");
    let parsed: PhaseCompletedPayload = serde_json::from_value(json).expect("deserialize");
    assert_eq!(parsed, payload);
}

#[test]
fn phase_cancelled_payload_roundtrips() {
    let payload = PhaseCancelledPayload {
        work_class: "warm".into(),
        phase: "ann_warm".into(),
        wall_us: 12_000,
        cpu_us: Some(9_500),
    };
    let json = serde_json::to_value(&payload).expect("serialize");
    let parsed: PhaseCancelledPayload = serde_json::from_value(json).expect("deserialize");
    assert_eq!(parsed, payload);
}
