// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! KG export / import — portable JSON archive for namespace-scoped knowledge graphs.
//!
//! Implements the v1 portability format described in ADR-010. Embeddings are
//! intentionally excluded: they are regenerable from the embedding model + text
//! and their inclusion would lock the format to a specific model.
//!
//! # Edge namespace enumeration
//!
//! `GraphStore::query_edges` has no namespace column — edges are linked to entities,
//! not namespaces. Export collects all entity IDs in the namespace first, then
//! queries edges where source_id is in that set. This covers every edge whose
//! source entity belongs to the namespace, which is the correct definition of
//! "edges in a namespace" for an export that preserves referential integrity.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use khive_storage::types::{EdgeFilter, LinkId, PageRequest};
use khive_storage::{EdgeRelation, EntityFilter};

use crate::error::{RuntimeError, RuntimeResult};
use crate::runtime::KhiveRuntime;

// ── Archive types ─────────────────────────────────────────────────────────────

/// Portable JSON archive of a namespace-scoped knowledge graph.
///
/// The `format` field is always `"khive-kg"`. The `version` field identifies
/// the serialization schema; parsers should reject unknown versions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KgArchive {
    pub format: String,
    pub version: String,
    pub namespace: String,
    pub exported_at: DateTime<Utc>,
    pub entities: Vec<ExportedEntity>,
    pub edges: Vec<ExportedEdge>,
}

/// An entity record in the portable archive.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExportedEntity {
    pub id: Uuid,
    /// Pack-owned kind string (e.g. `"concept"`, `"person"`).
    pub kind: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<serde_json::Value>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A directed edge record in the portable archive.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExportedEdge {
    pub source: Uuid,
    pub target: Uuid,
    /// One of the 13 canonical relations defined in ADR-002.
    pub relation: EdgeRelation,
    pub weight: f64,
}

/// Outcome of a successful import operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportSummary {
    pub entities_imported: usize,
    pub edges_imported: usize,
    /// Number of edges that were skipped because one or both endpoint UUIDs
    /// were not found in the target namespace after entity import.
    ///
    /// A non-zero value indicates the archive contained dangling edges (edges
    /// referencing entities not present in the archive or the existing graph).
    pub edges_skipped: usize,
}

// ── KhiveRuntime impl ─────────────────────────────────────────────────────────

impl KhiveRuntime {
    /// Export all entities and edges in a namespace to a portable JSON archive.
    ///
    /// Edge collection: all entity IDs in the namespace are gathered first;
    /// `query_edges` is then called with those IDs as `source_ids`. This
    /// captures every edge whose source entity belongs to the namespace.
    pub async fn export_kg(&self, namespace: Option<&str>) -> RuntimeResult<KgArchive> {
        let ns = self.ns(namespace).to_string();

        // 1. Collect all entities in the namespace.
        let entity_page = self
            .entities(Some(&ns))?
            .query_entities(
                &ns,
                EntityFilter::default(),
                PageRequest {
                    offset: 0,
                    limit: u32::MAX,
                },
            )
            .await?;

        let entities: Vec<ExportedEntity> = entity_page
            .items
            .into_iter()
            .map(|e| {
                let created_at =
                    DateTime::from_timestamp_micros(e.created_at).unwrap_or_else(Utc::now);
                let updated_at =
                    DateTime::from_timestamp_micros(e.updated_at).unwrap_or_else(Utc::now);
                ExportedEntity {
                    id: e.id,
                    kind: e.kind.to_string(),
                    name: e.name,
                    description: e.description,
                    properties: e.properties,
                    tags: e.tags,
                    created_at,
                    updated_at,
                }
            })
            .collect();

        // 2. Collect edges whose source is any entity in this namespace.
        let source_ids: Vec<Uuid> = entities.iter().map(|e| e.id).collect();
        let edges = if source_ids.is_empty() {
            Vec::new()
        } else {
            let filter = EdgeFilter {
                source_ids: source_ids.clone(),
                ..Default::default()
            };
            let edge_page = self
                .graph(Some(&ns))?
                .query_edges(
                    filter,
                    Vec::new(),
                    PageRequest {
                        offset: 0,
                        limit: u32::MAX,
                    },
                )
                .await?;

            let id_set: HashSet<Uuid> = source_ids.into_iter().collect();
            edge_page
                .items
                .into_iter()
                .filter(|e| id_set.contains(&e.source_id))
                .map(|e| ExportedEdge {
                    source: e.source_id,
                    target: e.target_id,
                    relation: e.relation,
                    weight: e.weight,
                })
                .collect()
        };

        Ok(KgArchive {
            format: "khive-kg".to_string(),
            version: "0.1".to_string(),
            namespace: ns,
            exported_at: Utc::now(),
            entities,
            edges,
        })
    }

