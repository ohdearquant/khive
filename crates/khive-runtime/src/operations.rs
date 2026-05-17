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
        let entity_kind = EntityKind::from_str(kind)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
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
                Some(k) => vec![EntityKind::from_str(k).map_err(|e| RuntimeError::InvalidInput(e.to_string()))?],
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

    /// Validate that `source_id` and `target_id` are legal endpoints for `relation`.
    ///
    /// Centralises the ADR-002/ADR-019/ADR-024 three-case contract so that both
    /// `link()` and `update_edge()` share identical enforcement:
    ///
    /// - `annotates`: source MUST be a note; target may be any substrate.
    /// - `supersedes`: same-substrate only (note→note or entity→entity).
    /// - All other 11 relations: both endpoints MUST be entities.
    ///
    /// Returns `Ok(())` when valid; otherwise `InvalidInput` or `NotFound` with
    /// the same messages as the previous inline block (byte-identical behaviour).
    async fn validate_edge_relation_endpoints(
        &self,
        namespace: Option<&str>,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
    ) -> RuntimeResult<()> {
        if relation == EdgeRelation::Annotates {
            // Source must be a note in namespace.
            match self.resolve(namespace, source_id).await? {
                Some(Resolved::Note(_)) => {}
                Some(_) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "annotates source {source_id} must be a note"
                    )));
                }
                None => {
                    // Existing edge used as annotates source: wrong kind, not absent.
                    if self.get_edge(namespace, source_id).await?.is_some() {
                        return Err(RuntimeError::InvalidInput(format!(
                            "annotates source {source_id} must be a note"
                        )));
                    }
                    return Err(RuntimeError::NotFound(format!(
                        "link source {source_id} not found in namespace"
                    )));
                }
            }
            // Target may be any substrate (entity, note, event, or edge).
            if !self.substrate_exists_in_ns(namespace, target_id).await? {
                return Err(RuntimeError::NotFound(format!(
                    "link target {target_id} not found in namespace"
                )));
            }
        } else if relation == EdgeRelation::Supersedes {
            // supersedes: same-substrate only (note→note or entity→entity).
            // Event and edge endpoints are invalid regardless of the other endpoint.
            let src = match self.resolve(namespace, source_id).await? {
                Some(r) => r,
                None => {
                    if self.get_edge(namespace, source_id).await?.is_some() {
                        return Err(RuntimeError::InvalidInput(format!(
                            "supersedes source {source_id} must be a note or entity (got edge)"
                        )));
                    }
                    return Err(RuntimeError::NotFound(format!(
                        "link source {source_id} not found in namespace"
                    )));
                }
            };
            let tgt = match self.resolve(namespace, target_id).await? {
                Some(r) => r,
                None => {
                    if self.get_edge(namespace, target_id).await?.is_some() {
                        return Err(RuntimeError::InvalidInput(format!(
                            "supersedes target {target_id} must be a note or entity (got edge)"
                        )));
                    }
                    return Err(RuntimeError::NotFound(format!(
                        "link target {target_id} not found in namespace"
                    )));
                }
            };
            match (&src, &tgt) {
                (Resolved::Entity(_), Resolved::Entity(_)) => {}
                (Resolved::Note(_), Resolved::Note(_)) => {}
                (Resolved::Event(_), _) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "supersedes does not apply to events; source {source_id} is an event"
                    )));
                }
                (_, Resolved::Event(_)) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "supersedes does not apply to events; target {target_id} is an event"
                    )));
                }
                (Resolved::Entity(_), Resolved::Note(_)) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "supersedes endpoints must be the same substrate (note→note or entity→entity); \
                         got source={source_id} (entity) target={target_id} (note)"
                    )));
                }
                (Resolved::Note(_), Resolved::Entity(_)) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "supersedes endpoints must be the same substrate (note→note or entity→entity); \
                         got source={source_id} (note) target={target_id} (entity)"
                    )));
                }
            }
        } else {
            // All 11 entity→entity relations: both endpoints must be entities.
            // resolve() covers entity/note/event; get_edge() covers edges (not in resolve).
            // None from resolve + Some from get_edge → InvalidInput (wrong substrate kind).
            // None from both → NotFound (phantom / cross-namespace).
            match self.resolve(namespace, source_id).await? {
                Some(Resolved::Entity(_)) => {}
                Some(_) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "link source {source_id} must be an entity for relation {relation:?} \
                         (ADR-002: only `annotates` crosses substrates)"
                    )));
                }
                None => {
                    if self.get_edge(namespace, source_id).await?.is_some() {
                        return Err(RuntimeError::InvalidInput(format!(
                            "link source {source_id} must be an entity for relation {relation:?} \
                             (ADR-002: only `annotates` crosses substrates)"
                        )));
                    }
                    return Err(RuntimeError::NotFound(format!(
                        "link source {source_id} not found in namespace"
                    )));
                }
            }
            match self.resolve(namespace, target_id).await? {
                Some(Resolved::Entity(_)) => {}
                Some(_) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "link target {target_id} must be an entity for relation {relation:?} \
                         (ADR-002: only `annotates` crosses substrates)"
                    )));
                }
                None => {
                    if self.get_edge(namespace, target_id).await?.is_some() {
                        return Err(RuntimeError::InvalidInput(format!(
                            "link target {target_id} must be an entity for relation {relation:?} \
                             (ADR-002: only `annotates` crosses substrates)"
                        )));
                    }
                    return Err(RuntimeError::NotFound(format!(
                        "link target {target_id} not found in namespace"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Create a directed edge between two substrates.
    ///
    /// Enforces the ADR-002/ADR-019/ADR-024 three-case relation contract via
    /// `validate_edge_relation_endpoints`. See that method for the full contract.
    ///
    /// A record that exists but belongs to a different namespace is treated as not found
    /// (fail-closed; no cross-namespace existence leak).
    pub async fn link(
        &self,
        namespace: Option<&str>,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
        weight: f64,
    ) -> RuntimeResult<Edge> {
        self.validate_edge_relation_endpoints(namespace, source_id, target_id, relation)
            .await?;
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

    /// Returns `true` if `id` resolves to a live substrate record in `namespace`.
    ///
    /// Covers entity, note, event (via `resolve`) and edge (via `get_edge`).
    /// A record that exists in a different namespace returns `false` (fail-closed).
    async fn substrate_exists_in_ns(
        &self,
        namespace: Option<&str>,
        id: Uuid,
    ) -> RuntimeResult<bool> {
        if self.resolve(namespace, id).await?.is_some() {
            return Ok(true);
        }
        Ok(self.get_edge(namespace, id).await?.is_some())
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

        // Validate all annotates targets before any write (ADR-024:295 atomicity).
        for &target_id in &annotates {
            if !self.substrate_exists_in_ns(namespace, target_id).await? {
                return Err(RuntimeError::NotFound(format!(
                    "create_note annotates target {target_id} not found in namespace"
                )));
            }
        }

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
            Some(k) => Some(NoteKind::from_str(k).map_err(|e| RuntimeError::InvalidInput(e.to_string()))?),
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
                Some(k) => vec![EntityKind::from_str(k).map_err(|e| RuntimeError::InvalidInput(e.to_string()))?],
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
    ///
    /// When `relation` is `Some(new_rel)`, validates that the edge's existing endpoints
    /// are legal for `new_rel` before persisting. Weight-only updates (`relation = None`)
    /// skip validation. Returns `InvalidInput` if the new relation would violate the
    /// ADR-002/ADR-019/ADR-024 three-case contract; the edge is NOT mutated on error.
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
            // Validate before mutating — use the existing endpoints with the new relation.
            self.validate_edge_relation_endpoints(namespace, edge.source_id, edge.target_id, r)
                .await?;
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

    // ---- Round-5 tests: update_edge endpoint validation (ADR-002 bypass fix) ----

    // update_edge: note→entity annotates → set relation=Supersedes → InvalidInput (crossing).
    // Edge must NOT be mutated in the store.
    #[tokio::test]
    async fn update_edge_annotates_note_to_entity_set_supersedes_returns_invalid_input() {
        let rt = rt();
        let note = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "a note",
                0.5,
                None,
                vec![],
            )
            .await
            .unwrap();
        let entity = rt
            .create_entity(None, "concept", "E", None, None, vec![])
            .await
            .unwrap();
        // Create a valid note→entity annotates edge.
        let edge = rt
            .link(None, note.id, entity.id, EdgeRelation::Annotates, 1.0)
            .await
            .unwrap();
        let edge_id: Uuid = edge.id.into();

        // Attempt to change relation to Supersedes (crossing substrates → invalid).
        let result = rt
            .update_edge(None, edge_id, Some(EdgeRelation::Supersedes), None)
            .await;
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(_))),
            "update to Supersedes on note→entity edge must return InvalidInput, got {result:?}"
        );

        // Edge must NOT be mutated — re-fetch and verify relation unchanged.
        let fetched = rt.get_edge(None, edge_id).await.unwrap().unwrap();
        assert_eq!(
            fetched.relation,
            EdgeRelation::Annotates,
            "edge relation must be unchanged after failed update"
        );
    }

    // update_edge: entity→entity extends → set relation=Annotates → InvalidInput
    // (annotates source must be a note).
    #[tokio::test]
    async fn update_edge_entity_to_entity_set_annotates_returns_invalid_input() {
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

        let result = rt
            .update_edge(None, edge_id, Some(EdgeRelation::Annotates), None)
            .await;
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(_))),
            "update to Annotates on entity→entity edge must return InvalidInput, got {result:?}"
        );
    }

    // update_edge: entity→entity extends → set relation=Supersedes → Ok
    // (entity→entity is valid for supersedes).
    #[tokio::test]
    async fn update_edge_entity_to_entity_set_supersedes_succeeds() {
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
            .update_edge(None, edge_id, Some(EdgeRelation::Supersedes), None)
            .await
            .unwrap();
        assert_eq!(updated.relation, EdgeRelation::Supersedes);

        // Verify persisted.
        let fetched = rt.get_edge(None, edge_id).await.unwrap().unwrap();
        assert_eq!(fetched.relation, EdgeRelation::Supersedes);
    }

    // update_edge: weight-only (relation = None) → Ok, no validation, unchanged relation.
    #[tokio::test]
    async fn update_edge_weight_only_skips_validation() {
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
            .update_edge(None, edge_id, None, Some(0.3))
            .await
            .unwrap();
        assert_eq!(updated.relation, EdgeRelation::Extends);
        assert!((updated.weight - 0.3).abs() < 0.001);
    }

    // update_edge: entity→entity extends → set relation=VariantOf (same class) → Ok.
    #[tokio::test]
    async fn update_edge_same_class_relation_change_succeeds() {
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

    // ---- Referential integrity tests (fix/link-referential-integrity) ----

    #[tokio::test]
    async fn link_phantom_source_returns_not_found() {
        let rt = rt();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        let phantom = Uuid::new_v4();

        let result = rt
            .link(None, phantom, b.id, EdgeRelation::Extends, 1.0)
            .await;
        match result {
            Err(RuntimeError::NotFound(msg)) => {
                assert!(
                    msg.contains("source"),
                    "error message must name 'source': {msg}"
                );
            }
            other => panic!("expected NotFound for phantom source, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn link_phantom_target_returns_not_found() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let phantom = Uuid::new_v4();

        let result = rt
            .link(None, a.id, phantom, EdgeRelation::Extends, 1.0)
            .await;
        match result {
            Err(RuntimeError::NotFound(msg)) => {
                assert!(
                    msg.contains("target"),
                    "error message must name 'target': {msg}"
                );
            }
            other => panic!("expected NotFound for phantom target, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn link_real_entities_succeeds() {
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
            .link(None, a.id, b.id, EdgeRelation::Extends, 0.8)
            .await
            .unwrap();
        assert_eq!(edge.source_id, a.id);
        assert_eq!(edge.target_id, b.id);
        assert_eq!(edge.relation, EdgeRelation::Extends);
    }

    #[tokio::test]
    async fn create_note_annotates_phantom_returns_not_found() {
        let rt = rt();
        let phantom = Uuid::new_v4();

        let result = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "some content",
                0.5,
                None,
                vec![phantom],
            )
            .await;
        assert!(
            matches!(result, Err(RuntimeError::NotFound(_))),
            "annotates with phantom uuid must return NotFound, got {result:?}"
        );
    }

    #[tokio::test]
    async fn create_note_annotates_real_entity_succeeds() {
        let rt = rt();
        let entity = rt
            .create_entity(None, "concept", "RealTarget", None, None, vec![])
            .await
            .unwrap();

        let note = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "content",
                0.5,
                None,
                vec![entity.id],
            )
            .await
            .unwrap();

        let neighbors = rt
            .neighbors(
                None,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].node_id, entity.id);
    }

    #[tokio::test]
    async fn link_target_in_different_namespace_returns_not_found() {
        let rt = rt();
        let a = rt
            .create_entity(Some("ns-a"), "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(Some("ns-b"), "concept", "B", None, None, vec![])
            .await
            .unwrap();

        // Linking from ns-a: target b lives in ns-b — must be treated as not found.
        let result = rt
            .link(Some("ns-a"), a.id, b.id, EdgeRelation::Extends, 1.0)
            .await;
        assert!(
            matches!(result, Err(RuntimeError::NotFound(_))),
            "target in different namespace must return NotFound (fail-closed), got {result:?}"
        );
    }

    #[tokio::test]
    async fn link_phantom_self_loop_returns_not_found() {
        let rt = rt();
        let phantom = Uuid::new_v4();

        let result = rt
            .link(None, phantom, phantom, EdgeRelation::Extends, 1.0)
            .await;
        match result {
            Err(RuntimeError::NotFound(msg)) => {
                assert!(
                    msg.contains("source"),
                    "self-loop must fail on source first: {msg}"
                );
            }
            other => panic!("expected NotFound for phantom self-loop, got {other:?}"),
        }
    }

    // ---- Round-2 tests: edge target coverage + atomicity ----

    #[tokio::test]
    async fn link_note_to_edge_annotates_succeeds() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        // Create a real edge between a and b, capture its UUID.
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        let edge_uuid: Uuid = edge.id.into();

        // Create a note and annotate the edge itself (edge is a valid substrate target per ADR-024).
        let note = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "edge note",
                0.5,
                None,
                vec![],
            )
            .await
            .unwrap();

        let result = rt
            .link(None, note.id, edge_uuid, EdgeRelation::Annotates, 1.0)
            .await;
        assert!(
            result.is_ok(),
            "note→edge Annotates must succeed, got {result:?}"
        );
    }

    #[tokio::test]
    async fn create_note_annotates_real_edge_succeeds() {
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
        let edge_uuid: Uuid = edge.id.into();

        let note = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "annotating an edge",
                0.5,
                None,
                vec![edge_uuid],
            )
            .await
            .unwrap();

        let neighbors = rt
            .neighbors(
                None,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].node_id, edge_uuid);
    }

    #[tokio::test]
    async fn create_note_annotates_phantom_is_atomic_no_note_persisted() {
        let rt = rt();
        let phantom = Uuid::new_v4();

        let before_count = rt.list_notes(None, None, 1000).await.unwrap().len();

        let result = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "should not persist",
                0.5,
                None,
                vec![phantom],
            )
            .await;
        assert!(
            matches!(result, Err(RuntimeError::NotFound(_))),
            "phantom annotates target must return NotFound, got {result:?}"
        );

        // Atomicity: the note row must NOT have been written.
        let after_count = rt.list_notes(None, None, 1000).await.unwrap().len();
        assert_eq!(
            before_count, after_count,
            "failed create_note must not persist any note row (atomicity)"
        );

        // FTS must not contain the content either.
        let search_hits = rt
            .search_notes(None, "should not persist", None, 10)
            .await
            .unwrap();
        assert!(
            search_hits.is_empty(),
            "failed create_note must not index into FTS (atomicity)"
        );
        // Vector-store row: only written when an embedding model is configured; the rt()
        // harness has none, so no vector assertion is needed here.
    }

    // ---- Round-3 tests: relation-aware endpoint contract (ADR-002) ----

    // Test #2: entity→entity with non-annotates rejects an edge UUID as target.
    #[tokio::test]
    async fn link_entity_to_edge_uuid_non_annotates_returns_invalid_input() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        // Create a real edge; capture its UUID as the bad target.
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        let edge_uuid: Uuid = edge.id.into();

        let result = rt
            .link(None, a.id, edge_uuid, EdgeRelation::Extends, 1.0)
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(
                    msg.contains("target"),
                    "error message must name 'target': {msg}"
                );
            }
            other => {
                panic!("expected InvalidInput for edge-uuid target with Extends, got {other:?}")
            }
        }
    }

    // Test #3: non-annotates rejects a note UUID as source.
    #[tokio::test]
    async fn link_note_as_source_non_annotates_returns_invalid_input() {
        let rt = rt();
        let note = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "a note",
                0.5,
                None,
                vec![],
            )
            .await
            .unwrap();
        let entity = rt
            .create_entity(None, "concept", "E", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(None, note.id, entity.id, EdgeRelation::DependsOn, 1.0)
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(
                    msg.contains("source"),
                    "error message must name 'source': {msg}"
                );
            }
            other => panic!("expected InvalidInput for note source with DependsOn, got {other:?}"),
        }
    }

    // Test #4: annotates rejects entity as source (source must be a note).
    #[tokio::test]
    async fn link_entity_as_annotates_source_returns_invalid_input() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(None, a.id, b.id, EdgeRelation::Annotates, 1.0)
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(
                    msg.contains("source") && msg.contains("note"),
                    "error must say source must be a note: {msg}"
                );
            }
            other => {
                panic!("expected InvalidInput for entity source with Annotates, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn link_edge_as_annotates_source_returns_invalid_input() {
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
        let edge_uuid: Uuid = edge.id.into();

        // An existing edge used as an annotates source: wrong kind, not absent.
        let result = rt
            .link(None, edge_uuid, a.id, EdgeRelation::Annotates, 1.0)
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(
                    msg.contains("source") && msg.contains("note"),
                    "edge-as-annotates-source must report wrong kind, not NotFound: {msg}"
                );
            }
            other => panic!("expected InvalidInput for edge source with Annotates, got {other:?}"),
        }
    }

    // Test #5: note→event with annotates succeeds (event is a valid annotates target).
    #[tokio::test]
    async fn link_note_to_event_annotates_succeeds() {
        use khive_storage::Event;
        use khive_types::SubstrateKind;

        let rt = rt();
        let note = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "observing an event",
                0.6,
                None,
                vec![],
            )
            .await
            .unwrap();

        // Build an event directly via the store (no runtime create_event exists).
        let ns = rt.ns(None);
        let event = Event::new(ns, "test_verb", SubstrateKind::Entity, "test_actor");
        let event_id = event.id;
        rt.events(None).unwrap().append_event(event).await.unwrap();

        let result = rt
            .link(None, note.id, event_id, EdgeRelation::Annotates, 1.0)
            .await;
        assert!(
            result.is_ok(),
            "note→event Annotates must succeed, got {result:?}"
        );
    }

    // Test #6: create_note with event as annotates target succeeds.
    #[tokio::test]
    async fn create_note_annotates_event_succeeds() {
        use khive_storage::Event;
        use khive_types::SubstrateKind;

        let rt = rt();
        let ns = rt.ns(None);
        let event = Event::new(ns, "test_verb", SubstrateKind::Entity, "test_actor");
        let event_id = event.id;
        rt.events(None).unwrap().append_event(event).await.unwrap();

        let result = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "note annotating an event",
                0.5,
                None,
                vec![event_id],
            )
            .await;
        assert!(
            result.is_ok(),
            "create_note with event annotates target must succeed, got {result:?}"
        );
        // Verify the annotates edge was created.
        let note = result.unwrap();
        let neighbors = rt
            .neighbors(
                None,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].node_id, event_id);
    }

    // ---- Round-4 tests: supersedes same-substrate contract (ADR-019/ADR-024) ----

    // Headline regression: note→note supersedes must succeed (was wrongly rejected before this fix).
    #[tokio::test]
    async fn link_supersedes_note_to_note_succeeds() {
        let rt = rt();
        let old_note = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "old observation",
                0.7,
                None,
                vec![],
            )
            .await
            .unwrap();
        let new_note = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "revised observation superseding the old one",
                0.9,
                None,
                vec![],
            )
            .await
            .unwrap();

        let result = rt
            .link(
                None,
                new_note.id,
                old_note.id,
                EdgeRelation::Supersedes,
                1.0,
            )
            .await;
        assert!(
            result.is_ok(),
            "note→note Supersedes must succeed (ADR-019 note supersession), got {result:?}"
        );
    }

    #[tokio::test]
    async fn link_supersedes_entity_to_entity_succeeds() {
        let rt = rt();
        let old_entity = rt
            .create_entity(None, "concept", "OldConcept", None, None, vec![])
            .await
            .unwrap();
        let new_entity = rt
            .create_entity(None, "concept", "NewConcept", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(
                None,
                new_entity.id,
                old_entity.id,
                EdgeRelation::Supersedes,
                1.0,
            )
            .await;
        assert!(
            result.is_ok(),
            "entity→entity Supersedes must succeed, got {result:?}"
        );
    }

    #[tokio::test]
    async fn link_supersedes_note_to_entity_returns_invalid_input() {
        let rt = rt();
        let note = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "a note",
                0.5,
                None,
                vec![],
            )
            .await
            .unwrap();
        let entity = rt
            .create_entity(None, "concept", "SomeEntity", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(None, note.id, entity.id, EdgeRelation::Supersedes, 1.0)
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(
                    msg.contains("same substrate") || msg.contains("same-substrate"),
                    "error must name the same-substrate rule: {msg}"
                );
            }
            other => panic!(
                "expected InvalidInput for note→entity Supersedes (cross-substrate), got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn link_supersedes_entity_to_note_returns_invalid_input() {
        let rt = rt();
        let entity = rt
            .create_entity(None, "concept", "SomeEntity", None, None, vec![])
            .await
            .unwrap();
        let note = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "a note",
                0.5,
                None,
                vec![],
            )
            .await
            .unwrap();

        let result = rt
            .link(None, entity.id, note.id, EdgeRelation::Supersedes, 1.0)
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(
                    msg.contains("same substrate") || msg.contains("same-substrate"),
                    "error must name the same-substrate rule: {msg}"
                );
            }
            other => panic!(
                "expected InvalidInput for entity→note Supersedes (cross-substrate), got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn link_supersedes_event_source_returns_invalid_input() {
        use khive_storage::Event;
        use khive_types::SubstrateKind;

        let rt = rt();
        let ns = rt.ns(None);
        let event = Event::new(ns, "test_verb", SubstrateKind::Entity, "test_actor");
        let event_id = event.id;
        rt.events(None).unwrap().append_event(event).await.unwrap();

        let entity = rt
            .create_entity(None, "concept", "SomeEntity", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(None, event_id, entity.id, EdgeRelation::Supersedes, 1.0)
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(msg.contains("event"), "error must mention 'event': {msg}");
            }
            other => {
                panic!("expected InvalidInput for event source with Supersedes, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn link_supersedes_event_target_returns_invalid_input() {
        use khive_storage::Event;
        use khive_types::SubstrateKind;

        let rt = rt();
        let ns = rt.ns(None);
        let event = Event::new(ns, "test_verb", SubstrateKind::Entity, "test_actor");
        let event_id = event.id;
        rt.events(None).unwrap().append_event(event).await.unwrap();

        let entity = rt
            .create_entity(None, "concept", "SomeEntity", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(None, entity.id, event_id, EdgeRelation::Supersedes, 1.0)
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(msg.contains("event"), "error must mention 'event': {msg}");
            }
            other => {
                panic!("expected InvalidInput for event target with Supersedes, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn link_supersedes_edge_source_returns_invalid_input() {
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
        let edge_uuid: Uuid = edge.id.into();

        let result = rt
            .link(None, edge_uuid, a.id, EdgeRelation::Supersedes, 1.0)
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(msg.contains("source"), "error must name 'source': {msg}");
            }
            other => {
                panic!("expected InvalidInput for edge-uuid source with Supersedes, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn link_supersedes_edge_target_returns_invalid_input() {
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
        let edge_uuid: Uuid = edge.id.into();

        let result = rt
            .link(None, a.id, edge_uuid, EdgeRelation::Supersedes, 1.0)
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(msg.contains("target"), "error must name 'target': {msg}");
            }
            other => {
                panic!("expected InvalidInput for edge-uuid target with Supersedes, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn link_supersedes_phantom_source_returns_not_found() {
        let rt = rt();
        let note = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "existing note",
                0.5,
                None,
                vec![],
            )
            .await
            .unwrap();
        let phantom = Uuid::new_v4();

        let result = rt
            .link(None, phantom, note.id, EdgeRelation::Supersedes, 1.0)
            .await;
        match result {
            Err(RuntimeError::NotFound(msg)) => {
                assert!(msg.contains("source"), "error must name 'source': {msg}");
            }
            other => panic!("expected NotFound for phantom source with Supersedes, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn link_supersedes_phantom_target_returns_not_found() {
        let rt = rt();
        let note = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "existing note",
                0.5,
                None,
                vec![],
            )
            .await
            .unwrap();
        let phantom = Uuid::new_v4();

        let result = rt
            .link(None, note.id, phantom, EdgeRelation::Supersedes, 1.0)
            .await;
        match result {
            Err(RuntimeError::NotFound(msg)) => {
                assert!(msg.contains("target"), "error must name 'target': {msg}");
            }
            other => panic!("expected NotFound for phantom target with Supersedes, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn link_supersedes_cross_namespace_source_returns_not_found() {
        let rt = rt();
        let note_a = rt
            .create_note(
                Some("ns-a"),
                khive_storage::NoteKind::Observation,
                None,
                "note in ns-a",
                0.5,
                None,
                vec![],
            )
            .await
            .unwrap();
        let note_b = rt
            .create_note(
                Some("ns-b"),
                khive_storage::NoteKind::Observation,
                None,
                "note in ns-b",
                0.5,
                None,
                vec![],
            )
            .await
            .unwrap();

        // From ns-a perspective, note_b is in a different namespace — treated as not found.
        let result = rt
            .link(
                Some("ns-a"),
                note_b.id,
                note_a.id,
                EdgeRelation::Supersedes,
                1.0,
            )
            .await;
        assert!(
            matches!(result, Err(RuntimeError::NotFound(_))),
            "cross-namespace source with Supersedes must return NotFound (fail-closed), got {result:?}"
        );
    }

    // Sanity: extends (non-annotates, non-supersedes) still requires entity→entity.
    #[tokio::test]
    async fn link_extends_note_source_still_returns_invalid_input() {
        let rt = rt();
        let note = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "a note that cannot be an extends source",
                0.5,
                None,
                vec![],
            )
            .await
            .unwrap();
        let entity = rt
            .create_entity(None, "concept", "E", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(None, note.id, entity.id, EdgeRelation::Extends, 1.0)
            .await;
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(_))),
            "note source with Extends must still return InvalidInput after this fix, got {result:?}"
        );
    }

    // Sanity: annotates note→edge still succeeds (unchanged path not broken by this fix).
    #[tokio::test]
    async fn link_annotates_note_to_edge_still_succeeds_after_fix() {
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
        let edge_uuid: Uuid = edge.id.into();

        let note = rt
            .create_note(
                None,
                khive_storage::NoteKind::Observation,
                None,
                "annotating an edge",
                0.5,
                None,
                vec![],
            )
            .await
            .unwrap();

        let result = rt
            .link(None, note.id, edge_uuid, EdgeRelation::Annotates, 1.0)
            .await;
        assert!(
            result.is_ok(),
            "note→edge Annotates must still succeed after supersedes fix, got {result:?}"
        );
    }
}
