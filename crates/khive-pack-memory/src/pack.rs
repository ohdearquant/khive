//! `MemoryPack` struct, trait impls, verb handler table, and inventory registration.

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
    /// In-memory Beta-posterior state for recall-domain feedback.
    ///
    /// Updated by `on_recall_hit`, `on_recall_miss`, and `on_explicit_feedback` in
    /// `recall_feedback`. Posteriors flow into `RecallConfig` via `PackTunable`.
    ///
    /// Persistence is deferred — state is rebuilt from actions on restart.
    pub(crate) recall_state: Mutex<BalancedRecallState>,
    /// Explicit brain profile ID from config (ADR-035 §Brain profile configuration).
    ///
    /// Tier-1 of the 3-tier feedback resolution: when set, `memory.feedback` directs
    /// feedback to this profile via `brain.feedback`. When absent, tier-2
    /// (namespace-bound profile) and tier-3 (global prior) are tried in order.
    pub(crate) brain_profile: Option<String>,
}

impl MemoryPack {
    /// Return a clone of the current active `RecallConfig`.
    ///
    /// Handlers call this to pick up the latest tuned parameters.
    pub(crate) fn active_config(&self) -> RecallConfig {
        self.config.lock().unwrap().clone()
    }

    /// Create a new `MemoryPack` backed by the given runtime.
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
    /// `memory_ann_epoch` (#812 review REQUEST CHANGES MEDIUM — pack schema
    /// contract): declared here instead of created inline on the epoch
    /// bump/read path, matching every other pack-auxiliary table in this
    /// codebase (ADR-028). Applied at boot by `server.rs`/`serve.rs`.
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

    /// #750 fix-round 1: install a note-mutation hook on the runtime THIS
    /// pack owns (`self.runtime`, not the `_runtime` param — mirrors
    /// `KgPack::register_entity_type_validator`'s multi-backend rationale:
    /// in a multi-backend deployment each pack is constructed with its own
    /// per-pack runtime, and `self.runtime` is always that one).
    ///
    /// KG's `update`/`delete` verbs reach `KhiveRuntime::update_note`/
    /// `delete_note` with no dependency on `khive-pack-memory` at all. When
    /// those touch a `kind="memory"` note (content update, soft delete, or
    /// hard delete), this hook bumps the affected models' write-generation
    /// counter (#750) and fires the same background warm `memory.remember`
    /// fires on its own write path (#791) — so `ann::is_current()` correctly
    /// treats the stale-but-still-installed index as behind, `search_loaded`
    /// keeps serving it in the meantime, and the background warm eventually
    /// installs a fresh one. This hook no longer clears the cache or deletes
    /// the persisted snapshot (see `remember.rs`'s #791 rationale).
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
    /// #889: wraps `handle_recall` in a bounded end-to-end deadline so a
    /// caller under sustained concurrent-load contention gets a fast, typed
    /// error instead of hanging until an upstream client-side ceiling fires
    /// (300s observed in production incidents on 2026-07-12). This bounds the
    /// *entire* pipeline (profile resolution, FTS + vector candidate gather
    /// and its ANN-overfetch retry loop, hydration, fusion/scoring, MMR, and
    /// supersedes suppression) — distinct from (and layered on top of)
    /// `#836`'s narrower `ann_ready_timeout_ms`, which bounds only a single
    /// cold-miss ANN-build wait inside the vector leg and degrades in-band to
    /// an FTS-only result. A timeout here does not cancel any in-flight
    /// storage work still owned by the runtime; it only bounds how long this
    /// call waits for a response.
    ///
    /// The deadline is read from `params.config.recall_deadline_ms` when
    /// present (a per-request override, checked here rather than inside
    /// `handle_recall` because the wrap has to start before that function's
    /// own `deser(params)` runs) with a defensive, non-strict lookup — an
    /// absent or malformed override just falls through to the process-wide
    /// `recall_deadline_ms()` default/env value, exactly like `handle_recall`
    /// itself already falls through to `ann_ready_timeout_ms()`. This never
    /// tightens or duplicates `RecallParams`'s own validation of `params`;
    /// `handle_recall` still parses (and rejects, if malformed) the full
    /// params normally.
    async fn handle_recall_with_deadline(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let budget_ms = params
            .get("config")
            .and_then(|c| c.get("recall_deadline_ms"))
            .and_then(|v| v.as_u64())
            .unwrap_or_else(recall_deadline_ms);
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

/// #889: bounded end-to-end deadline, in milliseconds, for the entire
/// `memory.recall` pipeline. 30s sits comfortably above every latency this
/// pipeline needs under normal-to-heavy load (single-digit-to-low-hundreds
/// of milliseconds per stage even at 20-way concurrency in the #889
/// load-harness repro, `.khive/workspaces/20260712/889-recall-stall/`) while
/// staying far short of the 300s client-side ceiling the deadline exists to
/// preempt. Overridable via env for operators who need to tune it for a
/// larger corpus or heavier sustained concurrency than the repro covered.
fn recall_deadline_ms() -> u64 {
    static DEADLINE_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *DEADLINE_MS.get_or_init(|| {
        const DEFAULT_RECALL_DEADLINE_MS: u64 = 30_000;
        let ms = std::env::var("KHIVE_MEMORY_RECALL_DEADLINE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_RECALL_DEADLINE_MS);
        khive_runtime::config_ledger::record_config_locked(
            "KHIVE_MEMORY_RECALL_DEADLINE_MS",
            ms.to_string(),
        );
        ms
    })
}

/// Check that the unified FTS tables are adequately populated relative to the
/// base `notes` and `entities` tables. Called at daemon warm time (after
/// `kkernel mcp` starts) to detect the V3→V4 migration footgun where the
/// empty unified tables silently strand FTS recall at ~1% until a manual
/// `kkernel reindex` is run.
///
/// Threshold: warns when `base_count > 100 AND fts_count < base_count / 2`.
/// A legitimately fresh or empty database (base_count ≤ 100) never warns.
/// Does NOT hard-fail — boot must succeed even on empty databases.
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
    use std::sync::Arc;

    use async_trait::async_trait;
    use khive_pack_kg::KgPack;
    use khive_runtime::{EmbedderProvider, Namespace, RuntimeConfig, VerbRegistryBuilder};
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use serial_test::serial;

    // Deterministic embedding service: distinct vector per unique text via FNV hash.
    struct HashVecService {
        dims: usize,
    }

    fn fnv_to_vec(text: &str, dims: usize) -> Vec<f32> {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in text.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0001_0000_01b3);
        }
        let mut v = Vec::with_capacity(dims);
        let mut s = h;
        for _ in 0..dims {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            v.push(((s >> 33) as f32) / (0x7fff_ffff_u32 as f32) - 1.0);
        }
        v
    }