    /// Export to a JSON string (convenience wrapper around `export_kg`).
    pub async fn export_kg_json(&self, namespace: Option<&str>) -> RuntimeResult<String> {
        let archive = self.export_kg(namespace).await?;
        serde_json::to_string(&archive).map_err(|e| RuntimeError::InvalidInput(e.to_string()))
    }

    /// Import an archive into `target_namespace`.
    ///
    /// If `target_namespace` is `None`, the archive's own namespace is used.
    ///
    /// - Entities: upserted by ID; existing records are overwritten.
    /// - Edges: upserted; existing records are overwritten.
    /// - Validation: `format != "khive-kg"` or unsupported version → `InvalidInput`.
    ///   Invalid edge relations are caught at JSON deserialization time.
    pub async fn import_kg(
        &self,
        archive: &KgArchive,
        target_namespace: Option<&str>,
    ) -> RuntimeResult<ImportSummary> {
        // Format validation.
        if archive.format != "khive-kg" {
            return Err(RuntimeError::InvalidInput(format!(
                "unsupported archive format {:?}; expected \"khive-kg\"",
                archive.format
            )));
        }
        if archive.version != "0.1" {
            return Err(RuntimeError::InvalidInput(format!(
                "unsupported archive version {:?}; supported: \"0.1\"",
                archive.version
            )));
        }

        let ns = target_namespace.unwrap_or(&archive.namespace).to_string();

        // Import entities.
        let store = self.entities(Some(&ns))?;
        let mut entities_imported = 0usize;
        for ee in &archive.entities {
            let created_micros = ee.created_at.timestamp_micros();
            let updated_micros = ee.updated_at.timestamp_micros();
            let entity = khive_storage::entity::Entity {
                id: ee.id,
                namespace: ns.clone(),
                kind: ee.kind.clone(),
                name: ee.name.clone(),
                description: ee.description.clone(),
                properties: ee.properties.clone(),
                tags: ee.tags.clone(),
                created_at: created_micros,
                updated_at: updated_micros,
                deleted_at: None,
            };
            store.upsert_entity(entity.clone()).await?;
            // Index into FTS5 (and vector store if a model is configured) so that
            // imported entities are visible to hybrid_search immediately.
            self.reindex_entity(Some(&ns), &entity).await?;
            entities_imported += 1;
        }

        // Import edges — validate both endpoints before inserting.
        //
        // An untrusted archive may contain edges whose source or target UUIDs
        // do not correspond to any entity in the target namespace. Inserting
        // such edges would leave dangling references in the graph store. We
        // therefore check each endpoint with `get_entity` (namespace-scoped,
        // fail-closed) and skip any edge whose source or target is absent.
        let graph = self.graph(Some(&ns))?;
        let mut edges_imported = 0usize;
        let mut edges_skipped = 0usize;
        for ee in &archive.edges {
            let source_ok = self.get_entity(Some(&ns), ee.source).await?.is_some();
            if !source_ok {
                tracing::warn!(
                    source = %ee.source,
                    target = %ee.target,
                    relation = ?ee.relation,
                    "import_kg: skipping edge — source entity not found in namespace {ns:?}"
                );
                edges_skipped += 1;
                continue;
            }
            let target_ok = self.get_entity(Some(&ns), ee.target).await?.is_some();
            if !target_ok {
                tracing::warn!(
                    source = %ee.source,
                    target = %ee.target,
                    relation = ?ee.relation,
                    "import_kg: skipping edge — target entity not found in namespace {ns:?}"
                );
                edges_skipped += 1;
                continue;
            }
            let edge = khive_storage::types::Edge {
                id: LinkId::from(Uuid::new_v4()),
                source_id: ee.source,
                target_id: ee.target,
                relation: ee.relation,
                weight: ee.weight,
                created_at: Utc::now(),
                metadata: None,
            };
            graph.upsert_edge(edge).await?;
            edges_imported += 1;
        }

        Ok(ImportSummary {
            entities_imported,
            edges_imported,
            edges_skipped,
        })
    }

