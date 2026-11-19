//! Public inversion controls with independent records and literal expected ops.

use std::collections::BTreeMap;

use khive_changeset::{
    from_ndjson, to_ndjson, ChangeSet, ChangeSetError, CreateOp, CreateTarget, CurrentState,
    DeleteOp, DeletePreimage, EdgePatch, EdgePreimage, EntityCreateFields, EntityPatch,
    EntityPreimage, Envelope, InvertError, LinkOp, MergeOp, MergePreimage, NoteCreateFields,
    NotePatch, NotePreimage, Op, RestoreOp, UpdateOp, UpdatePatch, UpdatePreimage,
};
use khive_types::{
    EdgeRelation, Entity, EntityKind, Header, Id128, Link, Namespace, Note, NoteStatus,
    PropertyValue, Timestamp,
};
use serde_json::{json, Value};

fn id(value: u128) -> Id128 {
    Id128::from_u128(value)
}

fn envelope(sequence: u64) -> Envelope {
    Envelope::new(
        format!("agent:{sequence}"),
        "family:inversion-test",
        Timestamp::from_secs(sequence),
    )
    .with_batch_id(format!("batch:{sequence}"))
}

fn properties(label: &str) -> BTreeMap<String, PropertyValue> {
    BTreeMap::from([
        ("label".into(), PropertyValue::String(label.into())),
        ("count".into(), PropertyValue::Integer(7)),
    ])
}

fn entity(identifier: Id128, name: &str) -> Entity {
    Entity {
        header: Header {
            id: identifier,
            namespace: Namespace::parse("test:inversion").unwrap(),
            created_at: Timestamp::from_secs(17),
            updated_at: Timestamp::from_secs(29),
        },
        kind: EntityKind::Concept,
        entity_type: Some("topic".into()),
        name: name.into(),
        description: Some("record description".into()),
        properties: properties("entity"),
        tags: vec!["one".into(), "two".into()],
        deleted_at: None,
    }
}

fn note(identifier: Id128, content: &str) -> Note {
    Note {
        header: Header {
            id: identifier,
            namespace: Namespace::parse("test:inversion").unwrap(),
            created_at: Timestamp::from_secs(31),
            updated_at: Timestamp::from_secs(43),
        },
        kind: "observation".into(),
        status: NoteStatus::Active,
        content: content.into(),
        properties: properties("note"),
        tags: vec!["note-tag".into()],
        salience: Some(0.625),
        decay_factor: Some(0.02),
        expires_at: None,
        deleted_at: None,
    }
}

fn edge(identifier: Id128) -> Link {
    Link {
        id: identifier,
        namespace: "test:inversion".into(),
        source: id(1),
        target: id(2),
        relation: EdgeRelation::Extends,
        weight: 0.625,
        properties: properties("edge"),
        created_at: Timestamp::from_secs(47),
        updated_at: Timestamp::from_secs(59),
        deleted_at: None,
    }
}

fn create_entity(row: &Entity) -> Op {
    Op::Create(CreateOp {
        id: row.header.id,
        namespace: row.header.namespace.clone(),
        target: CreateTarget::Entity(EntityCreateFields {
            entity_kind: row.kind,
            entity_type: row.entity_type.clone(),
            name: row.name.clone(),
            description: row.description.clone(),
            properties: row.properties.clone(),
            tags: row.tags.clone(),
        }),
    })
}

fn create_note(row: &Note) -> Op {
    Op::Create(CreateOp {
        id: row.header.id,
        namespace: row.header.namespace.clone(),
        target: CreateTarget::Note(NoteCreateFields {
            note_kind: row.kind.clone(),
            content: row.content.clone(),
            properties: row.properties.clone(),
            tags: row.tags.clone(),
            salience: row.salience,
            decay_factor: row.decay_factor,
        }),
    })
}

fn link(row: &Link) -> Op {
    Op::Link(LinkOp {
        id: row.id,
        namespace: Namespace::parse(&row.namespace).unwrap(),
        source: row.source,
        target: row.target,
        relation: row.relation,
        weight: row.weight,
        properties: row.properties.clone(),
    })
}

fn rename(identifier: Id128, new: &str, old: &str) -> Op {
    Op::Update(
        UpdateOp::new(
            identifier,
            UpdatePatch::Entity(EntityPatch {
                name: Some(new.into()),
                description: None,
                entity_type: None,
                properties: None,
                tags: None,
            }),
            UpdatePreimage::Entity(EntityPreimage {
                name: Some(old.into()),
                description: None,
                entity_type: None,
                properties: None,
                tags: None,
            }),
        )
        .unwrap(),
    )
}

fn assert_inverse(forward: &ChangeSet, current: &CurrentState, expected_ops: Vec<Op>) -> ChangeSet {
    let actual = forward.invert(envelope(2), current).unwrap();
    let expected = ChangeSet::new(envelope(2), expected_ops);
    assert_eq!(to_ndjson(&actual).unwrap(), to_ndjson(&expected).unwrap());
    let text = to_ndjson(&actual).unwrap();
    assert_eq!(to_ndjson(&from_ndjson(&text).unwrap()).unwrap(), text);
    actual
}

