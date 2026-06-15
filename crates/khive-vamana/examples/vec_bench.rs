//! Asserting ANN scale-proof bench — canonical schema v1.0.
//!
//! Drives dim from the loaded fvecs file (no hardcoded DIM const).
//! Produces a JSON file with the §1 schema and exits nonzero on assertion failure.
//!
//! Usage:
//!   KHIVE_N_CAP=1000000 cargo run --release -p khive-vamana --example vec_bench -- \
//!     --base  /data/sift/sift_base.fvecs \
//!     --query /data/sift/sift_query.fvecs \
//!     --ns    100000,200000,500000,1000000 \
//!     --dataset "SIFT-1M" \
//!     --targets perf/targets.toml \
//!     --out   probe-results.json
//!
//! Schema version: 1.0 (spec §1)
//! Canonical key in targets.toml: khive-vamana/1m-scale-proof/sift-1m
//!                              or khive-vamana/scale-proof/khive-real-384

use std::{
    collections::HashSet,
    fs::File,
    io::{BufReader, Read},
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use khive_vamana::{VamanaConfig, VamanaGraph, VisitedSet};
use serde::Serialize;
use serde_json::{json, Value};

// ─── tunables ────────────────────────────────────────────────────────────────

const K: usize = 10;
const TARGET_RECALL: f64 = 0.95;
const MAX_ISO_BEAM: usize = 1024;
const MAX_DEGREE: usize = 64;
const SEARCH_LIST_SIZE: usize = 128;
const BUILD_BATCH: usize = 1024;
const GT_QUERY_SAMPLE: usize = 1000;
const LATENCY_QUERY_SAMPLE: usize = 1000;
const LLC_SIZE_BYTES: u64 = 12 * 1024 * 1024;

// ─── CLI args ────────────────────────────────────────────────────────────────

struct Args {
    base_path: PathBuf,
    query_path: PathBuf,
    ns: Vec<usize>,
    out: PathBuf,
    dataset: String,
    alpha: f64,
    targets: Option<PathBuf>,
    target_key: String,
    intrinsic_dim: f64,
    normalization: String,
    source_url: Option<String>,
}

fn parse_args() -> Args {
    let mut args_iter = std::env::args().skip(1);
    let mut base_path = PathBuf::from("base.fvecs");
    let mut query_path = PathBuf::from("query.fvecs");
    let mut ns = vec![100_000usize, 300_000];
    let mut out = PathBuf::from("probe-results.json");
    let mut dataset = String::from("unknown");
    let mut alpha = 1.0f64;
    let mut targets: Option<PathBuf> = None;
    let mut target_key = String::from("khive-vamana/scale-proof/khive-real-384");
    let mut intrinsic_dim = 0.0f64;
    let mut normalization = String::from("l2");
    let mut source_url: Option<String> = None;

    while let Some(key) = args_iter.next() {
        match key.as_str() {
            "--base" => {
                if let Some(val) = args_iter.next() {
                    base_path = PathBuf::from(val);
                }
            }
            "--query" => {
                if let Some(val) = args_iter.next() {
                    query_path = PathBuf::from(val);
                }
            }
            "--ns" => {
                if let Some(val) = args_iter.next() {
                    ns = val
                        .split(',')
                        .filter_map(|s| s.trim().parse().ok())
                        .collect();
                }
            }
            "--out" => {
                if let Some(val) = args_iter.next() {
                    out = PathBuf::from(val);
                }
            }
            "--dataset" => {
                if let Some(val) = args_iter.next() {
                    dataset = val;
                }
            }
            "--alpha" => {
                if let Some(val) = args_iter.next() {
                    if let Ok(a) = val.trim().parse::<f64>() {
                        alpha = a;
                    }
                }
            }
            "--targets" => {
                if let Some(val) = args_iter.next() {
                    targets = Some(PathBuf::from(val));
                }
            }
            "--target-key" => {
                if let Some(val) = args_iter.next() {
                    target_key = val;
                }
            }
            "--intrinsic-dim" => {
                if let Some(val) = args_iter.next() {
                    if let Ok(v) = val.trim().parse::<f64>() {
                        intrinsic_dim = v;
                    }
                }
            }
            "--normalization" => {
                if let Some(val) = args_iter.next() {
                    normalization = val;
                }
            }
            "--source-url" => {
                if let Some(val) = args_iter.next() {
                    source_url = Some(val);
                }
            }
            _ => {}
        }
    }

    Args {
        base_path,
        query_path,
        ns,
        out,
        dataset,
        alpha,
        targets,
        target_key,
        intrinsic_dim,
        normalization,
        source_url,
    }
}

// ─── fvecs loader ────────────────────────────────────────────────────────────

fn load_fvecs(path: &Path) -> (Vec<f32>, usize) {
    let file = File::open(path).unwrap_or_else(|e| panic!("cannot open {}: {e}", path.display()));
    let mut reader = BufReader::new(file);
    let mut buf4 = [0u8; 4];

    reader
        .read_exact(&mut buf4)
        .expect("fvecs: failed to read dimension");
    let dim = i32::from_le_bytes(buf4) as usize;
    assert!(dim > 0 && dim <= 4096, "fvecs: suspicious dim={dim}");

    let file = File::open(path).unwrap();
    let file_size = file.metadata().unwrap().len() as usize;
    let record_size = 4 + dim * 4;
    assert!(
        file_size.is_multiple_of(record_size),
        "fvecs: file size {file_size} not a multiple of record_size {record_size}"
    );
    let n = file_size / record_size;

    let mut reader = BufReader::new(File::open(path).unwrap());
    let mut out = Vec::with_capacity(n * dim);
    let mut record_buf = vec![0u8; record_size];

    for _ in 0..n {
        reader
            .read_exact(&mut record_buf)
            .expect("fvecs read error");
        for i in 0..dim {
            let offset = 4 + i * 4;
            let v = f32::from_le_bytes(record_buf[offset..offset + 4].try_into().unwrap());
            out.push(v);
        }
    }

    (out, dim)
}

// ─── distance ────────────────────────────────────────────────────────────────

#[inline]
fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| (x - y) * (x - y)).sum()
}

