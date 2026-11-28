//! Wire stability: a change set written before a field was added keeps its
//! exact NDJSON bytes, and those bytes still parse.

use std::collections::BTreeMap;

use khive_changeset::{
    ChangeSet, CreateOp, CreateTarget, DeleteOp, DeletePreimage, EntityCreateFields, EntityPatch,
    EntityPreimage, EntityRecord, Envelope, MergeOp, MergePreimage, Op, UpdateOp, UpdatePatch,
    UpdatePreimage,
};

fn boxed(row: impl Into<EntityRecord>) -> Box<EntityRecord> {
    Box::new(row.into())
}
use khive_types::{Entity, EntityKind, Header, Id128, Link, Namespace, PropertyValue, Timestamp};

fn entity(id: u128, kind: EntityKind, name: &str) -> Entity {
    Entity {
        header: Header::new(
            Id128::from_u128(id),
            Namespace::local(),
            Timestamp::from_secs(1_700_000_000),
        ),
        kind,
        entity_type: Some("library".into()),
        name: name.into(),
        description: Some("a description".into()),
        properties: BTreeMap::from([("lang".to_string(), PropertyValue::String("rust".into()))]),
        tags: vec!["t1".into()],
        deleted_at: None,
    }
}

fn fixture() -> ChangeSet {
    let envelope = Envelope::new(
        "agent:golden",
        "family:golden",
        Timestamp::from_secs(1_700_000_100),
    )
    .with_batch_id("batch-golden");
    let ts = Timestamp::from_secs(1_700_000_000);
    let ops = vec![
        Op::Create(CreateOp {
            id: Id128::from_u128(0x11),
            namespace: Namespace::local(),
            target: CreateTarget::Entity(EntityCreateFields {
                entity_kind: EntityKind::Concept,
                entity_type: Some("technique".into()),
                name: "LoRA".into(),
                description: Some("low-rank adaptation".into()),
                properties: BTreeMap::from([("year".to_string(), PropertyValue::Integer(2021))]),
                tags: vec!["training".into()],
                key: None,
                aliases: vec![],
            }),
        }),
        Op::Update(
            UpdateOp::new(
                Id128::from_u128(0x22),
                UpdatePatch::Entity(EntityPatch {
                    name: Some("new-name".into()),
                    description: Some(None),
                    properties: None,
                    tags: Some(vec!["a".into(), "b".into()]),
                    entity_type: Some(Some("tool".into())),
                    aliases: None,
                }),
                UpdatePreimage::Entity(EntityPreimage {
                    name: Some("old-name".into()),
                    description: Some(Some("old description".into())),
                    properties: None,
                    tags: Some(vec![]),
                    entity_type: Some(None),
                    aliases: None,
                }),
            )
            .unwrap(),
        ),
        Op::Delete(DeleteOp {
            target_id: Id128::from_u128(0x33),
            hard: false,
            preimage: DeletePreimage::Entity(boxed(entity(0x33, EntityKind::Project, "gone"))),
        }),
        Op::Merge(MergeOp {
            into_id: Id128::from_u128(0x44),
            from_id: Id128::from_u128(0x55),
            preimage: MergePreimage {
                into: boxed(entity(0x44, EntityKind::Artifact, "kept")),
                from: boxed(entity(0x55, EntityKind::Artifact, "absorbed")),
                incident_edges: vec![Link {
                    id: Id128::from_u128(0x66),
                    namespace: "local".into(),
                    source: Id128::from_u128(0x55),
                    target: Id128::from_u128(0x77),
                    relation: khive_types::EdgeRelation::DependsOn,
                    properties: BTreeMap::new(),
                    weight: 0.75,
                    created_at: ts,
                    updated_at: ts,
                    deleted_at: None,
                }],
            },
        }),
    ];
    ChangeSet::new(envelope, ops)
}

/// NDJSON bytes of [`fixture`], computed with the codec at commit
/// a75deb16ad928d74a29f3a1fde645d593715e2f5, before entity keys and aliases
/// reached change sets. A field added since then must be omitted when empty,
/// so these bytes are what the codec still emits and still accepts.
const GOLDEN: &str = concat!(
    r#"{"schema_version":2,"producer":"agent:golden","producer_model_family":"family:golden","staged_at":1700000100000000,"batch_id":"batch-golden"}"#,
    "\n",
    r#"{"op":"create","id":"00000000-0000-0000-0000-000000000011","namespace":"local","target":{"kind":"entity","entity_kind":"concept","entity_type":"technique","name":"LoRA","description":"low-rank adaptation","properties":{"year":2021},"tags":["training"]}}"#,
    "\n",
    r#"{"op":"update","target_id":"00000000-0000-0000-0000-000000000022","patch":{"target":"entity","name":"new-name","description":null,"tags":["a","b"],"entity_type":"tool"},"preimage":{"target":"entity","name":"old-name","description":"old description","tags":[],"entity_type":null}}"#,
    "\n",
    r#"{"op":"delete","target_id":"00000000-0000-0000-0000-000000000033","hard":false,"preimage":{"substrate":"entity","id":"00000000-0000-0000-0000-000000000033","namespace":"local","created_at":1700000000000000,"updated_at":1700000000000000,"kind":"project","entity_type":"library","name":"gone","description":"a description","properties":{"lang":"rust"},"tags":["t1"],"deleted_at":null}}"#,
    "\n",
    r#"{"op":"merge","into_id":"00000000-0000-0000-0000-000000000044","from_id":"00000000-0000-0000-0000-000000000055","preimage":{"into":{"id":"00000000-0000-0000-0000-000000000044","namespace":"local","created_at":1700000000000000,"updated_at":1700000000000000,"kind":"artifact","entity_type":"library","name":"kept","description":"a description","properties":{"lang":"rust"},"tags":["t1"],"deleted_at":null},"from":{"id":"00000000-0000-0000-0000-000000000055","namespace":"local","created_at":1700000000000000,"updated_at":1700000000000000,"kind":"artifact","entity_type":"library","name":"absorbed","description":"a description","properties":{"lang":"rust"},"tags":["t1"],"deleted_at":null},"incident_edges":[{"id":"00000000-0000-0000-0000-000000000066","namespace":"local","source":"00000000-0000-0000-0000-000000000055","target":"00000000-0000-0000-0000-000000000077","relation":"depends_on","properties":{},"weight":0.75,"created_at":1700000000000000,"updated_at":1700000000000000,"deleted_at":null}]}}"#,
    "\n",
);

#[test]
fn a_change_set_without_keys_or_aliases_serializes_to_the_earlier_bytes() {
    let text = khive_changeset::to_ndjson(&fixture()).expect("serialize");
    assert_eq!(text, GOLDEN);
}

#[test]
fn the_earlier_bytes_still_parse_and_reserialize_unchanged() {
    let decoded = khive_changeset::from_ndjson(GOLDEN).expect("earlier bytes must parse");
    assert_eq!(decoded.ops.len(), 4);
    let text = khive_changeset::to_ndjson(&decoded).expect("re-serialize");
    assert_eq!(text, GOLDEN);
}
