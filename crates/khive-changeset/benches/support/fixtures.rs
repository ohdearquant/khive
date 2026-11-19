use std::collections::BTreeMap;

use khive_changeset::{
    from_ndjson, to_ndjson, ChangeSet, CreateOp, CreateTarget, CurrentState, DeleteOp,
    DeletePreimage, EntityCreateFields, EntityPatch, EntityPreimage, Envelope, Op, UpdateOp,
    UpdatePatch, UpdatePreimage,
};
use khive_types::{Entity, EntityKind, Header, Id128, Namespace, PropertyValue, Timestamp};

#[derive(Clone, Copy)]
pub enum Shape {
    Create,
    Update,
}

pub struct Fixture {
    pub forward: ChangeSet,
    pub current: CurrentState,
    pub inverse_envelope: Envelope,
}

fn update(id: Id128, after: String, before: String) -> Op {
    Op::Update(
        UpdateOp::new(
            id,
            UpdatePatch::Entity(EntityPatch {
                name: Some(after),
                description: None,
                entity_type: None,
                properties: None,
                tags: None,
            }),
            UpdatePreimage::Entity(EntityPreimage {
                name: Some(before),
                description: None,
                entity_type: None,
                properties: None,
                tags: None,
            }),
        )
        .expect("congruent name patch/preimage"),
    )
}

impl Fixture {
    pub fn new(count: usize, shape: Shape) -> Self {
        let shape_name = match shape {
            Shape::Create => "entity_create",
            Shape::Update => "entity_update",
        };
        let mut current = CurrentState::default();
        let mut ops = Vec::with_capacity(count);
        let mut inverse_ops = Vec::with_capacity(count);
        for index in 0..count {
            let id = Id128::from_u128((5_666_u128 << 64) | (index as u128 + 1));
            let row = Entity {
                header: Header {
                    id,
                    namespace: Namespace::local(),
                    created_at: Timestamp::from_secs(17),
                    updated_at: Timestamp::from_secs(29),
                },
                kind: EntityKind::Concept,
                entity_type: None,
                name: format!("after_{index}"),
                description: Some("fixed description with newline\nand Unicode λ".into()),
                properties: BTreeMap::from([
                    (
                        "label".into(),
                        PropertyValue::String("codec fixture".into()),
                    ),
                    ("index".into(), PropertyValue::Integer(index as i64)),
                ]),
                tags: vec!["benchmark".into(), "codec".into()],
                deleted_at: None,
            };
            match shape {
                Shape::Create => {
                    ops.push(Op::Create(CreateOp {
                        id,
                        namespace: row.header.namespace.clone(),
                        target: CreateTarget::Entity(EntityCreateFields {
                            entity_kind: row.kind,
                            entity_type: row.entity_type.clone(),
                            name: row.name.clone(),
                            description: row.description.clone(),
                            properties: row.properties.clone(),
                            tags: row.tags.clone(),
                        }),
                    }));
                    inverse_ops.push(Op::Delete(DeleteOp {
                        target_id: id,
                        hard: false,
                        preimage: DeletePreimage::Entity(Box::new(row.clone())),
                    }));
                }
                Shape::Update => {
                    let before = format!("before_{index}");
                    ops.push(update(id, row.name.clone(), before.clone()));
                    inverse_ops.push(update(id, before, row.name.clone()));
                }
            }
            assert!(current.entities.insert(id, row).is_none());
        }
        let forward = ChangeSet::new(
            Envelope::new("benchmark", "codec", Timestamp::from_secs(1)),
            ops,
        );
        let inverse_envelope = Envelope::new("benchmark", "codec", Timestamp::from_secs(2));
        inverse_ops.reverse();
        let expected_inverse = ChangeSet::new(inverse_envelope.clone(), inverse_ops);
        let fixture = Self {
            forward,
            current,
            inverse_envelope,
        };
        let encoded = to_ndjson(&fixture.forward).expect("encode fixture");
        assert_eq!(encoded.lines().count(), count + 1);
        assert!(encoded.ends_with('\n'));
        let decoded = from_ndjson(&encoded).expect("decode fixture");
        assert_eq!(decoded.envelope, fixture.forward.envelope);
        assert_eq!(decoded.ops.len(), count);
        assert_eq!(to_ndjson(&decoded).unwrap(), encoded);
        assert_eq!(fixture.current.entities.len(), count);
        assert!(fixture.current.notes.is_empty() && fixture.current.edges.is_empty());
        let records_before =
            serde_json::to_vec(&fixture.current.entities.values().collect::<Vec<_>>()).unwrap();
        let inverse = fixture
            .forward
            .invert(fixture.inverse_envelope.clone(), &fixture.current)
            .expect("invert matching current records");
        assert_eq!(inverse.ops.len(), count);
        assert_eq!(
            to_ndjson(&inverse).unwrap(),
            to_ndjson(&expected_inverse).unwrap()
        );
        assert_eq!(to_ndjson(&fixture.forward).unwrap(), encoded);
        assert_eq!(
            serde_json::to_vec(&fixture.current.entities.values().collect::<Vec<_>>()).unwrap(),
            records_before
        );
        println!(
            "changeset_payload_report shape={shape_name} ops={count} total_utf8_bytes={} bytes_per_op={}",
            encoded.len(),
            encoded.len() as f64 / count as f64
        );
        fixture
    }
}
