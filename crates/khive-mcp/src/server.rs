//! KhiveMcpServer — rmcp-based MCP server routing through VerbRegistry (ADR-025 step 5).
//!
//! The MCP layer is a thin translation shell: it provides rich tool schemas for MCP
//! clients, serializes typed params to JSON Value, dispatches through the VerbRegistry,
//! and maps RuntimeError to MCP error codes. All business logic lives in packs.

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};

use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder};

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

/// MCP server that dispatches all verbs through a VerbRegistry.
#[derive(Clone)]
pub struct KhiveMcpServer {
    registry: VerbRegistry,
}

impl KhiveMcpServer {
    pub fn new(runtime: KhiveRuntime) -> Self {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime));
        Self {
            registry: builder.build(),
        }
    }

    /// Serve over stdio (blocks until the connection closes).
    pub async fn serve_stdio(self) -> anyhow::Result<()> {
        use rmcp::transport::stdio;
        let service = self.serve(stdio()).await?;
        service.waiting().await?;
        Ok(())
    }

    /// Dispatch a verb with serialized params, mapping errors to MCP error codes.
    async fn dispatch(&self, verb: &str, params: serde_json::Value) -> Result<String, McpError> {
        let result = self
            .registry
            .dispatch(verb, params)
            .await
            .map_err(Self::runtime_err)?;
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))
    }

    /// Map RuntimeError to the appropriate MCP error kind.
    fn runtime_err(e: RuntimeError) -> McpError {
        match e {
            RuntimeError::InvalidInput(m) | RuntimeError::NotFound(m) => {
                McpError::invalid_params(m, None)
            }
            other => McpError::internal_error(other.to_string(), None),
        }
    }
}

// ---- Tool implementations ----

#[tool_router]
impl KhiveMcpServer {
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
        let params =
            serde_json::to_value(p).map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.dispatch("create", params).await
    }

    #[tool(
        description = r#"Fetch any record by UUID — automatically determines whether it's an entity, note, or edge.

Returns {"kind": "entity"|"note"|"edge", "data": {...}} if found.
Returns an error if no record with that UUID exists.

Examples:
  {"id":"<uuid>"}
  {"id":"<uuid>","namespace":"my-project"}"#
    )]
    async fn get(&self, Parameters(p): Parameters<GetParams>) -> Result<String, McpError> {
        let params =
            serde_json::to_value(p).map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.dispatch("get", params).await
    }

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
        let params =
            serde_json::to_value(p).map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.dispatch("list", params).await
    }

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
        let params =
            serde_json::to_value(p).map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.dispatch("update", params).await
    }

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
        let params =
            serde_json::to_value(p).map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.dispatch("delete", params).await
    }

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
        let params =
            serde_json::to_value(p).map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.dispatch("merge", params).await
    }

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
        let params =
            serde_json::to_value(p).map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.dispatch("search", params).await
    }

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
        let params =
            serde_json::to_value(p).map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.dispatch("link", params).await
    }

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
        let params =
            serde_json::to_value(p).map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.dispatch("neighbors", params).await
    }

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
        let params =
            serde_json::to_value(p).map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.dispatch("traverse", params).await
    }

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
        let params =
            serde_json::to_value(p).map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.dispatch("query", params).await
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
