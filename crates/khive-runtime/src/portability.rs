//! KG export / import — portable JSON archive for namespace-scoped knowledge graphs.
//!
//! Embeddings are excluded (regenerable from text + model). Edges are collected by
//! querying all entity IDs in the namespace first, then fetching incident edges.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use khive_storage::types::{EdgeFilter, LinkId, PageRequest};
use khive_storage::{EdgeRelation, EntityFilter};

use crate::error::{RuntimeError, RuntimeResult};
use crate::runtime::{KhiveRuntime, NamespaceToken};

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
    /// Pack-governed subtype token (e.g. `"paper"`, `"snapshot"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_type: Option<String>,
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
    /// Stable edge identity across export/import cycles.
    ///
    /// Old archives (pre-0.2) omit this field. `serde(default)` assigns a fresh
    /// UUID on import so backward-compatible archives are accepted as-is.
    #[serde(default = "Uuid::new_v4")]
    pub edge_id: Uuid,
    pub source: Uuid,
    pub target: Uuid,
    /// One of the canonical edge relations (closed enum).
    pub relation: EdgeRelation,
    pub weight: f64,
    /// Portable edge metadata, named `properties` in the ADR-020 wire shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<serde_json::Value>,
    /// Edge creation time. Older archives may omit it; those imports use the
    /// time at which the archive is decoded.
    #[serde(
        default = "default_import_timestamp",
        deserialize_with = "deserialize_import_timestamp"
    )]
    pub created_at: DateTime<Utc>,
    /// Edge last-update time. Kept independent from `created_at` so imported
    /// provenance is not collapsed during a round trip.
    #[serde(
        default = "default_import_timestamp",
        deserialize_with = "deserialize_import_timestamp"
    )]
    pub updated_at: DateTime<Utc>,
}

fn default_import_timestamp() -> DateTime<Utc> {
    Utc::now()
}

fn deserialize_import_timestamp<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    DateTime::parse_from_rfc3339(&raw)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|error| serde::de::Error::custom(format!("timestamp must be RFC3339: {error}")))
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
    /// Aggregate actual outcomes from reindexing imported entities. Source
    /// text remains complete in the entity store and FTS when this is non-zero.
    #[serde(default)]
    pub embedding_truncation: crate::retrieval::EmbeddingTruncationReport,
}

// ── KhiveRuntime impl ─────────────────────────────────────────────────────────

