//! High-level operations composing storage capabilities into user-facing verbs.

use std::collections::HashMap;
use std::str::FromStr;

use uuid::Uuid;

use khive_score::{rrf_score, DeterministicScore};
use khive_storage::note::{Note, NoteKind};
use khive_storage::types::{
    DeleteMode, Direction, EdgeSortField, GraphPath, LinkId, NeighborHit, NeighborQuery,
    PageRequest, SortOrder, SqlStatement, TextDocument, TextFilter, TextQueryMode,
    TextSearchRequest, TraversalRequest, VectorSearchRequest,
};
use khive_storage::{Edge, EdgeRelation, Entity, EntityFilter, Event};
use khive_types::{EntityKind, SubstrateKind};

use crate::error::{RuntimeError, RuntimeResult};
use crate::runtime::KhiveRuntime;

/// A note search result with UUID and salience-weighted RRF score.
#[derive(Clone, Debug)]
pub struct NoteSearchHit {
    pub note_id: Uuid,
    pub score: DeterministicScore,
}

/// Result of resolving a UUID to its substrate kind.
#[derive(Clone, Debug)]
pub enum Resolved {
    Entity(Entity),
    Note(Note),
    Event(Event),
}

impl KhiveRuntime {
    // ---- Entity operations ----

    /// Create and persist a new entity.
    pub async fn create_entity(
        &self,
        namespace: Option<&str>,
        kind: &str,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
    ) -> RuntimeResult<Entity> {
        let ns = self.ns(namespace);
        let entity_kind = EntityKind::from_str(kind).map_err(RuntimeError::InvalidInput)?;
        let mut entity = Entity::new(ns, entity_kind, name);
        if let Some(d) = description {
            entity = entity.with_description(d);
        }
        if let Some(p) = properties {
            entity = entity.with_properties(p);
        }
        if !tags.is_empty() {
            entity = entity.with_tags(tags);
        }
        self.entities(Some(ns))?
            .upsert_entity(entity.clone())
            .await?;

        let body = match &entity.description {
            Some(d) if !d.is_empty() => format!("{} {}", entity.name, d),
            _ => entity.name.clone(),
        };
        self.text(namespace)?
            .upsert_document(TextDocument {
                subject_id: entity.id,
                kind: SubstrateKind::Entity,
                title: Some(entity.name.clone()),
                body: body.clone(),
                tags: entity.tags.clone(),
                namespace: ns.to_string(),
                metadata: entity.properties.clone(),
                updated_at: chrono::Utc::now(),
            })
            .await?;

        if self.config().embedding_model.is_some() {
            let vector = self.embed(&body).await?;
            self.vectors(namespace)?
                .insert(entity.id, SubstrateKind::Entity, ns, vector)
                .await?;
        }

        Ok(entity)
    }

    /// Retrieve an entity by ID.
    ///
    /// Returns `None` if the entity does not exist or belongs to a different namespace.
    /// This enforces ADR-007 namespace isolation at the runtime layer.
    pub async fn get_entity(
        &self,
        namespace: Option<&str>,
        id: Uuid,
    ) -> RuntimeResult<Option<Entity>> {
        let entity = match self.entities(namespace)?.get_entity(id).await? {
            Some(e) => e,
            None => return Ok(None),
        };
        if entity.namespace != self.ns(namespace) {
            return Ok(None);
        }
        Ok(Some(entity))
    }

    /// List entities in a namespace, optionally filtered by kind.
    pub async fn list_entities(
        &self,
        namespace: Option<&str>,
        kind: Option<&str>,
        limit: u32,
    ) -> RuntimeResult<Vec<Entity>> {
        let filter = EntityFilter {
            kinds: match kind {
                Some(k) => vec![EntityKind::from_str(k).map_err(RuntimeError::InvalidInput)?],
                None => vec![],
            },
            ..Default::default()
        };
        let page = self
            .entities(namespace)?
            .query_entities(self.ns(namespace), filter, PageRequest { offset: 0, limit })
            .await?;
        Ok(page.items)
    }

    // ---- Edge operations ----

