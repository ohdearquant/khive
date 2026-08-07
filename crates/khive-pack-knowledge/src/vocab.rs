//! Static handler descriptor table for the knowledge pack (19 verbs + 1 subhandler).

use khive_types::{HandlerDef, ParamDef, VerbCategory, Visibility};

/// Pack-owned schema for persisted retrieval-evaluation summaries.
///
/// These statements stay in the Knowledge pack's schema plan rather than the
/// core migration ledger so multi-backend boot applies them to Knowledge's
/// assigned backend. Both statements are idempotent for repeated startup.
pub(crate) static KNOWLEDGE_SCHEMA_PLAN_STMTS: [&str; 2] = [
    "CREATE TABLE IF NOT EXISTS knowledge_eval_runs (\
        id              TEXT PRIMARY KEY,\
        namespace       TEXT NOT NULL,\
        run_at          INTEGER NOT NULL,\
        query_set       TEXT NOT NULL,\
        total_queries   INTEGER NOT NULL,\
        precision_at_5  REAL NOT NULL,\
        recall_at_5     REAL NOT NULL,\
        mrr             REAL NOT NULL,\
        notes           TEXT\
    )",
    "CREATE INDEX IF NOT EXISTS idx_knowledge_eval_runs_ns_run_at \
        ON knowledge_eval_runs(namespace, run_at DESC)",
];