fn assert_invalidated(error: InvertError, index: usize, identifier: Id128, field: &str) {
    match error {
        InvertError::Invalidated {
            index: actual_index,
            id: actual_id,
            field: actual_field,
        } => assert_eq!(
            (actual_index, actual_id, actual_field),
            (index, identifier, field)
        ),
        other => panic!("expected Invalidated({index}, {identifier}, {field}), got {other:?}"),
    }
}

fn snapshot(current: &CurrentState) -> Value {
    json!({
        "entities": current.entities,
        "notes": current.notes,
        "edges": current.edges,
    })
}

#[test]
fn creates_and_link_carry_supplied_full_records_into_reverse_soft_deletes() {
    let e = entity(id(1), "created entity");
    let n = note(id(3), "created note");
    let l = edge(id(4));
    let forward = ChangeSet::new(
        envelope(1),
        vec![create_entity(&e), create_note(&n), link(&l)],
    );
    let current = CurrentState {
        entities: BTreeMap::from([(id(1), e.clone())]),
        notes: BTreeMap::from([(id(3), n.clone())]),
        edges: BTreeMap::from([(id(4), l.clone())]),
    };
    let inverse = assert_inverse(
        &forward,
        &current,
        vec![
            Op::Delete(DeleteOp {
                target_id: id(4),
                hard: false,
                preimage: DeletePreimage::Edge(Box::new(l)),
            }),
            Op::Delete(DeleteOp {
                target_id: id(3),
                hard: false,
                preimage: DeletePreimage::Note(Box::new(n)),
            }),
            Op::Delete(DeleteOp {
                target_id: id(1),
                hard: false,
                preimage: DeletePreimage::Entity(Box::new(e)),
            }),
        ],
    );
    // These are live fixture timestamps, not either envelope's staged_at.
    let wire = serde_json::to_value(&inverse.ops[2]).unwrap();
    assert_eq!(wire["hard"], false);
    assert_eq!(wire["preimage"]["created_at"], 17_000_000_u64);
    assert_eq!(wire["preimage"]["updated_at"], 29_000_000_u64);
    assert_eq!(
        wire["preimage"]["properties"],
        json!({"count": 7, "label": "entity"})
    );
}

#[test]
fn entity_update_swaps_every_field_preserving_nullable_clear_and_whole_maps() {
    let mut current_row = entity(id(1), "new");
    current_row.description = None;
    current_row.entity_type = Some("new-type".into());
    current_row.properties = properties("new-map");
    current_row.tags = vec!["new-tag".into()];
    let update = Op::Update(
        UpdateOp::new(
            id(1),
            UpdatePatch::Entity(EntityPatch {
                name: Some("new".into()),
                description: Some(None),
                entity_type: Some(Some("new-type".into())),
                properties: Some(properties("new-map")),
                tags: Some(vec!["new-tag".into()]),
            }),
            UpdatePreimage::Entity(EntityPreimage {
                name: Some("old".into()),
                description: Some(Some("old-description".into())),
                entity_type: Some(None),
                properties: Some(properties("old-map")),
                tags: Some(vec!["old-tag".into()]),
            }),
        )
        .unwrap(),
    );
    let expected = Op::Update(
        UpdateOp::new(
            id(1),
            UpdatePatch::Entity(EntityPatch {
                name: Some("old".into()),
                description: Some(Some("old-description".into())),
                entity_type: Some(None),
                properties: Some(properties("old-map")),
                tags: Some(vec!["old-tag".into()]),
            }),
            UpdatePreimage::Entity(EntityPreimage {
                name: Some("new".into()),
                description: Some(None),
                entity_type: Some(Some("new-type".into())),
                properties: Some(properties("new-map")),
                tags: Some(vec!["new-tag".into()]),
            }),
        )
        .unwrap(),
    );
    assert_inverse(
        &ChangeSet::new(envelope(1), vec![update]),
        &CurrentState {
            entities: BTreeMap::from([(id(1), current_row)]),
            ..Default::default()
        },
        vec![expected],
    );
}

