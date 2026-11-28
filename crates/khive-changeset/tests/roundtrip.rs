//! Property-based round-trip coverage for the NDJSON-delta codec: for every
//! op kind, `to_ndjson(cs)` then `from_ndjson(text)` must reconstruct a
//! change-set that re-serializes to byte-identical NDJSON.

use std::collections::BTreeMap;

use khive_changeset::{
    ChangeSet, CreateOp, CreateTarget, DeleteOp, DeletePreimage, EdgePatch, EdgePreimage,
    EntityCreateFields, EntityPatch, EntityPreimage, EntityRecord, Envelope, LinkOp, MergeOp,
    MergePreimage, NoteCreateFields, NotePatch, NotePreimage, Op, UpdateOp, UpdatePatch,
    UpdatePreimage,
};
use khive_types::{
    Entity, EntityKind, Header, Id128, Link, Namespace, Note, NoteStatus, Timestamp,
};

fn boxed(row: impl Into<EntityRecord>) -> Box<EntityRecord> {
    Box::new(row.into())
}
use khive_types::{EntityAlias, EntityAliasOrigin};
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
                key: None,
                aliases: vec![],
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
                    entity_type: None,
                    aliases: None,
                }),
                UpdatePreimage::Entity(EntityPreimage {
                    name: preimage_name,
                    description: Some(Some("prior-description".to_string())),
                    properties: None,
                    tags: None,
                    entity_type: None,
                    aliases: None,
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
            preimage: DeletePreimage::Entity(boxed(sample_entity(target_id, kind))),
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
                into: boxed(sample_entity(into_id, kind)),
                from: boxed(sample_entity(from_id, kind)),
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
                        key: None,
                        aliases: vec![],
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
        preimage: DeletePreimage::Entity(boxed(sample_entity(
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

// ---- ADR-014 tri-state entity_type on the changeset surface ----

fn entity_type_op(patch_type: Option<Option<String>>, preimage_type: Option<Option<String>>) -> Op {
    let op = UpdateOp::new(
        Id128::from_u128(1),
        UpdatePatch::Entity(EntityPatch {
            name: None,
            description: None,
            properties: None,
            tags: None,
            entity_type: patch_type,
            aliases: None,
        }),
        UpdatePreimage::Entity(EntityPreimage {
            name: None,
            description: None,
            properties: None,
            tags: None,
            entity_type: preimage_type,
            aliases: None,
        }),
    )
    .expect("congruent entity_type pair must construct");
    Op::Update(op)
}

#[test]
fn update_entity_type_set_roundtrips_through_ndjson() {
    let cs = ChangeSet::new(
        Envelope::new("agent:proptest", "family:test", Timestamp::from_secs(1)),
        vec![entity_type_op(
            Some(Some("algorithm".to_string())),
            Some(Some("paper".to_string())),
        )],
    );
    assert_roundtrips_byte_identical(&cs);
}

#[test]
fn update_entity_type_clear_roundtrips_through_ndjson() {
    let cs = ChangeSet::new(
        Envelope::new("agent:proptest", "family:test", Timestamp::from_secs(1)),
        vec![entity_type_op(
            Some(None),
            Some(Some("algorithm".to_string())),
        )],
    );
    let text = khive_changeset::to_ndjson(&cs).expect("serialize");
    assert!(
        text.contains("\"entity_type\":null"),
        "the clear must be serialized as an explicit null, not dropped: {text}"
    );
    assert_roundtrips_byte_identical(&cs);
}

#[test]
fn update_entity_type_congruence_rejects_mismatched_preimage() {
    // patch sets entity_type but the preimage omits the prior value.
    assert!(
        UpdateOp::new(
            Id128::from_u128(1),
            UpdatePatch::Entity(EntityPatch {
                name: None,
                description: None,
                properties: None,
                tags: None,
                entity_type: Some(Some("algorithm".to_string())),
                aliases: None,
            }),
            UpdatePreimage::Entity(EntityPreimage {
                name: None,
                description: None,
                properties: None,
                tags: None,
                entity_type: None,
                aliases: None,
            }),
        )
        .is_err(),
        "a touched entity_type without a captured prior value must be rejected"
    );
    // preimage captures entity_type but the patch leaves it unchanged.
    assert!(
        UpdateOp::new(
            Id128::from_u128(1),
            UpdatePatch::Entity(EntityPatch {
                name: None,
                description: None,
                properties: None,
                tags: None,
                entity_type: None,
                aliases: None,
            }),
            UpdatePreimage::Entity(EntityPreimage {
                name: None,
                description: None,
                properties: None,
                tags: None,
                entity_type: Some(Some("algorithm".to_string())),
                aliases: None,
            }),
        )
        .is_err(),
        "an unchanged entity_type with a preimage value must be rejected"
    );
}

fn edge_weight_update(weight: Option<f64>, prior: Option<f64>) -> Result<UpdateOp, String> {
    UpdateOp::new(
        Id128::from_u128(1),
        UpdatePatch::Edge(EdgePatch {
            relation: None,
            weight,
        }),
        UpdatePreimage::Edge(EdgePreimage {
            relation: None,
            weight: prior,
        }),
    )
}

#[test]
fn checked_edge_updates_reject_invalid_new_weights() {
    for weight in [-0.01, 1.01, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let error = edge_weight_update(Some(weight), Some(0.5))
            .expect_err("the checked constructor must not create an undecodable edge update");
        let expected = if weight.is_finite() {
            format!("EdgePatch weight must be in [0.0, 1.0], got {weight}")
        } else {
            format!("EdgePatch weight must be finite, got {weight}")
        };
        assert_eq!(error, expected);
        if weight.is_finite() {
            let wire = serde_json::json!({
                "target_id": "00000000-0000-0000-0000-000000000001",
                "patch": {"target": "edge", "weight": weight},
                "preimage": {"target": "edge", "weight": 0.5}
            });
            let wire_error = serde_json::from_value::<UpdateOp>(wire).unwrap_err();
            assert!(wire_error.to_string().contains(&error));
        }
    }
}

#[test]
fn valid_checked_edge_weights_keep_exact_ndjson_roundtrips() {
    for weight in [None, Some(0.0), Some(-0.0), Some(0.5), Some(1.0)] {
        let update = edge_weight_update(weight, weight.map(|_| 0.5)).unwrap();
        let cs = ChangeSet::new(
            Envelope::new("agent:test", "family:test", Timestamp::from_secs(1)),
            vec![Op::Update(update)],
        );
        assert_roundtrips_byte_identical(&cs);
        let wire = serde_json::to_value(&cs.ops[0]).unwrap();
        assert_eq!(
            wire["patch"].get("weight"),
            weight.map(serde_json::Value::from).as_ref()
        );
    }
}

#[test]
fn edge_constructor_preserves_preimage_diagnostic_precedence() {
    assert_eq!(
        edge_weight_update(Some(1.5), None).unwrap_err(),
        "UpdateOp preimage is missing `weight`, which the patch sets or clears"
    );
    assert_eq!(
        edge_weight_update(None, Some(0.5)).unwrap_err(),
        "UpdateOp preimage carries `weight`, which the patch leaves unchanged"
    );
    assert_eq!(
        edge_weight_update(Some(1.5), Some(-0.5)).unwrap_err(),
        "UpdateOp edge preimage weight must be finite and in [0.0, 1.0], got -0.5"
    );
}

fn alias_patch(aliases: Option<Vec<EntityAlias>>) -> UpdatePatch {
    UpdatePatch::Entity(EntityPatch {
        name: None,
        description: None,
        properties: None,
        tags: None,
        entity_type: None,
        aliases,
    })
}

fn alias_preimage(aliases: Option<Vec<EntityAlias>>) -> UpdatePreimage {
    UpdatePreimage::Entity(EntityPreimage {
        name: None,
        description: None,
        properties: None,
        tags: None,
        entity_type: None,
        aliases,
    })
}

fn held_alias(alias: &str, origin: EntityAliasOrigin) -> EntityAlias {
    EntityAlias {
        alias: alias.into(),
        origin,
    }
}

#[test]
fn an_alias_set_on_one_side_only_or_naming_an_alias_twice_is_refused() {
    let id = Id128::from_u128(1);
    let one = vec![held_alias("a", EntityAliasOrigin::Caller)];
    let twice = vec![
        held_alias("a", EntityAliasOrigin::Caller),
        held_alias("a", EntityAliasOrigin::Rename),
    ];
    for (patch, preimage) in [(Some(one.clone()), None), (None, Some(one.clone()))] {
        let error = UpdateOp::new(id, alias_patch(patch), alias_preimage(preimage)).unwrap_err();
        assert!(error.contains("`aliases`"), "{error}");
    }
    for (patch, preimage) in [(twice.clone(), one.clone()), (one.clone(), twice)] {
        let error = UpdateOp::new(id, alias_patch(Some(patch)), alias_preimage(Some(preimage)))
            .unwrap_err();
        assert!(error.contains("\"a\" more than once"), "{error}");
    }
    let decoded: Result<UpdateOp, _> = serde_json::from_value(serde_json::json!({
        "target_id": id,
        "patch": {"target": "entity", "aliases": [
            {"alias": "a", "origin": "caller"},
            {"alias": "a", "origin": "merge"}
        ]},
        "preimage": {"target": "entity", "aliases": []}
    }));
    assert!(decoded.is_err(), "decoding runs the same check");
    // An empty set clears every alias.
    UpdateOp::new(id, alias_patch(Some(vec![])), alias_preimage(Some(one))).unwrap();
}

#[test]
fn keyed_creates_and_records_with_aliases_roundtrip_byte_identical() {
    let mut record = EntityRecord::new(sample_entity(Id128::from_u128(7), EntityKind::Concept));
    record.keys = vec!["doi:10.1/x".into()];
    record.aliases = vec![held_alias("x", EntityAliasOrigin::Rename)];
    let cs = ChangeSet::new(
        Envelope::new("agent:proptest", "family:test", Timestamp::from_secs(1)),
        vec![
            Op::Create(CreateOp {
                id: Id128::from_u128(8),
                namespace: Namespace::local(),
                target: CreateTarget::Entity(EntityCreateFields {
                    entity_kind: EntityKind::Concept,
                    entity_type: None,
                    name: "y".into(),
                    description: None,
                    properties: BTreeMap::new(),
                    tags: vec![],
                    key: Some("isbn:1".into()),
                    aliases: vec!["why".into()],
                }),
            }),
            Op::Delete(DeleteOp {
                target_id: Id128::from_u128(7),
                hard: false,
                preimage: DeletePreimage::Entity(boxed(record)),
            }),
        ],
    );
    let text = khive_changeset::to_ndjson(&cs).expect("serialize");
    for expected in [
        r#""key":"isbn:1""#,
        r#""aliases":["why"]"#,
        r#""keys":["doi:10.1/x"]"#,
        r#""aliases":[{"alias":"x","origin":"rename"}]"#,
    ] {
        assert!(text.contains(expected), "{expected} missing from {text}");
    }
    assert_roundtrips_byte_identical(&cs);
}
