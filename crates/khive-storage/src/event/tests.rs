use super::*;

#[test]
fn event_filter_target_id_roundtrips_and_defaults_for_legacy_frames() {
    let target = Uuid::new_v4();
    let filter = EventFilter {
        target_id: Some(target),
        ..EventFilter::default()
    };
    let mut wire = serde_json::to_value(&filter).unwrap();
    assert_eq!(wire["target_id"], target.to_string());
    let decoded: EventFilter = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(decoded.target_id, Some(target));
    wire.as_object_mut().unwrap().remove("target_id");
    let legacy: EventFilter = serde_json::from_value(wire).unwrap();
    assert_eq!(legacy.target_id, None);
}

#[test]
fn event_page_window_budget_stop_roundtrips_and_defaults_when_absent() {
    let stop = EventOrderKey {
        created_at_us: 7,
        physical_id: Uuid::from_u128(9).to_string(),
    };
    let window = EventPageWindow {
        rows: Vec::new(),
        budget_stop: Some(stop.clone()),
    };
    let mut wire = serde_json::to_value(&window).unwrap();
    let decoded: EventPageWindow = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(decoded.budget_stop, Some(stop));
    wire.as_object_mut().unwrap().remove("budget_stop");
    let legacy: EventPageWindow = serde_json::from_value(wire).unwrap();
    assert_eq!(legacy.budget_stop, None);
}

#[test]
fn event_new_generates_uuid_v7() {
    let event = Event::new(
        "default",
        "search",
        EventKind::SearchExecuted,
        SubstrateKind::Note,
        "agent:test",
    );

    // The UUID version is the high nibble of byte 6.
    assert_eq!(event.id.as_bytes()[6] >> 4, 7);
}

#[test]
fn event_new_generates_unique_valid_uuid_v7_ids() {
    let mut ids = std::collections::HashSet::new();
    for _ in 0..64 {
        let event = Event::new(
            "default",
            "search",
            EventKind::SearchExecuted,
            SubstrateKind::Note,
            "agent:test",
        );
        let bytes = event.id.as_bytes();
        assert_eq!(bytes[6] >> 4, 7, "new event identity must be UUIDv7");
        assert_eq!(bytes[8] >> 6, 0b10, "UUID must use the RFC variant");
        assert!(
            ids.insert(event.id),
            "new event identities must be distinct"
        );
    }
}

#[test]
fn event_serde_preserves_legacy_uuid_v4_identity() {
    let legacy_id = Uuid::from_u128(0xf47a_c10b_58cc_4372_a567_0e02_b2c3_d479);
    assert_eq!(legacy_id.as_bytes()[6] >> 4, 4);
    let legacy = Event {
        id: legacy_id,
        namespace: "legacy".into(),
        verb: "search".into(),
        substrate: SubstrateKind::Note,
        actor: "agent:legacy".into(),
        kind: EventKind::SearchExecuted,
        outcome: EventOutcome::Success,
        payload: serde_json::json!({"result_kind": "note", "legacy": true}),
        payload_schema_version: 1,
        profile_state_version: Some(3),
        duration_us: 23,
        target_id: None,
        session_id: None,
        aggregate_kind: None,
        aggregate_id: None,
        created_at: 1_700_000_000_000_000,
        op_index: None,
        ref_resolution: None,
    };
    let wire = serde_json::to_value(&legacy).unwrap();
    assert_eq!(wire["id"], "f47ac10b-58cc-4372-a567-0e02b2c3d479");
    let decoded: Event = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(decoded.id, legacy_id);
    assert_eq!(decoded, legacy);
    assert_eq!(serde_json::to_value(decoded).unwrap(), wire);
}