    /// Import from a JSON string (convenience wrapper around `import_kg`).
    pub async fn import_kg_json(
        &self,
        json: &str,
        target_namespace: Option<&str>,
    ) -> RuntimeResult<ImportSummary> {
        let archive: KgArchive =
            serde_json::from_str(json).map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
        self.import_kg(&archive, target_namespace).await
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::KhiveRuntime;
    use khive_storage::EdgeRelation;

    async fn make_rt() -> KhiveRuntime {
        KhiveRuntime::memory().expect("in-memory runtime")
    }

    /// 1. Roundtrip: 3 entities + 2 edges survive export → import on a fresh runtime.
    #[tokio::test]
    async fn roundtrip_entities_and_edges() {
        let src = make_rt().await;
        let e1 = src
            .create_entity(
                None,
                "concept",
                "FlashAttention",
                Some("fast attention"),
                None,
                vec![],
            )
            .await
            .unwrap();
        let e2 = src
            .create_entity(None, "concept", "FlashAttention-2", None, None, vec![])
            .await
            .unwrap();
        let e3 = src
            .create_entity(None, "person", "Tri Dao", None, None, vec!["author".into()])
            .await
            .unwrap();
        src.link(None, e2.id, e1.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        src.link(None, e1.id, e3.id, EdgeRelation::IntroducedBy, 0.9)
            .await
            .unwrap();

        let archive = src.export_kg(None).await.unwrap();
        assert_eq!(archive.entities.len(), 3);
        assert_eq!(archive.edges.len(), 2);
        assert_eq!(archive.format, "khive-kg");
        assert_eq!(archive.version, "0.1");

        let dst = make_rt().await;
        let summary = dst.import_kg(&archive, None).await.unwrap();
        assert_eq!(summary.entities_imported, 3);
        assert_eq!(summary.edges_imported, 2);

        // Spot-check: the imported entity is retrievable.
        let got = dst.get_entity(None, e1.id).await.unwrap();
        assert!(got.is_some());
        let got = got.unwrap();
        assert_eq!(got.name, "FlashAttention");
        assert_eq!(got.description.as_deref(), Some("fast attention"));
    }

    /// 2. JSON roundtrip: export_kg_json → import_kg_json produces equivalent state.
    #[tokio::test]
    async fn json_roundtrip() {
        let src = make_rt().await;
        let e1 = src
            .create_entity(
                None,
                "concept",
                "LoRA",
                Some("low-rank adaptation"),
                Some(serde_json::json!({"year": "2021"})),
                vec!["fine-tuning".into()],
            )
            .await
            .unwrap();
        let e2 = src
            .create_entity(None, "concept", "QLoRA", None, None, vec![])
            .await
            .unwrap();
        src.link(None, e2.id, e1.id, EdgeRelation::VariantOf, 0.9)
            .await
            .unwrap();

        let json_str = src.export_kg_json(None).await.unwrap();
        assert!(json_str.contains("khive-kg"));

        let dst = make_rt().await;
        let summary = dst.import_kg_json(&json_str, None).await.unwrap();
        assert_eq!(summary.entities_imported, 2);
        assert_eq!(summary.edges_imported, 1);

        let got = dst.get_entity(None, e1.id).await.unwrap().unwrap();
        assert_eq!(got.tags, vec!["fine-tuning"]);
    }

    /// 3. Namespace targeting: export from namespace "a", import into namespace "b" on a
    ///    fresh runtime — entities land in "b", and the source runtime's "a" is unaffected.
    ///
    ///    Note: source and destination are separate runtimes (separate in-memory DBs).
    ///    Same-DB cross-namespace copy is not a portability use case — portability is about
    ///    moving graphs between instances, not between namespaces within one instance.
    #[tokio::test]
    async fn namespace_targeting() {
        let src = make_rt().await;
        src.create_entity(Some("a"), "concept", "Sinkhorn", None, None, vec![])
            .await
            .unwrap();

        let archive = src.export_kg(Some("a")).await.unwrap();
        assert_eq!(archive.namespace, "a");

        // Import into a fresh runtime, targeting namespace "b".
        let dst = make_rt().await;
        let summary = dst.import_kg(&archive, Some("b")).await.unwrap();
        assert_eq!(summary.entities_imported, 1);

        // Entity is in "b" on the destination runtime.
        let in_b = dst.list_entities(Some("b"), None, 100).await.unwrap();
        assert_eq!(in_b.len(), 1);
        assert_eq!(in_b[0].name, "Sinkhorn");

        // Namespace "a" on the source runtime is unchanged.
        let in_a = src.list_entities(Some("a"), None, 100).await.unwrap();
        assert_eq!(in_a.len(), 1);

        // Namespace "a" on the destination runtime has nothing (only "b" was written).
        let dst_a = dst.list_entities(Some("a"), None, 100).await.unwrap();
        assert_eq!(dst_a.len(), 0);
    }

    /// 4. Format validation: wrong `format` field → InvalidInput.
    #[tokio::test]
    async fn format_validation_rejects_wrong_format() {
        let rt = make_rt().await;
        let bad = KgArchive {
            format: "wrong".to_string(),
            version: "0.1".to_string(),
            namespace: "local".to_string(),
            exported_at: Utc::now(),
            entities: vec![],
            edges: vec![],
        };
        let err = rt.import_kg(&bad, None).await.unwrap_err();
        assert!(matches!(err, RuntimeError::InvalidInput(_)));
    }

    /// 5. Unsupported archive version → InvalidInput.
    #[tokio::test]
    async fn import_unsupported_archive_version_returns_error() {
        let rt = make_rt().await;
        let bad = KgArchive {
            format: "khive-kg".to_string(),
            version: "999.0".to_string(),
            namespace: "local".to_string(),
            exported_at: Utc::now(),
            entities: vec![],
            edges: vec![],
        };
        let err = rt.import_kg(&bad, None).await.unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidInput(_)),
            "expected InvalidInput, got {err:?}"
        );
        if let RuntimeError::InvalidInput(msg) = err {
            assert!(
                msg.contains("999.0"),
                "error message should mention the unsupported version, got: {msg:?}"
            );
        }
    }

