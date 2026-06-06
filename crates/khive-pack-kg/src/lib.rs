// FILE SIZE JUSTIFICATION: lib.rs declares the full KG_HANDLERS table (16 HandlerDef entries),
// KG_EDGE_RULES, and the Pack/PackRuntime impl. The HandlerDef table is a single flat array of
// static data — splitting it across files would require unsafe static refs or separate crates
// without any architectural benefit. The inline test section at the bottom of this file tests
// the KgPack dispatch surface and requires access to pack internals unavailable in a separate
// integration test crate.

//! pack-kg — Knowledge Graph verb pack for khive.
//!
//! Provides 16 verbs for managing entities, notes, edges, graph queries, and
//! event-sourced proposals (ADR-046). First-party pack shipped with the khive binary.

pub mod apply_worker;
pub mod entity_type_registry;
pub mod handlers;
pub mod projection_worker;
pub mod vocab;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{
    EdgeEndpointRule, EdgeRelation, EndpointKind, HandlerDef, Pack, ParamDef, VerbCategory,
    Visibility,
};

pub use entity_type_registry::{EntityTypeDef, EntityTypeRegistry, ResolvedType};
pub use khive_types::EntityKind;
pub use vocab::NoteKind;

/// ADR-002 §"Pack-extensible edge endpoints": KG pack extends the base entity→entity
/// allowlist with person→org and org→org relationship pairs. These are additive only
/// — the base contract in operations.rs is unchanged.
static KG_EDGE_RULES: [EdgeEndpointRule; 7] = [
    EdgeEndpointRule {
        relation: EdgeRelation::PartOf,
        source: EndpointKind::EntityOfKind("person"),
        target: EndpointKind::EntityOfKind("org"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::InstanceOf,
        source: EndpointKind::EntityOfKind("person"),
        target: EndpointKind::EntityOfKind("org"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfKind("org"),
        target: EndpointKind::EntityOfKind("org"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Enables,
        source: EndpointKind::EntityOfKind("org"),
        target: EndpointKind::EntityOfKind("org"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::EntityOfKind("org"),
        target: EndpointKind::EntityOfKind("org"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::PartOf,
        source: EndpointKind::EntityOfKind("org"),
        target: EndpointKind::EntityOfKind("org"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Precedes,
        source: EndpointKind::EntityOfKind("org"),
        target: EndpointKind::EntityOfKind("org"),
    },
];

/// KG pack vocabulary declaration.
pub struct KgPack {
    runtime: KhiveRuntime,
}

impl Pack for KgPack {
    const NAME: &'static str = "kg";
    const NOTE_KINDS: &'static [&'static str] = &[
        "observation",
        "insight",
        "question",
        "decision",
        "reference",
    ];
    const ENTITY_KINDS: &'static [&'static str] = &[
        "concept", "document", "dataset", "project", "person", "org", "artifact", "service",
    ];
    const HANDLERS: &'static [HandlerDef] = &KG_HANDLERS;
    const EDGE_RULES: &'static [EdgeEndpointRule] = &KG_EDGE_RULES;
}

// ADR-060 / ADR-025: Illocutionary classification (Searle 1976)
//   Assertive  — retrieves/presents state of affairs
//   Commissive — commits caller to a persistent change
//   Declaration — changes institutional status by fiat
//
// Verbs 12-14 (propose, review, withdraw) added per ADR-046 (cluster-22).
// Verb 15 (verbs) added for top-level verb discovery (ue-help-introspection H5).
// Verb 16 (stats) added for namespace statistics.
//
// Issue #497 — Visibility audit: all 16 handlers are Visibility::Verb.
// Full rationale in docs/design.md §"Verb Visibility Audit".
static KG_HANDLERS: [HandlerDef; 16] = [
    // Commissive: commits an entity or note to the namespace
    HandlerDef {
        name: "create",
        description: "Create an entity or note",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "kind",
                param_type: "string",
                required: true,
                description: "Substrate or granular kind: \"entity\" | \"note\" | \"concept\" | \"document\" | \"observation\" | …",
            },
            ParamDef {
                name: "name",
                param_type: "string",
                required: false,
                description: "Human-readable name (entities).",
            },
            ParamDef {
                name: "entity_kind",
                param_type: "string",
                required: false,
                description: "Fine-grained entity kind when kind=\"entity\" (concept | document | dataset | project | person | org | artifact | service).",
            },
            ParamDef {
                name: "note_kind",
                param_type: "string",
                required: false,
                description: "Fine-grained note kind when kind=\"note\" (observation | insight | question | decision | reference).",
            },
            ParamDef {
                name: "content",
                param_type: "string",
                required: false,
                description: "Body text (notes).",
            },
            ParamDef {
                name: "description",
                param_type: "string",
                required: false,
                description: "Free-text description (entities).",
            },
            ParamDef {
                name: "tags",
                param_type: "array of string",
                required: false,
                description: "Tag list.",
            },
            ParamDef {
                name: "entity_type",
                param_type: "string",
                required: false,
                description: "First-class entity type tag (e.g. \"paper\", \"algorithm\", \"tool\"). Stored in the entity's type field; also available in properties.",
            },
            ParamDef {
                name: "properties",
                param_type: "object",
                required: false,
                description: "Arbitrary JSON properties.",
            },
        ],
    },
    // Assertive: retrieves and presents a record
    HandlerDef {
        name: "get",
        description: "Fetch any record by UUID",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[ParamDef {
            name: "id",
            param_type: "uuid",
            required: true,
            description: "UUID of the entity, note, or edge to fetch.",
        }],
    },
    // Assertive: retrieves and presents filtered records
    HandlerDef {
        name: "list",
        description: "List records with optional filtering",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "kind",
                param_type: "string",
                required: true,
                description: "Substrate or granular kind to list: \"entity\" | \"note\" | \"edge\" | \"event\" | \"proposal\" | granular kinds.",
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Maximum records to return (default 20).",
            },
            ParamDef {
                name: "offset",
                param_type: "integer",
                required: false,
                description: "Pagination offset (default 0).",
            },
            ParamDef {
                name: "entity_kind",
                param_type: "string",
                required: false,
                description: "Fine-grained entity kind filter when kind=\"entity\" (concept | document | dataset | project | person | org | artifact | service).",
            },
            ParamDef {
                name: "entity_type",
                param_type: "string",
                required: false,
                description: "Filter by entity type field when kind=\"entity\" (e.g. \"paper\", \"algorithm\", \"tool\").",
            },
            ParamDef {
                name: "note_kind",
                param_type: "string",
                required: false,
                description: "Fine-grained note kind filter when kind=\"note\" (observation | insight | question | decision | reference).",
            },
            ParamDef {
                name: "tags",
                param_type: "array of string",
                required: false,
                description: "Filter entities by any of these tags (kind=\"entity\" only).",
            },
            ParamDef {
                name: "source_id",
                param_type: "uuid",
                required: false,
                description: "Filter edges by source node UUID (kind=\"edge\" only).",
            },
            ParamDef {
                name: "target_id",
                param_type: "uuid",
                required: false,
                description: "Filter edges by target node UUID (kind=\"edge\" only).",
            },
            ParamDef {
                name: "relations",
                param_type: "array of string",
                required: false,
                description: "Filter edges to these relation types (kind=\"edge\" only).",
            },
            ParamDef {
                name: "min_weight",
                param_type: "number",
                required: false,
                description: "Minimum edge weight inclusive (kind=\"edge\" only).",
            },
            ParamDef {
                name: "max_weight",
                param_type: "number",
                required: false,
                description: "Maximum edge weight inclusive (kind=\"edge\" only).",
            },
            ParamDef {
                name: "event_kind",
                param_type: "string",
                required: false,
                description: "Filter events to a single EventKind (kind=\"event\" only). E.g. \"ProposalCreated\".",
            },
            ParamDef {
                name: "event_kinds",
                param_type: "array of string",
                required: false,
                description: "Filter events to multiple EventKinds (kind=\"event\" only). Additive with event_kind.",
            },
            ParamDef {
                name: "thread_id",
                param_type: "string",
                required: false,
                description: "Filter messages by thread ID (kind=\"message\" only). Accepts full UUID or 8-char prefix.",
            },
            ParamDef {
                name: "direction",
                param_type: "string",
                required: false,
                description: "Filter messages by direction (kind=\"message\" only): \"inbound\" | \"outbound\".",
            },
            ParamDef {
                name: "from",
                param_type: "string",
                required: false,
                description: "Filter messages by sender identifier (kind=\"message\" only).",
            },
            ParamDef {
                name: "to",
                param_type: "string",
                required: false,
                description: "Filter messages by recipient identifier (kind=\"message\" only).",
            },
            ParamDef {
                name: "read",
                param_type: "bool",
                required: false,
                description: "Filter messages by read status (kind=\"message\" only): true = read, false = unread.",
            },
        ],
    },
    // Assertive: returns aggregate substrate counts (#280)
    HandlerDef {
        name: "stats",
        description: "Return aggregate KG substrate counts (entities, edges, notes)",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    },
    // Declaration: changes entity or edge state by fiat
    HandlerDef {
        name: "update",
        description: "Patch entity, note, or edge fields. Accepted fields depend on substrate: \
                       entities accept name/description/properties/tags; notes accept \
                       name/content/salience/decay_factor/properties; edges accept relation/weight/properties.",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "id",
                param_type: "uuid",
                required: true,
                description: "UUID of the entity, note, or edge to patch.",
            },
            ParamDef {
                name: "kind",
                param_type: "string",
                required: false,
                description: "Substrate hint (entity | note | edge). Omit to resolve substrate from UUID (ADR-014).",
            },
            ParamDef {
                name: "name",
                param_type: "string",
                required: false,
                description: "New name (entities and notes).",
            },
            ParamDef {
                name: "description",
                param_type: "string",
                required: false,
                description: "New description (entities only; notes use 'content' for body text).",
            },
            ParamDef {
                name: "content",
                param_type: "string",
                required: false,
                description: "New body text (notes only).",
            },
            ParamDef {
                name: "salience",
                param_type: "number",
                required: false,
                description: "Importance score 0.0–1.0 (notes only; affects recall ranking).",
            },
            ParamDef {
                name: "decay_factor",
                param_type: "number",
                required: false,
                description: "Decay rate >= 0 (notes only; higher = faster decay).",
            },
            ParamDef {
                name: "relation",
                param_type: "string",
                required: false,
                description: "New edge relation (edges only; any of the 15 canonical relations).",
            },
            ParamDef {
                name: "weight",
                param_type: "number",
                required: false,
                description: "New edge weight 0.0–1.0 (edges only; 1.0=definitional, 0.7-0.9=strong, 0.4-0.6=plausible).",
            },
            ParamDef {
                name: "properties",
                param_type: "object",
                required: false,
                description: "Properties to merge in (shallow merge).",
            },
            ParamDef {
                name: "tags",
                param_type: "array of string",
                required: false,
                description: "Replace tag list.",
            },
        ],
    },
    // Declaration: declares a record removed
    HandlerDef {
        name: "delete",
        description: "Soft or hard delete a record",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "id",
                param_type: "uuid",
                required: true,
                description: "UUID of the record to delete.",
            },
            ParamDef {
                name: "kind",
                param_type: "string",
                required: false,
                description: "Substrate hint (entity | note | edge). Omit to resolve substrate from UUID (ADR-014).",
            },
            ParamDef {
                name: "hard",
                param_type: "bool",
                required: false,
                description: "If true, permanently remove with edge cascade (default false = soft delete).",
            },
        ],
    },
    // Declaration: declares two entities identical
    HandlerDef {
        name: "merge",
        description: "Deduplicate two entities",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "into_id",
                param_type: "uuid",
                required: true,
                description: "The entity that survives the merge (canonical).",
            },
            ParamDef {
                name: "from_id",
                param_type: "uuid",
                required: true,
                description: "The entity to merge from (will be soft-deleted after merge).",
            },
        ],
    },
    // Assertive: retrieves and presents search results
    HandlerDef {
        name: "search",
        description: "Hybrid FTS + vector search",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "kind",
                param_type: "string",
                required: true,
                description: "Substrate or granular kind to search.",
            },
            ParamDef {
                name: "query",
                param_type: "string",
                required: true,
                description: "Free-text search query.",
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Maximum results to return (default 10).",
            },
            ParamDef {
                name: "entity_kind",
                param_type: "string",
                required: false,
                description: "Filter search results to a specific entity kind (kind=\"entity\" only).",
            },
            ParamDef {
                name: "entity_type",
                param_type: "string",
                required: false,
                description: "Filter search results by entity type field (kind=\"entity\" only, e.g. \"paper\", \"algorithm\").",
            },
            ParamDef {
                name: "note_kind",
                param_type: "string",
                required: false,
                description: "Filter search results to a specific note kind (kind=\"note\" only).",
            },
            ParamDef {
                name: "include_superseded",
                param_type: "bool",
                required: false,
                description: "When true, include notes that are targeted by a supersedes edge (kind=\"note\" only). Default false — superseded notes are excluded from results.",
            },
            ParamDef {
                name: "properties",
                param_type: "object",
                required: false,
                description: "Post-filter search hits to entities whose properties contain all listed key=value pairs (kind=\"entity\" only). Applied after FTS+vector ranking. E.g. {\"type\": \"paper\", \"domain\": \"attention\"}.",
            },
            ParamDef {
                name: "tags",
                param_type: "array",
                required: false,
                description: "Post-filter entity search hits to entities with any listed tag (kind=\"entity\" only, OR semantics, case-insensitive). Applied after FTS+vector ranking. E.g. [\"rust\", \"ml\"].",
            },
            ParamDef {
                name: "min_score",
                param_type: "number",
                required: false,
                description: "Optional caller-supplied score floor (0.0–1.0). Results below this threshold are discarded. No server default is applied; RRF rank-1 scores are typically 0.013–0.033 on small corpora. Pass e.g. 0.02 to suppress near-zero noise hits.",
            },
        ],
    },
    // Commissive: commits a typed edge to the graph
    HandlerDef {
        name: "link",
        description: "Create a typed directed edge",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "source_id",
                param_type: "uuid",
                required: true,
                description: "UUID of the source node.",
            },
            ParamDef {
                name: "target_id",
                param_type: "uuid",
                required: true,
                description: "UUID of the target node.",
            },
            ParamDef {
                name: "relation",
                param_type: "string",
                required: true,
                description: "Edge relation (contains | part_of | instance_of | extends | variant_of | introduced_by | supersedes | derived_from | precedes | depends_on | enables | implements | competes_with | composed_with | annotates).",
            },
            ParamDef {
                name: "weight",
                param_type: "number",
                required: false,
                description: "Edge weight 0.0–1.0 (default 1.0). 1.0=definitional, 0.7-0.9=strong, 0.4-0.6=plausible.",
            },
        ],
    },
    // Assertive: retrieves immediate graph neighbors
    HandlerDef {
        name: "neighbors",
        description: "Immediate graph neighbors",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "node_id",
                param_type: "uuid",
                required: true,
                description: "UUID of the node whose neighbors to return.",
            },
            ParamDef {
                name: "direction",
                param_type: "string",
                required: false,
                description: "Edge direction: \"outgoing\" | \"incoming\" | \"both\" (default \"both\").",
            },
            ParamDef {
                name: "relations",
                param_type: "array of string",
                required: false,
                description: "Filter to these relation types only.",
            },
            ParamDef {
                name: "min_weight",
                param_type: "number",
                required: false,
                description: "Minimum edge weight for returned neighbors (0.0–1.0). Edges below this threshold are excluded.",
            },
        ],
    },
    // Assertive: retrieves multi-hop traversal results
    HandlerDef {
        name: "traverse",
        description: "Multi-hop BFS traversal",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "roots",
                param_type: "array of uuid",
                required: true,
                description: "Starting node UUIDs for the traversal.",
            },
            ParamDef {
                name: "max_depth",
                param_type: "integer",
                required: false,
                description: "Maximum traversal depth (default 3).",
            },
            ParamDef {
                name: "relations",
                param_type: "array of string",
                required: false,
                description: "Restrict traversal to these relation types.",
            },
        ],
    },
    // Assertive: retrieves pattern-matched results
    HandlerDef {
        name: "query",
        description: "GQL pattern matching. When a traversal mixes fixed-length and variable-length chains, split it into separate query() calls.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "query",
                param_type: "string",
                required: true,
                description: "GQL pattern query string. Mixed fixed-length plus variable-length traversals are not compiled in one call; split them into separate query() calls, one for the fixed-length portion and one for the variable-length portion.",
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Maximum rows returned (default 500, hard cap 10 000).",
            },
        ],
    },
    // Commissive: commits a proposal to the namespace event log (ADR-046)
    HandlerDef {
        name: "propose",
        description: "Create an event-sourced change proposal",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "title",
                param_type: "string",
                required: true,
                description: "Short title for the proposal (must be non-empty).",
            },
            ParamDef {
                name: "description",
                param_type: "string",
                required: true,
                description: "Full description explaining the proposed change (must be non-empty).",
            },
            ParamDef {
                name: "changeset",
                param_type: "object",
                required: true,
                description: "Proposed changes. Discriminated by 'kind' field. \
                    Variants (all fields are structured objects, not JSON strings): \
                    add_entity — {kind: \"add_entity\", entity: {kind: <entity-kind>, name: <string>, description?: <string>, properties?: <object>, tags?: [<string>]}}; \
                    update_entity — {kind: \"update_entity\", id: <full UUID>, patch: {name?: <string>, description?: <string|null>, properties?: <object>, tags?: [<string>]}}; \
                    add_edge — {kind: \"add_edge\", source: <UUID>, target: <UUID>, relation: <EdgeRelation>, weight?: <float>}; \
                    add_note — {kind: \"add_note\", note: {kind: <note-kind>, content: <string>, name?: <string>, properties?: <object>}}; \
                    merge_entities — {kind: \"merge_entities\", into: <UUID>, from: <UUID>}; \
                    supersede_entity — {kind: \"supersede_entity\", old: <UUID>, new: <UUID>}; \
                    compound — {kind: \"compound\", steps: [<changeset>, ...]}.",
            },
            ParamDef {
                name: "reviewers",
                param_type: "array<string>",
                required: false,
                description: "Actor IDs requested as reviewers. Default: empty list.",
            },
            ParamDef {
                name: "expiry",
                param_type: "integer",
                required: false,
                description: "Expiry timestamp in microseconds since epoch. Omit for no expiry.",
            },
            ParamDef {
                name: "parent_id",
                param_type: "uuid",
                required: false,
                description: "UUID of a parent proposal this supersedes or extends.",
            },
        ],
    },
    // Declaration: approves/rejects/comments on a proposal (ADR-046)
    HandlerDef {
        name: "review",
        description: "Approve, reject, comment, or request changes on a proposal",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "proposal_id",
                param_type: "uuid",
                required: true,
                description: "Full UUID or 8-char short ID of the proposal to review.",
            },
            ParamDef {
                name: "decision",
                param_type: "string",
                required: true,
                description: "Review outcome: \"approve\" | \"reject\" | \"comment\" | \"request_changes\".",
            },
            ParamDef {
                name: "comment",
                param_type: "string",
                required: false,
                description: "Optional reviewer comment attached to the review event.",
            },
        ],
    },
    // Commissive: rescinds an open proposal (ADR-046)
    HandlerDef {
        name: "withdraw",
        description: "Withdraw an open proposal (proposer-only)",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "proposal_id",
                param_type: "uuid",
                required: true,
                description: "Full UUID or 8-char short ID of the open proposal to withdraw.",
            },
            ParamDef {
                name: "rationale",
                param_type: "string",
                required: false,
                description: "Optional reason for withdrawing the proposal.",
            },
        ],
    },
    // Assertive: verb discovery (ue-help-introspection H5)
    HandlerDef {
        name: "verbs",
        description: "List all MCP-callable verbs registered on this server. \
                       Internal subhandlers are excluded. \
                       Pass category=<name> to filter by illocutionary category \
                       (Assertive | Commissive | Declaration | Directive). \
                       Pass pack=<name> to filter by pack.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "category",
                param_type: "string",
                required: false,
                description: "Filter by illocutionary category: Assertive | Commissive | Declaration | Directive.",
            },
            ParamDef {
                name: "pack",
                param_type: "string",
                required: false,
                description: "Filter by pack name (e.g. \"kg\", \"gtd\", \"memory\", \"brain\", \"comm\", \"schedule\").",
            },
        ],
    },
];

