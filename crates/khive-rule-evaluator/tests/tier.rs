use std::collections::BTreeMap;
use std::error::Error;

use khive_changeset::{ChangeSet, DeleteOp, DeletePreimage, Envelope, MergeOp, MergePreimage, Op};
use khive_rule_evaluator::{
    classify_changeset, Rules, RulesError, Tier, TierDecision, TierFinding,
};
use khive_types::{
    Entity, EntityKind, Header, Id128, Link, Namespace, Note, NoteStatus, Timestamp,
};
use serde_json::{json, Value};

const A: &str = "10000000-0000-4000-8000-000000000001";
const B: &str = "10000000-0000-4000-8000-000000000002";
const C: &str = "10000000-0000-4000-8000-000000000003";
const EDGE: &str = "10000000-0000-4000-8000-000000000004";

fn parse(value: Value) -> Op {
    serde_json::from_value(value).unwrap()
}

fn create(id: &str, note: bool) -> Op {
    let target = if note {
        json!({"kind":"note", "note_kind":"observation", "content":"Recorded note"})
    } else {
        json!({"kind":"entity", "entity_kind":"concept", "name":"Recorded entity"})
    };
    parse(json!({"op":"create", "id":id, "namespace":"local", "target":target}))
}

fn link(relation: &str, weight: f64) -> Op {
    parse(
        json!({"op":"link", "id":EDGE, "namespace":"local", "source":A,
        "target":B, "relation":relation, "weight":weight}),
    )
}

fn update(patch: Value, preimage: Value) -> Op {
    parse(json!({"op":"update", "target_id":A, "patch":patch, "preimage":preimage}))
}

fn entity(id: &str) -> Entity {
    Entity {
        header: Header::new(
            id.parse().unwrap(),
            Namespace::local(),
            Timestamp::from_secs(1),
        ),
        kind: EntityKind::Concept,
        entity_type: None,
        name: "Recorded entity".into(),
        description: None,
        properties: BTreeMap::new(),
        tags: Vec::new(),
        deleted_at: None,
    }
}

fn delete(substrate: &str) -> Op {
    let id: Id128 = A.parse().unwrap();
    let preimage = match substrate {
        "entity" => DeletePreimage::Entity(Box::new(entity(A))),
        "note" => DeletePreimage::Note(Box::new(Note {
            header: Header::new(id, Namespace::local(), Timestamp::from_secs(1)),
            kind: "observation".into(),
            status: NoteStatus::Active,
            content: "Recorded note".into(),
            properties: BTreeMap::new(),
            tags: Vec::new(),
            salience: None,
            decay_factor: None,
            expires_at: None,
            deleted_at: None,
        })),
        "edge" => DeletePreimage::Edge(Box::new(Link {
            id,
            namespace: "local".into(),
            source: B.parse().unwrap(),
            target: C.parse().unwrap(),
            relation: "contains".parse().unwrap(),
            weight: 0.9,
            properties: BTreeMap::new(),
            created_at: Timestamp::from_secs(1),
            updated_at: Timestamp::from_secs(1),
            deleted_at: None,
        })),
        _ => panic!("invalid fixture substrate"),
    };
    Op::Delete(DeleteOp {
        target_id: id,
        hard: false,
        preimage,
    })
}

fn merge() -> Op {
    Op::Merge(MergeOp {
        into_id: A.parse().unwrap(),
        from_id: B.parse().unwrap(),
        preimage: MergePreimage {
            into: Box::new(entity(A)),
            from: Box::new(entity(B)),
            incident_edges: Vec::new(),
        },
    })
}

fn classify(config: &str, ops: Vec<Op>, findings: &[TierFinding<'_>]) -> Vec<TierDecision> {
    let changeset = ChangeSet::new(
        Envelope::new("agent:producer", "family:producer", Timestamp::from_secs(1)),
        ops,
    );
    // Exercise the existing strict codec as well as the typed classifier.
    let changeset =
        khive_changeset::from_ndjson(&khive_changeset::to_ndjson(&changeset).unwrap()).unwrap();
    classify_changeset(&Rules::parse_toml(config).unwrap(), &changeset, findings)
}

fn expected(tier: Tier, ids: &[&str], reason: &str) -> TierDecision {
    TierDecision {
        tier,
        reason: reason.into(),
        reason_ids: ids.iter().map(|id| (*id).into()).collect(),
    }
}

include!("tier_tests/policy_default_tests.rs");

include!("tier_tests/operation_floor_tests.rs");

include!("tier_tests/findings_and_errors_tests.rs");

include!("tier_tests/restore_tests.rs");