    /// 6. Invalid relation in archive → InvalidInput.
    #[test]
    fn invalid_relation_rejected_at_deserialize() {
        let json = r#"{
            "format":"khive-kg","version":"0.1","namespace":"local",
            "exported_at":"2026-01-01T00:00:00Z",
            "entities":[],
            "edges":[{"source":"00000000-0000-0000-0000-000000000001",
                       "target":"00000000-0000-0000-0000-000000000002",
                       "relation":"related_to","weight":0.5}]
        }"#;
        let result: Result<KgArchive, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "non-canonical relation should fail to deserialize"
        );
    }

    // ── Dangling-edge validation tests (issue #28) ────────────────────────────

    /// 6. Edge with dangling source (source UUID not in entity table) is skipped.
    ///
    /// The archive has one entity + one edge whose source is a phantom UUID.
    /// Import succeeds, entities_imported=1, edges_imported=0, edges_skipped=1.
    #[tokio::test]
    async fn import_edge_with_dangling_source_is_skipped() {
        let phantom_source = Uuid::parse_str("deadbeef-dead-4ead-dead-deadbeefcafe").unwrap();

        let rt = make_rt().await;
        // Create an entity that will be the real target.
        let real = rt
            .create_entity(None, "concept", "Real", None, None, vec![])
            .await
            .unwrap();

        // Build archive manually: one real entity, one edge with phantom source.
        let archive = KgArchive {
            format: "khive-kg".to_string(),
            version: "0.1".to_string(),
            namespace: "local".to_string(),
            exported_at: Utc::now(),
            entities: vec![ExportedEntity {
                id: real.id,
                kind: "concept".to_string(),
                name: "Real".to_string(),
                description: None,
                properties: None,
                tags: vec![],
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }],
            edges: vec![ExportedEdge {
                source: phantom_source,
                target: real.id,
                relation: EdgeRelation::Extends,
                weight: 1.0,
            }],
        };

        let dst = make_rt().await;
        let summary = dst.import_kg(&archive, None).await.unwrap();
        assert_eq!(summary.entities_imported, 1);
        assert_eq!(
            summary.edges_imported, 0,
            "dangling source must not be imported"
        );
        assert_eq!(
            summary.edges_skipped, 1,
            "dangling source must be counted as skipped"
        );
    }

    /// 7. Edge with dangling target (target UUID not in entity table) is skipped.
    ///
    /// The archive has one entity + one edge whose target is a phantom UUID.
    /// Import succeeds, entities_imported=1, edges_imported=0, edges_skipped=1.
    #[tokio::test]
    async fn import_edge_with_dangling_target_is_skipped() {
        let phantom_target = Uuid::parse_str("cafebabe-cafe-4abe-cafe-cafebabecafe").unwrap();

        let rt = make_rt().await;
        let real = rt
            .create_entity(None, "concept", "Source", None, None, vec![])
            .await
            .unwrap();

        let archive = KgArchive {
            format: "khive-kg".to_string(),
            version: "0.1".to_string(),
            namespace: "local".to_string(),
            exported_at: Utc::now(),
            entities: vec![ExportedEntity {
                id: real.id,
                kind: "concept".to_string(),
                name: "Source".to_string(),
                description: None,
                properties: None,
                tags: vec![],
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }],
            edges: vec![ExportedEdge {
                source: real.id,
                target: phantom_target,
                relation: EdgeRelation::DependsOn,
                weight: 0.8,
            }],
        };

        let dst = make_rt().await;
        let summary = dst.import_kg(&archive, None).await.unwrap();
        assert_eq!(summary.entities_imported, 1);
        assert_eq!(
            summary.edges_imported, 0,
            "dangling target must not be imported"
        );
        assert_eq!(
            summary.edges_skipped, 1,
            "dangling target must be counted as skipped"
        );
    }

    /// 8. Mixed batch: some valid edges and some dangling edges — correct counts reported.
    ///
    /// Archive has 3 entities, 2 valid edges, and 1 dangling edge (phantom target).
    /// Import succeeds with edges_imported=2, edges_skipped=1.
    #[tokio::test]
    async fn import_mixed_edges_reports_correct_counts() {
        let phantom = Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap();

        let src = make_rt().await;
        let a = src
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = src
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        let c = src
            .create_entity(None, "concept", "C", None, None, vec![])
            .await
            .unwrap();

        // Build archive with 3 entities and 3 edges: 2 valid, 1 dangling.
        let archive = KgArchive {
            format: "khive-kg".to_string(),
            version: "0.1".to_string(),
            namespace: "local".to_string(),
            exported_at: Utc::now(),
            entities: vec![
                ExportedEntity {
                    id: a.id,
                    kind: "concept".to_string(),
                    name: "A".to_string(),
                    description: None,
                    properties: None,
                    tags: vec![],
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                },
                ExportedEntity {
                    id: b.id,
                    kind: "concept".to_string(),
                    name: "B".to_string(),
                    description: None,
                    properties: None,
                    tags: vec![],
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                },
                ExportedEntity {
                    id: c.id,
                    kind: "concept".to_string(),
                    name: "C".to_string(),
                    description: None,
                    properties: None,
                    tags: vec![],
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                },
            ],
            edges: vec![
                // Valid: A → B
                ExportedEdge {
                    source: a.id,
                    target: b.id,
                    relation: EdgeRelation::Extends,
                    weight: 1.0,
                },
                // Valid: B → C
                ExportedEdge {
                    source: b.id,
                    target: c.id,
                    relation: EdgeRelation::DependsOn,
                    weight: 0.9,
                },
                // Dangling: A → phantom
                ExportedEdge {
                    source: a.id,
                    target: phantom,
                    relation: EdgeRelation::Enables,
                    weight: 0.5,
                },
            ],
        };

        let dst = make_rt().await;
        let summary = dst.import_kg(&archive, None).await.unwrap();
        assert_eq!(summary.entities_imported, 3);
        assert_eq!(
            summary.edges_imported, 2,
            "only valid edges must be imported"
        );
        assert_eq!(
            summary.edges_skipped, 1,
            "one dangling edge must be reported"
        );
    }

    /// 9. All-valid edges produce edges_skipped=0 (no regression on the happy path).
    #[tokio::test]
    async fn import_all_valid_edges_reports_zero_skipped() {
        let src = make_rt().await;
        let e1 = src
            .create_entity(None, "concept", "E1", None, None, vec![])
            .await
            .unwrap();
        let e2 = src
            .create_entity(None, "concept", "E2", None, None, vec![])
            .await
            .unwrap();
        src.link(None, e1.id, e2.id, EdgeRelation::VariantOf, 0.7)
            .await
            .unwrap();

        let archive = src.export_kg(None).await.unwrap();
        let dst = make_rt().await;
        let summary = dst.import_kg(&archive, None).await.unwrap();
        assert_eq!(summary.edges_imported, 1);
        assert_eq!(
            summary.edges_skipped, 0,
            "no edges should be skipped when all endpoints exist"
        );
    }
}
