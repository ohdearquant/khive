//! ADR-135 F4 release-gate: per-shape writer hold-time regression coverage
//! for the coalesced comm dual-write unit (#1565).
//!
//! ADR-135's "Consequences" section states operation-local coalescing "can
//! lengthen each lock hold and correlate failures, so per-shape hold-time
//! and rollback tests become release requirements" (issue #1609). Rollback
//! fault-injection coverage for the dual-write unit already exists
//! (`crates/khive-pack-comm/src/message.rs` inline tests). This file adds
//! the missing hold-time half: it measures wall-clock latency of each
//! coalesced write shape and fails the gate if a shape regresses past a
//! calibrated bound, rather than only reporting a number.
//!
//! Shapes covered (the two `dual_write_message` callers — #1565):
//! - `comm.send` to a new root (outbound + inbound copies committed in one
//!   atomic writer transaction; the canonical thread_id is generated before
//!   either write, not patched in afterwards)
//! - `comm.reply` (same one-transaction dual write, plus the reply path's
//!   parent `get_note` read and header-threading computation, which run
//!   before the dispatch call returns and are part of this shape's cost)
//!
//! # Methodology
//!
//! The runtime under test is `KhiveRuntime::memory()`: in-memory SQLite,
//! zero embedding models configured (`RuntimeConfig::no_embeddings()`). With
//! no embedding models, `create_note_inner` performs exactly two DB writes
//! per note (note upsert + FTS document upsert) and zero suspending/external
//! compute in between — so end-to-end wall-clock latency of the verb call is
//! a tight proxy for aggregate writer-transaction hold time for that shape,
//! uncontaminated by embedding compute (the confound ADR-135 F4 explicitly
//! calls out: "Embedding computation ... must be completed before `BEGIN
//! IMMEDIATE`"). It does not isolate SQLite's own internal
//! `BEGIN`/`COMMIT` interval from surrounding Rust-side validation and
//! serialization work — no such per-statement instrumentation exists on
//! `main` at time of writing (#1609 grep of `khive-db`/`khive-storage` found
//! no `hold_time`/`Instant` bracketing around `atomic_unit` or the writer
//! checkout) — but it is the honest, currently-available measurement, and
//! it is exactly what regresses if a future change adds more writer work to
//! a shape (which is the failure mode this gate exists to catch).
//!
//! Each shape is sampled `SAMPLES` times after `WARMUP` untimed iterations
//! (first-call allocator/index warmup). The gate asserts on the **median**
//! (resistant to a single scheduler-preemption outlier) and on **p95**
//! (catches a shape whose tail — not just typical case — regressed). Both
//! bounds are `calibrated median/p95 x SAFETY_FACTOR`, calibrated from a
//! real run on 2026-08-01 (see the constants below) — not invented numbers.
//! `SAFETY_FACTOR` is deliberately large (10x) per the issue's own guidance
//! ("prefer a generous absolute threshold ... over a brittle ratchet"): this
//! gate exists to catch a shape that got structurally heavier (e.g. an
//! accidentally-reintroduced extra writer transaction, or a lost
//! coalescing optimization), not to police routine CI scheduling jitter.
//!
//! If a shape's own measured stats look pathological (a sample count short
//! of `SAMPLES`, or a non-finite duration), the test fails loudly with the
//! collected stats rather than silently passing — there is no skip path.

use std::time::{Duration, Instant};

use khive_pack_comm::CommPack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use serde_json::json;

const WARMUP: usize = 5;
const SAMPLES: usize = 40;

/// Calibration safety factor applied to both the measured median and p95
/// bounds below. See module doc: generous-absolute over brittle-ratchet.
const SAFETY_FACTOR: f64 = 10.0;

/// Calibrated 2026-08-01 (`cargo test -p khive-pack-comm --test
/// hold_time_regression -- --nocapture`, unoptimized `test` profile — the
/// same profile the gate itself runs under, so the bound and the measured
/// value share a build config): comm.send (new-root) observed median
/// 767-841us, p95 916us-1.06ms across 4 runs. Base calibration rounds the
/// observed max up: median 850us, p95 1.1ms. See
/// `.khive/IMPL_REPORT_1609.md` for the raw run-by-run output.
const SEND_MEDIAN_BOUND: Duration = Duration::from_micros(850);
const SEND_P95_BOUND: Duration = Duration::from_micros(1_100);

/// Calibrated 2026-08-02, same method, after the timed closure was reduced
/// to exactly one `comm.reply` dispatch (the root fixture moved outside the
/// timer): observed median 733-936us, p95 950us-1.22ms across 4 runs under
/// a machine-wide bench-window lock (1-min load average 15-17, disclosed
/// per fleet bench discipline). Reply sits slightly above send: the reply
/// path's extra parent `get_note` read plus header-threading computation
/// run inside the timed dispatch. Base calibration rounds the observed max
/// up: median 950us, p95 1.25ms.
const REPLY_MEDIAN_BOUND: Duration = Duration::from_micros(950);
const REPLY_P95_BOUND: Duration = Duration::from_micros(1_250);

fn build_registry() -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    (registry, runtime)
}

