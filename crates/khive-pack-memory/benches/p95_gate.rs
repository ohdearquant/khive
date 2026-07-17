//! ADR-116 gate condition 2: p95 baseline for `memory.recall` on the warm ANN path.
//!
//! ADR-116 (PR #1080, in review) defines the warm-hit generation-read gate against a
//! **file-backed WAL SQLite database with warm page cache "at one and three models"** —
//! that is the number of embedding models a single `memory.recall` call *queries* (M in
//! "returning M two-column/one-row records for M queried models"), not the total number
//! of models registered on the runtime. This harness measures exactly those two gate
//! configurations, each against its own runtime and database, plus one clearly-labeled
//! beyond-gate four-model fan-out row kept for context:
//!
//! - **one-model**: one embedding model registered, `memory.recall` queries it explicitly
//!   (M=1 queried).
//! - **three-model**: three embedding models registered, `memory.recall` called with no
//!   `embedding_model` so it fans out to all three (M=3 queried) — this is the ADR-116
//!   multi-model gate case.
//! - **four-model fan-out (beyond gate)**: four embedding models registered (matches the
//!   corpus this repo actually runs), `memory.recall` fans out to all four. ADR-116 does
//!   not gate M=4; this row is informational only and must never be read as a "primary"
//!   or single-model baseline.
//!
//! ADR-116 is Proposed, not yet implemented: this harness establishes the pre-change
//! baseline. Once the durable per-model generation read lands, rerun this bench and diff
//! against the recorded baseline to confirm the added generation check stays inside the
//! gate.
//!
//! ## Warm-route assertion
//!
//! `memory.recall`'s response marks every result with `"degraded": "ann_unavailable"`
//! when at least one queried model's vector leg missed its bounded ANN-readiness wait
//! and was served FTS-only instead (`crates/khive-pack-memory/src/handlers/recall.rs`,
//! `#836`). This harness polls `memory.recall` after seeding until it observes several
//! consecutive clean (non-degraded, non-empty) responses, treating that as "ANN warm"
//! before starting the timed loop, and asserts on every timed sample that the response
//! carries no `degraded` marker and is non-empty, panicking immediately otherwise. This
//! positively rules out the bounded-wait FTS-degradation fallback for every recorded
//! sample.
//!
//! The `khive-pack-memory::ann` module's per-model freshness state (`ann::is_current`),
//! its ANN-vs-sqlite-vec route variable, and its `warm_route_count` counter are all
//! `pub(crate)` — invisible to this harness, which lives in `benches/`, a separate crate
//! that only sees `khive-pack-memory`'s `pub` items. What *is* `pub` and crate-external is
//! the event plane: `ensure_ann_for_model` (the only path that installs or rebuilds a
//! model's ANN graph) emits a `memory.ann_warm` phase-started/completed event through
//! `KhiveRuntime::events`, a `pub` accessor returning `khive_storage::EventStore`
//! (`count_events`/`query_events` are both `pub` trait methods). This harness uses that:
//! it snapshots the `memory.ann_warm` event count immediately before the timed loop and
//! again immediately after, and asserts the count is unchanged. No ANN rebuild happens
//! without one of these events, and the internal sqlite-vec exact-fallback path (taken
//! only after an ANN search error) clears the model's cached graph as a side effect, so
//! the *next* recall for that model would trigger exactly such a rebuild — making a
//! zero-event-count window strong (though not airtight) evidence that none of the timed
//! samples took the exact-fallback path either.
//!
//! One wrinkle: a per-model durable-epoch debounce check (`maybe_check_durable_epoch`,
//! independent of ADR-116) can itself kick off a background ANN rebuild once its debounce
//! interval elapses after seeding — a benign maintenance action that still serves the
//! stale-but-installed graph on the fast path (not a slow/degraded sample) but does emit
//! `memory.ann_warm` events. Left unhandled this trips the event-count assertion on a false
//! positive. The harness closes over this by sleeping past that debounce interval plus one
//! settle recall (`EPOCH_DEBOUNCE_SETTLE`) after the warm-wait and before opening the timed
//! window, so any such rebuild fires and completes before counting starts; the ~200-call
//! timed window itself completes in well under one further debounce interval.
//!
//! The residual gap: an exact-fallback on the very last timed sample, with no subsequent
//! call in the window to reveal the resulting rebuild, would not be caught. Closing that
//! gap requires a `pub(crate)`-or-narrower counter to become crate-visible; tracked as
//! issue #1084 (verb-surface route observability). Partial honesty about what is and is
//! not assertable beats a fabricated positive-route assertion.
//!
//! Run:
//! ```bash
//! cd crates && cargo bench -p khive-pack-memory --bench p95_gate
//! ```
//!
//! Isolation: run on a quiet machine with no concurrent builds, benchmarks, or heavy I/O
//! in progress. Run the suite twice and require consistent numbers across both runs
//! before recording or refreshing baselines.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistryBuilder};
use khive_storage::EventFilter;
use lattice_embed::EmbeddingModel;