// ─── ground truth ────────────────────────────────────────────────────────────

fn row_slice(vectors: &[f32], dim: usize, idx: usize) -> &[f32] {
    &vectors[idx * dim..(idx + 1) * dim]
}

fn exact_topk(corpus: &[f32], dim: usize, query: &[f32], k: usize) -> Vec<u32> {
    let n = corpus.len() / dim;
    let mut dists: Vec<(u32, f32)> = (0..n as u32)
        .map(|id| {
            let v = row_slice(corpus, dim, id as usize);
            (id, l2_sq(query, v))
        })
        .collect();
    let eff_k = k.min(n);
    if eff_k < dists.len() {
        dists.select_nth_unstable_by(eff_k - 1, |(_, a), (_, b)| a.total_cmp(b));
    }
    dists.truncate(eff_k);
    dists.sort_unstable_by(|(_, a), (_, b)| a.total_cmp(b));
    dists.into_iter().map(|(id, _)| id).collect()
}

fn compute_subset_gt(
    corpus: &[f32],
    queries: &[f32],
    dim: usize,
    n_queries: usize,
    k: usize,
) -> Vec<Vec<u32>> {
    (0..n_queries)
        .map(|qi| {
            let q = row_slice(queries, dim, qi);
            exact_topk(corpus, dim, q, k)
        })
        .collect()
}

fn recall_at_k(approx: &[(u32, f32)], gt: &[u32], k: usize) -> f64 {
    let gt_set: HashSet<u32> = gt.iter().take(k).copied().collect();
    let approx_ids: HashSet<u32> = approx.iter().take(k).map(|&(id, _)| id).collect();
    let overlap = gt_set.intersection(&approx_ids).count();
    overlap as f64 / k.min(gt_set.len()) as f64
}

// ─── build ───────────────────────────────────────────────────────────────────

fn build_graph(corpus: &[f32], dim: usize, alpha: f64) -> (VamanaGraph, f64) {
    std::env::set_var("KHIVE_BUILD_BATCH", BUILD_BATCH.to_string());
    let config = VamanaConfig::with_dimensions(dim)
        .with_max_degree(MAX_DEGREE)
        .with_search_list_size(SEARCH_LIST_SIZE)
        .with_alpha(alpha);

    let t0 = Instant::now();
    let graph = VamanaGraph::build(corpus, &config).expect("build failed");
    let build_ms = t0.elapsed().as_secs_f64() * 1000.0;
    (graph, build_ms)
}

// ─── search ──────────────────────────────────────────────────────────────────

fn search_with_beam(
    graph: &VamanaGraph,
    corpus: &[f32],
    dim: usize,
    query: &[f32],
    k: usize,
    beam: usize,
    visited: &mut VisitedSet,
) -> Vec<(u32, f32)> {
    let result = graph
        .greedy_search(corpus, dim, query, k, beam, visited, None)
        .expect("greedy_search failed");
    let mut pairs = result.results;
    pairs.sort_unstable_by(|(_, a), (_, b)| a.total_cmp(b));
    pairs
}

fn mean_recall_gt(
    graph: &VamanaGraph,
    corpus: &[f32],
    dim: usize,
    queries: &[f32],
    gt: &[Vec<u32>],
    beam: usize,
) -> f64 {
    let n = corpus.len() / dim;
    let nq = gt.len();
    let mut visited = VisitedSet::new(n);
    let mut total = 0.0f64;
    for (qi, gt_row) in gt.iter().enumerate() {
        let q = row_slice(queries, dim, qi);
        let res = search_with_beam(graph, corpus, dim, q, K, beam, &mut visited);
        total += recall_at_k(&res, gt_row, K);
    }
    total / nq as f64
}

