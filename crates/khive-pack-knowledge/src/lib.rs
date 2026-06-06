//! pack-knowledge — knowledge corpus verbs for khive.
//!
//! Manages slug-keyed atoms and domain groupings (corpus tier), atom sections
//! (section tier, ADR-048), and KG concept sugar (`learn`, `cite`, `topic`).
//! Load with `KHIVE_PACKS=kg,knowledge` or `--pack knowledge`.

pub(crate) mod handlers;
pub(crate) mod knowledge;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, Pack, ParamDef, VerbCategory, Visibility};

use crate::knowledge::vamana;
use crate::knowledge::KnowledgeHandlers;

/// Knowledge corpus pack — atoms, domains, TF-IDF search, fold, import, and KG concept verbs.
pub struct KnowledgePack {
    pub(crate) runtime: KhiveRuntime,
    pub(crate) ann: vamana::SharedAnn,
}

impl Pack for KnowledgePack {
    const NAME: &'static str = "knowledge";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &KNOWLEDGE_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

static KNOWLEDGE_HANDLERS: [HandlerDef; 18] = [
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
                description: "List of atoms: {slug, name, content, description?, tags?, properties?, finalized?}",
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
        params: &[ParamDef {
            name: "id",
            param_type: "string",
            required: true,
            description: "Atom/domain UUID or slug",
        }],
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
                description: "Skip delete-then-insert (faster for fresh corpus backfill)",
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
        description: "TF-IDF ranked search over the knowledge corpus with embedding rerank (default when embedder is configured)",
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
                description: "Minimum TF-IDF score threshold (default 0.0)",
            },
            ParamDef {
                name: "weights",
                param_type: "object",
                required: false,
                description: "TF-IDF weight overrides: {w_name, w_description, w_tags, w_content, w_exact_name, w_bigram, expand_discount, coverage_alpha}",
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
        description: "Suggest relevant knowledge domains for a query",
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
    // ── section tier (ADR-048 Phase 2) ───────────────────────────────────────
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
                description: "Sections to upsert: [{section_type, content, heading?, sort_order?}]",
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
    // ── section review tier (ADR-049) ────────────────────────────────────────
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
                description: "Source entity ID; must be kind=document or kind=person (ADR-002 §introduced_by)",
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
];

impl KnowledgePack {
    /// Create a new `KnowledgePack` bound to the given runtime, initializing a shared ANN index.
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self {
            runtime,
            ann: vamana::new_shared(),
        }
    }
}

// ── ADR-027: inventory self-registration ──────────────────────────────────────

struct KnowledgePackFactory;

impl khive_runtime::PackFactory for KnowledgePackFactory {
    fn name(&self) -> &'static str {
        "knowledge"
    }

    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(KnowledgePack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&KnowledgePackFactory) }

#[async_trait]
impl PackRuntime for KnowledgePack {
    fn name(&self) -> &str {
        <KnowledgePack as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <KnowledgePack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <KnowledgePack as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        &KNOWLEDGE_HANDLERS
    }

    fn requires(&self) -> &'static [&'static str] {
        <KnowledgePack as Pack>::REQUIRES
    }

    async fn warm(&self) {
        knowledge::vamana::warm_known_snapshots(&self.runtime, &self.ann).await;
        // Pre-warm the embedding model so the first reranked knowledge.search does not
        // pay cold model-weight load in the request path (#595). Fire-and-forget so
        // pack warm never blocks daemon startup.
        if !self.runtime.default_embedder_name().is_empty() {
            let runtime = self.runtime.clone();
            tokio::spawn(async move {
                let _ = runtime.embed("__khive_knowledge_warm__").await;
            });
        }
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            // corpus tier
            "knowledge.upsert_atoms" => {
                KnowledgeHandlers::upsert_atoms(&self.runtime, token, params).await
            }
            "knowledge.upsert_domains" => {
                KnowledgeHandlers::upsert_domains(&self.runtime, token, params).await
            }
            "knowledge.get" => KnowledgeHandlers::get(&self.runtime, token, params).await,
            "knowledge.list" => KnowledgeHandlers::list(&self.runtime, token, params).await,
            "knowledge.delete_atoms" => {
                KnowledgeHandlers::delete_atoms(&self.runtime, token, params).await
            }
            "knowledge.stats" => KnowledgeHandlers::stats(&self.runtime, token, params).await,
            "knowledge.index" => {
                KnowledgeHandlers::index(&self.runtime, token, params, &self.ann).await
            }
            "knowledge.fold" => KnowledgeHandlers::fold(&self.runtime, token, params).await,
            "knowledge.search" => {
                KnowledgeHandlers::search(&self.runtime, token, params, &self.ann).await
            }
            "knowledge.suggest" => {
                KnowledgeHandlers::suggest(&self.runtime, token, params, &self.ann).await
            }
            "knowledge.compose" => KnowledgeHandlers::compose(&self.runtime, token, params).await,
            // section tier
            "knowledge.edit" => KnowledgeHandlers::edit(&self.runtime, token, params).await,
            "knowledge.import" => KnowledgeHandlers::import(&self.runtime, token, params).await,
            "knowledge.challenge" => {
                KnowledgeHandlers::challenge(&self.runtime, token, params).await
            }
            "knowledge.adjudicate" => {
                KnowledgeHandlers::adjudicate(&self.runtime, token, params).await
            }
            // concept tier
            "knowledge.learn" => self.handle_learn(token, params).await,
            "knowledge.cite" => self.handle_cite(token, params).await,
            "knowledge.topic" => self.handle_topic(token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "knowledge pack does not handle verb {verb:?}"
            ))),
        }
    }
}
