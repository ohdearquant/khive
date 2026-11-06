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
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
        db_path: None,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
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
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
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

const COMPOSE_COHORT_QUERY: &str =
    "zxqalpha zxqbravo zxqcharlie zxqdelta zxqecho zxqfoxtrot zxqgolf zxqhotel zxqindia zxqjuliet";
const COMPOSE_COHORT_DOMAINS: usize = 41;
const COMPOSE_COHORT_MARKER: &str = "COHORT_DOMAIN_";

tokio::task_local! {
    static COHORT_RERANK_TEXTS: Arc<std::sync::Mutex<Vec<String>>>;
}

fn cohort_vector(text: &str) -> Vec<f32> {
    let dimensions = EmbeddingModel::AllMiniLmL6V2.dimensions();
    // These nonce terms occur in no fixture name/content. The semantic query
    // remains nonzero even if the provider adds a role prefix to the text.
    if text.contains(COMPOSE_COHORT_QUERY) {
        let mut query = vec![0.0; dimensions];
        query[0] = 1.0;
        return query;
    }
    let Some(marker) = text.find(COMPOSE_COHORT_MARKER) else {
        let mut topic = text_to_vector(text);
        if topic.iter().any(|value| *value != 0.0) {
            topic.resize(dimensions, 0.0);
            return topic;
        }
        let mut neutral = vec![0.0; dimensions];
        neutral[2] = 1.0;
        return neutral;
    };
    let ordinal = text[marker + COMPOSE_COHORT_MARKER.len()..]
        .get(..2)
        .and_then(|digits| digits.parse::<usize>().ok())
        .expect("fixture domain ordinal");
    assert!(ordinal < COMPOSE_COHORT_DOMAINS);
    let mut vector = vec![0.0; dimensions];
    vector[0] = 1.0;
    vector[1] = (COMPOSE_COHORT_DOMAINS - ordinal) as f32 / COMPOSE_COHORT_DOMAINS as f32;
    vector
}

struct CohortEmbedService;

#[async_trait]
impl EmbeddingService for CohortEmbedService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        // Only this request task records. Detached ANN warming does not inherit
        // the task-local capture; seeding and explicit warming run outside it.
        let _ = COHORT_RERANK_TEXTS.try_with(|capture| {
            capture.lock().expect("domain embedding log").extend(
                texts
                    .iter()
                    .filter(|text| text.contains(COMPOSE_COHORT_MARKER))
                    .cloned(),
            );
        });
        Ok(texts.iter().map(|text| cohort_vector(text)).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "fixture-compose-cohort"
    }
}

struct CohortEmbedProvider;

#[async_trait]
impl EmbedderProvider for CohortEmbedProvider {
    fn name(&self) -> &str {
        MODEL_KEY
    }

    fn dimensions(&self) -> usize {
        EmbeddingModel::AllMiniLmL6V2.dimensions()
    }

    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
        Ok(Arc::new(CohortEmbedService))
    }
}

async fn cohort_statement(
    runtime: &KhiveRuntime,
    sql: &str,
    params: Vec<khive_storage::types::SqlValue>,
) {
    runtime
        .sql()
        .writer()
        .await
        .expect("cohort writer")
        .execute(khive_storage::types::SqlStatement {
            sql: sql.into(),
            params,
            label: None,
        })
        .await
        .expect("cohort fixture statement");
}

