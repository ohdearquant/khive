//! KhiveMcpServer — rmcp-based MCP server wrapping KhiveRuntime.

use std::str::FromStr;

use khive_runtime::NoteKind;
use khive_storage::EdgeRelation;

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use uuid::Uuid;

use khive_runtime::KhiveRuntime;
use khive_storage::types::{Direction, TraversalOptions, TraversalRequest};

use khive_runtime::{EdgeListFilter, EntityPatch, MergeStrategy};

use crate::tools::{
    edge::{EdgeDeleteParams, EdgeGetParams, EdgeListParams, EdgeUpdateParams},
    entity::{EntityCreateParams, EntityDeleteParams, EntityGetParams, EntityListParams},
    entity_curation::{EntityMergeParams, EntityUpdateParams},
    graph::{LinkParams, NeighborsParams, TraverseParams},
    note::{NoteCreateParams, NoteListParams},
    query::QueryParams,
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

    /// Parse a UUID from a string, returning an MCP error on failure.
    fn parse_uuid(s: &str) -> Result<Uuid, McpError> {
        Uuid::from_str(s)
            .map_err(|_| McpError::invalid_params(format!("invalid UUID: {s:?}"), None))
    }

    /// Map a direction string to the storage Direction enum.
    fn parse_direction(s: Option<&str>) -> Direction {
        match s {
            Some("in") => Direction::In,
            Some("both") => Direction::Both,
            _ => Direction::Out,
        }
    }

    /// Serialize a value to a pretty JSON string, returning an MCP error on failure.
    fn to_json<T: serde::Serialize>(v: &T) -> Result<String, McpError> {
        serde_json::to_string_pretty(v)
            .map_err(|e| McpError::internal_error(format!("serialize error: {e}"), None))
    }
}

// ---- Tool implementations ----

#[tool_router]
impl KhiveMcpServer {
    // ---- Entity tools ----

    #[tool(description = r#"Create an entity in the knowledge graph.

Kinds (use one): concept | document | dataset | project | person | org

- concept: algorithms, techniques, models, architectures, research gaps
- document: papers, preprints, reports, blog posts (has title/authors/year)
- dataset: benchmarks, corpora, evaluation sets
- project: codebases, libraries, tools, frameworks
- person: researchers, engineers, authors
- org: labs, companies, institutions

Examples:
  Add a paper:     {"kind":"document","name":"Attention Is All You Need","properties":{"authors":"Vaswani et al.","year":2017}}
  Add algorithm:   {"kind":"concept","name":"FlashAttention","properties":{"type":"algorithm","domain":"attention"}}
  Add a library:   {"kind":"project","name":"PyTorch","properties":{"language":"Python","repo":"https://github.com/pytorch/pytorch"}}"#)]
    async fn entity_create(
        &self,
        Parameters(p): Parameters<EntityCreateParams>,
    ) -> Result<String, McpError> {
        let tags = p.tags.unwrap_or_default();
        let entity = self
            .runtime
            .create_entity(
                p.namespace.as_deref(),
                &p.kind,
                &p.name,
                p.description.as_deref(),
                p.properties,
                tags,
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Self::to_json(&entity)
    }

    #[tool(description = "Get a single entity by UUID.")]
    async fn entity_get(
        &self,
        Parameters(p): Parameters<EntityGetParams>,
    ) -> Result<String, McpError> {
        let id = Self::parse_uuid(&p.id)?;
        let result = self
            .runtime
            .get_entity(p.namespace.as_deref(), id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        match result {
            Some(entity) => Self::to_json(&entity),
            None => Err(McpError::invalid_params(
                format!("entity not found: {}", p.id),
                None,
            )),
        }
    }

    #[tool(
        description = "List entities in a namespace, optionally filtered by kind. Returns up to `limit` results (default 50, max 500)."
    )]
    async fn entity_list(
        &self,
        Parameters(p): Parameters<EntityListParams>,
    ) -> Result<String, McpError> {
        let limit = p.limit.unwrap_or(50).min(500);
        let entities = self
            .runtime
            .list_entities(p.namespace.as_deref(), p.kind.as_deref(), limit)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Self::to_json(&entities)
    }

