use super::*;
use khive_types::{OperationAttribution, RefResolution};

#[test]
fn audit_operation_fields_are_closed_and_legacy_absence_stays_unknown() {
    let request = crate::GateRequest::new(
        crate::ActorRef::anonymous(),
        khive_types::Namespace::local(),
        "get",
        serde_json::json!({}),
    );
    let direct = AuditEvent::from_check(
        &request,
        &GateDecision::Allow {
            obligations: vec![],
        },
        "test",
    );
    assert_eq!((direct.op_index, direct.ref_resolution), (None, None));
    let mut old = serde_json::to_value(&direct).unwrap();
    old.as_object_mut().unwrap().remove("op_index");
    old.as_object_mut().unwrap().remove("ref_resolution");
    let legacy: AuditEvent = serde_json::from_value(old).unwrap();
    assert_eq!((legacy.op_index, legacy.ref_resolution), (None, None));

    let attributed = direct.with_operation_attribution(Some(OperationAttribution {
        op_index: 3,
        ref_resolution: RefResolution::Resolved,
    }));
    let mut value = serde_json::to_value(attributed).unwrap();
    assert_eq!(value["op_index"], 3);
    assert_eq!(value["ref_resolution"], "resolved");
    value["ref_resolution"] = serde_json::json!("unknown");
    assert!(serde_json::from_value::<AuditEvent>(value).is_err());
}
