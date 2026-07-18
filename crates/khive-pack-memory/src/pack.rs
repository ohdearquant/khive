//! `MemoryPack` struct, trait impls, verb handler table, and inventory registration.
//! See `crates/khive-pack-memory/docs/api/pack-integration.md`.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    KhiveRuntime, NamespaceToken, PackSchemaPlan, RuntimeError, SchemaPlan, VerbRegistry,
};
use khive_types::{HandlerDef, Pack, ParamDef, VerbCategory, Visibility};

use khive_brain_core::BalancedRecallState;

use crate::ann::{new_shared, SharedAnn, MEMORY_SCHEMA_PLAN_STMTS};
use crate::config::RecallConfig;
use crate::query_cache::QueryEmbeddingCache;

/// Pack implementation providing `memory.remember` and `memory.recall` verbs.
pub struct MemoryPack {
    pub(crate) runtime: KhiveRuntime,
    /// Active recall config.
    pub(crate) config: Mutex<RecallConfig>,
    /// Per-`(namespace, model)` warm ANN indexes.
    pub(crate) ann: SharedAnn,
    /// Bounded exact-match query embedding cache (model_name, query_text) → `Vec<f32>`.
    pub(crate) query_cache: QueryEmbeddingCache,
    /// In-memory recall posteriors; persistence is deferred and actions rebuild state.
    pub(crate) recall_state: Mutex<BalancedRecallState>,
    /// Optional tier-one brain profile used before binding and global-prior fallbacks.
    pub(crate) brain_profile: Option<String>,
}

impl MemoryPack {
    /// Clone the current tuned recall configuration for one request.
    pub(crate) fn active_config(&self) -> RecallConfig {
        self.config.lock().unwrap().clone()
    }

    /// Create a memory pack with default recall policy, ANN state, and query cache.
    ///
    /// See `crates/khive-pack-memory/docs/api/pack-integration.md`.
    pub fn new(runtime: KhiveRuntime) -> Self {
        let brain_profile = runtime.config().brain_profile.clone();
        Self {
            runtime,
            config: Mutex::new(RecallConfig::default()),
            ann: new_shared(),
            query_cache: QueryEmbeddingCache::with_default_capacity(),
            recall_state: Mutex::new(BalancedRecallState::new(10_000)),
            brain_profile,
        }
    }

    #[cfg(test)]
    pub(crate) fn ann_for_test(&self) -> SharedAnn {
        self.ann.clone()
    }
}

impl Pack for MemoryPack {
    const NAME: &'static str = "memory";
    const NOTE_KINDS: &'static [&'static str] = &["memory"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &MEMORY_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["kg"];
    /// Pack-owned durable ANN epoch schema, applied during registry boot.
    const SCHEMA_PLAN: Option<PackSchemaPlan> = Some(PackSchemaPlan {
        pack: "memory",
        statements: &MEMORY_SCHEMA_PLAN_STMTS,
    });
}