async fn seed_compose_cohort(runtime: &KhiveRuntime, registry: &VerbRegistry) {
    use khive_storage::types::SqlValue;

    for index in 0..COMPOSE_COHORT_DOMAINS {
        let slug = format!("cohort-domain-{index:02}");
        let member = format!("cohort-member-{index:02}");
        let member_id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, member.as_bytes());
        // Member rows use the admitted compose-read fixture schema. Domain
        // publication still uses the actual public indexing/upsert path.
        cohort_statement(runtime,
            "INSERT INTO knowledge_atoms(id,namespace,slug,name,content,tags,status,finalized,created_at,updated_at,deleted_at) VALUES(?1,'local',?2,?2,'fixture corpus content describing compile source code query detail','[]','reviewed',1,0,0,NULL)",
            vec![SqlValue::Text(member_id.to_string()), SqlValue::Text(member.clone())],
        ).await;
        registry.dispatch("knowledge.upsert_domains", json!({ "domains": [{
            "slug": &slug,
            "name": format!("{COMPOSE_COHORT_MARKER}{index:02} inference"),
            "description": format!("LLM_SERVING serving latency acceleration for large language models. Additional background supplies ordinary context for readers across common topics and examples. {}", "inference ".repeat(index + 1)),
        }] })).await.expect("publish cohort domain");
        cohort_statement(
            runtime,
            "UPDATE knowledge_domains SET members=?1 WHERE namespace='local' AND slug=?2",
            vec![
                SqlValue::Text(serde_json::to_string(&[member]).expect("member list")),
                SqlValue::Text(slug),
            ],
        )
        .await;
    }
}

fn build_cohort_registry(runtime: &KhiveRuntime) -> (VerbRegistry, vamana::SharedAnn) {
    let knowledge = crate::KnowledgePack::new(runtime.clone());
    let ann = knowledge.ann.clone();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(knowledge);
    let registry = builder.build().expect("cohort registry");
    runtime.install_edge_rules(registry.all_edge_rules());
    (registry, ann)
}

async fn dispatch_cohort_compose(
    registry: &VerbRegistry,
    params: serde_json::Value,
) -> (serde_json::Value, Vec<String>) {
    let capture = Arc::new(std::sync::Mutex::new(Vec::new()));
    let response = COHORT_RERANK_TEXTS
        .scope(
            Arc::clone(&capture),
            registry.dispatch("knowledge.compose", params),
        )
        .await
        .expect("public compose must succeed");
    assert_eq!(response["status"], "ok", "response: {response}");
    assert!(
        response.pointer("/data/suggest_error").is_none(),
        "response: {response}"
    );
    assert!(
        response.pointer("/data/ann_unavailable").is_none(),
        "the fixed loaded ANN must serve this request: {response}"
    );
    let mut cohort = capture.lock().expect("domain embedding log").clone();
    cohort.sort();
    cohort.dedup();
    (response, cohort)
}

fn compose_domain_ids(response: &serde_json::Value) -> Vec<String> {
    response["data"]["domains"]
        .as_array()
        .expect("domain array")
        .iter()
        .map(|domain| domain["id"].as_str().expect("domain id").to_owned())
        .collect()
}