/// Nearest-rank median (p50) and p95 over `durations`: rank
/// `ceil(n * q) - 1` after sorting, for both quantiles — for 40 samples
/// that is index 19 (median) and index 37 (p95). Panics on an empty slice
/// — callers must assert `durations.len() == SAMPLES` first so a short
/// collection fails on that explicit assertion, not silently here.
fn median_and_p95(durations: &mut [Duration]) -> (Duration, Duration) {
    assert!(
        !durations.is_empty(),
        "median_and_p95: no samples collected"
    );
    durations.sort_unstable();
    let nearest_rank = |q: f64| {
        let index = ((durations.len() as f64) * q).ceil() as usize - 1;
        durations[index.min(durations.len() - 1)]
    };
    (nearest_rank(0.50), nearest_rank(0.95))
}

fn scale_bound(bound: Duration, factor: f64) -> Duration {
    Duration::from_secs_f64(bound.as_secs_f64() * factor)
}

/// Runs `iters` untimed warmup calls, then `SAMPLES` timed calls of `op`,
/// returning the per-call wall-clock durations. `op` must perform exactly
/// one dispatch of the shape under measurement — no setup/teardown inside
/// the timed closure.
async fn sample_shape<F, Fut>(mut op: F) -> Vec<Duration>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    for _ in 0..WARMUP {
        op().await;
    }
    let mut durations = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let start = Instant::now();
        op().await;
        durations.push(start.elapsed());
    }
    durations
}

#[tokio::test]
async fn hold_time_regression_comm_send_new_root() {
    let (registry, _rt) = build_registry();

    let mut durations = sample_shape(|| {
        let registry = &registry;
        async move {
            registry
                .dispatch(
                    "comm.send",
                    json!({ "to": "local", "content": "hold-time gate probe" }),
                )
                .await
                .expect("comm.send succeeds");
        }
    })
    .await;

    assert_eq!(
        durations.len(),
        SAMPLES,
        "hold-time gate: comm.send did not collect {SAMPLES} samples (read failure, not a \
         regression verdict) — durations: {durations:?}"
    );

    let (median, p95) = median_and_p95(&mut durations);
    let median_bound = scale_bound(SEND_MEDIAN_BOUND, SAFETY_FACTOR);
    let p95_bound = scale_bound(SEND_P95_BOUND, SAFETY_FACTOR);

    assert!(
        median <= median_bound,
        "hold-time regression: comm.send median writer-hold proxy {median:?} exceeds \
         calibrated bound {median_bound:?} ({SEND_MEDIAN_BOUND:?} x {SAFETY_FACTOR}); \
         p95 was {p95:?}; all samples: {durations:?}"
    );
    assert!(
        p95 <= p95_bound,
        "hold-time regression: comm.send p95 writer-hold proxy {p95:?} exceeds calibrated \
         bound {p95_bound:?} ({SEND_P95_BOUND:?} x {SAFETY_FACTOR}); median was {median:?}; \
         all samples: {durations:?}"
    );

    eprintln!(
        "hold-time gate: comm.send (new-root) median={median:?} p95={p95:?} \
         (bounds: median<={median_bound:?} p95<={p95_bound:?})"
    );
}

#[tokio::test]
async fn hold_time_regression_comm_reply() {
    let (registry, _rt) = build_registry();

    // Seed one root OUTSIDE the timed region: the timed closure below must
    // dispatch exactly one comm.reply and nothing else, so the reply shape's
    // calibration is not coupled to comm.send performance.
    let root = registry
        .dispatch(
            "comm.send",
            json!({ "to": "local", "content": "hold-time gate reply root" }),
        )
        .await
        .expect("comm.send (reply root fixture) succeeds");
    let root_id = root
        .get("full_id")
        .and_then(|v| v.as_str())
        .expect("comm.send returns full_id")
        .to_string();

    let mut durations = sample_shape(|| {
        let registry = &registry;
        let root_id = root_id.clone();
        async move {
            registry
                .dispatch(
                    "comm.reply",
                    json!({ "id": root_id, "content": "hold-time gate reply probe" }),
                )
                .await
                .expect("comm.reply succeeds");
        }
    })
    .await;

    assert_eq!(
        durations.len(),
        SAMPLES,
        "hold-time gate: comm.reply did not collect {SAMPLES} samples (read failure, not a \
         regression verdict) — durations: {durations:?}"
    );

    let (median, p95) = median_and_p95(&mut durations);
    let median_bound = scale_bound(REPLY_MEDIAN_BOUND, SAFETY_FACTOR);
    let p95_bound = scale_bound(REPLY_P95_BOUND, SAFETY_FACTOR);

    assert!(
        median <= median_bound,
        "hold-time regression: comm.reply median writer-hold proxy {median:?} exceeds \
         calibrated bound {median_bound:?} ({REPLY_MEDIAN_BOUND:?} x {SAFETY_FACTOR}); \
         p95 was {p95:?}; all samples: {durations:?}"
    );
    assert!(
        p95 <= p95_bound,
        "hold-time regression: comm.reply p95 writer-hold proxy {p95:?} exceeds calibrated \
         bound {p95_bound:?} ({REPLY_P95_BOUND:?} x {SAFETY_FACTOR}); median was {median:?}; \
         all samples: {durations:?}"
    );

    eprintln!(
        "hold-time gate: comm.reply median={median:?} p95={p95:?} \
         (bounds: median<={median_bound:?} p95<={p95_bound:?})"
    );
}