// Illocutionary classification (Searle 1976):
//   Commissive — commits caller to a persistent change
//   Assertive  — retrieves/presents state of affairs
static MEMORY_HANDLERS: [HandlerDef; 10] = [
    // Commissive: commits a memory to the namespace
    HandlerDef {
        name: "memory.remember",
        description: "Create a memory note with salience and decay",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "content",
                param_type: "string",
                required: true,
                description: "Memory content to store.",
            },
            ParamDef {
                name: "salience",
                param_type: "number",
                required: false,
                description: "Salience weight 0.0–1.0. Default is type-differentiated: episodic=0.3, semantic=0.5.",
            },
            ParamDef {
                name: "decay_factor",
                param_type: "number",
                required: false,
                description: "Decay rate >= 0. Default is type-differentiated: episodic=0.02 (~35d half-life), semantic=0.005 (~139d half-life). Higher = faster decay.",
            },
            ParamDef {
                name: "memory_type",
                param_type: "string",
                required: false,
                description: "Memory type tag: \"episodic\" | \"semantic\" (default \"episodic\"). Other values are rejected.",
            },
            ParamDef {
                name: "source_id",
                param_type: "string",
                required: false,
                description: "UUID or 8-char short ID of the entity or note this memory annotates.",
            },
            ParamDef {
                name: "embedding_model",
                param_type: "string",
                required: false,
                description: "Model name for vector embedding (must be registered). Defaults to pack-configured model.",
            },
            ParamDef {
                name: "tags",
                param_type: "array",
                required: false,
                description: "Tag values to filter by. Matched against properties.tags on stored memories.",
            },
            ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Write namespace override. When absent: episodic memories land in the actor's namespace; semantic memories land in \"local\". When present, overrides both routing rules.",
            },
        ],
    },
    // Commissive: explicit feedback on a recalled memory — updates recall-domain posteriors
    HandlerDef {
        name: "memory.feedback",
        description: "Emit explicit feedback on a recalled entity; updates recall-domain posteriors",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "target_id",
                param_type: "string",
                required: true,
                description: "UUID of the recalled entity or memory being rated.",
            },
            ParamDef {
                name: "signal",
                param_type: "string",
                required: true,
                description: "Feedback signal: \"useful\" | \"not_useful\" | \"wrong\" | \"explicit_positive\" | \"explicit_negative\" | \"implicit_positive\" | \"implicit_negative\" | \"correction\".",
            },
        ],
    },
    // Assertive: retrieves memory notes via decay-aware ranking
    HandlerDef {
        name: "memory.recall",
        description: "Recall memory notes with decay-aware hybrid ranking. Each hit carries resolved (read-model) values: memory_type defaults to \"episodic\" when not stored, salience and decay_factor reflect the effective defaults used for ranking.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "query",
                param_type: "string",
                required: true,
                description: "Semantic recall query.",
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Maximum memories to return (default 10).",
            },
            ParamDef {
                name: "top_k",
                param_type: "integer",
                required: false,
                description: "Override result limit (max 100). Takes priority over limit.",
            },
            ParamDef {
                name: "min_score",
                param_type: "number",
                required: false,
                description: "Minimum rank_score to include (default 0.0). This filters `rank_score`, not `score`: `score` (absolute/raw relevance in each result) stays in [0,1] regardless of fusion strategy, but `rank_score` (the composite used for ranking and this filter) is the weighted relevance/salience/temporal composite — nominally [0,1] — further adjusted by ADR-104 posterior terms whenever a brain profile serves the request: a weight-reprojection component, and a per-entity term bounded to clamp(1 + 0.3 * (entity_posterior_mean - 0.5), 0.85, 1.15). So a served, positively-reinforced memory's rank_score can exceed 1.0 by up to 15%. Typical production floor: 0.3–0.7.",
            },
            ParamDef {
                name: "score_floor",
                param_type: "number",
                required: false,
                description: "Alias for min_score. Filters by `rank_score`, not `score` — see min_score for the [0,1]-plus-up-to-15%-under-ADR-104 range of rank_score when a profile serves the request. `score` (absolute/raw relevance) stays in [0,1] regardless of fusion strategy or served profile.",
            },
            ParamDef {
                name: "min_salience",
                param_type: "number",
                required: false,
                description: "Minimum salience score filter.",
            },
            ParamDef {
                name: "memory_type",
                param_type: "string",
                required: false,
                description: "Filter to this memory_type.",
            },
            ParamDef {
                name: "fusion_strategy",
                param_type: "string",
                required: false,
                description: "Fusion strategy: \"rrf\" | \"weighted\" | \"union\" | \"vector_only\" | \"keyword_only\". Weighted values come from pack config.",
            },
            ParamDef {
                name: "embedding_model",
                param_type: "string",
                required: false,
                description: "Model name for vector recall (must be registered). Defaults to pack-configured model.",
            },
            ParamDef {
                name: "include_breakdown",
                param_type: "boolean",
                required: false,
                description: "Include per-component score breakdowns in results.",
            },
            ParamDef {
                name: "entity_names",
                param_type: "array",
                required: false,
                description: "Entity names to boost in scoring. Memories mentioning these entities receive a 1.3× score multiplier.",
            },
            ParamDef {
                name: "profile_id",
                param_type: "string",
                required: false,
                description: "Serving-profile override (ADR-104 §4): short-circuits binding resolution so the named profile's state serves this request; stamped and ledgered like a resolved profile. Unknown ids error.",
            },
            ParamDef {
                name: "full_content",
                param_type: "boolean",
                required: false,
                description: "When false, content is truncated to 200 chars in results. Default true.",
            },
            ParamDef {
                name: "tags",
                param_type: "array",
                required: false,
                description: "Filter results to memories whose stored tags include at least one (any) or all (all) of these values. Matched against properties.tags.",
            },
            ParamDef {
                name: "tag_mode",
                param_type: "string",
                required: false,
                description: "Tag filter mode: \"any\" (OR, default) or \"all\" (AND). Only applies when tags is non-empty.",
            },
            ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Exact-match read-namespace override (ADR-007 Rev 6 escape hatch). When absent, reads the caller's default visible namespace set (unchanged default behavior). When present, scopes the candidate fetch to exactly this namespace; invalid values are rejected.",
            },
        ],
    },
    HandlerDef {
        name: "memory.recall_embed",
        description: "Return the embedding vector used by memory recall",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Assertive,
        params: &[ParamDef {
            name: "include_embeddings",
            param_type: "boolean",
            required: false,
            description: "When true, include full embedding vector arrays in the response. Default false — only model name and dimension metadata are returned.",
        }],
    },
    HandlerDef {
        name: "memory.recall_candidates",
        description: "Return raw memory recall candidates by retrieval source",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Assertive,
        params: &[],
    },
    HandlerDef {
        name: "memory.recall_fuse",
        description: "Return fused memory recall candidates before final scoring",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Assertive,
        params: &[],
    },
    // Rerank stage between fuse and final scoring.
    HandlerDef {
        name: "memory.recall_rerank",
        description: "Apply configured rerankers to fused candidates",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Assertive,
        params: &[],
    },
    HandlerDef {
        name: "memory.recall_score",
        description: "Score a memory recall candidate and return score breakdown",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Assertive,
        params: &[],
    },
    // Commissive: curation prune of low-salience or expired memories
    HandlerDef {
        name: "memory.prune",
        description: "Soft-delete memories below a salience threshold and/or past expires_at. Curation-layer operation per ADR-014.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "min_salience",
                param_type: "number",
                required: false,
                description: "Soft-delete memories with salience strictly below this value.",
            },
            ParamDef {
                name: "before",
                param_type: "integer",
                required: false,
                description: "Soft-delete memories expired at or before this Unix microsecond timestamp. Defaults to now. Pass 0 to skip expiry filter.",
            },
            ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Namespace to prune. Defaults to \"local\".",
            },
            ParamDef {
                name: "dry_run",
                param_type: "boolean",
                required: false,
                description: "When true, count candidates without deleting. Default false.",
            },
        ],
    },
    // Commissive: reclaim disk space freed by soft-deleted rows
    HandlerDef {
        name: "memory.vacuum",
        description: "Run SQLite VACUUM to reclaim space freed by soft-deleted rows.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[],
    },
];