impl KhiveRuntime {
    /// Export all entities and edges in a namespace to a portable JSON archive.
    ///
    /// Edge collection: all entity IDs in the namespace are gathered first;
    /// `query_edges` is then called with those IDs as `source_ids`. This
    /// captures every edge whose source entity belongs to the namespace.
    pub async fn export_kg(&self, token: &NamespaceToken) -> RuntimeResult<KgArchive> {
        let ns = token.namespace().as_str().to_owned();

        let entity_page = self
            .entities(token)?
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
                    entity_type: e.entity_type,
                    name: e.name,
                    description: e.description,
                    properties: e.properties,
                    tags: e.tags,
                    created_at,
                    updated_at,
                }
            })
            .collect();

        let source_ids: Vec<Uuid> = entities.iter().map(|e| e.id).collect();
        let edges = if source_ids.is_empty() {
            Vec::new()
        } else {
            let filter = EdgeFilter {
                source_ids: source_ids.clone(),
                ..Default::default()
            };
            let edge_page = self
                .graph(token)?
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
                    edge_id: e.id.into(),
                    source: e.source_id,
                    target: e.target_id,
                    relation: e.relation,
                    weight: e.weight,
                    properties: e.metadata,
                    created_at: e.created_at,
                    updated_at: e.updated_at,
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
    pub async fn export_kg_json(&self, token: &NamespaceToken) -> RuntimeResult<String> {
        let archive = self.export_kg(token).await?;
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
        token: &NamespaceToken,
    ) -> RuntimeResult<ImportSummary> {
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

        let ns = token.namespace().as_str().to_owned();

        // Complete deterministic validation before opening a store or issuing
        // the first write. Endpoint existence and namespace checks remain at
        // write time because they depend on mutable target state.
        for (index, entity) in archive.entities.iter().enumerate() {
            self.validate_entity_kind(&entity.kind)?;
            // Archive content is caller-controlled input: the runtime-owned
            // `khive:secret_gate` property key is reservation-only on import,
            // exactly as on every other properties-bearing write path
            // (ADR-115 Amendment 1 §3). Import never consumes exemptions.
            crate::secret_gate::reject_reserved_secret_gate_property(entity.properties.as_ref())?;
            if entity.name.trim().is_empty() {
                return Err(RuntimeError::InvalidInput(format!(
                    "archive entity {index} ({}) name must be non-blank",
                    entity.id
                )));
            }
        }
        for edge in &archive.edges {
            crate::operations::validate_edge_weight(edge.weight)?;
        }

        let store = self.entities(token)?;
        let mut entities_imported = 0usize;
        let mut embedding_truncation = crate::retrieval::EmbeddingTruncationReport::default();
        for ee in &archive.entities {
            let created_micros = ee.created_at.timestamp_micros();
            let updated_micros = ee.updated_at.timestamp_micros();
            let entity = khive_storage::entity::Entity {
                id: ee.id,
                namespace: ns.clone(),
                kind: ee.kind.clone(),
                entity_type: ee.entity_type.clone(),
                name: ee.name.clone(),
                description: ee.description.clone(),
                properties: ee.properties.clone(),
                tags: ee.tags.clone(),
                created_at: created_micros,
                updated_at: updated_micros,
                deleted_at: None,
                merged_into: None,
                merge_event_id: None,
                content_ref: None,
            };
            store.upsert_entity(entity.clone()).await?;
            // Reindex so imported entities are searchable via hybrid_search immediately.
            embedding_truncation.merge(self.reindex_entity(token, &entity).await?);
            entities_imported += 1;
        }

        // Untrusted archives may reference entities absent from the target namespace;
        // check both endpoints and skip edges with a missing source or target to avoid
        // dangling references in the graph store.
        let graph = self.graph(token)?;
        let mut edges_imported = 0usize;
        let mut edges_skipped = 0usize;
        for ee in &archive.edges {
            let source_ok = match self.get_entity(token, ee.source).await {
                Ok(_) => true,
                Err(RuntimeError::NotFound(_)) => false,
                Err(e) => return Err(e),
            };
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
            let target_ok = match self.get_entity(token, ee.target).await {
                Ok(_) => true,
                Err(RuntimeError::NotFound(_)) => false,
                Err(e) => return Err(e),
            };
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
            // Only a contract verdict (InvalidInput) or an endpoint-resolution
            // miss (NotFound) is a property of the edge being imported; any
            // other error is an operational failure of the check itself and
            // must abort the import rather than silently discard a valid edge.
            match self
                .validate_edge_relation_endpoints(token, ee.source, ee.target, ee.relation)
                .await
            {
                Ok(()) => {}
                Err(e @ (RuntimeError::InvalidInput(_) | RuntimeError::NotFound(_))) => {
                    tracing::warn!(
                        source = %ee.source,
                        target = %ee.target,
                        relation = ?ee.relation,
                        error = %e,
                        "import_kg: skipping edge — endpoint contract violation in namespace {ns:?}"
                    );
                    edges_skipped += 1;
                    continue;
                }
                Err(e) => return Err(e),
            }
            let edge = khive_storage::types::Edge {
                id: LinkId::from(ee.edge_id),
                namespace: ns.clone(),
                source_id: ee.source,
                target_id: ee.target,
                relation: ee.relation,
                weight: ee.weight,
                created_at: ee.created_at,
                updated_at: ee.updated_at,
                deleted_at: None,
                metadata: ee.properties.clone(),
                target_backend: None,
            };
            graph.upsert_edge(edge).await?;
            edges_imported += 1;
        }

        Ok(ImportSummary {
            entities_imported,
            edges_imported,
            edges_skipped,
            embedding_truncation,
        })
    }

    /// Import from a JSON string (convenience wrapper around `import_kg`).
    pub async fn import_kg_json(
        &self,
        json: &str,
        token: &NamespaceToken,
    ) -> RuntimeResult<ImportSummary> {
        let archive: KgArchive =
            serde_json::from_str(json).map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
        self.import_kg(&archive, token).await
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

// Kept inline: these tests exercise round-trip invariants over private encoding
// helpers that would otherwise need to be made pub to test from tests/.
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::runtime::{KhiveRuntime, NamespaceToken};
    use crate::{EmbedderProvider, Namespace};
    use async_trait::async_trait;
    use khive_storage::EdgeRelation;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService, MAX_TEXT_BYTES};

    const IMPORT_TEST_MODEL: &str = "all-minilm-l6-v2";

    struct ImportEmbeddingService;

    #[async_trait]
    impl EmbeddingService for ImportEmbeddingService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            let dimensions = EmbeddingModel::AllMiniLmL6V2.dimensions();
            Ok(texts.iter().map(|_| vec![1.0; dimensions]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "kg-import-truncation-test"
        }
    }

    struct ImportEmbeddingProvider;

    #[async_trait]
    impl EmbedderProvider for ImportEmbeddingProvider {
        fn name(&self) -> &str {
            IMPORT_TEST_MODEL
        }

        fn dimensions(&self) -> usize {
            EmbeddingModel::AllMiniLmL6V2.dimensions()
        }

        async fn build(&self) -> std::result::Result<Arc<dyn EmbeddingService>, RuntimeError> {
            Ok(Arc::new(ImportEmbeddingService))
        }
    }

    async fn make_rt() -> KhiveRuntime {
        KhiveRuntime::memory().expect("in-memory runtime")
    }

    fn make_rt_with_embedder() -> KhiveRuntime {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        runtime.register_embedder(ImportEmbeddingProvider);
        runtime
    }

    #[tokio::test]
    async fn import_summary_reports_actual_embedding_truncation() {
        let runtime = make_rt_with_embedder();
        let token = NamespaceToken::local();
        let archive = KgArchive {
            format: "khive-kg".to_string(),
            version: "0.1".to_string(),
            namespace: "local".to_string(),
            exported_at: Utc::now(),
            entities: vec![ExportedEntity {
                id: Uuid::new_v4(),
                kind: "concept".to_string(),
                entity_type: None,
                name: "Imported long entity".to_string(),
                description: Some("x".repeat(MAX_TEXT_BYTES + 1)),
                properties: None,
                tags: vec![],
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }],
            edges: vec![],
        };

        let summary = runtime
            .import_kg(&archive, &token)
            .await
            .expect("import with bounded embedding input");

        assert_eq!(summary.entities_imported, 1);
        assert_eq!(summary.embedding_truncation.truncated, 1);
        assert!(summary.embedding_truncation.discarded_bytes > 0);
        let wire = serde_json::to_value(&summary).expect("serialize import summary");
        assert_eq!(wire["embedding_truncation"]["truncated"], 1);
    }

    #[tokio::test]
    async fn import_rejects_whitespace_name_before_any_entity_write() {
        let runtime = make_rt().await;
        let token = NamespaceToken::local();
        let valid_id = Uuid::new_v4();
        let archive = KgArchive {
            format: "khive-kg".to_string(),
            version: "0.1".to_string(),
            namespace: "local".to_string(),
            exported_at: Utc::now(),
            entities: vec![
                ExportedEntity {
                    id: valid_id,
                    kind: "concept".to_string(),
                    entity_type: None,
                    name: "Must not be written".to_string(),
                    description: None,
                    properties: None,
                    tags: vec![],
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                },
                ExportedEntity {
                    id: Uuid::new_v4(),
                    kind: "concept".to_string(),
                    entity_type: None,
                    name: " \t\n ".to_string(),
                    description: None,
                    properties: None,
                    tags: vec![],
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                },
            ],
            edges: vec![],
        };

        let err = runtime
            .import_kg(&archive, &token)
            .await
            .expect_err("whitespace-only entity names must fail the whole import");
        assert!(
            err.to_string().contains("non-blank"),
            "error must explain the name invariant: {err}"
        );
        assert!(
            runtime.get_entity(&token, valid_id).await.is_err(),
            "deterministic validation must finish before the first entity write"
        );
    }

    /// 1. Roundtrip: 3 entities + 2 edges survive export → import on a fresh runtime.
    #[tokio::test]
    async fn roundtrip_entities_and_edges() {
        let src = make_rt().await;
        let tok = NamespaceToken::local();
        let e1 = src
            .create_entity(
                &tok,
                "concept",
                None,
                "FlashAttention",
                Some("fast attention"),
                None,
                vec![],
            )
            .await
            .unwrap();
        let e2 = src
            .create_entity(
                &tok,
                "concept",
                None,
                "FlashAttention-2",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let e3 = src
            .create_entity(
                &tok,
                "person",
                None,
                "Tri Dao",
                None,
                None,
                vec!["author".into()],
            )
            .await
            .unwrap();
        src.link(&tok, e2.id, e1.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        src.link(&tok, e1.id, e3.id, EdgeRelation::IntroducedBy, 0.9, None)
            .await
            .unwrap();

        let archive = src.export_kg(&tok).await.unwrap();
        assert_eq!(archive.entities.len(), 3);
        assert_eq!(archive.edges.len(), 2);
        assert_eq!(archive.format, "khive-kg");
        assert_eq!(archive.version, "0.1");

        let dst = make_rt().await;
        let summary = dst.import_kg(&archive, &tok).await.unwrap();
        assert_eq!(summary.entities_imported, 3);
        assert_eq!(summary.edges_imported, 2);

        let got = dst.get_entity(&tok, e1.id).await.unwrap();
        assert_eq!(got.name, "FlashAttention");
        assert_eq!(got.description.as_deref(), Some("fast attention"));
    }

    /// 2. JSON roundtrip: export_kg_json → import_kg_json produces equivalent state.
    #[tokio::test]
    async fn json_roundtrip() {
        let src = make_rt().await;
        let tok = NamespaceToken::local();
        let e1 = src
            .create_entity(
                &tok,
                "concept",
                None,
                "LoRA",
                Some("low-rank adaptation"),
                Some(serde_json::json!({"year": "2021"})),
                vec!["fine-tuning".into()],
            )
            .await
            .unwrap();
        let e2 = src
            .create_entity(&tok, "concept", None, "QLoRA", None, None, vec![])
            .await
            .unwrap();
        src.link(&tok, e2.id, e1.id, EdgeRelation::VariantOf, 0.9, None)
            .await
            .unwrap();

        let json_str = src.export_kg_json(&tok).await.unwrap();
        assert!(json_str.contains("khive-kg"));

        let dst = make_rt().await;
        let summary = dst.import_kg_json(&json_str, &tok).await.unwrap();
        assert_eq!(summary.entities_imported, 2);
        assert_eq!(summary.edges_imported, 1);

        let got = dst.get_entity(&tok, e1.id).await.unwrap();
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
        let tok_a = NamespaceToken::for_namespace(Namespace::parse("a").unwrap());
        let tok_b = NamespaceToken::for_namespace(Namespace::parse("b").unwrap());
        src.create_entity(&tok_a, "concept", None, "Sinkhorn", None, None, vec![])
            .await
            .unwrap();

        let archive = src.export_kg(&tok_a).await.unwrap();
        assert_eq!(archive.namespace, "a");

        let dst = make_rt().await;
        let summary = dst.import_kg(&archive, &tok_b).await.unwrap();
        assert_eq!(summary.entities_imported, 1);

        let in_b = dst.list_entities(&tok_b, None, None, 100, 0).await.unwrap();
        assert_eq!(in_b.len(), 1);
        assert_eq!(in_b[0].name, "Sinkhorn");

        let in_a = src.list_entities(&tok_a, None, None, 100, 0).await.unwrap();
        assert_eq!(in_a.len(), 1);

        let dst_a = dst.list_entities(&tok_a, None, None, 100, 0).await.unwrap();
        assert_eq!(dst_a.len(), 0);
    }

    /// 4. Format validation: wrong `format` field → InvalidInput.
    #[tokio::test]
    async fn format_validation_rejects_wrong_format() {
        let rt = make_rt().await;
        let tok = NamespaceToken::local();
        let bad = KgArchive {
            format: "wrong".to_string(),
            version: "0.1".to_string(),
            namespace: "local".to_string(),
            exported_at: Utc::now(),
            entities: vec![],
            edges: vec![],
        };
        let err = rt.import_kg(&bad, &tok).await.unwrap_err();
        assert!(matches!(err, RuntimeError::InvalidInput(_)));
    }

    /// 5. Unsupported archive version → InvalidInput.
    #[tokio::test]
    async fn import_unsupported_archive_version_returns_error() {
        let rt = make_rt().await;
        let tok = NamespaceToken::local();
        let bad = KgArchive {
            format: "khive-kg".to_string(),
            version: "999.0".to_string(),
            namespace: "local".to_string(),
            exported_at: Utc::now(),
            entities: vec![],
            edges: vec![],
        };
        let err = rt.import_kg(&bad, &tok).await.unwrap_err();
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

    /// ADR-115 Amendment 1 §3: an archive entity carrying the reserved
    /// `khive:secret_gate` property key must be rejected before any upsert —
    /// import is a caller-controlled write path and stays reservation-only.
    #[tokio::test]
    async fn import_entity_with_reserved_secret_gate_property_is_rejected() {
        let rt = make_rt().await;
        let tok = NamespaceToken::local();
        let archive = KgArchive {
            format: "khive-kg".to_string(),
            version: "0.1".to_string(),
            namespace: "local".to_string(),
            exported_at: Utc::now(),
            entities: vec![ExportedEntity {
                id: Uuid::new_v4(),
                kind: "concept".to_string(),
                entity_type: None,
                name: "ReservedKeyImport".to_string(),
                description: None,
                properties: Some(serde_json::json!({
                    "khive:secret_gate": "exempted:content-sha256-manifest-v1"
                })),
                tags: vec![],
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }],
            edges: vec![],
        };

        let err = rt.import_kg(&archive, &tok).await.unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("khive:secret_gate") && msg.contains("runtime-owned")),
            "expected a reservation rejection, got {err:?}"
        );

        let entities = rt.list_entities(&tok, None, None, 100, 0).await.unwrap();
        assert!(
            !entities.iter().any(|e| e.name == "ReservedKeyImport"),
            "rejected archive import must not create the entity"
        );
    }

    /// 6. Invalid relation in archive → InvalidInput.
    #[test]
    fn invalid_relation_rejected_at_deserialize() {
        let json = r#"{
            "format":"khive-kg","version":"0.1","namespace":"local",
            "exported_at":"2026-01-01T00:00:00Z",
            "entities":[],
            "edges":[{"edge_id":"00000000-0000-0000-0000-000000000099",
                       "source":"00000000-0000-0000-0000-000000000001",
                       "target":"00000000-0000-0000-0000-000000000002",
                       "relation":"related_to","weight":0.5}]
        }"#;
        let result: Result<KgArchive, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "non-canonical relation should fail to deserialize"
        );
    }

    // ── Dangling-edge validation tests ────────────────────────────────────────

    /// 6. Edge with dangling source (source UUID not in entity table) is skipped.
    ///
    /// The archive has one entity + one edge whose source is a phantom UUID.
    /// Import succeeds, entities_imported=1, edges_imported=0, edges_skipped=1.
    #[tokio::test]
    async fn import_edge_with_dangling_source_is_skipped() {
        let phantom_source = Uuid::parse_str("deadbeef-dead-4ead-dead-deadbeefcafe").unwrap();

        let rt = make_rt().await;
        let tok = NamespaceToken::local();
        let real = rt
            .create_entity(&tok, "concept", None, "Real", None, None, vec![])
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
                entity_type: None,
                name: "Real".to_string(),
                description: None,
                properties: None,
                tags: vec![],
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }],
            edges: vec![ExportedEdge {
                edge_id: Uuid::new_v4(),
                source: phantom_source,
                target: real.id,
                relation: EdgeRelation::Extends,
                weight: 1.0,
                properties: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }],
        };

        let dst = make_rt().await;
        let summary = dst.import_kg(&archive, &tok).await.unwrap();
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
        let tok = NamespaceToken::local();
        let real = rt
            .create_entity(&tok, "concept", None, "Source", None, None, vec![])
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
                entity_type: None,
                name: "Source".to_string(),
                description: None,
                properties: None,
                tags: vec![],
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }],
            edges: vec![ExportedEdge {
                edge_id: Uuid::new_v4(),
                source: real.id,
                target: phantom_target,
                relation: EdgeRelation::DependsOn,
                weight: 0.8,
                properties: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }],
        };

        let dst = make_rt().await;
        let summary = dst.import_kg(&archive, &tok).await.unwrap();
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
        let tok = NamespaceToken::local();
        let a = src
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = src
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let c = src
            .create_entity(&tok, "concept", None, "C", None, None, vec![])
            .await
            .unwrap();

        let archive = KgArchive {
            format: "khive-kg".to_string(),
            version: "0.1".to_string(),
            namespace: "local".to_string(),
            exported_at: Utc::now(),
            entities: vec![
                ExportedEntity {
                    id: a.id,
                    kind: "concept".to_string(),
                    entity_type: None,
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
                    entity_type: None,
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
                    entity_type: None,
                    name: "C".to_string(),
                    description: None,
                    properties: None,
                    tags: vec![],
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                },
            ],
            edges: vec![
                ExportedEdge {
                    edge_id: Uuid::new_v4(),
                    source: a.id,
                    target: b.id,
                    relation: EdgeRelation::Extends,
                    weight: 1.0,
                    properties: None,
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                },
                ExportedEdge {
                    edge_id: Uuid::new_v4(),
                    source: b.id,
                    target: c.id,
                    // #1235: concept->concept is not in depends_on's endpoint
                    // contract; use variant_of (concept->concept is valid) so
                    // this fixture exercises "one dangling edge skipped" only,
                    // not an endpoint-contract violation this test isn't about.
                    relation: EdgeRelation::VariantOf,
                    weight: 0.9,
                    properties: None,
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                },
                ExportedEdge {
                    edge_id: Uuid::new_v4(),
                    source: a.id,
                    target: phantom,
                    relation: EdgeRelation::Enables,
                    weight: 0.5,
                    properties: None,
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                },
            ],
        };

        let dst = make_rt().await;
        let summary = dst.import_kg(&archive, &tok).await.unwrap();
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
        let tok = NamespaceToken::local();
        let e1 = src
            .create_entity(&tok, "concept", None, "E1", None, None, vec![])
            .await
            .unwrap();
        let e2 = src
            .create_entity(&tok, "concept", None, "E2", None, None, vec![])
            .await
            .unwrap();
        src.link(&tok, e1.id, e2.id, EdgeRelation::VariantOf, 0.7, None)
            .await
            .unwrap();

        let archive = src.export_kg(&tok).await.unwrap();
        let dst = make_rt().await;
        let summary = dst.import_kg(&archive, &tok).await.unwrap();
        assert_eq!(summary.edges_imported, 1);
        assert_eq!(
            summary.edges_skipped, 0,
            "no edges should be skipped when all endpoints exist"
        );
    }

    /// #1235: an edge whose (source kind, relation, target kind) triple violates the
    /// endpoint contract (here: `precedes` between two `concept` entities — `precedes`
    /// is restricted to document/dataset/artifact/service/project pairs) must be
    /// skipped by import the same way a dangling endpoint is, not written straight
    /// through with `graph.upsert_edge`.
    #[tokio::test]
    async fn import_edge_violating_endpoint_contract_is_skipped() {
        let src = make_rt().await;
        let tok = NamespaceToken::local();
        let e1 = src
            .create_entity(&tok, "concept", None, "E1", None, None, vec![])
            .await
            .unwrap();
        let e2 = src
            .create_entity(&tok, "concept", None, "E2", None, None, vec![])
            .await
            .unwrap();

        let archive = KgArchive {
            format: "khive-kg".to_string(),
            version: "0.1".to_string(),
            namespace: "local".to_string(),
            exported_at: Utc::now(),
            entities: vec![
                ExportedEntity {
                    id: e1.id,
                    kind: "concept".to_string(),
                    entity_type: None,
                    name: "E1".to_string(),
                    description: None,
                    properties: None,
                    tags: vec![],
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                },
                ExportedEntity {
                    id: e2.id,
                    kind: "concept".to_string(),
                    entity_type: None,
                    name: "E2".to_string(),
                    description: None,
                    properties: None,
                    tags: vec![],
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                },
            ],
            edges: vec![ExportedEdge {
                edge_id: Uuid::new_v4(),
                source: e1.id,
                target: e2.id,
                relation: EdgeRelation::Precedes,
                weight: 1.0,
                properties: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }],
        };

        let dst = make_rt().await;
        let summary = dst.import_kg(&archive, &tok).await.unwrap();
        assert_eq!(summary.entities_imported, 2);
        assert_eq!(
            summary.edges_imported, 0,
            "an endpoint-contract-violating edge must not be imported"
        );
        assert_eq!(
            summary.edges_skipped, 1,
            "an endpoint-contract-violating edge must be counted as skipped"
        );
        assert!(
            dst.neighbors(
                &tok,
                e1.id,
                khive_storage::types::Direction::Out,
                None,
                None
            )
            .await
            .unwrap()
            .is_empty(),
            "the contract-violating edge must not exist in the destination graph"
        );
    }

    // ── edge_id contract tests ────────────────────────────────────────────────

    /// 10. export_kg sets edge_id in the archive to the LinkId returned by link.
    #[tokio::test]
    async fn export_kg_preserves_edge_id() {
        let rt = make_rt().await;
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "Alpha", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "Beta", None, None, vec![])
            .await
            .unwrap();
        let stored_edge = rt
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let stored_id: Uuid = stored_edge.id.into();

        let archive = rt.export_kg(&tok).await.unwrap();
        assert_eq!(archive.edges.len(), 1);
        assert_eq!(
            archive.edges[0].edge_id, stored_id,
            "exported edge_id must equal the LinkId returned by link"
        );
    }

    /// 11. import_kg writes the archive edge identity and timestamps exactly.
    #[tokio::test]
    async fn import_kg_persists_edge_id_and_timestamps() {
        let src = make_rt().await;
        let tok = NamespaceToken::local();
        let a = src
            .create_entity(&tok, "concept", None, "Alpha", None, None, vec![])
            .await
            .unwrap();
        let b = src
            .create_entity(&tok, "concept", None, "Beta", None, None, vec![])
            .await
            .unwrap();
        let stored_edge = src
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let original_id: Uuid = stored_edge.id.into();

        let expected_created = chrono::DateTime::parse_from_rfc3339("2026-03-03T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let expected_updated = chrono::DateTime::parse_from_rfc3339("2026-04-04T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut archive = src.export_kg(&tok).await.unwrap();
        archive.edges[0].created_at = expected_created;
        archive.edges[0].updated_at = expected_updated;
        archive.edges[0].properties = Some(serde_json::json!({"confidence": 0.95}));
        let dst = make_rt().await;
        dst.import_kg(&archive, &tok).await.unwrap();

        let imported_edge = dst.get_edge(&tok, original_id).await.unwrap();
        assert!(
            imported_edge.is_some(),
            "imported edge must be retrievable by the original edge_id"
        );
        let imported_edge = imported_edge.unwrap();
        assert_eq!(
            Uuid::from(imported_edge.id),
            original_id,
            "stored edge id must equal the archive edge_id"
        );
        assert_eq!(
            imported_edge.created_at, expected_created,
            "present edge created_at must not be replaced with import time"
        );
        assert_eq!(
            imported_edge.updated_at, expected_updated,
            "present edge updated_at must not be replaced with created_at or import time"
        );
        assert_eq!(
            imported_edge.metadata,
            Some(serde_json::json!({"confidence": 0.95})),
            "edge properties must persist as storage metadata"
        );

        let reexported = dst.export_kg(&tok).await.unwrap();
        assert_eq!(
            reexported.edges[0].properties,
            Some(serde_json::json!({"confidence": 0.95})),
            "edge metadata must re-export as portable properties"
        );
    }

    /// 12. Old archive (no edge_id field) deserializes, imports, and re-exports with the
    ///     same generated UUID — proving the generated ID survives the full round trip.
    ///
    ///     The fixture includes two entities so the edge is not skipped during import.
    #[tokio::test]
    async fn old_archive_missing_edge_id_round_trips() {
        let src_id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let tgt_id = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();

        // Simulate a pre-0.2 archive JSON where the edge lacks an edge_id field.
        let json = format!(
            r#"{{
                "format": "khive-kg",
                "version": "0.1",
                "namespace": "local",
                "exported_at": "2026-01-01T00:00:00Z",
                "entities": [
                    {{"id":"{src_id}","kind":"concept","name":"SrcNode","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}},
                    {{"id":"{tgt_id}","kind":"concept","name":"TgtNode","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}}
                ],
                "edges": [
                    {{
                        "source": "{src_id}",
                        "target": "{tgt_id}",
                        "relation": "extends",
                        "weight": 0.9
                    }}
                ]
            }}"#
        );

        // serde(default) must assign a fresh non-nil UUID when edge_id is absent.
        let archive: KgArchive = serde_json::from_str(&json)
            .expect("old archive without edge_id must deserialize successfully");
        assert_eq!(archive.edges.len(), 1);
        let generated_id = archive.edges[0].edge_id;
        assert_ne!(
            generated_id,
            Uuid::nil(),
            "missing edge_id in old archive must get a fresh non-nil UUID"
        );

        let rt = make_rt().await;
        let tok = NamespaceToken::local();
        let summary = rt.import_kg(&archive, &tok).await.unwrap();
        assert_eq!(summary.entities_imported, 2);
        assert_eq!(
            summary.edges_imported, 1,
            "edge must be imported when both endpoints exist"
        );

        let stored = rt.get_edge(&tok, generated_id).await.unwrap();
        assert!(
            stored.is_some(),
            "imported edge must be retrievable by the generated edge_id"
        );
        assert_eq!(
            Uuid::from(stored.unwrap().id),
            generated_id,
            "stored edge id must equal the generated edge_id"
        );

        let re_archive = rt.export_kg(&tok).await.unwrap();
        assert_eq!(re_archive.edges.len(), 1);
        assert_eq!(
            re_archive.edges[0].edge_id, generated_id,
            "re-exported edge_id must equal the ID generated on first import"
        );
    }

    /// 13. Explicit export → import → export equality: the edge_id is unchanged across
    ///     a full round trip when the source archive already contains an edge_id.
    ///
    ///     Verifies by (source, target, relation) key that re-export emits the original ID.
    #[tokio::test]
    async fn export_import_export_edge_id_equality() {
        let src = make_rt().await;
        let tok = NamespaceToken::local();
        let a = src
            .create_entity(&tok, "concept", None, "NodeA", None, None, vec![])
            .await
            .unwrap();
        let b = src
            .create_entity(&tok, "concept", None, "NodeB", None, None, vec![])
            .await
            .unwrap();
        let stored = src
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let original_edge_id: Uuid = stored.id.into();

        let archive1 = src.export_kg(&tok).await.unwrap();
        assert_eq!(archive1.edges.len(), 1);
        assert_eq!(
            archive1.edges[0].edge_id, original_edge_id,
            "first export must carry the stored edge_id"
        );

        let dst = make_rt().await;
        dst.import_kg(&archive1, &tok).await.unwrap();

        let archive2 = dst.export_kg(&tok).await.unwrap();
        assert_eq!(archive2.edges.len(), 1);

        let re_edge = archive2
            .edges
            .iter()
            .find(|e| e.source == a.id && e.target == b.id && e.relation == EdgeRelation::Extends)
            .expect(
                "re-exported archive must contain the original edge by (source,target,relation)",
            );
        assert_eq!(
            re_edge.edge_id, original_edge_id,
            "edge_id must be identical across export → import → export"
        );
    }
}
