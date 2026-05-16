// Licensed under the Apache License, Version 2.0.

//! Curation operations: entity update/merge and edge-list filter type.
//!
//! See ADR-014 for the full specification and semantics.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use khive_storage::types::{
    DeleteMode, EdgeFilter, EdgeSortField, LinkId, PageRequest, SortOrder, TextDocument,
};
use khive_storage::{Edge, EdgeRelation, Entity, SubstrateKind};

use crate::error::{RuntimeError, RuntimeResult};
use crate::runtime::KhiveRuntime;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Patch for `update_entity`. Only `Some(_)` fields are applied; `None` means "leave unchanged".
///
/// For `description`:
/// - `None` (outer) — leave the current description as-is
/// - `Some(None)` — clear the description (set to NULL)
/// - `Some(Some(s))` — set the description to `s`
#[derive(Clone, Debug, Default)]
pub struct EntityPatch {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub properties: Option<Value>,
    pub tags: Option<Vec<String>>,
}

/// Strategy used when merging two entities.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeStrategy {
    /// `into` values win on conflict. Tags are unioned. Properties from `from` fill in
    /// keys that `into` doesn't have. This is the default.
    #[default]
    PreferInto,
    /// `from` values win on conflict.
    PreferFrom,
    /// Deep-merge: object properties merge recursively. Scalar conflicts go to `into`.
    Union,
}

/// Result returned by `merge_entity`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeSummary {
    pub kept_id: Uuid,
    pub removed_id: Uuid,
    pub edges_rewired: usize,
    pub properties_merged: usize,
    pub tags_unioned: usize,
}

/// Filter for `list_edges` / `count_edges`.
#[derive(Clone, Debug, Default)]
pub struct EdgeListFilter {
    pub source_id: Option<Uuid>,
    pub target_id: Option<Uuid>,
    /// Empty = any relation.
    pub relations: Vec<EdgeRelation>,
    pub min_weight: Option<f64>,
    pub max_weight: Option<f64>,
}

