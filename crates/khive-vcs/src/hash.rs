// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Canonical JSON serialization and SHA-256 snapshot hashing.
//!
//! Algorithm:
//! 1. Collect non-soft-deleted entities; sort by UUID string ascending.
//! 2. Collect edges; canonicalize symmetric endpoints, then sort by
//!    (source, target, relation) ascending.
//! 3. Serialize as `{"edges":[...],"entities":[...]}` with fixed field order and no whitespace.
//! 4. SHA-256 the UTF-8 bytes; prefix with `"sha256:"`.

use std::io::{self, Write};

use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use khive_runtime::portability::{ExportedEdge, ExportedEntity, KgArchive};

use crate::error::VcsError;
use crate::types::SnapshotId;

/// Compute the content-addressed `SnapshotId` for a `KgArchive`.
///
/// Record references are sorted once, then serialized directly into SHA-256.
/// The full canonical JSON and a cloned archive are never materialized.
pub fn snapshot_id_for_archive(archive: &KgArchive) -> Result<SnapshotId, VcsError> {
    let canonical = CanonicalArchive::new(archive)?;
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, &canonical).map_err(VcsError::Json)?;
    SnapshotId::from_hash(&hex::encode(writer.0.finalize()))
}

/// Produce canonical JSON for callers that need the bytes, using the same
/// borrowed serializer as the hashing path.
pub fn canonical_json(archive: &KgArchive) -> Result<String, VcsError> {
    serde_json::to_string(&CanonicalArchive::new(archive)?).map_err(VcsError::Json)
}

struct HashWriter(Sha256);

impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct CanonicalArchive<'a> {
    entities: Vec<CanonicalEntity<'a>>,
    edges: Vec<CanonicalEdge<'a>>,
}

impl<'a> CanonicalArchive<'a> {
    fn new(archive: &'a KgArchive) -> Result<Self, VcsError> {
        // UUID's byte ordering is the ordering of its canonical lowercase
        // hyphenated string, without allocating that string per comparison.
        let mut entities: Vec<_> = archive.entities.iter().map(CanonicalEntity).collect();
        entities.sort_by_key(|entity| entity.0.id);

        let mut edges = Vec::with_capacity(archive.edges.len());
        for edge in &archive.edges {
            let (source, target) = edge.relation.canonical_endpoints(edge.source, edge.target);
            let weight = serde_json::Number::from_f64(edge.weight).ok_or_else(|| {
                VcsError::Internal(format!(
                    "edge weight is not finite (NaN or Infinity): {}",
                    edge.weight
                ))
            })?;
            edges.push(CanonicalEdge {
                record: edge,
                source,
                target,
                relation: edge.relation.to_string(),
                weight,
            });
        }
        edges.sort_by(|a, b| {
            (a.source, a.target, a.relation.as_str()).cmp(&(
                b.source,
                b.target,
                b.relation.as_str(),
            ))
        });
        Ok(Self { entities, edges })
    }
}

impl Serialize for CanonicalArchive<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("edges", &self.edges)?;
        map.serialize_entry("entities", &self.entities)?;
        map.end()
    }
}

struct CanonicalEntity<'a>(&'a ExportedEntity);

impl Serialize for CanonicalEntity<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let entity = self.0;
        let mut tags: Vec<_> = entity.tags.iter().map(String::as_str).collect();
        tags.sort_unstable();
        let mut map = serializer.serialize_map(Some(7))?;
        map.serialize_entry("description", &entity.description)?;
        map.serialize_entry("entity_type", &entity.entity_type)?;
        map.serialize_entry("id", &entity.id.to_string())?;
        map.serialize_entry("kind", &entity.kind)?;
        map.serialize_entry("name", &entity.name)?;
        map.serialize_entry("properties", &entity.properties.as_ref().map(SortedValue))?;
        map.serialize_entry("tags", &tags)?;
        map.end()
    }
}

struct CanonicalEdge<'a> {
    record: &'a ExportedEdge,
    source: Uuid,
    target: Uuid,
    relation: String,
    weight: serde_json::Number,
}

impl Serialize for CanonicalEdge<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(6))?;
        map.serialize_entry("edge_id", &self.record.edge_id.to_string())?;
        map.serialize_entry(
            "properties",
            &self.record.properties.as_ref().map(SortedValue),
        )?;
        map.serialize_entry("relation", &self.relation)?;
        map.serialize_entry("source", &self.source.to_string())?;
        map.serialize_entry("target", &self.target.to_string())?;
        map.serialize_entry("weight", &self.weight)?;
        map.end()
    }
}

/// Sort nested object keys without cloning property values. Array order stays
/// unchanged; JSON string and number rendering remains serde_json's own.
struct SortedValue<'a>(&'a Value);