#[test]
fn note_and_edge_updates_swap_their_fields_without_clearing_untouched_fields() {
    let mut n = note(id(3), "new-content");
    n.salience = None;
    n.decay_factor = Some(0.0);
    n.properties = properties("new-note-map");
    n.tags = vec!["new-note-tag".into()];
    let mut l = edge(id(4));
    l.relation = EdgeRelation::Contains;
    l.weight = 1.0;
    let forward = ChangeSet::new(
        envelope(1),
        vec![
            Op::Update(
                UpdateOp::new(
                    id(3),
                    UpdatePatch::Note(NotePatch {
                        content: Some("new-content".into()),
                        salience: Some(None),
                        decay_factor: Some(Some(0.0)),
                        properties: Some(properties("new-note-map")),
                        tags: Some(vec!["new-note-tag".into()]),
                    }),
                    UpdatePreimage::Note(NotePreimage {
                        content: Some("old-content".into()),
                        salience: Some(Some(0.5)),
                        decay_factor: Some(None),
                        properties: Some(properties("old-note-map")),
                        tags: Some(vec!["old-note-tag".into()]),
                    }),
                )
                .unwrap(),
            ),
            Op::Update(
                UpdateOp::new(
                    id(4),
                    UpdatePatch::Edge(EdgePatch {
                        relation: Some(EdgeRelation::Contains),
                        weight: Some(1.0),
                    }),
                    UpdatePreimage::Edge(EdgePreimage {
                        relation: Some(EdgeRelation::Extends),
                        weight: Some(0.625),
                    }),
                )
                .unwrap(),
            ),
        ],
    );
    let current = CurrentState {
        notes: BTreeMap::from([(id(3), n)]),
        edges: BTreeMap::from([(id(4), l)]),
        ..Default::default()
    };
    assert_inverse(
        &forward,
        &current,
        vec![
            Op::Update(
                UpdateOp::new(
                    id(4),
                    UpdatePatch::Edge(EdgePatch {
                        relation: Some(EdgeRelation::Extends),
                        weight: Some(0.625),
                    }),
                    UpdatePreimage::Edge(EdgePreimage {
                        relation: Some(EdgeRelation::Contains),
                        weight: Some(1.0),
                    }),
                )
                .unwrap(),
            ),
            Op::Update(
                UpdateOp::new(
                    id(3),
                    UpdatePatch::Note(NotePatch {
                        content: Some("old-content".into()),
                        salience: Some(Some(0.5)),
                        decay_factor: Some(None),
                        properties: Some(properties("old-note-map")),
                        tags: Some(vec!["old-note-tag".into()]),
                    }),
                    UpdatePreimage::Note(NotePreimage {
                        content: Some("new-content".into()),
                        salience: Some(None),
                        decay_factor: Some(Some(0.0)),
                        properties: Some(properties("new-note-map")),
                        tags: Some(vec!["new-note-tag".into()]),
                    }),
                )
                .unwrap(),
            ),
        ],
    );
    let mut e = entity(id(1), "after");
    e.description = Some("later unrelated description".into());
    e.properties = properties("later unrelated map");
    let inverse = assert_inverse(
        &ChangeSet::new(envelope(1), vec![rename(id(1), "after", "before")]),
        &CurrentState {
            entities: BTreeMap::from([(id(1), e)]),
            ..Default::default()
        },
        vec![rename(id(1), "before", "after")],
    );
    let wire = serde_json::to_value(&inverse.ops[0]).unwrap();
    for field in ["description", "entity_type", "properties", "tags"] {
        assert!(wire["patch"].get(field).is_none());
        assert!(wire["preimage"].get(field).is_none());
    }
}

#[test]
fn mixed_create_link_update_reverses_order_and_keeps_stable_ids() {
    let original = entity(id(1), "before");
    let l = edge(id(4));
    let mut live = original.clone();
    live.name = "after".into();
    let forward = ChangeSet::new(
        envelope(1),
        vec![
            create_entity(&original),
            link(&l),
            rename(id(1), "after", "before"),
        ],
    );
    assert_inverse(
        &forward,
        &CurrentState {
            entities: BTreeMap::from([(id(1), live)]),
            edges: BTreeMap::from([(id(4), l.clone())]),
            ..Default::default()
        },
        vec![
            rename(id(1), "before", "after"),
            Op::Delete(DeleteOp {
                target_id: id(4),
                hard: false,
                preimage: DeletePreimage::Edge(Box::new(l)),
            }),
            Op::Delete(DeleteOp {
                target_id: id(1),
                hard: false,
                preimage: DeletePreimage::Entity(Box::new(original)),
            }),
        ],
    );
}

#[test]
fn repeated_updates_check_rewound_intermediate_state() {
    let forward = ChangeSet::new(
        envelope(1),
        vec![
            rename(id(1), "middle", "before"),
            rename(id(1), "after", "middle"),
        ],
    );
    assert_inverse(
        &forward,
        &CurrentState {
            entities: BTreeMap::from([(id(1), entity(id(1), "after"))]),
            ..Default::default()
        },
        vec![
            rename(id(1), "middle", "after"),
            rename(id(1), "before", "middle"),
        ],
    );
}

