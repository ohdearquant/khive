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
pub(crate) struct SortedValue<'a>(pub(crate) &'a Value);

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
#[path = "hash_tests.rs"]
mod tests;