/// Handle the `verbs` introspection verb (ue-help-introspection H5).
///
/// Returns all MCP-callable verbs registered on this server — identical to the
/// list the `request` tool's description advertises. Internal subhandlers
/// (`Visibility::Subhandler`) are excluded.
///
/// Supports optional `category` and `pack` filters so agents can enumerate a
/// subset of the verb surface without parsing the prose description.
fn handle_verbs(params: Value, registry: &VerbRegistry) -> Result<Value, RuntimeError> {
    #[derive(serde::Deserialize, Default)]
    struct VerbsParams {
        category: Option<String>,
        pack: Option<String>,
    }
    let p: VerbsParams =
        serde_json::from_value(params).map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

    let verbs: Vec<Value> = registry
        .all_verbs_with_names()
        .into_iter()
        .filter(|(pack_name, handler)| {
            let cat_ok = p
                .category
                .as_deref()
                .is_none_or(|c| format!("{:?}", handler.category).eq_ignore_ascii_case(c));
            let pack_ok = p
                .pack
                .as_deref()
                .is_none_or(|pk| pack_name.eq_ignore_ascii_case(pk));
            cat_ok && pack_ok
        })
        .map(|(pack_name, handler)| {
            serde_json::json!({
                "verb": handler.name,
                "pack": pack_name,
                "description": handler.description,
                "category": format!("{:?}", handler.category),
            })
        })
        .collect();

    let total = verbs.len();
    Ok(serde_json::json!({
        "verbs": verbs,
        "total": total,
    }))
}

