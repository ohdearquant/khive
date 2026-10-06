use khive_quant::{GsEncodedVector, GsSq8Codec};

use crate::{
    config::VamanaConfig,
    error::{Result, VamanaError},
    graph::VamanaGraph,
};

use super::{CodeStore, SearchVisitedPool, VectorStorage};

/// An in-memory Vamana ANN index over pre-normalized vectors.
#[derive(Debug)]
pub struct VamanaIndex {
    pub(super) vectors: VectorStorage,
    pub(super) graph: VamanaGraph,
    pub(super) config: VamanaConfig,
    pub(super) num_vectors: usize,
    pub(super) dimensions: usize,
    pub(super) search_visited: SearchVisitedPool,
    // ---- PR2: lifecycle fields (ADR-052 §2; see docs/design.md#lifecycle-fields) ----
    /// Bit-packed tombstone marks. Bit `i` set ⇒ node `i` is soft-deleted.
    pub(super) tombstones: Vec<u64>,
    /// Count of currently tombstoned nodes.
    pub(super) tombstone_count: usize,
    /// Cumulative delete+insert churn since the last consolidation.
    pub(super) ops_since_consolidation: usize,
    /// Recycled ordinal slots from previous tombstone calls; consumed by insert (PR3).
    pub(super) free_slots: Vec<u32>,
    /// Trigger tau: consolidation fires when `ops_since_consolidation >= consolidation_tau`.
    pub(super) consolidation_tau: usize,
    // ---- SQ8 acquisition tier (ADR-052 §1, Step 2) ----
    /// Global-scale SQ8 codec trained over the build corpus; used for acquisition-tier distances.
    pub(super) gs_codec: GsSq8Codec,
    /// Pre-encoded corpus vectors, ordinal-stable (owned or mmap `codes.bin`).
    pub(super) gs_codes: CodeStore,
    /// External write-log watermark carried in the v2 commit record. `None` on
    /// indexes built or loaded from segments that predate the field; the
    /// storage layer that owns the log sets it before `save_atomic` and reads
    /// it back after load to classify restart state.
    pub(super) last_applied_seq: Option<u64>,
}

pub(super) struct IndexMetadata {
    pub(super) num_vectors: usize,
    pub(super) dimensions: usize,
    pub(super) max_degree: usize,
    pub(super) search_list_size: usize,
    pub(super) alpha: f64,
}

/// Train + encode the SQ8 acquisition-tier codec; called by every index constructor.
pub(super) fn train_codec_and_encode(
    vectors: &[f32],
    dims: usize,
) -> (GsSq8Codec, Vec<GsEncodedVector>) {
    let codec = GsSq8Codec::train_flat(vectors, dims);
    let codes = codec.encode_flat_par(vectors, dims);
    (codec, codes)
}

/// Scan a flat f32 slice for any non-finite value, returning an error on the first hit.
pub(super) fn require_finite(values: &[f32], location: &str) -> Result<()> {
    for (i, v) in values.iter().enumerate() {
        if !v.is_finite() {
            return Err(VamanaError::non_finite(location, format!("index {i}: {v}")));
        }
    }
    Ok(())
}

/// Threads an index build may use. Half the machine's parallelism by default,
/// at least one, overridable with `KHIVE_ANN_BUILD_THREADS`.
///
/// A build is not the only thing the machine is doing. On the default rayon pool
/// it takes every core, so a build triggered while requests are in flight
/// competes with the process serving them — and on a host where several
/// processes share one index root, with the other builds too. Half leaves the
/// machine responsive and costs build wall-clock, which is the right trade for
/// work that is supposed to happen rarely.
#[cfg(feature = "parallel")]
pub(super) fn build_thread_count() -> usize {
    resolve_build_threads(
        std::env::var("KHIVE_ANN_BUILD_THREADS").ok().as_deref(),
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    )
}

/// Pure half of [`build_thread_count`], so the policy is testable without
/// mutating process environment. A malformed or zero override falls back to the
/// default rather than failing: this knob bounds a cost, it does not gate
/// correctness, and a typo in it must not stop an index from building.
#[cfg(feature = "parallel")]
pub(super) fn resolve_build_threads(override_value: Option<&str>, available: usize) -> usize {
    if let Some(threads) = override_value
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|threads| *threads > 0)
    {
        return threads;
    }
    available.div_ceil(2).max(1)
}

/// The bounded pool index builds run in. `None` if the pool could not be built,
/// in which case the build runs on the caller's thread pool as it did before —
/// a failure to bound parallelism is not a reason to fail the build.
#[cfg(feature = "parallel")]
pub(super) fn build_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(build_thread_count())
            .thread_name(|i| format!("khive-ann-build-{i}"))
            .build()
            .ok()
    })
    .as_ref()
}
