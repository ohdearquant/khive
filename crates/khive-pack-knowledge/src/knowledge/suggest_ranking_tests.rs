//! Regression fixture for issue #90 — cross-domain word-sense collisions
//! outranking the on-field domain in `knowledge.suggest`.
//!
//! Root cause (verified against source, see search.rs + util.rs comments):
//! `suggest` blends a lexical/RRF-fused score (dominated by TF-IDF matches on
//! the domain NAME, weighted `D_W_EXACT_NAME=5.0`/`D_W_NAME=3.0`) with a
//! fresh embedding cosine, via `score = alpha*norm_tfidf + (1-alpha)*cos`.
//! The old `alpha=0.7` let a domain whose TITLE happens to contain a rare,
//! polysemous query token (e.g. "decoding") win on lexical weight alone, even
//! when a genuinely good semantic signal (cosine) correctly favors the
//! on-topic domain. The fix lowers the blend weight to `D_SUGGEST_RERANK_ALPHA
//! = 0.3` so the semantic leg — the only leg capable of sense disambiguation —
//! dominates the final rank.
//!
//! This fixture uses a controlled fake embedder (topic vectors keyed on a
//! unique marker token per domain, NOT derived from surface tokens) so the
//! test isolates the blend-weight mechanism from real embedding-model
//! quality. Five query/domain-set pairs across different fields: the
//! production repro from #90 plus four constructed pairs, each following the
//! same shape (collision domain shares a literal rare token with the query in
//! its title; correct domain is semantically on-topic per the controlled
//! embedding but shares fewer/no literal title tokens).

use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_runtime::{
    AllowAllGate, BackendId, EmbedderProvider, KhiveRuntime, Namespace, NamespaceToken,
    RuntimeConfig, VerbRegistry, VerbRegistryBuilder,
};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::json;
use std::sync::Arc;

use crate::knowledge::{vamana, KnowledgeHandlers};

const MODEL_KEY: &str = "all-minilm-l6-v2";
const DIM: usize = 8;

/// Context-keyword bags per topic — a controlled stand-in for a real
/// embedding model's ability to use SURROUNDING CONTEXT to disambiguate a
/// shared polysemous token. Deliberately excludes the collision token itself
/// (e.g. "decoding") from every bucket, since that lone token is exactly what
/// a bag-of-words TF-IDF leg over-weights; the buckets instead capture the
/// multi-word context a real query naturally uses, so the query embeds close
/// to its on-topic domain and far from the lexical-collision domain
/// independent of the literal shared word.
const TOPIC_KEYWORDS: &[&[&str]] = &[
    &[
        "inference",
        "language model",
        "acceleration",
        "serving",
        "latency",
        "throughput",
    ],
    &[
        "channel",
        "communication theoretic",
        "error budget",
        "bandwidth",
        "signal processing",
    ],
    &[
        "contract formation",
        "bargained-for",
        "enforceable",
        "bilateral",
        "consideration requirement",
    ],
    &[
        "empathy",
        "interpersonal",
        "workplace communication",
        "thoughtfulness",
        "regard for others",
    ],
    &[
        "polymerase",
        "promoter",
        "gene expression",
        "chromatin",
        "molecular biology",
    ],
    &[
        "acoustic model",
        "audio recording",
        "speech recognition",
        "diarization",
        "transcript",
    ],
    &[
        "diatonic",
        "voice leading",
        "cadence",
        "songwriting",
        "harmonic function",
    ],
    &[
        "inscribed angle",
        "perpendicular bisector",
        "tangent",
        "euclidean",
        "compass",
    ],
    &[
        "options",
        "portfolio risk",
        "derivatives",
        "downside protection",
        "volatility",
    ],
    &[
        "shrub",
        "garden boundary",
        "pruning",
        "irrigation",
        "landscape design",
    ],
];

fn text_to_vector(text: &str) -> Vec<f32> {
    let lower = text.to_lowercase();
    TOPIC_KEYWORDS
        .iter()
        .map(|keywords| keywords.iter().filter(|kw| lower.contains(*kw)).count() as f32)
        .collect()
}

struct FixtureEmbedService;

#[async_trait]
impl EmbeddingService for FixtureEmbedService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|t| text_to_vector(t)).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "fixture-topic"
    }
}

struct FixtureEmbedProvider;

#[async_trait]
impl EmbedderProvider for FixtureEmbedProvider {
    fn name(&self) -> &str {
        MODEL_KEY
    }

    fn dimensions(&self) -> usize {
        DIM.max(10)
    }

    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
        Ok(Arc::new(FixtureEmbedService))
    }
}