// ── Inventory self-registration ───────────────────────────────────────────────

struct MemoryPackFactory;

impl khive_runtime::PackFactory for MemoryPackFactory {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(MemoryPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&MemoryPackFactory) }

#[async_trait]
impl PackRuntime for MemoryPack {
    fn name(&self) -> &str {
        <MemoryPack as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <MemoryPack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <MemoryPack as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        &MEMORY_HANDLERS
    }

    fn requires(&self) -> &'static [&'static str] {
        <MemoryPack as Pack>::REQUIRES
    }

    fn schema_plan(&self) -> SchemaPlan {
        SchemaPlan {
            pack: "memory",
            statements: &MEMORY_SCHEMA_PLAN_STMTS,
        }
    }

    async fn warm(&self) {
        crate::ann::warm_existing_memory_indexes(&self.runtime, &self.ann).await;
        fts_population_guard(&self.runtime).await;
    }

    /// Report registered models for remember's dispatch resource accounting.
    fn registered_embedding_model_names(&self) -> Vec<String> {
        self.runtime.registered_embedding_model_names()
    }

    /// Install memory-note generation bumps on this pack's own runtime.
    ///
    /// Generic KG mutation paths preserve the stale graph and schedule replacement. See
    /// `crates/khive-pack-memory/docs/api/pack-integration.md`.
    fn register_note_mutation_hook(&self, _runtime: &KhiveRuntime) {
        let runtime = self.runtime.clone();
        let ann = self.ann.clone();
        let hook: khive_runtime::NoteMutationHookFn = std::sync::Arc::new(move |kind, _id| {
            let runtime = runtime.clone();
            let ann = ann.clone();
            Box::pin(async move {
                if kind != "memory" {
                    return;
                }
                let Ok(token) = runtime.authorize(khive_runtime::Namespace::local()) else {
                    return;
                };
                for model in runtime.registered_embedding_model_names() {
                    let key = crate::ann::AnnKey::new("local", model.as_str());
                    crate::ann::bump_generation(&ann, &key).await;
                    crate::ann::ensure_ann_background(&runtime, &token, &ann, &model).await;
                }
            })
        });
        self.runtime.install_note_mutation_hook(hook);
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "memory.remember" => self.handle_remember(token, params).await,
            "memory.feedback" => self.handle_feedback(token, params, registry).await,
            "memory.recall" => {
                self.handle_recall_with_deadline(token, params, registry)
                    .await
            }
            "memory.recall_embed" => self.handle_recall_embed(params).await,
            "memory.recall_candidates" => self.handle_recall_candidates(token, params).await,
            "memory.recall_fuse" => self.handle_recall_fuse(token, params, registry).await,
            "memory.recall_rerank" => self.handle_recall_rerank(params).await,
            "memory.recall_score" => self.handle_recall_score(params).await,
            "memory.prune" => self.handle_prune(token, params).await,
            "memory.vacuum" => self.handle_vacuum(params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "memory pack does not handle verb {verb:?}"
            ))),
        }
    }
}