impl Serialize for SortedValue<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.0 {
            Value::Null => serializer.serialize_unit(),
            Value::Bool(value) => serializer.serialize_bool(*value),
            Value::Number(value) => value.serialize(serializer),
            Value::String(value) => serializer.serialize_str(value),
            Value::Array(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(&SortedValue(value))?;
                }
                sequence.end()
            }
            Value::Object(values) => {
                let mut entries: Vec<_> = values.iter().collect();
                entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
                let mut map = serializer.serialize_map(Some(entries.len()))?;
                for (key, value) in entries {
                    map.serialize_entry(key, &SortedValue(value))?;
                }
                map.end()
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use khive_runtime::portability::{ExportedEdge, ExportedEntity, KgArchive};
    use khive_storage::EdgeRelation;
    use serde_json::{json, Map};
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
            properties: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }];

        assert_ne!(
            snapshot_id_for_archive(&without_edge).unwrap(),
            snapshot_id_for_archive(&with_edge).unwrap()
        );
    }

    #[test]
    fn symmetric_edge_direction_does_not_change_snapshot_id() {
        let low = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let high = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        let edge_id = Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap();
        let timestamp = Utc::now();
        for relation in [EdgeRelation::CompetesWith, EdgeRelation::ComposedWith] {
            let edge = ExportedEdge {
                edge_id,
                source: low,
                target: high,
                relation,
                weight: 0.7,
                properties: None,
                created_at: timestamp,
                updated_at: timestamp,
            };
            let mut forward = empty_archive();
            forward.edges.push(edge.clone());
            let mut reversed = empty_archive();
            reversed.edges.push(ExportedEdge {
                source: high,
                target: low,
                ..edge
            });
            assert_eq!(
                snapshot_id_for_archive(&forward).unwrap(),
                snapshot_id_for_archive(&reversed).unwrap(),
                "{relation} endpoint order must not change snapshot identity"
            );
        }
    }

    #[test]
    fn directed_edge_direction_still_changes_snapshot_id() {
        let low = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let high = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        let edge = ExportedEdge {
            edge_id: Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
            source: low,
            target: high,
            relation: EdgeRelation::Extends,
            weight: 0.7,
            properties: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let mut forward = empty_archive();
        forward.edges.push(edge.clone());
        let mut reversed = empty_archive();
        reversed.edges.push(ExportedEdge {
            source: high,
            target: low,
            ..edge
        });
        assert_ne!(
            snapshot_id_for_archive(&forward).unwrap(),
            snapshot_id_for_archive(&reversed).unwrap(),
            "directed edge orientation remains part of snapshot identity"
        );
    }

    #[test]
    fn edge_properties_change_snapshot_hash() {
        let source = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let target = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        let edge_id = Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap();
        let timestamp = Utc::now();
        let edge = ExportedEdge {
            edge_id,
            source,
            target,
            relation: EdgeRelation::Extends,
            weight: 1.0,
            properties: Some(serde_json::json!({"confidence": 0.4})),
            created_at: timestamp,
            updated_at: timestamp,
        };
        let mut changed = edge.clone();
        changed.properties = Some(serde_json::json!({"confidence": 0.9}));

        let mut before = empty_archive();
        before.edges = vec![edge];
        let mut after = empty_archive();
        after.edges = vec![changed];

        assert_ne!(
            snapshot_id_for_archive(&before).unwrap(),
            snapshot_id_for_archive(&after).unwrap(),
            "metadata-only edge changes must alter snapshot identity"
        );
    }

    #[test]
    fn edge_property_key_order_is_hash_independent() {
        let source = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let target = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        let timestamp = Utc::now();
        let edge = ExportedEdge {
            edge_id: Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
            source,
            target,
            relation: EdgeRelation::Extends,
            weight: 1.0,
            properties: Some(serde_json::json!({"a": 1, "z": {"a": 2, "z": 3}})),
            created_at: timestamp,
            updated_at: timestamp,
        };
        let mut reordered = edge.clone();
        reordered.properties = Some(serde_json::json!({"z": {"z": 3, "a": 2}, "a": 1}));
        let mut left = empty_archive();
        left.edges = vec![edge];
        let mut right = empty_archive();
        right.edges = vec![reordered];

        assert_eq!(
            snapshot_id_for_archive(&left).unwrap(),
            snapshot_id_for_archive(&right).unwrap(),
            "edge property object ordering must not affect snapshot identity"
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
            properties: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let edge2 = ExportedEdge {
            edge_id: edge_id2,
            source: uid2,
            target: uid3,
            relation: EdgeRelation::Extends,
            weight: 0.5,
            properties: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
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
            properties: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
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

    fn legacy_sorted_value(value: Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut pairs: Vec<_> = map.into_iter().collect();
                pairs.sort_by(|a, b| a.0.cmp(&b.0));
                Value::Object(
                    pairs
                        .into_iter()
                        .map(|(key, value)| (key, legacy_sorted_value(value)))
                        .collect(),
                )
            }
            Value::Array(values) => {
                Value::Array(values.into_iter().map(legacy_sorted_value).collect())
            }
            other => other,
        }
    }

    // Keep the former materialized representation as a byte-level oracle for
    // this small fixture. The production path no longer builds these Values.
    fn legacy_canonical_json(archive: &KgArchive) -> String {
        let mut entities = archive.entities.clone();
        entities.sort_by(|a, b| {
            a.id.to_string()
                .to_ascii_lowercase()
                .cmp(&b.id.to_string().to_ascii_lowercase())
        });
        let mut edges = archive.edges.clone();
        for edge in &mut edges {
            if edge.relation.is_symmetric() && edge.target < edge.source {
                std::mem::swap(&mut edge.source, &mut edge.target);
            }
        }
        edges.sort_by(|a, b| {
            (
                a.source.to_string(),
                a.target.to_string(),
                a.relation.to_string(),
            )
                .cmp(&(
                    b.source.to_string(),
                    b.target.to_string(),
                    b.relation.to_string(),
                ))
        });
        let entity_values: Vec<_> = entities
            .iter()
            .map(|entity| {
                let mut tags = entity.tags.clone();
                tags.sort();
                let mut object = Map::new();
                object.insert("id".into(), Value::String(entity.id.to_string()));
                object.insert("kind".into(), Value::String(entity.kind.clone()));
                object.insert(
                    "entity_type".into(),
                    entity
                        .entity_type
                        .clone()
                        .map_or(Value::Null, Value::String),
                );
                object.insert("name".into(), Value::String(entity.name.clone()));
                object.insert(
                    "description".into(),
                    entity
                        .description
                        .clone()
                        .map_or(Value::Null, Value::String),
                );
                object.insert(
                    "properties".into(),
                    entity
                        .properties
                        .clone()
                        .map_or(Value::Null, legacy_sorted_value),
                );
                object.insert(
                    "tags".into(),
                    Value::Array(tags.into_iter().map(Value::String).collect()),
                );
                Value::Object(object)
            })
            .collect();
        let edge_values: Vec<_> = edges
            .iter()
            .map(|edge| {
                let mut object = Map::new();
                object.insert("edge_id".into(), Value::String(edge.edge_id.to_string()));
                object.insert("source".into(), Value::String(edge.source.to_string()));
                object.insert("target".into(), Value::String(edge.target.to_string()));
                object.insert("relation".into(), Value::String(edge.relation.to_string()));
                object.insert(
                    "weight".into(),
                    Value::Number(serde_json::Number::from_f64(edge.weight).unwrap()),
                );
                object.insert(
                    "properties".into(),
                    edge.properties
                        .clone()
                        .map_or(Value::Null, legacy_sorted_value),
                );
                Value::Object(object)
            })
            .collect();
        let mut root = Map::new();
        root.insert("entities".into(), Value::Array(entity_values));
        root.insert("edges".into(), Value::Array(edge_values));
        serde_json::to_string(&Value::Object(root)).unwrap()
    }

    #[test]
    fn borrowed_serializer_preserves_legacy_canonical_bytes_and_hash() {
        let low = Uuid::from_u128(1);
        let high = Uuid::from_u128(2);
        let mut archive = empty_archive();
        let mut first = make_entity(low, "α\nquoted \"name\"");
        first.entity_type = Some("paper".into());
        first.description = Some("description".into());
        first.tags = vec!["z".into(), "a".into(), "a".into()];
        first.properties = Some(json!({"z":[{"b":2,"a":1},null],"a":{"y":true,"x":0.5}}));
        archive.entities = vec![make_entity(high, "second"), first];
        archive.edges = vec![ExportedEdge {
            edge_id: Uuid::from_u128(3),
            source: high,
            target: low,
            relation: EdgeRelation::ComposedWith,
            weight: -0.0,
            properties: Some(json!({"z":{"b":2,"a":1},"a":[3,2,1]})),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }];

        let legacy = legacy_canonical_json(&archive);
        assert_eq!(canonical_json(&archive).unwrap(), legacy);
        let expected =
            SnapshotId::from_hash(&hex::encode(Sha256::digest(legacy.as_bytes()))).unwrap();
        assert_eq!(snapshot_id_for_archive(&archive).unwrap(), expected);
    }
}