/// Memories seeded per registered model. Large enough to force the Vamana ANN path (not
/// the small-corpus exact fallback) while keeping local CPU embedding time bounded.
const MEMORIES_PER_MODEL: usize = 200;

/// Recall iterations measured per configuration after warmup, for percentile stats.
const RECALL_ITERS: usize = 200;

/// `khive_pack_memory::ann::maybe_check_durable_epoch`'s debounce interval outside a
/// `#[cfg(test)]` build (5s). One settle sleep of longer than this, followed by one more
/// recall, lets any epoch-check due since seeding fire and its background rebuild's
/// `memory.ann_warm` events land before the timed window opens — otherwise that debounced
/// check (a background maintenance action against a still-warm, still-fast route; see
/// `search_loaded_serves_stale_installed_entry_without_rebuild` in `ann.rs`) can fire mid-
/// timing and trip the event-count assertion below on a false positive.
const EPOCH_DEBOUNCE_SETTLE: Duration = Duration::from_millis(5_200);

/// Bounded attempts while polling for a stable warm ANN route before timing starts.
const WARM_WAIT_MAX_ATTEMPTS: usize = 200;
/// Consecutive clean (non-degraded, non-empty) recalls required to declare warm.
const WARM_WAIT_CONSECUTIVE_CLEAN: usize = 5;
/// Sleep between warm-wait polls after a non-clean response.
const WARM_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

const PRIMARY_MODEL: EmbeddingModel = EmbeddingModel::BgeSmallEnV15;
const RETIRED_MODELS: [EmbeddingModel; 3] = [
    EmbeddingModel::MultilingualE5Small,
    EmbeddingModel::AllMiniLmL6V2,
    EmbeddingModel::ParaphraseMultilingualMiniLmL12V2,
];

const CONTENT_PHRASES: &[&str] = &[
    "attention mechanism transformers query key value projection",
    "Rust ownership borrow checker lifetime memory safety",
    "knowledge graph entity edge relation ontology traversal",
    "agent orchestration parallel multi-agent coordination patterns",
    "recall scoring fusion strategy weighted reciprocal rank",
    "namespace isolation security token authentication gate",
    "git workflow commit branch pull request review merge",
    "embedding model vector search cosine similarity index",
    "full text search trigram BM25 inverted index tokenizer",
    "memory decay salience temporal ranking pipeline schedule",
    "SQLite write ahead log checkpoint durability transaction",
    "vamana ANN graph greedy construction beam search",
];

const RECALL_QUERY: &str = "recall scoring fusion vector search memory decay";

/// One ADR-116 gate configuration: which models are registered on the runtime, and how
/// `memory.recall` is called against them (explicit single model vs. fan-out `None`).
struct GateConfig {
    label: &'static str,
    primary: EmbeddingModel,
    additional: &'static [EmbeddingModel],
    recall_model: Option<EmbeddingModel>,
    gate_note: &'static str,
}

fn gate_configs() -> Vec<GateConfig> {
    vec![
        GateConfig {
            label: "one-model",
            primary: PRIMARY_MODEL,
            additional: &[],
            recall_model: Some(PRIMARY_MODEL),
            gate_note: "ADR-116 gate case M=1 queried",
        },
        GateConfig {
            label: "three-model fan-out",
            primary: PRIMARY_MODEL,
            additional: &RETIRED_MODELS[0..2],
            recall_model: None,
            gate_note: "ADR-116 gate case M=3 queried",
        },
        GateConfig {
            label: "four-model fan-out (beyond gate, informational)",
            primary: PRIMARY_MODEL,
            additional: &RETIRED_MODELS,
            recall_model: None,
            gate_note: "M=4 queried — ADR-116 gates only M=1 and M=3; not a primary-only baseline",
        },
    ]
}

impl GateConfig {
    fn registered_models(&self) -> Vec<EmbeddingModel> {
        std::iter::once(self.primary)
            .chain(self.additional.iter().copied())
            .collect()
    }
}

fn make_runtime(
    db_path: PathBuf,
    primary: EmbeddingModel,
    additional: Vec<EmbeddingModel>,
) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: Some(db_path),
        embedding_model: Some(primary),
        additional_embedding_models: additional,
        ..RuntimeConfig::default()
    })
    .expect("file-backed WAL runtime")
}