fn rt_with_fixture_embedder() -> KhiveRuntime {
    let rt = KhiveRuntime::new(RuntimeConfig {
        git_write: Default::default(),
        db_path: None,
        default_namespace: Namespace::local(),
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "knowledge".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
    })
    .expect("in-memory runtime");
    rt.register_embedder(FixtureEmbedProvider);
    rt
}

fn build_registry(rt: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(crate::KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    registry
}

struct Pair {
    label: &'static str,
    query: &'static str,
    correct_name: &'static str,
    correct_desc: &'static str,
    collision_name: &'static str,
    collision_desc: &'static str,
}

const PAIRS: &[Pair] = &[
    Pair {
        label: "llm-inference (issue #90 production repro)",
        query: "speculative decoding inference acceleration techniques for large language models",
        correct_name: "Model Serving and Inference Optimization",
        correct_desc: "LLM_SERVING covering serving throughput latency batching kv cache scheduling gpu utilization request queuing model parallelism autoscaling deployment topology load balancing techniques for production inference workloads",
        collision_name: "Decoding and Batch-Inference Error Budgets for Communication-Aware ML",
        collision_desc: "COMM_THEORY covering channel coding error budgets communication theoretic bounds noisy channel capacity redundancy checksums forward error correction signal processing bandwidth allocation reliability engineering topics",
    },
    Pair {
        label: "contract-law vs general regard",
        query: "consideration requirement for enforceable bilateral contract formation",
        correct_name: "Contract Formation and Consideration Doctrine",
        correct_desc: "CONTRACT_LAW covering offer acceptance bargained-for exchange enforceability mutual assent capacity legality promissory estoppel breach remedies damages formation defenses statute of frauds topics",
        collision_name: "Consideration and Empathy in Workplace Communication",
        collision_desc: "AFFECT_REGARD covering thoughtfulness regard for others interpersonal communication empathy active listening workplace kindness emotional intelligence rapport building conflict de-escalation soft skills coaching topics",
    },
    Pair {
        label: "molecular transcription vs speech transcription",
        query: "RNA polymerase transcription initiation promoter binding gene expression",
        correct_name: "Transcription Factor Binding and Gene Regulation",
        correct_desc: "MOL_BIO covering polymerase promoter enhancer chromatin gene expression regulation transcription factor binding sites epigenetics histone modification cell signaling molecular biology laboratory technique topics",
        collision_name: "Automatic Transcription of Spoken Audio Recordings",
        collision_desc: "SPEECH_TO_TEXT covering audio recording acoustic model language model decoding speech recognition pipelines diarization noise robustness transcript formatting punctuation restoration captioning workflow topics",
    },
    Pair {
        label: "music chord progressions vs geometric chords",
        query: "diatonic chord progression voice leading harmonic function in songwriting",
        correct_name: "Chord Progressions and Harmonic Function",
        correct_desc: "MUSIC_THEORY covering diatonic harmony voice leading cadence songwriting chord substitution modal interchange functional harmony key modulation counterpoint composition arranging practice topics",
        collision_name: "Chord Length and Circle Geometry Theorems",
        collision_desc: "GEOMETRY covering circle chord arc theorem perpendicular bisector inscribed angle tangent secant power of a point compass straightedge construction proof classical euclidean topics",
    },
    Pair {
        label: "financial hedging vs garden hedges",
        query: "options based hedge strategy for portfolio downside risk management",
        correct_name: "Portfolio Hedging Strategies with Derivatives",
        correct_desc: "FINANCE_HEDGE covering options futures downside protection portfolio risk management delta hedging volatility exposure tail risk collar strategy derivatives pricing risk management topics",
        collision_name: "Hedge Planting and Garden Boundary Maintenance",
        collision_desc: "LANDSCAPING covering hedge shrub planting garden boundary pruning trimming schedule privacy screening soil preparation irrigation seasonal maintenance landscape design boundary fencing topics",
    },
];

async fn seed_pair(registry: &VerbRegistry, pair: &Pair, idx: usize) {
    registry
        .dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [
                    {
                        "slug": format!("fixture-{idx}-correct"),
                        "name": pair.correct_name,
                        "description": pair.correct_desc,
                    },
                    {
                        "slug": format!("fixture-{idx}-collision"),
                        "name": pair.collision_name,
                        "description": pair.collision_desc,
                    }
                ]
            }),
        )
        .await
        .expect("upsert domain pair");
}