    #[tool(
        description = "Delete an entity by UUID. Soft-delete by default (recoverable); set hard=true for permanent removal."
    )]
    async fn entity_delete(
        &self,
        Parameters(p): Parameters<EntityDeleteParams>,
    ) -> Result<String, McpError> {
        let id = Self::parse_uuid(&p.id)?;
        let deleted = self
            .runtime
            .delete_entity(p.namespace.as_deref(), id, p.hard.unwrap_or(false))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Self::to_json(&serde_json::json!({ "deleted": deleted, "id": p.id }))
    }

    // ---- Graph / edge tools ----

    #[tool(description = r#"Create a directed edge between two entities.

Canonical relations (13 total — use ONLY these):
  Structure:      contains | part_of | instance_of
  Derivation:     extends | variant_of | introduced_by | supersedes
  Dependency:     depends_on | enables
  Implementation: implements
  Lateral:        competes_with | composed_with
  Annotation:     annotates

Weight guide: 1.0=definitional, 0.7-0.9=strong, 0.4-0.6=plausible, <0.4=speculative

Examples:
  {"source_id":"<uuid>","target_id":"<uuid>","relation":"introduced_by","weight":1.0}
  {"source_id":"<LoRA-uuid>","target_id":"<QLoRA-uuid>","relation":"variant_of","weight":0.9}"#)]
    async fn link(&self, Parameters(p): Parameters<LinkParams>) -> Result<String, McpError> {
        let source = Self::parse_uuid(&p.source_id)?;
        let target = Self::parse_uuid(&p.target_id)?;
        let weight = p.weight.unwrap_or(1.0).clamp(0.0, 1.0);
        let relation = p.relation.parse::<EdgeRelation>().map_err(|_| {
            McpError::invalid_params(
                format!(
                    "unknown relation {:?}; must be one of: contains | part_of | instance_of | extends | variant_of | introduced_by | supersedes | depends_on | enables | implements | competes_with | composed_with",
                    p.relation
                ),
                None,
            )
        })?;
        let edge = self
            .runtime
            .link(p.namespace.as_deref(), source, target, relation, weight)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Self::to_json(&edge)
    }

    #[tool(
        description = "Get immediate neighbors of a node. direction=out|in|both (default: out)."
    )]
    async fn neighbors(
        &self,
        Parameters(p): Parameters<NeighborsParams>,
    ) -> Result<String, McpError> {
        let node_id = Self::parse_uuid(&p.node_id)?;
        let direction = Self::parse_direction(p.direction.as_deref());
        let relations: Option<Vec<EdgeRelation>> = p.relations.map(|v| {
            v.into_iter()
                .filter_map(|s| s.parse::<EdgeRelation>().ok())
                .collect()
        });
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

    #[tool(description = r#"Multi-hop graph traversal from root nodes.

Returns all nodes reachable within max_depth hops.

Example — find all papers that influenced FlashAttention (2 hops):
  {"roots":["<flashattn-uuid>"],"max_depth":2,"direction":"in","relations":["introduced_by","extends"]}"#)]
    async fn traverse(
        &self,
        Parameters(p): Parameters<TraverseParams>,
    ) -> Result<String, McpError> {
        let roots = p
            .roots
            .iter()
            .map(|s| Self::parse_uuid(s))
            .collect::<Result<Vec<Uuid>, _>>()?;
        let direction = Self::parse_direction(p.direction.as_deref());
        let relations: Option<Vec<EdgeRelation>> = p.relations.map(|v| {
            v.into_iter()
                .filter_map(|s| s.parse::<EdgeRelation>().ok())
                .collect()
        });
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

    // ---- Note tools ----

    #[tool(description = r#"Create a note in the knowledge graph.

Notes are lightweight text records. Kinds (5 total — use one):
  observation (default) | insight | question | decision | reference
Aliases accepted: obs, finding, q, choice, ref, citation

Optional: `annotates` takes UUIDs of entities/notes this note is about (creates edges).

Examples:
  {"kind":"decision","content":"Use FlashAttention-2 for attention — 2x faster on A100","salience":0.9}
  {"kind":"obs","content":"Benchmark in §4.2 uses English-only data","salience":0.6,"annotates":["<entity-uuid>"]}
  {"content":"Noticed unusual latency spike at batch_size=512"}"#)]
    async fn note_create(
        &self,
        Parameters(p): Parameters<NoteCreateParams>,
    ) -> Result<String, McpError> {
        let kind = match p.kind.as_deref() {
            None | Some("") => NoteKind::Observation,
            Some(s) => NoteKind::from_str(s).map_err(|_| {
                McpError::invalid_params(
                    format!(
                        "invalid kind {s:?}. Valid: observation | insight | question | decision | reference (aliases: obs, finding, q, choice, ref, citation)"
                    ),
                    None,
                )
            })?,
        };
        let salience = p.salience.unwrap_or(0.5);
        let annotates = p
            .annotates
            .unwrap_or_default()
            .iter()
            .map(|s| Self::parse_uuid(s))
            .collect::<Result<Vec<_>, _>>()?;
        let note = self
            .runtime
            .create_note(
                p.namespace.as_deref(),
                kind,
                &p.content,
                salience,
                p.properties,
                annotates,
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Self::to_json(&note)
    }

    #[tool(
        description = r#"List notes from the knowledge graph, optionally filtered by kind.

Kinds: observation | insight | question | decision | reference
Aliases accepted: obs, finding, q, choice, ref, citation
Returns up to `limit` notes (default 20, max 200)."#
    )]
    async fn note_list(
        &self,
        Parameters(p): Parameters<NoteListParams>,
    ) -> Result<String, McpError> {
        let kind_str = match p.kind.as_deref() {
            None | Some("") => None,
            Some(s) => {
                let k = NoteKind::from_str(s).map_err(|_| {
                    McpError::invalid_params(
                        format!(
                            "invalid kind {s:?}. Valid: observation | insight | question | decision | reference (aliases: obs, finding, q, choice, ref, citation)"
                        ),
                        None,
                    )
                })?;
                Some(k.to_string())
            }
        };
        let limit = p.limit.unwrap_or(20).min(200);
        let notes = self
            .runtime
            .list_notes(p.namespace.as_deref(), kind_str.as_deref(), limit)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Self::to_json(&notes)
    }

    // ---- Query tool ----

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

    // ---- Entity curation tools (ADR-014) ----

    #[tool(
        description = r#"Patch-update an entity. Only fields you provide are changed — omitted fields are left as-is.

description rules: omit the key = leave unchanged | null = clear | string = set to that value
properties: wholesale replace if provided (fetch first, modify, then send the full object)
tags: wholesale replace if provided

Example — rename only:
  {"id":"<uuid>","name":"FlashAttention-2"}
Example — clear description:
  {"id":"<uuid>","description":null}
Example — set properties, leave name and description:
  {"id":"<uuid>","properties":{"type":"algorithm","domain":"attention","status":"shipped"}}"#
    )]
    async fn entity_update(
        &self,
        Parameters(p): Parameters<EntityUpdateParams>,
    ) -> Result<String, McpError> {
        let id = Self::parse_uuid(&p.id)?;
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
            .update_entity(p.namespace.as_deref(), id, patch)
            .await
            .map_err(|e| match e {
                khive_runtime::RuntimeError::NotFound(msg) => {
                    McpError::invalid_params(format!("entity not found: {msg}"), None)
                }
                other => McpError::internal_error(other.to_string(), None),
            })?;
        Self::to_json(&entity)
    }

    #[tool(
        description = r#"Merge two entities: rewire all edges from `from_id` to `into_id`, merge properties, hard-delete `from_id`.

