//! High-level operations composing storage capabilities into user-facing verbs.

use std::collections::HashMap;
use std::str::FromStr;

use serde::Serialize;
use uuid::Uuid;

use khive_score::{rrf_score, DeterministicScore};
use khive_storage::note::Note;
use khive_storage::types::{
    DeleteMode, Direction, EdgeSortField, GraphPath, LinkId, NeighborHit, NeighborQuery, Page,
    PageRequest, SortOrder, SqlRow, SqlStatement, TextDocument, TextFilter, TextQueryMode,
    TextSearchRequest, TraversalRequest,
};
use khive_storage::{Edge, EdgeRelation, Entity, EntityFilter, Event, EventFilter};
use khive_types::{EdgeEndpointRule, EndpointKind, EventKind, SubstrateKind};

use crate::error::{RuntimeError, RuntimeResult};
use crate::runtime::KhiveRuntime;

// Test-only failure injection for `create_note_inner`.
//
// A test sets `LINK_FAIL_AFTER` to N > 0 before calling `create_note`.  The
// Nth `link` call inside the loop returns `RuntimeError::Internal("injected
// link failure")` instead of calling the real implementation.  The counter is
// reset to 0 after each call regardless of whether it triggered, so tests are
// isolated from one another.
#[cfg(test)]
std::thread_local! {
    static LINK_FAIL_AFTER: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A note search result with UUID, salience-weighted RRF score, and display text.
#[derive(Clone, Debug)]
pub struct NoteSearchHit {
    pub note_id: Uuid,
    pub score: DeterministicScore,
    pub title: Option<String>,
    pub snippet: Option<String>,
}

fn text_preview(text: &str, max_chars: usize) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(max_chars).collect())
    }
}

/// ADR-002: symmetric relations (`competes_with`, `composed_with`) are stored
/// with a canonical source (lower UUID wins), so a directed `Out` or `In` query
/// may miss results. When the relations filter is non-empty and contains **only**
/// symmetric relations, override direction to `Both` so callers always see all
/// edges for these relations regardless of storage canonicalization.
fn normalize_symmetric_direction(
    direction: Direction,
    relations: Option<&[EdgeRelation]>,
) -> Direction {
    let Some(rels) = relations else {
        return direction;
    };
    if rels.is_empty() {
        return direction;
    }
    let all_symmetric = rels
        .iter()
        .all(|r| matches!(r, EdgeRelation::CompetesWith | EdgeRelation::ComposedWith));
    if all_symmetric {
        Direction::Both
    } else {
        direction
    }
}

fn note_title(note: &Note) -> Option<String> {
    note.name
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| text_preview(&note.content, 80))
}

fn note_snippet(note: &Note) -> Option<String> {
    text_preview(&note.content, 200)
}

/// Runtime-local namespace proof until ADR-007 auth tokens are wired through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceToken {
    namespace: khive_types::Namespace,
}

impl NamespaceToken {
    pub fn new(namespace: impl Into<String>) -> Self {
        Self {
            namespace: khive_types::Namespace::new(namespace.into()),
        }
    }

    pub fn namespace(&self) -> &str {
        self.namespace.as_str()
    }
}

/// Result of resolving a UUID to its substrate kind.
#[derive(Clone, Debug)]
pub enum Resolved {
    Entity(Entity),
    Note(Note),
    Event(Event),
}

/// Map a resolved endpoint to its `(substrate, kind)` pair, or `None` if
/// the substrate is not a valid edge endpoint (events, edges).
fn resolved_pair(r: Option<&Resolved>) -> Option<(&'static str, &str)> {
    match r? {
        Resolved::Entity(e) => Some(("entity", e.kind.as_str())),
        Resolved::Note(n) => Some(("note", n.kind.as_str())),
        Resolved::Event(_) => None,
    }
}

/// `true` if `spec` matches the given substrate + kind pair.
fn endpoint_matches(spec: &EndpointKind, substrate: &str, kind: &str) -> bool {
    match spec {
        EndpointKind::EntityOfKind(k) => substrate == "entity" && *k == kind,
        EndpointKind::NoteOfKind(k) => substrate == "note" && *k == kind,
    }
}

/// `true` if any pack-declared edge endpoint rule allows the
/// `(source, relation, target)` triple. ADR-031: rules are additive only.
fn pack_rule_allows(
    rules: &[EdgeEndpointRule],
    relation: EdgeRelation,
    src: Option<&Resolved>,
    tgt: Option<&Resolved>,
) -> bool {
    let Some((src_sub, src_kind)) = resolved_pair(src) else {
        return false;
    };
    let Some((tgt_sub, tgt_kind)) = resolved_pair(tgt) else {
        return false;
    };
    rules.iter().any(|r| {
        r.relation == relation
            && endpoint_matches(&r.source, src_sub, src_kind)
            && endpoint_matches(&r.target, tgt_sub, tgt_kind)
    })
}

/// ADR-002 base endpoint allowlist for entity→entity relations.
///
/// Returns `true` if `(src_kind, relation, tgt_kind)` is an explicitly listed
/// triple in the ADR-002 base contract. `"*"` as `src_kind` means "any entity
/// kind" (used for `instance_of` whose source is unrestricted).
///
/// Pack rules (via `EDGE_RULES`) are additive — they cannot remove rows here.
fn base_entity_rule_allows(src_kind: &str, relation: EdgeRelation, tgt_kind: &str) -> bool {
    const RULES: &[(&str, EdgeRelation, &str)] = &[
        // Structure
        ("concept", EdgeRelation::Contains, "concept"),
        ("project", EdgeRelation::Contains, "project"),
        ("project", EdgeRelation::Contains, "artifact"),
        ("org", EdgeRelation::Contains, "project"),
        ("org", EdgeRelation::Contains, "service"),
        ("concept", EdgeRelation::PartOf, "concept"),
        ("project", EdgeRelation::PartOf, "project"),
        ("project", EdgeRelation::PartOf, "org"),
        ("*", EdgeRelation::InstanceOf, "concept"),
        ("service", EdgeRelation::InstanceOf, "project"),
        // Derivation
        ("concept", EdgeRelation::Extends, "concept"),
        ("concept", EdgeRelation::VariantOf, "concept"),
        ("artifact", EdgeRelation::VariantOf, "artifact"),
        ("concept", EdgeRelation::IntroducedBy, "document"),
        ("concept", EdgeRelation::IntroducedBy, "person"),
        ("artifact", EdgeRelation::IntroducedBy, "document"),
        // Provenance
        ("artifact", EdgeRelation::DerivedFrom, "dataset"),
        ("artifact", EdgeRelation::DerivedFrom, "document"),
        ("artifact", EdgeRelation::DerivedFrom, "project"),
        ("artifact", EdgeRelation::DerivedFrom, "artifact"),
        // Temporal
        ("document", EdgeRelation::Precedes, "document"),
        ("dataset", EdgeRelation::Precedes, "dataset"),
        ("artifact", EdgeRelation::Precedes, "artifact"),
        ("service", EdgeRelation::Precedes, "service"),
        ("project", EdgeRelation::Precedes, "project"),
        // Dependency
        ("project", EdgeRelation::DependsOn, "project"),
        ("service", EdgeRelation::DependsOn, "project"),
        ("service", EdgeRelation::DependsOn, "service"),
        ("service", EdgeRelation::DependsOn, "artifact"),
        ("service", EdgeRelation::DependsOn, "dataset"),
        ("artifact", EdgeRelation::DependsOn, "project"),
        ("artifact", EdgeRelation::DependsOn, "service"),
        ("concept", EdgeRelation::Enables, "concept"),
        ("service", EdgeRelation::Enables, "concept"),
        ("dataset", EdgeRelation::Enables, "concept"),
        // Implementation
        ("project", EdgeRelation::Implements, "concept"),
        ("service", EdgeRelation::Implements, "concept"),
        // Lateral
        ("concept", EdgeRelation::CompetesWith, "concept"),
        ("project", EdgeRelation::CompetesWith, "project"),
        ("service", EdgeRelation::CompetesWith, "service"),
        ("concept", EdgeRelation::ComposedWith, "concept"),
        ("project", EdgeRelation::ComposedWith, "project"),
        // Versioning (Supersedes — ADR-002:190-194: Concept/Document/Artifact/Service/Dataset only)
        ("concept", EdgeRelation::Supersedes, "concept"),
        ("document", EdgeRelation::Supersedes, "document"),
        ("artifact", EdgeRelation::Supersedes, "artifact"),
        ("service", EdgeRelation::Supersedes, "service"),
        ("dataset", EdgeRelation::Supersedes, "dataset"),
    ];
    RULES.iter().any(|(src, rel, tgt)| {
        *rel == relation && (*src == "*" || *src == src_kind) && *tgt == tgt_kind
    })
}

/// Canonical endpoint order for symmetric relations (F012).
///
/// For `competes_with` and `composed_with`, normalises direction so that
/// `source_uuid < target_uuid` (lexicographic on the UUID bytes). This
/// collapses A→B and B→A into a single canonical row, preventing duplicates.
fn canonical_edge_endpoints(
    relation: EdgeRelation,
    source_id: Uuid,
    target_id: Uuid,
) -> (Uuid, Uuid) {
    if relation.is_symmetric() && target_id < source_id {
        (target_id, source_id)
    } else {
        (source_id, target_id)
    }
}