/// Cosine similarity, mirroring `search::cosine_similarity` (module-private,
/// so duplicated here — this is pure test-fixture math, not shipped code).
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na < 1e-8 || nb < 1e-8 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// Full fixture table, run both through the shipped end-to-end `suggest()`
/// path AND through a manual reproduction of the blend formula at the OLD
/// alpha=0.7 (proving the pre-fix ordering was indeed wrong) and the NEW
/// alpha=0.3 (proving the fix, and matching the real `suggest()` outcome).
#[tokio::test]
async fn suggest_ranks_on_field_domain_above_lexical_collision() {
    const OLD_ALPHA: f32 = 0.7;
    const NEW_ALPHA: f32 = crate::knowledge::util::D_SUGGEST_RERANK_ALPHA;

    let mut regressions: Vec<String> = Vec::new();
    let mut report_rows: Vec<String> = Vec::new();

    for (idx, pair) in PAIRS.iter().enumerate() {
        let rt = rt_with_fixture_embedder();
        let registry = build_registry(&rt);
        seed_pair(&registry, pair, idx).await;

        // Raw lexical (TF-IDF) score per domain, unblended — `rerank:false`
        // skips the embedding leg entirely.
        let lex = registry
            .dispatch(
                "knowledge.search",
                json!({ "query": pair.query, "type": "domain", "rerank": false, "limit": 10 }),
            )
            .await
            .expect("lexical search");
        let lex_results = lex["results"].as_array().expect("results array");
        let lex_score = |name: &str| -> f32 {
            lex_results
                .iter()
                .find(|r| r["name"].as_str() == Some(name))
                .and_then(|r| r["score"].as_f64())
                .unwrap_or(0.0) as f32
        };
        let correct_lex = lex_score(pair.correct_name);
        let collision_lex = lex_score(pair.collision_name);
        let max_lex = correct_lex.max(collision_lex).max(1e-6);

        // Cosine leg from the SAME controlled embedder used by suggest().
        let q_vec = text_to_vector(pair.query);
        // suggest's embed text is "{name} {content}"; our marker-substring
        // matcher only needs the marker to appear anywhere in that string.
        let correct_text = format!("{} {}", pair.correct_name, pair.correct_desc);
        let collision_text = format!("{} {}", pair.collision_name, pair.collision_desc);
        let correct_cos = cosine(&q_vec, &text_to_vector(&correct_text)).max(0.0);
        let collision_cos = cosine(&q_vec, &text_to_vector(&collision_text)).max(0.0);

        let blend = |alpha: f32, lex: f32, cos: f32| alpha * (lex / max_lex) + (1.0 - alpha) * cos;
        let old_correct = blend(OLD_ALPHA, correct_lex, correct_cos);
        let old_collision = blend(OLD_ALPHA, collision_lex, collision_cos);
        let new_correct = blend(NEW_ALPHA, correct_lex, correct_cos);
        let new_collision = blend(NEW_ALPHA, collision_lex, collision_cos);

        let old_correct_wins = old_correct > old_collision;
        let new_correct_wins = new_correct > new_collision;

        // End-to-end: the real `suggest()` handler, fresh unwarmed ANN (no
        // `knowledge.index` run) so the FTS+fresh-cosine blend path is
        // exercised deterministically without ANN timing variance.
        let ann = vamana::new_shared();
        let token: NamespaceToken = rt.authorize(Namespace::local()).expect("authorize");
        let suggest_result =
            KnowledgeHandlers::suggest(&rt, &token, json!({ "query": pair.query }), &ann)
                .await
                .expect("suggest must not Err");
        let results = suggest_result["results"].as_array().expect("results array");
        let e2e_top_is_correct = results
            .first()
            .and_then(|r| r["name"].as_str())
            .is_some_and(|n| n == pair.correct_name);

        report_rows.push(format!(
            "| {} | {:.3}/{:.3} | {:.3}/{:.3} | old(0.7)={} | new({NEW_ALPHA})={} | e2e_top_correct={} |",
            pair.label,
            correct_lex,
            collision_lex,
            correct_cos,
            collision_cos,
            if old_correct_wins { "CORRECT_WINS" } else { "COLLISION_WINS" },
            if new_correct_wins { "CORRECT_WINS" } else { "COLLISION_WINS" },
            e2e_top_is_correct,
        ));

        if !new_correct_wins {
            regressions.push(format!(
                "{}: correct domain does NOT win under the shipped alpha={NEW_ALPHA}",
                pair.label
            ));
        }
        if !e2e_top_is_correct {
            regressions.push(format!(
                "{}: end-to-end suggest() does not rank the correct domain first",
                pair.label
            ));
        }
    }

    eprintln!("=== suggest ranking fixture (before/after) ===");
    for row in &report_rows {
        eprintln!("{row}");
    }

    assert!(
        regressions.is_empty(),
        "suggest ranking fixture regressions:\n{}\n\nfull table:\n{}",
        regressions.join("\n"),
        report_rows.join("\n")
    );
}