    /// Create a directed edge between two entities.
    pub async fn link(
        &self,
        namespace: Option<&str>,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
        weight: f64,
    ) -> RuntimeResult<Edge> {
        let edge = Edge {
            id: LinkId::from(Uuid::new_v4()),
            source_id,
            target_id,
            relation,
            weight,
            created_at: chrono::Utc::now(),
            metadata: None,
        };
        self.graph(namespace)?.upsert_edge(edge.clone()).await?;
        Ok(edge)
    }

    /// Get immediate neighbors of a node, optionally filtered by relation type.
    ///
    /// Pass `relations: Some(vec![EdgeRelation::Annotates])` to retrieve only
    /// annotation edges, enabling cross-substrate navigation as described in ADR-024.
    pub async fn neighbors(
        &self,
        namespace: Option<&str>,
        node_id: Uuid,
        direction: Direction,
        limit: Option<u32>,
        relations: Option<Vec<EdgeRelation>>,
    ) -> RuntimeResult<Vec<NeighborHit>> {
        let query = NeighborQuery {
            direction,
            relations,
            limit,
            min_weight: None,
        };
        Ok(self.graph(namespace)?.neighbors(node_id, query).await?)
    }

    /// Traverse the graph from a set of root nodes.
    pub async fn traverse(
        &self,
        namespace: Option<&str>,
        request: TraversalRequest,
    ) -> RuntimeResult<Vec<GraphPath>> {
        Ok(self.graph(namespace)?.traverse(request).await?)
    }

    // ---- Note operations ----