#[test]
fn update_only_double_inversion_is_byte_exact_across_multiple_substrates() {
    let entity_update = Op::Update(
        UpdateOp::new(
            id(1),
            UpdatePatch::Entity(EntityPatch {
                name: Some("middle".into()),
                description: Some(None),
                entity_type: Some(Some("after-type".into())),
                properties: Some(properties("after-map")),
                tags: Some(vec!["after-tag".into()]),
            }),
            UpdatePreimage::Entity(EntityPreimage {
                name: Some("before".into()),
                description: Some(Some("before-description".into())),
                entity_type: Some(None),
                properties: Some(properties("before-map")),
                tags: Some(vec!["before-tag".into()]),
            }),
        )
        .unwrap(),
    );
    let note_update = Op::Update(
        UpdateOp::new(
            id(3),
            UpdatePatch::Note(NotePatch {
                content: Some("after-note".into()),
                salience: Some(None),
                decay_factor: Some(Some(0.0)),
                properties: None,
                tags: None,
            }),
            UpdatePreimage::Note(NotePreimage {
                content: Some("before-note".into()),
                salience: Some(Some(0.5)),
                decay_factor: Some(Some(0.02)),
                properties: None,
                tags: None,
            }),
        )
        .unwrap(),
    );
    let edge_update = Op::Update(
        UpdateOp::new(
            id(4),
            UpdatePatch::Edge(EdgePatch {
                relation: None,
                weight: Some(-0.0),
            }),
            UpdatePreimage::Edge(EdgePreimage {
                relation: None,
                weight: Some(0.625),
            }),
        )
        .unwrap(),
    );
    let forward = ChangeSet::new(
        envelope(1),
        vec![
            entity_update,
            note_update,
            rename(id(1), "after", "middle"),
            edge_update,
        ],
    );
    let mut after_note = note(id(3), "after-note");
    after_note.salience = None;
    after_note.decay_factor = Some(0.0);
    let mut after_edge = edge(id(4));
    after_edge.weight = -0.0;
    let mut after_entity = entity(id(1), "after");
    after_entity.description = None;
    after_entity.entity_type = Some("after-type".into());
    after_entity.properties = properties("after-map");
    after_entity.tags = vec!["after-tag".into()];
    let mut after = CurrentState {
        entities: BTreeMap::from([(id(1), after_entity.clone())]),
        notes: BTreeMap::from([(id(3), after_note)]),
        edges: BTreeMap::from([(id(4), after_edge)]),
    };
    // Additional untouched rows arrive in a different insertion order; neither
    // that order nor their values may replace the forward operation order.
    after.entities.insert(id(9), entity(id(9), "unrelated"));
    let inverse = forward.invert(envelope(2), &after).unwrap();
    let mut before_note = note(id(3), "before-note");
    before_note.salience = Some(0.5);
    let mut before_entity = entity(id(1), "before");
    before_entity.description = Some("before-description".into());
    before_entity.entity_type = None;
    before_entity.properties = properties("before-map");
    before_entity.tags = vec!["before-tag".into()];
    let before = CurrentState {
        entities: BTreeMap::from([(id(9), entity(id(9), "unrelated")), (id(1), before_entity)]),
        notes: BTreeMap::from([(id(3), before_note)]),
        edges: BTreeMap::from([(id(4), edge(id(4)))]),
    };
    let roundtrip = inverse.invert(forward.envelope.clone(), &before).unwrap();
    assert_eq!(to_ndjson(&roundtrip).unwrap(), to_ndjson(&forward).unwrap());
    let mut reverse_insert = CurrentState::default();
    reverse_insert
        .entities
        .insert(id(9), entity(id(9), "unrelated"));
    reverse_insert.entities.insert(id(1), after_entity);
    reverse_insert.notes = after.notes.clone();
    reverse_insert.edges = after.edges.clone();
    assert_eq!(
        to_ndjson(&forward.invert(envelope(2), &reverse_insert).unwrap()).unwrap(),
        to_ndjson(&inverse).unwrap(),
    );
}

#[test]
fn inputs_remain_unchanged_after_success_and_failure_after_a_reverse_update() {
    let current = CurrentState {
        entities: BTreeMap::from([(id(1), entity(id(1), "after"))]),
        ..Default::default()
    };
    let good = ChangeSet::new(envelope(1), vec![rename(id(1), "after", "before")]);
    let good_before = to_ndjson(&good).unwrap();
    let current_before = snapshot(&current);
    good.invert(envelope(2), &current).unwrap();
    assert_eq!(to_ndjson(&good).unwrap(), good_before);
    assert_eq!(snapshot(&current), current_before);
    let bad = ChangeSet::new(
        envelope(1),
        vec![
            Op::Delete(DeleteOp {
                target_id: id(9),
                hard: true,
                preimage: DeletePreimage::Entity(Box::new(entity(id(9), "not invertible"))),
            }),
            rename(id(1), "after", "before"),
        ],
    );
    let bad_before = to_ndjson(&bad).unwrap();
    let error = bad.invert(envelope(2), &current).unwrap_err();
    assert!(matches!(
        error,
        InvertError::NotInvertible {
            index: 0,
            op: "delete"
        }
    ));
    assert_eq!(snapshot(&current), current_before);
    assert_eq!(to_ndjson(&bad).unwrap(), bad_before);
}

