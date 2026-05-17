//! KhiveMcpServer — rmcp-based MCP server wrapping KhiveRuntime.
//!
//! Implements the verb-consolidated MCP surface from ADR-023 + ADR-024.
//! 11 tools: create, get, list, update, delete, merge, search,
//! link, neighbors, traverse, query.

use std::str::FromStr;

use khive_storage::EdgeRelation;

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use uuid::Uuid;

use khive_runtime::KhiveRuntime;
use khive_runtime::{EdgeListFilter, EntityPatch, MergeStrategy};
use khive_storage::types::{Direction, TraversalOptions, TraversalRequest};

use crate::tools::{
    create::CreateParams,
    delete::DeleteParams,
    get::GetParams,
    graph::{LinkParams, NeighborsParams, TraverseParams},
    list::ListParams,
    merge::MergeParams,
    query::QueryParams,
    search::SearchParams,
    update::UpdateParams,
};

/// MCP server that wraps KhiveRuntime.
#[derive(Clone)]
pub struct KhiveMcpServer {
    runtime: KhiveRuntime,
}

impl KhiveMcpServer {
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }

    /// Serve over stdio (blocks until the connection closes).
    pub async fn serve_stdio(self) -> anyhow::Result<()> {
        use rmcp::transport::stdio;
        let service = self.serve(stdio()).await?;
        service.waiting().await?;
        Ok(())
    }

    /// Resolve a UUID from either a full string or a short 8+ hex-char prefix.
    /// Namespace-scoped: only matches records in the caller's namespace.
    async fn resolve_uuid(&self, s: &str, namespace: Option<&str>) -> Result<Uuid, McpError> {
        if let Ok(uuid) = Uuid::from_str(s) {
            return Ok(uuid);
        }
        if s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()) {
            match self.runtime.resolve_prefix(namespace, s).await {
                Ok(Some(uuid)) => return Ok(uuid),
                Ok(None) => {
                    return Err(McpError::invalid_params(
                        format!("no record matches prefix: {s:?}"),
                        None,
                    ))
                }
                Err(e) => return Err(McpError::invalid_params(format!("{e}"), None)),
            }
        }
        Err(McpError::invalid_params(
            format!("invalid UUID (expected full UUID or 8+ hex prefix): {s:?}"),
            None,
        ))
    }

    /// Map a direction string to the storage Direction enum.
    fn parse_direction(s: Option<&str>) -> Direction {
        match s {
            Some("in") => Direction::In,
            Some("both") => Direction::Both,
            _ => Direction::Out,
        }
    }

    /// Parse an EdgeRelation from a string, returning a descriptive MCP error on failure.
    fn parse_relation(s: &str) -> Result<EdgeRelation, McpError> {
        s.parse::<EdgeRelation>().map_err(|_| {
            McpError::invalid_params(
                format!(
                    "unknown relation {s:?}; valid: contains | part_of | instance_of | extends | \
                     variant_of | introduced_by | supersedes | depends_on | enables | implements | \
                     competes_with | composed_with | annotates"
                ),
                None,
            )
        })
    }

    /// Serialize a value to a pretty JSON string, returning an MCP error on failure.
    fn to_json<T: serde::Serialize>(v: &T) -> Result<String, McpError> {
        serde_json::to_string_pretty(v)
            .map_err(|e| McpError::internal_error(format!("serialize error: {e}"), None))
    }

    /// Map a RuntimeError to the correct MCP error kind.
    /// InvalidInput and NotFound are caller-correctable — invalid_params.
    /// Everything else is internal_error.
    fn validation_err(e: khive_runtime::RuntimeError) -> McpError {
        match e {
            khive_runtime::RuntimeError::InvalidInput(m)
            | khive_runtime::RuntimeError::NotFound(m) => McpError::invalid_params(m, None),
            other => McpError::internal_error(other.to_string(), None),
        }
    }
}

// ---- Tool implementations ----

#[tool_router]
impl KhiveMcpServer {
    // ---- create ----