impl MemoryPack {
    /// Run the complete recall pipeline under its validated end-to-end deadline.
    ///
    /// Timeout returns `DeadlineExceeded` without claiming to cancel runtime-owned storage
    /// work. See `crates/khive-pack-memory/docs/api/recall-pipeline.md`.
    async fn handle_recall_with_deadline(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let budget_ms = match parse_recall_deadline_override(&params)? {
            Some(ms) => ms,
            None => recall_deadline_ms(),
        };
        let start = std::time::Instant::now();
        match tokio::time::timeout(
            std::time::Duration::from_millis(budget_ms),
            self.handle_recall(token, params, registry),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(RuntimeError::DeadlineExceeded {
                operation: "memory.recall".to_string(),
                budget_ms,
                elapsed_ms: start.elapsed().as_millis() as u64,
            }),
        }
    }
}

/// Parse an optional positive per-request deadline; null or absence means fallback.
pub(crate) fn parse_recall_deadline_override(params: &Value) -> Result<Option<u64>, RuntimeError> {
    let Some(raw) = params
        .get("config")
        .and_then(|c| c.get("recall_deadline_ms"))
    else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    match raw.as_u64() {
        Some(ms) if ms > 0 => Ok(Some(ms)),
        _ => Err(RuntimeError::InvalidInput(format!(
            "config.recall_deadline_ms must be a positive integer milliseconds value, got {raw}"
        ))),
    }
}

/// Parse the operator deadline, falling back to 30 seconds for absent or invalid input.
///
/// Operator mistakes warn instead of breaking every recall; request mistakes are errors.
pub(crate) fn parse_recall_deadline_env(raw: Option<&str>) -> u64 {
    const DEFAULT_RECALL_DEADLINE_MS: u64 = 30_000;
    let Some(raw) = raw else {
        return DEFAULT_RECALL_DEADLINE_MS;
    };
    match raw.parse::<u64>() {
        Ok(ms) if ms > 0 => ms,
        Ok(_) => {
            tracing::warn!(
                raw = %raw,
                default_ms = DEFAULT_RECALL_DEADLINE_MS,
                "KHIVE_MEMORY_RECALL_DEADLINE_MS=0 is not a valid recall deadline; \
                 falling back to the default (#889)"
            );
            DEFAULT_RECALL_DEADLINE_MS
        }
        Err(_) => {
            tracing::warn!(
                raw = %raw,
                default_ms = DEFAULT_RECALL_DEADLINE_MS,
                "KHIVE_MEMORY_RECALL_DEADLINE_MS is not a valid positive integer; \
                 falling back to the default (#889)"
            );
            DEFAULT_RECALL_DEADLINE_MS
        }
    }
}

/// Return the cached end-to-end recall deadline, defaulting to 30 seconds.
pub(crate) fn recall_deadline_ms() -> u64 {
    static DEADLINE_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *DEADLINE_MS.get_or_init(|| {
        let raw = std::env::var("KHIVE_MEMORY_RECALL_DEADLINE_MS").ok();
        let ms = parse_recall_deadline_env(raw.as_deref());
        khive_runtime::config_ledger::record_config_locked(
            "KHIVE_MEMORY_RECALL_DEADLINE_MS",
            ms.to_string(),
        );
        ms
    })
}