/// Infer the default `dependency_kind` from endpoint entity kinds (ADR-002).
fn infer_dependency_kind(src_kind: &str, tgt_kind: &str) -> Option<&'static str> {
    match (src_kind, tgt_kind) {
        ("project", "project") => Some("build"),
        ("service", "service") => Some("runtime"),
        ("service", "dataset") => Some("data"),
        ("service", "artifact") => Some("artifact"),
        ("artifact", "project") | ("artifact", "service") => Some("tooling"),
        _ => None,
    }
}

/// Merge an inferred `dependency_kind` into `depends_on` edge metadata.
///
/// If `metadata` already carries a `dependency_kind` key the existing value is
/// preserved. If the key is absent and the endpoint pair has a known default,
/// the inferred value is added. Returns `metadata` unchanged for all other
/// cases (no matching default, or metadata already has the key).
fn merge_dependency_kind(
    src_kind: &str,
    tgt_kind: &str,
    metadata: Option<serde_json::Value>,
) -> Option<serde_json::Value> {
    if let Some(ref m) = metadata {
        if m.get("dependency_kind").is_some() {
            return metadata;
        }
    }
    let inferred = infer_dependency_kind(src_kind, tgt_kind)?;
    let mut obj = metadata.unwrap_or_else(|| serde_json::json!({}));
    if let Some(o) = obj.as_object_mut() {
        o.insert("dependency_kind".to_string(), serde_json::json!(inferred));
    }
    Some(obj)
}

/// Valid `dependency_kind` values for `depends_on` edges (ADR-002).
const VALID_DEPENDENCY_KINDS: &[&str] = &["build", "runtime", "data", "artifact", "tooling"];

/// Validate governed edge metadata keys (ADR-002 §Edge Metadata).
///
/// Currently enforces:
/// - `dependency_kind` is only valid on `depends_on` edges.
/// - `dependency_kind`, when present, must be one of the five governed values.
fn validate_edge_metadata(
    relation: EdgeRelation,
    metadata: Option<&serde_json::Value>,
) -> RuntimeResult<()> {
    let Some(meta) = metadata else {
        return Ok(());
    };
    if let Some(dk) = meta.get("dependency_kind") {
        if relation != EdgeRelation::DependsOn {
            return Err(RuntimeError::InvalidInput(format!(
                "dependency_kind is only valid on depends_on edges (got {})",
                relation.as_str()
            )));
        }
        let dk_str = dk
            .as_str()
            .ok_or_else(|| RuntimeError::InvalidInput("dependency_kind must be a string".into()))?;
        if !VALID_DEPENDENCY_KINDS.contains(&dk_str) {
            return Err(RuntimeError::InvalidInput(format!(
                "unknown dependency_kind {dk_str:?}; valid: {}",
                VALID_DEPENDENCY_KINDS.join(" | ")
            )));
        }
    }
    Ok(())
}

impl KhiveRuntime {
    // ---- Entity operations ----