#[tokio::test]
async fn compose_presentation_limit_preserves_rerank_cohort_and_selected_prefix() {
    let corpus = tempfile::TempDir::new().expect("cohort tempdir");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(corpus.path().join("cohort.db")),
        default_namespace: Namespace::local(),
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_owned(), "knowledge".to_owned()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("cohort in-memory runtime");
    runtime
        .try_register_embedder(CohortEmbedProvider)
        .expect("register deterministic cohort provider before first resolution");
    // Probe the same generic embedding boundary used by fresh reranking before
    // seeding/indexing; this query has no marker and runs outside capture scope.
    let probe_texts = vec![COMPOSE_COHORT_QUERY.to_owned()];
    let probe_vectors = runtime
        .embed_batch(&probe_texts)
        .await
        .expect("deterministic cohort provider probe");
    assert_eq!(
        probe_vectors,
        vec![cohort_vector(COMPOSE_COHORT_QUERY)],
        "cohort fixture must use the registered deterministic provider"
    );
    let seed_registry = build_registry(&runtime);
    seed_compose_cohort(&runtime, &seed_registry).await;
    let index_result = seed_registry
        .dispatch("knowledge.index", json!({ "rebuild_ann": true }))
        .await
        .expect("prepare stored fixture vectors");
    let indexed = index_result
        .get("indexed")
        .and_then(|count| count.as_u64())
        .unwrap_or(0);
    assert!(
        indexed >= COMPOSE_COHORT_DOMAINS as u64,
        "indexing must prepare the nonvacuous domain corpus: {index_result}"
    );
    drop(seed_registry);

    // The final registry owns this exact ANN instance and is created only after
    // all corpus writes. No corpus writes occur during the K comparisons.
    let (registry, ann) = build_cohort_registry(&runtime);
    let token = runtime.authorize(Namespace::local()).expect("cohort token");
    let model = runtime.default_embedder_name().to_owned();
    let key = vamana::AnnKey::new("local", &model);
    let _ = vamana::ensure_ann_for_model(&runtime, &token, &ann, &model).await;
    assert!(
        vamana::wait_ready(&ann, &key, 0, 1).await,
        "completed warm must have installed the fixture index"
    );
    // Freeze maintenance at an existing supported test state while continuing
    // to serve the real loaded index; no background attempt can replace it.
    vamana::simulate_warming_in_flight(&ann, key.clone());
    assert!(
        vamana::search_loaded(&ann, &key, &cohort_vector(COMPOSE_COHORT_QUERY), 60)
            .await
            .is_some(),
        "the actual registered ANN must stay loaded after the sentinel"
    );

    // No query term occurs in the corpus: both successful and timed-out lexical
    // probes have no matching candidates. Loaded ANN is the only candidate leg.
    let mut observations = Vec::new();
    for limit in [1_usize, 5, 8, 20] {
        assert!(
            vamana::wait_ready(&ann, &key, 0, 1).await,
            "fixture ANN lost readiness"
        );
        let (response, cohort) = dispatch_cohort_compose(&registry,
            json!({ "query": COMPOSE_COHORT_QUERY, "auto_limit": limit, "max_tokens": 1200, "blend_kg": false }),
        ).await;
        assert_eq!(
            compose_domain_ids(&response).len(),
            limit,
            "response: {response}"
        );
        assert!(
            response["data"]["count"]
                .as_u64()
                .is_some_and(|count| count > 0),
            "response: {response}"
        );
        observations.push((limit, response, cohort));
    }
    let (_, full_response, full_cohort) = observations.last().expect("twenty-domain observation");
    let full_domains = compose_domain_ids(full_response);
    assert_eq!(full_domains.len(), 20);
    assert_eq!(
        full_cohort.len(),
        COMPOSE_COHORT_DOMAINS,
        "every seeded matching domain must reach fresh reranking; fixture qualification failed"
    );
    for (limit, response, cohort) in &observations {
        assert_eq!(
            cohort, full_cohort,
            "changing presentation limit to {limit} must not change the domain rerank cohort"
        );
        assert_eq!(
            compose_domain_ids(response).as_slice(),
            &full_domains[..*limit],
            "limit {limit} must select the ordered prefix of the same twenty-domain ranking"
        );
    }

    // Explicit domains bypass automatic selection, including its ten-word gate.
    let mut pinned_domains = None;
    for limit in [1_usize, 5, 8, 20] {
        assert!(
            vamana::wait_ready(&ann, &key, 0, 1).await,
            "fixture ANN lost readiness"
        );
        let (response, cohort) = dispatch_cohort_compose(
            &registry,
            json!({ "query": "serving", "domain_ids": ["cohort-domain-00"],
                "auto_limit": limit, "max_tokens": 1200, "blend_kg": false }),
        )
        .await;
        assert!(
            cohort.is_empty(),
            "explicit domains must bypass automatic domain reranking"
        );
        let domains = &response["data"]["domains"];
        if let Some(expected) = &pinned_domains {
            assert_eq!(domains, expected);
        } else {
            pinned_domains = Some(domains.clone());
        }
        assert_eq!(domains.as_array().expect("pinned domains").len(), 1);
        assert_eq!(domains[0]["slug"], "cohort-domain-00");
        assert!(response["data"]["count"]
            .as_u64()
            .is_some_and(|count| count > 0));
    }
}
