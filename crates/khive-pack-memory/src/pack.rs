//! `MemoryPack` struct, trait impls, verb handler table, and inventory registration.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, Pack, ParamDef, VerbCategory, Visibility};

use crate::ann::{new_shared, SharedAnn};
use crate::config::RecallConfig;
use crate::query_cache::QueryEmbeddingCache;

/// Pack implementation providing `memory.remember` and `memory.recall` verbs.
pub struct MemoryPack {
    pub(crate) runtime: KhiveRuntime,
    /// Active recall config.
    pub(crate) config: Mutex<RecallConfig>,
    /// Per-`(namespace, model)` warm ANN indexes.
    pub(crate) ann: SharedAnn,
    /// Bounded exact-match query embedding cache (model_name, query_text) → Vec<f32>.
    pub(crate) query_cache: QueryEmbeddingCache,
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
        Self {
            runtime,
            config: Mutex::new(RecallConfig::default()),
            ann: new_shared(),
            query_cache: QueryEmbeddingCache::with_default_capacity(),
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
}

// Illocutionary classification (Searle 1976):
//   Commissive — commits caller to a persistent change
//   Assertive  — retrieves/presents state of affairs
static MEMORY_HANDLERS: [HandlerDef; 7] = [
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
                description: "Salience weight 0.0–1.0 (default 0.5).",
            },
            ParamDef {
                name: "decay_factor",
                param_type: "number",
                required: false,
                description: "Decay rate >= 0 (default 0.01). Higher = faster decay. At 0.01 the effective half-life is ~69 days.",
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
        ],
    },
    // Assertive: retrieves memory notes via decay-aware ranking
    HandlerDef {
        name: "memory.recall",
        description: "Recall memory notes with decay-aware hybrid ranking",
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
                description: "Minimum composite score to include (default 0.0). Composite scores are always in [0,1]: relevance is normalized to [0,1] per strategy (RRF rank-1 → 1.0; Weighted scores are [0,1] natively), and all three weighted contributions sum to at most 1.0. Typical production floor: 0.3–0.7.",
            },
            ParamDef {
                name: "score_floor",
                param_type: "number",
                required: false,
                description: "Alias for min_score. Filter out hits below this composite score. Scores are always in [0,1] regardless of fusion strategy.",
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

    async fn warm(&self) {
        crate::ann::warm_existing_memory_indexes(&self.runtime, &self.ann).await;
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
            "memory.recall" => self.handle_recall(token, params, registry).await,
            "memory.recall_embed" => self.handle_recall_embed(params).await,
            "memory.recall_candidates" => self.handle_recall_candidates(token, params).await,
            "memory.recall_fuse" => self.handle_recall_fuse(token, params, registry).await,
            "memory.recall_rerank" => self.handle_recall_rerank(params).await,
            "memory.recall_score" => self.handle_recall_score(params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "memory pack does not handle verb {verb:?}"
            ))),
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
    #[tokio::test]
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