fn find_iso_recall_beam(
    graph: &VamanaGraph,
    corpus: &[f32],
    dim: usize,
    queries: &[f32],
    gt: &[Vec<u32>],
) -> (usize, f64, bool) {
    let min_beam = K.max(MAX_DEGREE);
    let max_beam = MAX_ISO_BEAM;

    let recall_at_max = mean_recall_gt(graph, corpus, dim, queries, gt, max_beam);
    if recall_at_max < TARGET_RECALL {
        return (max_beam, recall_at_max, true);
    }

    let mut lo = min_beam;
    let mut hi = max_beam;
    let mut best_beam = max_beam;
    let mut best_recall = 0.0f64;

    while lo <= hi {
        let mid = (lo + hi) / 2;
        let r = mean_recall_gt(graph, corpus, dim, queries, gt, mid);
        if r >= TARGET_RECALL {
            best_beam = mid;
            best_recall = r;
            if mid == lo {
                break;
            }
            hi = mid - 1;
        } else {
            lo = mid + 1;
        }
    }

    (best_beam, best_recall, false)
}

// ─── latency (Change A: p50/p95/p99/max) ────────────────────────────────────

fn measure_warm_latency_us(
    graph: &VamanaGraph,
    corpus: &[f32],
    dim: usize,
    queries: &[f32],
    n_queries: usize,
    beam: usize,
) -> (f64, f64, f64, f64) {
    let n = corpus.len() / dim;
    let mut visited = VisitedSet::new(n);
    for qi in 0..n_queries {
        let q = row_slice(queries, dim, qi);
        let _ = search_with_beam(graph, corpus, dim, q, K, beam, &mut visited);
    }
    let mut samples: Vec<f64> = Vec::with_capacity(n_queries);
    for qi in 0..n_queries {
        let q = row_slice(queries, dim, qi);
        let t0 = Instant::now();
        let res = search_with_beam(graph, corpus, dim, q, K, beam, &mut visited);
        samples.push(t0.elapsed().as_secs_f64() * 1e6);
        std::hint::black_box(res);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = samples.len();
    let p50 = samples[n / 2];
    let p95 = samples[(n as f64 * 0.95) as usize].min(*samples.last().unwrap());
    let p99 = samples[(n as f64 * 0.99) as usize].min(*samples.last().unwrap());
    let max = *samples.last().unwrap();
    (p50, p95, p99, max)
}

fn measure_bruteforce_latency_us(
    corpus: &[f32],
    dim: usize,
    queries: &[f32],
    n_queries: usize,
) -> f64 {
    for qi in 0..n_queries {
        let q = row_slice(queries, dim, qi);
        std::hint::black_box(exact_topk(corpus, dim, q, K));
    }
    let mut samples: Vec<f64> = Vec::with_capacity(n_queries);
    for qi in 0..n_queries {
        let q = row_slice(queries, dim, qi);
        let t0 = Instant::now();
        let res = exact_topk(corpus, dim, q, K);
        samples.push(t0.elapsed().as_secs_f64() * 1e6);
        std::hint::black_box(res);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples[samples.len() / 2]
}

// ─── log-log OLS ─────────────────────────────────────────────────────────────

fn log_log_slope(xs: &[f64], ys: &[f64]) -> (f64, f64) {
    let pairs: Vec<(f64, f64)> = xs
        .iter()
        .zip(ys.iter())
        .filter(|(_, &y)| y > 0.0)
        .map(|(&x, &y)| (x.ln(), y.ln()))
        .collect();

    let n = pairs.len() as f64;
    if n < 2.0 {
        return (f64::NAN, f64::NAN);
    }

    let mean_x = pairs.iter().map(|(x, _)| x).sum::<f64>() / n;
    let mean_y = pairs.iter().map(|(_, y)| y).sum::<f64>() / n;
    let ss_xy: f64 = pairs.iter().map(|(x, y)| (x - mean_x) * (y - mean_y)).sum();
    let ss_xx: f64 = pairs.iter().map(|(x, _)| (x - mean_x) * (x - mean_x)).sum();

    if ss_xx.abs() < 1e-15 {
        return (f64::NAN, f64::NAN);
    }

    let slope = ss_xy / ss_xx;
    let intercept = mean_y - slope * mean_x;

    let ss_res: f64 = pairs
        .iter()
        .map(|(x, y)| {
            let pred = slope * x + intercept;
            (y - pred) * (y - pred)
        })
        .sum();
    let ss_tot: f64 = pairs.iter().map(|(_, y)| (y - mean_y) * (y - mean_y)).sum();

    let r2 = if ss_tot < 1e-15 {
        1.0
    } else {
        1.0 - ss_res / ss_tot
    };

    (slope, r2)
}

// ─── run metadata (Change B) ─────────────────────────────────────────────────

fn collect_produced_at() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, mo, d, h, mi, s) = unix_to_datetime(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn unix_to_datetime(mut secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let s = (secs % 60) as u32;
    secs /= 60;
    let mi = (secs % 60) as u32;
    secs /= 60;
    let h = (secs % 24) as u32;
    secs /= 24;
    let mut days = secs as u32;
    let mut year = 1970u32;
    loop {
        let dy = if is_leap(year) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        year += 1;
    }
    let month_days: &[u32] = if is_leap(year) {
        &[31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        &[31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 1u32;
    for &md in month_days {
        if days < md {
            break;
        }
        days -= md;
        month += 1;
    }
    (year, month, days + 1, h, mi, s)
}

fn is_leap(y: u32) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

fn collect_git_sha() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn collect_runner_os() -> String {
    if let Ok(v) = std::env::var("RUNNER_OS") {
        if !v.is_empty() {
            return v.to_lowercase();
        }
    }
    if cfg!(target_os = "macos") {
        // Detect arm64 vs x86_64
        let arch = std::process::Command::new("uname")
            .arg("-m")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        format!("macos-{arch}")
    } else if cfg!(target_os = "linux") {
        "linux-x86_64".to_string()
    } else {
        "unknown".to_string()
    }
}

fn collect_loadavg1() -> f64 {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok()))
            .unwrap_or(0.0)
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("sysctl")
            .args(["-n", "vm.loadavg"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| {
                let s = s.trim();
                // Format: "{ 1.23 2.34 3.45 }" or "1.23 2.34 3.45"
                let s = s.trim_start_matches('{').trim_end_matches('}').trim();
                s.split_whitespace().next().and_then(|v| v.parse().ok())
            })
            .unwrap_or(0.0)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0.0
    }
}

// ─── targets.toml parsing (Change C) ─────────────────────────────────────────

#[derive(Debug, Clone)]
struct CheckSpec {
    metric: String,
    scope: String,
    operator: String,
    threshold: f64,
    tolerance: f64,
}

fn load_targets(path: &Path, target_key: &str) -> Option<Vec<CheckSpec>> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "WARNING: could not read targets file {}: {e} — running without assertions",
                path.display()
            );
            return None;
        }
    };

    let doc: toml::Value = match content.parse() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("WARNING: failed to parse targets.toml: {e} — running without assertions");
            return None;
        }
    };

    let section = doc.get("scale-proof")?;
    let targets = section.get("target")?.as_array()?;
    for target in targets {
        let name = target.get("name")?.as_str()?;
        if name != target_key {
            continue;
        }
        let checks = target.get("check")?.as_array()?;
        let mut specs = Vec::new();
        for c in checks {
            let metric = c.get("metric")?.as_str()?.to_string();
            let scope = c.get("scope")?.as_str()?.to_string();
            let operator = c.get("operator")?.as_str()?.to_string();
            let threshold = c.get("threshold")?.as_float()?;
            let tolerance = c.get("tolerance").and_then(|v| v.as_float()).unwrap_or(0.0);
            specs.push(CheckSpec {
                metric,
                scope,
                operator,
                threshold,
                tolerance,
            });
        }
        return Some(specs);
    }

    eprintln!(
        "WARNING: target_key '{target_key}' not found in targets.toml — running without assertions"
    );
    None
}