    #[tool(description = r#"Create an entity or note in the knowledge graph.

kind="entity" — create a knowledge graph entity.
  Required: name, entity_kind
  entity_kind values: concept | document | dataset | project | person | org
    concept:  algorithms, techniques, models, architectures, research gaps
    document: papers, preprints, reports, blog posts
    dataset:  benchmarks, corpora, evaluation sets
    project:  codebases, libraries, tools, frameworks
    person:   researchers, engineers, authors
    org:      labs, companies, institutions
  Optional: description, properties (JSON), tags

kind="note" — create a lightweight text note.
  Required: content
  note_kind: observation (default) | insight | question | decision | reference
  Aliases: obs, finding, q, choice, ref, citation
  Optional: salience (0.0–1.0, default 0.5), annotates (UUIDs → creates annotates edges),
            properties (JSON)

Examples:
  Add algorithm:  {"kind":"entity","entity_kind":"concept","name":"FlashAttention","properties":{"domain":"attention"}}
  Add paper:      {"kind":"entity","entity_kind":"document","name":"Attention Is All You Need","properties":{"year":2017}}
  Add decision:   {"kind":"note","note_kind":"decision","content":"Use FlashAttention-2 for attention","salience":0.9}
  Annotating:     {"kind":"note","content":"Reduces memory by tiling","annotates":["<entity-uuid>"]}"#)]
    async fn create(&self, Parameters(p): Parameters<CreateParams>) -> Result<String, McpError> {
        match p.kind.as_str() {
            "entity" => {
                let name = p
                    .name
                    .ok_or_else(|| McpError::invalid_params("kind=entity requires 'name'", None))?;
                let entity_kind = p.entity_kind.ok_or_else(|| {
                    McpError::invalid_params(
                        "kind=entity requires 'entity_kind' (concept | document | dataset | project | person | org)",
                        None,
                    )
                })?;
                let tags = p.tags.unwrap_or_default();
                let entity = self
                    .runtime
                    .create_entity(
                        p.namespace.as_deref(),
                        &entity_kind,
                        &name,
                        p.description.as_deref(),
                        p.properties,
                        tags,
                    )
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                Self::to_json(&entity)
            }
            "note" => {
                let content = p.content.ok_or_else(|| {
                    McpError::invalid_params("kind=note requires 'content'", None)
                })?;
                let kind = match p.note_kind.as_deref() {
                    None | Some("") => "observation",
                    Some(s) => s,
                };
                let salience = p.salience.unwrap_or(0.5);
                let mut annotates = Vec::new();
                for s in p.annotates.unwrap_or_default() {
                    annotates.push(self.resolve_uuid(&s, p.namespace.as_deref()).await?);
                }
                let note = self
                    .runtime
                    .create_note(
                        p.namespace.as_deref(),
                        kind,
                        p.name.as_deref(),
                        &content,
                        salience,
                        p.properties,
                        annotates,
                    )
                    .await
                    .map_err(Self::validation_err)?;
                Self::to_json(&note)
            }
            other => Err(McpError::invalid_params(
                format!("unknown kind {other:?}; valid: entity | note"),
                None,
            )),
        }
    }

    // ---- get ----