Use when you discover two entities represent the same concept (deduplication).

strategy: prefer_into (default) | prefer_from | union
  prefer_into: into's values win on conflict; from fills in missing keys
  prefer_from: from's values win on conflict
  union: deep object merge; scalar conflicts go to into

Returns a summary with counts: kept_id, removed_id, edges_rewired, properties_merged, tags_unioned.

Warning: not atomic in v0.1 — re-run with the same args to recover from mid-way failures."#
    )]
    async fn entity_merge(
        &self,
        Parameters(p): Parameters<EntityMergeParams>,
    ) -> Result<String, McpError> {
        let into_id = Self::parse_uuid(&p.into_id)?;
        let from_id = Self::parse_uuid(&p.from_id)?;
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

    // ---- Edge CRUD tools (ADR-014) ----

    #[tool(description = "Fetch a single edge by UUID. Returns null if the edge does not exist.")]
    async fn edge_get(&self, Parameters(p): Parameters<EdgeGetParams>) -> Result<String, McpError> {
        let id = Self::parse_uuid(&p.id)?;
        let result = self
            .runtime
            .get_edge(p.namespace.as_deref(), id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        match result {
            Some(edge) => Self::to_json(&edge),
            None => Self::to_json(&serde_json::Value::Null),
        }
    }

    #[tool(
        description = r#"List edges with optional filtering. Useful for exploring what an entity connects to.

All filter fields are optional — omit to return all edges.
limit: default 100, max 1000.

Example — what does FlashAttention depend on?
  {"source_id":"<flashattn-uuid>","relations":["depends_on","extends"]}
Example — all edges with weight ≥ 0.8:
  {"min_weight":0.8}"#
    )]
    async fn edge_list(
        &self,
        Parameters(p): Parameters<EdgeListParams>,
    ) -> Result<String, McpError> {
        let source_id = p.source_id.as_deref().map(Self::parse_uuid).transpose()?;
        let target_id = p.target_id.as_deref().map(Self::parse_uuid).transpose()?;
        let relations: Vec<EdgeRelation> = p
            .relations
            .unwrap_or_default()
            .into_iter()
            .map(|s| {
                s.parse::<EdgeRelation>().map_err(|_| {
                    McpError::invalid_params(
                        format!(
                            "unknown relation {:?}; valid: contains | part_of | instance_of | extends | variant_of | introduced_by | supersedes | depends_on | enables | implements | competes_with | composed_with | annotates",
                            s
                        ),
                        None,
                    )
                })
            })
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

    #[tool(
        description = r#"Patch-update an edge's relation or weight. Only fields you provide are changed.

relation must be one of the 13 canonical ADR-002 relations:
  contains | part_of | instance_of | extends | variant_of | introduced_by | supersedes
  depends_on | enables | implements | competes_with | composed_with | annotates

Example — correct a mistyped relation:
  {"id":"<edge-uuid>","relation":"extends"}
Example — adjust weight:
  {"id":"<edge-uuid>","weight":0.7}"#
    )]
    async fn edge_update(
        &self,
        Parameters(p): Parameters<EdgeUpdateParams>,
    ) -> Result<String, McpError> {
        let id = Self::parse_uuid(&p.id)?;
        let relation = p
            .relation
            .as_deref()
            .map(|s| {
                s.parse::<EdgeRelation>().map_err(|_| {
                    McpError::invalid_params(
                        format!(
                            "unknown relation {s:?}; must be one of: contains | part_of | instance_of | extends | variant_of | introduced_by | supersedes | depends_on | enables | implements | competes_with | composed_with"
                        ),
                        None,
                    )
                })
            })
            .transpose()?;
        let edge = self
            .runtime
            .update_edge(p.namespace.as_deref(), id, relation, p.weight)
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
        Self::to_json(&edge)
    }

    #[tool(
        description = "Hard-delete an edge by UUID. Returns {\"deleted\": true, \"id\": \"...\"}. Edges have no soft-delete."
    )]
    async fn edge_delete(
        &self,
        Parameters(p): Parameters<EdgeDeleteParams>,
    ) -> Result<String, McpError> {
        let id = Self::parse_uuid(&p.id)?;
        let deleted = self
            .runtime
            .delete_edge(p.namespace.as_deref(), id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Self::to_json(&serde_json::json!({ "deleted": deleted, "id": p.id }))
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
                "khive knowledge graph operations: entity CRUD (concept/document/dataset/project/person/org), \
                 graph edges (13 canonical relations), memory notes, and GQL/SPARQL queries. \
                 All operations are namespace-scoped."
            )
    }
}
