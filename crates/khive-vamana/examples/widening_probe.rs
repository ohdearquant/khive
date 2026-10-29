//! Synthetic ANN-only widening probe for #3875, intended for an owner-host run.
//!
//! Uses one index and deterministic synthetic eligibility, not the full filtered
//! retrieval pipeline. Pooled latency percentiles repeat a few queries and are
//! not representative workload percentiles. Build, oracle, eligibility checks,
//! result validation, quality scoring and hit drop are outside the measured interval.
//! Percentiles use sorted sample index ceil((n - 1) * p / 100).

use std::collections::HashSet;
use std::time::Instant;

use khive_vamana::distance::try_l2_squared;
use khive_vamana::{VamanaConfig, VamanaIndex};
use rand::{rngs::StdRng, RngExt, SeedableRng};
use serde_json::{json, Value};

const SEED: u64 = 0x3875;
const N: usize = 20_000;
const DIM: usize = 32;
const QUERY_COUNT: usize = 24;
const REPEATS: usize = 6;
const TARGET: usize = 10;
const WIDTHS: [usize; 3] = [2_000, 4_000, 8_000];
const SCENARIOS: [&str; 5] = [
    "uniform_1pct",
    "uniform_5pct",
    "uniform_20pct",
    "near_1pct",
    "far_1pct",
];

struct Scenario {
    eligible: Vec<bool>,
    oracle: HashSet<usize>,
}

struct QueryFixture {
    query_id: usize,
    query: Vec<f32>,
    scenarios: Vec<Scenario>,
}

#[derive(Default)]
struct ResultSamples {
    latency_us: Vec<f64>,
    latency_by_query: Vec<Vec<f64>>,
    returned: Vec<usize>,
    fill: [f64; SCENARIOS.len()],
    recall: [f64; SCENARIOS.len()],
}