    #[tool(
        description = r#"Fetch any record by UUID — automatically determines whether it's an entity, note, or edge.

Returns {"kind": "entity"|"note"|"edge", "data": {...}} if found.
Returns an error if no record with that UUID exists.

Examples:
  {"id":"<uuid>"}
  {"id":"<uuid>","namespace":"my-project"}"#
    )]
    async fn get(&self, Parameters(p): Parameters<GetParams>) -> Result<String, McpError> {
        let id = self.resolve_uuid(&p.id, p.namespace.as_deref()).await?;
        let ns = p.namespace.as_deref();

        // Try entity first.
        if let Some(entity) = self
            .runtime
            .get_entity(ns, id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
        {
            return Self::to_json(&serde_json::json!({"kind": "entity", "data": entity}));
        }

        // Try note.
        let note_store = self
            .runtime
            .notes(ns)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        if let Some(note) = note_store
            .get_note(id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
        {
            if note.namespace == self.runtime.ns(ns) {
                return Self::to_json(&serde_json::json!({"kind": "note", "data": note}));
            }
        }

        // Try edge.
        if let Some(edge) = self
            .runtime
            .get_edge(ns, id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
        {
            return Self::to_json(&serde_json::json!({"kind": "edge", "data": edge}));
        }

        Err(McpError::invalid_params(
            format!("not found: {}", p.id),
            None,
        ))
    }

    // ---- list ----

    #[tool(description = r#"List records with optional filtering.

kind="entity": list entities. Optional: entity_kind filter, limit (default 50, max 500).
kind="edge": list edges. Optional: source_id, target_id, relations, min_weight, max_weight, limit (default 100, max 1000).
kind="note": list notes. Optional: note_kind filter, limit (default 20, max 200).

Examples:
  All concepts:         {"kind":"entity","entity_kind":"concept"}
  Edges from node:      {"kind":"edge","source_id":"<uuid>","relations":["depends_on","extends"]}
  Recent decisions:     {"kind":"note","note_kind":"decision","limit":10}
  High-weight edges:    {"kind":"edge","min_weight":0.8}"#)]
    async fn list(&self, Parameters(p): Parameters<ListParams>) -> Result<String, McpError> {
        match p.kind.as_str() {
            "entity" => {
                let limit = p.limit.unwrap_or(50).min(500);
                let entities = self
                    .runtime
                    .list_entities(p.namespace.as_deref(), p.entity_kind.as_deref(), limit)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                Self::to_json(&entities)
            }
            "edge" => {
                let source_id = match p.source_id.as_deref() {
                    Some(s) => Some(self.resolve_uuid(s, p.namespace.as_deref()).await?),
                    None => None,
                };
                let target_id = match p.target_id.as_deref() {
                    Some(s) => Some(self.resolve_uuid(s, p.namespace.as_deref()).await?),
                    None => None,
                };
                let relations: Vec<EdgeRelation> = p
                    .relations
                    .unwrap_or_default()
                    .into_iter()
                    .map(|s| Self::parse_relation(&s))
                    .collect::<Result<Vec<_>, _>>()?;
                let filter = EdgeListFilter {
                    source_id,
                    target_id,
                    relations,
                    min_weight: p.min_weight,
                    max_weight: p.max_weight,
                };
                let limit = p.limit.unwrap_or(100);
                let edges = self
                    .runtime
                    .list_edges(p.namespace.as_deref(), filter, limit)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                Self::to_json(&edges)
            }
            "note" => {
                let kind_str = match p.note_kind.as_deref() {
                    None | Some("") => None,
                    Some(s) => Some(s),
                };
                let limit = p.limit.unwrap_or(20).min(200);
                let notes = self
                    .runtime
                    .list_notes(p.namespace.as_deref(), kind_str, limit)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                Self::to_json(&notes)
            }
            other => Err(McpError::invalid_params(
                format!("unknown kind {other:?}; valid: entity | edge | note"),
                None,
            )),
        }
    }

    // ---- update ----

    #[tool(
        description = r#"Patch-update an entity or edge. Only fields you provide are changed. Kind is determined from the UUID.

entity fields (description: omit=unchanged, null=clear, string=set):
  name, description, properties (wholesale replace), tags (wholesale replace)

edge fields:
  relation: one of the 13 canonical ADR-002 relations
  weight: float in [0.0, 1.0]

Examples:
  Rename entity:     {"id":"<uuid>","name":"FlashAttention-2"}
  Clear description: {"id":"<uuid>","description":null}
  Set properties:    {"id":"<uuid>","properties":{"type":"algorithm","status":"shipped"}}
  Correct relation:  {"id":"<uuid>","relation":"extends"}
  Adjust weight:     {"id":"<uuid>","weight":0.7}"#
    )]
    async fn update(&self, Parameters(p): Parameters<UpdateParams>) -> Result<String, McpError> {
        let id = self.resolve_uuid(&p.id, p.namespace.as_deref()).await?;
        let ns = p.namespace.as_deref();

        // Try entity first.
        if self
            .runtime
            .get_entity(ns, id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .is_some()
        {
            let description = match p.description {
                None => None,
                Some(serde_json::Value::Null) => Some(None),
                Some(serde_json::Value::String(s)) => Some(Some(s)),
                Some(other) => {
                    return Err(McpError::invalid_params(
                        format!("description must be null or a string, got: {other}"),
                        None,
                    ))
                }
            };
            let patch = EntityPatch {
                name: p.name,
                description,
                properties: p.properties,
                tags: p.tags,
            };
            let entity = self
                .runtime
                .update_entity(ns, id, patch)
                .await
                .map_err(|e| match e {
                    khive_runtime::RuntimeError::NotFound(msg) => {
                        McpError::invalid_params(format!("entity not found: {msg}"), None)
                    }
                    other => McpError::internal_error(other.to_string(), None),
                })?;
            return Self::to_json(&entity);
        }

        // Try edge.
        if self
            .runtime
            .get_edge(ns, id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .is_some()
        {
            let relation = p
                .relation
                .as_deref()
                .map(Self::parse_relation)
                .transpose()?;
            let edge = self
                .runtime
                .update_edge(ns, id, relation, p.weight)
                .await
                .map_err(|e| match e {
                    khive_runtime::RuntimeError::NotFound(msg) => {
                        McpError::invalid_params(format!("edge not found: {msg}"), None)
                    }
                    khive_runtime::RuntimeError::InvalidInput(msg) => {
                        McpError::invalid_params(msg, None)
                    }
                    other => McpError::internal_error(other.to_string(), None),
                })?;
            return Self::to_json(&edge);
        }

        Err(McpError::invalid_params(
            format!("not found: {}", p.id),
            None,
        ))
    }

    // ---- delete ----

    #[tool(
        description = r#"Delete a record by UUID. Kind is determined automatically from the UUID.

Entity and note: soft-delete by default (recoverable); set hard=true for permanent removal.
Edge: always hard-deleted (edges have no soft-delete state).

Returns {"deleted": true|false, "id": "<uuid>"}.

Examples:
  Soft-delete:  {"id":"<uuid>"}
  Hard-delete:  {"id":"<uuid>","hard":true}"#
    )]
    async fn delete(&self, Parameters(p): Parameters<DeleteParams>) -> Result<String, McpError> {
        let id = self.resolve_uuid(&p.id, p.namespace.as_deref()).await?;
        let ns = p.namespace.as_deref();

        // Try entity first.
        if self
            .runtime
            .get_entity(ns, id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .is_some()
        {
            let deleted = self
                .runtime
                .delete_entity(ns, id, p.hard.unwrap_or(false))
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Self::to_json(&serde_json::json!({ "deleted": deleted, "id": p.id }));
        }

        // Try edge.
        if self
            .runtime
            .get_edge(ns, id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .is_some()
        {
            let deleted = self
                .runtime
                .delete_edge(ns, id)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Self::to_json(&serde_json::json!({ "deleted": deleted, "id": p.id }));
        }

        // Try note — delete_note enforces namespace isolation and returns false
        // for both "not found" and "wrong namespace" (both surface as not-found).
        let deleted_note = self
            .runtime
            .delete_note(ns, id, p.hard.unwrap_or(false))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        if deleted_note {
            return Self::to_json(&serde_json::json!({ "deleted": true, "id": p.id }));
        }

        Err(McpError::invalid_params(
            format!("not found: {}", p.id),
            None,
        ))
    }

    // ---- merge ----

    #[tool(
        description = r#"Merge two entity records: rewire all edges from `from_id` to `into_id`, merge properties, hard-delete `from_id`.

v0.1: entity-only. Note merge is deferred past v0.1.

Use when you discover two entity records describe the same thing (deduplication).
Compare with `supersede` (deferred past v0.1) which preserves the old record as history.

strategy: prefer_into (default) | prefer_from | union
  prefer_into: into's values win on conflict; from fills in missing keys
  prefer_from: from's values win on conflict
  union: deep object merge; scalar conflicts go to into

Returns: {kept_id, removed_id, edges_rewired, properties_merged, tags_unioned}.

Warning: not atomic in v0.1 — re-run with same args to recover from mid-way failures.

Example:
  {"into_id":"<uuid>","from_id":"<uuid>","strategy":"prefer_into"}"#
    )]
    async fn merge(&self, Parameters(p): Parameters<MergeParams>) -> Result<String, McpError> {
        let into_id = self
            .resolve_uuid(&p.into_id, p.namespace.as_deref())
            .await?;
        let from_id = self
            .resolve_uuid(&p.from_id, p.namespace.as_deref())
            .await?;
        let strategy = match p.strategy.as_deref().unwrap_or("prefer_into") {
            "prefer_into" => MergeStrategy::PreferInto,
            "prefer_from" => MergeStrategy::PreferFrom,
            "union" => MergeStrategy::Union,
            other => {
                return Err(McpError::invalid_params(
                    format!("unknown strategy {other:?}; use prefer_into | prefer_from | union"),
                    None,
                ))
            }
        };
        let summary = self
            .runtime
            .merge_entity(p.namespace.as_deref(), into_id, from_id, strategy)
            .await
            .map_err(|e| match e {
                khive_runtime::RuntimeError::NotFound(msg) => {
                    McpError::invalid_params(format!("entity not found: {msg}"), None)
                }
                other => McpError::internal_error(other.to_string(), None),
            })?;
        Self::to_json(&summary)
    }

    // ---- search ----

    #[tool(description = r#"Semantic/hybrid search across entities or notes.

kind="entity": hybrid search (FTS5 text + optional vector) across all entities.
kind="note": hybrid search with salience weighting (ADR-024).
  - FTS5 text search + vector similarity (if embedding model configured).
  - Fused via Reciprocal Rank Fusion (k=60).
  - Note results reranked: score *= (0.5 + 0.5 * salience).
  - Soft-deleted and superseded notes are excluded.

limit: default 10, max 100.

Examples:
  Find entities:   {"kind":"entity","query":"memory efficient attention mechanism"}
  Find notes:      {"kind":"note","query":"LoRA fine-tuning parameter efficiency","limit":5}
  Find papers:     {"kind":"entity","query":"FlashAttention IO-aware attention","limit":20}"#)]
    async fn search(&self, Parameters(p): Parameters<SearchParams>) -> Result<String, McpError> {
        let limit = p.limit.unwrap_or(10).min(100);
        match p.kind.as_str() {
            "entity" => {
                let query_vector = if self.runtime.config().embedding_model.is_some() {
                    Some(
                        self.runtime
                            .embed(&p.query)
                            .await
                            .map_err(|e| McpError::internal_error(e.to_string(), None))?,
                    )
                } else {
                    None
                };
                let hits = self
                    .runtime
                    .hybrid_search(p.namespace.as_deref(), &p.query, query_vector, limit)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                let result: Vec<serde_json::Value> = hits
                    .iter()
                    .map(|h| {
                        serde_json::json!({
                            "entity_id": h.entity_id.to_string(),
                            "score": h.score.to_f64(),
                            "title": h.title,
                            "snippet": h.snippet,
                        })
                    })
                    .collect();
                Self::to_json(&result)
            }
            "note" => {
                let query_vector = if self.runtime.config().embedding_model.is_some() {
                    Some(
                        self.runtime
                            .embed(&p.query)
                            .await
                            .map_err(|e| McpError::internal_error(e.to_string(), None))?,
                    )
                } else {
                    None
                };
                let hits = self
                    .runtime
                    .search_notes(p.namespace.as_deref(), &p.query, query_vector, limit)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                // Return note_ids and scores as JSON.
                let result: Vec<serde_json::Value> = hits
                    .iter()
                    .map(|h| {
                        serde_json::json!({
                            "note_id": h.note_id.to_string(),
                            "score": h.score.to_f64(),
                        })
                    })
                    .collect();
                Self::to_json(&result)
            }
            other => Err(McpError::invalid_params(
                format!("unknown kind {other:?}; valid: entity | note"),
                None,
            )),
        }
    }

    // ---- link ----

    #[tool(description = r#"Create a directed edge between two nodes.

Canonical relations (13 total — use ONLY these):
  Structure:      contains | part_of | instance_of
  Derivation:     extends | variant_of | introduced_by | supersedes
  Dependency:     depends_on | enables
  Implementation: implements
  Lateral:        competes_with | composed_with
  Annotation:     annotates

Weight guide: 1.0=definitional, 0.7-0.9=strong, 0.4-0.6=plausible, <0.4=speculative

annotates edges: source must be a note; target may be an entity, note, edge, or event (cross-substrate navigation per ADR-024).

Examples:
  {"source_id":"<uuid>","target_id":"<uuid>","relation":"introduced_by","weight":1.0}
  {"source_id":"<LoRA-uuid>","target_id":"<QLoRA-uuid>","relation":"variant_of","weight":0.9}
  {"source_id":"<note-uuid>","target_id":"<entity-uuid>","relation":"annotates","weight":1.0}"#)]
    async fn link(&self, Parameters(p): Parameters<LinkParams>) -> Result<String, McpError> {
        let source = self
            .resolve_uuid(&p.source_id, p.namespace.as_deref())
            .await?;
        let target = self
            .resolve_uuid(&p.target_id, p.namespace.as_deref())
            .await?;
        let weight = p.weight.unwrap_or(1.0).clamp(0.0, 1.0);
        let relation = Self::parse_relation(&p.relation)?;
        let edge = self
            .runtime
            .link(p.namespace.as_deref(), source, target, relation, weight)
            .await
            .map_err(Self::validation_err)?;
        Self::to_json(&edge)
    }

    // ---- neighbors ----

    #[tool(description = r#"Get immediate neighbors of a node.

direction: out (default) | in | both

Use relations=["annotates"] to find notes that annotate a given entity (cross-substrate navigation per ADR-024).

Examples:
  {"node_id":"<uuid>","direction":"out"}
  {"node_id":"<entity-uuid>","direction":"in","relations":["annotates"]}
  {"node_id":"<uuid>","direction":"both","limit":20}"#)]
    async fn neighbors(
        &self,
        Parameters(p): Parameters<NeighborsParams>,
    ) -> Result<String, McpError> {
        let node_id = self
            .resolve_uuid(&p.node_id, p.namespace.as_deref())
            .await?;
        let direction = Self::parse_direction(p.direction.as_deref());
        let relations: Option<Vec<EdgeRelation>> = p
            .relations
            .map(|v| {
                v.into_iter()
                    .map(|s| Self::parse_relation(&s))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let hits = self
            .runtime
            .neighbors(
                p.namespace.as_deref(),
                node_id,
                direction,
                p.limit,
                relations,
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Self::to_json(&hits)
    }

    // ---- traverse ----

    #[tool(description = r#"Multi-hop graph traversal from root nodes.

Returns all nodes reachable within max_depth hops.

Example — find all papers that influenced FlashAttention (2 hops):
  {"roots":["<flashattn-uuid>"],"max_depth":2,"direction":"in","relations":["introduced_by","extends"]}

Example — expand annotation graph from an entity:
  {"roots":["<entity-uuid>"],"direction":"in","relations":["annotates"],"max_depth":1}"#)]
    async fn traverse(
        &self,
        Parameters(p): Parameters<TraverseParams>,
    ) -> Result<String, McpError> {
        let mut roots = Vec::with_capacity(p.roots.len());
        for s in &p.roots {
            roots.push(self.resolve_uuid(s, p.namespace.as_deref()).await?);
        }
        let direction = Self::parse_direction(p.direction.as_deref());
        let relations: Option<Vec<EdgeRelation>> = p
            .relations
            .map(|v| {
                v.into_iter()
                    .map(|s| Self::parse_relation(&s))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let options = TraversalOptions {
            max_depth: p.max_depth.unwrap_or(3),
            direction,
            relations,
            min_weight: None,
            limit: None,
        };
        let request = TraversalRequest {
            roots,
            options,
            include_roots: p.include_roots.unwrap_or(true),
        };
        let paths = self
            .runtime
            .traverse(p.namespace.as_deref(), request)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Self::to_json(&paths)
    }

    // ---- query ----

    #[tool(
        description = r#"Execute a GQL or SPARQL query against the knowledge graph.

GQL syntax (preferred):
  MATCH (a:concept)-[e:extends]->(b:concept) RETURN a.name, b.name LIMIT 10
  MATCH (p:document)-[e:introduced_by]->(a:concept) WHERE a.name = "LoRA" RETURN p.name

SPARQL syntax:
  SELECT ?a ?name WHERE { ?a :kind "concept" ; :name ?name . }

Returns raw SQL rows as JSON objects with column names as keys."#
    )]
    async fn query(&self, Parameters(p): Parameters<QueryParams>) -> Result<String, McpError> {
        let rows = self
            .runtime
            .query(p.namespace.as_deref(), &p.query)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Self::to_json(&rows)
    }
}

#[tool_handler]
impl ServerHandler for KhiveMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "khive knowledge graph — verb-consolidated MCP surface (ADR-023 + ADR-024). \
                 11 tools: create, get, list, update, delete, merge, search (CRUD verbs) \
                 + link, neighbors, traverse, query (graph verbs). \
                 get/update/delete/merge auto-detect record kind from UUID. \
                 All operations are namespace-scoped.",
            )
    }
}
