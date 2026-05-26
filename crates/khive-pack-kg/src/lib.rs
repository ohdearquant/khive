//! pack-kg — Knowledge Graph verb pack for khive.
//!
//! Provides 14 verbs for managing entities, notes, edges, graph queries, and
//! event-sourced proposals (ADR-046) in a research knowledge graph. This is
//! the first-party pack shipped with the khive binary.
//!
//! ## Proposal worker architecture (ADR-046 §5)
//!
//! Proposal side-effects are handled by two workers:
//!
//! - [`apply_worker::ProposalApplyWorker`]: subscribes to approved proposals,
//!   applies the changeset, emits `ProposalApplied`.
//! - [`projection_worker::ProposalsProjectionWorker`]: maintains the
//!   `proposals_open` projection table from all four proposal EventKinds.
//!
//! The KG handlers emit events first, then call the workers. Handlers do NOT
//! update `proposals_open` directly.

pub mod apply_worker;
pub mod handlers;
pub mod projection_worker;
pub mod vocab;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, Pack, ParamDef, VerbCategory, Visibility};

pub use khive_types::EntityKind;
pub use vocab::NoteKind;

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
}

// ADR-060 / ADR-025: Illocutionary classification (Searle 1976)
//   Assertive  — retrieves/presents state of affairs
//   Commissive — commits caller to a persistent change
//   Declaration — changes institutional status by fiat
//
// Verbs 12-14 (propose, review, withdraw) added per ADR-046 (cluster-22).
static KG_HANDLERS: [HandlerDef; 14] = [
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
                description: "Substrate or granular kind to list: \"entity\" | \"note\" | \"edge\" | granular kinds.",
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
                name: "tags",
                param_type: "array of string",
                required: false,
                description: "Filter by all listed tags.",
            },
        ],
    },
    // Declaration: changes entity or edge state by fiat
    HandlerDef {
        name: "update",
        description: "Patch entity or edge fields",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "id",
                param_type: "uuid",
                required: true,
                description: "UUID of the entity or edge to patch.",
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
                description: "New name (entities only).",
            },
            ParamDef {
                name: "description",
                param_type: "string",
                required: false,
                description: "New description (entities only).",
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
        description: "GQL pattern matching",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[ParamDef {
            name: "query",
            param_type: "string",
            required: true,
            description: "GQL pattern query string.",
        }],
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
];

impl KgPack {
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

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "create" => self.handle_create(token, params, registry).await,
            "get" => self.handle_get(token, params).await,
            "list" => self.handle_list(token, params, registry).await,
            "update" => self.handle_update(token, params, registry).await,
            "delete" => self.handle_delete(token, params, registry).await,
            "merge" => self.handle_merge(token, params, registry).await,
            "search" => self.handle_search(token, params, registry).await,
            "link" => self.handle_link(token, params).await,
            "neighbors" => self.handle_neighbors(token, params).await,
            "traverse" => self.handle_traverse(token, params).await,
            "query" => self.handle_query(token, params).await,
            "propose" => self.handle_propose(token, params).await,
            "review" => self.handle_review(token, params).await,
            "withdraw" => self.handle_withdraw(token, params).await,
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
}