impl KgPack {
    /// Create a new KG pack backed by the given runtime.
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }
}

// ── ADR-027: inventory self-registration ─────────────────────────────────────

struct KgPackFactory;

impl khive_runtime::PackFactory for KgPackFactory {
    fn name(&self) -> &'static str {
        "kg"
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(KgPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&KgPackFactory) }

#[async_trait]
impl PackRuntime for KgPack {
    fn name(&self) -> &str {
        "kg"
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <KgPack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <KgPack as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        &KG_HANDLERS
    }

    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        <KgPack as Pack>::EDGE_RULES
    }

    async fn warm(&self) {
        let _ = self.runtime.embed("khive warmup").await;
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        // The `verbs` introspection verb has no namespace side-effect and is routed
        // before graph dispatch.
        if verb == "verbs" {
            return handle_verbs(params, registry);
        }

        // KG graph operations honor the NamespaceToken minted by VerbRegistry::dispatch.
        // OSS sharing comes from the registry/runtime default namespace; cloud isolation
        // comes from authenticated token namespace plus backend-file routing (ADR-050).
        let graph_token = token;

        // Peek at `kind` for verbs that can operate on both entities and notes.
        let raw_kind = params
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let kind_is_entity_or_edge = matches!(
            raw_kind.as_str(),
            "entity"
                | "edge"
                | "concept"
                | "document"
                | "dataset"
                | "project"
                | "person"
                | "org"
                | "artifact"
                | "service"
        );

        match verb {
            // Kind-discriminated: override only for entity/edge kinds.
            "create" | "list" | "search" => {
                let tok = if kind_is_entity_or_edge {
                    graph_token
                } else {
                    token
                };
                match verb {
                    "create" => self.handle_create(tok, params, registry).await,
                    "list" => self.handle_list(tok, params, registry).await,
                    _ => self.handle_search(tok, params, registry).await,
                }
            }
            // Pure graph verbs: always use graph namespace.
            "link" => self.handle_link(graph_token, params).await,
            "neighbors" => self.handle_neighbors(graph_token, params).await,
            "traverse" => self.handle_traverse(graph_token, params).await,
            "query" => self.handle_query(graph_token, params).await,
            "propose" => self.handle_propose(graph_token, params).await,
            "review" => self.handle_review(graph_token, params, registry).await,
            "withdraw" => self.handle_withdraw(graph_token, params).await,
            "stats" => self.handle_stats(graph_token, params).await,
            "merge" => self.handle_merge(graph_token, params, registry).await,
            // UUID-based: entities/edges use graph token, notes/events use caller token.
            "get" => self.handle_get(token, graph_token, params).await,
            "update" => self.handle_update(graph_token, params, registry).await,
            "delete" => self.handle_delete(graph_token, params, registry).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "kg pack does not handle verb {verb:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod help_tests {
    use super::*;

    fn find_handler(name: &str) -> &'static HandlerDef {
        KG_HANDLERS
            .iter()
            .find(|h| h.name == name)
            .unwrap_or_else(|| panic!("handler {name:?} not found in KG_HANDLERS"))
    }

    #[test]
    fn propose_params_has_required_title_description_changeset() {
        let h = find_handler("propose");
        assert!(!h.params.is_empty(), "propose must have params");
        assert!(
            h.params.iter().any(|p| p.name == "title" && p.required),
            "propose must have required title param"
        );
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "description" && p.required),
            "propose must have required description param"
        );
        assert!(
            h.params.iter().any(|p| p.name == "changeset" && p.required),
            "propose must have required changeset param"
        );
    }

    #[test]
    fn propose_params_has_optional_reviewers_expiry_parent_id() {
        let h = find_handler("propose");
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "reviewers" && !p.required),
            "propose must document optional reviewers"
        );
        assert!(
            h.params.iter().any(|p| p.name == "expiry" && !p.required),
            "propose must document optional expiry"
        );
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "parent_id" && !p.required),
            "propose must document optional parent_id"
        );
    }

    #[test]
    fn review_params_has_required_proposal_id_and_decision() {
        let h = find_handler("review");
        assert!(!h.params.is_empty(), "review must have params");
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "proposal_id" && p.required),
            "review must have required proposal_id param"
        );
        assert!(
            h.params.iter().any(|p| p.name == "decision" && p.required),
            "review must have required decision param"
        );
        assert!(
            h.params.iter().any(|p| p.name == "comment" && !p.required),
            "review must document optional comment param"
        );
    }

    #[test]
    fn withdraw_params_has_required_proposal_id_and_optional_rationale() {
        let h = find_handler("withdraw");
        assert!(!h.params.is_empty(), "withdraw must have params");
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "proposal_id" && p.required),
            "withdraw must have required proposal_id param"
        );
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "rationale" && !p.required),
            "withdraw must document optional rationale param"
        );
    }

    // ── ue-help-introspection C2 regressions ─────────────────────────────────

    /// update.help must document `content` for notes (C2 / H4).
    #[test]
    fn update_params_documents_content_for_notes() {
        let h = find_handler("update");
        assert!(
            h.params.iter().any(|p| p.name == "content"),
            "update must document 'content' param (notes only)"
        );
        let content = h.params.iter().find(|p| p.name == "content").unwrap();
        assert!(
            content.description.contains("note"),
            "update.content description must mention 'note'"
        );
    }

    /// update.name must NOT say "entities only" (C2).
    #[test]
    fn update_params_name_not_entities_only() {
        let h = find_handler("update");
        let name_param = h.params.iter().find(|p| p.name == "name").unwrap();
        assert!(
            !name_param.description.contains("entities only"),
            "update.name must not claim 'entities only' — notes also have names"
        );
    }

    /// update.help must document `salience` for notes (H4).
    #[test]
    fn update_params_documents_salience_for_notes() {
        let h = find_handler("update");
        assert!(
            h.params.iter().any(|p| p.name == "salience"),
            "update must document 'salience' param (notes only)"
        );
    }

    /// update.help must document `decay_factor` for notes (H4).
    #[test]
    fn update_params_documents_decay_factor_for_notes() {
        let h = find_handler("update");
        assert!(
            h.params.iter().any(|p| p.name == "decay_factor"),
            "update must document 'decay_factor' param (notes only)"
        );
    }

    /// update.help must document `relation` for edges (codex High).
    #[test]
    fn update_params_documents_relation_for_edges() {
        let h = find_handler("update");
        assert!(
            h.params.iter().any(|p| p.name == "relation"),
            "update must document 'relation' param (edges only)"
        );
        let rel = h.params.iter().find(|p| p.name == "relation").unwrap();
        assert!(
            rel.description.contains("edge"),
            "update.relation description must mention 'edge'"
        );
    }

    /// update.help must document `weight` for edges (codex High).
    #[test]
    fn update_params_documents_weight_for_edges() {
        let h = find_handler("update");
        assert!(
            h.params.iter().any(|p| p.name == "weight"),
            "update must document 'weight' param (edges only)"
        );
        let w = h.params.iter().find(|p| p.name == "weight").unwrap();
        assert!(
            w.description.contains("edge"),
            "update.weight description must mention 'edge'"
        );
    }

    // ── ue-help-introspection C3 regression ──────────────────────────────────

    /// No handler named "thread" should exist in the KG pack.
    /// This guards against accidentally adding a `thread` Verb-visibility
    /// handler without a corresponding dispatch arm (C3).
    #[test]
    fn no_thread_verb_in_kg_handlers() {
        assert!(
            KG_HANDLERS.iter().all(|h| h.name != "thread"),
            "KG_HANDLERS must not contain a 'thread' handler — see C3"
        );
    }

    // ── ue-help-introspection H5 regression ──────────────────────────────────

    /// The `verbs` introspection handler must be present and have params.
    #[test]
    fn verbs_handler_is_present_and_has_params() {
        let h = find_handler("verbs");
        assert!(
            !h.params.is_empty(),
            "verbs must have documented params (category, pack)"
        );
        assert!(
            h.params.iter().any(|p| p.name == "category"),
            "verbs must document 'category' filter param"
        );
        assert!(
            h.params.iter().any(|p| p.name == "pack"),
            "verbs must document 'pack' filter param"
        );
    }

    // ── ADR-050 token-namespace contract ─────────────────────────────────────

    /// KG `create` with a `tenant-a` token stores the entity in `tenant-a`.
    ///
    /// Under ADR-050 the KG pack honors the NamespaceToken it receives.
    /// An entity created under `tenant-a` is visible to `tenant-a` and opaque to
    /// `tenant-b` (cross-namespace reads fail closed as NotFound).
    #[tokio::test]
    async fn kg_create_entity_honors_caller_namespace() {
        use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};
        use serde_json::json;

        let rt = KhiveRuntime::memory().expect("in-memory runtime");

        let tenant_a = rt
            .authorize(Namespace::parse("tenant-a").expect("valid namespace"))
            .unwrap();
        let tenant_b = rt
            .authorize(Namespace::parse("tenant-b").expect("valid namespace"))
            .unwrap();

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let pack = KgPack::new(rt.clone());

        // Create an entity using tenant-a token.
        let result = pack
            .dispatch(
                "create",
                json!({
                    "kind": "concept",
                    "name": "TenantConcept",
                    "description": "concept visible only to tenant-a"
                }),
                &registry,
                &tenant_a,
            )
            .await
            .expect("create must succeed");

        let entity_id = result
            .get("id")
            .and_then(|v| v.as_str())
            .expect("result must contain id");

        // tenant-a can retrieve the entity it created.
        let get_result = pack
            .dispatch("get", json!({ "id": entity_id }), &registry, &tenant_a)
            .await
            .expect("tenant-a must retrieve entity in its own namespace");

        assert_eq!(
            get_result
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "TenantConcept",
            "tenant-a must read back the entity it created"
        );

        // tenant-b cannot see tenant-a's entity.
        let not_found = pack
            .dispatch("get", json!({ "id": entity_id }), &registry, &tenant_b)
            .await;
        assert!(
            matches!(not_found, Err(RuntimeError::NotFound(_))),
            "tenant-b must not see tenant-a's entity (expected NotFound, got {:?})",
            not_found
        );
    }

    /// OSS default path: two entity creates with no explicit namespace land in
    /// the same default namespace, preserving the unified single-user graph.
    ///
    /// This regression guards that removing the KG pack namespace override does
    /// not break the OSS common path — the registry/runtime default namespace
    /// already ensures both entities end up in `local`.
    #[tokio::test]
    async fn kg_oss_default_namespace_entities_colocate() {
        use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};
        use serde_json::json;

        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let local_token = rt.authorize(Namespace::local()).unwrap();

        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let pack = KgPack::new(rt.clone());

        // Two creates with the default local token — no explicit namespace.
        let r1 = pack
            .dispatch(
                "create",
                json!({ "kind": "concept", "name": "Alpha" }),
                &registry,
                &local_token,
            )
            .await
            .expect("first create must succeed");
        let r2 = pack
            .dispatch(
                "create",
                json!({ "kind": "concept", "name": "Beta" }),
                &registry,
                &local_token,
            )
            .await
            .expect("second create must succeed");

        let id1 = r1.get("id").and_then(|v| v.as_str()).expect("id1");
        let id2 = r2.get("id").and_then(|v| v.as_str()).expect("id2");

        // Both entities readable via the local token — co-located in default namespace.
        pack.dispatch("get", json!({ "id": id1 }), &registry, &local_token)
            .await
            .expect("Alpha must be retrievable via local token");
        pack.dispatch("get", json!({ "id": id2 }), &registry, &local_token)
            .await
            .expect("Beta must be retrievable via local token");
    }

    #[test]
    fn query_help_documents_mixed_variable_chain_limitation() {
        let h = find_handler("query");
        assert!(
            h.description
                .contains("mixes fixed-length and variable-length"),
            "query help must document mixed fixed/variable traversal limitation"
        );
        let query_param = h
            .params
            .iter()
            .find(|p| p.name == "query")
            .expect("query param documented");
        assert!(
            query_param
                .description
                .contains("split them into separate query() calls"),
            "query param help must document split-query workaround"
        );
        let limit_param = h
            .params
            .iter()
            .find(|p| p.name == "limit")
            .expect("limit param must be documented in query handler metadata");
        assert!(!limit_param.required, "limit must be optional");
    }
}