fn evaluate_check(spec: &CheckSpec, rows: &[RowResult], fits: &FitsResult) -> (Value, Value) {
    let effective_threshold = match spec.operator.as_str() {
        "<=" => spec.threshold * (1.0 + spec.tolerance),
        ">=" => spec.threshold * (1.0 - spec.tolerance),
        "<" => spec.threshold,
        ">" => spec.threshold,
        _ => spec.threshold,
    };

    let (measured, pass) = match spec.scope.as_str() {
        "all_rows" => {
            let vals: Vec<f64> = rows
                .iter()
                .map(|r| get_row_metric(&spec.metric, r))
                .collect();
            let all_pass = vals
                .iter()
                .all(|&v| compare(v, &spec.operator, effective_threshold));
            (serde_json::to_value(&vals).unwrap(), all_pass)
        }
        "fits" => {
            let v = get_fits_metric(&spec.metric, fits);
            let pass = compare(v, &spec.operator, effective_threshold);
            (json!(v), pass)
        }
        "max_n" => {
            let v = rows
                .last()
                .map(|r| get_row_metric(&spec.metric, r))
                .unwrap_or(f64::NAN);
            let pass = compare(v, &spec.operator, effective_threshold);
            (json!(v), pass)
        }
        _ => (json!(null), false),
    };

    let result = if pass { "PASS" } else { "FAIL" };
    (measured, json!(result))
}