#[test]
fn delete_and_merge_refuse_with_original_forward_index_without_current_rows() {
    let deleted = Op::Delete(DeleteOp {
        target_id: id(1),
        hard: true,
        preimage: DeletePreimage::Entity(Box::new(entity(id(1), "deleted"))),
    });
    let merged = Op::Merge(MergeOp {
        into_id: id(1),
        from_id: id(2),
        preimage: MergePreimage {
            into: Box::new(entity(id(1), "into")),
            from: Box::new(entity(id(2), "from")),
            incident_edges: vec![],
        },
    });
    for (op, expected_name) in [(deleted, "delete"), (merged, "merge")] {
        let cs = ChangeSet::new(
            envelope(1),
            vec![create_entity(&entity(id(9), "earlier")), op],
        );
        match cs
            .invert(envelope(2), &CurrentState::default())
            .unwrap_err()
        {
            InvertError::NotInvertible { index, op } => assert_eq!((index, op), (1, expected_name)),
            other => panic!("expected refusal of operation1, got {other:?}"),
        }
    }
}

#[test]
fn missing_current_and_reused_envelope_have_distinct_typed_errors() {
    let e = entity(id(1), "created");
    for (op, required_id) in [
        (create_entity(&e), id(1)),
        (rename(id(1), "after", "before"), id(1)),
        (create_note(&note(id(3), "created note")), id(3)),
        (link(&edge(id(4))), id(4)),
    ] {
        let cs = ChangeSet::new(envelope(1), vec![op]);
        assert!(matches!(
            cs.invert(envelope(2), &CurrentState::default()).unwrap_err(),
            InvertError::MissingCurrent { index: 0, id: target } if target == required_id
        ));
        assert!(matches!(
            cs.invert(cs.envelope.clone(), &CurrentState::default())
                .unwrap_err(),
            InvertError::ReusedEnvelope
        ));
    }
    let empty = ChangeSet::new(envelope(1), vec![]);
    assert!(matches!(
        empty.invert(envelope(1), &CurrentState::default()),
        Err(InvertError::ReusedEnvelope)
    ));
    assert!(empty
        .invert(envelope(2), &CurrentState::default())
        .unwrap()
        .ops
        .is_empty());
}

#[test]
fn create_and_link_checks_refuse_later_writes_and_tombstones() {
    let e = entity(id(1), "created");
    let cs = ChangeSet::new(envelope(1), vec![create_entity(&e)]);
    let mut renamed = e.clone();
    renamed.name = "later rename".into();
    assert_invalidated(
        cs.invert(
            envelope(2),
            &CurrentState {
                entities: BTreeMap::from([(id(1), renamed)]),
                ..Default::default()
            },
        )
        .unwrap_err(),
        0,
        id(1),
        "name",
    );
    let mut dead = e.clone();
    dead.deleted_at = Some(Timestamp::from_secs(71));
    assert_invalidated(
        cs.invert(
            envelope(2),
            &CurrentState {
                entities: BTreeMap::from([(id(1), dead)]),
                ..Default::default()
            },
        )
        .unwrap_err(),
        0,
        id(1),
        "deleted_at",
    );
    let mut changed_map = e;
    changed_map
        .properties
        .insert("later".into(), PropertyValue::Boolean(true));
    assert_invalidated(
        cs.invert(
            envelope(2),
            &CurrentState {
                entities: BTreeMap::from([(id(1), changed_map)]),
                ..Default::default()
            },
        )
        .unwrap_err(),
        0,
        id(1),
        "properties",
    );
    let l = edge(id(4));
    let cs = ChangeSet::new(envelope(1), vec![link(&l)]);
    for (field, row) in [
        ("target", {
            let mut row = l.clone();
            row.target = id(99);
            row
        }),
        ("namespace", {
            let mut row = l.clone();
            row.namespace = "other".into();
            row
        }),
        ("deleted_at", {
            let mut row = l.clone();
            row.deleted_at = Some(Timestamp::from_secs(73));
            row
        }),
    ] {
        assert_invalidated(
            cs.invert(
                envelope(2),
                &CurrentState {
                    edges: BTreeMap::from([(id(4), row)]),
                    ..Default::default()
                },
            )
            .unwrap_err(),
            0,
            id(4),
            field,
        );
    }
}

#[test]
fn created_note_status_expiry_and_stamped_numeric_defaults_refuse() {
    let n = note(id(3), "created note");
    let cs = ChangeSet::new(envelope(1), vec![create_note(&n)]);
    for (field, row) in [
        ("status", {
            let mut row = n.clone();
            row.status = NoteStatus::Archived;
            row
        }),
        ("expires_at", {
            let mut row = n.clone();
            row.expires_at = Some(Timestamp::from_secs(83));
            row
        }),
    ] {
        assert_invalidated(
            cs.invert(
                envelope(2),
                &CurrentState {
                    notes: BTreeMap::from([(id(3), row)]),
                    ..Default::default()
                },
            )
            .unwrap_err(),
            0,
            id(3),
            field,
        );
    }
    let mut omitted = n.clone();
    omitted.salience = None;
    let cs = ChangeSet::new(envelope(1), vec![create_note(&omitted)]);
    assert_invalidated(
        cs.invert(
            envelope(2),
            &CurrentState {
                notes: BTreeMap::from([(id(3), n)]),
                ..Default::default()
            },
        )
        .unwrap_err(),
        0,
        id(3),
        "salience",
    );
}