impl From<EdgeListFilter> for EdgeFilter {
    fn from(f: EdgeListFilter) -> Self {
        EdgeFilter {
            source_ids: f.source_id.into_iter().collect(),
            target_ids: f.target_id.into_iter().collect(),
            relations: f.relations,
            min_weight: f.min_weight,
            max_weight: f.max_weight,
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl KhiveRuntime {
    /// Patch-style entity update.
    ///
    /// Only fields set to `Some(_)` are changed. Re-indexes FTS5 (and vectors if configured)
    /// when `name` or `description` changes; skips re-indexing for property/tag-only patches.
    pub async fn update_entity(
        &self,
        namespace: Option<&str>,
        id: Uuid,
        patch: EntityPatch,
    ) -> RuntimeResult<Entity> {
        let store = self.entities(namespace)?;
        let mut entity = store
            .get_entity(id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound(format!("entity {id}")))?;

        let mut text_changed = false;

        if let Some(name) = patch.name {
            text_changed |= entity.name != name;
            entity.name = name;
        }
        if let Some(desc_patch) = patch.description {
            text_changed |= entity.description != desc_patch;
            entity.description = desc_patch;
        }
        if let Some(props) = patch.properties {
            entity.properties = Some(props);
        }
        if let Some(tags) = patch.tags {
            entity.tags = tags;
        }

        entity.updated_at = chrono::Utc::now().timestamp_micros();
        store.upsert_entity(entity.clone()).await?;

        if text_changed {
            self.reindex_entity(namespace, &entity).await?;
        }

        Ok(entity)
    }

    /// Merge `from_id` into `into_id`.
    ///
    /// All edges incident to `from_id` are rewired to `into_id`. Self-loops that would
    /// result from the rewire are dropped. Properties and tags are merged per `strategy`.
    /// `from_id` is hard-deleted and removed from indexes. Returns a summary.
    ///
    /// Not transactional in v0.1 — idempotent enough to re-run if interrupted mid-way.
    pub async fn merge_entity(
        &self,
        namespace: Option<&str>,
        into_id: Uuid,
        from_id: Uuid,
        strategy: MergeStrategy,
    ) -> RuntimeResult<MergeSummary> {
        let store = self.entities(namespace)?;
        let graph = self.graph(namespace)?;

        let into_entity = store
            .get_entity(into_id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound(format!("entity {into_id}")))?;
        let from_entity = store
            .get_entity(from_id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound(format!("entity {from_id}")))?;

        // Collect all edges incident to from_id (as source OR target).
        let incident_filter = EdgeFilter {
            source_ids: vec![from_id],
            ..Default::default()
        };
        let outbound = graph
            .query_edges(
                incident_filter,
                vec![SortOrder {
                    field: EdgeSortField::CreatedAt,
                    direction: khive_storage::types::SortDirection::Asc,
                }],
                PageRequest {
                    offset: 0,
                    limit: 10_000,
                },
            )
            .await?
            .items;

        let inbound_filter = EdgeFilter {
            target_ids: vec![from_id],
            ..Default::default()
        };
        let inbound = graph
            .query_edges(
                inbound_filter,
                vec![SortOrder {
                    field: EdgeSortField::CreatedAt,
                    direction: khive_storage::types::SortDirection::Asc,
                }],
                PageRequest {
                    offset: 0,
                    limit: 10_000,
                },
            )
            .await?
            .items;

        // Rewire edges, dropping any that would become self-loops.
        let mut edges_rewired = 0usize;
        for edge in outbound {
            let new_source = into_id;
            let new_target: Uuid = edge.target_id;
            if new_source == new_target {
                graph.delete_edge(edge.id).await?;
                continue;
            }
            let rewired = Edge {
                source_id: LinkId::from(new_source).into(),
                ..edge
            };
            graph.upsert_edge(rewired).await?;
            edges_rewired += 1;
        }
        for edge in inbound {
            let new_target = into_id;
            let new_source: Uuid = edge.source_id;
            if new_source == new_target {
                graph.delete_edge(edge.id).await?;
                continue;
            }
            let rewired = Edge {
                target_id: LinkId::from(new_target).into(),
                ..edge
            };
            graph.upsert_edge(rewired).await?;
            edges_rewired += 1;
        }

        // Merge properties.
        let (merged_props, properties_merged) =
            merge_properties(&into_entity.properties, &from_entity.properties, strategy);

        // Merge description and name per strategy.
        let merged_name = merge_string_field(&into_entity.name, &from_entity.name, strategy);
        let merged_description =
            merge_option_string_field(&into_entity.description, &from_entity.description, strategy);

        // Union tags.
        let (merged_tags, tags_unioned) = union_tags(&into_entity.tags, &from_entity.tags);

        // Upsert updated into entity.
        let mut updated_into = into_entity;
        updated_into.name = merged_name;
        updated_into.description = merged_description;
        updated_into.properties = merged_props;
        updated_into.tags = merged_tags;
        updated_into.updated_at = chrono::Utc::now().timestamp_micros();
        store.upsert_entity(updated_into.clone()).await?;
        self.reindex_entity(namespace, &updated_into).await?;

        // Hard-delete from entity and remove from indexes.
        store.delete_entity(from_id, DeleteMode::Hard).await?;
        self.remove_from_indexes(namespace, from_id).await?;

        Ok(MergeSummary {
            kept_id: into_id,
            removed_id: from_id,
            edges_rewired,
            properties_merged,
            tags_unioned,
        })
    }

    // ---- Internal helpers ----

    /// Re-upsert FTS5 document (and vector if model configured) for the entity.
    pub(crate) async fn reindex_entity(
        &self,
        namespace: Option<&str>,
        entity: &Entity,
    ) -> RuntimeResult<()> {
        let body = match &entity.description {
            Some(d) if !d.is_empty() => format!("{} {}", entity.name, d),
            _ => entity.name.clone(),
        };
        let ns = self.ns(namespace).to_string();
        self.text(namespace)?
            .upsert_document(TextDocument {
                subject_id: entity.id,
                kind: SubstrateKind::Entity,
                title: Some(entity.name.clone()),
                body: body.clone(),
                tags: entity.tags.clone(),
                namespace: ns.clone(),
                metadata: entity.properties.clone(),
                updated_at: chrono::Utc::now(),
            })
            .await?;

        if self.config().embedding_model.is_some() {
            let vector = self.embed(&body).await?;
            self.vectors(namespace)?
                .insert(entity.id, SubstrateKind::Entity, &ns, vector)
                .await?;
        }

        Ok(())
    }

    /// Remove an entity from FTS5 and (if configured) vector indexes.
    async fn remove_from_indexes(&self, namespace: Option<&str>, id: Uuid) -> RuntimeResult<()> {
        let ns = self.ns(namespace).to_string();
        self.text(namespace)?.delete_document(&ns, id).await?;
        if self.config().embedding_model.is_some() {
            self.vectors(namespace)?.delete(id).await?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Merge helpers (pure functions — easier to unit test)
// ---------------------------------------------------------------------------

fn merge_string_field(into: &str, from: &str, strategy: MergeStrategy) -> String {
    match strategy {
        MergeStrategy::PreferInto | MergeStrategy::Union => into.to_string(),
        MergeStrategy::PreferFrom => from.to_string(),
    }
}

fn merge_option_string_field(
    into: &Option<String>,
    from: &Option<String>,
    strategy: MergeStrategy,
) -> Option<String> {
    match strategy {
        MergeStrategy::PreferInto => {
            if into.is_some() {
                into.clone()
            } else {
                from.clone()
            }
        }
        MergeStrategy::PreferFrom => {
            if from.is_some() {
                from.clone()
            } else {
                into.clone()
            }
        }
        MergeStrategy::Union => {
            // Keep into's description; if empty, append from's.
            match (into, from) {
                (Some(a), _) if !a.is_empty() => Some(a.clone()),
                (_, Some(b)) => Some(b.clone()),
                _ => None,
            }
        }
    }
}

/// Merge two property objects. Returns (merged, count_of_fields_from_from_that_were_added).
fn merge_properties(
    into: &Option<Value>,
    from: &Option<Value>,
    strategy: MergeStrategy,
) -> (Option<Value>, usize) {
    match (into, from) {
        (None, None) => (None, 0),
        (Some(a), None) => (Some(a.clone()), 0),
        (None, Some(b)) => {
            let count = if let Value::Object(m) = b { m.len() } else { 1 };
            (Some(b.clone()), count)
        }
        (Some(into_val), Some(from_val)) => {
            let (merged, added) = merge_json(into_val, from_val, strategy);
            (Some(merged), added)
        }
    }
}

/// Deep-merge two JSON values per strategy. Returns (merged, keys_contributed_by_from).
fn merge_json(into: &Value, from: &Value, strategy: MergeStrategy) -> (Value, usize) {
    match (into, from, strategy) {
        (Value::Object(a), Value::Object(b), MergeStrategy::Union) => {
            let mut result = a.clone();
            let mut added = 0usize;
            for (k, v_from) in b {
                if let Some(v_into) = a.get(k) {
                    let (merged, sub_added) = merge_json(v_into, v_from, MergeStrategy::Union);
                    result.insert(k.clone(), merged);
                    added += sub_added;
                } else {
                    result.insert(k.clone(), v_from.clone());
                    added += 1;
                }
            }
            (Value::Object(result), added)
        }
        (Value::Object(a), Value::Object(b), MergeStrategy::PreferInto) => {
            let mut result = a.clone();
            let mut added = 0usize;
            for (k, v) in b {
                if !a.contains_key(k) {
                    result.insert(k.clone(), v.clone());
                    added += 1;
                }
            }
            (Value::Object(result), added)
        }
        (Value::Object(a), Value::Object(b), MergeStrategy::PreferFrom) => {
            let mut result = a.clone();
            let mut added = 0usize;
            for (k, v) in b {
                result.insert(k.clone(), v.clone());
                if !a.contains_key(k) {
                    added += 1;
                }
            }
            (Value::Object(result), added)
        }
        // Non-object scalars: apply strategy directly.
        (_into_val, from_val, MergeStrategy::PreferFrom) => (from_val.clone(), 1),
        _ => (into.clone(), 0),
    }
}

fn union_tags(into: &[String], from: &[String]) -> (Vec<String>, usize) {
    let mut seen: HashSet<&str> = into.iter().map(|s| s.as_str()).collect();
    let mut result: Vec<String> = into.to_vec();
    let mut added = 0usize;
    for tag in from {
        if seen.insert(tag.as_str()) {
            result.push(tag.clone());
            added += 1;
        }
    }
    (result, added)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::KhiveRuntime;
    use khive_storage::types::{Direction, TextFilter, TextQueryMode, TextSearchRequest};

    fn rt() -> KhiveRuntime {
        KhiveRuntime::memory().unwrap()
    }

    // Helper: search FTS5 for `query` in a runtime namespace.
    async fn fts_hit(rt: &KhiveRuntime, namespace: Option<&str>, query: &str) -> Vec<Uuid> {
        let ns = rt.ns(namespace).to_string();
        rt.text(namespace)
            .unwrap()
            .search(TextSearchRequest {
                query: query.to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    namespaces: vec![ns],
                    ..Default::default()
                }),
                top_k: 50,
                snippet_chars: 100,
            })
            .await
            .unwrap()
            .into_iter()
            .map(|h| h.subject_id)
            .collect()
    }

    #[tokio::test]
    async fn update_entity_patch_changes_only_specified_fields() {
        let rt = rt();
        let entity = rt
            .create_entity(
                None,
                "concept",
                "OriginalName",
                Some("orig desc"),
                Some(serde_json::json!({"k":"v"})),
                vec![],
            )
            .await
            .unwrap();

        let updated = rt
            .update_entity(
                None,
                entity.id,
                EntityPatch {
                    description: Some(Some("new desc".to_string())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.name, "OriginalName");
        assert_eq!(updated.description.as_deref(), Some("new desc"));
        assert_eq!(updated.properties, Some(serde_json::json!({"k":"v"})));
    }

    #[tokio::test]
    async fn update_entity_clear_description_with_some_none() {
        let rt = rt();
        let entity = rt
            .create_entity(
                None,
                "concept",
                "ClearDesc",
                Some("has description"),
                None,
                vec![],
            )
            .await
            .unwrap();

        let updated = rt
            .update_entity(
                None,
                entity.id,
                EntityPatch {
                    description: Some(None),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert!(
            updated.description.is_none(),
            "description should be cleared"
        );
    }

    #[tokio::test]
    async fn update_entity_reindexes_when_name_changes() {
        let rt = rt();
        let entity = rt
            .create_entity(None, "concept", "OldName", None, None, vec![])
            .await
            .unwrap();

        // Old name is findable.
        let hits_before = fts_hit(&rt, None, "OldName").await;
        assert!(
            hits_before.contains(&entity.id),
            "entity should be findable by old name"
        );

        rt.update_entity(
            None,
            entity.id,
            EntityPatch {
                name: Some("NewName".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let hits_old = fts_hit(&rt, None, "OldName").await;
        let hits_new = fts_hit(&rt, None, "NewName").await;

        // After rename, old name no longer matches this entity (FTS index updated).
        assert!(
            !hits_old.contains(&entity.id),
            "old name should no longer match after rename"
        );
        assert!(
            hits_new.contains(&entity.id),
            "new name should be findable after rename"
        );
    }

    #[tokio::test]
    async fn update_entity_skips_reindex_when_only_properties_change() {
        let rt = rt();
        let entity = rt
            .create_entity(None, "concept", "StableIndexed", None, None, vec![])
            .await
            .unwrap();

        // Verify it's in the index before.
        let hits_before = fts_hit(&rt, None, "StableIndexed").await;
        assert!(hits_before.contains(&entity.id));

        // Only patch properties — text index should be untouched (still findable).
        rt.update_entity(
            None,
            entity.id,
            EntityPatch {
                properties: Some(serde_json::json!({"new": "prop"})),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let hits_after = fts_hit(&rt, None, "StableIndexed").await;
        assert!(
            hits_after.contains(&entity.id),
            "still findable after props-only patch"
        );
    }

    #[tokio::test]
    async fn merge_entity_rewires_edges() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(None, "concept", "C", None, None, vec![])
            .await
            .unwrap();
        let d = rt
            .create_entity(None, "concept", "D", None, None, vec![])
            .await
            .unwrap();

        // A→B and C→B; merge B into D → should become A→D and C→D.
        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        rt.link(None, c.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();

        let summary = rt
            .merge_entity(None, d.id, b.id, MergeStrategy::PreferInto)
            .await
            .unwrap();

        assert_eq!(summary.kept_id, d.id);
        assert_eq!(summary.removed_id, b.id);
        assert_eq!(summary.edges_rewired, 2);

        // Verify edges now point to D.
        let a_neighbors = rt
            .neighbors(None, a.id, Direction::Out, None)
            .await
            .unwrap();
        assert_eq!(a_neighbors.len(), 1);
        assert_eq!(a_neighbors[0].node_id, d.id);

        let c_neighbors = rt
            .neighbors(None, c.id, Direction::Out, None)
            .await
            .unwrap();
        assert_eq!(c_neighbors.len(), 1);
        assert_eq!(c_neighbors[0].node_id, d.id);
    }

    #[tokio::test]
    async fn merge_entity_prefer_into_strategy() {
        let rt = rt();
        let into = rt
            .create_entity(
                None,
                "concept",
                "Into",
                None,
                Some(serde_json::json!({"a": 1})),
                vec![],
            )
            .await
            .unwrap();
        let from = rt
            .create_entity(
                None,
                "concept",
                "From",
                None,
                Some(serde_json::json!({"a": 2, "b": 3})),
                vec![],
            )
            .await
            .unwrap();

        rt.merge_entity(None, into.id, from.id, MergeStrategy::PreferInto)
            .await
            .unwrap();

        let kept = rt.get_entity(None, into.id).await.unwrap().unwrap();
        let props = kept.properties.unwrap();
        // a stays as 1 (into wins), b is added from from.
        assert_eq!(props["a"], 1);
        assert_eq!(props["b"], 3);
    }

    #[tokio::test]
    async fn merge_entity_prefer_from_strategy() {
        let rt = rt();
        let into = rt
            .create_entity(
                None,
                "concept",
                "Into",
                None,
                Some(serde_json::json!({"a": 1})),
                vec![],
            )
            .await
            .unwrap();
        let from = rt
            .create_entity(
                None,
                "concept",
                "From",
                None,
                Some(serde_json::json!({"a": 2, "b": 3})),
                vec![],
            )
            .await
            .unwrap();

        rt.merge_entity(None, into.id, from.id, MergeStrategy::PreferFrom)
            .await
            .unwrap();

        let kept = rt.get_entity(None, into.id).await.unwrap().unwrap();
        let props = kept.properties.unwrap();
        // from wins on a, b also from from.
        assert_eq!(props["a"], 2);
        assert_eq!(props["b"], 3);
    }

    #[tokio::test]
    async fn merge_entity_union_strategy() {
        let rt = rt();
        let into = rt
            .create_entity(
                None,
                "concept",
                "Into",
                None,
                Some(serde_json::json!({"a": 1})),
                vec![],
            )
            .await
            .unwrap();
        let from = rt
            .create_entity(
                None,
                "concept",
                "From",
                None,
                Some(serde_json::json!({"a": 2, "b": 3})),
                vec![],
            )
            .await
            .unwrap();

        rt.merge_entity(None, into.id, from.id, MergeStrategy::Union)
            .await
            .unwrap();

        let kept = rt.get_entity(None, into.id).await.unwrap().unwrap();
        let props = kept.properties.unwrap();
        // Scalar conflict: into wins → a=1. b added from from.
        assert_eq!(props["a"], 1);
        assert_eq!(props["b"], 3);
    }

    #[tokio::test]
    async fn merge_entity_unions_tags() {
        let rt = rt();
        let into = rt
            .create_entity(
                None,
                "concept",
                "Into",
                None,
                None,
                vec!["x".to_string(), "y".to_string()],
            )
            .await
            .unwrap();
        let from = rt
            .create_entity(
                None,
                "concept",
                "From",
                None,
                None,
                vec!["y".to_string(), "z".to_string()],
            )
            .await
            .unwrap();

        rt.merge_entity(None, into.id, from.id, MergeStrategy::PreferInto)
            .await
            .unwrap();

        let kept = rt.get_entity(None, into.id).await.unwrap().unwrap();
        let mut tags = kept.tags.clone();
        tags.sort();
        assert_eq!(tags, vec!["x", "y", "z"]);
    }

    #[tokio::test]
    async fn merge_entity_drops_self_loops() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();

        // A `extends` B — merging B into A would produce A `extends` A → drop it.
        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();

        let summary = rt
            .merge_entity(None, a.id, b.id, MergeStrategy::PreferInto)
            .await
            .unwrap();

        assert_eq!(
            summary.edges_rewired, 0,
            "self-loop should be dropped, not rewired"
        );

        let a_out = rt
            .neighbors(None, a.id, Direction::Out, None)
            .await
            .unwrap();
        assert!(a_out.is_empty(), "no self-loop should remain");
    }

    // ---- merge helper unit tests ----

    #[test]
    fn union_tags_deduplicates() {
        let (tags, added) = union_tags(
            &["x".to_string(), "y".to_string()],
            &["y".to_string(), "z".to_string()],
        );
        let mut sorted = tags.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["x", "y", "z"]);
        assert_eq!(added, 1);
    }

    #[test]
    fn merge_properties_prefer_into_fills_missing_keys() {
        let a = serde_json::json!({"a": 1});
        let b = serde_json::json!({"a": 99, "b": 2});
        let (merged, added) = merge_properties(&Some(a), &Some(b), MergeStrategy::PreferInto);
        let m = merged.unwrap();
        assert_eq!(m["a"], 1);
        assert_eq!(m["b"], 2);
        assert_eq!(added, 1);
    }
}
