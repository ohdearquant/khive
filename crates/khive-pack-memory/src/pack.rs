//! `MemoryPack` struct, trait impls, verb handler table, and inventory registration.
//! See `crates/khive-pack-memory/docs/api/pack-integration.md`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    KhiveRuntime, KindHook, NamespaceToken, PackSchemaPlan, RuntimeError, VerbRegistry,
};
use khive_types::{HandlerDef, IdResolutionMode, Pack, ParamDef, VerbCategory, Visibility};

use khive_brain_core::BalancedRecallState;

use crate::ann::{new_shared_for_role, SharedAnn, MEMORY_SCHEMA_PLAN_STMTS};
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
        Self::new_with_index_role(runtime, true)
    }

    /// As [`Self::new`], but states whether this process may build the memory
    /// index from the full corpus. The serving factory passes the daemon role:
    /// a corpus build is minutes of CPU and a segment rewrite every other reader
    /// on the index root must absorb, so a short-lived client serves what is
    /// persisted and leaves the build to the daemon. Direct constructions —
    /// admin reindex, benches, tests — build, because building is what they are
    /// for.
    pub fn new_with_index_role(runtime: KhiveRuntime, builds_corpus_indexes: bool) -> Self {
        let brain_profile = runtime.config().brain_profile.clone();
        Self {
            runtime,
            config: Mutex::new(RecallConfig::default()),
            ann: new_shared_for_role(builds_corpus_indexes),
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
    const BRAIN_CONSUMER_KINDS: &'static [&'static str] =
        &[khive_brain_core::ConsumerKind::Recall.as_str()];
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
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "key",
                param_type: "string",
                required: false,
                description: "Optional idempotency key (also accepted as the legacy `key` spelling), at most 512 UTF-8 bytes and no NUL. It is scoped to the write namespace. An identical replay returns the original memory with `replayed=true`; a different content under the same key is refused and names the conflict.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "idempotency_key",
                param_type: "string",
                required: false,
                description: "Alias for `key`; namespace-scoped idempotency key. An identical replay returns the original memory with `replayed=true`, while different content under the same key is refused.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "salience",
                param_type: "number",
                required: false,
                description: "Salience weight 0.0–1.0. Default is type-differentiated: episodic=0.3, semantic=0.5.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "decay_factor",
                param_type: "number",
                required: false,
                description: "Decay rate >= 0. Default is type-differentiated: episodic=0.02 (~35d half-life), semantic=0.005 (~139d half-life). Higher = faster decay.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "memory_type",
                param_type: "string",
                required: false,
                description: "Memory type tag: \"episodic\" | \"semantic\" (default \"episodic\"). Other values are rejected.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "source_id",
                param_type: "string",
                required: false,
                description: "UUID or 8-char short ID of the entity or note this memory annotates.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "embedding_model",
                param_type: "string",
                required: false,
                description: "Model name for vector embedding (must be registered). Defaults to pack-configured model.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "tags",
                param_type: "array",
                required: false,
                description: "Tag values to filter by. Matched against properties.tags on stored memories.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "properties",
                param_type: "object",
                required: false,
                description: "Structured fields stored beside memory_type and tags and returned on recall hits. Keep the memory text to the fact and put who, when, where, kind and source ids here. Rules: a JSON object; at most 32 keys; each key 1-64 chars, a lowercase ASCII letter first, then lowercase letters, digits or _; the keys memory_type, tags and attribution are reserved; each value is a string, a finite number, a boolean or an array of strings (null and nested objects are refused); the serialized object is at most 8192 bytes, and an object over the limit is refused, never truncated.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Write namespace override. When absent, both episodic and semantic memories land in the caller's actor namespace; an anonymous caller (no actor configured) writes to \"local\" and the memory is flagged with attribution \"unresolved\". When present, the explicit namespace wins, \"local\" included.",
                resolution_mode: IdResolutionMode::NotApplicable,
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
                name: "id",
                param_type: "uuid",
                required: true,
                description: "Full UUID of the recalled entity or memory being rated. A short-prefix resolution can miss or be ambiguous and is rejected because feedback must identify one exact recalled record. `target_id` remains accepted as an alias for `id`.",
                resolution_mode: IdResolutionMode::UnscopedFullUuidOnly,
            },
            ParamDef {
                name: "signal",
                param_type: "string",
                required: true,
                description: "Feedback signal: \"useful\" | \"not_useful\" | \"wrong\" | \"explicit_positive\" | \"explicit_negative\" | \"implicit_positive\" | \"implicit_negative\" | \"correction\".",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // Assertive: retrieves memory notes via decay-aware ranking
    HandlerDef {
        name: "memory.recall",
        description: "Recall memory notes with decay-aware hybrid ranking. Each hit carries resolved (read-model) values: memory_type defaults to \"episodic\" when not stored, salience and decay_factor reflect the effective defaults used for ranking. Default responses are arrays; budget-capped hits carry truncated: true per result. When the budget removes every ranked candidate, the response is {results: [], truncated: true} so the cutoff stays distinguishable from a genuine no-match. Degraded serving is a third state and never changes ok: it stays true, and the degradation is reported in-band. A non-empty degraded response keeps the array shape and stamps each hit with degraded: \"ann_unavailable\" plus a degraded_reason naming the failure site; when degradation leaves no hits the response is {results: [], degraded: true, degraded_reason} for the same reason the capped-empty response changes shape. A caller that reads only ok cannot tell a degraded serve from a healthy one, so read degraded. One more shape supersedes all of these: a verbose request whose vector recall ran against more than one model returns {results, candidates.vector_candidates_per_model, budget_capped, truncated_for_budget}, which reports the budget as a COUNT rather than a flag and carries no top-level degraded marker, though the per-hit stamps are still on the results. include_timings=true adds a timings field to whichever shape applies, wrapping an array as {results, timings}.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "query",
                param_type: "string",
                required: true,
                description: "Semantic recall query.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Maximum memories to return (default 10, max 100); 0 returns no hits.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "top_k",
                param_type: "integer",
                required: false,
                description: "Override result limit (max 100); 0 returns no hits. Takes priority over limit.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "min_score",
                param_type: "number",
                required: false,
                description: "Minimum rank_score to include (default 0.0). This filters `rank_score`, not `score`: `score` (absolute/raw relevance in each result) stays in [0,1] regardless of fusion strategy, but `rank_score` (the composite used for ranking and this filter) is the weighted relevance/salience/temporal composite — nominally [0,1] — further adjusted by posterior terms whenever a brain profile serves the request: a weight-reprojection component, and a per-entity term bounded to clamp(1 + 0.3 * (entity_posterior_mean - 0.5), 0.85, 1.15). So a served, positively-reinforced memory's rank_score can exceed 1.0 by up to 15%. Typical production floor: 0.3–0.7.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "score_floor",
                param_type: "number",
                required: false,
                description: "Alias for min_score. Filters by `rank_score`, not `score` — see min_score for the [0,1]-plus-up-to-15% range of rank_score when a profile serves the request. `score` (absolute/raw relevance) stays in [0,1] regardless of fusion strategy or served profile.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "min_salience",
                param_type: "number",
                required: false,
                description: "Minimum salience score filter.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "memory_type",
                param_type: "string",
                required: false,
                description: "Filter to this memory_type.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "created_after",
                param_type: "string",
                required: false,
                description: "Only memories created at or after this instant (inclusive). RFC 3339 with offset, e.g. \"2026-08-20T00:00:00-04:00\" or a \"Z\" form; date-only strings are rejected. With created_before this is a half-open window [after, before) — the brain.event_counts since/until convention.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "created_before",
                param_type: "string",
                required: false,
                description: "Only memories created strictly before this instant (exclusive). Same RFC 3339-with-offset format as created_after. An empty window (created_after >= created_before) is an error.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "fusion_strategy",
                param_type: "string",
                required: false,
                description: "Fusion strategy: \"rrf\" | \"weighted\" | \"union\" | \"vector_only\" | \"keyword_only\". Weighted values and the RRF constant come from pack config. keyword_only runs only the text search (no query embedding, no vector search, so it never reports degraded; under a session fence it still waits for the fence's ANN watermark); vector_only runs only the vector search.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "embedding_model",
                param_type: "string",
                required: false,
                description: "Model name for vector recall (must be registered). Defaults to pack-configured model.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "consistency",
                param_type: "string",
                required: false,
                description: "Recall consistency: eventual (default) or session. Session requires a visibility_token and proves each requested model fence in its candidate-producing read.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "visibility_token",
                param_type: "string",
                required: false,
                description: concat!(
                    "Opaque sealed visibility receipt returned by memory.remember; ",
                    "required when consistency is session. ",
                    "Clear version-1 receipts are refused."
                ),
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "timeout_ms",
                param_type: "integer",
                required: false,
                description: "Session proof wait in milliseconds, 0 by default and at most 10000; also bounded by the request deadline.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "include_breakdown",
                param_type: "boolean",
                required: false,
                description: "Include per-component score breakdowns in results.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "include_timings",
                param_type: "boolean",
                required: false,
                description: "When true the response carries timings: total_us, the retrieval stage wall times embed_us, fts_us, ann_us, fresh_tail_us and hydrate_us (null when the stage did not run; FTS and the vector search run concurrently, so they do not add up), retrieval_rounds, and rounds[] with each round's candidate_limit, text_hits, vector_hits, hydrated and eligible counts and stage times. An array response becomes {results, timings}; an object response gains timings. Default false.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "entity_names",
                param_type: "array",
                required: false,
                description: "Entity names to boost in scoring. Memories mentioning these entities receive a 1.3× score multiplier.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "profile_id",
                param_type: "string",
                required: false,
                // MAINTENANCE, deliberately kept out of the description: ADR-104 §4.
                description: "Serving-profile override: short-circuits binding resolution so the named profile's state serves this request; stamped and ledgered like a resolved profile. Unknown ids error.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "full_content",
                param_type: "boolean",
                required: false,
                description: "When false, content is truncated to 200 chars in results. Default true.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "tags",
                param_type: "array",
                required: false,
                description: "Filter results to memories whose stored tags include at least one (any) or all (all) of these values. Matched against properties.tags.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "tag_mode",
                param_type: "string",
                required: false,
                description: "Tag filter mode: \"any\" (OR, default) or \"all\" (AND). Only applies when tags is non-empty.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "exclude_tags",
                param_type: "array",
                required: false,
                description: "Drop memories whose stored tags include any of these values. Applied after tags/tag_mode and before ranking and limit, so a run can recall everything except its own writes.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "include_source_id",
                param_type: "boolean",
                required: false,
                description: "When true every hit carries source_id: the UUID the memory annotates (its source_id at remember time), or null when it has none. Default false.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                // MAINTENANCE, deliberately kept out of the description: this is the
                // ADR-007 Rev 6 escape hatch.
                description: "Exact-match read-namespace override. When absent, reads the caller's default visible namespace set (unchanged default behavior). When present, scopes the candidate fetch to exactly this namespace; invalid values are rejected.",
                resolution_mode: IdResolutionMode::NotApplicable,
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
            resolution_mode: IdResolutionMode::NotApplicable,
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
        // MAINTENANCE, deliberately kept out of the description: ADR-014 defines the
        // curation layer this verb belongs to.
        description: "Soft-delete memories below a salience threshold and/or past expires_at. A curation-layer operation.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "min_salience",
                param_type: "number",
                required: false,
                description: "Soft-delete memories with salience strictly below this value.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "min_effective_salience",
                param_type: "number",
                required: false,
                description: "Soft-delete memories whose decay-adjusted salience is strictly below this value, using the active recall decay configuration. Omit to disable this selector; criteria are combined with OR.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "before",
                param_type: "integer",
                required: false,
                description: "Soft-delete memories expired at or before this Unix microsecond timestamp. Defaults to now. Pass 0 to skip expiry filter.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "namespace",
                param_type: "string",
                required: false,
                description: "Namespace to prune. Defaults to \"local\".",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "dry_run",
                param_type: "boolean",
                required: false,
                description: "When true, count candidates without deleting. Default false.",
                resolution_mode: IdResolutionMode::NotApplicable,
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
    khive_runtime::pack_factory_metadata!(MemoryPack);

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(MemoryPack::new_with_index_role(
            runtime,
            khive_runtime::daemon::is_warm_index_host(),
        ))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&MemoryPackFactory) }