/// Warn when a nontrivial base table has less than half its rows represented in FTS.
/// Never fail boot for an empty, fresh, or partially migrated database.
async fn fts_population_guard(rt: &KhiveRuntime) {
    use khive_storage::types::{SqlStatement, SqlValue};

    let sql = rt.sql();

    let Ok(mut reader) = sql.reader().await else {
        tracing::warn!("fts_population_guard: could not open SQL reader — skipping check");
        return;
    };

    for (base_table, fts_table) in [("notes", "fts_notes"), ("entities", "fts_entities")] {
        let base_row = reader
            .query_row(SqlStatement {
                sql: format!("SELECT COUNT(*) AS cnt FROM {base_table} WHERE deleted_at IS NULL"),
                params: vec![],
                label: None,
            })
            .await;

        let base_count: u64 = match base_row {
            Ok(Some(r)) => match r.get("cnt") {
                Some(SqlValue::Integer(n)) => *n as u64,
                _ => 0,
            },
            _ => 0,
        };

        if base_count <= 100 {
            continue;
        }

        let fts_row = reader
            .query_row(SqlStatement {
                sql: format!("SELECT COUNT(*) AS cnt FROM {fts_table}"),
                params: vec![],
                label: None,
            })
            .await;

        let fts_count: u64 = match fts_row {
            Ok(Some(r)) => match r.get("cnt") {
                Some(SqlValue::Integer(n)) => *n as u64,
                _ => 0,
            },
            _ => 0,
        };

        if fts_count < base_count / 2 {
            tracing::warn!(
                base_table,
                fts_table,
                base_count,
                fts_count,
                "FTS table is severely under-populated relative to base rows. \
                 FTS recall will return near-nothing. This is typically caused by \
                 a V3→V4 schema migration that did not run `kkernel reindex`. \
                 Fix: run `kkernel reindex --no-knowledge` to repopulate {fts_table}."
            );
        }
    }
}

// ── MAJ-1 regression test: second recall routes through warm ANN ──────────────

#[cfg(test)]
mod ann_route_tests {
    use super::*;

    use khive_pack_kg::KgPack;
    use khive_runtime::{Namespace, RuntimeConfig, VerbRegistryBuilder};
    use serial_test::serial;

    use crate::test_support::HashVecProvider;

    /// The second recall uses the installed ANN index, measured by its route counter.
    /// See `crates/khive-pack-memory/docs/api/ann-lifecycle.md`.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn recall_second_call_uses_warm_ann_route() {
        let tmp = tempfile::Builder::new()
            .prefix("khive-memory-ann-route-")
            .tempdir_in(std::env::temp_dir())
            .expect("temp /tmp db dir");
        let db_path = tmp.path().join("khive-graph.db");

        const MODEL: &str = "ann-route-test-model";
        const DIMS: usize = 32;

        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(db_path),
            embedding_model: None,
            additional_embedding_models: vec![],
            ..RuntimeConfig::default()
        })
        .expect("runtime");
        rt.register_embedder(HashVecProvider {
            model_name: MODEL.to_owned(),
            dims: DIMS,
        });

        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns).expect("authorize local");

        // Create notes with embedding_model: None so the runtime auto-detects
        // the registered custom provider (resolve_embedding_model only handles
        // lattice aliases; custom provider names must go through the auto-detect path).
        for i in 0..32u32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("ann warm route note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create note");
        }

        let pack = MemoryPack::new(rt.clone());
        let ann = pack.ann_for_test();

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(pack);
        let registry = builder.build().expect("registry");

        // First recall: triggers synchronous ANN build on cache miss.
        // No explicit embedding_model — auto-detects ann-route-test-model.
        registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": "ann warm route note 7",
                    "limit": 10
                }),
            )
            .await
            .expect("first recall");

        ann.reset_warm_route_count();

        // Second recall: index is already loaded — must go through warm ANN.
        registry
            .dispatch(
                "memory.recall",
                serde_json::json!({
                    "query": "ann warm route note 7",
                    "limit": 10
                }),
            )
            .await
            .expect("second recall");

        assert!(
            ann.warm_route_count() > 0,
            "second recall must route through warm ANN, not exact sqlite-vec fallback"
        );
    }
}