pub(crate) static KNOWLEDGE_HANDLERS: [HandlerDef; 20] = [
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
        description: "Fetch a single atom or domain by UUID, unique short prefix, or slug",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "id",
                param_type: "string",
                required: true,
                description: "Atom/domain full UUID, unique 8+ hex UUID prefix, or slug. UUID and prefix forms are namespace-agnostic by-ID reads; slug lookup uses the caller namespace.",
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
            ParamDef {
                name: "status",
                param_type: "string | array<string>",
                required: false,
                description: "Filter atoms to this status or set of statuses (e.g. \"draft\" or [\"draft\",\"reviewed\"]). Ignored for domains.",
            },
            ParamDef {
                name: "exclude_status",
                param_type: "string",
                required: false,
                description: "Exclude atoms with this exact status. Only used when status= is not set. Ignored for domains.",
            },
        ],
    },
    HandlerDef {
        name: "knowledge.delete_atoms",
        description: "Soft-delete atoms by slug or ID",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "ids",
                param_type: "array<string>",
                required: true,
                description: "Atom slugs or UUIDs to delete",
            },
            ParamDef {
                name: "cascade",
                param_type: "boolean",
                required: false,
                description: "Deprecated no-op. Accepted for API compatibility but not yet implemented; sections are not cascade-deleted when atoms are soft-deleted.",
            },
        ],
    },
    HandlerDef {
        name: "knowledge.stats",
        description: "Corpus statistics: atom count, domain count, coverage",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    },
    HandlerDef {
        name: "knowledge.eval_retrieval",
        description: "Run a labeled, draft-inclusive atom-retrieval query set and persist aggregate quality metrics",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Commissive,
        params: &[ParamDef {
            name: "query_set",
            param_type: "string",
            required: true,
            description: "Absolute path to a TOML query set containing query and expected_slugs entries",
        }],
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
                description: "Scored items: {id, score, size, name?, content?, category?, information_gain?}. `knowledge.suggest`'s `results` feed this directly.",
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
            ParamDef {
                name: "diversity_bias",
                param_type: "number",
                required: false,
                description: "Selector diversity bias weight (default 0.0; must be finite)",
            },
            ParamDef {
                name: "epistemic_weight",
                param_type: "number",
                required: false,
                description: "Selector weight applied to each candidate's information_gain (default 0.0; must be finite)",
            },
        ],
    },
    HandlerDef {
        name: "knowledge.search",
        description: "TF-IDF ranked search over the atom teaching corpus ONLY — knowledge graph entities and notes are a disjoint corpus and are never returned here; use the kg pack's `search` verb for those. Embedding rerank applies by default when an embedder is configured. Draft and deprecated atoms are excluded by default; pass include_drafts=true to include drafts (deprecated remain excluded). Score bands: score>=0.46 reliably on-target, 0.42<=score<0.46 mixed quality, score<0.42 mostly off-target.",
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
        description: "Suggest relevant knowledge domains for a query. Draft and deprecated domain atoms are excluded by default (same quality default as knowledge.search). Each result carries {id, name, score, size} — `size` is the aggregate estimated-token cost of the domain's member atom bodies that compose expands, in the same unit as `knowledge.fold`'s `budget`, so results feed `knowledge.fold(candidates=...)` directly with no caller-side field construction. When ANN candidate retrieval is unavailable, the response sets `ann_unavailable: true` and reports `degraded.mode`: `no_match` when lexical/FTS retrieval found no candidates, `ann_candidates_degraded` when lexical/FTS candidates still received fresh embedding cosine reranking, or `lexical_only` when that fresh rerank did not run.",
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
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Exact-match read namespace override (ADR-007 Rev 6 escape hatch). When absent, compose uses the caller token's namespace. When present, atom, domain, section, KG-blend, and profile-weight reads use exactly this namespace; invalid values are rejected.",
            },
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
            ParamDef {
                name: "blend_kg",
                param_type: "boolean",
                required: false,
                description: "Blend relevant KG concept/document entities into the briefing as a supplementary \"Knowledge graph\" section (default true). Has no effect on atom_ids-only calls, which never blend.",
            },
            ParamDef {
                name: "auto_limit",
                param_type: "integer",
                required: false,
                description: "Number of domains to auto-suggest from `query` when both domain_ids and atom_ids are empty (default 5, clamped 1-20).",
            },
            ParamDef {
                name: "max_tokens",
                param_type: "integer",
                required: false,
                description: "Output token budget for the composed briefing (default 8000, clamped 500-100000; ~4 chars/token). Sections are trimmed to fit after scoring/selection.",
            },
            ParamDef {
                name: "explain",
                param_type: "boolean",
                required: false,
                description: "Include per-section score breakdowns in the response (default false).",
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
                required: false,
                description: "Concept name. When omitted, auto-derived from `description`/`content` (truncated to the last word boundary <=60 chars); at least one of name or description/content must be non-empty.",
            },
            ParamDef {
                name: "description",
                param_type: "string",
                required: false,
                description: "Optional concept description. Also accepted as `content` (alias) for UX consistency.",
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
                description: "Concept entity complete UUID or unique 8+ hex prefix. Prefix \
                              resolution searches the caller's primary namespace.",
            },
            ParamDef {
                name: "source_id",
                param_type: "uuid",
                required: true,
                description: "Source entity complete UUID or unique 8+ hex prefix; prefix \
                              resolution searches the caller's primary namespace. The source \
                              must be kind=document, kind=person, or kind=org (introduced_by \
                              edge rule).",
            },
            ParamDef {
                name: "weight",
                param_type: "float",
                required: false,
                description: "Edge weight; defaults to 1.0, clamped 0.0-1.0",
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

    /// Parameter-parity guard (cross-model tooling evaluation, "compose budget
    /// discoverability"): the top-level field names each handler's `#[derive(Deserialize)]`
    /// params struct actually accepts, hand-kept in sync with
    /// `src/knowledge/schema.rs` and `src/handlers.rs`. Every verb's entry here must be
    /// exactly the set of top-level param names advertised in `KNOWLEDGE_HANDLERS` for
    /// that verb (order-independent) -- so a field added to a params struct without a
    /// matching `ParamDef` (or a stale `ParamDef` for a removed field) fails this test
    /// instead of silently drifting from the actual accepted request shape.
    ///
    /// Nested/child structs (e.g. `AtomInput` within `upsert_atoms.atoms`,
    /// `FoldCandidate` within `fold.candidates`) are documented in prose inside the
    /// parent `ParamDef.description` rather than as separate top-level entries here.
    fn expected_params() -> &'static [(&'static str, &'static [&'static str])] {
        &[
            ("knowledge.upsert_atoms", &["atoms", "chunk_size"]),
            ("knowledge.upsert_domains", &["domains"]),
            ("knowledge.get", &["id", "include_sections"]),
            (
                "knowledge.list",
                &["type", "limit", "offset", "status", "exclude_status"],
            ),
            ("knowledge.delete_atoms", &["ids", "cascade"]),
            ("knowledge.stats", &[]),
            ("knowledge.eval_retrieval", &["query_set"]),
            (
                "knowledge.index",
                &["ids", "batch_size", "insert_only", "rebuild_ann"],
            ),
            (
                "knowledge.fold",
                &[
                    "candidates",
                    "budget",
                    "min_score",
                    "category_weights",
                    "diversity_bias",
                    "epistemic_weight",
                ],
            ),
            (
                "knowledge.search",
                &[
                    "query",
                    "type",
                    "include_drafts",
                    "status",
                    "exclude_status",
                    "role",
                    "limit",
                    "min_score",
                    "weights",
                    "decompose",
                    "decompose_threshold",
                    "intersection_bonus",
                    "rerank",
                    "rerank_alpha",
                ],
            ),
            ("knowledge.suggest", &["query", "role", "limit"]),
            (
                "knowledge.compose",
                &[
                    "namespace",
                    "domain_ids",
                    "atom_ids",
                    "query",
                    "blend_kg",
                    "auto_limit",
                    "max_tokens",
                    "explain",
                ],
            ),
            ("knowledge.edit", &["id", "sections"]),
            ("knowledge.import", &["path", "format", "chunk_strategy"]),
            (
                "knowledge.challenge",
                &["atom_id", "section_type", "content_hash", "reason"],
            ),
            (
                "knowledge.adjudicate",
                &["atom_id", "section_type", "content_hash", "resolution"],
            ),
            (
                "knowledge.learn",
                &["name", "description", "domain", "tags"],
            ),
            ("knowledge.cite", &["concept_id", "source_id", "weight"]),
            ("knowledge.topic", &["domain", "query", "limit"]),
            ("knowledge.feedback", &["section_signals", "target_id"]),
        ]
    }

    #[test]
    fn advertised_params_match_expected_deserialized_fields_for_every_verb() {
        for (verb, expected) in expected_params() {
            let h = find_handler(verb);
            let mut advertised: Vec<&str> = h.params.iter().map(|p| p.name).collect();
            let mut expected: Vec<&str> = expected.to_vec();
            advertised.sort_unstable();
            expected.sort_unstable();
            assert_eq!(
                advertised, expected,
                "{verb}: advertised params {advertised:?} do not match the params \
                 struct's deserialized fields {expected:?} -- update either the \
                 `ParamDef`s in KNOWLEDGE_HANDLERS or the `expected_params()` table \
                 in this test (whichever is stale)"
            );
        }
    }

    /// Every verb declared in `KNOWLEDGE_HANDLERS` must have a parity row above --
    /// otherwise a newly added verb could silently escape this guard.
    #[test]
    fn every_handler_has_a_parity_row() {
        let covered: std::collections::HashSet<&str> =
            expected_params().iter().map(|(name, _)| *name).collect();
        for h in KNOWLEDGE_HANDLERS.iter() {
            assert!(
                covered.contains(h.name),
                "{}: no entry in expected_params() -- add one so parameter drift \
                 on this verb cannot silently return",
                h.name
            );
        }
    }
}
