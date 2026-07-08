//! Property-based round-trip coverage for the NDJSON-delta codec: for every
//! op kind, `to_ndjson(cs)` then `from_ndjson(text)` must reconstruct a
//! change-set that re-serializes to byte-identical NDJSON.

use std::collections::BTreeMap;

use khive_changeset::{
    ChangeSet, CreateOp, CreateTarget, DeleteOp, DeletePreimage, EdgePatch, EdgePreimage,
    EntityCreateFields, EntityPatch, EntityPreimage, Envelope, LinkOp, MergeOp, MergePreimage,
    NoteCreateFields, NotePatch, NotePreimage, Op, UpdateOp, UpdatePatch, UpdatePreimage,
};
use khive_types::{
    Entity, EntityKind, Header, Id128, Link, Namespace, Note, NoteStatus, Timestamp,
};
use proptest::prelude::*;

fn entity_kind_strategy() -> impl Strategy<Value = EntityKind> {
    prop::sample::select(EntityKind::ALL.to_vec())
}

fn edge_relation_strategy() -> impl Strategy<Value = khive_types::EdgeRelation> {
    prop::sample::select(khive_types::EdgeRelation::ALL.to_vec())
}

fn id_strategy() -> impl Strategy<Value = Id128> {
    any::<u128>()
        .prop_map(Id128::from_u128)
        .prop_filter("nil id is not a realistic minted identifier", |id| {
            !id.is_nil()
        })
}

fn weight_strategy() -> impl Strategy<Value = f64> {
    0.0f64..=1.0f64
}

fn sample_entity(id: Id128, kind: EntityKind) -> Entity {
    Entity {
        header: Header::new(id, Namespace::local(), Timestamp::from_secs(1)),
        kind,
        entity_type: None,
        name: "preimage-entity".into(),
        description: None,
        properties: BTreeMap::new(),
        tags: vec![],
        deleted_at: None,
    }
}

fn sample_note(id: Id128) -> Note {
    Note {
        header: Header::new(id, Namespace::local(), Timestamp::from_secs(1)),
        kind: "observation".into(),
        status: NoteStatus::Active,
        content: "preimage note body".into(),
        properties: BTreeMap::new(),
        tags: vec![],
        salience: Some(0.5),
        decay_factor: Some(0.01),
        expires_at: None,
        deleted_at: None,
    }
}

fn sample_link(id: Id128, source: Id128, target: Id128) -> Link {
    let ts = Timestamp::from_secs(1);
    Link {
        id,
        namespace: "local".into(),
        source,
        target,
        relation: khive_types::EdgeRelation::Extends,
        properties: BTreeMap::new(),
        weight: 0.9,
        created_at: ts,
        updated_at: ts,
        deleted_at: None,
    }
}

fn assert_roundtrips_byte_identical(cs: &ChangeSet) {
    let text = khive_changeset::to_ndjson(cs).expect("serialize");
    let decoded = khive_changeset::from_ndjson(&text).expect("deserialize");
    let text2 = khive_changeset::to_ndjson(&decoded).expect("re-serialize");
    assert_eq!(text, text2, "round-trip must be byte-identical");
    assert_eq!(decoded.ops.len(), cs.ops.len());
}