/// Count `memory.ann_warm` phase events recorded so far in `rt`'s local namespace. This is
/// the `pub` oracle instrument for ANN (re)builds — see the module-level "Warm-route
/// assertion" doc for what a stable count across the timed window does and does not prove.
async fn ann_warm_event_count(rt: &KhiveRuntime) -> u64 {
    let token = rt
        .authorize(Namespace::local())
        .expect("authorize local namespace for event-plane read");
    let events = rt.events(&token).expect("event store available");
    events
        .count_events(EventFilter {
            verbs: vec!["memory.ann_warm".to_string()],
            ..EventFilter::default()
        })
        .await
        .expect("count_events")
}

fn make_registry(rt: &KhiveRuntime) -> khive_runtime::VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(rt.clone()));
    builder.build().expect("registry builds")
}

async fn seed_model(registry: &khive_runtime::VerbRegistry, model: EmbeddingModel, n: usize) {
    for i in 0..n {
        let phrase = CONTENT_PHRASES[i % CONTENT_PHRASES.len()];
        let content = format!("{phrase} — {model} seed-{i}");
        registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": content,
                    "memory_type": "semantic",
                    "salience": 0.6,
                    "decay_factor": 0.01,
                    "embedding_model": model.to_string(),
                }),
            )
            .await
            .unwrap_or_else(|e| panic!("remember seeding failed for {model}: {e}"));
    }
}

async fn recall_once(
    registry: &khive_runtime::VerbRegistry,
    model: Option<EmbeddingModel>,
) -> (u128, Value) {
    let mut params = json!({
        "query": RECALL_QUERY,
        "limit": 10,
    });
    if let Some(m) = model {
        params["embedding_model"] = json!(m.to_string());
    }
    let t = Instant::now();
    let resp = registry
        .dispatch("memory.recall", params)
        .await
        .expect("recall");
    (t.elapsed().as_micros(), resp)
}

/// A clean sample is a non-empty result array with no `degraded` marker on any result —
/// see the module-level "Warm-route assertion" doc for exactly what this does and does
/// not prove.
fn is_clean_ann_route(resp: &Value) -> bool {
    let Some(arr) = resp.as_array() else {
        return false;
    };
    !arr.is_empty() && !arr.iter().any(|r| r.get("degraded").is_some())
}

/// Poll `memory.recall` until it returns several consecutive clean (non-degraded,
/// non-empty) responses, or panic loud if the route never stabilizes. Seeding schedules
/// an async ANN rebuild; this closes the race instead of assuming N warmup calls are
/// enough.
async fn wait_until_ann_warm(
    registry: &khive_runtime::VerbRegistry,
    label: &str,
    model: Option<EmbeddingModel>,
) {
    let mut consecutive_clean = 0usize;
    for attempt in 0..WARM_WAIT_MAX_ATTEMPTS {
        let (_us, resp) = recall_once(registry, model).await;
        if is_clean_ann_route(&resp) {
            consecutive_clean += 1;
            if consecutive_clean >= WARM_WAIT_CONSECUTIVE_CLEAN {
                eprintln!(
                    "  [{label}] ANN route warm after {} attempt(s) ({} consecutive clean)",
                    attempt + 1,
                    consecutive_clean
                );
                return;
            }
        } else {
            consecutive_clean = 0;
            tokio::time::sleep(WARM_WAIT_POLL_INTERVAL).await;
        }
    }
    panic!(
        "[{label}] ANN route did not reach a stable warm state after {WARM_WAIT_MAX_ATTEMPTS} \
         attempts (model={model:?}) — memory.recall kept returning ann_unavailable degradation \
         or empty results. Refusing to record a p95 baseline against a degraded route."
    );
}

struct Percentiles {
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    n: usize,
}

fn percentiles(mut latencies_us: Vec<u128>) -> Percentiles {
    latencies_us.sort_unstable();
    let n = latencies_us.len();
    let at = |q: f64| -> f64 {
        let idx = ((n as f64 * q).ceil() as usize)
            .saturating_sub(1)
            .min(n - 1);
        latencies_us[idx] as f64 / 1000.0
    };
    Percentiles {
        p50_ms: at(0.50),
        p95_ms: at(0.95),
        p99_ms: at(0.99),
        n,
    }
}

