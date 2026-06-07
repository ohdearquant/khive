// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Canonical JSON serialization and SHA-256 snapshot hashing.
//!
//! Algorithm:
//! 1. Collect non-soft-deleted entities; sort by UUID string ascending.
//! 2. Collect edges; sort by (source, target, relation) ascending.
//! 3. Serialize as `{"edges":[...],"entities":[...]}` with fixed field order and no whitespace.
//! 4. SHA-256 the UTF-8 bytes; prefix with `"sha256:"`.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use khive_runtime::portability::{ExportedEdge, ExportedEntity, KgArchive};

use crate::error::VcsError;
use crate::types::SnapshotId;

/// Compute the content-addressed `SnapshotId` for a `KgArchive`.
///
/// The archive is assumed to already contain only non-deleted entities in the
/// caller's intended order. This function performs the sort and serializes
/// deterministically before hashing.
pub fn snapshot_id_for_archive(archive: &KgArchive) -> Result<SnapshotId, VcsError> {
    let canonical = canonical_json(archive)?;
    let digest = Sha256::digest(canonical.as_bytes());
    let hex = hex::encode(digest);
    SnapshotId::from_hash(&hex)
}

/// Produce the canonical JSON bytes for a `KgArchive`.
///
/// Entities are sorted by UUID (case-insensitive string comparison).
/// Edges are sorted by (source, target, relation).
/// Properties keys within each entity are sorted alphabetically.
/// Tags within each entity are sorted lexicographically.
/// No whitespace in the output.
pub fn canonical_json(archive: &KgArchive) -> Result<String, VcsError> {
    let mut entities = archive.entities.clone();
    entities.sort_by(|a, b| {
        a.id.to_string()
            .to_ascii_lowercase()
            .cmp(&b.id.to_string().to_ascii_lowercase())
    });

    let mut edges = archive.edges.clone();
    edges.sort_by(|a, b| {
        let ak = (
            a.source.to_string(),
            a.target.to_string(),
            a.relation.to_string(),
        );
        let bk = (
            b.source.to_string(),
            b.target.to_string(),
            b.relation.to_string(),
        );
        ak.cmp(&bk)
    });

    let entity_values: Vec<Value> = entities.iter().map(entity_to_canonical_value).collect();
    let edge_values: Vec<Value> = edges
        .iter()
        .map(edge_to_canonical_value)
        .collect::<Result<Vec<_>, _>>()?;

    let mut root = Map::new();
    root.insert("entities".to_string(), Value::Array(entity_values));
    root.insert("edges".to_string(), Value::Array(edge_values));

    serde_json::to_string(&Value::Object(root)).map_err(VcsError::Json)
}

/// Serialize a single entity with fixed key order and sorted sub-fields.
///
/// `entity_type` is included in the canonical representation so that two
/// snapshots differing only in `entity_type` produce different `SnapshotId`s.
fn entity_to_canonical_value(e: &ExportedEntity) -> Value {
    let properties = sort_properties_value(e.properties.clone());
    let mut tags = e.tags.clone();
    tags.sort();

    let mut obj = Map::new();
    obj.insert("id".to_string(), Value::String(e.id.to_string()));
    obj.insert("kind".to_string(), Value::String(e.kind.clone()));
    obj.insert(
        "entity_type".to_string(),
        e.entity_type
            .as_ref()
            .map_or(Value::Null, |t| Value::String(t.clone())),
    );
    obj.insert("name".to_string(), Value::String(e.name.clone()));
    obj.insert(
        "description".to_string(),
        e.description
            .as_ref()
            .map_or(Value::Null, |d| Value::String(d.clone())),
    );
    obj.insert("properties".to_string(), properties.unwrap_or(Value::Null));
    obj.insert(
        "tags".to_string(),
        Value::Array(tags.into_iter().map(Value::String).collect()),
    );
    Value::Object(obj)
}

/// Serialize a single edge with fixed key order.
fn edge_to_canonical_value(e: &ExportedEdge) -> Result<Value, VcsError> {
    let mut obj = Map::new();
    obj.insert("edge_id".to_string(), Value::String(e.edge_id.to_string()));
    obj.insert("source".to_string(), Value::String(e.source.to_string()));
    obj.insert("target".to_string(), Value::String(e.target.to_string()));
    obj.insert(
        "relation".to_string(),
        Value::String(e.relation.to_string()),
    );
    let weight_num = serde_json::Number::from_f64(e.weight).ok_or_else(|| {
        VcsError::Internal(format!(
            "edge weight is not finite (NaN or Infinity): {}",
            e.weight
        ))
    })?;
    obj.insert("weight".to_string(), Value::Number(weight_num));
    Ok(Value::Object(obj))
}