    /// Create and persist a note, optionally with properties and annotation targets.
    ///
    /// After creating the note:
    /// - Always indexes into FTS5 at the `notes_<namespace>` key.
    /// - If an embedding model is configured, indexes into the vector store with
    ///   `SubstrateKind::Note`.
    /// - For each UUID in `annotates`, creates an `EdgeRelation::Annotates` edge from
    ///   the note to that target.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_note(
        &self,
        namespace: Option<&str>,
        kind: NoteKind,
        name: Option<&str>,
        content: &str,
        salience: f64,
        properties: Option<serde_json::Value>,
        annotates: Vec<Uuid>,
    ) -> RuntimeResult<Note> {
        let ns = self.ns(namespace);
        let mut note = Note::new(ns, kind, content).with_salience(salience);
        if let Some(n) = name {
            note = note.with_name(n);
        }
        if let Some(p) = properties {
            note = note.with_properties(p);
        }
        self.notes(Some(ns))?.upsert_note(note.clone()).await?;

        let body = match &note.name {
            Some(n) => format!("{n} {}", note.content),
            None => note.content.clone(),
        };

        // Index into FTS5.
        self.text_for_notes(Some(ns))?
            .upsert_document(TextDocument {
                subject_id: note.id,
                kind: SubstrateKind::Note,
                title: note.name.clone(),
                body,
                tags: vec![],
                namespace: ns.to_string(),
                metadata: note.properties.clone(),
                updated_at: chrono::Utc::now(),
            })
            .await?;

        // Index into vector store if model is configured.
        if self.config().embedding_model.is_some() {
            let vector = self.embed(&note.content).await?;
            self.vectors(Some(ns))?
                .insert(note.id, SubstrateKind::Note, ns, vector)
                .await?;
        }

        // Create annotates edges.
        for target_id in annotates {
            self.link(Some(ns), note.id, target_id, EdgeRelation::Annotates, 1.0)
                .await?;
        }

        Ok(note)
    }

    /// List notes, optionally filtered by kind.
    pub async fn list_notes(
        &self,
        namespace: Option<&str>,
        kind: Option<&str>,
        limit: u32,
    ) -> RuntimeResult<Vec<Note>> {
        let note_kind = match kind {
            Some(k) => Some(NoteKind::from_str(k).map_err(RuntimeError::InvalidInput)?),
            None => None,
        };
        let page = self
            .notes(namespace)?
            .query_notes(
                self.ns(namespace),
                note_kind,
                PageRequest { offset: 0, limit },
            )
            .await?;
        Ok(page.items)
    }

    /// Search notes using a hybrid FTS5 + vector pipeline with salience weighting.
    ///
    /// Pipeline (per ADR-024):
    /// 1. FTS5 query against `notes_<namespace>`.
    /// 2. If embedding model is configured: vector search filtered to `kind="note"`.
    /// 3. RRF fusion (k=60).
    /// 4. Salience-weighted rerank: `score *= (0.5 + 0.5 * note.salience)`.
    /// 5. Filter soft-deleted notes (`deleted_at IS NOT NULL`).
    /// 6. Truncate to `limit`.
    pub async fn search_notes(
        &self,
        namespace: Option<&str>,
        query_text: &str,
        query_vector: Option<Vec<f32>>,
        limit: u32,
    ) -> RuntimeResult<Vec<NoteSearchHit>> {
        const RRF_K: usize = 60;
        let candidates = limit.saturating_mul(4).max(limit);
        let ns = self.ns(namespace).to_string();

        // FTS5 over the notes index.
        let text_hits = self
            .text_for_notes(namespace)?
            .search(TextSearchRequest {
                query: query_text.to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    namespaces: vec![ns.clone()],
                    ..TextFilter::default()
                }),
                top_k: candidates,
                snippet_chars: 200,
            })
            .await?;

        // Vector search filtered to notes.
        let vector_hits = if let Some(vec) = query_vector {
            self.vectors(namespace)?
                .search(VectorSearchRequest {
                    query_embedding: vec,
                    top_k: candidates,
                    namespace: Some(ns.clone()),
                    kind: Some(SubstrateKind::Note),
                })
                .await?
        } else {
            vec![]
        };

        // RRF fusion.
        let mut buckets: HashMap<Uuid, DeterministicScore> = HashMap::new();
        for (i, hit) in text_hits.into_iter().enumerate() {
            let rank = i + 1;
            let entry = buckets.entry(hit.subject_id).or_default();
            *entry = *entry + rrf_score(rank, RRF_K);
        }
        for (i, hit) in vector_hits.into_iter().enumerate() {
            let rank = i + 1;
            let entry = buckets.entry(hit.subject_id).or_default();
            *entry = *entry + rrf_score(rank, RRF_K);
        }

        let candidate_ids: Vec<Uuid> = buckets.keys().copied().collect();
        if candidate_ids.is_empty() {
            return Ok(vec![]);
        }

        // Fetch each candidate note individually to get salience and apply soft-delete filter.
        let note_store = self.notes(namespace)?;
        let mut alive_notes: HashMap<Uuid, Note> = HashMap::new();
        for id in &candidate_ids {
            if let Some(note) = note_store.get_note(*id).await? {
                if note.deleted_at.is_none() {
                    alive_notes.insert(*id, note);
                }
            }
        }

        // Drop superseded notes: any note targeted by a `supersedes` edge is
        // obsolete and excluded from default search (ADR-019, ADR-024).
        if !alive_notes.is_empty() {
            let graph = self.graph(namespace)?;
            let mut superseded: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
            for &note_id in alive_notes.keys() {
                let inbound = graph
                    .neighbors(
                        note_id,
                        NeighborQuery {
                            direction: Direction::In,
                            relations: Some(vec![EdgeRelation::Supersedes]),
                            limit: Some(1),
                            min_weight: None,
                        },
                    )
                    .await?;
                if !inbound.is_empty() {
                    superseded.insert(note_id);
                }
            }
            alive_notes.retain(|id, _| !superseded.contains(id));
        }

        // Apply salience weighting and collect final hits.
        let mut hits: Vec<NoteSearchHit> = buckets
            .into_iter()
            .filter_map(|(id, rrf)| {
                let note = alive_notes.get(&id)?;
                let weight = 0.5 + 0.5 * note.salience;
                let weighted = DeterministicScore::from_f64(rrf.to_f64() * weight);
                Some(NoteSearchHit {
                    note_id: id,
                    score: weighted,
                })
            })
            .collect();

        hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.note_id.cmp(&b.note_id)));
        hits.truncate(limit as usize);
        Ok(hits)
    }

    /// Resolve a short UUID prefix (8+ hex chars) to a full UUID.
    ///
    /// Searches entities, notes, and edges tables for a UUID starting with the
    /// given prefix, scoped to the caller's namespace. Returns `Ok(Some(uuid))`
    /// if exactly one match is found, `Ok(None)` if no matches, or an error if
    /// ambiguous (multiple matches).
    pub async fn resolve_prefix(
        &self,
        namespace: Option<&str>,
        prefix: &str,
    ) -> RuntimeResult<Option<Uuid>> {
        use khive_storage::types::{SqlStatement, SqlValue};

        let ns = self.ns(namespace).to_string();
        let pattern = format!("{}%", prefix);

        let tables = [("entities", true), ("notes", true), ("graph_edges", false)];

        let mut matches: Vec<String> = Vec::new();
        let mut reader = self.sql().reader().await.map_err(RuntimeError::Storage)?;

        for (table, has_deleted_at) in tables {
            let deleted_filter = if has_deleted_at {
                " AND deleted_at IS NULL"
            } else {
                ""
            };
            let sql = SqlStatement {
                sql: format!(
                    "SELECT id FROM {table} WHERE id LIKE ?1 AND namespace = ?2{deleted_filter} LIMIT 2"
                ),
                params: vec![
                    SqlValue::Text(pattern.clone()),
                    SqlValue::Text(ns.clone()),
                ],
                label: Some("resolve_prefix".into()),
            };
            match reader.query_all(sql).await {
                Ok(rows) => {
                    for row in rows {
                        if let Some(col) = row.columns.first() {
                            if let SqlValue::Text(s) = &col.value {
                                matches.push(s.clone());
                            }
                        }
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("no such table") {
                        continue;
                    }
                    return Err(RuntimeError::Storage(e));
                }
            }
            if matches.len() > 1 {
                break;
            }
        }

        match matches.len() {
            0 => Ok(None),
            1 => {
                let uuid = Uuid::from_str(&matches[0])
                    .map_err(|e| RuntimeError::Internal(format!("stored UUID is invalid: {e}")))?;
                Ok(Some(uuid))
            }
            _ => Err(RuntimeError::Ambiguous(format!(
                "prefix '{prefix}' matches multiple UUIDs"
            ))),
        }
    }

    /// Resolve a UUID to its substrate kind by trying entity, then note, then event stores.
    ///
    /// Returns `None` if the UUID is not found in any substrate.
    /// Cost: at most 3 store lookups per call (cheap for v0.1).
    pub async fn resolve(
        &self,
        namespace: Option<&str>,
        id: Uuid,
    ) -> RuntimeResult<Option<Resolved>> {
        let ns = self.ns(namespace);

        // Entity: use the namespace-checked getter (returns None on mismatch).
        if let Some(entity) = self.get_entity(namespace, id).await? {
            return Ok(Some(Resolved::Entity(entity)));
        }

        // Note: storage get_note is ID-only — verify namespace after fetch.
        if let Some(note) = self.notes(namespace)?.get_note(id).await? {
            if note.namespace == ns {
                return Ok(Some(Resolved::Note(note)));
            }
        }

        // Event: storage get_event is ID-only — verify namespace after fetch.
        if let Some(event) = self.events(namespace)?.get_event(id).await? {
            if event.namespace == ns {
                return Ok(Some(Resolved::Event(event)));
            }
        }

        Ok(None)
    }

    /// Delete a note by ID, enforcing namespace isolation.
    ///
    /// Returns `false` without deleting if the note does not exist or belongs to
    /// a different namespace (ADR-007 namespace isolation).
    pub async fn delete_note(
        &self,
        namespace: Option<&str>,
        id: Uuid,
        hard: bool,
    ) -> RuntimeResult<bool> {
        let ns = self.ns(namespace);
        let note_store = self.notes(namespace)?;
        let note = match note_store.get_note(id).await? {
            Some(n) => n,
            None => return Ok(false),
        };
        if note.namespace != ns {
            return Ok(false);
        }
        let mode = if hard {
            DeleteMode::Hard
        } else {
            DeleteMode::Soft
        };
        Ok(note_store.delete_note(id, mode).await?)
    }

    // ---- Query operations ----

    /// Execute a GQL or SPARQL query string, returning raw SQL rows.
    ///
    /// The query is compiled to SQL with the namespace scope applied.
    /// GQL syntax: `MATCH (a:concept)-[e:extends]->(b) RETURN a, b LIMIT 10`
    /// SPARQL syntax: `SELECT ?a WHERE { ?a :kind "concept" . }`
    pub async fn query(
        &self,
        namespace: Option<&str>,
        query: &str,
    ) -> RuntimeResult<Vec<khive_storage::types::SqlRow>> {
        let ns = self.ns(namespace);
        let ast = khive_query::parse_auto(query)?;
        let opts = khive_query::CompileOptions {
            scopes: vec![ns.to_string()],
            ..Default::default()
        };
        let compiled = khive_query::compile(&ast, &opts)?;
        let mut reader = self.sql().reader().await?;
        let stmt = SqlStatement {
            sql: compiled.sql,
            params: compiled.params,
            label: None,
        };
        Ok(reader.query_all(stmt).await?)
    }

    /// Delete an entity by ID (soft delete by default).
    ///
    /// On hard delete, cascades to remove all incident edges (both inbound and
    /// outbound) to prevent dangling references. Soft delete leaves edges in
    /// place — queries already filter by `deleted_at IS NULL`.
    ///
    /// Returns `false` without deleting if the entity exists but belongs to a
    /// different namespace (ADR-007 namespace isolation).
    pub async fn delete_entity(
        &self,
        namespace: Option<&str>,
        id: Uuid,
        hard: bool,
    ) -> RuntimeResult<bool> {
        let entity = match self.entities(namespace)?.get_entity(id).await? {
            Some(e) => e,
            None => return Ok(false),
        };
        if entity.namespace != self.ns(namespace) {
            return Ok(false);
        }
        let mode = if hard {
            DeleteMode::Hard
        } else {
            DeleteMode::Soft
        };

        // On hard delete, cascade-remove incident edges to prevent dangling refs.
        if hard {
            let graph = self.graph(namespace)?;
            for direction in [Direction::Out, Direction::In] {
                let hits = graph
                    .neighbors(
                        id,
                        NeighborQuery {
                            direction,
                            relations: None,
                            limit: None,
                            min_weight: None,
                        },
                    )
                    .await?;
                for hit in hits {
                    graph.delete_edge(LinkId::from(hit.edge_id)).await?;
                }
            }
            self.remove_from_indexes(namespace, id).await?;
        }

        Ok(self.entities(namespace)?.delete_entity(id, mode).await?)
    }

    /// Count entities in a namespace, optionally filtered.
    pub async fn count_entities(
        &self,
        namespace: Option<&str>,
        kind: Option<&str>,
    ) -> RuntimeResult<u64> {
        let filter = EntityFilter {
            kinds: match kind {
                Some(k) => vec![EntityKind::from_str(k).map_err(RuntimeError::InvalidInput)?],
                None => vec![],
            },
            ..Default::default()
        };
        Ok(self
            .entities(namespace)?
            .count_entities(self.ns(namespace), filter)
            .await?)
    }

    // ---- Edge CRUD operations ----

    /// Fetch a single edge by id. Returns `None` if the edge does not exist.
    pub async fn get_edge(
        &self,
        namespace: Option<&str>,
        edge_id: Uuid,
    ) -> RuntimeResult<Option<Edge>> {
        Ok(self
            .graph(namespace)?
            .get_edge(LinkId::from(edge_id))
            .await?)
    }

    /// List edges matching `filter`. `limit` is capped at 1000; defaults to 100.
    pub async fn list_edges(
        &self,
        namespace: Option<&str>,
        filter: crate::curation::EdgeListFilter,
        limit: u32,
    ) -> RuntimeResult<Vec<Edge>> {
        let limit = limit.clamp(1, 1000);
        let page = self
            .graph(namespace)?
            .query_edges(
                filter.into(),
                vec![SortOrder {
                    field: EdgeSortField::CreatedAt,
                    direction: khive_storage::types::SortDirection::Asc,
                }],
                PageRequest { offset: 0, limit },
            )
            .await?;
        Ok(page.items)
    }

    /// Patch-style edge update. Only `Some(_)` fields are applied.
    pub async fn update_edge(
        &self,
        namespace: Option<&str>,
        edge_id: Uuid,
        relation: Option<EdgeRelation>,
        weight: Option<f64>,
    ) -> RuntimeResult<Edge> {
        let graph = self.graph(namespace)?;
        let mut edge = graph
            .get_edge(LinkId::from(edge_id))
            .await?
            .ok_or_else(|| crate::RuntimeError::NotFound(format!("edge {edge_id}")))?;

        if let Some(r) = relation {
            edge.relation = r;
        }
        if let Some(w) = weight {
            edge.weight = w.clamp(0.0, 1.0);
        }

        graph.upsert_edge(edge.clone()).await?;
        Ok(edge)
    }

    /// Hard-delete an edge by id. Returns `true` if an edge was removed.
    pub async fn delete_edge(&self, namespace: Option<&str>, edge_id: Uuid) -> RuntimeResult<bool> {
        Ok(self
            .graph(namespace)?
            .delete_edge(LinkId::from(edge_id))
            .await?)
    }

    /// Count edges matching `filter`.
    pub async fn count_edges(
        &self,
        namespace: Option<&str>,
        filter: crate::curation::EdgeListFilter,
    ) -> RuntimeResult<u64> {
        Ok(self.graph(namespace)?.count_edges(filter.into()).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curation::EdgeListFilter;
    use crate::runtime::KhiveRuntime;

    fn rt() -> KhiveRuntime {
        KhiveRuntime::memory().unwrap()
    }

    #[tokio::test]
    async fn update_edge_changes_weight() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        let edge_id: Uuid = edge.id.into();

        let updated = rt
            .update_edge(None, edge_id, None, Some(0.5))
            .await
            .unwrap();
        assert!((updated.weight - 0.5).abs() < 0.001);
    }

    #[tokio::test]
    async fn update_edge_changes_relation() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        let edge_id: Uuid = edge.id.into();

        let updated = rt
            .update_edge(None, edge_id, Some(EdgeRelation::VariantOf), None)
            .await
            .unwrap();
        assert_eq!(updated.relation, EdgeRelation::VariantOf);
    }

    #[tokio::test]
    async fn list_edges_filters_by_relation() {
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

        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        rt.link(None, a.id, c.id, EdgeRelation::DependsOn, 1.0)
            .await
            .unwrap();

        let filter = EdgeListFilter {
            relations: vec![EdgeRelation::Extends],
            ..Default::default()
        };
        let edges = rt.list_edges(None, filter, 100).await.unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].relation, EdgeRelation::Extends);
    }

    #[tokio::test]
    async fn list_edges_filters_by_source() {
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

        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        rt.link(None, c.id, d.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();

        let filter = EdgeListFilter {
            source_id: Some(a.id),
            ..Default::default()
        };
        let edges = rt.list_edges(None, filter, 100).await.unwrap();
        assert_eq!(edges.len(), 1);
        let src: Uuid = edges[0].source_id;
        assert_eq!(src, a.id);
    }

    #[tokio::test]
    async fn delete_edge_removes_from_storage() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        let edge_id: Uuid = edge.id.into();

        let deleted = rt.delete_edge(None, edge_id).await.unwrap();
        assert!(deleted);

        let fetched = rt.get_edge(None, edge_id).await.unwrap();
        assert!(fetched.is_none(), "edge should be gone after delete");
    }

    #[tokio::test]
    async fn count_edges_matches_filter() {
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

        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        rt.link(None, a.id, c.id, EdgeRelation::DependsOn, 1.0)
            .await
            .unwrap();

        let all = rt
            .count_edges(None, EdgeListFilter::default())
            .await
            .unwrap();
        assert_eq!(all, 2);

        let just_extends = rt
            .count_edges(
                None,
                EdgeListFilter {
                    relations: vec![EdgeRelation::Extends],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(just_extends, 1);
    }

    #[tokio::test]
    async fn get_entity_namespace_isolation() {
        let rt = rt();
        let entity = rt
            .create_entity(Some("ns-a"), "concept", "Alpha", None, None, vec![])
            .await
            .unwrap();

        // Same namespace: visible.
        let found = rt.get_entity(Some("ns-a"), entity.id).await.unwrap();
        assert!(found.is_some(), "should be visible in its own namespace");

        // Different namespace: invisible.
        let not_found = rt.get_entity(Some("ns-b"), entity.id).await.unwrap();
        assert!(
            not_found.is_none(),
            "should not be visible across namespaces"
        );
    }

    #[tokio::test]
    async fn delete_entity_namespace_isolation() {
        let rt = rt();
        let entity = rt
            .create_entity(Some("ns-a"), "concept", "Beta", None, None, vec![])
            .await
            .unwrap();

        // Delete from wrong namespace: no-op, returns false.
        let deleted = rt
            .delete_entity(Some("ns-b"), entity.id, true)
            .await
            .unwrap();
        assert!(!deleted, "cross-namespace delete must return false");

        // Entity still present in its own namespace.
        let still_there = rt.get_entity(Some("ns-a"), entity.id).await.unwrap();
        assert!(
            still_there.is_some(),
            "entity must survive cross-ns delete attempt"
        );

        // Delete from correct namespace: succeeds.
        let deleted_ok = rt
            .delete_entity(Some("ns-a"), entity.id, true)
            .await
            .unwrap();
        assert!(deleted_ok, "same-namespace delete must succeed");
    }

    // ---- Note ADR-024 tests ----

    #[tokio::test]
    async fn create_note_indexes_into_fts5() {
        let rt = rt();
        let note = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "FlashAttention reduces memory by using tiling",
                0.8,
                None,
                vec![],
            )
            .await
            .unwrap();

        // FTS5 should have indexed the note content.
        let ns = rt.ns(None).to_string();
        let hits = rt
            .text_for_notes(None)
            .unwrap()
            .search(khive_storage::types::TextSearchRequest {
                query: "FlashAttention".to_string(),
                mode: khive_storage::types::TextQueryMode::Plain,
                filter: Some(khive_storage::types::TextFilter {
                    namespaces: vec![ns],
                    ..Default::default()
                }),
                top_k: 10,
                snippet_chars: 100,
            })
            .await
            .unwrap();

        assert!(
            hits.iter().any(|h| h.subject_id == note.id),
            "note should be indexed in FTS5 after create"
        );
    }

    #[tokio::test]
    async fn create_note_with_properties() {
        let rt = rt();
        let props = serde_json::json!({"source": "arxiv:2205.14135"});
        let note = rt
            .create_note(
                None,
                khive_storage::NoteKind::Insight,
                None,
                "FlashAttention is IO-aware",
                0.9,
                Some(props.clone()),
                vec![],
            )
            .await
            .unwrap();

        assert_eq!(note.properties.as_ref().unwrap(), &props);
    }

    #[tokio::test]
    async fn create_note_creates_annotates_edges() {
        let rt = rt();
        let entity = rt
            .create_entity(None, "concept", "FlashAttention", None, None, vec![])
            .await
            .unwrap();

        let note = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "FlashAttention uses SRAM tiling for memory efficiency",
                0.9,
                None,
                vec![entity.id],
            )
            .await
            .unwrap();

        // The note should have an outbound `annotates` edge to the entity.
        let out_neighbors = rt
            .neighbors(
                None,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(out_neighbors.len(), 1);
        assert_eq!(out_neighbors[0].node_id, entity.id);
        assert_eq!(out_neighbors[0].relation, EdgeRelation::Annotates);

        // The entity should have an inbound `annotates` edge from the note.
        let in_neighbors = rt
            .neighbors(
                None,
                entity.id,
                Direction::In,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(in_neighbors.len(), 1);
        assert_eq!(in_neighbors[0].node_id, note.id);
    }

    #[tokio::test]
    async fn neighbors_without_relation_filter_returns_all() {
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

        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        rt.link(None, a.id, c.id, EdgeRelation::DependsOn, 1.0)
            .await
            .unwrap();

        let all = rt
            .neighbors(None, a.id, Direction::Out, None, None)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn neighbors_with_relation_filter_returns_subset() {
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

        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        rt.link(None, a.id, c.id, EdgeRelation::DependsOn, 1.0)
            .await
            .unwrap();

        let filtered = rt
            .neighbors(
                None,
                a.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Extends]),
            )
            .await
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].node_id, b.id);
        assert_eq!(filtered[0].relation, EdgeRelation::Extends);
    }

    #[tokio::test]
    async fn search_notes_returns_relevant_note() {
        let rt = rt();
        rt.create_note(
            None,
            khive_storage::NoteKind::Observation,
            None,
            "GQA reduces KV cache memory for large models",
            0.8,
            None,
            vec![],
        )
        .await
        .unwrap();

        let results = rt
            .search_notes(None, "GQA KV cache", None, 10)
            .await
            .unwrap();

        assert!(!results.is_empty(), "search should return the indexed note");
    }

    #[tokio::test]
    async fn search_notes_excludes_soft_deleted() {
        let rt = rt();
        let note = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "RoPE positional encoding rotary embeddings",
                0.7,
                None,
                vec![],
            )
            .await
            .unwrap();

        // Soft-delete the note.
        rt.notes(None)
            .unwrap()
            .delete_note(note.id, DeleteMode::Soft)
            .await
            .unwrap();

        let results = rt
            .search_notes(None, "RoPE rotary positional", None, 10)
            .await
            .unwrap();

        assert!(
            results.iter().all(|h| h.note_id != note.id),
            "soft-deleted note should be excluded from search"
        );
    }

    #[tokio::test]
    async fn resolve_returns_entity() {
        let rt = rt();
        let entity = rt
            .create_entity(None, "concept", "LoRA", None, None, vec![])
            .await
            .unwrap();

        let resolved = rt.resolve(None, entity.id).await.unwrap();
        match resolved {
            Some(Resolved::Entity(e)) => assert_eq!(e.id, entity.id),
            other => panic!("expected Resolved::Entity, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn resolve_returns_note() {
        let rt = rt();
        let note = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "LoRA fine-tunes LLMs with low-rank adapters",
                0.85,
                None,
                vec![],
            )
            .await
            .unwrap();

        let resolved = rt.resolve(None, note.id).await.unwrap();
        match resolved {
            Some(Resolved::Note(n)) => assert_eq!(n.id, note.id),
            other => panic!("expected Resolved::Note, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn resolve_returns_none_for_unknown_uuid() {
        let rt = rt();
        let unknown = Uuid::new_v4();
        let resolved = rt.resolve(None, unknown).await.unwrap();
        assert!(resolved.is_none(), "unknown UUID should resolve to None");
    }

    #[tokio::test]
    async fn resolve_prefix_finds_entity_in_own_namespace() {
        let rt = rt();
        let entity = rt
            .create_entity(None, "concept", "PrefixTest", None, None, vec![])
            .await
            .unwrap();
        let prefix = &entity.id.to_string()[..8];

        let resolved = rt.resolve_prefix(None, prefix).await.unwrap();
        assert_eq!(resolved, Some(entity.id));
    }

    #[tokio::test]
    async fn resolve_prefix_invisible_across_namespaces() {
        let rt = rt();
        let entity = rt
            .create_entity(Some("ns_a"), "concept", "Invisible", None, None, vec![])
            .await
            .unwrap();
        let prefix = &entity.id.to_string()[..8];

        // From ns_b, the entity in ns_a should not be visible.
        let resolved = rt.resolve_prefix(Some("ns_b"), prefix).await.unwrap();
        assert_eq!(resolved, None);
    }

    #[tokio::test]
    async fn resolve_prefix_ambiguous_same_namespace() {
        use khive_storage::entity::Entity;
        use khive_storage::EntityStore;
        use khive_types::EntityKind;

        let rt = rt();
        // Two entities with UUIDs sharing the same 8-char prefix "aabbccdd".
        let id_a = Uuid::parse_str("aabbccdd-1111-4000-8000-000000000001").unwrap();
        let id_b = Uuid::parse_str("aabbccdd-2222-4000-8000-000000000002").unwrap();

        let mut entity_a = Entity::new("local", EntityKind::Concept, "AmbigA");
        entity_a.id = id_a;
        let mut entity_b = Entity::new("local", EntityKind::Concept, "AmbigB");
        entity_b.id = id_b;

        let store = rt.entities(None).unwrap();
        store.upsert_entity(entity_a).await.unwrap();
        store.upsert_entity(entity_b).await.unwrap();

        let result = rt.resolve_prefix(None, "aabbccdd").await;
        assert!(
            result.is_err(),
            "shared 8-char prefix must return Ambiguous error"
        );
    }
}