async fn bench_configuration(config: &GateConfig) -> Percentiles {
    let tmp = tempfile::Builder::new()
        .prefix("khive-p95-gate-")
        .tempdir()
        .expect("tmpdir");
    let db_path = tmp.path().join("p95-gate.db");
    let registered = config.registered_models();

    eprintln!("\n--- {} ({}) ---", config.label, config.gate_note);
    eprintln!(
        "db: {} (file-backed, WAL pool); registered models ({}): {}",
        db_path.display(),
        registered.len(),
        registered
            .iter()
            .map(|m| m.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    eprintln!(
        "recall query mode: {}",
        match config.recall_model {
            Some(m) => format!("explicit single model ({m})"),
            None => format!(
                "fan-out (embedding_model omitted, queries all {})",
                registered.len()
            ),
        }
    );

    let rt = make_runtime(db_path.clone(), config.primary, config.additional.to_vec());
    let registry = make_registry(&rt);

    let t_seed = Instant::now();
    for model in &registered {
        seed_model(&registry, *model, MEMORIES_PER_MODEL).await;
    }
    eprintln!(
        "seeded {} memories ({} per model x {} models) in {:.1}s",
        MEMORIES_PER_MODEL * registered.len(),
        MEMORIES_PER_MODEL,
        registered.len(),
        t_seed.elapsed().as_secs_f64()
    );

    wait_until_ann_warm(&registry, config.label, config.recall_model).await;

    // Let any durable-epoch debounce check already due from seeding fire and settle
    // (see EPOCH_DEBOUNCE_SETTLE) before opening the timed window, then confirm one more
    // clean call — this both consumes the next due check deterministically and re-asserts
    // the route is still clean after whatever background rebuild that check may have kicked
    // off.
    tokio::time::sleep(EPOCH_DEBOUNCE_SETTLE).await;
    let (_us, settle_resp) = recall_once(&registry, config.recall_model).await;
    assert!(
        is_clean_ann_route(&settle_resp),
        "[{}] post-settle recall observed ann_unavailable degradation (or empty results)",
        config.label
    );

    let ann_warm_events_before = ann_warm_event_count(&rt).await;
    let mut latencies = Vec::with_capacity(RECALL_ITERS);
    for i in 0..RECALL_ITERS {
        let (us, resp) = recall_once(&registry, config.recall_model).await;
        assert!(
            is_clean_ann_route(&resp),
            "[{}] timed sample {i} observed ann_unavailable degradation (or empty results) — \
             warm-route assertion failed, refusing to record this baseline",
            config.label
        );
        latencies.push(us);
    }
    let ann_warm_events_after = ann_warm_event_count(&rt).await;
    assert_eq!(
        ann_warm_events_after, ann_warm_events_before,
        "[{}] a memory.ann_warm phase event fired during the timed window — an ANN graph \
         (re)build started mid-measurement, meaning at least one timed sample raced a rebuild \
         or took the sqlite-vec exact-fallback route (which clears the cached graph and \
         triggers exactly this event on the following call). Refusing to record this baseline.",
        config.label
    );
    let stats = percentiles(latencies);
    eprintln!(
        "  {}: p50={:.3}ms p95={:.3}ms p99={:.3}ms n={}",
        config.label, stats.p50_ms, stats.p95_ms, stats.p99_ms, stats.n
    );
    stats
}

#[tokio::main]
async fn main() {
    let configs = gate_configs();

    eprintln!(
        "ADR-116 (PR #1080, in review) gate condition 2 — memory.recall p95 baseline, \
         warm ANN path, {} configuration(s)",
        configs.len()
    );

    let mut rows = Vec::with_capacity(configs.len());
    for config in &configs {
        let stats = bench_configuration(config).await;
        rows.push((config, stats));
    }

    println!("{}", "=".repeat(96));
    println!("ADR-116 (PR #1080, in review) GATE — memory.recall p95 baseline (file-backed WAL, warm ANN)");
    println!("{}", "=".repeat(96));
    println!(
        "{:<45} {:>8} {:>8} {:>8} {:>6}  note",
        "configuration", "p50 ms", "p95 ms", "p99 ms", "n"
    );
    println!("{}", "-".repeat(96));
    for (config, stats) in &rows {
        println!(
            "{:<45} {:>8.3} {:>8.3} {:>8.3} {:>6}  {}",
            config.label, stats.p50_ms, stats.p95_ms, stats.p99_ms, stats.n, config.gate_note
        );
    }
    println!("{}", "=".repeat(96));
    println!(
        "gate reference (ADR-116 §Warm hit, PR #1080): the added per-model durable generation \
         check must cost at most 1.0ms absolute p95 and at most 5% of the matching M=1 or M=3 \
         baseline's warm memory.recall p95 above. The M=4 row is beyond the gate's stated \
         configurations and is informational only."
    );
}
