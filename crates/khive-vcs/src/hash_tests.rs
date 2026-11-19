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

include!("hash_tests_identity.rs");
include!("hash_tests_canonical.rs");
