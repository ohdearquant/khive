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