/// Mutation-hook tests assert ANN generation staleness directly after corpus changes.
/// See `crates/khive-pack-memory/docs/recall-reliability.md`.
#[cfg(test)]
mod note_mutation_hook_tests {
    use super::*;
    use crate::ann;
    use khive_pack_kg::KgPack;
    use khive_runtime::VerbRegistryBuilder;
    use serde_json::json;
    use serial_test::serial;
    use uuid::Uuid;

    const FR1_MODEL: &str = "fr1-mutation-hook-model";

    /// Builds a registry with the production note-mutation hook and returns its ANN state.
    fn build_note_hook_registry(rt: &KhiveRuntime) -> (khive_runtime::VerbRegistry, SharedAnn) {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        let memory_pack = MemoryPack::new(rt.clone());
        let ann = memory_pack.ann_for_test();
        builder.register(memory_pack);
        let registry = builder.build().expect("registry builds");
        registry.call_register_note_mutation_hooks(rt);
        (registry, ann)
    }

    fn mutation_hook_ann_key() -> ann::AnnKey {
        ann::AnnKey::new("local", FR1_MODEL)
    }

    /// Seeds one note, warms ANN through recall, and verifies the pre-mutation state.
    async fn seed_and_warm_ann(
        rt: &KhiveRuntime,
        registry: &khive_runtime::VerbRegistry,
        ann: &SharedAnn,
        content: &str,
        salience: f64,
    ) -> Uuid {
        rt.register_embedder(Fr1FixedVecProvider {
            model_name: FR1_MODEL.to_string(),
            vector: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        });
        let id = registry
            .dispatch(
                "memory.remember",
                json!({"content": content, "salience": salience}),
            )
            .await
            .expect("seed remember")["id"]
            .as_str()
            .expect("id")
            .parse::<Uuid>()
            .expect("valid uuid");

        registry
            .dispatch(
                "memory.recall",
                json!({
                    "query": content,
                    "namespace": "local",
                    "fusion_strategy": "vector_only",
                    "embedding_model": FR1_MODEL,
                }),
            )
            .await
            .expect("warm recall");

        assert!(
            ann::is_current(ann, &mutation_hook_ann_key()).await,
            "sanity: warm-up recall must leave the ANN cache current before \
             the mutation under test"
        );
        id
    }

    #[tokio::test]
    #[serial(background_tasks)]
    async fn prune_invalidates_warm_ann_without_subsequent_remember() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let (registry, ann) = build_note_hook_registry(&rt);

        seed_and_warm_ann(&rt, &registry, &ann, "fr1 prune target content", 0.9).await;

        // `min_salience: 1.0` is strictly above the seeded note's 0.9
        // salience, so it is the one candidate. No `memory.remember` call
        // follows.
        let pruned = registry
            .dispatch(
                "memory.prune",
                json!({ "min_salience": 1.0, "namespace": "local" }),
            )
            .await
            .expect("prune");
        assert_eq!(
            pruned["pruned"], 1,
            "the seeded note must be the one candidate pruned: {pruned:?}"
        );

