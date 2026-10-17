use super::*;
use alloc::string::ToString;
use serde_json::json;

#[test]
fn canonical_arm_preserves_opaque_delta_and_version() {
    let delta = "{\"schema_version\":1}\n{\"op\":\"create\"}\n";
    let value = json!({"kind":"canonical", "schema_version":1, "delta":delta});
    let decoded: ProposalChangeset = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        decoded,
        ProposalChangeset::Canonical {
            schema_version: 1,
            delta: delta.into()
        }
    );
    assert_eq!(serde_json::to_value(decoded).unwrap(), value);
}

#[test]
fn canonical_arm_requires_its_fields_and_rejects_unknown_fields() {
    for value in [
        json!({"kind":"canonical", "delta":"x"}),
        json!({"kind":"canonical", "schema_version":1}),
        json!({"kind":"canonical", "schema_version":1, "delta":12}),
        json!({"kind":"canonical", "schema_version":1, "delta":"x", "extra":true}),
    ] {
        assert!(serde_json::from_value::<ProposalChangeset>(value).is_err());
    }
}

#[test]
fn canonical_is_refused_at_every_compound_depth() {
    let mut value = json!({"kind":"canonical", "schema_version":1, "delta":"x"});
    for _ in 0..8 {
        value = json!({"kind":"compound", "steps":[value]});
        let error = serde_json::from_value::<ProposalChangeset>(value.clone()).unwrap_err();
        assert!(error.to_string().contains("must be top-level"));
    }
    let nested = ProposalChangeset::Compound {
        steps: alloc::vec![ProposalChangeset::Compound {
            steps: alloc::vec![ProposalChangeset::Canonical {
                schema_version: 1,
                delta: "x".into()
            }]
        }],
    };
    assert!(nested.validate().is_err());
}

#[test]
fn legacy_arms_retain_wire_shapes_and_unknown_field_tolerance() {
    let id1 = Id128::from_u128(1);
    let id2 = Id128::from_u128(2);
    for value in [
        json!({"kind":"add_entity", "entity":{"kind":"concept", "name":"one", "tags":[]}}),
        json!({"kind":"update_entity", "id":id1, "patch":{"name":"new"}}),
        json!({"kind":"add_edge", "source":id1, "target":id2, "relation":"extends", "weight":0.75}),
        json!({"kind":"add_note", "note":{"kind":"observation", "content":"note"}}),
        json!({"kind":"merge_entities", "into":id1, "from":id2}),
        json!({"kind":"supersede_entity", "old":id1, "new":id2}),
        json!({"kind":"compound", "steps":[]}),
    ] {
        let decoded: ProposalChangeset = serde_json::from_value(value.clone()).unwrap();
        let encoded = serde_json::to_vec(&decoded).unwrap();
        let mut extra = value;
        extra["legacy_extension"] = json!(true);
        let with_extra: ProposalChangeset = serde_json::from_value(extra).unwrap();
        assert_eq!(decoded, with_extra);
        assert_eq!(serde_json::to_vec(&with_extra).unwrap(), encoded);
    }
}

#[test]
fn historical_applied_payload_keeps_missing_preimage_absent() {
    let old = json!({
        "proposal_id":Id128::from_u128(1),
        "applied_at":123456_u64,
        "applied_by":"actor",
        "result":{"success":{"created_records":[]}},
    });
    let payload: ProposalAppliedPayload = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(payload.preimage, Option::None);
    assert_eq!(serde_json::to_value(payload).unwrap(), old);
}

#[test]
fn applied_preimage_markers_have_exact_typed_wire_values() {
    assert_eq!(
        serde_json::to_value(ProposalPreimageStatus::None).unwrap(),
        json!("none")
    );
    assert_eq!(
        serde_json::to_value(ProposalPreimageStatus::Captured).unwrap(),
        json!("captured")
    );
    assert!(serde_json::from_value::<ProposalPreimageStatus>(json!("unchecked")).is_err());
    for status in [
        ProposalPreimageStatus::None,
        ProposalPreimageStatus::Captured,
    ] {
        let payload = ProposalAppliedPayload {
            proposal_id: Id128::from_u128(1),
            applied_at: crate::Timestamp::EPOCH,
            applied_by: "actor".into(),
            result: ApplyResult::Success {
                created_records: Vec::new(),
            },
            preimage: Some(status),
        };
        let value = serde_json::to_value(&payload).unwrap();
        let restored: ProposalAppliedPayload = serde_json::from_value(value).unwrap();
        assert_eq!(restored, payload);
    }
}