#[async_trait]
impl PackRuntime for MemoryPack {
    khive_runtime::pack_runtime_metadata!();

    fn validate_config(&self) -> Result<(), RuntimeError> {
        if let Some(notice) = self.runtime.visibility_receipt_custody_notice() {
            tracing::warn!(target: "khive.boot", "{notice}");
        }
        Ok(())
    }

    fn kind_hook(&self, kind: &str) -> Option<Arc<dyn KindHook>> {
        match kind {
            "memory" => Some(Arc::new(crate::hook::MemoryHook)),
            _ => None,
        }
    }

    async fn warm(&self) {
        // ANN warm is not a read-only operation: a missing consumer is first
        // registered as pending, and a cold/stale segment can checkpoint and
        // compact. Snapshot inspection must not enter that lifecycle. The FTS
        // population check below is genuinely load-only and remains useful.
        if !self.runtime.is_read_only() {
            crate::ann::warm_existing_memory_indexes(&self.runtime, &self.ann).await;
            crate::ann::start_rotation_watcher(&self.runtime, &self.ann);
        }
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
        let runtime = self.runtime.detached_for_note_search_ann_provider();
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
                    let key = crate::ann::AnnKey::new(model.as_str());
                    crate::ann::bump_generation(&ann, &key).await;
                    crate::ann::ensure_ann_background(&runtime, &token, &ann, &model).await;
                }
            })
        });
        self.runtime.install_note_mutation_hook(hook);
    }

    fn register_note_search_ann_provider(&self, runtime: &KhiveRuntime) {
        if self.runtime.backend_id() != runtime.backend_id()
            || !std::ptr::eq(self.runtime.backend(), runtime.backend())
        {
            return;
        }
        crate::ann::enable_note_search_consumer(&self.ann);
        runtime.install_note_search_ann_provider(Arc::new(
            crate::note_search::MemoryNoteSearchAnnProvider::new(
                self.runtime.clone(),
                self.ann.clone(),
            ),
        ));
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
            "memory.recall_rerank" => self.handle_recall_rerank(token, params).await,
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
    pub(crate) async fn handle_recall_with_deadline(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let budget_ms = match parse_recall_deadline_override(&params)? {
            Some(ms) => ms,
            None => recall_deadline_ms(&self.runtime),
        };
        let start = std::time::Instant::now();
        // `handle_recall` is a large state machine: its text/vector fan-out
        // contains several cancellation-aware select/join branches. Keep that
        // generator (inside its profiling wrapper `handle_recall_profiled`)
        // behind one heap pointer at the deadline boundary so it is
        // not embedded again in this future, `PackRuntime::dispatch`, and the
        // MCP request stack. Inlining it here can overflow Tokio's worker stack
        // even though the pipeline has no recursive call cycle.
        let hard_deadline = start.checked_add(std::time::Duration::from_millis(budget_ms));
        let recall = Box::pin(self.handle_recall_profiled(token, params, registry, hard_deadline));
        match tokio::time::timeout(std::time::Duration::from_millis(budget_ms), recall).await {
            Ok(result) => result,
            Err(_) => {
                let elapsed_ms = start.elapsed().as_millis() as u64;
                // #30/#889: the deadline-exceeded case is exactly the incident this
                // issue reports (a recall that never returns, no daemon-side trace)
                // -- the request never reaches recall.rs's own slow-request WARN
                // because `handle_recall`'s future is dropped, not completed. Log
                // it here, at the one call site every recall passes through.
                tracing::warn!(
                    operation = "memory.recall",
                    budget_ms,
                    elapsed_ms,
                    "memory.recall exceeded its deadline and was abandoned"
                );
                Err(RuntimeError::DeadlineExceeded {
                    operation: "memory.recall".to_string(),
                    budget_ms,
                    elapsed_ms,
                })
            }
        }
    }
}