/// Recursively sort the keys of a JSON value so the hash is key-order-independent.
fn sort_value_recursive(val: Value) -> Value {
    match val {
        Value::Object(map) => {
            let mut pairs: Vec<(String, Value)> = map.into_iter().collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            let sorted: Map<String, Value> = pairs
                .into_iter()
                .map(|(k, v)| (k, sort_value_recursive(v)))
                .collect();
            Value::Object(sorted)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(sort_value_recursive).collect()),
        other => other,
    }
}

fn sort_properties_value(props: Option<Value>) -> Option<Value> {
    props.map(sort_value_recursive)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use khive_runtime::portability::{ExportedEdge, ExportedEntity, KgArchive};
    use khive_storage::EdgeRelation;
    use uuid::Uuid;

    use super::*;

    fn empty_archive() -> KgArchive {
        KgArchive {
            format: "khive-kg".into(),
            version: "0.1".into(),
            namespace: "test".into(),
            exported_at: Utc::now(),
            entities: vec![],
            edges: vec![],
        }
    }

    fn make_entity(id: Uuid, name: &str) -> ExportedEntity {
        ExportedEntity {
            id,
            kind: "concept".into(),
            entity_type: None,
            name: name.into(),
            description: None,
            properties: None,
            tags: vec![],
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn empty_archive_has_stable_hash() {
        let a1 = empty_archive();
        let a2 = empty_archive();
        // Two empty archives produce the same hash regardless of `exported_at`.
        assert_eq!(
            snapshot_id_for_archive(&a1).unwrap(),
            snapshot_id_for_archive(&a2).unwrap()
        );
    }

    #[test]
    fn hash_changes_with_entity_addition() {
        let mut archive = empty_archive();
        let id1 = snapshot_id_for_archive(&archive).unwrap();
        archive
            .entities
            .push(make_entity(Uuid::new_v4(), "FlashAttention"));
        let id2 = snapshot_id_for_archive(&archive).unwrap();
        assert_ne!(id1, id2);
    }

    #[test]
    fn entity_order_independent_hash() {
        let e1 = make_entity(
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            "Alpha",
        );
        let e2 = make_entity(
            Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
            "Beta",
        );

        let mut a1 = empty_archive();
        a1.entities = vec![e1.clone(), e2.clone()];
        let mut a2 = empty_archive();
        a2.entities = vec![e2, e1]; // reversed insertion order

        // Sort-by-UUID makes both hashes identical.
        assert_eq!(
            snapshot_id_for_archive(&a1).unwrap(),
            snapshot_id_for_archive(&a2).unwrap()
        );
    }

    #[test]
    fn snapshot_id_has_sha256_prefix() {
        let id = snapshot_id_for_archive(&empty_archive()).unwrap();
        assert!(id.as_str().starts_with("sha256:"));
        assert_eq!(id.hex().len(), 64);
    }

    #[test]
    fn edge_included_in_hash() {
        let uid1 = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let uid2 = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        let e1 = make_entity(uid1, "A");
        let e2 = make_entity(uid2, "B");

        let mut without_edge = empty_archive();
        without_edge.entities = vec![e1.clone(), e2.clone()];

        let mut with_edge = empty_archive();
        with_edge.entities = vec![e1, e2];
        with_edge.edges = vec![ExportedEdge {
            edge_id: Uuid::new_v4(),
            source: uid1,
            target: uid2,
            relation: EdgeRelation::Extends,
            weight: 1.0,
        }];

        assert_ne!(
            snapshot_id_for_archive(&without_edge).unwrap(),
            snapshot_id_for_archive(&with_edge).unwrap()
        );
    }

    #[test]
    fn canonical_json_for_empty_archive_is_known_string() {
        let json = canonical_json(&empty_archive()).unwrap();
        // serde_json::Map uses BTreeMap by default: keys sort alphabetically,
        // so "edges" precedes "entities".
        assert_eq!(json, r#"{"edges":[],"entities":[]}"#);
    }

    #[test]
    fn tags_sorted_lexicographically_same_hash() {
        let id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let mut e1 = make_entity(id, "Alpha");
        e1.tags = vec!["z".into(), "a".into(), "m".into()];
        let mut e2 = make_entity(id, "Alpha");
        e2.tags = vec!["a".into(), "m".into(), "z".into()];

        let mut a1 = empty_archive();
        a1.entities = vec![e1];
        let mut a2 = empty_archive();
        a2.entities = vec![e2];

        assert_eq!(canonical_json(&a1).unwrap(), canonical_json(&a2).unwrap());
        assert_eq!(
            snapshot_id_for_archive(&a1).unwrap(),
            snapshot_id_for_archive(&a2).unwrap()
        );
    }

    #[test]
    fn property_key_order_independent_hash() {
        let id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let mut e1 = make_entity(id, "Alpha");
        e1.properties = Some(serde_json::json!({"z_key": 1, "a_key": 2}));
        let mut e2 = make_entity(id, "Alpha");
        e2.properties = Some(serde_json::json!({"a_key": 2, "z_key": 1}));

        let mut a1 = empty_archive();
        a1.entities = vec![e1];
        let mut a2 = empty_archive();
        a2.entities = vec![e2];

        assert_eq!(canonical_json(&a1).unwrap(), canonical_json(&a2).unwrap());
        assert_eq!(
            snapshot_id_for_archive(&a1).unwrap(),
            snapshot_id_for_archive(&a2).unwrap()
        );
    }

    #[test]
    fn edge_order_independent_hash() {
        let uid1 = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let uid2 = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        let uid3 = Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap();
        let edge_id1 = Uuid::parse_str("00000000-0000-0000-0000-000000000010").unwrap();
        let edge_id2 = Uuid::parse_str("00000000-0000-0000-0000-000000000020").unwrap();
        let edge1 = ExportedEdge {
            edge_id: edge_id1,
            source: uid1,
            target: uid2,
            relation: EdgeRelation::Extends,
            weight: 1.0,
        };
        let edge2 = ExportedEdge {
            edge_id: edge_id2,
            source: uid2,
            target: uid3,
            relation: EdgeRelation::Extends,
            weight: 0.5,
        };

        let mut a1 = empty_archive();
        a1.edges = vec![edge1.clone(), edge2.clone()];
        let mut a2 = empty_archive();
        a2.edges = vec![edge2, edge1]; // reversed

        assert_eq!(
            snapshot_id_for_archive(&a1).unwrap(),
            snapshot_id_for_archive(&a2).unwrap()
        );
    }

    #[test]
    fn non_finite_edge_weight_rejected() {
        let uid1 = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let uid2 = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        let mut archive = empty_archive();
        archive.edges = vec![ExportedEdge {
            edge_id: Uuid::new_v4(),
            source: uid1,
            target: uid2,
            relation: EdgeRelation::Extends,
            weight: f64::NAN,
        }];
        let err = snapshot_id_for_archive(&archive).unwrap_err();
        assert!(matches!(err, VcsError::Internal(ref msg) if msg.contains("not finite")));
    }

    #[test]
    fn different_entity_name_changes_hash() {
        let id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let e1 = make_entity(id, "Alpha");
        let e2 = make_entity(id, "Beta");

        let mut a1 = empty_archive();
        a1.entities = vec![e1];
        let mut a2 = empty_archive();
        a2.entities = vec![e2];

        assert_ne!(
            snapshot_id_for_archive(&a1).unwrap(),
            snapshot_id_for_archive(&a2).unwrap()
        );
    }

    #[test]
    fn entity_type_change_changes_hash() {
        let id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let mut e1 = make_entity(id, "Alpha");
        e1.entity_type = None;
        let mut e2 = make_entity(id, "Alpha");
        e2.entity_type = Some("paper".to_string());

        let mut a1 = empty_archive();
        a1.entities = vec![e1];
        let mut a2 = empty_archive();
        a2.entities = vec![e2];

        assert_ne!(
            snapshot_id_for_archive(&a1).unwrap(),
            snapshot_id_for_archive(&a2).unwrap(),
            "entity_type must be included in canonical hash (VCS-AUD-003)"
        );
    }
}