    #[async_trait]
    impl EmbeddingService for HashVecService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|t| fnv_to_vec(t, self.dims)).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "hash-vec"
        }
    }

    struct HashVecProvider {
        model_name: String,
        dims: usize,
    }

    #[async_trait]
    impl EmbedderProvider for HashVecProvider {
        fn name(&self) -> &str {
            &self.model_name
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        async fn build(&self) -> Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(HashVecService { dims: self.dims }))
        }
    }

    /// Regression: the second `memory.recall` call on a namespace with N embedded
    /// notes must route through the warm Vamana ANN index, not the O(N) sqlite-vec
    /// exact fallback.
    ///
    /// Proof of correctness: the first recall builds the ANN synchronously (via
    /// `ensure_ann_for_model` awaited at `handlers.rs:690`). After the build the
    /// `AnnState` warm-route counter is reset. The second recall hits
    /// `search_loaded` with the index already loaded and increments the counter.
    /// An assertion on the counter value is deterministic — it does not depend on
    /// wall-clock timing or tracing output.
    ///
    /// Fail-on-revert proof: reverting the awaited `ensure_ann_for_model` call back
    /// to fire-and-forget (`ensure_ann_background`) means the first recall does not
    /// build the index synchronously. The second recall races against the background
    /// task; in test execution without `tokio::time::sleep`, the task typically has
    /// not completed, so `search_loaded` returns `Ok(None)` and the counter stays 0,
    /// causing this assertion to fail.
    // `#[serial(background_tasks)]`: both recalls below return non-empty
    // results, which fire the serve-ledger append's
    // `khive_runtime::track_background_task` (`handlers/recall.rs`), driving
    // the same process-wide counter that `ann.rs`'s
    // `ensure_ann_background_registers_a_tracked_task_not_a_bare_spawn`
    // asserts on — untagged, cargo's default parallelism can race them.
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

