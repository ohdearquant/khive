//! Static verb descriptor table for the knowledge pack (19 verbs).

use khive_types::{HandlerDef, ParamDef, VerbCategory, Visibility};

pub(crate) static KNOWLEDGE_HANDLERS: [HandlerDef; 19] = [
    // ── corpus tier ──────────────────────────────────────────────────────────
    HandlerDef {
        name: "knowledge.upsert_atoms",
        description: "Bulk insert or update knowledge atoms by slug",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "atoms",
                param_type: "array<object>",
                required: true,
                description: "List of atoms: {slug, name, content, tags?, properties?, finalized?}",
            },
            ParamDef {
                name: "chunk_size",
                param_type: "integer",
                required: false,
                description: "Per-chunk size for client-side chunking hint (max 5000)",
            },
        ],
    },
    HandlerDef {
        name: "knowledge.upsert_domains",
        description: "Bulk insert or update domain groupings of atoms",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[ParamDef {
            name: "domains",
            param_type: "array<object>",
            required: true,
            description: "List of domains: {slug, name, description?, tags?, members?}",
        }],
    },
    HandlerDef {
        name: "knowledge.get",
        description: "Fetch a single atom or domain by UUID or slug",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "id",
                param_type: "string",
                required: true,
                description: "Atom/domain UUID or slug",
            },
            ParamDef {
                name: "include_sections",
                param_type: "boolean",
                required: false,
                description: "When true, include the atom's sections under a `sections` key. Each section object contains: id, atom_id, namespace, section_type, heading, content, content_hash, status, tokens, sort_order, created_at, updated_at. Ordered by sort_order ASC, created_at ASC, id ASC. Ignored for domains. Default: false.",
            },
        ],
    },
    HandlerDef {
        name: "knowledge.list",
        description: "Paginated listing of atoms or domains",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "type",
                param_type: "string",
                required: false,
                description: "Filter: \"atom\" | \"domain\" (default: atom)",
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Max results (default 20, max 500)",
            },
            ParamDef {
                name: "offset",
                param_type: "integer",
                required: false,
                description: "Pagination offset",
            },
        ],
    },
    HandlerDef {
        name: "knowledge.delete_atoms",
        description: "Soft-delete atoms by slug or ID",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[ParamDef {
            name: "ids",
            param_type: "array<string>",
            required: true,
            description: "Atom slugs or UUIDs to delete",
        }],
    },
    HandlerDef {
        name: "knowledge.stats",
        description: "Corpus statistics: atom count, domain count, coverage",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    },
    HandlerDef {
        name: "knowledge.index",
        description: "Backfill embeddings + FTS for atoms/domains",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "ids",
                param_type: "array<string>",
                required: false,
                description: "Atom slugs/IDs to index. Omit to index all.",
            },
            ParamDef {
                name: "batch_size",
                param_type: "integer",
                required: false,
                description: "Page size for whole-corpus indexing (default 500, max 1000)",
            },
            ParamDef {
                name: "insert_only",
                param_type: "boolean",
                required: false,
                description: "Deprecated no-op. Accepted for API compatibility but no longer drives any pre-delete behavior; SqliteVecStore::insert atomically replaces regardless of this flag.",
            },
            ParamDef {
                name: "rebuild_ann",
                param_type: "boolean",
                required: false,
                description: "Rebuild in-memory Vamana ANN index from embeddings (default false)",
            },
        ],
    },
    HandlerDef {
        name: "knowledge.fold",
        description: "Budget-constrained knapsack selection of scored candidates",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "candidates",
                param_type: "array<object>",
                required: true,
                description: "Scored items: {id, score, size, content?, category?}",
            },
            ParamDef {
                name: "budget",
                param_type: "integer",
                required: true,
                description: "Token/size budget for the selected set",
            },
            ParamDef {
                name: "min_score",
                param_type: "number",
                required: false,
                description: "Minimum score threshold (default 0.0)",
            },
            ParamDef {
                name: "category_weights",
                param_type: "object",
                required: false,
                description: "Per-category score multipliers",
            },
        ],
    },
    HandlerDef {
        name: "knowledge.search",
        description: "TF-IDF ranked search over the knowledge corpus with embedding rerank (default when embedder is configured). Draft and deprecated atoms are excluded by default; pass include_drafts=true to include drafts (deprecated remain excluded). Score bands: score>=0.46 reliably on-target, 0.42<=score<0.46 mixed quality, score<0.42 mostly off-target.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "query",
                param_type: "string",
                required: true,
                description: "Search query text",
            },
            ParamDef {
                name: "type",
                param_type: "string",
                required: false,
                description: "Filter: \"atom\" | \"domain\" (default: both)",
            },
            ParamDef {
                name: "include_drafts",
                param_type: "boolean",
                required: false,
                description: "Include draft (unfinalized) atoms in results (default false). Has no effect when status= is set explicitly.",
            },
            ParamDef {
                name: "status",
                param_type: "string",
                required: false,
                description: "Return only atoms with this exact status. Valid values: draft | reviewed | deprecated. Overrides include_drafts — when set, include_drafts has no effect.",
            },
            ParamDef {
                name: "exclude_status",
                param_type: "string",
                required: false,
                description: "Exclude atoms with this exact status. Only used when status= is not set. Valid values: draft | reviewed | deprecated.",
            },
            ParamDef {
                name: "role",
                param_type: "string",
                required: false,
                description: "Agent role hint prepended to query for scoring",
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Max results (default 10, max 100)",
            },
            ParamDef {
                name: "min_score",
                param_type: "number",
                required: false,
                description: "Minimum score threshold (default 0.0). Score bands: score>=0.46 reliable, 0.42<=score<0.46 mixed, score<0.42 mostly off-target.",
            },
            ParamDef {
                name: "weights",
                param_type: "object",
                required: false,
                description: "TF-IDF weight overrides: {w_name, w_tags, w_content, w_exact_name, w_bigram, expand_discount, coverage_alpha}",
            },
            ParamDef {
                name: "decompose",
                param_type: "boolean",
                required: false,
                description: "Enable query decomposition (default false)",
            },
            ParamDef {
                name: "decompose_threshold",
                param_type: "integer",
                required: false,
                description: "Min non-stop terms to trigger decomposition (default 4)",
            },
            ParamDef {
                name: "intersection_bonus",
                param_type: "number",
                required: false,
                description: "Score multiplier for multi-sub-query hits (default 0.25)",
            },
            ParamDef {
                name: "rerank",
                param_type: "boolean",
                required: false,
                description: "Enable embedding rerank (default true; set false to opt out; no-op if no embedder is configured)",
            },
            ParamDef {
                name: "rerank_alpha",
                param_type: "number",
                required: false,
                description: "TF-IDF vs embedding blend weight (default 0.7 = TF-IDF dominant)",
            },
        ],
    },
    HandlerDef {
        name: "knowledge.suggest",
        description: "Suggest relevant knowledge domains for a query. Draft and deprecated domain atoms are excluded by default (same quality default as knowledge.search).",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "query",
                param_type: "string",
                required: true,
                description: "Orientation query text",
            },
            ParamDef {
                name: "role",
                param_type: "string",
                required: false,
                description: "Agent role hint prepended to query for scoring",
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Max domains (default 8, max 100)",
            },
        ],
    },
    HandlerDef {
        name: "knowledge.compose",
        description: "Compose a markdown briefing from selected knowledge domains and atoms",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "domain_ids",
                param_type: "array<string>",
                required: false,
                description: "Domain UUIDs or slugs whose member atoms should be included",
            },
            ParamDef {
                name: "atom_ids",
                param_type: "array<string>",
                required: false,
                description: "Atom UUIDs or slugs to include directly",
            },
            ParamDef {
                name: "query",
                param_type: "string",
                required: true,
                description: "Query used to rerank selected atom bodies",
            },
        ],
    },
    // ── section tier ─────────────────────────────────────────────────────────
    HandlerDef {
        name: "knowledge.edit",
        description: "Upsert sections for an atom without wiping other sections",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "id",
                param_type: "string",
                required: true,
                description: "Atom UUID or slug to edit sections for",
            },
            ParamDef {
                name: "sections",
                param_type: "array<object>",
                required: true,
                description: "Sections to upsert: [{section_type, content, heading?, sort_order?}]. \
                    section_type is a closed enum — valid values: overview | core_model | boundary_conditions | formalism | operational_guidance | examples | failure_modes | expert_lens | references | other. \
                    content must be ≥80 characters.",
            },
        ],
    },
    HandlerDef {
        name: "knowledge.import",
        description: "Ingest atlas markdown file(s) as atoms with parsed sections",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "path",
                param_type: "string",
                required: true,
                description: "Filesystem path to a markdown file or directory",
            },
            ParamDef {
                name: "format",
                param_type: "string",
                required: false,
                description: "Markdown format hint; only \"atlas_md\" supported (default)",
            },
            ParamDef {
                name: "chunk_strategy",
                param_type: "string",
                required: false,
                description: "\"section\" (one section per atom, default) or \"atom\" (entire file as one atom)",
            },
        ],
    },
    // ── section review tier ───────────────────────────────────────────────────
    HandlerDef {
        name: "knowledge.challenge",
        description: "Mark a section as disputed and increment atom dispute_count",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "atom_id",
                param_type: "string",
                required: true,
                description: "Atom UUID or slug",
            },
            ParamDef {
                name: "section_type",
                param_type: "string",
                required: true,
                description: "Section type to challenge",
            },
            ParamDef {
                name: "content_hash",
                param_type: "string",
                required: false,
                description: "content_hash (from knowledge.edit) disambiguating which same-type section to challenge; required when more than one eligible section of section_type exists",
            },
            ParamDef {
                name: "reason",
                param_type: "string",
                required: false,
                description: "Optional challenge reason",
            },
        ],
    },
    HandlerDef {
        name: "knowledge.adjudicate",
        description: "Resolve a disputed section and decrement atom dispute_count",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "atom_id",
                param_type: "string",
                required: true,
                description: "Atom UUID or slug",
            },
            ParamDef {
                name: "section_type",
                param_type: "string",
                required: true,
                description: "Section type to adjudicate",
            },
            ParamDef {
                name: "content_hash",
                param_type: "string",
                required: false,
                description: "content_hash (from knowledge.edit) disambiguating which same-type disputed section to resolve; required when more than one disputed section of section_type exists",
            },
            ParamDef {
                name: "resolution",
                param_type: "string",
                required: true,
                description: "\"accept\" (mark verified) or \"reject\" (mark reviewed)",
            },
        ],
    },
    // ── concept tier (KG sugar) ───────────────────────────────────────────────
    HandlerDef {
        name: "knowledge.learn",
        description: "Register a concept entity with optional domain and tags",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "name",
                param_type: "string",
                required: true,
                description: "Concept name",
            },
            ParamDef {
                name: "description",
                param_type: "string",
                required: false,
                description: "Optional concept description",
            },
            ParamDef {
                name: "domain",
                param_type: "string",
                required: false,
                description: "Optional domain tag (folded into properties.domain)",
            },
            ParamDef {
                name: "tags",
                param_type: "array<string>",
                required: false,
                description: "Optional tag list",
            },
        ],
    },
    HandlerDef {
        name: "knowledge.cite",
        description: "Link a concept to the paper or source that introduced it",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "concept_id",
                param_type: "uuid",
                required: true,
                description: "Concept entity ID",
            },
            ParamDef {
                name: "source_id",
                param_type: "uuid",
                required: true,
                description: "Source entity ID; must be kind=document or kind=person (introduced_by edge rule)",
            },
            ParamDef {
                name: "weight",
                param_type: "float",
                required: false,
                description: "Edge weight; defaults to 1.0",
            },
        ],
    },
    HandlerDef {
        name: "knowledge.topic",
        description: "List concepts filtered by domain or free-text query",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "domain",
                param_type: "string",
                required: false,
                description: "Filter to concepts with this domain tag",
            },
            ParamDef {
                name: "query",
                param_type: "string",
                required: false,
                description: "Free-text search across concept name + description",
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Max results; defaults to 20, capped at 100",
            },
        ],
    },
    // ── feedback tier ─────────────────────────────────────────────────────────
    HandlerDef {
        name: "knowledge.feedback",
        description: "Apply per-section feedback signals to update section posterior weights",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "section_signals",
                param_type: "object",
                required: true,
                description: "Map of section_type → signal string: {\"overview\": \"useful\", \"formalism\": \"not_useful\"}. Valid signals: useful | not_useful | wrong",
            },
            ParamDef {
                name: "target_id",
                param_type: "string",
                required: false,
                description: "Optional UUID of the atom or entity being rated. When provided alongside a configured brain profile, feedback is also forwarded to brain.feedback for profile-scoped section tracking.",
            },
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn find_handler(name: &str) -> &'static HandlerDef {
        KNOWLEDGE_HANDLERS
            .iter()
            .find(|h| h.name == name)
            .unwrap_or_else(|| panic!("handler {name:?} not found in KNOWLEDGE_HANDLERS"))
    }

    /// knowledge.edit sections param must document the closed section_type enum (10 values) and
    /// the 80-character content minimum (#160).
    #[test]
    fn knowledge_edit_sections_documents_enum_and_content_minimum() {
        let h = find_handler("knowledge.edit");
        let sections = h
            .params
            .iter()
            .find(|p| p.name == "sections")
            .expect("knowledge.edit must have a sections param");
        assert!(
            sections.description.contains("80"),
            "knowledge.edit sections description must document the 80-character content minimum"
        );
        // Spot-check the first and last members of the closed enum.
        assert!(
            sections.description.contains("overview"),
            "knowledge.edit sections description must list 'overview' as a valid section_type"
        );
        assert!(
            sections.description.contains("other"),
            "knowledge.edit sections description must list 'other' as a valid section_type"
        );
        // Verify the description calls out the closed-enum nature so callers know unrecognized
        // values are rejected (not silently coerced).
        assert!(
            sections.description.contains("closed enum"),
            "knowledge.edit sections description must state that section_type is a closed enum"
        );
    }
}