#[test]
fn touched_update_and_incoherent_map_keys_refuse_without_comparing_unrelated_fields() {
    let cs = ChangeSet::new(envelope(1), vec![rename(id(1), "after", "before")]);
    assert_invalidated(
        cs.invert(
            envelope(2),
            &CurrentState {
                entities: BTreeMap::from([(id(1), entity(id(1), "intervening write"))]),
                ..Default::default()
            },
        )
        .unwrap_err(),
        0,
        id(1),
        "name",
    );
    assert_invalidated(
        cs.invert(
            envelope(2),
            &CurrentState {
                entities: BTreeMap::from([(id(1), entity(id(99), "after"))]),
                ..Default::default()
            },
        )
        .unwrap_err(),
        0,
        id(1),
        "id",
    );
    let n = note(id(3), "created");
    let mut wrong_note = n.clone();
    wrong_note.header.id = id(99);
    assert_invalidated(
        ChangeSet::new(envelope(1), vec![create_note(&n)])
            .invert(
                envelope(2),
                &CurrentState {
                    notes: BTreeMap::from([(id(3), wrong_note)]),
                    ..Default::default()
                },
            )
            .unwrap_err(),
        0,
        id(3),
        "id",
    );
    let l = edge(id(4));
    let mut wrong_edge = l.clone();
    wrong_edge.id = id(99);
    assert_invalidated(
        ChangeSet::new(envelope(1), vec![link(&l)])
            .invert(
                envelope(2),
                &CurrentState {
                    edges: BTreeMap::from([(id(4), wrong_edge)]),
                    ..Default::default()
                },
            )
            .unwrap_err(),
        0,
        id(4),
        "id",
    );
}

#[test]
fn swapped_note_numeric_patch_reaches_checked_constructor_error() {
    for (field, patch, prior, row) in [
        (
            "salience",
            NotePatch {
                content: None,
                salience: Some(Some(2.0)),
                decay_factor: None,
                properties: None,
                tags: None,
            },
            NotePreimage {
                content: None,
                salience: Some(Some(0.5)),
                decay_factor: None,
                properties: None,
                tags: None,
            },
            {
                let mut row = note(id(3), "unchanged");
                row.salience = Some(2.0);
                row
            },
        ),
        (
            "decay_factor",
            NotePatch {
                content: None,
                salience: None,
                decay_factor: Some(Some(-1.0)),
                properties: None,
                tags: None,
            },
            NotePreimage {
                content: None,
                salience: None,
                decay_factor: Some(Some(0.02)),
                properties: None,
                tags: None,
            },
            {
                let mut row = note(id(3), "unchanged");
                row.decay_factor = Some(-1.0);
                row
            },
        ),
    ] {
        let update =
            UpdateOp::new(id(3), UpdatePatch::Note(patch), UpdatePreimage::Note(prior)).unwrap();
        let cs = ChangeSet::new(envelope(1), vec![Op::Update(update)]);
        // Record fields are public: this is a reachable constructor refusal,
        // rather than a fabricated preimage-free UpdateOp.
        match cs
            .invert(
                envelope(2),
                &CurrentState {
                    notes: BTreeMap::from([(id(3), row)]),
                    ..Default::default()
                },
            )
            .unwrap_err()
        {
            InvertError::InverseUpdate { index, reason } => {
                assert_eq!(index, 0);
                assert!(reason.contains("preimage"));
                assert!(reason.contains(field));
            }
            other => panic!("expected InverseUpdate for {field}, got {other:?}"),
        }
    }
}

#[test]
fn absent_preimage_is_a_decode_error_and_retains_ndjson_line_context() {
    // Decode-side sibling of op.rs::merge_op_requires_preimage. This tests an
    // ABSENT key; update_op_rejects_preimage_missing_touched_field tests an empty one.
    let absent = json!({
        "target_id": id(1),
        "patch": {"target": "entity", "name": "after"},
    });
    let err = serde_json::from_value::<UpdateOp>(absent.clone()).unwrap_err();
    assert!(err.to_string().contains("preimage"));
    let mut wire = absent;
    wire["op"] = json!("update");
    let text = format!(
        "{}\n{}\n",
        serde_json::to_string(&envelope(1)).unwrap(),
        wire
    );
    match from_ndjson(&text).unwrap_err() {
        ChangeSetError::MalformedLine { line, source } => {
            assert_eq!(line, 2);
            assert!(source.to_string().contains("preimage"));
        }
        other => panic!("expected second-line preimage decode error, got {other:?}"),
    }
    let incongruent = json!({
        "target_id": id(1),
        "patch": {"target": "entity", "name": "after"},
        "preimage": {"target": "entity"},
    });
    assert!(serde_json::from_value::<UpdateOp>(incongruent).is_err());
}