fn get_row_metric(metric: &str, row: &RowResult) -> f64 {
    match metric {
        "recall_at_10" => row.recall_at_10,
        "query_warm_p50_us" => row.query_warm_p50_us,
        "query_warm_p95_us" => row.query_warm_p95_us,
        "query_warm_p99_us" => row.query_warm_p99_us,
        "query_warm_max_us" => row.query_warm_max_us,
        "bruteforce_p50_us" => row.bruteforce_p50_us,
        "speedup_vs_brute_force" => row.speedup_vs_brute_force,
        "build_ms" => row.build_ms,
        _ => f64::NAN,
    }
}

fn get_fits_metric(metric: &str, fits: &FitsResult) -> f64 {
    match metric {
        "beam_growth_exponent" => fits.beam_growth_exponent,
        "beam_growth_r2" => fits.beam_growth_r2,
        "build_wallclock_exponent" => fits.build_wallclock_exponent,
        "build_wallclock_r2" => fits.build_wallclock_r2,
        "iso_recall_query_exponent_warm" => fits.iso_recall_query_exponent_warm,
        "iso_recall_query_r2_warm" => fits.iso_recall_query_r2_warm,
        "bruteforce_exponent" => fits.bruteforce_exponent,
        "bruteforce_r2" => fits.bruteforce_r2,
        _ => f64::NAN,
    }
}

fn compare(measured: f64, operator: &str, threshold: f64) -> bool {
    if measured.is_nan() {
        return false;
    }
    match operator {
        "<=" => measured <= threshold,
        ">=" => measured >= threshold,
        "<" => measured < threshold,
        ">" => measured > threshold,
        "==" => (measured - threshold).abs() < 1e-9,
        _ => false,
    }
}

// ─── result types ────────────────────────────────────────────────────────────

struct RowResult {
    n: usize,
    build_ms: f64,
    iso_recall_beam: usize,
    recall_at_10: f64,
    recall_saturated: bool,
    query_warm_p50_us: f64,
    query_warm_p95_us: f64,
    query_warm_p99_us: f64,
    query_warm_max_us: f64,
    bruteforce_p50_us: f64,
    speedup_vs_brute_force: f64,
    per_query_resident_exceeds_llc: bool,
}

#[derive(Serialize)]
struct FitsResult {
    note: String,
    beam_growth_exponent: f64,
    beam_growth_r2: f64,
    build_wallclock_exponent: f64,
    build_wallclock_r2: f64,
    build_work_exponent: Option<f64>,
    build_work_r2: Option<f64>,
    iso_recall_query_exponent_warm: f64,
    iso_recall_query_r2_warm: f64,
    bruteforce_exponent: f64,
    bruteforce_r2: f64,
}

// ─── main ────────────────────────────────────────────────────────────────────