        assert!(
            !ann::is_current(&ann, &mutation_hook_ann_key()).await,
            "memory.prune deleting a candidate must invalidate the warm ANN \
             generation for affected models (#750)"
        );
    }

    #[tokio::test]
    #[serial(background_tasks)]
    async fn kg_update_reindex_invalidates_warm_ann_without_subsequent_remember() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let (registry, ann) = build_note_hook_registry(&rt);

        let id = seed_and_warm_ann(&rt, &registry, &ann, "fr1 update target content", 0.7).await;

        // KG's generic `update` verb — NOT `memory.remember` — changes the
        // note's content. Same call shape `khive-pack-kg/src/handlers/
        // update.rs` dispatches through `KhiveRuntime::update_note`; no
        // `kind` param needed, the UUID resolves the substrate.
        registry
            .dispatch(
                "update",
                json!({ "id": id.to_string(), "content": "entirely different rewritten content" }),
            )
            .await
            .expect("kg update on memory-kind note");

        assert!(
            !ann::is_current(&ann, &mutation_hook_ann_key()).await,
            "a KG `update` that changes a memory-kind note's content must \
             invalidate the warm ANN generation (#750)"
        );
    }

    #[tokio::test]
    #[serial(background_tasks)]
    async fn kg_delete_invalidates_warm_ann_without_subsequent_remember() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let (registry, ann) = build_note_hook_registry(&rt);

        let id = seed_and_warm_ann(&rt, &registry, &ann, "fr1 delete target content", 0.7).await;

        // KG's generic `delete` verb (soft delete by default) — NOT any
        // memory-pack verb.
        let deleted = registry
            .dispatch("delete", json!({ "id": id.to_string() }))
            .await
            .expect("kg delete on memory-kind note");
        assert_eq!(
            deleted["deleted"].as_bool(),
            Some(true),
            "delete must report success: {deleted:?}"
        );

        assert!(
            !ann::is_current(&ann, &mutation_hook_ann_key()).await,
            "a KG `delete` on a memory-kind note must invalidate the warm \
             ANN generation (#750)"
        );
    }

    /// A real merge invalidates an index warmed only after both notes were seeded.
    /// See `crates/khive-pack-memory/docs/recall-reliability.md`.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn kg_merge_invalidates_warm_ann_without_subsequent_remember() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let (registry, ann) = build_note_hook_registry(&rt);

        rt.register_embedder(Fr1FixedVecProvider {
            model_name: FR1_MODEL.to_string(),
            vector: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        });

        let into_id = registry
            .dispatch(
                "memory.remember",
                json!({"content": "fr2 merge into content", "salience": 0.7}),
            )
            .await
            .expect("seed into-note")["id"]
            .as_str()
            .expect("id")
            .parse::<Uuid>()
            .expect("valid uuid");
        let from_id = registry
            .dispatch(
                "memory.remember",
                json!({"content": "fr2 merge from content", "salience": 0.7}),
            )
            .await
            .expect("seed from-note")["id"]
            .as_str()
            .expect("id")
            .parse::<Uuid>()
            .expect("valid uuid");

        // ONE warm-up recall, after BOTH notes exist.
        registry
            .dispatch(
                "memory.recall",
                json!({
                    "query": "fr2 merge into content",
                    "namespace": "local",
                    "fusion_strategy": "vector_only",
                    "embedding_model": FR1_MODEL,
                }),
            )
            .await
            .expect("warm recall");
        // Trust freshness only after all single-flight rebuilds release the warm guard.
        ann::wait_until_warm_idle(&ann, &mutation_hook_ann_key()).await;
        assert!(
            ann::is_current(&ann, &mutation_hook_ann_key()).await,
            "sanity: warm-up recall must leave the ANN cache current before \
             the merge under test"
        );

        registry
            .dispatch(
                "merge",
                json!({
                    "into_id": into_id.to_string(),
                    "from_id": from_id.to_string(),
                    "kind": "memory",
                }),
            )
            .await
            .expect("kg merge on memory-kind notes");

        assert!(
            !ann::is_current(&ann, &mutation_hook_ann_key()).await,
            "a KG `merge` of two memory-kind notes must invalidate the warm \
             ANN generation (#750)"
        );
    }

    struct Fr1FixedVecProvider {
        model_name: String,
        vector: [f32; 8],
    }

    #[async_trait::async_trait]
    impl khive_runtime::EmbedderProvider for Fr1FixedVecProvider {
        fn name(&self) -> &str {
            &self.model_name
        }

        fn dimensions(&self) -> usize {
            8
        }

        async fn build(
            &self,
        ) -> Result<std::sync::Arc<dyn lattice_embed::EmbeddingService>, RuntimeError> {
            Ok(std::sync::Arc::new(Fr1FixedVecService {
                vector: self.vector,
            }))
        }
    }

    struct Fr1FixedVecService {
        vector: [f32; 8],
    }

    #[async_trait::async_trait]
    impl lattice_embed::EmbeddingService for Fr1FixedVecService {
        async fn embed(
            &self,
            texts: &[String],
            _model: lattice_embed::EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
            // Every text maps to the SAME fixed vector — deterministic
            // cosine=1.0 between any query and any seeded content under this
            // provider, so ANN warming/hit behavior is fully controlled by
            // this test module, not by real embedding semantics.
            Ok(texts.iter().map(|_| self.vector.to_vec()).collect())
        }

        fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "fr1-fixed-vec"
        }
    }
}
