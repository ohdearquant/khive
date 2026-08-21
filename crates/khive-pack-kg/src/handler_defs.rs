//! Static `KG_HANDLERS` table (20 `HandlerDef` entries) and the `verbs` introspection handler.

// Illocutionary classification (Searle 1976):
//   Assertive  -- retrieves/presents state of affairs
//   Commissive -- commits caller to a persistent change
//   Declaration -- changes institutional status by fiat
//
// propose, review, and withdraw implement the event-sourced proposal
// lifecycle. verbs serves verb discovery. stats provides namespace
// statistics.

use serde_json::Value;

use khive_runtime::{RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, IdResolutionMode, ParamDef, VerbCategory, Visibility};

pub(crate) static KG_HANDLERS: [HandlerDef; 20] = [
    // Commissive: commits an entity or note to the namespace
    HandlerDef {
        name: "create",
        description: "Create an entity or note (singleton) or a batch of entities (bulk via `items`).",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "kind",
                param_type: "string",
                // `kind` is required for the singleton path but NOT for the bulk path:
                // each item in `items` carries its own `kind`. Required=false here to
                // reflect that `create(items=[...])` is valid without a top-level `kind`.
                required: false,
                description: "Substrate or granular kind for the singleton path: \
                              \"entity\" | \"note\" | \"concept\" | \"document\" | \
                              \"observation\" | … Required when `items` is absent.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "name",
                param_type: "string",
                required: false,
                description: "Human-readable name (entities, singleton path).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "entity_kind",
                param_type: "string",
                required: false,
                description: "Fine-grained entity kind when kind=\"entity\" (concept | document | dataset | project | person | org | artifact | service | resource).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "note_kind",
                param_type: "string",
                required: false,
                description: "Fine-grained note kind when kind=\"note\" (observation | insight | question | decision | reference).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "content",
                param_type: "string",
                required: false,
                description: "Body text (notes, singleton path).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "description",
                param_type: "string",
                required: false,
                description: "Free-text description (entities).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "embedding_content",
                param_type: "string",
                required: false,
                description: "Singleton kind=note only. A non-empty proper prefix of \
                              `content` to send to the vector embedder instead of the \
                              full text — use when `content` exceeds an embedder's \
                              input cap. Stored and FTS-indexed content are always the \
                              full `content`; this only replaces the vector input.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "tags",
                param_type: "array of string",
                required: false,
                description: "Tag list.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "entity_type",
                param_type: "string",
                required: false,
                description: "First-class entity type tag (e.g. \"paper\", \"algorithm\", \"tool\"). Stored in the entity's type field; also available in properties.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "properties",
                param_type: "object",
                required: false,
                description: "Arbitrary JSON properties.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "items",
                param_type: "array of object",
                required: false,
                description: "Bulk entity creation. Each element is an object with \
                              `kind` (required), `name` (required), and optional \
                              `entity_kind`, `entity_type`, `description`, `properties`, \
                              `tags`. When present, the top-level `kind` is NOT required. \
                              Capped at 1000 entries per request. Bulk-created entities \
                              skip vector embedding and are not vector-searchable until \
                              a subsequent `reindex` call.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "atomic",
                param_type: "bool",
                required: false,
                description: "Bulk path only. When true (default), all items succeed or \
                              none are written. When false, items are attempted individually \
                              and per-item errors are collected in the response.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "verbose",
                param_type: "bool",
                required: false,
                description: "Bulk path only. When true, the response includes the full \
                              entity objects in an `entities` array.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // Assertive: retrieves and presents a record
    HandlerDef {
        name: "get",
        description: "Fetch any record by UUID",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "id",
                param_type: "uuid",
                required: true,
                description: "Complete UUID or globally unique 8+ hex prefix of the entity, \
                              note, edge, event, or proposal to fetch. Entity-name fallback \
                              uses the primary namespace.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "include_deleted",
                param_type: "bool",
                required: false,
                description:
                    "If true, return soft-deleted entities (with deleted_at populated). Default false. \
                     Accepts a full UUID or a unique short hex prefix — prefix resolution falls back \
                     to soft-deleted entities when no live record matches.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // Assertive: retrieves and presents filtered records
    HandlerDef {
        name: "list",
        description: "List records with optional filtering. Offset-mode results always use \
                      {\"items\": [...], \"requested_limit\": N, \"effective_limit\": M, \
                      \"limit_clamped\": bool}; clients advance offset by items.length, while M \
                      discloses the server cap and is not a guaranteed row count. \
                      Entity, note, and edge cursor modes return \
                      {\"entities|notes|edges\": [...], \"next_after\": ...} with the same \
                      limit metadata. Caps are entity 500, note 200, edge 1000, event 1000, \
                      and proposal 500.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "kind",
                param_type: "string",
                required: true,
                description: "Substrate or granular kind to list: \"entity\" | \"note\" | \"edge\" | \"event\" | \"proposal\" | granular kinds.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Maximum records to return (default varies by kind). Values above \
                              the kind's server-side cap are clamped and return explicit \
                              requested_limit, effective_limit, and limit_clamped metadata.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "offset",
                param_type: "integer",
                required: false,
                description: "Pagination offset (default 0). For complete entity, note, or edge \
                              walks prefer \"after\", whose indexed seek cost does not grow with \
                              depth and whose boundaries are not shifted by concurrent inserts. \
                              Explicit offset and after values are mutually exclusive.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "after",
                param_type: "string",
                required: false,
                description: "Insertion-sequence cursor for entity, note, and edge lists: the full UUID from \
                              the prior page's next_after, or \"\" to start cursor mode. A new id is \
                              assigned a durable database sequence, so later inserts cannot fall behind \
                              an issued boundary even when timestamps tie. This is a live walk, not an \
                              MVCC snapshot: inserts may extend it, and rows committed after a terminal \
                              page require a new walk. Responses are {\"entities\": [...]}, \
                              {\"notes\": [...]}, or {\"edges\": [...]}, plus next_after. Reuse the \
                              same filters throughout a walk. A missing, hard-deleted, or out-of-scope \
                              cursor fails explicitly. Short prefixes are rejected because they can miss \
                              or be ambiguous while keyset pagination needs the exact stable insertion \
                              boundary. Mutually exclusive with offset.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "entity_kind",
                param_type: "string",
                required: false,
                description: "Fine-grained entity kind filter when kind=\"entity\" (concept | document | dataset | project | person | org | artifact | service | resource).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "entity_type",
                param_type: "string",
                required: false,
                description: "Filter by entity type field when kind=\"entity\" (e.g. \"paper\", \"algorithm\", \"tool\").",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "note_kind",
                param_type: "string",
                required: false,
                description: "Fine-grained note kind filter when kind=\"note\" (observation | insight | question | decision | reference).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "tags",
                param_type: "array of string",
                required: false,
                description: "Case-insensitive OR-filter over entity tags or note \
                              properties.tags (kind=\"entity\" or kind=\"note\").",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "source_id",
                param_type: "uuid",
                required: false,
                description: "Filter edges by source node complete UUID, unique 8+ hex prefix, \
                              or entity name (kind=\"edge\" only). Prefix and name resolution \
                              search the caller's primary namespace.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "target_id",
                param_type: "uuid",
                required: false,
                description: "Filter edges by target node complete UUID, unique 8+ hex prefix, \
                              or entity name (kind=\"edge\" only). Prefix and name resolution \
                              search the caller's primary namespace.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "relations",
                param_type: "array of string",
                required: false,
                description: "Filter edges to these relation types (kind=\"edge\" only).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "min_weight",
                param_type: "number",
                required: false,
                description: "Minimum edge weight inclusive (kind=\"edge\" only).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "max_weight",
                param_type: "number",
                required: false,
                description: "Maximum edge weight inclusive (kind=\"edge\" only).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "event_kind",
                param_type: "string",
                required: false,
                description: "Filter events to a single EventKind (kind=\"event\" only). E.g. \"ProposalCreated\".",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "event_kinds",
                param_type: "array of string",
                required: false,
                description: "Filter events to multiple EventKinds (kind=\"event\" only). Additive with event_kind.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "session_id",
                param_type: "uuid",
                required: false,
                description: "Filter events by an exact full session UUID (kind=\"event\" only). A short-prefix resolution can miss or be ambiguous, so it is rejected for this stable record filter.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "observed",
                param_type: "array of uuid",
                required: false,
                description: "Filter events that observed every listed exact full UUID (kind=\"event\" only). Short-prefix resolution can miss or be ambiguous, so prefixes are rejected for these stable record filters.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "selected",
                param_type: "array of uuid",
                required: false,
                description: "Filter events that selected every listed exact full UUID (kind=\"event\" only). Short-prefix resolution can miss or be ambiguous, so prefixes are rejected for these stable record filters.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "thread_id",
                param_type: "string",
                required: false,
                description: "Filter messages by thread ID (kind=\"message\" only). Accepts a \
                              complete UUID or a unique 8+ hex prefix resolved across stored \
                              thread roots in the caller's primary namespace; missing or \
                              ambiguous prefixes fail explicitly. Legacy exceptions: input that \
                              is not hex, or is shorter than 8 chars, is never treated as a \
                              prefix — it is matched exactly against stored thread labels \
                              (e.g. pre-v1 non-UUID labels), and no match yields an empty list \
                              rather than an error. For all-hex >=8-char input, a stored label \
                              byte-equal to the input takes precedence over any UUID-prefix \
                              match; a label differing only in ASCII case is a fallback, \
                              consulted only when no UUID-prefix candidate resolves.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "direction",
                param_type: "string",
                required: false,
                description: "Filter messages by direction (kind=\"message\" only): \"inbound\" | \"outbound\".",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "from",
                param_type: "string",
                required: false,
                description: "Filter messages by sender identifier (kind=\"message\" only).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "to",
                param_type: "string",
                required: false,
                description: "Filter messages by recipient identifier (kind=\"message\" only).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "read",
                param_type: "bool",
                required: false,
                description: "Filter messages by read status (kind=\"message\" only): true = read, false = unread.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "delivered",
                param_type: "bool",
                required: false,
                description: "Filter messages by delivery status (kind=\"message\" only): true = delivered, false = undelivered (missing or null delivered_at).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // Assertive: returns aggregate substrate counts (#280)
    HandlerDef {
        name: "stats",
        description: "Return aggregate KG substrate counts (entities, edges, notes), plus an \
                      edges_by_relation breakdown (relation name -> count) so full-graph audits \
                      know the true per-relation population before sampling.",
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
                description: "Complete UUID or globally unique 8+ hex prefix of the entity, note, \
                              or edge to patch. Entity-name fallback uses the primary namespace.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "kind",
                param_type: "string",
                required: false,
                description: "Substrate hint (entity | note | edge). Omit to resolve substrate from UUID.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "name",
                param_type: "string",
                required: false,
                description: "New name (entities and notes).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "description",
                param_type: "string",
                required: false,
                description: "New description (entities only; notes use 'content' for body text).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "content",
                param_type: "string",
                required: false,
                description: "New body text (notes only).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "salience",
                param_type: "number",
                required: false,
                description: "Importance score 0.0–1.0 (notes only; affects recall ranking).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "decay_factor",
                param_type: "number",
                required: false,
                description: "Decay rate >= 0 (notes only; higher = faster decay).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "relation",
                param_type: "string",
                required: false,
                description: "New edge relation (edges only; any of the 17 canonical relations).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "weight",
                param_type: "number",
                required: false,
                description: "New edge weight 0.0–1.0 (edges only; 1.0=definitional, 0.7-0.9=strong, 0.4-0.6=plausible).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "properties",
                param_type: "object",
                required: false,
                description: "Properties to merge in (shallow merge).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "tags",
                param_type: "array of string",
                required: false,
                description: "Replace tag list.",
                resolution_mode: IdResolutionMode::NotApplicable,
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
                description: "Complete UUID or globally unique 8+ hex prefix of the record to \
                              delete. Entity-name fallback uses the primary namespace.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "kind",
                param_type: "string",
                required: false,
                description: "Substrate hint (entity | note | edge). Omit to resolve substrate from UUID.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "hard",
                param_type: "bool",
                required: false,
                description: "If true, permanently remove with edge cascade (default false = soft delete).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // Declaration: declares two records identical
    HandlerDef {
        name: "merge",
        description: "Deduplicate two entities or notes. Entity merges that fail the cheap entity_kind, name_similarity, or project_compatibility guard return a structured conflict error naming the guard in details.guard. force=true bypasses those guards and means the caller accepts responsibility. Successful non-dry-run merges emit an entity_merged or note_merged audit event. Natural-key edge collisions return and audit complete edge_conflict_preimages, including annotations cascaded with the dropped edge. Returns {kept_id, removed_id, edges_rewired, edges_contract_skipped, edge_conflict_preimages, properties_merged, tags_unioned, content_appended, dry_run}; \
                       chain with $prev.kept_id (not $prev.id — merge does not return a top-level id field).",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "into_id",
                param_type: "uuid",
                required: true,
                description: "Complete UUID or globally unique 8+ hex prefix of the entity or \
                              note that survives. Entity-name fallback uses the primary namespace.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "from_id",
                param_type: "uuid",
                required: true,
                description: "Complete UUID or globally unique 8+ hex prefix of the entity or \
                              note to merge from. Entity-name fallback uses the primary namespace.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "kind",
                param_type: "string",
                required: false,
                description: "Optional substrate or granular kind hint. Omit to resolve the substrate from into_id.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "strategy",
                param_type: "string",
                required: false,
                description: "Field merge policy: prefer_into (default) | prefer_from | union.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "content_strategy",
                param_type: "string",
                required: false,
                description: "Description/content policy: append (default) | prefer_into | prefer_from.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "dry_run",
                param_type: "bool",
                required: false,
                description: "If true, return the planned summary without mutating records or emitting an event.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "force",
                param_type: "bool",
                required: false,
                description: "If true, bypass entity merge safety guards; the caller accepts responsibility for the merge.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "reason",
                param_type: "string",
                required: false,
                description: "Optional caller-supplied reason preserved verbatim in the merge audit event.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // Assertive: retrieves and presents search results
    HandlerDef {
        name: "search",
        description: "Hybrid FTS + vector search over knowledge-graph entities and notes. Corpora owned by other packs (for example teaching or document corpora with their own search verbs) are disjoint and are not searched here.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "kind",
                param_type: "string",
                required: true,
                description: "Substrate or granular kind to search.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "query",
                param_type: "string",
                required: true,
                description: "Free-text search query.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Maximum results to return (default 10).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "entity_kind",
                param_type: "string",
                required: false,
                description: "Filter search results to a specific entity kind (kind=\"entity\" only).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "entity_type",
                param_type: "string",
                required: false,
                description: "Filter search results by entity type field (kind=\"entity\" only, e.g. \"paper\", \"algorithm\").",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "note_kind",
                param_type: "string",
                required: false,
                description: "Filter search results to a specific note kind (kind=\"note\" only).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "include_superseded",
                param_type: "bool",
                required: false,
                description: "When true, include notes that are targeted by a supersedes edge (kind=\"note\" only). Default false — superseded notes are excluded from results.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "properties",
                param_type: "object",
                required: false,
                description: "Filter to records whose properties contain all listed key=value pairs (kind=\"entity\" or kind=\"note\"). Predicates are applied BEFORE result truncation inside a bounded candidate window (entity tags: SQL-level; entity/note properties: Rust-level in the alive-set loop). For notes, properties are stored in the note's `properties` JSON object. E.g. {\"type\": \"paper\", \"domain\": \"attention\"}. Matches ranked beyond the runtime candidate budget (limit × 4 × handler_overfetch) may still be missed — use specific queries to bring matches into the top candidates.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "tags",
                param_type: "array",
                required: false,
                description: "Filter to records with any listed tag (kind=\"entity\" or kind=\"note\", OR semantics, case-insensitive). Predicates are applied BEFORE result truncation inside a bounded candidate window (entity tags: SQL-level via EntityFilter; note tags: Rust-level in the alive-set loop). For notes, tags are read from `properties[\"tags\"]` (there is no separate tag column on notes). E.g. [\"rust\", \"ml\"]. Matches ranked beyond the runtime candidate budget (limit × 4 × handler_overfetch) may still be missed — use specific queries to bring matches into the top candidates.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "min_score",
                param_type: "number",
                required: false,
                description: "Optional caller-supplied score floor (0.0–1.0). Results below this threshold are discarded. No server default is applied; RRF rank-1 scores are typically 0.013–0.033 on small corpora. Pass e.g. 0.02 to suppress near-zero noise hits.",
                resolution_mode: IdResolutionMode::NotApplicable,
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
                description: "Source node complete UUID or globally unique 8+ hex prefix. \
                              Entity-name fallback uses the primary namespace.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "target_id",
                param_type: "uuid",
                required: true,
                description: "Target node complete UUID or globally unique 8+ hex prefix. \
                              Entity-name fallback uses the primary namespace.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "relation",
                param_type: "string",
                required: true,
                description: "Edge relation (contains | part_of | instance_of | extends | variant_of | introduced_by | supersedes | derived_from | precedes | depends_on | enables | implements | competes_with | composed_with | annotates | supports | refutes). \
                    Each relation only accepts specific (source_kind -> target_kind) endpoint pairs; an out-of-allowlist pair between two otherwise-valid endpoints is rejected with InvalidInput, and a missing endpoint returns NotFound — never silently accepted. \
                    Base ADR-002 entity->entity allowlist (issue #964 — this table is a hand-maintained mirror of `base_entity_endpoint_rules()` (khive-runtime) and is guarded by a regression test on key rows; enforcement consults the shared rule data via `base_entity_rule_allows()`, not this text — `base_entity_endpoint_rules()` is just an exposed view of the same constant): \
                    contains: concept->concept, project->project, project->artifact, org->project, org->service. \
                    part_of: concept->concept, project->project, project->org. \
                    instance_of: *->concept (any source kind), service->project. \
                    extends: concept->concept. variant_of: concept->concept, artifact->artifact. \
                    introduced_by: concept->document, concept->person, concept->org, artifact->document, document->person, document->org. \
                    derived_from: artifact->dataset, artifact->document, artifact->project, artifact->artifact, document->document. \
                    precedes: document->document, dataset->dataset, artifact->artifact, service->service, project->project. \
                    depends_on: project->project, service->project, service->service, service->artifact, service->dataset, artifact->project, artifact->service, document->document. \
                    enables: concept->concept, service->concept, dataset->concept. \
                    implements: project->concept, service->concept. \
                    competes_with (symmetric): concept<->concept, project<->project, service<->service. \
                    composed_with (symmetric): concept<->concept, project<->project. \
                    supersedes: concept->concept, document->document, artifact->artifact, service->service, dataset->dataset, note->note (same-substrate only). \
                    supports / refutes: concept->concept, document->concept, dataset->concept, artifact->concept (evidence -> claim), note->note (same-substrate only). \
                    annotates: note -> {entity, note, edge, event} — the only relation permitting a note source paired with ANY target substrate (supersedes/supports/refutes also permit a note source, but only same-substrate: a note source there requires a note target too). \
                    The `kg` pack additionally allows (pack-extensible, additive-only per ADR-017): part_of/instance_of person->org, part_of/instance_of person->project, depends_on/enables/contains/part_of/precedes org->org, precedes decision-note->decision-note. \
                    Other loaded packs may add further pairs (e.g. `gtd` allows depends_on task-note->task-note; `formal` allows typed depends_on between theorem/definition/axiom/structure/instance/goal entity_types) — pack rules only ever add allowed pairs, never remove one listed here. Full pack-rule source: `KG_EDGE_RULES` in `khive-pack-kg/src/pack.rs` (ADR-017).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "weight",
                param_type: "number",
                required: false,
                description: "Edge weight 0.0–1.0 (default 1.0). 1.0=definitional, 0.7-0.9=strong, 0.4-0.6=plausible.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // Assertive: retrieves immediate graph neighbors
    HandlerDef {
        name: "neighbors",
        description: "Immediate graph neighbors; each hit includes origin_id for the queried node",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "node_id",
                param_type: "uuid",
                required: true,
                description: "Complete UUID, unique 8+ hex prefix, or entity name of the node \
                              whose neighbors to return. Prefix and name resolution search the \
                              caller's primary namespace.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "direction",
                param_type: "string",
                required: false,
                description: "Edge direction: \"outgoing\" | \"incoming\" | \"both\" (default \"both\").",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "relations",
                param_type: "array of string",
                required: false,
                description: "Filter to these relation types only.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "min_weight",
                param_type: "number",
                required: false,
                description: "Minimum edge weight for returned neighbors (0.0–1.0). Edges below this threshold are excluded.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // Assertive: retrieves multi-hop traversal results
    HandlerDef {
        name: "traverse",
        description: "Bounded multi-hop BFS traversal returning one path per distinct root. \
                      At most 100 roots, depth 10, 1,000 non-root results per root, 100,000 \
                      adjacency rows, and five seconds of storage expansion per request; \
                      over-budget calls fail without partial paths. Entity and note nodes \
                      both include name/kind; note names use the same fallback as \
                      `neighbors` when no explicit name is stored.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "roots",
                param_type: "array of uuid",
                required: true,
                description: "Starting node complete UUIDs, unique 8+ hex prefixes, or entity \
                              names (maximum 100; aliases resolving to the same UUID are \
                              de-duplicated). Prefix and name resolution search the caller's \
                              primary namespace.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "max_depth",
                param_type: "integer",
                required: false,
                description: "Maximum traversal depth (default 3, maximum 10).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "relations",
                param_type: "array of string",
                required: false,
                description: "Restrict traversal to these relation types.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "direction",
                param_type: "string",
                required: false,
                description: "out|outgoing|in|incoming|both (default both).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "min_weight",
                param_type: "number",
                required: false,
                description: "Minimum edge weight (finite, 0.0–1.0).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Maximum non-root first-visit nodes per root (default 100, maximum 1000).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "include_roots",
                param_type: "boolean",
                required: false,
                description: "Include each root as a depth-0 path node (default true; roots do not consume limit).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "include_properties",
                param_type: "boolean",
                required: false,
                description: "Include entity properties on enriched path nodes (default false).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // Assertive: entity-anchored graph context in one call (ADR-089)
    HandlerDef {
        name: "context",
        description: "Entity-anchored graph context: resolve anchors from `query` and/or \
                      `entity_ids`, expand 1-2 hops with neighbors_with_query, and assemble \
                      a budgeted, deterministically-ordered response. `direction` defaults to \
                      \"both\" here (unlike `neighbors`, which defaults to \"outgoing\"). At \
                      least one of `query`/`entity_ids` is required. One embedding inference \
                      when `query` is used; zero for a pure `entity_ids` call.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "query",
                param_type: "string",
                required: false,
                description: "Semantic anchor selection via hybrid search over entities; also \
                              contributes anchors alongside entity_ids (duplicates collapse). \
                              At least one of query/entity_ids is required.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "entity_ids",
                param_type: "array of string",
                required: false,
                description: "Explicit anchor UUIDs, short prefixes, or slugs (ADR-046 \
                              resolution). Honored in full — never clamped by `limit`. At \
                              least one of query/entity_ids is required.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "hops",
                param_type: "integer",
                required: false,
                description: "Expansion depth, clamped 0..=2 (default 1). 0 = anchors only, \
                              no neighbor expansion.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "budget",
                param_type: "integer",
                required: false,
                description: "Output budget in Unicode scalar values of compact JSON per \
                              record, clamped 256..=65536 (default 4096). Governs response \
                              size, not expansion work.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "relations",
                param_type: "array of string",
                required: false,
                description: "Edge-relation filter applied during expansion (default: all).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "direction",
                param_type: "string",
                required: false,
                description: "Edge direction during expansion: \"outgoing\" | \"incoming\" | \
                              \"both\" (default \"both\" — diverges from `neighbors`' \
                              \"outgoing\" default; see ADR-089).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Max anchors taken from the `query` search leg, clamped 1..=20 \
                              (default 5). Does not clamp explicit entity_ids.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "fanout",
                param_type: "integer",
                required: false,
                description: "Max neighbors returned per expanded node per hop, clamped \
                              1..=50 (default 10). Work bound: anchors × (fanout + fanout²).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // Assertive: retrieves pattern-matched results
    HandlerDef {
        name: "query",
        description: "GQL or SPARQL pattern matching (read-only). GQL pages are deterministically ordered; when `has_more` is true, repeat the query with SKIP set to `next_offset`. Write-shaped input (SPARQL INSERT/DELETE/LOAD/WITH…DELETE, GQL/Cypher CREATE/DELETE/DETACH DELETE/SET/MERGE) is rejected; use create, update, link, merge, delete to mutate the graph. When a traversal mixes fixed-length and variable-length chains, split it into separate query() calls.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "query",
                param_type: "string",
                required: true,
                description: "GQL or SPARQL pattern query string (read-only). GQL supports terminal `SKIP n [LIMIT m]` paging; use the returned `next_offset` as the next SKIP while `has_more` is true. SPARQL OFFSET is not supported. Write-shaped forms are rejected with an actionable error naming the mutation verbs to use instead. Mixed fixed-length plus variable-length traversals are not compiled in one call; split them into separate query() calls.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "page_size",
                param_type: "integer",
                required: false,
                description: "Maximum rows in this result page (minimum 1, default 500, \
                              clamped to the hard cap 10 000). Mutually exclusive with \
                              deprecated `limit`. Query-text LIMIT composes as the smaller bound.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Deprecated alias for `page_size`; mutually exclusive with \
                              `page_size`.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // Commissive: commits a proposal to the namespace event log
    HandlerDef {
        name: "propose",
        description: "Create an event-sourced change proposal. Returns {id, full_id, parent_id, status, proposer, title}; \
                       chain review/withdraw with $prev.id (not $prev.proposal_id), and reuse \
                       $prev.full_id as parent_id in a subsequent proposal request. \
                       Note: the changeset field contains nested objects and cannot be expressed in \
                       function-call DSL form — use JSON form instead: \
                       request(ops=\"[{\\\"tool\\\":\\\"propose\\\",\\\"args\\\":{...}}]\").",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "title",
                param_type: "string",
                required: true,
                description: "Short title for the proposal (must be non-empty).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "description",
                param_type: "string",
                required: true,
                description: "Full description explaining the proposed change (must be non-empty).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "changeset",
                param_type: "object",
                required: true,
                description: "Proposed changes. Discriminated by 'kind' field. \
                    Every identifier below must be a full UUID; short prefixes are rejected because resolution could miss, be ambiguous, or change the proposal's stable intent. \
                    Variants (all fields are structured objects, not JSON strings): \
                    add_entity — {kind: \"add_entity\", entity: {kind: <entity-kind>, name: <string>, description?: <string>, properties?: <object>, tags?: [<string>]}}; \
                    update_entity — {kind: \"update_entity\", id: <full UUID>, patch: {name?: <string>, description?: <string|null>, properties?: <object>, tags?: [<string>]}}; \
                    add_edge — {kind: \"add_edge\", source: <full UUID>, target: <full UUID>, relation: <EdgeRelation>, weight?: <float>}; \
                    add_note — {kind: \"add_note\", note: {kind: <note-kind>, content: <string>, name?: <string>, properties?: <object>}}; \
                    merge_entities — {kind: \"merge_entities\", into: <full UUID>, from: <full UUID>}; \
                    supersede_entity — {kind: \"supersede_entity\", old: <full UUID>, new: <full UUID>}; \
                    compound — {kind: \"compound\", steps: [<changeset>, ...]}.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "reviewers",
                param_type: "array<string>",
                required: false,
                description: "Actor IDs requested as reviewers. Default: empty list.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "expiry",
                param_type: "integer",
                required: false,
                description: "Expiry timestamp in microseconds since epoch. Omit for no expiry.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "parent_id",
                param_type: "uuid",
                required: false,
                description: "Full UUID of a parent proposal this supersedes or extends. A short prefix would require proposal-namespace resolution and is rejected because ancestry is an explicit stable reference.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // Declaration: approves/rejects/comments on a proposal
    HandlerDef {
        name: "review",
        description: "Approve, reject, comment, or request changes on a proposal",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "id",
                param_type: "uuid",
                required: true,
                description: "Complete UUID or unique 8+ hex prefix of the proposal to review. \
                              Prefix resolution searches open proposals in the caller's primary \
                              namespace.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "decision",
                param_type: "string",
                required: true,
                description: "Review outcome: \"approve\" | \"reject\" | \"comment\" | \"request_changes\".",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "comment",
                param_type: "string",
                required: false,
                description: "Optional reviewer comment attached to the review event.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // Commissive: rescinds an open proposal
    HandlerDef {
        name: "withdraw",
        description: "Withdraw an open proposal (proposer-only)",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "id",
                param_type: "uuid",
                required: true,
                description: "Complete UUID or unique 8+ hex prefix of the open proposal to \
                              withdraw. Prefix resolution searches open proposals in the caller's \
                              primary namespace.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "rationale",
                param_type: "string",
                required: false,
                description: "Optional reason for withdrawing the proposal.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // Assertive: deterministic natural-language reference resolution
    // (unified-verb draft ADR, Slice 1). Read-only — never mutates, never
    // executes a plan; `ask` (a later slice) is the write-planning entrance.
    HandlerDef {
        name: "resolve",
        description: "Resolve natural-language references to ids. Each ref in \
                       `refs` is resolved through, in order: (1) id-string \
                       passthrough (UUID / 8+ hex prefix) via the by-ID path; \
                       Entity ids only: note, edge, and event ids return \
                       NotFound here; use `get` for auto-detection. (2) this \
                       actor's recently-referenced ring; (3) an exact, \
                       case-sensitive entity-name match, which resolves \
                       deterministically regardless of search rank (one match \
                       -> Resolved; several identically-named entities -> \
                       Ambiguous over exactly that set); (4) hybrid search over \
                       the namespace, discarding vector hits with raw cosine similarity \
                       below 0.3 before RRF fusion. Returns one of Resolved{id,confidence} | \
                       Ambiguous{candidates} | NotFound per ref — never a silent \
                       pick among close candidates. For a non-exact ref that \
                       stays ambiguous, `candidates` is a bounded sample capped \
                       at `limit` (raise `limit` to surface deeper-ranked \
                       matches); an exact-name match is an identity and is \
                       exempt from that bound. Read-only: performs no mutation.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "refs",
                param_type: "array of string",
                required: true,
                description: "Natural-language references to resolve (e.g. \
                              \"the old record\", a UUID, a short hex prefix, \
                              or an exact entity name).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "kind",
                param_type: "string",
                required: false,
                description: "Restrict the exact-name (stage 3) and \
                              hybrid-search (stage 4) stages to an entity kind \
                              (e.g. \"concept\", \"project\"). Has no effect on \
                              the id-string or ring stages.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Max candidates in a non-exact ref's Ambiguous \
                              payload from the hybrid-search fallback; raise it \
                              to surface deeper-ranked matches. An exact-name \
                              match resolves to a single id and ignores this \
                              bound. Default 5, max 20.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // Assertive: reports the caller's own resolved identity
    HandlerDef {
        name: "whoami",
        description: "Report the caller's identity as the runtime already resolved it for \
                      this request: actor_id, actor_kind, whether the actor is the \
                      unattributed/anonymous fallback, the write namespace, and the \
                      read-visible namespace set. Never returns tokens or credentials — \
                      only labels the runtime already computed before dispatch.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    },
    // Assertive: writer-contention, edge-integrity, and WAL diagnostics (ADR-091/ADR-135)
    HandlerDef {
        name: "db_diagnostics",
        description: "Report writer-contention, graph-edge integrity, and WAL/checkpoint \
                      diagnostics for the main \
                      database: aggregate and class-specific pooled/standalone/writer-task \
                      acquisitions, finite-wait pool timeouts, swallowed best-effort audit \
                      append failures, build identity, duplicate edge-ID and list-ledger counts, \
                      ADR-091 checkpoint counters, a PASSIVE \
                      checkpoint probe, the -wal sidecar file size, and an explicitly qualified \
                      WAL-pin holder census. The \
                      checkpoint probe issues a real PRAGMA wal_checkpoint(PASSIVE), which \
                      backfills WAL frames into the main database on the happy path — that \
                      is ordinary checkpoint I/O, never a TRUNCATE escalation, and it never \
                      creates a missing database file or perturbs the reported counters.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
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
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "pack",
                param_type: "string",
                required: false,
                description: "Filter by pack name (e.g. \"kg\", \"gtd\", \"memory\", \"brain\", \"comm\", \"schedule\").",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
];

/// Render a `HandlerDef`'s params as a one-line call shape a caller can copy
/// and fill in, e.g. `search(kind, query, limit?, entity_kind?, ...)`.
///
/// Required params are listed bare; optional params carry a trailing `?`.
/// This is deliberately compact (names only, no types/descriptions) — the
/// full schema is still available per-verb via `help=true`; `verbs()` is a
/// catalog, not a `help=true` dump for every row.
fn compact_signature(handler: &HandlerDef) -> String {
    let params = handler
        .params
        .iter()
        .map(|p| {
            if p.required {
                p.name.to_string()
            } else {
                format!("{}?", p.name)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}({params})", handler.name)
}

/// Handle the `verbs` introspection verb — returns all public verbs, with optional category/pack filters.
pub(crate) fn handle_verbs(params: Value, registry: &VerbRegistry) -> Result<Value, RuntimeError> {
    #[derive(serde::Deserialize, Default)]
    struct VerbsParams {
        category: Option<String>,
        pack: Option<String>,
    }
    let p: VerbsParams =
        serde_json::from_value(params).map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

    let all_verbs = registry.all_verbs_with_names();
    let pack_counts: serde_json::Map<String, Value> = registry
        .pack_names()
        .into_iter()
        .map(|pack_name| {
            let count = all_verbs
                .iter()
                .filter(|(owner, _)| *owner == pack_name)
                .count();
            (pack_name.to_string(), serde_json::json!(count))
        })
        .collect();
    let verbs: Vec<Value> = all_verbs
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
                "signature": compact_signature(handler),
            })
        })
        .collect();

    let total = verbs.len();
    Ok(serde_json::json!({
        "verbs": verbs,
        "total": total,
        "pack_counts": pack_counts,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_handler(name: &str) -> &'static HandlerDef {
        KG_HANDLERS
            .iter()
            .find(|h| h.name == name)
            .unwrap_or_else(|| panic!("handler {name:?} not found in KG_HANDLERS"))
    }

    /// Regression for #899: `create.entity_kind`/`list.entity_kind` help text must list
    /// every canonical `EntityKind::NAMES` entry, so a stale hand-written list fails loudly.
    #[test]
    fn entity_kind_param_descriptions_list_all_canonical_kinds() {
        for handler_name in ["create", "list"] {
            let h = find_handler(handler_name);
            let entity_kind_param = h
                .params
                .iter()
                .find(|p| p.name == "entity_kind")
                .unwrap_or_else(|| panic!("{handler_name}.entity_kind param not found"));
            for kind in crate::vocab::EntityKind::NAMES {
                assert!(
                    entity_kind_param.description.contains(kind),
                    "{handler_name}.entity_kind description missing canonical kind {kind:?}: {:?}",
                    entity_kind_param.description
                );
            }
        }
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
    fn review_params_has_required_id_and_decision() {
        let h = find_handler("review");
        assert!(!h.params.is_empty(), "review must have params");
        assert!(
            h.params.iter().any(|p| p.name == "id" && p.required),
            "review must have required id param"
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

    /// Locate the substring of `desc` documenting `relation` — either its own
    /// `"{relation}:"` clause, its `"{relation} (symmetric):"` clause, or a
    /// grouped `"a / b:"` clause (used for `supports / refutes`).
    fn relation_clause<'a>(desc: &'a str, relation: &str) -> &'a str {
        desc.split(". ")
            .find(|c| {
                c.contains(&format!("{relation}:"))
                    || c.contains(&format!("{relation} ("))
                    || c.contains(&format!("{relation} /"))
                    || c.contains(&format!("/ {relation}"))
            })
            .unwrap_or_else(|| {
                panic!("link.relation help must document a clause for relation `{relation}`")
            })
    }

    fn endpoint_kind_label(kind: &khive_types::EndpointKind) -> String {
        match kind {
            khive_types::EndpointKind::EntityOfKind(k) => (*k).to_string(),
            khive_types::EndpointKind::NoteOfKind(k) => format!("{k}-note"),
            khive_types::EndpointKind::EntityOfType { kind, .. } => (*kind).to_string(),
        }
    }

    /// Regression for #964: `link(help=true)` must surface the per-relation
    /// edge-endpoint allowlist so batch appliers can defer to the kernel's own
    /// table instead of reimplementing (and drifting from) it.
    ///
    /// Full-coverage drift tripwire (#1060): derives the
    /// expected rows from the live rule sources (`base_entity_endpoint_rules()`
    /// and `KG_EDGE_RULES`) instead of asserting a handful of substrings, so a
    /// typo'd or dropped row in an untested relation (e.g. `derived_from`,
    /// `depends_on`, the epistemic pairs) fails the test rather than shipping
    /// a stale contract.
    #[test]
    fn link_relation_param_documents_edge_endpoint_allowlist() {
        let h = find_handler("link");
        let relation_param = h
            .params
            .iter()
            .find(|p| p.name == "relation")
            .expect("link must document a relation param");
        let desc = relation_param.description;

        for (src, relation, tgt) in khive_runtime::base_entity_endpoint_rules() {
            let rel = relation.as_str();
            let clause = relation_clause(desc, rel);
            if relation.is_symmetric() {
                let a = format!("{src}<->{tgt}");
                let b = format!("{tgt}<->{src}");
                assert!(
                    clause.contains(&a) || clause.contains(&b),
                    "link.relation help missing symmetric base row {rel}: {src}<->{tgt}"
                );
            } else {
                let row = format!("{src}->{tgt}");
                assert!(
                    clause.contains(&row),
                    "link.relation help missing base row {rel}: {row}\nclause: {clause}"
                );
            }
        }

        // The three same-substrate note->note families (ADR-055/ADR-002) must
        // be documented alongside their entity->entity cases.
        for rel in ["supersedes", "supports", "refutes"] {
            let clause = relation_clause(desc, rel);
            assert!(
                clause.contains("note->note"),
                "link.relation help must document {rel}: note->note (same-substrate only)"
            );
        }

        // annotates: the only relation permitting a note source with any target.
        assert!(
            desc.contains("annotates: note ->"),
            "link.relation help must document the annotates note->* endpoint"
        );

        // kg pack's additive EDGE_RULES — every (relation, source, target)
        // TRIPLE must appear in the pack clause, not merely the endpoint pair.
        // The clause groups relations that share an endpoint pair
        // ("part_of/instance_of person->org"), so a row is verified by finding a
        // comma-delimited segment that carries BOTH the relation token AND the
        // "src->tgt" endpoint — deleting a relation from a grouped run (e.g.
        // dropping instance_of but keeping part_of) then fails the test.
        let kg_clause = desc
            .split(". ")
            .find(|c| c.contains("kg` pack additionally allows"))
            .expect("link.relation help must document the kg pack's additive EDGE_RULES clause");
        for rule in crate::pack::KG_EDGE_RULES.iter() {
            let src = endpoint_kind_label(&rule.source);
            let tgt = endpoint_kind_label(&rule.target);
            let row = format!("{src}->{tgt}");
            let rel = rule.relation.as_str();
            let matched = kg_clause
                .split(", ")
                .any(|seg| seg.contains(&row) && seg.contains(rel));
            assert!(
                matched,
                "kg pack EDGE_RULES triple missing from help: {rel} {row}\nclause: {kg_clause}"
            );
        }
    }

    #[test]
    fn withdraw_params_has_required_id_and_optional_rationale() {
        let h = find_handler("withdraw");
        assert!(!h.params.is_empty(), "withdraw must have params");
        assert!(
            h.params.iter().any(|p| p.name == "id" && p.required),
            "withdraw must have required id param"
        );
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "rationale" && !p.required),
            "withdraw must document optional rationale param"
        );
    }

    // ── update/help param-documentation regressions ──────────────────────────

    /// update.help must document `content` for notes.
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

    /// update.name must NOT say "entities only".
    #[test]
    fn update_params_name_not_entities_only() {
        let h = find_handler("update");
        let name_param = h.params.iter().find(|p| p.name == "name").unwrap();
        assert!(
            !name_param.description.contains("entities only"),
            "update.name must not claim 'entities only' — notes also have names"
        );
    }

    /// update.help must document `salience` for notes.
    #[test]
    fn update_params_documents_salience_for_notes() {
        let h = find_handler("update");
        assert!(
            h.params.iter().any(|p| p.name == "salience"),
            "update must document 'salience' param (notes only)"
        );
    }

    /// update.help must document `decay_factor` for notes.
    #[test]
    fn update_params_documents_decay_factor_for_notes() {
        let h = find_handler("update");
        assert!(
            h.params.iter().any(|p| p.name == "decay_factor"),
            "update must document 'decay_factor' param (notes only)"
        );
    }

    /// update.help must document `relation` for edges.
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

    /// update.help must document `weight` for edges.
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

    /// No handler named "thread" should exist in the KG pack (guards against accidental addition).
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
        let page_size_param = h
            .params
            .iter()
            .find(|p| p.name == "page_size")
            .expect("page_size param must be documented in query handler metadata");
        assert!(!page_size_param.required, "page_size must be optional");
        assert!(
            page_size_param.description.contains("hard cap 10 000")
                && page_size_param.description.contains("Query-text LIMIT"),
            "page_size help must document hard-cap and query-LIMIT composition"
        );
        let limit_param = h
            .params
            .iter()
            .find(|p| p.name == "limit")
            .expect("limit param must be documented in query handler metadata");
        assert!(!limit_param.required, "limit must be optional");
        assert!(
            limit_param.description.contains("Deprecated alias"),
            "legacy limit must be explicitly documented as an alias"
        );
    }

    // ── issue #160 return-shape regressions ──────────────────────────────────

    /// propose returns {id, ...}; the correct chain key is $prev.id, not $prev.proposal_id (#160).
    /// The description may mention $prev.proposal_id in a "not this" warning, which is fine.
    #[test]
    fn propose_description_documents_id_field_not_proposal_id() {
        let h = find_handler("propose");
        assert!(
            h.description.contains("Returns {id"),
            "propose description must name the 'id' return field"
        );
        assert!(
            h.description.contains("$prev.id"),
            "propose description must document chaining via $prev.id"
        );
        // The description warns callers off $prev.proposal_id by name; the critical
        // check is that $prev.id appears first as the authoritative form.
        let id_pos = h
            .description
            .find("$prev.id")
            .expect("$prev.id must appear in propose description");
        let proposal_id_pos = h.description.find("$prev.proposal_id");
        if let Some(pid_pos) = proposal_id_pos {
            // $prev.proposal_id is only acceptable when it appears AFTER $prev.id
            // (i.e., as a negative example, not as the recommended form).
            assert!(
                id_pos < pid_pos,
                "propose description must present $prev.id before $prev.proposal_id"
            );
        }
    }

    /// merge returns {kept_id, removed_id, ...}; no top-level 'id' field.
    /// Chain with $prev.kept_id, not $prev.id (#160).
    #[test]
    fn merge_description_documents_kept_id_and_removed_id_return_fields() {
        let h = find_handler("merge");
        assert!(
            h.description.contains("kept_id") && h.description.contains("removed_id"),
            "merge description must name both kept_id and removed_id return fields"
        );
        assert!(
            h.description.contains("$prev.kept_id"),
            "merge description must document chaining via $prev.kept_id"
        );
    }

    #[test]
    fn merge_metadata_documents_the_complete_wire_contract() {
        let h = find_handler("merge");
        assert!(
            h.description.contains("entities or notes"),
            "merge description must cover both supported substrates"
        );
        for required in ["into_id", "from_id"] {
            assert!(
                h.params.iter().any(|p| p.name == required && p.required),
                "merge must document required {required}"
            );
        }
        for optional in [
            "kind",
            "strategy",
            "content_strategy",
            "dry_run",
            "force",
            "reason",
        ] {
            assert!(
                h.params.iter().any(|p| p.name == optional && !p.required),
                "merge must document optional {optional}"
            );
        }
        assert!(
            h.description.contains("details.guard") && h.description.contains("force=true"),
            "merge must document structured guard refusal and the responsibility override"
        );
    }
}