/// #750 fix-round 1 regressions: `memory.prune`, KG `update`, and KG `delete`
/// all mutate the same memory-note corpus the warm ANN cache is built from,
/// but only `memory.remember` bumped `AnnState::generations` before this
/// round. Each test here warms the cache (proving `is_current()` is `true`
/// for the registered model), performs the mutation WITHOUT any intervening
/// `memory.remember`, and asserts `is_current()` is `false` afterward — the
/// exact mechanism `handlers/common.rs`'s recall-path cache-hit gate checks
/// before trusting an installed index.
///
/// This is a white-box, crate-internal assertion (`ann::is_current`/
/// `ann::AnnKey` are `pub(crate)`) rather than a black-box recall-result
/// check. Confirmed via a throwaway experiment (temporarily disabling the
/// note-mutation hook call sites in `curation.rs`/`operations.rs`/
/// `prune.rs`): for the delete path specifically, "the deleted note is
/// absent from recall results" passes EVEN WITH THE BUG REINTRODUCED,
/// because recall's post-search hydration filters soft-deleted notes
/// regardless of ANN cache freshness — it is not a discriminating
/// assertion. `is_current` is. The same experiment confirmed all three
/// tests below fail (as expected) with the fix removed, and pass with it
/// restored.
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

    /// Build a kg+memory registry AND wire the note-mutation hook.
    /// Production boot does this via `khive-mcp`'s `serve.rs`/`server.rs`
    /// calling `VerbRegistry::call_register_note_mutation_hooks`; a
    /// hand-built test registry must call it explicitly, same as this
    /// codebase's existing `call_register_entity_type_validators` test
    /// pattern. Also returns the `MemoryPack`'s `SharedAnn` handle (cloned
    /// before the pack is moved into the builder) so tests can assert
    /// `ann::is_current` directly.
    fn fr1_build_registry(rt: &KhiveRuntime) -> (khive_runtime::VerbRegistry, SharedAnn) {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        let memory_pack = MemoryPack::new(rt.clone());
        let ann = memory_pack.ann_for_test();
        builder.register(memory_pack);
        let registry = builder.build().expect("registry builds");
        registry.call_register_note_mutation_hooks(rt);
        (registry, ann)
    }

    fn fr1_key() -> ann::AnnKey {
        ann::AnnKey::new("local", FR1_MODEL)
    }

    /// Seed one memory note, then run a `memory.recall` to synchronously
    /// warm the ANN cache — the cache-miss fallback in `handlers/common.rs`
    /// calls `ensure_ann_for_model` and awaits it before returning, so a
    /// single recall is enough to guarantee the index is installed. Returns
    /// the seeded note's id and asserts the cache is current before the
    /// caller's mutation under test.
    async fn fr1_seed_and_warm(
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
            ann::is_current(ann, &fr1_key()).await,
            "sanity: warm-up recall must leave the ANN cache current before \
             the mutation under test"
        );
        id
    }

    #[tokio::test]
    #[serial(background_tasks)]
    async fn fr1_prune_invalidates_warm_ann_without_subsequent_remember() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let (registry, ann) = fr1_build_registry(&rt);

        fr1_seed_and_warm(&rt, &registry, &ann, "fr1 prune target content", 0.9).await;

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
            !ann::is_current(&ann, &fr1_key()).await,
            "memory.prune deleting a candidate must invalidate the warm ANN \
             generation for affected models (#750 fix-round 1)"
        );
    }

    #[tokio::test]
    #[serial(background_tasks)]
    async fn fr1_kg_update_reindex_invalidates_warm_ann_without_subsequent_remember() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let (registry, ann) = fr1_build_registry(&rt);

        let id = fr1_seed_and_warm(&rt, &registry, &ann, "fr1 update target content", 0.7).await;

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
            !ann::is_current(&ann, &fr1_key()).await,
            "a KG `update` that changes a memory-kind note's content must \
             invalidate the warm ANN generation (#750 fix-round 1)"
        );
    }

    #[tokio::test]
    #[serial(background_tasks)]
    async fn fr1_kg_delete_invalidates_warm_ann_without_subsequent_remember() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let (registry, ann) = fr1_build_registry(&rt);

        let id = fr1_seed_and_warm(&rt, &registry, &ann, "fr1 delete target content", 0.7).await;

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
            !ann::is_current(&ann, &fr1_key()).await,
            "a KG `delete` on a memory-kind note must invalidate the warm \
             ANN generation (#750 fix-round 1)"
        );
    }

    /// #750 fix-round 2 (codex r2 High 1): `merge_note`'s non-dry-run
    /// success path reindexes the surviving note (a corpus change exactly
    /// like `update_note`'s `text_changed` branch) but never fired the
    /// note-mutation hook. Same white-box shape as the three fr1 tests
    /// above.
    ///
    /// Both notes are seeded and the cache is warmed only ONCE, AFTER both
    /// exist — NOT via `fr1_seed_and_warm` for the first note followed by a
    /// second `memory.remember` for the second, because `memory.remember`'s
    /// own handler already calls `ann::bump_generation` on every create
    /// (`handlers/remember.rs`). Warming before the second note existed
    /// would make the merge assertion pass trivially off that unrelated
    /// bump — this exact non-discriminating-test trap is what caught out
    /// an earlier draft of this test (confirmed by a disable/re-enable
    /// run: the earlier draft still passed with the merge fix's hook-fire
    /// call commented out). See the fix-round-2 report for the full
    /// before/after evidence.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn fr2_kg_merge_invalidates_warm_ann_without_subsequent_remember() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let (registry, ann) = fr1_build_registry(&rt);

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
        // ANN warming is asynchronous (`ann::ensure_ann_background`'s doc
        // comment): both the seeding `memory.remember` calls above and this
        // warm-up recall itself can fire a fire-and-forget background
        // rebuild rather than complete it inline (#844). Wait for the
        // single-flight `warming` guard for this key to clear before
        // trusting `is_current` — the guard is only released once every
        // in-flight rebuild for this key has actually finished.
        ann::wait_until_warm_idle(&ann, &fr1_key()).await;
        assert!(
            ann::is_current(&ann, &fr1_key()).await,
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
            !ann::is_current(&ann, &fr1_key()).await,
            "a KG `merge` of two memory-kind notes must invalidate the warm \
             ANN generation (#750 fix-round 2, codex r2 High 1)"
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