#[test]
fn soft_deletes_restore_full_records_and_reinvert_without_content_loss() {
    let e = entity(id(1), "original");
    let mut n = note(id(2), "original note");
    n.expires_at = Some(Timestamp::from_secs(100));
    let l = edge(id(3));
    let records = vec![
        DeletePreimage::Entity(Box::new(e.clone())),
        DeletePreimage::Note(Box::new(n.clone())),
        DeletePreimage::Edge(Box::new(l.clone())),
    ];
    let forward = ChangeSet::new(
        envelope(1),
        records
            .iter()
            .enumerate()
            .map(|(i, record)| {
                Op::Delete(DeleteOp {
                    target_id: id(i as u128 + 1),
                    hard: false,
                    preimage: record.clone(),
                })
            })
            .collect(),
    );
    let mut deleted = CurrentState {
        entities: BTreeMap::from([(id(1), e.clone())]),
        notes: BTreeMap::from([(id(2), n.clone())]),
        edges: BTreeMap::from([(id(3), l.clone())]),
    };
    deleted.entities.get_mut(&id(1)).unwrap().deleted_at = Some(Timestamp::from_secs(80));
    let dead_note = deleted.notes.get_mut(&id(2)).unwrap();
    dead_note.deleted_at = Some(Timestamp::from_secs(80));
    dead_note.status = NoteStatus::Deleted;
    let dead_edge = deleted.edges.get_mut(&id(3)).unwrap();
    dead_edge.deleted_at = Some(Timestamp::from_secs(80));
    dead_edge.updated_at = Timestamp::from_secs(80);
    let before = snapshot(&deleted);
    let inverse = assert_inverse(
        &forward,
        &deleted,
        records
            .iter()
            .enumerate()
            .rev()
            .map(|(i, record)| {
                Op::Restore(RestoreOp::new(id(i as u128 + 1), record.clone()).unwrap())
            })
            .collect(),
    );
    assert_eq!(snapshot(&deleted), before);
    let mut restored = CurrentState {
        entities: BTreeMap::from([(id(1), e)]),
        notes: BTreeMap::from([(id(2), n)]),
        edges: BTreeMap::from([(id(3), l)]),
    };
    restored.entities.get_mut(&id(1)).unwrap().header.updated_at = Timestamp::from_secs(90);
    restored.notes.get_mut(&id(2)).unwrap().header.updated_at = Timestamp::from_secs(90);
    restored.edges.get_mut(&id(3)).unwrap().updated_at = Timestamp::from_secs(90);
    let reinverted = inverse.invert(envelope(3), &restored).unwrap();
    let expected = vec![
        DeletePreimage::Entity(Box::new(restored.entities[&id(1)].clone())),
        DeletePreimage::Note(Box::new(restored.notes[&id(2)].clone())),
        DeletePreimage::Edge(Box::new(restored.edges[&id(3)].clone())),
    ];
    for (op, record) in reinverted.ops.iter().zip(expected) {
        let Op::Delete(delete) = op else {
            panic!("restore must invert to delete")
        };
        assert!(!delete.hard);
        assert_eq!(
            serde_json::to_value(&delete.preimage).unwrap(),
            serde_json::to_value(record).unwrap()
        );
    }
    // Lifecycle timestamps change on live restoration; content and all other fields do not.
    for (op, original) in reinverted.ops.iter().zip(records) {
        let Op::Delete(delete) = op else {
            unreachable!()
        };
        let mut actual = serde_json::to_value(&delete.preimage).unwrap();
        let original = serde_json::to_value(original).unwrap();
        actual["updated_at"] = original["updated_at"].clone();
        assert_eq!(actual, original);
    }
}

#[test]
fn mixed_delete_restore_history_rewinds_liveness_without_an_invented_timestamp() {
    let original = entity(id(1), "before");
    let restored_record = DeletePreimage::Entity(Box::new(entity(id(1), "after")));
    let forward = ChangeSet::new(
        envelope(1),
        vec![
            rename(id(1), "after", "before"),
            Op::Delete(DeleteOp {
                target_id: id(1),
                hard: false,
                preimage: restored_record.clone(),
            }),
            Op::Restore(RestoreOp::new(id(1), restored_record.clone()).unwrap()),
        ],
    );
    let mut live = entity(id(1), "after");
    live.header.updated_at = Timestamp::from_secs(90);
    let current = CurrentState {
        entities: BTreeMap::from([(id(1), live.clone())]),
        ..Default::default()
    };
    assert_inverse(
        &forward,
        &current,
        vec![
            Op::Delete(DeleteOp {
                target_id: id(1),
                hard: false,
                preimage: DeletePreimage::Entity(Box::new(live)),
            }),
            Op::Restore(RestoreOp::new(id(1), restored_record).unwrap()),
            rename(original.header.id, "before", "after"),
        ],
    );
}