#[cfg(test)]
#[path = "pack/recall_future_footprint_tests.rs"]
mod recall_future_footprint_tests;

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
pub(crate) fn recall_deadline_ms(runtime: &khive_runtime::KhiveRuntime) -> u64 {
    static DEADLINE_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let ms = *DEADLINE_MS.get_or_init(|| {
        let raw = std::env::var("KHIVE_MEMORY_RECALL_DEADLINE_MS").ok();
        parse_recall_deadline_env(raw.as_deref())
    });
    runtime.record_config_locked("KHIVE_MEMORY_RECALL_DEADLINE_MS", || ms.to_string());
    ms
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

#[cfg(test)]
mod receipt_custody_tests;

#[cfg(test)]
#[path = "pack/read_only_warm_tests.rs"]
mod read_only_warm_tests;

// ── A second recall routes through warm ANN ─────────────────────────────────

#[cfg(test)]
#[path = "pack/ann_route_tests.rs"]
mod ann_route_tests;

/// Pack-level lifecycle coverage for `start_rotation_watcher`'s production
/// wiring (issue #2340): the rotation unit tests call the private
/// `refresh_rotated_segments_once` test helper directly, so nothing
/// previously asserted that a writable `warm()` actually starts the tracked
/// watcher, that a repeated `warm()` is idempotent, or that the watcher
/// exits once its `AnnState` is dropped. Time is paused so the watcher's
/// 5-second tick can be crossed deterministically instead of by a real
/// sleep. The assertions read the watcher's own state (the one-shot started
/// flag and the task handle retained for this `AnnState`) rather than the
/// process-wide `background_task_count()` counter, which any test that
/// dispatches a memory verb can move. `#[serial(background_tasks)]` stays on
/// the test because the watcher is a tracked task: while it runs it moves
/// that counter for the tests in this crate that read it under the same key.
#[cfg(test)]
#[path = "pack/rotation_watcher_lifecycle_tests.rs"]
mod rotation_watcher_lifecycle_tests;

/// Mutation-hook tests assert ANN generation staleness directly after corpus changes.
/// See `crates/khive-pack-memory/docs/recall-reliability.md`.
#[cfg(test)]
#[path = "pack/note_mutation_hook_tests.rs"]
mod note_mutation_hook_tests;