    /// Create and persist a new entity.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_entity(
        &self,
        namespace: Option<&str>,
        kind: &str,
        entity_type: Option<&str>,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
    ) -> RuntimeResult<Entity> {
        let ns = self.ns(namespace);
        let mut entity = Entity::new(ns, kind, name).with_entity_type(entity_type);
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

    /// List entities in a namespace, optionally filtered by kind and entity_type.
    pub async fn list_entities(
        &self,
        namespace: Option<&str>,
        kind: Option<&str>,
        entity_type: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> RuntimeResult<Vec<Entity>> {
        let filter = EntityFilter {
            kinds: match kind {
                Some(k) => vec![k.to_string()],
                None => vec![],
            },
            entity_types: match entity_type {
                Some(t) => vec![t.to_string()],
                None => vec![],
            },
            ..Default::default()
        };
        let page = self
            .entities(namespace)?
            .query_entities(
                self.ns(namespace),
                filter,
                PageRequest {
                    offset: offset.into(),
                    limit,
                },
            )
            .await?;
        Ok(page.items)
    }

    /// List events in the namespace proven by the caller token.
    pub async fn list_events(
        &self,
        token: &NamespaceToken,
        filter: EventFilter,
        page: PageRequest,
    ) -> RuntimeResult<Page<Event>> {
        self.events(Some(token.namespace()))?
            .query_events(filter, page)
            .await
            .map_err(Into::into)
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
                (Resolved::Entity(src_e), Resolved::Entity(tgt_e)) => {
                    if !base_entity_rule_allows(&src_e.kind, EdgeRelation::Supersedes, &tgt_e.kind)
                    {
                        return Err(RuntimeError::InvalidInput(format!(
                            "({}) -[supersedes]-> ({}) is not in the ADR-002 base endpoint \
                             allowlist; supersedes requires same-kind entity endpoints",
                            src_e.kind, tgt_e.kind
                        )));
                    }
                }
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
            // All 13 base relations: ADR-002 contract is entity→entity with
            // kind-level restrictions (see base allowlist). ADR-031 allows packs
            // to extend the allowlist additively via EDGE_RULES.
            //
            // Strategy: resolve both endpoints once, consult pack rules first;
            // on miss, enforce the ADR-002 substrate check then the kind-level
            // base allowlist.
            let src_res = self.resolve(namespace, source_id).await?;
            let tgt_res = self.resolve(namespace, target_id).await?;

            if pack_rule_allows(
                &self.pack_edge_rules(),
                relation,
                src_res.as_ref(),
                tgt_res.as_ref(),
            ) {
                return Ok(());
            }

            // Substrate check: both endpoints must be entities.
            let src_kind = match src_res {
                Some(Resolved::Entity(e)) => e.kind,
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
            };
            let tgt_kind = match tgt_res {
                Some(Resolved::Entity(e)) => e.kind,
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
            };
            if !base_entity_rule_allows(&src_kind, relation, &tgt_kind) {
                return Err(RuntimeError::InvalidInput(format!(
                    "({src_kind}) -[{}]-> ({tgt_kind}) is not in the ADR-002 base endpoint \
                     allowlist; use pack EDGE_RULES to extend the allowlist",
                    relation.as_str()
                )));
            }
        }
        Ok(())
    }

    /// Create a directed edge between two substrates.
    ///
    /// Enforces the ADR-002/ADR-019/ADR-024 three-case relation contract via
    /// `validate_edge_relation_endpoints`. See that method for the full contract.
    ///
    /// For symmetric relations (`competes_with`, `composed_with`) the endpoint
    /// pair is canonicalised to `source_uuid < target_uuid` so that A→B and B→A
    /// deduplicate to one row (F012).
    ///
    /// `metadata` is validated against governed keys (ADR-002 §Edge Metadata);
    /// `dependency_kind` is inferred for `depends_on` edges when absent (F013).
    ///
    /// ADR-009 invariant: `target_backend` is always `None` for locally-routed
    /// edges written through this path. The `validate_edge_relation_endpoints`
    /// call above already ensures both endpoints exist in the local namespace,
    /// so setting `target_backend = None` is the only valid choice (F161).
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
        metadata: Option<serde_json::Value>,
    ) -> RuntimeResult<Edge> {
        self.validate_edge_relation_endpoints(namespace, source_id, target_id, relation)
            .await?;
        let (source_id, target_id) = canonical_edge_endpoints(relation, source_id, target_id);
        let metadata = if relation == EdgeRelation::DependsOn {
            match (
                self.resolve(namespace, source_id).await?,
                self.resolve(namespace, target_id).await?,
            ) {
                (Some(Resolved::Entity(src_e)), Some(Resolved::Entity(tgt_e))) => {
                    merge_dependency_kind(&src_e.kind, &tgt_e.kind, metadata)
                }
                _ => metadata,
            }
        } else {
            metadata
        };
        validate_edge_metadata(relation, metadata.as_ref())?;
        let now = chrono::Utc::now();
        let edge = Edge {
            id: LinkId::from(Uuid::new_v4()),
            namespace: self.ns(namespace).to_string(),
            source_id,
            target_id,
            relation,
            weight,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata,
            target_backend: None,
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
    ///
    /// ADR-002: symmetric relations (`competes_with`, `composed_with`) are stored
    /// with the canonical source as the lower UUID. Direction normalization is
    /// applied in `neighbors_with_query` so both callers see correct results.
    pub async fn neighbors(
        &self,
        namespace: Option<&str>,
        node_id: Uuid,
        direction: Direction,
        limit: Option<u32>,
        relations: Option<Vec<EdgeRelation>>,
    ) -> RuntimeResult<Vec<NeighborHit>> {
        self.neighbors_with_query(
            namespace,
            node_id,
            NeighborQuery {
                direction,
                relations,
                limit,
                min_weight: None,
            },
        )
        .await
    }

    /// Get neighbors with full query control (includes `min_weight`).
    ///
    /// Applies symmetric-relation direction normalization (ADR-002): if the
    /// relations filter contains only symmetric relations the direction is
    /// overridden to `Both` so edges stored in canonical order are always found.
    pub async fn neighbors_with_query(
        &self,
        namespace: Option<&str>,
        node_id: Uuid,
        mut query: NeighborQuery,
    ) -> RuntimeResult<Vec<NeighborHit>> {
        query.direction =
            normalize_symmetric_direction(query.direction, query.relations.as_deref());
        let mut hits = self.graph(namespace)?.neighbors(node_id, query).await?;
        self.enrich_neighbor_hits(namespace, &mut hits).await;
        Ok(hits)
    }

    /// Traverse the graph from a set of root nodes.
    pub async fn traverse(
        &self,
        namespace: Option<&str>,
        request: TraversalRequest,
    ) -> RuntimeResult<Vec<GraphPath>> {
        let mut paths = self.graph(namespace)?.traverse(request).await?;
        self.enrich_path_nodes(namespace, &mut paths).await;
        Ok(paths)
    }

    /// Populate `name` and `kind` on each `NeighborHit` from the corresponding
    /// entity record (#162). Best-effort — IDs that don't resolve to an entity
    /// (e.g. note-to-note `annotates` edges) leave the fields `None`.
    ///
    /// Done as a single batched entity fetch instead of an SQL JOIN at the
    /// graph store, so test databases that wire up a graph store without an
    /// entities table still work. Cost: one query per neighbors() call.
    async fn enrich_neighbor_hits(&self, namespace: Option<&str>, hits: &mut [NeighborHit]) {
        if hits.is_empty() {
            return;
        }
        let store = match self.entities(namespace) {
            Ok(s) => s,
            Err(_) => return, // no entity store configured; leave name/kind as None
        };
        for hit in hits.iter_mut() {
            if let Ok(Some(entity)) = store.get_entity(hit.node_id).await {
                hit.name = Some(entity.name);
                hit.kind = Some(entity.kind);
            }
        }
    }

    /// Populate `name` and `kind` on each `PathNode` from the corresponding
    /// entity record (#162). Same best-effort policy as `enrich_neighbor_hits`.
    async fn enrich_path_nodes(&self, namespace: Option<&str>, paths: &mut [GraphPath]) {
        if paths.is_empty() {
            return;
        }
        let store = match self.entities(namespace) {
            Ok(s) => s,
            Err(_) => return,
        };
        for path in paths.iter_mut() {
            for node in path.nodes.iter_mut() {
                if let Ok(Some(entity)) = store.get_entity(node.node_id).await {
                    node.name = Some(entity.name);
                    node.kind = Some(entity.kind);
                }
            }
        }
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
        kind: &str,
        name: Option<&str>,
        content: &str,
        salience: f64,
        properties: Option<serde_json::Value>,
        annotates: Vec<Uuid>,
    ) -> RuntimeResult<Note> {
        self.create_note_inner(
            namespace, kind, name, content, salience, None, properties, annotates,
        )
        .await
    }

    /// Like [`create_note`] but also sets a non-zero decay factor on the note.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_note_with_decay(
        &self,
        namespace: Option<&str>,
        kind: &str,
        name: Option<&str>,
        content: &str,
        salience: f64,
        decay_factor: f64,
        properties: Option<serde_json::Value>,
        annotates: Vec<Uuid>,
    ) -> RuntimeResult<Note> {
        self.create_note_inner(
            namespace,
            kind,
            name,
            content,
            salience,
            Some(decay_factor),
            properties,
            annotates,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_note_inner(
        &self,
        namespace: Option<&str>,
        kind: &str,
        name: Option<&str>,
        content: &str,
        salience: f64,
        decay_factor: Option<f64>,
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
        if let Some(df) = decay_factor {
            note = note.with_decay(df);
        }
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

        if self.config().embedding_model.is_some() {
            let vector = self.embed(&note.content).await?;
            self.vectors(Some(ns))?
                .insert(note.id, SubstrateKind::Note, ns, vector)
                .await?;
        }

        // Create annotates edges, compensating on failure to preserve atomicity.
        //
        // Pre-validation (above) ensures all targets exist, so link failures are
        // unexpected. If one occurs: delete any edges already created, then remove
        // the note, its FTS document, and its vector entry.
        let mut created_edges: Vec<Uuid> = Vec::with_capacity(annotates.len());

        // In test builds, iterate with an index so the failure-injection hook can
        // target a specific call.  In release builds, skip the enumerate overhead.
        #[cfg(test)]
        let annotates_iter: Vec<(usize, Uuid)> = annotates
            .iter()
            .enumerate()
            .map(|(i, &id)| (i, id))
            .collect();
        #[cfg(test)]
        macro_rules! next_target {
            ($pair:expr) => {
                $pair.1
            };
        }
        #[cfg(not(test))]
        let annotates_iter: Vec<Uuid> = annotates.to_vec();
        #[cfg(not(test))]
        macro_rules! next_target {
            ($pair:expr) => {
                $pair
            };
        }

        for pair in annotates_iter {
            let target_id = next_target!(pair);

            // Test-only: inject a failure on the configured call index (1-based).
            #[cfg(test)]
            let injected_err: Option<RuntimeError> = {
                let call_idx = pair.0;
                LINK_FAIL_AFTER.with(|cell| {
                    let n = cell.get();
                    if n > 0 && call_idx + 1 == n {
                        cell.set(0); // reset so subsequent calls are unaffected
                        Some(RuntimeError::Internal("injected link failure".to_string()))
                    } else {
                        None
                    }
                })
            };
            #[cfg(not(test))]
            let injected_err: Option<RuntimeError> = None;

            let link_result = if let Some(e) = injected_err {
                Err(e)
            } else {
                self.link(
                    Some(ns),
                    note.id,
                    target_id,
                    EdgeRelation::Annotates,
                    1.0,
                    None,
                )
                .await
            };

            match link_result {
                Ok(edge) => created_edges.push(edge.id.into()),
                Err(e) => {
                    // Best-effort compensation — ignore cleanup errors.
                    for edge_id in created_edges {
                        let _ = self.delete_edge(Some(ns), edge_id, true).await;
                    }
                    if let Ok(store) = self.notes(Some(ns)) {
                        let _ = store.delete_note(note.id, DeleteMode::Hard).await;
                    }
                    if let Ok(fts) = self.text_for_notes(Some(ns)) {
                        let _ = fts.delete_document(ns, note.id).await;
                    }
                    if self.config().embedding_model.is_some() {
                        if let Ok(vs) = self.vectors(Some(ns)) {
                            let _ = vs.delete(note.id).await;
                        }
                    }
                    return Err(e);
                }
            }
        }

        Ok(note)
    }

    /// List notes, optionally filtered by kind.
    pub async fn list_notes(
        &self,
        namespace: Option<&str>,
        kind: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> RuntimeResult<Vec<Note>> {
        let page = self
            .notes(namespace)?
            .query_notes(
                self.ns(namespace),
                kind,
                PageRequest {
                    offset: offset.into(),
                    limit,
                },
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
        note_kind: Option<&str>,
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
        let vector_hits = if query_vector.is_some() || self.config().embedding_model.is_some() {
            self.vector_search(
                namespace,
                query_vector,
                Some(query_text),
                candidates,
                Some(SubstrateKind::Note),
            )
            .await?
        } else {
            vec![]
        };

        // RRF fusion.
        #[derive(Default)]
        struct Bucket {
            score: DeterministicScore,
            title: Option<String>,
            snippet: Option<String>,
        }

        let mut buckets: HashMap<Uuid, Bucket> = HashMap::new();
        for (i, hit) in text_hits.into_iter().enumerate() {
            let rank = i + 1;
            let entry = buckets.entry(hit.subject_id).or_default();
            entry.score = entry.score + rrf_score(rank, RRF_K);
            if entry.title.is_none() {
                entry.title = hit.title;
            }
            if entry.snippet.is_none() {
                entry.snippet = hit.snippet;
            }
        }
        for (i, hit) in vector_hits.into_iter().enumerate() {
            let rank = i + 1;
            let entry = buckets.entry(hit.subject_id).or_default();
            entry.score = entry.score + rrf_score(rank, RRF_K);
        }

        let candidate_ids: Vec<Uuid> = buckets.keys().copied().collect();
        if candidate_ids.is_empty() {
            return Ok(vec![]);
        }

        // Fetch each candidate note individually to get salience and apply
        // soft-delete + (optional) kind filtering. Notes whose `kind` doesn't
        // match `note_kind` are dropped post-fetch — they're a small set
        // bounded by `candidates`, so the extra read is cheap.
        let note_store = self.notes(namespace)?;
        let mut alive_notes: HashMap<Uuid, Note> = HashMap::new();
        for id in &candidate_ids {
            if let Some(note) = note_store.get_note(*id).await? {
                if note.deleted_at.is_some() {
                    continue;
                }
                if let Some(want_kind) = note_kind {
                    if note.kind != want_kind {
                        continue;
                    }
                }
                alive_notes.insert(*id, note);
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
            .filter_map(|(id, bucket)| {
                let note = alive_notes.get(&id)?;
                let weight = 0.5 + 0.5 * note.salience;
                let weighted = DeterministicScore::from_f64(bucket.score.to_f64() * weight);
                Some(NoteSearchHit {
                    note_id: id,
                    score: weighted,
                    title: bucket.title.or_else(|| note_title(note)),
                    snippet: bucket.snippet.or_else(|| note_snippet(note)),
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

        let tables = [
            ("entities", true),
            ("notes", true),
            ("events", false),
            ("graph_edges", false),
        ];

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
    /// On hard delete, cascades to remove all incident edges (both inbound and
    /// outbound) and cleans up FTS and vector indexes, preventing dangling
    /// references for `annotates` edges that target this note (ADR-002, ADR-024).
    /// Soft delete also cleans FTS and vector indexes; edges are left in place.
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

        // On hard delete, cascade-remove incident edges and clean up indexes.
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
                    graph
                        .delete_edge(LinkId::from(hit.edge_id), DeleteMode::Hard)
                        .await?;
                }
            }
            let ns_str = ns.to_string();
            self.text_for_notes(namespace)?
                .delete_document(&ns_str, id)
                .await?;
            if self.config().embedding_model.is_some() {
                self.vectors(namespace)?.delete(id).await?;
            }
        }

        let deleted = note_store.delete_note(id, mode).await?;
        if !hard && deleted {
            let ns_str = ns.to_string();
            self.text_for_notes(namespace)?
                .delete_document(&ns_str, id)
                .await?;
            if self.config().embedding_model.is_some() {
                self.vectors(namespace)?.delete(id).await?;
            }
        }
        if deleted {
            if let Ok(event_store) = self.events(namespace) {
                let ns_str = ns.to_string();
                let event = khive_storage::event::Event::new(
                    ns_str,
                    "delete",
                    EventKind::NoteDeleted,
                    SubstrateKind::Note,
                    "",
                )
                .with_target(id)
                .with_payload(serde_json::json!({"id": id, "hard": hard}));
                if let Err(e) = event_store.append_event(event).await {
                    tracing::warn!(error = %e, "delete_note: event store write failed (non-fatal)");
                }
            }
        }
        Ok(deleted)
    }
}

/// Result of a GQL/SPARQL query with optional validation warnings.
#[derive(Clone, Debug, Serialize)]
pub struct QueryResult {
    pub rows: Vec<SqlRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl KhiveRuntime {
    // ---- Query operations ----

    /// Execute a GQL or SPARQL query string, returning raw SQL rows.
    ///
    /// The query is compiled to SQL with the namespace scope applied.
    /// GQL syntax: `MATCH (a:concept)-[e:extends]->(b) RETURN a, b LIMIT 10`
    /// SPARQL syntax: `SELECT ?a WHERE { ?a :kind "concept" . }`
    pub async fn query(&self, namespace: Option<&str>, query: &str) -> RuntimeResult<Vec<SqlRow>> {
        Ok(self.query_with_metadata(namespace, query).await?.rows)
    }

    /// Execute a GQL/SPARQL query, returning rows and any validation warnings.
    pub async fn query_with_metadata(
        &self,
        namespace: Option<&str>,
        query: &str,
    ) -> RuntimeResult<QueryResult> {
        let ns = self.ns(namespace);
        let ast = khive_query::parse_auto(query)?;
        let opts = khive_query::CompileOptions {
            scopes: vec![ns.to_string()],
            ..Default::default()
        };
        let compiled = khive_query::compile(&ast, &opts)?;
        let warnings = compiled.warnings;
        let mut reader = self.sql().reader().await?;
        let stmt = SqlStatement {
            sql: compiled.sql,
            params: compiled.params,
            label: None,
        };
        let rows = reader.query_all(stmt).await?;
        Ok(QueryResult { rows, warnings })
    }

    /// Delete an entity by ID (soft delete by default).
    ///
    /// On hard delete, cascades to remove all incident edges (both inbound and
    /// outbound) to prevent dangling references. Soft delete also cleans FTS
    /// and vector indexes; edges are left in place.
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
                    graph
                        .delete_edge(LinkId::from(hit.edge_id), DeleteMode::Hard)
                        .await?;
                }
            }
            self.remove_from_indexes(namespace, id).await?;
        }

        let deleted = self.entities(namespace)?.delete_entity(id, mode).await?;
        if !hard && deleted {
            self.remove_from_indexes(namespace, id).await?;
        }
        if deleted {
            if let Ok(event_store) = self.events(namespace) {
                let ns = entity.namespace.clone();
                let event = khive_storage::event::Event::new(
                    ns,
                    "delete",
                    EventKind::EntityDeleted,
                    SubstrateKind::Entity,
                    "",
                )
                .with_target(id)
                .with_payload(serde_json::json!({"id": id, "hard": hard}));
                if let Err(e) = event_store.append_event(event).await {
                    tracing::warn!(error = %e, "delete_entity: event store write failed (non-fatal)");
                }
            }
        }
        Ok(deleted)
    }

    /// Count entities in a namespace, optionally filtered.
    pub async fn count_entities(
        &self,
        namespace: Option<&str>,
        kind: Option<&str>,
    ) -> RuntimeResult<u64> {
        let filter = EntityFilter {
            kinds: match kind {
                Some(k) => vec![k.to_string()],
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

        if let Ok(event_store) = self.events(namespace) {
            let ns = self.ns(namespace).to_string();
            let event = khive_storage::event::Event::new(
                ns,
                "update",
                EventKind::EdgeUpdated,
                SubstrateKind::Entity,
                "",
            )
            .with_target(edge_id)
            .with_payload(serde_json::json!({"id": edge_id}));
            if let Err(e) = event_store.append_event(event).await {
                tracing::warn!(error = %e, "update_edge: event store write failed (non-fatal)");
            }
        }

        Ok(edge)
    }

    /// Hard-delete an edge by id.
    ///
    /// Cascades to remove any `annotates` edges whose target is the deleted edge
    /// (ADR-002: `annotates` is note → anything; deleting an edge target leaves
    /// annotation edges dangling if not cleaned up). Returns `true` if the primary
    /// edge was removed.
    ///
    /// If `edge_id` does not refer to an edge (e.g. the caller passes an entity or
    /// note UUID by mistake), this method returns `Ok(false)` immediately with no
    /// side effects — it does **not** cascade inbound edges of the non-edge record.
    pub async fn delete_edge(
        &self,
        namespace: Option<&str>,
        edge_id: Uuid,
        hard: bool,
    ) -> RuntimeResult<bool> {
        let graph = self.graph(namespace)?;
        let mode = if hard {
            DeleteMode::Hard
        } else {
            DeleteMode::Soft
        };

        // Guard: verify `edge_id` is actually an edge before touching anything.
        // Without this check, passing an entity/note UUID would delete all inbound
        // annotates edges targeting that record and then return false — a destructive
        // side effect on an invalid call.
        if graph.get_edge(LinkId::from(edge_id)).await?.is_none() {
            return Ok(false);
        }

        // Cascade: remove annotate edges that target this edge (inbound from note sources).
        let inbound = graph
            .neighbors(
                edge_id,
                NeighborQuery {
                    direction: Direction::In,
                    relations: None,
                    limit: None,
                    min_weight: None,
                },
            )
            .await?;
        for hit in inbound {
            graph
                .delete_edge(LinkId::from(hit.edge_id), DeleteMode::Hard)
                .await?;
        }

        let deleted = graph.delete_edge(LinkId::from(edge_id), mode).await?;
        if deleted {
            if let Ok(event_store) = self.events(namespace) {
                let ns = self.ns(namespace).to_string();
                let event = khive_storage::event::Event::new(
                    ns,
                    "delete",
                    EventKind::EdgeDeleted,
                    SubstrateKind::Entity,
                    "",
                )
                .with_target(edge_id)
                .with_payload(serde_json::json!({"id": edge_id, "hard": hard}));
                if let Err(e) = event_store.append_event(event).await {
                    tracing::warn!(error = %e, "delete_edge: event store write failed (non-fatal)");
                }
            }
        }
        Ok(deleted)
    }

    /// Count edges matching `filter`.
    pub async fn count_edges(
        &self,
        namespace: Option<&str>,
        filter: crate::curation::EdgeListFilter,
    ) -> RuntimeResult<u64> {
        Ok(self.graph(namespace)?.count_edges(filter.into()).await?)
    }

    /// Validate and construct an edge from a [`LinkSpec`] without writing to storage.
    ///
    /// Applies the full ADR-002 contract (endpoint validation, symmetric
    /// canonicalization, `dependency_kind` inference and metadata validation).
    /// Returns the constructed `Edge` on success; the caller is responsible for
    /// persisting it (e.g. via `upsert_edge` or `link_many`).
    pub async fn build_edge(&self, spec: &LinkSpec) -> RuntimeResult<Edge> {
        let ns = spec.namespace.as_deref();
        self.validate_edge_relation_endpoints(ns, spec.source_id, spec.target_id, spec.relation)
            .await?;
        let (source_id, target_id) =
            canonical_edge_endpoints(spec.relation, spec.source_id, spec.target_id);
        let metadata = if spec.relation == EdgeRelation::DependsOn {
            match (
                self.resolve(ns, source_id).await?,
                self.resolve(ns, target_id).await?,
            ) {
                (Some(Resolved::Entity(src_e)), Some(Resolved::Entity(tgt_e))) => {
                    merge_dependency_kind(&src_e.kind, &tgt_e.kind, spec.metadata.clone())
                }
                _ => spec.metadata.clone(),
            }
        } else {
            spec.metadata.clone()
        };
        validate_edge_metadata(spec.relation, metadata.as_ref())?;
        let now = chrono::Utc::now();
        Ok(Edge {
            id: LinkId::from(Uuid::new_v4()),
            namespace: self.ns(ns).to_string(),
            source_id,
            target_id,
            relation: spec.relation,
            weight: spec.weight,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata,
            target_backend: None,
        })
    }

    /// Validate and atomically upsert a batch of edges.
    ///
    /// All edges are validated and constructed with `build_edge` before any
    /// write. If validation fails for any entry the entire batch is rejected
    /// (no writes occur). On success, all edges are persisted in a single
    /// atomic transaction via `upsert_edges`.
    ///
    /// All specs must share the same namespace; the namespace of the first
    /// spec is used as the graph store scope.
    pub async fn link_many(&self, specs: Vec<LinkSpec>) -> RuntimeResult<Vec<Edge>> {
        if specs.is_empty() {
            return Ok(vec![]);
        }
        let mut edges = Vec::with_capacity(specs.len());
        for spec in &specs {
            edges.push(self.build_edge(spec).await?);
        }
        let ns = specs[0].namespace.as_deref();
        self.graph(ns)?.upsert_edges(edges.clone()).await?;
        Ok(edges)
    }
}

/// Fully specified edge creation request — input to [`KhiveRuntime::build_edge`]
/// and [`KhiveRuntime::link_many`].
#[derive(Clone, Debug)]
pub struct LinkSpec {
    pub namespace: Option<String>,
    pub source_id: Uuid,
    pub target_id: Uuid,
    pub relation: EdgeRelation,
    pub weight: f64,
    pub metadata: Option<serde_json::Value>,
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
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
            .create_note(None, "observation", None, "a note", 0.5, None, vec![])
            .await
            .unwrap();
        let entity = rt
            .create_entity(None, "concept", None, "E", None, None, vec![])
            .await
            .unwrap();
        // Create a valid note→entity annotates edge.
        let edge = rt
            .link(None, note.id, entity.id, EdgeRelation::Annotates, 1.0, None)
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(None, "concept", None, "C", None, None, vec![])
            .await
            .unwrap();

        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(None, a.id, c.id, EdgeRelation::Enables, 1.0, None)
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(None, "concept", None, "C", None, None, vec![])
            .await
            .unwrap();
        let d = rt
            .create_entity(None, "concept", None, "D", None, None, vec![])
            .await
            .unwrap();

        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(None, c.id, d.id, EdgeRelation::Extends, 1.0, None)
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_id: Uuid = edge.id.into();

        let deleted = rt.delete_edge(None, edge_id, true).await.unwrap();
        assert!(deleted);

        let fetched = rt.get_edge(None, edge_id).await.unwrap();
        assert!(fetched.is_none(), "edge should be gone after delete");
    }

    #[tokio::test]
    async fn count_edges_matches_filter() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(None, "concept", None, "C", None, None, vec![])
            .await
            .unwrap();

        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(None, a.id, c.id, EdgeRelation::Enables, 1.0, None)
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
            .create_entity(Some("ns-a"), "concept", None, "Alpha", None, None, vec![])
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
            .create_entity(Some("ns-a"), "concept", None, "Beta", None, None, vec![])
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
                "observation",
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
                "insight",
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
            .create_entity(None, "concept", None, "FlashAttention", None, None, vec![])
            .await
            .unwrap();

        let note = rt
            .create_note(
                None,
                "observation",
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(None, "concept", None, "C", None, None, vec![])
            .await
            .unwrap();

        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(None, a.id, c.id, EdgeRelation::Enables, 1.0, None)
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(None, "concept", None, "C", None, None, vec![])
            .await
            .unwrap();

        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(None, a.id, c.id, EdgeRelation::Enables, 1.0, None)
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
            "observation",
            None,
            "GQA reduces KV cache memory for large models",
            0.8,
            None,
            vec![],
        )
        .await
        .unwrap();

        let results = rt
            .search_notes(None, "GQA KV cache", None, 10, None)
            .await
            .unwrap();

        assert!(!results.is_empty(), "search should return the indexed note");
        let hit = &results[0];
        assert!(
            hit.title.is_some(),
            "note hit title should be populated (falls back to content)"
        );
        assert!(
            hit.snippet.is_some(),
            "note hit snippet should be populated"
        );
    }

    #[tokio::test]
    async fn search_notes_excludes_soft_deleted() {
        let rt = rt();
        let note = rt
            .create_note(
                None,
                "observation",
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
            .search_notes(None, "RoPE rotary positional", None, 10, None)
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
            .create_entity(None, "concept", None, "LoRA", None, None, vec![])
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
                "observation",
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
            .create_entity(None, "concept", None, "PrefixTest", None, None, vec![])
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
            .create_entity(
                Some("ns_a"),
                "concept",
                None,
                "Invisible",
                None,
                None,
                vec![],
            )
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

        let rt = rt();
        // Two entities with UUIDs sharing the same 8-char prefix "aabbccdd".
        let id_a = Uuid::parse_str("aabbccdd-1111-4000-8000-000000000001").unwrap();
        let id_b = Uuid::parse_str("aabbccdd-2222-4000-8000-000000000002").unwrap();

        let mut entity_a = Entity::new("local", "concept", "AmbigA");
        entity_a.id = id_a;
        let mut entity_b = Entity::new("local", "concept", "AmbigB");
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

    // ---- Event resolution tests (issue #30) ----
    //
    // resolve_prefix and handle_get already include events; these tests are
    // regression coverage confirming event UUIDs are resolvable and that get()
    // returns kind="event".

    #[tokio::test]
    async fn resolve_finds_event_by_full_uuid() {
        use khive_storage::Event;
        use khive_types::{EventKind, SubstrateKind};

        let rt = rt();
        let ns = rt.ns(None);
        let event = Event::new(
            ns,
            "test_verb",
            EventKind::Audit,
            SubstrateKind::Entity,
            "actor",
        );
        let event_id = event.id;
        rt.events(None).unwrap().append_event(event).await.unwrap();

        let resolved = rt.resolve(None, event_id).await.unwrap();
        assert!(
            matches!(resolved, Some(Resolved::Event(_))),
            "event UUID must resolve to Resolved::Event, got {resolved:?}"
        );
    }

    #[tokio::test]
    async fn resolve_prefix_finds_event() {
        use khive_storage::Event;
        use khive_types::{EventKind, SubstrateKind};

        let rt = rt();
        let ns = rt.ns(None);
        let event = Event::new(
            ns,
            "test_verb",
            EventKind::Audit,
            SubstrateKind::Entity,
            "actor",
        );
        let event_id = event.id;
        rt.events(None).unwrap().append_event(event).await.unwrap();

        let prefix = &event_id.to_string()[..8];
        let resolved = rt.resolve_prefix(None, prefix).await.unwrap();
        assert_eq!(
            resolved,
            Some(event_id),
            "resolve_prefix must return event UUID for 8-char prefix"
        );
    }

    // ---- Referential integrity tests (fix/link-referential-integrity) ----

    #[tokio::test]
    async fn link_phantom_source_returns_not_found() {
        let rt = rt();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let phantom = Uuid::new_v4();

        let result = rt
            .link(None, phantom, b.id, EdgeRelation::Extends, 1.0, None)
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let phantom = Uuid::new_v4();

        let result = rt
            .link(None, a.id, phantom, EdgeRelation::Extends, 1.0, None)
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();

        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 0.8, None)
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
                "observation",
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
            .create_entity(None, "concept", None, "RealTarget", None, None, vec![])
            .await
            .unwrap();

        let note = rt
            .create_note(
                None,
                "observation",
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

    // Atomicity: multi-target annotates golden path — all edges created, note present.
    #[tokio::test]
    async fn create_note_multi_annotates_creates_all_edges() {
        let rt = rt();
        let t1 = rt
            .create_entity(None, "concept", None, "Target1", None, None, vec![])
            .await
            .unwrap();
        let t2 = rt
            .create_entity(None, "concept", None, "Target2", None, None, vec![])
            .await
            .unwrap();

        let note = rt
            .create_note(
                None,
                "observation",
                None,
                "content",
                0.5,
                None,
                vec![t1.id, t2.id],
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
        assert_eq!(
            neighbors.len(),
            2,
            "multi-annotates note must have exactly 2 outbound annotates edges"
        );
        let target_ids: Vec<Uuid> = neighbors.iter().map(|n| n.node_id).collect();
        assert!(target_ids.contains(&t1.id));
        assert!(target_ids.contains(&t2.id));
    }

    #[tokio::test]
    async fn link_target_in_different_namespace_returns_not_found() {
        let rt = rt();
        let a = rt
            .create_entity(Some("ns-a"), "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(Some("ns-b"), "concept", None, "B", None, None, vec![])
            .await
            .unwrap();

        // Linking from ns-a: target b lives in ns-b — must be treated as not found.
        let result = rt
            .link(Some("ns-a"), a.id, b.id, EdgeRelation::Extends, 1.0, None)
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
            .link(None, phantom, phantom, EdgeRelation::Extends, 1.0, None)
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        // Create a real edge between a and b, capture its UUID.
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_uuid: Uuid = edge.id.into();

        // Create a note and annotate the edge itself (edge is a valid substrate target per ADR-024).
        let note = rt
            .create_note(None, "observation", None, "edge note", 0.5, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(None, note.id, edge_uuid, EdgeRelation::Annotates, 1.0, None)
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_uuid: Uuid = edge.id.into();

        let note = rt
            .create_note(
                None,
                "observation",
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

        let before_count = rt.list_notes(None, None, 1000, 0).await.unwrap().len();

        let result = rt
            .create_note(
                None,
                "observation",
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
        let after_count = rt.list_notes(None, None, 1000, 0).await.unwrap().len();
        assert_eq!(
            before_count, after_count,
            "failed create_note must not persist any note row (atomicity)"
        );

        // FTS must not contain the content either.
        let search_hits = rt
            .search_notes(None, "should not persist", None, 10, None)
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        // Create a real edge; capture its UUID as the bad target.
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_uuid: Uuid = edge.id.into();

        let result = rt
            .link(None, a.id, edge_uuid, EdgeRelation::Extends, 1.0, None)
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
            .create_note(None, "observation", None, "a note", 0.5, None, vec![])
            .await
            .unwrap();
        let entity = rt
            .create_entity(None, "concept", None, "E", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(None, note.id, entity.id, EdgeRelation::DependsOn, 1.0, None)
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(None, a.id, b.id, EdgeRelation::Annotates, 1.0, None)
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_uuid: Uuid = edge.id.into();

        // An existing edge used as an annotates source: wrong kind, not absent.
        let result = rt
            .link(None, edge_uuid, a.id, EdgeRelation::Annotates, 1.0, None)
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
        use khive_types::{EventKind, SubstrateKind};

        let rt = rt();
        let note = rt
            .create_note(
                None,
                "observation",
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
        let event = Event::new(
            ns,
            "test_verb",
            EventKind::Audit,
            SubstrateKind::Entity,
            "test_actor",
        );
        let event_id = event.id;
        rt.events(None).unwrap().append_event(event).await.unwrap();

        let result = rt
            .link(None, note.id, event_id, EdgeRelation::Annotates, 1.0, None)
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
        use khive_types::{EventKind, SubstrateKind};

        let rt = rt();
        let ns = rt.ns(None);
        let event = Event::new(
            ns,
            "test_verb",
            EventKind::Audit,
            SubstrateKind::Entity,
            "test_actor",
        );
        let event_id = event.id;
        rt.events(None).unwrap().append_event(event).await.unwrap();

        let result = rt
            .create_note(
                None,
                "observation",
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
                "observation",
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
                "observation",
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
                None,
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
            .create_entity(None, "concept", None, "OldConcept", None, None, vec![])
            .await
            .unwrap();
        let new_entity = rt
            .create_entity(None, "concept", None, "NewConcept", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(
                None,
                new_entity.id,
                old_entity.id,
                EdgeRelation::Supersedes,
                1.0,
                None,
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
            .create_note(None, "observation", None, "a note", 0.5, None, vec![])
            .await
            .unwrap();
        let entity = rt
            .create_entity(None, "concept", None, "SomeEntity", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(
                None,
                note.id,
                entity.id,
                EdgeRelation::Supersedes,
                1.0,
                None,
            )
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
            .create_entity(None, "concept", None, "SomeEntity", None, None, vec![])
            .await
            .unwrap();
        let note = rt
            .create_note(None, "observation", None, "a note", 0.5, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(
                None,
                entity.id,
                note.id,
                EdgeRelation::Supersedes,
                1.0,
                None,
            )
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
        use khive_types::{EventKind, SubstrateKind};

        let rt = rt();
        let ns = rt.ns(None);
        let event = Event::new(
            ns,
            "test_verb",
            EventKind::Audit,
            SubstrateKind::Entity,
            "test_actor",
        );
        let event_id = event.id;
        rt.events(None).unwrap().append_event(event).await.unwrap();

        let entity = rt
            .create_entity(None, "concept", None, "SomeEntity", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(
                None,
                event_id,
                entity.id,
                EdgeRelation::Supersedes,
                1.0,
                None,
            )
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
        use khive_types::{EventKind, SubstrateKind};

        let rt = rt();
        let ns = rt.ns(None);
        let event = Event::new(
            ns,
            "test_verb",
            EventKind::Audit,
            SubstrateKind::Entity,
            "test_actor",
        );
        let event_id = event.id;
        rt.events(None).unwrap().append_event(event).await.unwrap();

        let entity = rt
            .create_entity(None, "concept", None, "SomeEntity", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(
                None,
                entity.id,
                event_id,
                EdgeRelation::Supersedes,
                1.0,
                None,
            )
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_uuid: Uuid = edge.id.into();

        let result = rt
            .link(None, edge_uuid, a.id, EdgeRelation::Supersedes, 1.0, None)
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_uuid: Uuid = edge.id.into();

        let result = rt
            .link(None, a.id, edge_uuid, EdgeRelation::Supersedes, 1.0, None)
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
                "observation",
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
            .link(None, phantom, note.id, EdgeRelation::Supersedes, 1.0, None)
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
                "observation",
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
            .link(None, note.id, phantom, EdgeRelation::Supersedes, 1.0, None)
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
                "observation",
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
                "observation",
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
                None,
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
                "observation",
                None,
                "a note that cannot be an extends source",
                0.5,
                None,
                vec![],
            )
            .await
            .unwrap();
        let entity = rt
            .create_entity(None, "concept", None, "E", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(None, note.id, entity.id, EdgeRelation::Extends, 1.0, None)
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
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_uuid: Uuid = edge.id.into();

        let note = rt
            .create_note(
                None,
                "observation",
                None,
                "annotating an edge",
                0.5,
                None,
                vec![],
            )
            .await
            .unwrap();

        let result = rt
            .link(None, note.id, edge_uuid, EdgeRelation::Annotates, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "note→edge Annotates must still succeed after supersedes fix, got {result:?}"
        );
    }

    // ---- Compensation-path rollback (fix/annotates) ----

    // The compensation branch in `create_note_inner` (operations.rs) rolls back
    // a partial write — note row + first edge + FTS + vector — when a subsequent
    // link call fails. The failure trigger is a storage error (e.g. I/O failure)
    // that cannot occur in the in-memory runtime; this test instead exercises the
    // exact cleanup operations that the compensation branch performs, starting from
    // a manually-constructed partial state, and verifies the post-cleanup invariants.
    //
    // What this covers: the cleanup sequence (delete_edge, delete_note hard, FTS
    // index clean) is correct and leaves the DB in a pristine state. What it does
    // not cover: the trigger condition (second link failure). Storage-error injection
    // would require a mock GraphStore, which is beyond the current test infrastructure.
    #[tokio::test]
    async fn create_note_multi_annotates_compensation_cleanup_restores_pristine_state() {
        let rt = rt();
        let t1 = rt
            .create_entity(None, "concept", None, "T1", None, None, vec![])
            .await
            .unwrap();

        // Construct the partial state that the compensation branch would encounter:
        // note persisted + first annotates edge created.
        let note = rt
            .create_note(
                None,
                "observation",
                None,
                "partial note",
                0.5,
                None,
                vec![t1.id],
            )
            .await
            .unwrap();

        // Confirm the partial state exists before compensation.
        let before_notes = rt.list_notes(None, None, 1000, 0).await.unwrap();
        assert_eq!(before_notes.len(), 1, "note must be present before cleanup");
        let before_edges = rt
            .neighbors(
                None,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(
            before_edges.len(),
            1,
            "one annotates edge must exist before cleanup"
        );
        let edge_id: Uuid = before_edges[0].edge_id;

        // Execute the same cleanup sequence that `create_note_inner`'s Err branch runs.
        rt.delete_edge(None, edge_id, true).await.unwrap();
        rt.delete_note(None, note.id, true /* hard */)
            .await
            .unwrap();

        // Post-compensation invariants:
        let after_notes = rt.list_notes(None, None, 1000, 0).await.unwrap();
        assert!(
            after_notes.is_empty(),
            "compensation must remove the note row; got {after_notes:?}"
        );
        let search_hits = rt
            .search_notes(None, "partial note", None, 10, None)
            .await
            .unwrap();
        assert!(
            search_hits.is_empty(),
            "compensation must clean the FTS index; got {search_hits:?}"
        );
        let after_edges = rt
            .neighbors(None, note.id, Direction::Out, None, None)
            .await
            .unwrap();
        assert!(
            after_edges.is_empty(),
            "compensation must remove all partial edges; got {after_edges:?}"
        );
    }

    // ---- Hard-delete cascade for note and edge annotation targets (fix/annotates) ----

    // ADR-002:73 — annotates is note → ANYTHING (entity, note, edge, event).
    // ADR-024:103 — targets may be entity, edge, event, or note.
    // Hard-deleting any of those targets must cascade incident annotates edges.
    // Soft deletes leave edges (data-vs-view rule).

    #[tokio::test]
    async fn annotated_entity_hard_delete_cascades_annotate_edge() {
        let rt = rt();
        let entity = rt
            .create_entity(None, "concept", None, "E", None, None, vec![])
            .await
            .unwrap();
        let note = rt
            .create_note(
                None,
                "observation",
                None,
                "note about entity",
                0.5,
                None,
                vec![entity.id],
            )
            .await
            .unwrap();

        // Confirm edge exists before delete.
        let before = rt
            .neighbors(
                None,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(
            before.len(),
            1,
            "annotates edge must exist before entity delete"
        );

        // Hard delete the entity.
        let deleted = rt.delete_entity(None, entity.id, true).await.unwrap();
        assert!(deleted, "entity hard delete must return true");

        // Annotates edge must be gone.
        let after = rt
            .neighbors(
                None,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert!(
            after.is_empty(),
            "annotates edge must be cascaded on entity hard delete; got {after:?}"
        );
    }

    #[tokio::test]
    async fn annotated_note_hard_delete_cascades_annotate_edge() {
        let rt = rt();
        // note_target is the thing being annotated (a note itself).
        let note_target = rt
            .create_note(None, "observation", None, "target note", 0.5, None, vec![])
            .await
            .unwrap();
        // note_source annotates note_target.
        let note_source = rt
            .create_note(
                None,
                "insight",
                None,
                "annotation",
                0.5,
                None,
                vec![note_target.id],
            )
            .await
            .unwrap();

        let before = rt
            .neighbors(
                None,
                note_source.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(
            before.len(),
            1,
            "annotates edge must exist before note delete"
        );

        // Hard delete the annotation TARGET note.
        let deleted = rt.delete_note(None, note_target.id, true).await.unwrap();
        assert!(deleted, "note hard delete must return true");

        // The annotates edge targeting note_target must be gone.
        let after = rt
            .neighbors(
                None,
                note_source.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert!(
            after.is_empty(),
            "annotates edge must be cascaded on note-target hard delete; got {after:?}"
        );
    }

    #[tokio::test]
    async fn annotated_edge_delete_cascades_annotate_edge() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        // Create an edge to annotate.
        let base_edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let base_edge_uuid: Uuid = base_edge.id.into();

        // Create a note that annotates the edge.
        let note = rt
            .create_note(
                None,
                "observation",
                None,
                "note about edge",
                0.5,
                None,
                vec![base_edge_uuid],
            )
            .await
            .unwrap();

        let before = rt
            .neighbors(
                None,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(
            before.len(),
            1,
            "annotates edge must exist before base edge delete"
        );

        // Delete the base edge.
        let deleted = rt.delete_edge(None, base_edge_uuid, true).await.unwrap();
        assert!(deleted, "edge delete must return true");

        // The annotates edge targeting base_edge must be gone.
        let after = rt
            .neighbors(
                None,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert!(
            after.is_empty(),
            "annotates edge must be cascaded on base edge delete; got {after:?}"
        );
    }

    #[tokio::test]
    async fn mixed_multi_annotates_partial_target_hard_delete_leaves_remaining_edges() {
        let rt = rt();
        let t1 = rt
            .create_entity(None, "concept", None, "T1", None, None, vec![])
            .await
            .unwrap();
        let t2 = rt
            .create_entity(None, "concept", None, "T2", None, None, vec![])
            .await
            .unwrap();

        // Note annotates both t1 and t2.
        let note = rt
            .create_note(
                None,
                "observation",
                None,
                "multi-target note",
                0.5,
                None,
                vec![t1.id, t2.id],
            )
            .await
            .unwrap();

        let before = rt
            .neighbors(
                None,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(
            before.len(),
            2,
            "must have 2 annotates edges before any delete"
        );

        // Hard delete only t1.
        rt.delete_entity(None, t1.id, true).await.unwrap();

        // Edge to t1 must be gone, edge to t2 must remain.
        let after = rt
            .neighbors(
                None,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(
            after.len(),
            1,
            "only the edge to t1 must be cascaded; t2 edge must remain"
        );
        assert_eq!(
            after[0].node_id, t2.id,
            "remaining annotates edge must point to t2"
        );
    }

    #[tokio::test]
    async fn annotated_note_soft_delete_preserves_annotate_edge() {
        let rt = rt();
        let note_target = rt
            .create_note(None, "observation", None, "target", 0.5, None, vec![])
            .await
            .unwrap();
        let note_source = rt
            .create_note(
                None,
                "insight",
                None,
                "annotation",
                0.5,
                None,
                vec![note_target.id],
            )
            .await
            .unwrap();

        let before = rt
            .neighbors(
                None,
                note_source.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(before.len(), 1);

        // Soft delete must NOT cascade edges (data-vs-view principle).
        let deleted = rt.delete_note(None, note_target.id, false).await.unwrap();
        assert!(deleted, "soft delete must return true");

        let after = rt
            .neighbors(
                None,
                note_source.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(
            after.len(),
            1,
            "soft delete must NOT cascade edges; got {after:?}"
        );
    }

    // ---- delete_edge public-API safety (fix/annotates round-3) ----

    // Passing an entity/note UUID to `delete_edge` must return Ok(false) with no
    // side effects — it must NOT delete inbound annotates edges targeting that record.
    // Without the get_edge guard, the old code would cascade inbound edges before
    // returning false.
    #[tokio::test]
    async fn delete_edge_non_edge_uuid_has_no_side_effects() {
        let rt = rt();

        // Create an entity that has an inbound annotates edge.
        let entity = rt
            .create_entity(None, "concept", None, "Target", None, None, vec![])
            .await
            .unwrap();
        let note = rt
            .create_note(
                None,
                "observation",
                None,
                "annotates the entity",
                0.5,
                None,
                vec![entity.id],
            )
            .await
            .unwrap();

        // Confirm the annotates edge exists.
        let before = rt
            .neighbors(
                None,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(before.len(), 1, "annotates edge must exist before test");
        let annotates_edge_id: Uuid = before[0].edge_id;

        // Call delete_edge with the entity UUID (NOT an edge UUID).
        let result = rt.delete_edge(None, entity.id, true).await;
        assert!(
            result.is_ok(),
            "delete_edge must not error on a non-edge UUID"
        );
        assert!(
            !result.unwrap(),
            "delete_edge must return false for a non-edge UUID"
        );

        // The inbound annotates edge to the entity must still exist — no side effects.
        let after = rt
            .neighbors(
                None,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(
            after.len(),
            1,
            "delete_edge with a non-edge UUID must not touch inbound annotates edges"
        );
        assert_eq!(
            after[0].edge_id, annotates_edge_id,
            "the original annotates edge must be unchanged"
        );
    }

    // ---- create_note compensation branch (fix/annotates round-3) ----

    // This test injects a deterministic failure on the second `link` call inside
    // `create_note_inner` (the one that would create the second annotates edge).
    // It verifies that the compensation branch is wired — i.e. this test would
    // fail if the `Err(e)` rollback arm at operations.rs were deleted.
    //
    // Injection mechanism: LINK_FAIL_AFTER thread-local (ops.rs, cfg(test) only).
    // Setting it to 2 forces the 2nd link call to return an error.  The counter is
    // reset to 0 once triggered, so no other test is affected.
    #[tokio::test]
    async fn create_note_multi_annotates_second_link_failure_rolls_back_partial_write() {
        let rt = rt();
        let t1 = rt
            .create_entity(None, "concept", None, "T1", None, None, vec![])
            .await
            .unwrap();
        let t2 = rt
            .create_entity(None, "concept", None, "T2", None, None, vec![])
            .await
            .unwrap();

        // Arm the injection: fail on the 2nd link (link_idx+1 == 2).
        LINK_FAIL_AFTER.with(|cell| cell.set(2));

        let result = rt
            .create_note(
                None,
                "observation",
                None,
                "rollback target",
                0.5,
                None,
                vec![t1.id, t2.id],
            )
            .await;

        // The call must fail with the injected error.
        assert!(
            result.is_err(),
            "create_note must propagate the injected link failure"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("injected link failure"),
            "error must carry injection message; got: {err_msg}"
        );

        // Compensation must have removed the note row.
        let notes = rt.list_notes(None, None, 1000, 0).await.unwrap();
        assert!(
            notes.is_empty(),
            "compensation must remove the note row; got {notes:?}"
        );

        // FTS must have no hit for the content.
        let hits = rt
            .search_notes(None, "rollback target", None, 10, None)
            .await
            .unwrap();
        assert!(
            hits.is_empty(),
            "compensation must clean FTS index; got {hits:?}"
        );

        // No partial annotates edges must remain (first edge must have been deleted).
        let edges_from_t1 = rt
            .neighbors(
                None,
                t1.id,
                Direction::In,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        let edges_from_t2 = rt
            .neighbors(
                None,
                t2.id,
                Direction::In,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert!(
            edges_from_t1.is_empty(),
            "compensation must delete the first annotates edge; got {edges_from_t1:?}"
        );
        assert!(
            edges_from_t2.is_empty(),
            "no second annotates edge must exist; got {edges_from_t2:?}"
        );
    }

    // ---- #232 soft-delete index cleanup tests ----

    #[tokio::test]
    async fn soft_delete_entity_removes_indexes() {
        let rt = rt();
        let entity = rt
            .create_entity(
                None,
                "concept",
                None,
                "QuantumEntanglement",
                Some("unique FTS term xzqjwv for soft delete test"),
                None,
                vec![],
            )
            .await
            .unwrap();

        let ns = rt.ns(None).to_string();

        let before = rt
            .text(None)
            .unwrap()
            .search(TextSearchRequest {
                query: "xzqjwv".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    namespaces: vec![ns.clone()],
                    ..Default::default()
                }),
                top_k: 10,
                snippet_chars: 100,
            })
            .await
            .unwrap();
        assert!(
            before.iter().any(|h| h.subject_id == entity.id),
            "entity must be in FTS before soft-delete"
        );

        let deleted = rt.delete_entity(None, entity.id, false).await.unwrap();
        assert!(deleted, "soft delete must return true");

        let after = rt
            .text(None)
            .unwrap()
            .search(TextSearchRequest {
                query: "xzqjwv".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    namespaces: vec![ns],
                    ..Default::default()
                }),
                top_k: 10,
                snippet_chars: 100,
            })
            .await
            .unwrap();
        assert!(
            after.iter().all(|h| h.subject_id != entity.id),
            "soft-deleted entity must be removed from FTS index"
        );
    }

    #[tokio::test]
    async fn soft_delete_note_removes_indexes() {
        let rt = rt();
        let note = rt
            .create_note(
                None,
                "observation",
                None,
                "SpectralDecomposition unique term yvwkqz for soft delete test",
                0.7,
                None,
                vec![],
            )
            .await
            .unwrap();

        let before = rt
            .search_notes(None, "yvwkqz", None, 10, None)
            .await
            .unwrap();
        assert!(
            before.iter().any(|h| h.note_id == note.id),
            "note must be in FTS before soft-delete"
        );

        let deleted = rt.delete_note(None, note.id, false).await.unwrap();
        assert!(deleted, "soft delete must return true");

        let after = rt
            .search_notes(None, "yvwkqz", None, 10, None)
            .await
            .unwrap();
        assert!(
            after.iter().all(|h| h.note_id != note.id),
            "soft-deleted note must be removed from FTS index"
        );
    }

    // F010 (CRIT): ADR-002 base endpoint allowlist — unlisted triples must fail closed.
    // Document->Document Extends is not in the ADR-002 table; current generic fallthrough accepts it.
    #[tokio::test]
    async fn link_extends_document_to_document_returns_invalid_input() {
        let rt = rt();
        let d1 = rt
            .create_entity(None, "document", None, "DocA", None, None, vec![])
            .await
            .unwrap();
        let d2 = rt
            .create_entity(None, "document", None, "DocB", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(None, d1.id, d2.id, EdgeRelation::Extends, 1.0, None)
            .await;
        assert!(
            result.is_err(),
            "F010: document->document Extends must be rejected by ADR-002 allowlist; \
             current generic entity fallthrough incorrectly accepts it"
        );
    }

    // F010 happy path: Concept->Concept Extends is in the ADR-002 allowlist and must succeed.
    #[tokio::test]
    async fn link_extends_concept_to_concept_succeeds() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", None, "CA", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "CB", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "F010: concept->concept Extends must be allowed (ADR-002 allowlist)"
        );
    }

    // F012 (CRIT): CompetesWith is symmetric; reversed pair must deduplicate to one canonical row.
    // Current code stores both directions as distinct rows (no canonicalization).
    #[tokio::test]
    async fn link_symmetric_relation_canonicalizes_endpoint_order() {
        use khive_storage::EdgeFilter;
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", None, "ConceptP", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "ConceptQ", None, None, vec![])
            .await
            .unwrap();
        // Link A->B then B->A with the same symmetric relation.
        rt.link(None, a.id, b.id, EdgeRelation::CompetesWith, 1.0, None)
            .await
            .unwrap();
        rt.link(None, b.id, a.id, EdgeRelation::CompetesWith, 1.0, None)
            .await
            .unwrap();
        let count = rt
            .graph(None)
            .unwrap()
            .count_edges(EdgeFilter::default())
            .await
            .unwrap();
        assert_eq!(
            count,
            1,
            "F012: CompetesWith is symmetric; A->B and B->A must deduplicate to one canonical row; \
             found {count} rows (canonicalization not yet implemented)"
        );
    }

    // F010 (ADR-002): Supersedes — positive tests for all 5 allowed entity kinds.
    #[tokio::test]
    async fn f010_supersedes_document_to_document_allowed() {
        let rt = rt();
        let a = rt
            .create_entity(None, "document", None, "DocA", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "document", None, "DocB", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(None, b.id, a.id, EdgeRelation::Supersedes, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "document->document Supersedes must be allowed (ADR-002:191), got {result:?}"
        );
    }

    #[tokio::test]
    async fn f010_supersedes_artifact_to_artifact_allowed() {
        let rt = rt();
        let a = rt
            .create_entity(None, "artifact", None, "ArtA", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "artifact", None, "ArtB", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(None, b.id, a.id, EdgeRelation::Supersedes, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "artifact->artifact Supersedes must be allowed (ADR-002:192), got {result:?}"
        );
    }

    #[tokio::test]
    async fn f010_supersedes_service_to_service_allowed() {
        let rt = rt();
        let a = rt
            .create_entity(None, "service", None, "SvcA", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "service", None, "SvcB", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(None, b.id, a.id, EdgeRelation::Supersedes, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "service->service Supersedes must be allowed (ADR-002:193), got {result:?}"
        );
    }

    #[tokio::test]
    async fn f010_supersedes_dataset_to_dataset_allowed() {
        let rt = rt();
        let a = rt
            .create_entity(None, "dataset", None, "DataA", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "dataset", None, "DataB", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(None, b.id, a.id, EdgeRelation::Supersedes, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "dataset->dataset Supersedes must be allowed (ADR-002:194), got {result:?}"
        );
    }

    // F010 (ADR-002): Supersedes — negative tests for rejected entity kinds.
    #[tokio::test]
    async fn f010_supersedes_project_to_project_rejected() {
        let rt = rt();
        let a = rt
            .create_entity(None, "project", None, "ProjA", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "project", None, "ProjB", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(None, b.id, a.id, EdgeRelation::Supersedes, 1.0, None)
            .await;
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(_))),
            "project->project Supersedes must be rejected (not in ADR-002 allowlist), got {result:?}"
        );
    }

    #[tokio::test]
    async fn f010_supersedes_person_to_person_rejected() {
        let rt = rt();
        let a = rt
            .create_entity(None, "person", None, "Alice", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "person", None, "Bob", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(None, b.id, a.id, EdgeRelation::Supersedes, 1.0, None)
            .await;
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(_))),
            "person->person Supersedes must be rejected (not in ADR-002 allowlist), got {result:?}"
        );
    }

    #[tokio::test]
    async fn f010_supersedes_org_to_org_rejected() {
        let rt = rt();
        let a = rt
            .create_entity(None, "org", None, "OrgA", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "org", None, "OrgB", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(None, b.id, a.id, EdgeRelation::Supersedes, 1.0, None)
            .await;
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(_))),
            "org->org Supersedes must be rejected (not in ADR-002 allowlist), got {result:?}"
        );
    }

    // Fix 1: Supersedes entity→entity — same kind (concept→concept) must be allowed.
    #[tokio::test]
    async fn f010_supersedes_same_kind_entity_allowed() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", None, "OldV", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "NewV", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(None, b.id, a.id, EdgeRelation::Supersedes, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "concept->concept Supersedes must be allowed by ADR-002 allowlist, got {result:?}"
        );
    }

    // F161: ADR-009 target_backend invariant — all edges written through link() must have
    // target_backend = None because validate_edge_relation_endpoints already ensured the
    // target exists locally.
    #[tokio::test]
    async fn f161_link_always_writes_null_target_backend() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(None, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        assert!(
            edge.target_backend.is_none(),
            "ADR-009: target_backend must be None for locally-routed edges (F161); got {:?}",
            edge.target_backend
        );
    }

    // F161: link_many must also write null target_backend for all local edges.
    #[tokio::test]
    async fn f161_link_many_always_writes_null_target_backend() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(None, "concept", None, "C", None, None, vec![])
            .await
            .unwrap();
        let specs = vec![
            LinkSpec {
                namespace: None,
                source_id: a.id,
                target_id: b.id,
                relation: EdgeRelation::Extends,
                weight: 1.0,
                metadata: None,
            },
            LinkSpec {
                namespace: None,
                source_id: a.id,
                target_id: c.id,
                relation: EdgeRelation::Enables,
                weight: 1.0,
                metadata: None,
            },
        ];
        let edges = rt.link_many(specs).await.unwrap();
        for edge in &edges {
            assert!(
                edge.target_backend.is_none(),
                "ADR-009: target_backend must be None for locally-routed edges in link_many (F161); got {:?}",
                edge.target_backend
            );
        }
    }

    // F012: symmetric relation neighbors — competes_with queried from the non-canonical
    // endpoint must still return results when direction=Out is requested.
    #[tokio::test]
    async fn f012_symmetric_neighbors_visible_from_both_endpoints() {
        let rt = rt();
        let a = rt
            .create_entity(None, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        // Link A→B competes_with; if A.id > B.id the edge is stored as B→A (canonical).
        rt.link(None, a.id, b.id, EdgeRelation::CompetesWith, 1.0, None)
            .await
            .unwrap();
        // Both endpoints should see the edge regardless of direction=Out.
        let from_a = rt
            .neighbors(
                None,
                a.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::CompetesWith]),
            )
            .await
            .unwrap();
        let from_b = rt
            .neighbors(
                None,
                b.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::CompetesWith]),
            )
            .await
            .unwrap();
        assert_eq!(
            from_a.len(),
            1,
            "node A must see competes_with neighbor from Direction::Out (F012); got {from_a:?}"
        );
        assert_eq!(
            from_b.len(),
            1,
            "node B must see competes_with neighbor from Direction::Out (F012); got {from_b:?}"
        );
    }

    // Fix 1: Supersedes entity→entity — cross-kind (concept→document) must be rejected.
    #[tokio::test]
    async fn f010_supersedes_cross_kind_entity_rejected() {
        let rt = rt();
        let concept = rt
            .create_entity(None, "concept", None, "MyConcept", None, None, vec![])
            .await
            .unwrap();
        let doc = rt
            .create_entity(None, "document", None, "MyDoc", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(
                None,
                concept.id,
                doc.id,
                EdgeRelation::Supersedes,
                1.0,
                None,
            )
            .await;
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(_))),
            "concept->document Supersedes must be rejected by ADR-002 allowlist, got {result:?}"
        );
    }
}