proptest! {
    #[test]
    fn create_entity_op_roundtrips(
        id in id_strategy(),
        kind in entity_kind_strategy(),
        name in "[a-zA-Z0-9 ]{1,32}",
        tags in prop::collection::vec("[a-z]{1,8}", 0..4),
    ) {
        let envelope = Envelope::new("agent:proptest", "family:test", Timestamp::from_secs(1));
        let op = Op::Create(CreateOp {
            id,
            namespace: Namespace::local(),
            target: CreateTarget::Entity(EntityCreateFields {
                entity_kind: kind,
                entity_type: None,
                name,
                description: None,
                properties: BTreeMap::new(),
                tags,
            }),
        });
        let cs = ChangeSet::new(envelope, vec![op]);
        assert_roundtrips_byte_identical(&cs);
    }

    #[test]
    fn create_note_op_roundtrips(
        id in id_strategy(),
        content in "[a-zA-Z0-9 ]{1,64}",
        salience in prop::option::of(0.0f64..=1.0f64),
    ) {
        let envelope = Envelope::new("agent:proptest", "family:test", Timestamp::from_secs(1));
        let op = Op::Create(CreateOp {
            id,
            namespace: Namespace::local(),
            target: CreateTarget::Note(NoteCreateFields {
                note_kind: "observation".into(),
                content,
                properties: BTreeMap::new(),
                tags: vec![],
                salience,
                decay_factor: None,
            }),
        });
        let cs = ChangeSet::new(envelope, vec![op]);
        assert_roundtrips_byte_identical(&cs);
    }

    #[test]
    fn link_op_roundtrips(
        id in id_strategy(),
        source in id_strategy(),
        target in id_strategy(),
        relation in edge_relation_strategy(),
        weight in weight_strategy(),
    ) {
        let envelope = Envelope::new("agent:proptest", "family:test", Timestamp::from_secs(1));
        let op = Op::Link(LinkOp {
            id,
            namespace: Namespace::local(),
            source,
            target,
            relation,
            weight,
            properties: BTreeMap::new(),
        });
        let cs = ChangeSet::new(envelope, vec![op]);
        assert_roundtrips_byte_identical(&cs);
    }

    #[test]
    fn update_entity_patch_roundtrips(
        target_id in id_strategy(),
        name in prop::option::of("[a-zA-Z0-9 ]{1,16}"),
    ) {
        let envelope = Envelope::new("agent:proptest", "family:test", Timestamp::from_secs(1));
        // preimage field presence must exactly mirror the patch's touched fields.
        let preimage_name = name.is_some().then(|| "prior-name".to_string());
        let op = Op::Update(
            UpdateOp::new(
                target_id,
                UpdatePatch::Entity(EntityPatch {
                    name,
                    description: Some(None),
                    properties: None,
                    tags: None,
                }),
                UpdatePreimage::Entity(EntityPreimage {
                    name: preimage_name,
                    description: Some(Some("prior-description".to_string())),
                    properties: None,
                    tags: None,
                }),
            )
            .unwrap(),
        );
        let cs = ChangeSet::new(envelope, vec![op]);
        assert_roundtrips_byte_identical(&cs);
    }

    #[test]
    fn update_note_patch_roundtrips(
        target_id in id_strategy(),
        content in prop::option::of("[a-zA-Z0-9 ]{1,16}"),
    ) {
        let envelope = Envelope::new("agent:proptest", "family:test", Timestamp::from_secs(1));
        // preimage field presence must exactly mirror the patch's touched fields.
        let preimage_content = content.is_some().then(|| "prior-content".to_string());
        let op = Op::Update(
            UpdateOp::new(
                target_id,
                UpdatePatch::Note(NotePatch {
                    content,
                    salience: Some(Some(0.42)),
                    decay_factor: None,
                    properties: None,
                    tags: None,
                }),
                UpdatePreimage::Note(NotePreimage {
                    content: preimage_content,
                    salience: Some(Some(0.1)),
                    decay_factor: None,
                    properties: None,
                    tags: None,
                }),
            )
            .unwrap(),
        );
        let cs = ChangeSet::new(envelope, vec![op]);
        assert_roundtrips_byte_identical(&cs);
    }

    #[test]
    fn update_edge_patch_roundtrips(
        target_id in id_strategy(),
        relation in prop::option::of(edge_relation_strategy()),
        weight in prop::option::of(weight_strategy()),
    ) {
        let envelope = Envelope::new("agent:proptest", "family:test", Timestamp::from_secs(1));
        // preimage field presence must exactly mirror the patch's touched fields.
        let preimage = EdgePreimage {
            relation: relation.map(|_| khive_types::EdgeRelation::Extends),
            weight: weight.map(|_| 0.5),
        };
        let op = Op::Update(
            UpdateOp::new(
                target_id,
                UpdatePatch::Edge(EdgePatch { relation, weight }),
                UpdatePreimage::Edge(preimage),
            )
            .unwrap(),
        );
        let cs = ChangeSet::new(envelope, vec![op]);
        assert_roundtrips_byte_identical(&cs);
    }

    #[test]
    fn delete_entity_op_roundtrips(target_id in id_strategy(), kind in entity_kind_strategy()) {
        let envelope = Envelope::new("agent:proptest", "family:test", Timestamp::from_secs(1));
        let op = Op::Delete(DeleteOp {
            target_id,
            hard: false,
            preimage: DeletePreimage::Entity(Box::new(sample_entity(target_id, kind))),
        });
        let cs = ChangeSet::new(envelope, vec![op]);
        assert_roundtrips_byte_identical(&cs);
    }

    #[test]
    fn delete_note_op_roundtrips(target_id in id_strategy()) {
        let envelope = Envelope::new("agent:proptest", "family:test", Timestamp::from_secs(1));
        let op = Op::Delete(DeleteOp {
            target_id,
            hard: true,
            preimage: DeletePreimage::Note(Box::new(sample_note(target_id))),
        });
        let cs = ChangeSet::new(envelope, vec![op]);
        assert_roundtrips_byte_identical(&cs);
    }

    #[test]
    fn delete_edge_op_roundtrips(target_id in id_strategy(), source in id_strategy(), target in id_strategy()) {
        let envelope = Envelope::new("agent:proptest", "family:test", Timestamp::from_secs(1));
        let op = Op::Delete(DeleteOp {
            target_id,
            hard: false,
            preimage: DeletePreimage::Edge(Box::new(sample_link(target_id, source, target))),
        });
        let cs = ChangeSet::new(envelope, vec![op]);
        assert_roundtrips_byte_identical(&cs);
    }

    #[test]
    fn merge_op_roundtrips(
        into_id in id_strategy(),
        from_id in id_strategy(),
        kind in entity_kind_strategy(),
        edge_id in id_strategy(),
    ) {
        let envelope = Envelope::new("agent:proptest", "family:test", Timestamp::from_secs(1));
        let incident = sample_link(edge_id, from_id, into_id);
        let op = Op::Merge(MergeOp {
            into_id,
            from_id,
            preimage: MergePreimage {
                into: Box::new(sample_entity(into_id, kind)),
                from: Box::new(sample_entity(from_id, kind)),
                incident_edges: vec![incident],
            },
        });
        let cs = ChangeSet::new(envelope, vec![op]);
        assert_roundtrips_byte_identical(&cs);
    }

    #[test]
    fn op_order_survives_arbitrary_shuffles(ids in prop::collection::vec(id_strategy(), 1..8)) {
        let envelope = Envelope::new("agent:proptest", "family:test", Timestamp::from_secs(1));
        let ops: Vec<Op> = ids
            .iter()
            .map(|&id| {
                Op::Create(CreateOp {
                    id,
                    namespace: Namespace::local(),
                    target: CreateTarget::Entity(EntityCreateFields {
                        entity_kind: EntityKind::Concept,
                        entity_type: None,
                        name: "n".into(),
                        description: None,
                        properties: BTreeMap::new(),
                        tags: vec![],
                    }),
                })
            })
            .collect();
        let cs = ChangeSet::new(envelope, ops);
        let text = khive_changeset::to_ndjson(&cs).unwrap();
        let decoded = khive_changeset::from_ndjson(&text).unwrap();
        let decoded_ids: Vec<u128> = decoded
            .ops
            .iter()
            .map(|op| match op {
                Op::Create(c) => c.id.to_u128(),
                _ => unreachable!(),
            })
            .collect();
        let original_ids: Vec<u128> = ids.iter().map(|id| id.to_u128()).collect();
        prop_assert_eq!(decoded_ids, original_ids);
    }
}