// Deterministic ID sampling; this does not model a real metadata distribution.
fn mix(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e3779b97f4a7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

fn make_corpus() -> Vec<f32> {
    let mut rng = StdRng::seed_from_u64(SEED);
    let mut corpus: Vec<f32> = (0..N * DIM)
        .map(|_| rng.random_range(-1.0f32..1.0))
        .collect();
    for row in corpus.chunks_exact_mut(DIM) {
        let norm = row.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(norm.is_finite() && norm > 0.0, "invalid synthetic vector");
        for x in row {
            *x /= norm;
        }
    }
    corpus
}

fn make_query_fixture(corpus: &[f32], query_id: usize) -> QueryFixture {
    let query = corpus[query_id * DIM..(query_id + 1) * DIM].to_vec();
    let mut exact: Vec<(usize, f32)> = corpus
        .chunks_exact(DIM)
        .enumerate()
        .map(|(id, row)| {
            let distance = try_l2_squared(&query, row).expect("exact oracle distance");
            assert!(distance.is_finite(), "non-finite oracle distance");
            (id, distance)
        })
        .collect();
    exact.sort_unstable_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    assert_eq!(exact.len(), N);
    assert_eq!(exact[0], (query_id, 0.0), "query must match its corpus row");
    assert!(exact[1].1 > 0.0, "query must have a unique exact match");

    let mut masks = vec![vec![false; N]; SCENARIOS.len()];
    for id in 0..N {
        let bucket = mix(id as u64 ^ SEED) % 100;
        for (scenario, threshold) in [1, 5, 20].iter().enumerate() {
            masks[scenario][id] = bucket < *threshold;
        }
    }
    for (rank, &(id, _)) in exact.iter().enumerate() {
        masks[3][id] = rank < N / 100;
        masks[4][id] = rank >= N - N / 100;
    }
    let scenarios: Vec<Scenario> = masks
        .into_iter()
        .map(|eligible| {
            let oracle: HashSet<usize> = exact
                .iter()
                .filter_map(|&(id, _)| eligible[id].then_some(id))
                .take(TARGET)
                .collect();
            assert_eq!(oracle.len(), TARGET, "not enough eligible oracle items");
            Scenario { eligible, oracle }
        })
        .collect();
    assert!(scenarios[3].eligible[query_id], "self must be near-eligible");
    assert!(
        scenarios[3].oracle.contains(&query_id),
        "self must be in near oracle"
    );
    QueryFixture {
        query_id,
        query,
        scenarios,
    }
}

fn validate_and_score(
    hits: &[(u32, f32)],
    fixture: &QueryFixture,
    corpus: &[f32],
    samples: &mut ResultSamples,
) {
    assert!(
        hits.len() >= TARGET && hits.len() <= N,
        "invalid ANN result size"
    );
    let mut unique = HashSet::with_capacity(hits.len());
    for &(id, distance) in hits {
        assert!((id as usize) < N, "ANN returned an invalid ID");
        assert!(
            distance.is_finite() && distance >= 0.0,
            "invalid ANN distance"
        );
        assert!(unique.insert(id), "ANN returned a duplicate ID");
        let start = id as usize * DIM;
        let exact = try_l2_squared(&fixture.query, &corpus[start..start + DIM])
            .expect("exact hit distance");
        assert!(
            (distance - exact).abs() <= 1e-5 * (1.0 + exact.abs()),
            "ANN hit distance differs from exact squared L2"
        );
    }
    assert!(
        hits.windows(2).all(|pair| {
            pair[0]
                .1
                .total_cmp(&pair[1].1)
                .then_with(|| pair[0].0.cmp(&pair[1].0))
                .is_le()
        }),
        "ANN results are not ordered by distance and ID"
    );
    for (scenario_index, scenario) in fixture.scenarios.iter().enumerate() {
        let chosen: Vec<usize> = hits
            .iter()
            .map(|&(id, _)| id as usize)
            .filter(|&id| scenario.eligible[id])
            .take(TARGET)
            .collect();
        samples.fill[scenario_index] += chosen.len() as f64 / TARGET as f64;
        samples.recall[scenario_index] += chosen
            .iter()
            .filter(|id| scenario.oracle.contains(*id))
            .count() as f64
            / TARGET as f64;
    }
    samples.returned.push(hits.len());
}

fn percentile(sorted: &[f64], percentage: usize) -> f64 {
    assert!(!sorted.is_empty());
    sorted[((sorted.len() - 1) * percentage).div_ceil(100)]
}

fn main() {
    let corpus = make_corpus();
    assert_eq!(corpus.len(), N * DIM);
    let fixtures: Vec<_> = (0..QUERY_COUNT)
        .map(|i| make_query_fixture(&corpus, i * (N / QUERY_COUNT)))
        .collect();
    let config = VamanaConfig::with_dimensions(DIM)
        .with_max_degree(32)
        .with_search_list_size(64);
    // Keep the original corpus for the oracle; this probe does not measure build memory.
    let index = VamanaIndex::build_owned(corpus.clone(), config).expect("build synthetic index");
    let mut results: [ResultSamples; WIDTHS.len()] = std::array::from_fn(|_| ResultSamples {
        latency_by_query: vec![Vec::new(); QUERY_COUNT],
        ..ResultSamples::default()
    });

    // Warm each query/width combination before collecting samples.
    for fixture in &fixtures {
        for width in WIDTHS {
            let hits = index.search(&fixture.query, width).expect("warmup search");
            assert!(hits.len() <= width, "warmup exceeds requested width");
            let mut warmup = ResultSamples::default();
            let self_hit = hits
                .iter()
                .find(|(id, _)| *id as usize == fixture.query_id)
                .expect("warmup must find the query's own vector");
            assert_eq!(self_hit.1, 0.0, "warmup self distance must be zero");
            validate_and_score(&hits, fixture, &corpus, &mut warmup);
            assert!(
                warmup.fill[3] > 0.0 && warmup.recall[3] > 0.0,
                "near-eligible self must contribute to warmup fill and recall"
            );
        }
    }

    for repetition in 0..REPEATS {
        for (query_index, fixture) in fixtures.iter().enumerate() {
            // Rotate ordering deterministically so one arm is not always first.
            for offset in 0..WIDTHS.len() {
                let arm = (repetition + query_index + offset) % WIDTHS.len();
                let start = Instant::now();
                let hits = index
                    .search(&fixture.query, WIDTHS[arm])
                    .expect("measured search");
                let elapsed_us = start.elapsed().as_secs_f64() * 1_000_000.0;
                assert!(hits.len() <= WIDTHS[arm], "search exceeds requested width");
                results[arm].latency_us.push(elapsed_us);
                results[arm].latency_by_query[query_index].push(elapsed_us);
                validate_and_score(&hits, fixture, &corpus, &mut results[arm]);
            }
        }
    }

    let arms: Vec<Value> = WIDTHS
        .iter()
        .zip(results.iter_mut())
        .map(|(&width, samples)| {
            let count = samples.latency_us.len();
            assert_eq!(count, QUERY_COUNT * REPEATS);
            assert_eq!(samples.returned.len(), count);
            samples.latency_us.sort_unstable_by(f64::total_cmp);
            let per_query: Vec<Value> = samples
                .latency_by_query
                .iter_mut()
                .enumerate()
                .map(|(query, latencies)| {
                    assert_eq!(latencies.len(), REPEATS);
                    latencies.sort_unstable_by(f64::total_cmp);
                    json!({
                        "query": query,
                        "samples": latencies.len(),
                        "p50_us": percentile(latencies, 50),
                    })
                })
                .collect();
            let quality: Vec<Value> = SCENARIOS
                .iter()
                .enumerate()
                .map(|(i, name)| json!({
                    "scenario": name,
                    "mean_fill_at_10": samples.fill[i] / count as f64,
                    "mean_exact_eligible_recall_at_10": samples.recall[i] / count as f64,
                }))
                .collect();
            json!({
                "k": width,
                "pooled_samples": count,
                "latency_us": {
                    "p50": percentile(&samples.latency_us, 50),
                    "p95": percentile(&samples.latency_us, 95),
                    "p99": percentile(&samples.latency_us, 99),
                },
                "returned_min": samples.returned.iter().min().unwrap(),
                "returned_max": samples.returned.iter().max().unwrap(),
                "quality": quality,
                "per_query_latency": per_query,
            })
        })
        .collect();
    println!(
        "{}",
        json!({
            "purpose": "synthetic ANN-only candidate widening; not full filtered search",
            "limitations": "single thread; repeated corpus-row queries; per-arm timing excludes sequential widening and other stages; pooled percentiles are not workload distributions; repetitions mostly sample scheduler noise; no RSS measurement",
            "model": "synthetic normalized vectors; exact squared L2 oracle",
            "max_degree": 32,
            "search_list_size": 64,
            "seed": SEED,
            "corpus_vectors": N,
            "dimensions": DIM,
            "distinct_queries": QUERY_COUNT,
            "repetitions_per_query_per_arm": REPEATS,
            "arm_order": "rotated by repetition and query index",
            "percentile_estimator": "sorted index ceil((n - 1) * p / 100)",
            "timed_region": "VamanaIndex::search call; excludes result drop and validation",
            "arms": arms,
        })
    );
}