#[test]
fn restore_and_soft_delete_refuse_changed_content_or_wrong_lifecycle() {
    let original = entity(id(1), "original");
    let restore = Op::Restore(
        RestoreOp::new(id(1), DeletePreimage::Entity(Box::new(original.clone()))).unwrap(),
    );
    let delete = Op::Delete(DeleteOp {
        target_id: id(1),
        hard: false,
        preimage: DeletePreimage::Entity(Box::new(original.clone())),
    });
    for (op, dead) in [(restore.clone(), false), (delete.clone(), true)] {
        let mut changed = original.clone();
        changed.name = "intervening edit".into();
        changed.deleted_at = dead.then(|| Timestamp::from_secs(80));
        let current = CurrentState {
            entities: BTreeMap::from([(id(1), changed)]),
            ..Default::default()
        };
        let before = snapshot(&current);
        assert_invalidated(
            ChangeSet::new(envelope(1), vec![op])
                .invert(envelope(2), &current)
                .unwrap_err(),
            0,
            id(1),
            "name",
        );
        assert_eq!(snapshot(&current), before);
    }
    let live = CurrentState {
        entities: BTreeMap::from([(id(1), original.clone())]),
        ..Default::default()
    };
    assert_invalidated(
        ChangeSet::new(envelope(1), vec![delete])
            .invert(envelope(2), &live)
            .unwrap_err(),
        0,
        id(1),
        "deleted_at",
    );
    let mut dead = original;
    dead.deleted_at = Some(Timestamp::from_secs(80));
    let tombstoned = CurrentState {
        entities: BTreeMap::from([(id(1), dead)]),
        ..Default::default()
    };
    assert_invalidated(
        ChangeSet::new(envelope(1), vec![restore])
            .invert(envelope(2), &tombstoned)
            .unwrap_err(),
        0,
        id(1),
        "deleted_at",
    );
}

#[test]
fn soft_delete_refuses_changed_entity_timestamp_and_unrestorable_note_status() {
    let original = entity(id(1), "original");
    let mut dead = original.clone();
    dead.deleted_at = Some(Timestamp::from_secs(80));
    dead.header.updated_at = Timestamp::from_secs(80);
    let current = CurrentState {
        entities: BTreeMap::from([(id(1), dead)]),
        ..Default::default()
    };
    let forward = ChangeSet::new(
        envelope(1),
        vec![Op::Delete(DeleteOp {
            target_id: id(1),
            hard: false,
            preimage: DeletePreimage::Entity(Box::new(original)),
        })],
    );
    assert_invalidated(
        forward.invert(envelope(2), &current).unwrap_err(),
        0,
        id(1),
        "updated_at",
    );
    let mut archived = note(id(2), "archived");
    archived.status = NoteStatus::Archived;
    let forward = ChangeSet::new(
        envelope(1),
        vec![Op::Delete(DeleteOp {
            target_id: id(2),
            hard: false,
            preimage: DeletePreimage::Note(Box::new(archived)),
        })],
    );
    assert!(
        matches!(forward.invert(envelope(2), &CurrentState::default()), Err(InvertError::InverseRestore { index: 0, reason }) if reason.contains("must be active"))
    );
}

#[test]
fn note_and_edge_content_changes_and_unknown_current_rows_refuse_restore() {
    let n = note(id(2), "original");
    let l = edge(id(3));
    for (record, mut current, identifier, field) in [
        (
            DeletePreimage::Note(Box::new(n.clone())),
            CurrentState {
                notes: BTreeMap::from([(id(2), n)]),
                ..Default::default()
            },
            id(2),
            "expires_at",
        ),
        (
            DeletePreimage::Edge(Box::new(l.clone())),
            CurrentState {
                edges: BTreeMap::from([(id(3), l)]),
                ..Default::default()
            },
            id(3),
            "weight",
        ),
    ] {
        let forward = ChangeSet::new(
            envelope(1),
            vec![Op::Restore(RestoreOp::new(identifier, record).unwrap())],
        );
        assert!(
            matches!(forward.invert(envelope(2), &CurrentState::default()), Err(InvertError::MissingCurrent { index: 0, id }) if id == identifier)
        );
        if let Some(row) = current.notes.get_mut(&identifier) {
            row.expires_at = Some(Timestamp::from_secs(100));
        }
        if let Some(row) = current.edges.get_mut(&identifier) {
            row.weight = 0.5;
        }
        assert_invalidated(
            forward.invert(envelope(2), &current).unwrap_err(),
            0,
            identifier,
            field,
        );
    }
}

#[test]
fn reverse_restore_does_not_allow_an_earlier_create_of_a_tombstoned_record() {
    let row = entity(id(1), "original");
    let forward = ChangeSet::new(
        envelope(1),
        vec![
            create_entity(&row),
            Op::Restore(
                RestoreOp::new(id(1), DeletePreimage::Entity(Box::new(row.clone()))).unwrap(),
            ),
        ],
    );
    let current = CurrentState {
        entities: BTreeMap::from([(id(1), row)]),
        ..Default::default()
    };
    assert_invalidated(
        forward.invert(envelope(2), &current).unwrap_err(),
        0,
        id(1),
        "deleted_at",
    );
}