#[test]
fn probe_rejects_out_of_range_edge_patch_weight() {
    let json = serde_json::json!({
        "target_id": "00000000-0000-0000-0000-000000000001",
        "patch": {
            "target": "edge",
            "weight": 1.5
        },
        "preimage": {
            "target": "edge",
            "weight": 0.5
        }
    });

    let result: Result<UpdateOp, _> = serde_json::from_value(json);
    let err = result.expect_err("edge update patches must reject weights outside [0.0, 1.0]");
    let message = err.to_string();
    assert!(
        message.contains("must be in [0.0, 1.0]"),
        "expected the weight-bound error, got: {message}"
    );
}

#[test]
fn probe_rejects_delete_preimage_with_mismatched_record_id() {
    let envelope = Envelope::new("agent:probe", "family:test", Timestamp::from_secs(1));
    let target_id = Id128::from_u128(1);
    let wrong_preimage_id = Id128::from_u128(2);
    let op = Op::Delete(DeleteOp {
        target_id,
        hard: false,
        preimage: DeletePreimage::Entity(Box::new(sample_entity(
            wrong_preimage_id,
            EntityKind::Concept,
        ))),
    });
    let cs = ChangeSet::new(envelope, vec![op]);
    let text = khive_changeset::to_ndjson(&cs).unwrap();

    let result = khive_changeset::from_ndjson(&text);
    assert!(
        result.is_err(),
        "delete preimage id must match the deleted target_id"
    );
}

#[test]
fn probe_rejects_unknown_field_inside_delete_preimage() {
    let json = serde_json::json!({
        "target_id": "00000000-0000-0000-0000-000000000001",
        "hard": false,
        "preimage": {
            "substrate": "entity",
            "id": "00000000-0000-0000-0000-000000000001",
            "namespace": "local",
            "created_at": 1,
            "updated_at": 1,
            "kind": "concept",
            "entity_type": null,
            "name": "preimage",
            "description": null,
            "properties": {},
            "tags": [],
            "deleted_at": null,
            "unexpected": true
        }
    });

    let result: Result<DeleteOp, _> = serde_json::from_value(json);
    assert!(
        result.is_err(),
        "unknown fields inside full-record preimages must not be silently dropped"
    );
}