fn main() {
    let args = parse_args();

    let n_cap: usize = std::env::var("KHIVE_N_CAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300_000);
    for &n in &args.ns {
        assert!(
            n <= n_cap,
            "N={n} exceeds the {n_cap} cap — set KHIVE_N_CAP={n} or higher."
        );
    }

    // Change B: collect run metadata at start.
    let produced_at = collect_produced_at();
    let git_sha = collect_git_sha();
    let runner_os = collect_runner_os();
    let loadavg1 = collect_loadavg1();

    let targets_path = args
        .targets
        .clone()
        .unwrap_or_else(|| PathBuf::from("perf/targets.toml"));
    let check_specs = load_targets(&targets_path, &args.target_key);

    println!("=== Vamana Scale-Proof Bench (schema v1.0) ===");
    println!("dataset: {}", args.dataset);
    println!("base: {}", args.base_path.display());
    println!("query: {}", args.query_path.display());
    println!(
        "config: R={MAX_DEGREE}, L={SEARCH_LIST_SIZE}, alpha={}, batch={BUILD_BATCH}, k={K}, target_recall={TARGET_RECALL}",
        args.alpha
    );
    println!("N values: {:?}", args.ns);
    println!("produced_at: {produced_at}  git_sha: {git_sha}  runner_os: {runner_os}  loadavg1: {loadavg1:.2}");
    println!();

    print!("Loading base vectors... ");
    let t0 = Instant::now();
    let (base_all, dim) = load_fvecs(&args.base_path);
    let n_base = base_all.len() / dim;
    println!(
        "{:.1}ms  ({} vectors, dim={})",
        t0.elapsed().as_secs_f64() * 1000.0,
        n_base,
        dim
    );

    print!("Loading query vectors... ");
    let t0 = Instant::now();
    let (query_all, query_dim) = load_fvecs(&args.query_path);
    let n_query = query_all.len() / query_dim;
    println!(
        "{:.1}ms  ({} vectors, dim={})",
        t0.elapsed().as_secs_f64() * 1000.0,
        n_query,
        query_dim
    );
    assert_eq!(dim, query_dim, "base dim={dim} != query dim={query_dim}");

    let n_gt_queries = GT_QUERY_SAMPLE.min(n_query);
    let n_lat = LATENCY_QUERY_SAMPLE.min(n_query);
    println!("Using {n_gt_queries} queries for GT + recall; {n_lat} for latency.");
    println!();

    let mut rows: Vec<RowResult> = Vec::new();

    for &n in &args.ns {
        assert!(n <= n_base, "N={n} > base size={n_base}");
        println!("=== N={n} ===");

        let corpus: Vec<f32> = base_all[..n * dim].to_vec();

        print!("  Recomputing brute-force GT@{K} for {n_gt_queries} queries on N={n}... ");
        let t0 = Instant::now();
        let gt = compute_subset_gt(&corpus, &query_all, dim, n_gt_queries, K);
        println!("{:.1}ms", t0.elapsed().as_secs_f64() * 1000.0);

        print!("  Building Vamana graph (N={n})... ");
        let (graph, build_ms) = build_graph(&corpus, dim, args.alpha);
        println!("{build_ms:.1}ms");

        print!("  Finding iso-recall beam (target={TARGET_RECALL}, cap={MAX_ISO_BEAM})... ");
        let (iso_beam, achieved_recall, recall_saturated) =
            find_iso_recall_beam(&graph, &corpus, dim, &query_all, &gt);
        if recall_saturated {
            println!("beam={iso_beam} (CAP), recall@{K}={achieved_recall:.4} BELOW TARGET");
        } else {
            println!("beam={iso_beam}, recall@{K}={achieved_recall:.4}");
        }

        // Change A: p50/p95/p99/max
        print!("  Measuring warm latency ({n_lat} queries, beam={iso_beam})... ");
        let (warm_p50, warm_p95, warm_p99, warm_max) =
            measure_warm_latency_us(&graph, &corpus, dim, &query_all, n_lat, iso_beam);
        println!(
            "p50={warm_p50:.1}us, p95={warm_p95:.1}us, p99={warm_p99:.1}us, max={warm_max:.1}us"
        );

        print!("  Measuring brute-force latency ({n_lat} queries)... ");
        let bf_p50 = measure_bruteforce_latency_us(&corpus, dim, &query_all, n_lat);
        println!("p50={bf_p50:.1}us");

        let speedup = bf_p50 / warm_p50;
        println!(
            "  Speedup: {speedup:.2}x  (ANN {} brute-force)",
            if speedup > 1.0 {
                "BEATS"
            } else {
                "SLOWER THAN"
            }
        );

        let vec_bytes = (dim * 4) as u64;
        let adj = graph.adjacency();
        let avg_degree = adj.iter().map(|v| v.len()).sum::<usize>() as f64 / n as f64;
        let edge_bytes = (avg_degree * 4.0) as u64;
        let bytes_per_node = vec_bytes + edge_bytes;

        let mut visited_set = VisitedSet::new(n);
        let q0 = row_slice(&query_all, dim, 0);
        let r0 = graph
            .greedy_search(&corpus, dim, q0, K, iso_beam, &mut visited_set, None)
            .unwrap();
        let avg_visited = r0.expanded.len() as u64;
        let resident_bytes = avg_visited * bytes_per_node;
        let exceeds_llc = resident_bytes > LLC_SIZE_BYTES;
        println!(
            "  avg degree: {avg_degree:.1}, sample visited: {avg_visited}, resident~{resident_bytes}B ({} LLC)",
            if exceeds_llc { "EXCEEDS" } else { "within" }
        );
        println!();

        rows.push(RowResult {
            n,
            build_ms,
            iso_recall_beam: iso_beam,
            recall_at_10: achieved_recall,
            recall_saturated,
            query_warm_p50_us: warm_p50,
            query_warm_p95_us: warm_p95,
            query_warm_p99_us: warm_p99,
            query_warm_max_us: warm_max,
            bruteforce_p50_us: bf_p50,
            speedup_vs_brute_force: speedup,
            per_query_resident_exceeds_llc: exceeds_llc,
        });
    }

    // ─── exponent fits ─────────────────────────────────────────────────────

    let ns_f: Vec<f64> = rows.iter().map(|r| r.n as f64).collect();
    let beam_vals: Vec<f64> = rows.iter().map(|r| r.iso_recall_beam as f64).collect();
    let build_vals: Vec<f64> = rows.iter().map(|r| r.build_ms).collect();
    let warm_vals: Vec<f64> = rows.iter().map(|r| r.query_warm_p50_us).collect();
    let bf_vals: Vec<f64> = rows.iter().map(|r| r.bruteforce_p50_us).collect();

    let (beam_exp, beam_r2) = log_log_slope(&ns_f, &beam_vals);
    let (build_exp, build_r2) = log_log_slope(&ns_f, &build_vals);
    let (query_exp, query_r2) = log_log_slope(&ns_f, &warm_vals);
    let (bf_exp, bf_r2) = log_log_slope(&ns_f, &bf_vals);

    let n_pts = rows.len();
    let fit_note = format!("{n_pts}-point fit");

    let fits = FitsResult {
        note: fit_note.clone(),
        beam_growth_exponent: beam_exp,
        beam_growth_r2: beam_r2,
        build_wallclock_exponent: build_exp,
        build_wallclock_r2: build_r2,
        build_work_exponent: None,
        build_work_r2: None,
        iso_recall_query_exponent_warm: query_exp,
        iso_recall_query_r2_warm: query_r2,
        bruteforce_exponent: bf_exp,
        bruteforce_r2: bf_r2,
    };

    // ─── summary table ─────────────────────────────────────────────────────

    println!("=== Summary Table ===");
    println!(
        "{:<10} {:>10} {:>10} {:>9} {:>10} {:>10} {:>10} {:>10} {:>12} {:>9}",
        "N",
        "build_ms",
        "iso_beam",
        "recall10",
        "warm_p50us",
        "warm_p95us",
        "warm_p99us",
        "warm_maxus",
        "bf_p50us",
        "speedup"
    );
    println!("{}", "-".repeat(110));
    for r in &rows {
        println!(
            "{:<10} {:>10.0} {:>10} {:>9.4} {:>10.1} {:>10.1} {:>10.1} {:>10.1} {:>12.1} {:>8.2}x",
            r.n,
            r.build_ms,
            r.iso_recall_beam,
            r.recall_at_10,
            r.query_warm_p50_us,
            r.query_warm_p95_us,
            r.query_warm_p99_us,
            r.query_warm_max_us,
            r.bruteforce_p50_us,
            r.speedup_vs_brute_force,
        );
    }
    println!();
    println!("=== Fitted Exponents ({fit_note}) ===");
    println!("  beam_growth_exponent:      {beam_exp:.4} (R²={beam_r2:.4})");
    println!("  build_wallclock_exponent:  {build_exp:.4} (R²={build_r2:.4})");
    println!("  iso_recall_query_exponent: {query_exp:.4} (R²={query_r2:.4})");
    println!("  brute_force_exponent:      {bf_exp:.4} (R²={bf_r2:.4})");
    println!();

    // ─── decisive question ─────────────────────────────────────────────────

    let recall_holds = rows
        .iter()
        .all(|r| r.recall_at_10 >= TARGET_RECALL && !r.recall_saturated);
    let speedup_at_max_n = rows.last().map(|r| r.speedup_vs_brute_force).unwrap_or(0.0);
    let max_n = rows.last().map(|r| r.n).unwrap_or(0);
    let beats_brute_force = speedup_at_max_n > 1.0;
    let beam_flat = !beam_exp.is_nan() && beam_exp < 0.5;
    let query_sublinear = !query_exp.is_nan() && query_exp < 0.8;

    let all_criteria = recall_holds && beats_brute_force && beam_flat && query_sublinear;
    let verdict = if all_criteria {
        "YES — all criteria met: beam flat, query sublinear, recall >=0.95, ANN beats brute force"
            .to_string()
    } else {
        format!(
            "PARTIAL/NO — beam_flat={beam_flat}, query_sublinear={query_sublinear}, recall_holds={recall_holds}, beats_brute_force={beats_brute_force}"
        )
    };

    println!("=== DECISIVE QUESTION ===");
    println!(
        "(a) beam flat (exp<0.5): {} ({beam_exp:.4})",
        if beam_flat { "YES" } else { "NO" }
    );
    println!(
        "(b) query sublinear (exp<0.8): {} ({query_exp:.4})",
        if query_sublinear { "YES" } else { "NO" }
    );
    println!(
        "(c) recall >=0.95 at all N: {}",
        if recall_holds { "YES" } else { "NO" }
    );
    println!(
        "(d) ANN beats brute-force at max N: {} ({speedup_at_max_n:.2}x at N={max_n})",
        if beats_brute_force { "YES" } else { "NO" }
    );
    println!("VERDICT: {verdict}");
    println!();

    // ─── Build caveats ─────────────────────────────────────────────────────

    let mut caveats: Vec<String> = vec![
        format!("{fit_note}"),
        "GT recomputed by brute-force L2 on each subset (provided GT indexes full base — invalid for subsets)".into(),
        "build exponent is Omega(N) floor — sublinear claim is query+update only".into(),
        "latency measured warm-cache on single-node; cold-cache latency is higher".into(),
    ];
    if loadavg1 > 4.0 {
        caveats.push(format!("high-load run: loadavg1={loadavg1:.2}"));
    }

    // ─── Change C: assertions ──────────────────────────────────────────────

    let targets_file_str = targets_path.display().to_string();
    let assertions_value = build_assertions(
        &check_specs,
        &rows,
        &fits,
        &targets_file_str,
        &args.target_key,
    );
    let exit_code = if assertions_value["overall"] == "PASS" {
        0
    } else {
        1
    };

    // ─── Serialize rows to JSON ────────────────────────────────────────────

    let rows_json: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "n": r.n,
                "build_ms": r.build_ms,
                "iso_recall_beam": r.iso_recall_beam,
                "recall_at_10": r.recall_at_10,
                "recall_saturated": r.recall_saturated,
                "query_warm_p50_us": r.query_warm_p50_us,
                "query_warm_p95_us": r.query_warm_p95_us,
                "query_warm_p99_us": r.query_warm_p99_us,
                "query_warm_max_us": r.query_warm_max_us,
                "bruteforce_p50_us": r.bruteforce_p50_us,
                "speedup_vs_brute_force": r.speedup_vs_brute_force,
                "build_distance_calls": null,
                "query_flushed_p50_us": null,
                "per_query_resident_bytes": null,
                "per_query_resident_exceeds_llc": r.per_query_resident_exceeds_llc,
                "gt_source": "brute-force recomputed on subset",
            })
        })
        .collect();

    let per_n_recall: Vec<Value> = rows
        .iter()
        .map(|r| json!({"n": r.n, "recall": r.recall_at_10, "saturated": r.recall_saturated}))
        .collect();

    let seed_value: Value = json!(null);
    let base_file_value: Value = json!(args.base_path.display().to_string());
    let query_file_value: Value = json!(args.query_path.display().to_string());
    let source_url_value: Value = args
        .source_url
        .as_deref()
        .map(|u| json!(u))
        .unwrap_or(json!(null));
    let intrinsic_dim_value: Value = if args.intrinsic_dim > 0.0 {
        json!(args.intrinsic_dim)
    } else {
        json!(null)
    };

    let output = json!({
        "schema_version": "1.0",
        "produced_at": produced_at,
        "git_sha": git_sha,
        "runner_os": runner_os,
        "loadavg1": loadavg1,
        "dataset": {
            "name": args.dataset,
            "dim": dim,
            "base_n": n_base,
            "query_n": n_query,
            "intrinsic_dim_approx": intrinsic_dim_value,
            "normalization": args.normalization,
            "seed": seed_value,
            "base_file": base_file_value,
            "query_file": query_file_value,
            "source_url": source_url_value,
        },
        "config": {
            "max_degree": MAX_DEGREE,
            "search_list_size": SEARCH_LIST_SIZE,
            "alpha": args.alpha,
            "build_batch": BUILD_BATCH,
            "k": K,
            "target_recall": TARGET_RECALL,
            "max_iso_beam": MAX_ISO_BEAM,
            "n_gt_queries": n_gt_queries,
            "n_latency_queries": n_lat,
        },
        "gt_policy": {
            "source": "brute-force recomputed on subset",
            "note": "any provided GT file indexes the full base and is invalid for subsets",
        },
        "rows": rows_json,
        "fits": fits,
        "decisive_question": {
            "verdict": verdict,
            "criteria": {
                "a_beam_flat": beam_flat,
                "a_beam_exp": beam_exp,
                "b_query_sublinear": query_sublinear,
                "b_query_exp": query_exp,
                "c_recall_holds": recall_holds,
                "d_beats_brute_force": beats_brute_force,
                "d_speedup_at_max_n": speedup_at_max_n,
                "d_max_n": max_n,
                "per_n_recall": per_n_recall,
            },
        },
        "assertions": assertions_value,
        "caveats": caveats,
    });

    if let Some(parent) = args.out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }

    let json_str = serde_json::to_string_pretty(&output).expect("json serialization failed");
    // Always write JSON BEFORE process::exit so failures are inspectable.
    std::fs::write(&args.out, &json_str).expect("failed to write JSON");
    println!("JSON written to: {}", args.out.display());
    println!("assertions.overall: {}", output["assertions"]["overall"]);
    println!("exit_code: {exit_code}");

    std::process::exit(exit_code);
}

// ─── assertions builder ───────────────────────────────────────────────────────

fn build_assertions(
    check_specs: &Option<Vec<CheckSpec>>,
    rows: &[RowResult],
    fits: &FitsResult,
    targets_file: &str,
    target_key: &str,
) -> Value {
    let checks_json: Vec<Value> = match check_specs {
        None => vec![],
        Some(specs) => specs
            .iter()
            .map(|spec| {
                let (measured, result) = evaluate_check(spec, rows, fits);
                json!({
                    "metric": spec.metric,
                    "scope": spec.scope,
                    "operator": spec.operator,
                    "threshold": spec.threshold,
                    "tolerance": spec.tolerance,
                    "result": result,
                    "measured": measured,
                })
            })
            .collect(),
    };

    let overall = if check_specs.is_none() {
        "SKIPPED"
    } else if checks_json.iter().all(|c| c["result"] == "PASS") {
        "PASS"
    } else {
        "FAIL"
    };

    let exit_code = if overall == "PASS" { 0 } else { 1 };

    json!({
        "targets_file": targets_file,
        "target_key": target_key,
        "checks": checks_json,
        "overall": overall,
        "exit_code": exit_code,
    })
}
