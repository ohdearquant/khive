// FILE SIZE JUSTIFICATION: This module exceeds the 700-line soft target because it owns
// the complete Vamana ANN lifecycle for knowledge search: SharedAnn type, AnnKey, snapshot
// persistence (warm_known_snapshots / ensure_ann_background), index build (build_ann),
// search (search_loaded), and all associated SQL queries and serialization logic. These
// responsibilities are tightly coupled through the shared AnnState and cannot be split
// without breaking the atomic lock protocol. Refactoring is deferred until
// a stable snapshot format and the warm-start contract are defined.

//! Vamana ANN bridge — parallel semantic signal for `knowledge.search`.
//!
//! Wraps `khive_vamana::VamanaIndex` with an ID map (u32 → UUID) so search
//! results can be fused with FTS5 candidates via RRF.
//!
//! Persistence (ADR-079): v2 binary segments are written to `data_dir/ann/<hex>/`
//! on every cold-start rebuild or explicit reindex.  `ensure_ann_for_model` checks
//! the v2 segment directory first (content-hash gated), falling back to legacy v1
//! JSON rows in `retrieval_snapshots` for in-place upgrades, then rebuilds from the
//! full sqlite-vec corpus on cache-miss.  `kkernel reindex` re-persists v2 segments
//! and calls `invalidate_snapshot` to clean up stale v1 rows.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_vamana::{
    corpus_content_hash, read_commit_fingerprint, CorpusFingerprint, VamanaConfig, VamanaIndex,
    VamanaSnapshot,
};
use tokio::sync::RwLock;
use uuid::Uuid;

pub(crate) struct AnnBridge {
    index: VamanaIndex,
    id_map: Vec<Uuid>,
    /// Namespace write-generation this build's corpus scan started at or after
    /// (issue #770). Stamped just before install; `install_if_fresher` uses it
    /// to reject a late-arriving build whose scan predates a `clear_namespace`
    /// invalidation that landed while it was still running.
    generation: u64,
}

/// Cache key for a per-{namespace, model} ANN index slot.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct AnnKey {
    namespace: String,
    model: String,
}

impl AnnKey {
    pub(crate) fn new(namespace: &str, model: &str) -> Self {
        Self {
            namespace: namespace.to_owned(),
            model: model.to_owned(),
        }
    }
}

/// Shared ANN state: per-{namespace, model} indexes plus a single-flight guard
/// so at most one background warm runs per key at a time.
pub(crate) struct AnnState {
    indexes: RwLock<HashMap<AnnKey, AnnBridge>>,
    /// Keys currently being warmed (or already warmed). `std::sync::Mutex`
    /// so the fire-and-check guard in `ensure_ann_background` stays sync.
    warming: std::sync::Mutex<HashSet<AnnKey>>,
    /// Per-namespace write-generation counter (issue #770). Bumped by
    /// `clear_namespace` whenever a corpus mutation invalidates a namespace's
    /// ANN slots. `ensure_ann_for_model` captures the current value for its
    /// namespace before doing anything else — including before its own
    /// "already loaded" fast path and before the corpus scan — and stamps it
    /// on the resulting `AnnBridge`. `install_if_fresher` then only replaces
    /// an already-installed entry when the candidate's generation is >= the
    /// installed entry's, instead of the old `entry(key).or_insert(...)`,
    /// which always kept whichever build reached the install site first even
    /// if it had scanned a corpus version predating a later invalidation.
    /// Keyed by namespace (not the full `AnnKey`) because `clear_namespace`
    /// only knows the namespace being invalidated, not which models have (or
    /// will have) a build in flight for it.
    generations: std::sync::Mutex<HashMap<String, u64>>,
}

pub(crate) type SharedAnn = Arc<AnnState>;

pub(crate) fn new_shared() -> SharedAnn {
    Arc::new(AnnState {
        indexes: RwLock::new(HashMap::new()),
        warming: std::sync::Mutex::new(HashSet::new()),
        generations: std::sync::Mutex::new(HashMap::new()),
    })
}

// Recover a poisoned generations Mutex rather than aborting: the guarded
// HashMap<String, u64> stays logically valid through a poison (worst case a
// stale reader misses one bump, which only widens — never narrows — the set
// of builds treated as possibly-stale).
fn generations_guard(
    m: &std::sync::Mutex<HashMap<String, u64>>,
) -> std::sync::MutexGuard<'_, HashMap<String, u64>> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Bump `namespace`'s write-generation counter and return the new value
/// (issue #770). Called from `clear_namespace`, the single chokepoint every
/// corpus-mutating write already routes through.
fn bump_generation(ann: &SharedAnn, namespace: &str) -> u64 {
    let mut gens = generations_guard(&ann.generations);
    let slot = gens.entry(namespace.to_owned()).or_insert(0);
    *slot += 1;
    *slot
}

/// Read `namespace`'s current write-generation counter (0 if never bumped).
pub(crate) fn current_generation(ann: &SharedAnn, namespace: &str) -> u64 {
    generations_guard(&ann.generations)
        .get(namespace)
        .copied()
        .unwrap_or(0)
}

/// Install `candidate` into the cache for `key` unless it is stale (PR #815
/// review, HIGH — the #770 scenario through the empty-slot door). Two
/// independent fences, both evaluated while holding the write lock:
///
/// 1. `candidate.generation` must be >= the namespace's CURRENT generation.
///    Comparing only against an existing entry (the old behavior) has
///    nothing to compare against once `clear_namespace` has emptied the
///    slot — a pre-invalidation candidate would install unconditionally
///    even though it scanned a corpus version the namespace has since
///    invalidated. `clear_namespace` bumps the generation counter inside
///    this same write-lock scope, so a candidate's read of the current
///    generation here can never observe a pre-bump value for a slot that
///    has already been (or is about to be) evicted.
/// 2. `candidate.generation` must be >= any already-installed entry's
///    generation, so a slower-but-staler build can never clobber a faster
///    build that already scanned a newer corpus.
pub(crate) async fn install_if_fresher(ann: &SharedAnn, key: &AnnKey, candidate: AnnBridge) {
    let mut idxs = ann.indexes.write().await;

    let ns_generation = current_generation(ann, &key.namespace);
    if candidate.generation < ns_generation {
        tracing::debug!(
            key = ?key,
            candidate_generation = candidate.generation,
            namespace_generation = ns_generation,
            "knowledge ANN install skipped: candidate predates namespace's current generation"
        );
        return;
    }

    match idxs.get(key) {
        Some(existing) if existing.generation >= candidate.generation => {
            tracing::debug!(
                key = ?key,
                existing_generation = existing.generation,
                candidate_generation = candidate.generation,
                "knowledge ANN install skipped: cached entry already >= this build's generation"
            );
        }
        _ => {
            idxs.insert(key.clone(), candidate);
        }
    }
}

// Recover a poisoned warming Mutex rather than aborting: the guarded HashSet<AnnKey>
// stays logically valid through a poison (spurious presence/absence is tolerable).
fn warming_guard(
    m: &std::sync::Mutex<HashSet<AnnKey>>,
) -> std::sync::MutexGuard<'_, HashSet<AnnKey>> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Insert `bridge` under `key` only if the slot is empty. Returns `true` when
/// the bridge was inserted, `false` if the key was already present.
///
/// Test-only: unlike `install_if_fresher`, this performs no generation
/// fencing at all, so production install sites must never use it.
#[cfg(test)]
pub(crate) async fn insert_ann_if_absent(ann: &SharedAnn, key: AnnKey, bridge: AnnBridge) -> bool {
    use std::collections::hash_map::Entry;
    let mut guard = ann.indexes.write().await;
    match guard.entry(key) {
        Entry::Occupied(_) => false,
        Entry::Vacant(e) => {
            e.insert(bridge);
            true
        }
    }
}

/// Remove all in-memory ANN slots and warming-guard entries for `namespace`.
///
/// Called after any corpus mutation so the next search triggers a fresh load.
pub(crate) async fn clear_namespace(ann: &SharedAnn, namespace: &str) {
    {
        // Evict and bump the generation counter inside the SAME write-lock
        // scope (PR #815). `install_if_fresher` takes this same
        // lock before reading the namespace's current generation, so there
        // is no window between "slot emptied" and "generation bumped" where
        // a concurrent install could read a stale (pre-bump) generation and
        // self-approve into the just-emptied slot.
        let mut idxs = ann.indexes.write().await;
        idxs.retain(|k, _| k.namespace != namespace);
        bump_generation(ann, namespace);
    }
    warming_guard(&ann.warming).retain(|k| k.namespace != namespace);
}

/// Search the already-loaded index for `key`. Returns `None` on cache miss.
pub(crate) async fn search_loaded(
    ann: &SharedAnn,
    key: &AnnKey,
    query: &[f32],
    k: usize,
) -> Option<Vec<(Uuid, f32)>> {
    let guard = ann.indexes.read().await;
    guard.get(key).map(|bridge| bridge.search(query, k))
}

/// Returns `true` when `key` is registered in the warming set but its index has
/// not yet been inserted — i.e. a background load is in flight right now.
///
/// `false` means either (a) the index is already loaded, or (b) no warm has
/// been triggered for this key at all (e.g. the corpus is empty).
pub(crate) fn is_warming_not_loaded(ann: &SharedAnn, key: &AnnKey) -> bool {
    let in_warming = warming_guard(&ann.warming).contains(key);
    if !in_warming {
        return false;
    }
    // Sync check: if index is present, warming finished already.
    // `try_read()` avoids blocking — if the write lock is held we conservatively
    // report warming=true (the write lock is held during insert, so the index is
    // about to appear; treating it as "still warming" is safe).
    match ann.indexes.try_read() {
        Ok(guard) => !guard.contains_key(key),
        Err(_) => true,
    }
}

/// Poll `ann` until `key` appears in the loaded index set or `timeout_ms` elapses.
///
/// Returns `true` if the index became available within the timeout.
pub(crate) async fn wait_for_ann(
    ann: &SharedAnn,
    key: &AnnKey,
    timeout_ms: u64,
    poll_ms: u64,
) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if ann.indexes.read().await.contains_key(key) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;
    }
}

/// Bounded wait for a background ANN warm to complete before a search degrades
/// to FTS-only results. A valid-snapshot cold load on a large corpus can exceed
/// the previous 3s; 5s covers the snapshot deserialize while still bounding the
/// first post-restart query. On timeout the search degrades to FTS-only — it
/// never errors (issue #322).
pub(crate) const ANN_WARM_WAIT_TIMEOUT_MS: u64 = 5_000;
pub(crate) const ANN_WARM_WAIT_POLL_MS: u64 = 50;

// ── Test-only seam: override the ANN warm-wait timeout ───────────────────────
//
// Zero means use the production default (ANN_WARM_WAIT_TIMEOUT_MS).
// Tests set this to a small value (e.g. 50 ms) to avoid blocking the test
// suite while still exercising the full degrade code path.
static ANN_WARM_WAIT_TIMEOUT_OVERRIDE_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Returns the effective ANN warm-wait timeout in milliseconds.
///
/// In production this always equals `ANN_WARM_WAIT_TIMEOUT_MS`.  During
/// tests the value may be overridden via `set_warm_wait_timeout_override_ms`
/// to avoid a 5-second stall per test run.
pub(crate) fn warm_wait_timeout_ms() -> u64 {
    let o = ANN_WARM_WAIT_TIMEOUT_OVERRIDE_MS.load(std::sync::atomic::Ordering::Relaxed);
    if o > 0 {
        o
    } else {
        ANN_WARM_WAIT_TIMEOUT_MS
    }
}

/// Set the ANN warm-wait timeout override for tests.  Pass `0` to restore the
/// production default (`ANN_WARM_WAIT_TIMEOUT_MS`).
#[cfg(test)]
pub(crate) fn set_warm_wait_timeout_override_ms(ms: u64) {
    ANN_WARM_WAIT_TIMEOUT_OVERRIDE_MS.store(ms, std::sync::atomic::Ordering::Relaxed);
}

impl AnnBridge {
    pub fn build(mut vectors: Vec<f32>, dim: usize, id_map: Vec<Uuid>) -> Result<Self, String> {
        if dim == 0 {
            return Err("dimension must be > 0".into());
        }
        if vectors.is_empty() || id_map.is_empty() {
            return Err("no vectors to build ANN index from".into());
        }
        let n = vectors.len() / dim;
        if n != id_map.len() {
            return Err(format!(
                "id_map length {} != vector count {}",
                id_map.len(),
                n
            ));
        }
        // L2→cosine conversion requires unit vectors; normalize before building.
        for row in vectors.chunks_exact_mut(dim) {
            l2_normalize(row);
        }
        let cfg = VamanaConfig::with_dimensions(dim);
        let index = VamanaIndex::build(&vectors, cfg).map_err(|e| format!("{e}"))?;
        Ok(Self {
            index,
            id_map,
            generation: 0,
        })
    }

    /// Stamp this bridge with the namespace write-generation its corpus scan
    /// started at or after (issue #770). Called just before install; see
    /// `install_if_fresher`.
    pub(crate) fn with_generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }

    pub fn search(&self, query: &[f32], k: usize) -> Vec<(Uuid, f32)> {
        let mut q = query.to_vec();
        l2_normalize(&mut q);
        match self.index.search(&q, k) {
            Ok(results) => results
                .into_iter()
                .filter_map(|(idx, dist)| {
                    self.id_map.get(idx as usize).map(|uuid| {
                        // L2² → cosine: cos(a,b) = 1 - L2²(a,b)/2 for unit vectors
                        let cosine = 1.0 - dist / 2.0;
                        (*uuid, cosine.max(0.0))
                    })
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "vamana ANN search failed");
                Vec::new()
            }
        }
    }

    pub fn num_vectors(&self) -> usize {
        self.index.num_vectors()
    }

    pub fn from_vamana_snapshot(snapshot: VamanaSnapshot) -> Result<Self, String> {
        let id_map: Vec<Uuid> = snapshot
            .external_ids
            .iter()
            .map(|s| Uuid::parse_str(s).map_err(|e| format!("bad UUID {s}: {e}")))
            .collect::<Result<_, _>>()?;
        let index =
            VamanaIndex::from_snapshot(&snapshot).map_err(|e| format!("snapshot restore: {e}"))?;
        Ok(Self {
            index,
            id_map,
            generation: 0,
        })
    }

    /// Save this bridge to `dir` atomically.
    ///
    /// Writes Vamana index segments via [`VamanaIndex::save_atomic`] (which commits
    /// a v2 KHVVAMG2 record in `metadata.bin` carrying a `content_hash`), then writes
    /// the id-map sidecar (`external_ids.bin`) atomically via a tmp-then-rename sequence.
    /// The sidecar is stamped with the corpus `content_hash` taken from the v2 commit
    /// record.
    ///
    /// Crash-safety rationale: the v2 segment commit gate is `metadata.bin`, and the
    /// sidecar is written second. On any crash between the two writes the sidecar's
    /// stamped hash will not match the new commit's hash, so the load-time cross-check
    /// detects the torn pair and the caller rebuilds. Ordering alone is insufficient;
    /// the cross-check is the guarantee.
    #[allow(dead_code)]
    pub fn save_atomic(&self, dir: &std::path::Path) -> Result<(), String> {
        let count = self.id_map.len();
        if count != self.index.num_vectors() {
            return Err(format!(
                "id_map length {count} != index.num_vectors() {}",
                self.index.num_vectors()
            ));
        }

        // Step 1: write v2 segments atomically (metadata.bin is the commit gate).
        self.index
            .save_atomic(dir)
            .map_err(|e| format!("VamanaIndex::save_atomic: {e}"))?;

        // Step 2: read back the v2 commit fingerprint to obtain the content_hash.
        // Must be Some — we just committed it. None means an unexpected v1/torn state.
        let fp = read_commit_fingerprint(dir)
            .map_err(|e| format!("read_commit_fingerprint after save: {e}"))?
            .ok_or_else(|| {
                "save_atomic succeeded but read_commit_fingerprint returned None \
                 (unexpected v1 or torn commit)"
                    .to_string()
            })?;

        // Step 3: write the id-map sidecar atomically (tmp rename), stamped with
        // the commit's content_hash so a torn pair is self-detecting at load time.
        write_external_ids_sidecar(dir, &fp.content_hash, &self.id_map)
    }

    /// Load a bridge from a segment directory previously written by
    /// [`AnnBridge::save_atomic`].
    ///
    /// Both the Vamana v2 commit record and the id-map sidecar must be present and
    /// self-consistent (matching `content_hash` and vector count). Any mismatch returns
    /// `Err`; the caller should treat that as a Cold signal and rebuild from the corpus.
    #[allow(dead_code)]
    pub fn load(dir: &std::path::Path) -> Result<Self, String> {
        // Step 1: require a v2 commit fingerprint. Absent/v1/torn → Cold.
        let fp = read_commit_fingerprint(dir)
            .map_err(|e| format!("read_commit_fingerprint: {e}"))?
            .ok_or_else(|| {
                "no v2 commit fingerprint: segment dir is absent, v1, or has a torn commit"
                    .to_string()
            })?;

        // Step 2: raw-load the committed v2 index. VamanaIndex::load is v2-aware
        // (ADR-079): it reads the segments, verifies their checksums, and restores
        // graph + lifecycle without a corpus and without rebuilding. A torn or
        // mismatched segment surfaces as an error, which the caller treats as Cold.
        let index = VamanaIndex::load(dir).map_err(|e| format!("VamanaIndex::load: {e}"))?;

        // Step 3: read the external_ids sidecar and run cross-checks.
        let (sidecar_hash, id_map) = read_external_ids_sidecar(dir)?;

        // Cross-check: sidecar content_hash must match the v2 commit's content_hash.
        // A mismatch means a torn segment/sidecar pair (crash between the segment
        // commit and the sidecar write in save_atomic).
        if sidecar_hash != fp.content_hash {
            return Err(
                "external_ids.bin content_hash mismatch: torn segment/sidecar pair".to_string(),
            );
        }

        // Cross-check: sidecar UUID count must match the loaded index vector count.
        if id_map.len() != index.num_vectors() {
            return Err(format!(
                "external_ids.bin count {} != index.num_vectors() {}",
                id_map.len(),
                index.num_vectors()
            ));
        }

        Ok(Self {
            index,
            id_map,
            generation: 0,
        })
    }
}

fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-8 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

// ── external-id sidecar helpers (slice 1b-i, ADR-079) ────────────────────────
//
// Binary format for `external_ids.bin`:
//   magic       8 bytes   b"KHVANIDS"
//   content_hash 32 bytes corpus blake3 hash (from v2 commit fingerprint)
//   count        8 bytes  u64 little-endian — number of UUIDs
//   ids          16 × count bytes — UUIDs as raw big-endian bytes (uuid_to_bytes_le)

#[allow(dead_code)]
const SIDECAR_MAGIC: &[u8; 8] = b"KHVANIDS";

/// Write `ids` to `dir/external_ids.bin` using a tmp-then-rename pattern.
///
/// The sidecar is stamped with `content_hash` so `AnnBridge::load` can detect
/// a torn segment/sidecar pair (segments committed with hash A, sidecar still
/// holding hash B from a prior save, or vice versa).
#[allow(dead_code)]
fn write_external_ids_sidecar(
    dir: &std::path::Path,
    content_hash: &[u8; 32],
    ids: &[Uuid],
) -> Result<(), String> {
    use std::io::Write as _;

    let tmp_path = dir.join("external_ids.bin.tmp");
    let final_path = dir.join("external_ids.bin");

    let count = ids.len() as u64;
    let mut buf: Vec<u8> = Vec::with_capacity(8 + 32 + 8 + ids.len() * 16);
    buf.extend_from_slice(SIDECAR_MAGIC);
    buf.extend_from_slice(content_hash);
    buf.extend_from_slice(&count.to_le_bytes());
    for id in ids {
        buf.extend_from_slice(id.as_bytes());
    }

    let mut f = std::fs::File::create(&tmp_path)
        .map_err(|e| format!("create external_ids.bin.tmp: {e}"))?;
    f.write_all(&buf)
        .map_err(|e| format!("write external_ids.bin.tmp: {e}"))?;
    f.sync_all()
        .map_err(|e| format!("sync external_ids.bin.tmp: {e}"))?;
    drop(f);
    std::fs::rename(&tmp_path, &final_path)
        .map_err(|e| format!("rename external_ids.bin.tmp -> external_ids.bin: {e}"))
}

/// Read `dir/external_ids.bin` and return `(content_hash, ids)`.
///
/// Returns `Err` on any I/O error, wrong magic, truncated header, or count/size mismatch.
#[allow(dead_code)]
fn read_external_ids_sidecar(dir: &std::path::Path) -> Result<([u8; 32], Vec<Uuid>), String> {
    let bytes = std::fs::read(dir.join("external_ids.bin"))
        .map_err(|e| format!("read external_ids.bin: {e}"))?;

    // magic (8) + content_hash (32) + count (8) = 48 bytes minimum header
    if bytes.len() < 48 {
        return Err(format!(
            "external_ids.bin too short: {} bytes (need at least 48)",
            bytes.len()
        ));
    }

    let magic = &bytes[0..8];
    if magic != SIDECAR_MAGIC {
        return Err(format!(
            "external_ids.bin bad magic: got {:?}, expected {:?}",
            magic, SIDECAR_MAGIC
        ));
    }

    let mut content_hash = [0u8; 32];
    content_hash.copy_from_slice(&bytes[8..40]);

    let count = u64::from_le_bytes(bytes[40..48].try_into().unwrap()) as usize;
    let expected_len = 48 + count * 16;
    if bytes.len() != expected_len {
        return Err(format!(
            "external_ids.bin length mismatch: got {} bytes, expected {} for {count} UUIDs",
            bytes.len(),
            expected_len
        ));
    }

    let mut ids = Vec::with_capacity(count);
    for i in 0..count {
        let start = 48 + i * 16;
        let raw: [u8; 16] = bytes[start..start + 16].try_into().unwrap();
        ids.push(Uuid::from_bytes(raw));
    }

    Ok((content_hash, ids))
}

// ── persistence helpers ───────────────────────────────────────────────────────

/// Namespace key used in `retrieval_snapshots` for a given ns+model pair.
pub(crate) fn snapshot_key(namespace: &str, model: &str) -> String {
    format!("{namespace}::vamana::{model}")
}

/// Filesystem directory for v2 Vamana segment files for a given `(ns, model)` pair.
///
/// Returns `Some(data_dir/ann/<hex>)` where `<hex>` is the lowercase hex encoding of
/// the bytes of `snapshot_key(ns, model)`. Hex encoding is injective, filesystem-safe,
/// and reversible via `decode_ann_dir_name`. Returns `None` for in-memory backends.
fn ann_segment_dir(rt: &KhiveRuntime, ns: &str, model: &str) -> Option<std::path::PathBuf> {
    let data_dir = rt.backend_data_dir()?;
    let key = snapshot_key(ns, model);
    let hex: String = key.bytes().map(|b| format!("{b:02x}")).collect();
    Some(data_dir.join("ann").join(hex))
}

/// Decode a hex-encoded ann directory name back to `(namespace, model)`.
///
/// Reverses the encoding done by `ann_segment_dir`: hex-decodes `name` to bytes,
/// interprets them as UTF-8, then splits on `"::vamana::"`. Returns `None` on bad
/// hex, non-UTF-8 bytes, a missing separator, or empty namespace/model parts.
fn decode_ann_dir_name(name: &str) -> Option<(String, String)> {
    let raw = name.as_bytes();
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(raw.len() / 2);
    for pair in raw.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        bytes.push((hi * 16 + lo) as u8);
    }
    let key = String::from_utf8(bytes).ok()?;
    let (ns, model) = key.split_once("::vamana::")?;
    if ns.is_empty() || model.is_empty() {
        return None;
    }
    Some((ns.to_string(), model.to_string()))
}

/// Model-key sanitization — must match `khive_runtime::sanitize_key`.
pub(crate) fn sanitize_model_key(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Persist `bridge` as v2 Vamana segments under `data_dir/ann/<hex>/`.
///
/// Resolves the segment directory via `ann_segment_dir`. Returns `Ok(())` when the
/// backend is in-memory (no `data_dir`) — skipping persistence is not an error.
/// `save_atomic` computes and stamps the `content_hash` internally; callers do not
/// need to supply a `CorpusFingerprint`.
pub(crate) fn persist_ann_v2(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
    bridge: &AnnBridge,
) -> Result<(), String> {
    match ann_segment_dir(rt, ns, model) {
        Some(dir) => bridge.save_atomic(&dir),
        None => Ok(()), // in-memory backend — no filesystem, skip silently
    }
}

/// Try to load a Vamana snapshot for `namespace`+`model` from `retrieval_snapshots`.
///
/// Returns `Ok(None)` when the table is absent, the row is missing, or
/// deserialization fails — all of which are treated as cache-miss signals.
async fn try_load_snapshot(
    rt: &KhiveRuntime,
    namespace: &str,
    model: &str,
) -> Option<VamanaSnapshot> {
    let key = snapshot_key(namespace, model);
    let sql = rt.sql();
    let mut reader = sql.reader().await.ok()?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT snapshot FROM retrieval_snapshots \
                  WHERE namespace = ?1 AND index_type = ?2"
                .into(),
            params: vec![SqlValue::Text(key), SqlValue::Text("vamana".into())],
            label: None,
        })
        .await
        .ok()?;

    let row = rows.into_iter().next()?;
    let blob = match row.get("snapshot")? {
        SqlValue::Blob(b) => b.clone(),
        _ => return None,
    };
    serde_json::from_slice::<VamanaSnapshot>(&blob).ok()
}

/// Get the corpus fingerprint by querying the vector store.
pub(crate) async fn compute_fingerprint(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Option<CorpusFingerprint> {
    let store = rt.vectors_for_model(token, model).ok()?;
    let info = store.info().await.ok()?;
    Some(CorpusFingerprint {
        vector_count: info.entry_count,
        dimensions: info.dimensions as u32,
    })
}

/// Scan the sqlite-vec corpus for `model` and return raw (un-normalized) flat
/// vectors alongside the ordered UUID id-map.
///
/// Rows are fetched `ORDER BY subject_id` so the mapping is deterministic.
/// Returns `Ok(None)` when the model is not configured, the table is empty, or
/// no rows pass the byte-length validity check.  The caller derives `dims` as
/// `flat.len() / id_map.len()`.
async fn scan_corpus_raw(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Result<Option<(Vec<f32>, Vec<Uuid>)>, RuntimeError> {
    let store = match rt.vectors_for_model(token, model) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };

    let info = store
        .info()
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;
    let count = info.entry_count;
    let dims = info.dimensions;

    if count == 0 || dims == 0 {
        return Ok(None);
    }

    let ns = token.namespace().as_str().to_owned();
    let model_key = sanitize_model_key(model);
    let table_name = format!("vec_{model_key}");
    let model_str = model.to_owned();

    let sql = rt.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;

    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT subject_id, embedding FROM {table_name} \
                 WHERE namespace = ?1 AND embedding_model = ?2 \
                   AND field = 'knowledge.atom' \
                 ORDER BY subject_id"
            ),
            params: vec![SqlValue::Text(ns), SqlValue::Text(model_str)],
            label: Some("vamana_corpus_scan".into()),
        })
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;

    if rows.is_empty() {
        return Ok(None);
    }

    let mut id_map: Vec<Uuid> = Vec::with_capacity(rows.len());
    let mut flat: Vec<f32> = Vec::with_capacity(rows.len() * dims);

    for row in &rows {
        let id_str = match row.get("subject_id") {
            Some(SqlValue::Text(s)) => s.as_str(),
            _ => continue,
        };
        let uuid = match Uuid::parse_str(id_str) {
            Ok(id) => id,
            Err(_) => continue,
        };
        let bytes = match row.get("embedding") {
            Some(SqlValue::Blob(b)) => b.as_slice(),
            _ => continue,
        };
        if bytes.len() != dims * 4 {
            continue;
        }
        let vec: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        id_map.push(uuid);
        flat.extend_from_slice(&vec);
    }

    if id_map.is_empty() {
        return Ok(None);
    }

    Ok(Some((flat, id_map)))
}

/// Scan the sqlite-vec table and build a fresh `AnnBridge`.
///
/// Returns `None` when there are no vectors or the model is not configured.
pub(crate) async fn load_and_build_from_vector_store(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Result<Option<AnnBridge>, RuntimeError> {
    let Some((flat, id_map)) = scan_corpus_raw(rt, token, model).await? else {
        return Ok(None);
    };
    let dims = flat.len() / id_map.len();
    AnnBridge::build(flat, dims, id_map)
        .map(Some)
        .map_err(RuntimeError::Internal)
}

/// Hash the live corpus as it would appear inside a freshly built `AnnBridge`.
///
/// Scans the sqlite-vec corpus, L2-normalizes each vector in place (matching
/// `AnnBridge::build` which calls `l2_normalize` exactly once per row), then
/// passes the normalized flat buffer through `corpus_content_hash`.  Returns
/// `None` when the corpus is empty or the model is not configured.
///
/// INVARIANT: normalize ONCE here, never re-normalize an already-normalized
/// buffer.  Calling this on a buffer that has already been normalized produces a
/// non-idempotent hash and causes always-stale comparisons.
async fn live_content_hash(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Option<[u8; 32]> {
    let (mut flat, id_map) = scan_corpus_raw(rt, token, model).await.ok()??;
    let dims = flat.len() / id_map.len();
    for row in flat.chunks_exact_mut(dims) {
        l2_normalize(row);
    }
    Some(corpus_content_hash(&flat))
}

/// Delete all Vamana snapshots for `namespace` from `retrieval_snapshots`.
///
/// Called after any vector-corpus mutation to guarantee `ensure_ann_for_model` cannot
/// load a snapshot that no longer matches the live corpus.  Best-effort: if
/// the `retrieval_snapshots` table doesn't exist yet, the call is a no-op.
/// Escape SQLite `LIKE` wildcard characters (`%`, `_`) and the escape
/// character itself (`\`) so a caller-supplied namespace is matched literally
/// under `LIKE ... ESCAPE '\'` rather than as a pattern (#819: an
/// underscore-bearing namespace like `a_b` must not also match `aXb`).
fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub(crate) async fn invalidate_snapshot(rt: &KhiveRuntime, namespace: &str) {
    let pattern = format!("{}::vamana::%", escape_like(namespace));
    let sql = rt.sql();
    let mut w = match sql.writer().await {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "failed to open writer for Vamana snapshot invalidation");
            return;
        }
    };
    match w
        .execute(SqlStatement {
            sql: "DELETE FROM retrieval_snapshots WHERE namespace LIKE ?1 ESCAPE '\\'".into(),
            params: vec![SqlValue::Text(pattern)],
            label: Some("invalidate_vamana_snapshot".into()),
        })
        .await
    {
        Ok(_) => {}
        Err(e) if e.to_string().contains("no such table") => {}
        Err(e) => {
            tracing::warn!(error = %e, "failed to invalidate Vamana snapshot");
        }
    }
}

/// Pre-load Vamana snapshots for all `{ns}::vamana::{model}` keys found in
/// `retrieval_snapshots`.  Called from `KnowledgePack::warm()` before the first
/// search request so in-memory indexes are ready without a first-query spike.
///
/// Each unique namespace+model pair gets its own keyed slot; all snapshots are
/// loaded, not just the first one.
pub(crate) async fn warm_known_snapshots(rt: &KhiveRuntime, ann: &SharedAnn) {
    // v1 legacy pass: warm namespaces recorded in retrieval_snapshots, if that
    // table exists. On a v2-only database it will not, so a query error must fall
    // through to the v2 segment enumeration below rather than abort the warm pass.
    let rows = {
        let sql = rt.sql();
        match sql.reader().await {
            Ok(mut reader) => reader
                .query_all(SqlStatement {
                    sql:
                        "SELECT DISTINCT namespace FROM retrieval_snapshots WHERE namespace LIKE ?1"
                            .into(),
                    params: vec![SqlValue::Text("%::vamana::%".into())],
                    label: None,
                })
                .await
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    };

    for row in &rows {
        let ns_key = match row.get("namespace") {
            Some(SqlValue::Text(s)) => s.as_str(),
            _ => continue,
        };
        let Some((ns_str, model)) = ns_key.split_once("::vamana::") else {
            continue;
        };
        if ns_str.is_empty() || model.is_empty() {
            continue;
        }
        let ns = match Namespace::parse(ns_str) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let token = match rt.authorize(ns) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let key = AnnKey::new(ns_str, model);
        {
            let mut warming = warming_guard(&ann.warming);
            if warming.contains(&key) {
                continue; // another path is already warming this key
            }
            warming.insert(key.clone());
        }
        ensure_ann_for_model(rt, &token, ann, model).await;
        let loaded = ann.indexes.read().await.contains_key(&key);
        if !loaded {
            warming_guard(&ann.warming).remove(&key);
        }
    }

    // Enumerate v2 segment directories in `data_dir/ann/` and warm any keys not
    // already loaded by the v1 DB pass above.
    let ann_root = match rt.backend_data_dir() {
        Some(d) => d.join("ann"),
        None => return,
    };
    let read_dir = match std::fs::read_dir(&ann_root) {
        Ok(rd) => rd,
        Err(_) => return, // no ann/ dir yet — nothing to warm
    };
    for entry in read_dir.flatten() {
        let name = entry.file_name();
        let hex = name.to_string_lossy();
        let Some((ns_str, model)) = decode_ann_dir_name(hex.as_ref()) else {
            continue;
        };
        let ns = match Namespace::parse(&ns_str) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let token = match rt.authorize(ns) {
            Ok(t) => t,
            Err(_) => continue,
        };
        // Guard: skip if already loaded by the v1 pass.
        let key = AnnKey::new(&ns_str, &model);
        if ann.indexes.read().await.contains_key(&key) {
            continue;
        }
        ensure_ann_for_model(rt, &token, ann, &model).await;
    }
}

/// Fire-once per-key background warm. Returns immediately. If the key is already
/// loaded or warming is in flight for it, does nothing. On a completed attempt
/// that produced no index (e.g. no corpus yet), removes the key from the warming
/// guard so a later search can retry.
pub(crate) fn ensure_ann_background(rt: &KhiveRuntime, token: &NamespaceToken, ann: &SharedAnn) {
    let model = rt.default_embedder_name().to_string();
    if model.is_empty() {
        return;
    }
    let ns = token.namespace().as_str().to_owned();
    let key = AnnKey::new(&ns, &model);

    {
        let mut warming = warming_guard(&ann.warming);
        if warming.contains(&key) {
            return; // already warming or warmed
        }
        warming.insert(key.clone());
    }

    let rt = rt.clone();
    let ann = ann.clone();
    let token_ns = token.namespace().clone();
    tokio::spawn(async move {
        if let Ok(token) = rt.authorize(token_ns) {
            ensure_ann_for_model(&rt, &token, &ann, &model).await;
        }
        // If loading failed, remove from warming so a later search can retry.
        let loaded = ann.indexes.read().await.contains_key(&key);
        if !loaded {
            warming_guard(&ann.warming).remove(&key);
        }
    });
}

/// Lazy warm-load for a specific `model`.
///
/// Load order (first hit wins):
///
/// 1. **Fast path** — already in the in-memory cache; return immediately.
/// 2. **v2 segment path** — if a `data_dir/ann/<hex>/` directory exists with a
///    valid `metadata.bin`, compare its `content_hash` against a freshly computed
///    `live_content_hash`.  On match, load the Vamana binary segments directly via
///    `AnnBridge::load` (O(load), no rebuild).  On mismatch, fall through.
/// 3. **v1 JSON snapshot path** — try `retrieval_snapshots`; on hit, validate
///    the `CorpusFingerprint` (count + dims) and restore from JSON.  On miss /
///    stale / corrupt, fall through.
/// 4. **Rebuild fallthrough** — scan the full sqlite-vec corpus, build the index
///    from scratch, and atomically write a v2 segment directory so the next daemon
///    restart can use path 2.  Write failures are logged and do not block search.
pub(crate) async fn ensure_ann_for_model(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
) {
    if model.is_empty() {
        return;
    }
    let ns = token.namespace().as_str().to_owned();
    let key = AnnKey::new(&ns, model);

    // Capture the namespace's write-generation BEFORE anything else (issue
    // #770) — including before the fast path below and before the corpus
    // scan — so a write that lands after this point is guaranteed to be
    // reflected as a higher generation than anything this build can install.
    let target_generation = current_generation(ann, &ns);

    // 1. Fast path: already loaded AND at least as fresh as this namespace's
    // current generation (PR #815). A present entry with a
    // stale generation is not a hit — mere presence let a pre-invalidation
    // build served from an emptied-then-refilled slot serve indefinitely.
    // Falling through here re-enters the same rebuild path a genuine cache
    // miss would take.
    if let Some(loaded_generation) = ann
        .indexes
        .read()
        .await
        .get(&key)
        .map(|bridge| bridge.generation)
    {
        if loaded_generation >= target_generation {
            return;
        }
        tracing::debug!(
            namespace = %ns,
            model = %model,
            loaded_generation,
            target_generation,
            "knowledge ANN fast path skipped: cached entry generation stale; rebuilding"
        );
    }

    // 2. v2 segment path.
    if let Some(seg_dir) = ann_segment_dir(rt, &ns, model) {
        match read_commit_fingerprint(&seg_dir) {
            Ok(Some(persisted)) => {
                // Cheap count+dims gate before hashing the full corpus.
                let current_fp = compute_fingerprint(rt, token, model).await;
                let count_dims_ok = current_fp.is_some_and(|fp| {
                    fp.vector_count == persisted.vector_count
                        && fp.dimensions as u64 == persisted.dimensions
                });
                if count_dims_ok {
                    // Full content-hash check (normalizes corpus once).
                    if let Some(live_hash) = live_content_hash(rt, token, model).await {
                        if live_hash == persisted.content_hash {
                            match AnnBridge::load(&seg_dir) {
                                Ok(bridge) => {
                                    install_if_fresher(
                                        ann,
                                        &key,
                                        bridge.with_generation(target_generation),
                                    )
                                    .await;
                                    return;
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        dir = %seg_dir.display(),
                                        "v2 segment load failed; falling through to rebuild"
                                    );
                                }
                            }
                        } else {
                            tracing::info!(
                                namespace = %ns,
                                model = %model,
                                "stale v2 segment (content-hash mismatch); rebuilding"
                            );
                        }
                    }
                } else {
                    tracing::info!(
                        namespace = %ns,
                        model = %model,
                        "stale v2 segment (count/dims mismatch); rebuilding"
                    );
                }
            }
            Ok(None) => {
                // No v2 segments yet (or torn write) — fall through to v1 / rebuild.
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    dir = %seg_dir.display(),
                    "error reading v2 segment fingerprint; falling through"
                );
            }
        }
    }

    // 3. v1 JSON snapshot path (backwards-compat transition).
    if let Some(snapshot) = try_load_snapshot(rt, &ns, model).await {
        let current_fp = compute_fingerprint(rt, token, model).await;
        if let Some(fp) = current_fp {
            if snapshot.fingerprint == fp {
                match AnnBridge::from_vamana_snapshot(snapshot) {
                    Ok(bridge) => {
                        install_if_fresher(ann, &key, bridge.with_generation(target_generation))
                            .await;
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "corrupt Vamana v1 snapshot; rebuilding");
                    }
                }
            } else {
                tracing::info!(
                    namespace = %ns,
                    model = %model,
                    "stale Vamana v1 snapshot (fingerprint mismatch); rebuilding"
                );
            }
        }
    }

    // 4. Rebuild fallthrough — build from vector store and persist v2 segments.
    match load_and_build_from_vector_store(rt, token, model).await {
        Ok(Some(bridge)) => {
            if let Err(e) = persist_ann_v2(rt, &ns, model, &bridge) {
                tracing::error!(error = %e, "failed to persist v2 Vamana segment after rebuild");
            }
            install_if_fresher(ann, &key, bridge.with_generation(target_generation)).await;
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(error = %e, "failed to rebuild Vamana ANN index");
        }
    }
}

/// Simulate an in-flight warm by inserting `key` into the warming set without
/// populating the index.  Call this in tests to construct the "warming but not
/// yet loaded" state that triggers the cold-start guard in `suggest`/`search`.
#[cfg(test)]
pub(crate) fn simulate_warming_in_flight(ann: &SharedAnn, key: AnnKey) {
    warming_guard(&ann.warming).insert(key);
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::KhiveRuntime;
    use khive_storage::types::{SqlStatement, SqlValue};

    #[tokio::test]
    async fn test_invalidate_snapshot_removes_vamana_rows() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let sql = rt.sql();

        let mut w = sql.writer().await.expect("writer");
        w.execute_script(
            "CREATE TABLE IF NOT EXISTS retrieval_snapshots (\
             namespace TEXT NOT NULL, index_type TEXT NOT NULL, \
             snapshot BLOB NOT NULL, created_at INTEGER NOT NULL, \
             PRIMARY KEY (namespace, index_type));"
                .into(),
        )
        .await
        .expect("create table");

        for (ns, idx_type) in &[
            ("local::vamana::model-a", "vamana"),
            ("local::vamana::model-b", "vamana"),
            ("local::hnsw::model-a", "hnsw"),
        ] {
            w.execute(SqlStatement {
                sql: "INSERT INTO retrieval_snapshots (namespace, index_type, snapshot, created_at) VALUES (?1, ?2, ?3, 0)".into(),
                params: vec![
                    SqlValue::Text(ns.to_string()),
                    SqlValue::Text(idx_type.to_string()),
                    SqlValue::Blob(b"{}".to_vec()),
                ],
                label: None,
            })
            .await
            .expect("insert");
        }
        drop(w);

        invalidate_snapshot(&rt, "local").await;

        let mut r = sql.reader().await.expect("reader");
        let rows = r
            .query_all(SqlStatement {
                sql: "SELECT namespace FROM retrieval_snapshots ORDER BY namespace".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query");

        let remaining: Vec<String> = rows
            .iter()
            .filter_map(|row| match row.get("namespace") {
                Some(SqlValue::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();

        assert!(
            remaining.contains(&"local::hnsw::model-a".to_string()),
            "HNSW rows must survive: {remaining:?}"
        );
        assert!(
            !remaining.contains(&"local::vamana::model-a".to_string()),
            "vamana model-a must be deleted: {remaining:?}"
        );
        assert!(
            !remaining.contains(&"local::vamana::model-b".to_string()),
            "vamana model-b must be deleted: {remaining:?}"
        );
    }

    #[tokio::test]
    async fn test_invalidate_snapshot_does_not_cross_underscore_namespace() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let sql = rt.sql();

        let mut w = sql.writer().await.expect("writer");
        w.execute_script(
            "CREATE TABLE IF NOT EXISTS retrieval_snapshots (\
             namespace TEXT NOT NULL, index_type TEXT NOT NULL, \
             snapshot BLOB NOT NULL, created_at INTEGER NOT NULL, \
             PRIMARY KEY (namespace, index_type));"
                .into(),
        )
        .await
        .expect("create table");

        // "a_b" and "aXb" are distinct namespaces (the `_` in "a_b" is a
        // literal underscore, not a wildcard). Before #819's fix, invalidating
        // "a_b" also deleted "aXb"'s row because `_` is a single-character
        // LIKE wildcard.
        for ns in &["a_b::vamana::model-a", "aXb::vamana::model-a"] {
            w.execute(SqlStatement {
                sql: "INSERT INTO retrieval_snapshots (namespace, index_type, snapshot, created_at) VALUES (?1, ?2, ?3, 0)".into(),
                params: vec![
                    SqlValue::Text(ns.to_string()),
                    SqlValue::Text("vamana".to_string()),
                    SqlValue::Blob(b"{}".to_vec()),
                ],
                label: None,
            })
            .await
            .expect("insert");
        }
        drop(w);

        invalidate_snapshot(&rt, "a_b").await;

        let mut r = sql.reader().await.expect("reader");
        let rows = r
            .query_all(SqlStatement {
                sql: "SELECT namespace FROM retrieval_snapshots ORDER BY namespace".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query");

        let remaining: Vec<String> = rows
            .iter()
            .filter_map(|row| match row.get("namespace") {
                Some(SqlValue::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();

        assert!(
            remaining.contains(&"aXb::vamana::model-a".to_string()),
            "unrelated namespace 'aXb' must survive invalidating 'a_b': {remaining:?}"
        );
        assert!(
            !remaining.contains(&"a_b::vamana::model-a".to_string()),
            "'a_b' own snapshot must still be deleted: {remaining:?}"
        );
    }

    #[tokio::test]
    async fn test_invalidate_snapshot_tolerates_missing_table() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        // No retrieval_snapshots table — must not panic.
        invalidate_snapshot(&rt, "local").await;
    }

    #[tokio::test]
    async fn test_invalidate_clears_in_memory_ann() {
        let ann = new_shared();

        let dim = 4;
        let vectors = vec![1.0f32, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0];
        let ids = vec![Uuid::new_v4(), Uuid::new_v4()];
        let bridge = AnnBridge::build(vectors, dim, ids).expect("build");
        let key = AnnKey::new("local", "test-model");
        assert!(
            insert_ann_if_absent(&ann, key.clone(), bridge).await,
            "insert must succeed on empty cache"
        );
        assert!(
            ann.indexes.read().await.contains_key(&key),
            "pre-condition: ANN loaded"
        );

        clear_namespace(&ann, "local").await;
        assert!(
            !ann.indexes.read().await.contains_key(&key),
            "clearing SharedAnn must remove the bridge"
        );
    }

    #[tokio::test]
    async fn shared_ann_is_keyed_by_namespace_and_model() {
        let ann = new_shared();
        let model = "test-model";
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();

        let bridge_a = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![id_a])
            .expect("build namespace A bridge");
        let bridge_b = AnnBridge::build(vec![0.0, 1.0, 0.0, 0.0], 4, vec![id_b])
            .expect("build namespace B bridge");

        assert!(insert_ann_if_absent(&ann, AnnKey::new("ns:a", model), bridge_a).await);
        assert!(insert_ann_if_absent(&ann, AnnKey::new("ns:b", model), bridge_b).await);

        let hits_b = search_loaded(&ann, &AnnKey::new("ns:b", model), &[1.0, 0.0, 0.0, 0.0], 1)
            .await
            .expect("namespace B bridge exists");

        assert_eq!(hits_b.len(), 1);
        assert_eq!(
            hits_b[0].0, id_b,
            "namespace B query must not return namespace A neighbour"
        );
    }

    // ── generation-checked install (issue #770) ──────────────────────────────

    #[tokio::test]
    async fn install_if_fresher_rejects_late_stale_build() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");

        let fresh = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build fresh bridge")
            .with_generation(2);
        let stale = AnnBridge::build(vec![0.0, 1.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build stale bridge")
            .with_generation(1);

        // Fresh install first, then a late-arriving stale build must not clobber it.
        install_if_fresher(&ann, &key, fresh).await;
        install_if_fresher(&ann, &key, stale).await;

        let installed_generation = ann
            .indexes
            .read()
            .await
            .get(&key)
            .expect("entry present")
            .generation;
        assert_eq!(
            installed_generation, 2,
            "stale build (generation 1) must not replace fresher installed entry (generation 2)"
        );
    }

    #[tokio::test]
    async fn install_if_fresher_accepts_forward_progress() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");

        let old = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build old bridge")
            .with_generation(1);
        let newer = AnnBridge::build(vec![0.0, 1.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build newer bridge")
            .with_generation(2);

        // Normal forward progress: old installs first, newer build replaces it.
        install_if_fresher(&ann, &key, old).await;
        install_if_fresher(&ann, &key, newer).await;

        let installed_generation = ann
            .indexes
            .read()
            .await
            .get(&key)
            .expect("entry present")
            .generation;
        assert_eq!(
            installed_generation, 2,
            "newer build must replace an older installed entry"
        );
    }

    #[tokio::test]
    async fn install_if_fresher_ties_keep_incumbent() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");

        let first = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build first bridge")
            .with_generation(1);
        let second_id = Uuid::new_v4();
        let second = AnnBridge::build(vec![0.0, 1.0, 0.0, 0.0], 4, vec![second_id])
            .expect("build second bridge")
            .with_generation(1);

        install_if_fresher(&ann, &key, first).await;
        install_if_fresher(&ann, &key, second).await;

        let hits = search_loaded(&ann, &key, &[0.0, 1.0, 0.0, 0.0], 1)
            .await
            .expect("entry present");
        assert_ne!(
            hits.first().map(|(id, _)| *id),
            Some(second_id),
            "equal-generation candidate must not replace the incumbent"
        );
    }

    #[tokio::test]
    async fn install_if_fresher_installs_into_empty_slot() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");
        let bridge = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build bridge")
            .with_generation(0);

        install_if_fresher(&ann, &key, bridge).await;

        assert!(
            ann.indexes.read().await.contains_key(&key),
            "first successful build must always install into an empty slot"
        );
    }

    #[tokio::test]
    async fn clear_namespace_bumps_generation_scoped_to_namespace() {
        let ann = new_shared();

        assert_eq!(current_generation(&ann, "ns:a"), 0);
        assert_eq!(current_generation(&ann, "ns:b"), 0);

        clear_namespace(&ann, "ns:a").await;

        assert_eq!(
            current_generation(&ann, "ns:a"),
            1,
            "clear_namespace must bump the invalidated namespace's generation"
        );
        assert_eq!(
            current_generation(&ann, "ns:b"),
            0,
            "clear_namespace must not affect a different namespace's generation"
        );
    }

    #[tokio::test]
    async fn stale_build_installs_before_invalidation_race_is_rejected_after() {
        // Simulates the #770 race deterministically: build A (slow, e.g. the
        // full corpus rebuild fallthrough) starts scanning and captures its
        // generation floor. An invalidating write lands mid-build, clearing
        // the slot and bumping the namespace generation. The empty slot lets
        // a second, concurrent build B (e.g. `ensure_ann_background` retried
        // by the next search, since `clear_namespace` also freed the warming
        // guard) start, scan the now-current corpus, and install first. Only
        // afterward does build A's slow scan finish and attempt to install
        // its stale result — it must lose to B rather than clobbering it, the
        // exact bug this issue reports (`entry().or_insert()` would have let
        // A's late install win regardless of arrival order).
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");

        // Build A starts: capture the generation floor before doing any work.
        let build_a_generation = current_generation(&ann, "local");
        assert_eq!(build_a_generation, 0);

        // A concurrent write invalidates the namespace while A is still scanning.
        clear_namespace(&ann, "local").await;
        assert_eq!(current_generation(&ann, "local"), 1);

        // Build B starts after the invalidation (slot is empty, warming guard
        // was cleared too), scans the current corpus, and installs first.
        let build_b_generation = current_generation(&ann, "local");
        let build_b_id = Uuid::new_v4();
        let build_b_bridge = AnnBridge::build(vec![0.0, 1.0, 0.0, 0.0], 4, vec![build_b_id])
            .expect("build fresh bridge")
            .with_generation(build_b_generation);
        install_if_fresher(&ann, &key, build_b_bridge).await;
        assert!(
            ann.indexes.read().await.contains_key(&key),
            "build B (post-invalidation generation) must install"
        );

        // Build A's slow scan finally finishes and attempts to install its
        // stale (pre-invalidation) result.
        let build_a_bridge = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build stale bridge")
            .with_generation(build_a_generation);
        install_if_fresher(&ann, &key, build_a_bridge).await;

        let hits = search_loaded(&ann, &key, &[0.0, 1.0, 0.0, 0.0], 1)
            .await
            .expect("entry present");
        assert_eq!(
            hits.first().map(|(id, _)| *id),
            Some(build_b_id),
            "build A's late, stale install must not clobber build B's fresher result"
        );
    }

    #[tokio::test]
    async fn stale_build_rejected_installing_into_still_empty_post_invalidation_slot() {
        // Deterministic reproduction of the #770 scenario through the EMPTY-SLOT
        // door (PR #815): unlike the test above (where a fresh build
        // B installs first, so the stale build has an incumbent to lose against),
        // this exercises the case where NOTHING has installed yet when the stale
        // build arrives. Build A captures its generation floor, an invalidating
        // write (`clear_namespace`) bumps the namespace's generation while the
        // slot is still empty, and only then does A's late, stale install attempt
        // land — straight into that still-empty slot. The old `install_if_fresher`
        // compared a candidate only against an *existing* entry, so an empty slot
        // meant nothing to compare against and the stale build installed
        // unconditionally. The fix compares against the namespace's CURRENT
        // generation instead, so this must be rejected even with no incumbent.
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");

        // Build A starts: capture the generation floor before doing any work.
        let build_a_generation = current_generation(&ann, "local");
        assert_eq!(build_a_generation, 0);

        // An invalidating write lands while A is still scanning. The slot was
        // never populated, so this is a no-op on the map, but it must still
        // bump the namespace's generation.
        clear_namespace(&ann, "local").await;
        assert_eq!(current_generation(&ann, "local"), 1);
        assert!(
            !ann.indexes.read().await.contains_key(&key),
            "precondition: slot must still be empty after clear_namespace"
        );

        // Build A's slow scan finally finishes and attempts to install its
        // stale (pre-invalidation) result into the still-empty slot.
        let build_a_bridge = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build stale bridge")
            .with_generation(build_a_generation);
        install_if_fresher(&ann, &key, build_a_bridge).await;

        assert!(
            !ann.indexes.read().await.contains_key(&key),
            "stale pre-invalidation build must not install into the emptied slot, \
             even with no incumbent to compare against"
        );
        assert!(
            search_loaded(&ann, &key, &[1.0, 0.0, 0.0, 0.0], 1)
                .await
                .is_none(),
            "the fast path must not serve a stale index that was correctly rejected at install"
        );
    }

    // ── is_warming_not_loaded ─────────────────────────────────────────────────

    #[test]
    fn is_warming_false_when_neither_warming_nor_loaded() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");
        assert!(
            !is_warming_not_loaded(&ann, &key),
            "key absent from both sets must return false"
        );
    }

    #[test]
    fn is_warming_true_when_in_warming_but_not_indexes() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");
        simulate_warming_in_flight(&ann, key.clone());
        assert!(
            is_warming_not_loaded(&ann, &key),
            "key in warming but not indexes must return true"
        );
    }

    #[tokio::test]
    async fn is_warming_false_when_both_warming_and_loaded() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");
        // Mark as warming.
        simulate_warming_in_flight(&ann, key.clone());
        // Now insert the index (simulates background warm completing).
        let bridge =
            AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()]).expect("build");
        insert_ann_if_absent(&ann, key.clone(), bridge).await;
        assert!(
            !is_warming_not_loaded(&ann, &key),
            "key in both warming and indexes must return false (warm is done)"
        );
    }

    // ── wait_for_ann ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn wait_for_ann_returns_true_immediately_when_already_loaded() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");
        let bridge =
            AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()]).expect("build");
        insert_ann_if_absent(&ann, key.clone(), bridge).await;
        // Already loaded — should return true without sleeping.
        let ready = wait_for_ann(&ann, &key, 100, 10).await;
        assert!(ready, "must return true when index is already in the map");
    }

    #[tokio::test]
    async fn wait_for_ann_returns_false_on_timeout_when_never_loaded() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");
        // Nothing inserted — should time out and return false.
        let ready = wait_for_ann(&ann, &key, 60, 10).await;
        assert!(
            !ready,
            "must return false when index never appears within timeout"
        );
    }

    #[tokio::test]
    async fn wait_for_ann_returns_true_when_index_appears_mid_poll() {
        let ann = new_shared();
        let key = AnnKey::new("local", "test-model");
        let ann2 = ann.clone();
        let key2 = key.clone();
        // Spawn a task that inserts the bridge after a short delay.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            let bridge = AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
                .expect("build");
            insert_ann_if_absent(&ann2, key2, bridge).await;
        });
        // Poll with a 500ms timeout; the insert happens at ~40ms so it should succeed.
        let ready = wait_for_ann(&ann, &key, 500, 10).await;
        assert!(ready, "must return true when index appears before timeout");
    }

    // ── poison recovery ───────────────────────────────────────────────────────

    /// Poison the warming Mutex by panicking while holding the guard, then verify
    /// that `warming_guard` and callers built on it survive and return sane results.
    ///
    /// This test WOULD panic if `warming_guard` were reverted to `.expect("warming
    /// lock")`, because a poisoned Mutex causes `lock()` to return `Err`, and
    /// `.expect()` converts that to a panic.
    #[test]
    fn warming_guard_recovers_from_poison() {
        let ann = new_shared();
        let key = AnnKey::new("poison-ns", "poison-model");

        // Poison the mutex by sharing the Ann via Arc across a thread that panics
        // while holding the guard.
        let ann2 = ann.clone();
        let join_result = std::thread::spawn(move || {
            let _guard = ann2.warming.lock().expect("pre-poison lock");
            panic!("deliberate poison");
        })
        .join();
        assert!(join_result.is_err(), "poison thread must have panicked");
        assert!(
            ann.warming.is_poisoned(),
            "mutex must be poisoned before recovery"
        );

        // `warming_guard` must recover the guard without panicking.
        let guard = warming_guard(&ann.warming);
        assert!(
            !guard.contains(&key),
            "recovered guard must report key absent (HashSet is empty after poison)"
        );
        drop(guard);

        // Higher-level callers built on `warming_guard` must also succeed.
        assert!(
            !is_warming_not_loaded(&ann, &key),
            "is_warming_not_loaded must not panic on poisoned Mutex"
        );
    }

    // ── warm-path-unification (Change D) invariants ───────────────────────────

    #[tokio::test]
    async fn warm_path_key_in_warming_set_before_and_after_successful_load() {
        // Verifies the warm-path-unification protocol introduced for warm_known_snapshots:
        // (1) key is registered in warming BEFORE the load attempt,
        // (2) after a successful load, key is in both warming AND indexes,
        //     so is_warming_not_loaded returns false (warm complete, not in flight).
        // (3) during (1)→(2), is_warming_not_loaded returns true — a concurrent query
        //     that arrives mid-warm correctly identifies the in-flight state.
        let ann = new_shared();
        let key = AnnKey::new("local", "warm-unify-model");

        // Step 1: register key in warming (mirrors new warm_known_snapshots pre-warm step).
        {
            let mut warming = warming_guard(&ann.warming);
            warming.insert(key.clone());
        }
        assert!(
            is_warming_not_loaded(&ann, &key),
            "key in warming but not indexes must report warming in flight"
        );

        // Step 2: simulate successful ensure_ann_for_model (bridge inserted into indexes).
        let bridge = AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build bridge for warm-path test");
        insert_ann_if_absent(&ann, key.clone(), bridge).await;

        // Step 3: key now in both warming and indexes → warm is complete, not in-flight.
        assert!(
            !is_warming_not_loaded(&ann, &key),
            "key in both warming and indexes must not report warming in flight"
        );
        assert!(
            ann.indexes.read().await.contains_key(&key),
            "index must be present after successful build"
        );
    }

    #[tokio::test]
    async fn warm_path_failed_load_removes_key_from_warming_set() {
        // Verifies that when warm_known_snapshots fails to load an index (e.g. no corpus
        // vectors), the key is removed from the warming set, allowing a later search
        // to trigger a fresh load attempt.
        let ann = new_shared();
        let key = AnnKey::new("local", "warm-unify-fail-model");

        // Pre-warm step: insert key into warming (mirrors new warm_known_snapshots code).
        warming_guard(&ann.warming).insert(key.clone());
        assert!(
            is_warming_not_loaded(&ann, &key),
            "pre-condition: key must show as warming in flight"
        );

        // Load failed — no bridge inserted. Cleanup step removes key from warming.
        let loaded = ann.indexes.read().await.contains_key(&key);
        if !loaded {
            warming_guard(&ann.warming).remove(&key);
        }

        // After cleanup, key is in neither set → is_warming_not_loaded = false.
        // A subsequent search can now trigger a fresh load attempt.
        assert!(
            !is_warming_not_loaded(&ann, &key),
            "after failed-load cleanup, warming must not show in-flight"
        );
        assert!(
            !ann.indexes.read().await.contains_key(&key),
            "index must remain absent after failed load"
        );
    }

    // ── AnnBridge::save_atomic / load (slice 1b-i, ADR-079) ──────────────────

    fn build_test_bridge(dim: usize, n: usize) -> (AnnBridge, Vec<Uuid>) {
        let ids: Vec<Uuid> = (0..n).map(|_| Uuid::new_v4()).collect();
        let mut vectors: Vec<f32> = Vec::with_capacity(n * dim);
        for i in 0..n {
            for d in 0..dim {
                vectors.push(if d == i % dim { 1.0 } else { 0.0 });
            }
        }
        let bridge = AnnBridge::build(vectors, dim, ids.clone()).expect("build test bridge");
        (bridge, ids)
    }

    #[test]
    fn ann_bridge_save_atomic_load_round_trip() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let dim = 4;
        let (bridge, ids) = build_test_bridge(dim, 4);
        let first_id = ids[0];

        bridge.save_atomic(dir.path()).expect("save_atomic");

        let loaded = AnnBridge::load(dir.path()).expect("load");
        assert_eq!(
            loaded.num_vectors(),
            bridge.num_vectors(),
            "loaded vector count must match saved"
        );

        // Search with a query that points at vector 0 (1.0, 0.0, 0.0, 0.0)
        let query = vec![1.0f32, 0.0, 0.0, 0.0];
        let hits = loaded.search(&query, 1);
        assert_eq!(hits.len(), 1, "must return 1 hit");
        assert_eq!(hits[0].0, first_id, "top hit must be the first UUID");
    }

    #[test]
    fn ann_bridge_load_missing_sidecar_err() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let (bridge, _) = build_test_bridge(4, 2);

        bridge.save_atomic(dir.path()).expect("save_atomic");
        std::fs::remove_file(dir.path().join("external_ids.bin")).expect("remove sidecar");

        let result = AnnBridge::load(dir.path());
        assert!(
            result.is_err(),
            "load must fail when external_ids.bin is missing"
        );
    }

    #[test]
    fn ann_bridge_load_torn_pair_err() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let dim = 4;

        // Save bridge A into the directory — both segments and sidecar for A.
        let (bridge_a, _) = build_test_bridge(dim, 2);
        bridge_a.save_atomic(dir.path()).expect("save_atomic A");

        // Overwrite the Vamana segments with bridge B's segments ONLY (no sidecar update).
        // This simulates a crash after VamanaIndex::save_atomic but before write_external_ids_sidecar.
        let (bridge_b, _) = build_test_bridge(dim, 3);
        bridge_b
            .index
            .save_atomic(dir.path())
            .expect("save_atomic B segments");

        // Now: metadata.bin carries B's content_hash, external_ids.bin still has A's hash.
        let result = AnnBridge::load(dir.path());
        assert!(
            result.is_err(),
            "load must fail when segment content_hash != sidecar content_hash (torn pair)"
        );
        let err = result.err().expect("already asserted is_err");
        assert!(
            err.contains("content_hash mismatch") || err.contains("torn"),
            "error message must mention hash mismatch or torn pair, got: {err}"
        );
    }

    #[test]
    fn ann_bridge_load_count_mismatch_err() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let (bridge, _) = build_test_bridge(4, 2);
        bridge.save_atomic(dir.path()).expect("save_atomic");

        // Read back the sidecar, parse content_hash, then rewrite with wrong count.
        let sidecar_bytes =
            std::fs::read(dir.path().join("external_ids.bin")).expect("read sidecar");
        // content_hash lives at bytes[8..40]; reuse it. Write count=99 instead of 2.
        let mut new_sidecar: Vec<u8> = Vec::with_capacity(48 + 99 * 16);
        new_sidecar.extend_from_slice(b"KHVANIDS");
        new_sidecar.extend_from_slice(&sidecar_bytes[8..40]); // original content_hash
        new_sidecar.extend_from_slice(&99u64.to_le_bytes()); // wrong count
        new_sidecar.extend(std::iter::repeat_n(0u8, 99 * 16)); // 99 zero UUIDs
        std::fs::write(dir.path().join("external_ids.bin"), &new_sidecar)
            .expect("write patched sidecar");

        let result = AnnBridge::load(dir.path());
        assert!(
            result.is_err(),
            "load must fail when sidecar count != index.num_vectors()"
        );
    }

    #[test]
    fn ann_bridge_load_bad_magic_err() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let (bridge, _) = build_test_bridge(4, 2);
        bridge.save_atomic(dir.path()).expect("save_atomic");

        // Overwrite the first 8 bytes with a wrong magic.
        let mut sidecar_bytes =
            std::fs::read(dir.path().join("external_ids.bin")).expect("read sidecar");
        sidecar_bytes[0..8].copy_from_slice(b"WRONGMAG");
        std::fs::write(dir.path().join("external_ids.bin"), &sidecar_bytes)
            .expect("write bad-magic sidecar");

        let result = AnnBridge::load(dir.path());
        assert!(
            result.is_err(),
            "load must fail when external_ids.bin has wrong magic"
        );
        let err = result.err().expect("already asserted is_err");
        assert!(
            err.contains("magic"),
            "error must mention magic mismatch, got: {err}"
        );
    }

    // ── slice 1b-ii-a: warm-path tests (ADR-079) ─────────────────────────────

    use async_trait::async_trait;
    use khive_runtime::{AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig};
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use tempfile::TempDir;

    const WARM_TEST_MODEL: &str = "ann-test-model";
    const WARM_DIMS: usize = 4;

    struct ConstVecService;

    #[async_trait]
    impl EmbeddingService for ConstVecService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|_| vec![1.0_f32; WARM_DIMS]).collect())
        }

        fn supports_model(&self, _: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "const-vec"
        }
    }

    struct TestEmbedderProvider;

    #[async_trait]
    impl EmbedderProvider for TestEmbedderProvider {
        fn name(&self) -> &str {
            WARM_TEST_MODEL
        }

        fn dimensions(&self) -> usize {
            WARM_DIMS
        }

        async fn build(&self) -> khive_runtime::RuntimeResult<Arc<dyn EmbeddingService>> {
            Ok(Arc::new(ConstVecService))
        }
    }

    fn file_rt_with_embedder(db_path: std::path::PathBuf) -> KhiveRuntime {
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(db_path),
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "knowledge".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        })
        .expect("file-backed runtime");
        rt.register_embedder(TestEmbedderProvider);
        rt
    }

    /// Seed `n` distinct rows into the vec0 table for `WARM_TEST_MODEL`.
    ///
    /// Calls `rt.vectors_for_model` first so the virtual table is created, then
    /// inserts raw f32 LE blobs directly via SQL.
    async fn seed_warm_corpus(rt: &KhiveRuntime, token: &NamespaceToken, n: usize) {
        let _store = rt
            .vectors_for_model(token, WARM_TEST_MODEL)
            .expect("vec store");
        let model_key = sanitize_model_key(WARM_TEST_MODEL);
        let table = format!("vec_{model_key}");
        let ns = token.namespace().as_str().to_owned();
        let sql = rt.sql();
        let mut w = sql.writer().await.expect("writer");
        for i in 0..n {
            let id = Uuid::new_v4();
            let mut v = [0.0_f32; WARM_DIMS];
            v[i % WARM_DIMS] = 1.0;
            let bytes: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
            w.execute(SqlStatement {
                sql: format!(
                    "INSERT INTO {table} \
                     (subject_id, namespace, kind, field, embedding_model, embedding) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
                ),
                params: vec![
                    SqlValue::Text(id.to_string()),
                    SqlValue::Text(ns.clone()),
                    SqlValue::Text("concept".to_string()),
                    SqlValue::Text("knowledge.atom".to_string()),
                    SqlValue::Text(WARM_TEST_MODEL.to_string()),
                    SqlValue::Blob(bytes),
                ],
                label: None,
            })
            .await
            .expect("insert corpus row");
        }
    }

    /// `ann_segment_dir` encodes a round-trippable hex key that `decode_ann_dir_name` reverses.
    #[tokio::test]
    async fn ann_segment_dir_encode_decode_round_trip() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let seg_dir = ann_segment_dir(&rt, "local", WARM_TEST_MODEL)
            .expect("file backend must return Some(seg_dir)");

        let dir_name = seg_dir
            .file_name()
            .expect("seg_dir must have a basename")
            .to_string_lossy()
            .into_owned();

        let (decoded_ns, decoded_model) =
            decode_ann_dir_name(&dir_name).expect("decode must succeed for a valid encode");
        assert_eq!(decoded_ns, "local");
        assert_eq!(decoded_model, WARM_TEST_MODEL);

        // Parent directory is `data_dir/ann/`.
        let parent = seg_dir.parent().expect("seg_dir must have a parent");
        assert_eq!(
            parent.file_name().unwrap().to_string_lossy(),
            "ann",
            "seg_dir parent must be named 'ann'"
        );
    }

    /// `ensure_ann_for_model` must not panic on an in-memory runtime (no data_dir).
    #[tokio::test]
    async fn ensure_ann_no_data_dir_does_not_panic() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ann = new_shared();
        let token = rt.authorize(Namespace::local()).expect("authorize");
        // No data_dir → v2 path skipped. No corpus → no rebuild. Must complete silently.
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        assert!(
            !ann.indexes.read().await.contains_key(&key),
            "no index should be loaded when corpus is empty and model is unknown"
        );
    }

    /// Cold-start build persists v2 segments; a second call restores from disk.
    ///
    /// Also gates the normalize-once invariant: `live_content_hash` must equal the
    /// persisted commit hash, proving the Hot branch can fire without double-normalization.
    #[tokio::test]
    async fn ensure_ann_round_trip_hot() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 4).await;

        // Cold-start: rebuild from corpus, persist v2 segments.
        let ann = new_shared();
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        assert!(
            ann.indexes.read().await.contains_key(&key),
            "first call must build the ANN index"
        );

        // Normalize-once invariant: the live corpus hash must equal the persisted
        // commit hash.  A double-normalize bug produces always-stale hashes here.
        let seg_dir = ann_segment_dir(&rt, "local", WARM_TEST_MODEL)
            .expect("file backend must have a seg_dir");
        assert!(
            seg_dir.join("metadata.bin").exists(),
            "first call must persist v2 segments (metadata.bin missing)"
        );
        let persisted = read_commit_fingerprint(&seg_dir)
            .expect("read_commit_fingerprint must not err")
            .expect("metadata.bin must carry a v2 fingerprint");
        let live = live_content_hash(&rt, &token, WARM_TEST_MODEL)
            .await
            .expect("live_content_hash must return Some for a seeded corpus");
        assert_eq!(
            live, persisted.content_hash,
            "live_content_hash must equal persisted content_hash (normalize-once invariant)"
        );

        // Hot path: load from persisted v2 segments without rebuilding. A rebuild
        // would call save_atomic and rewrite metadata.bin (new inode); a true Hot
        // load via AnnBridge::load never writes. Asserting the inode is unchanged
        // proves the second call took the v2 Hot branch, not a silent rebuild.
        use std::os::unix::fs::MetadataExt;
        let meta_path = seg_dir.join("metadata.bin");
        let ino_before = std::fs::metadata(&meta_path)
            .expect("metadata.bin must exist after first build")
            .ino();
        let ann2 = new_shared();
        ensure_ann_for_model(&rt, &token, &ann2, WARM_TEST_MODEL).await;
        assert!(
            ann2.indexes.read().await.contains_key(&key),
            "second call must restore the ANN index from v2 segments"
        );
        let ino_after = std::fs::metadata(&meta_path)
            .expect("metadata.bin must still exist")
            .ino();
        assert_eq!(
            ino_before, ino_after,
            "second call must NOT rewrite metadata.bin — proves the v2 Hot load path, not a rebuild"
        );
    }

    /// After a corpus mutation the persisted v2 segment is stale and triggers a rebuild.
    ///
    /// Proves the stale branch is actually exercised (not silently falling through to rebuild),
    /// and that the rebuild re-persists v2 segments matching the mutated corpus.
    #[tokio::test]
    async fn ensure_ann_stale_rebuild() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 4).await;

        // Initial build: persist v2 segments.
        let ann = new_shared();
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        assert!(ann.indexes.read().await.contains_key(&key), "initial build");

        // Mutate corpus: add one more row.
        seed_warm_corpus(&rt, &token, 1).await;

        // Gate: after mutation, live hash must DIFFER from persisted — proves the stale
        // branch will fire (not silently succeed with the same hash).
        let seg_dir = ann_segment_dir(&rt, "local", WARM_TEST_MODEL)
            .expect("file backend must have a seg_dir");
        let persisted_before = read_commit_fingerprint(&seg_dir)
            .expect("read_commit_fingerprint must not err")
            .expect("v2 fingerprint must be present after initial build");
        let live_after_mutation = live_content_hash(&rt, &token, WARM_TEST_MODEL)
            .await
            .expect("live_content_hash must return Some");
        assert_ne!(
            live_after_mutation, persisted_before.content_hash,
            "mutation must make live hash differ from persisted (stale-branch pre-condition)"
        );

        // Fresh SharedAnn: stale v2 detected → rebuild from corpus.
        let ann2 = new_shared();
        ensure_ann_for_model(&rt, &token, &ann2, WARM_TEST_MODEL).await;
        assert!(
            ann2.indexes.read().await.contains_key(&key),
            "must rebuild the index after corpus mutation (stale v2 segment rejected)"
        );

        // Rebuild must re-persist v2 segments matching the mutated (5-row) corpus.
        let persisted_after = read_commit_fingerprint(&seg_dir)
            .expect("read_commit_fingerprint after rebuild must not err")
            .expect("v2 fingerprint must be present after rebuild");
        let live_final = live_content_hash(&rt, &token, WARM_TEST_MODEL)
            .await
            .expect("live_content_hash must return Some after rebuild");
        assert_eq!(
            live_final, persisted_after.content_hash,
            "rebuild must re-persist v2 matching the mutated corpus"
        );
        assert_eq!(
            persisted_after.vector_count, 5,
            "re-persisted segment must reflect the 5-row corpus (4 initial + 1 mutation)"
        );
    }

    /// `ensure_ann_for_model`'s fast path must treat a present-but-generation-stale
    /// cached entry as a miss, not a hit (PR #815). In production
    /// `install_if_fresher`'s own fencing prevents a stale entry from ever
    /// installing, so this test bumps the namespace generation directly
    /// (bypassing `clear_namespace`'s eviction) to construct the "present but
    /// stale" state as an independent, defense-in-depth check on the fast path
    /// itself — mere presence must never again be trusted as freshness.
    #[tokio::test]
    async fn ensure_ann_fast_path_ignores_generation_stale_cached_entry() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 4).await;

        let ann = new_shared();
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        assert!(
            ann.indexes.read().await.contains_key(&key),
            "setup: first call must build and install the index at generation 0"
        );

        // Bump the namespace's generation directly, leaving the generation-0
        // entry present — the state install_if_fresher's fencing prevents in
        // production, exercised here purely to isolate the fast-path check.
        bump_generation(&ann, "local");
        assert_eq!(current_generation(&ann, "local"), 1);

        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;

        // If the in-memory fast path had (incorrectly) treated mere presence
        // as a hit, it would return immediately and the cached entry's
        // generation would still read 0. Falling through re-stamps it with
        // the namespace's current generation (1) via the v2/rebuild paths —
        // proof the stale entry was NOT served as a hit.
        assert_eq!(
            ann.indexes
                .read()
                .await
                .get(&key)
                .expect("entry present")
                .generation,
            1,
            "a present-but-generation-stale entry must not short-circuit via the fast \
             path; the reloaded/rebuilt entry must be re-stamped with the namespace's \
             new current generation"
        );
    }

    /// `warm_known_snapshots` must warm v2 segments even when the legacy
    /// `retrieval_snapshots` table is absent (the v1 query errors). Pre-fix it
    /// early-returned on that error and never reached the filesystem segment
    /// enumeration, so v2-only databases never warmed at daemon startup.
    #[tokio::test]
    async fn warm_known_snapshots_v2_only_no_legacy_table() {
        let dir = TempDir::new().expect("tempdir");
        let rt = file_rt_with_embedder(dir.path().join("test.db"));
        let token = rt.authorize(Namespace::local()).expect("authorize");
        seed_warm_corpus(&rt, &token, 4).await;

        // Setup: build + persist v2 segments to data_dir/ann/<hex>/.
        let ann = new_shared();
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
        let key = AnnKey::new("local", WARM_TEST_MODEL);
        assert!(
            ann.indexes.read().await.contains_key(&key),
            "setup: first ensure must persist v2 segments"
        );

        // Force the worst case the fix targets: the v1 table is absent, so the
        // legacy query errors. Pre-fix, that error aborted the whole warm pass.
        {
            let sql = rt.sql();
            let mut w = sql.writer().await.expect("writer");
            w.execute(SqlStatement {
                sql: "DROP TABLE IF EXISTS retrieval_snapshots".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("drop retrieval_snapshots");
        }

        // Cold cache + warm: the v2 filesystem enumeration must still warm the
        // key despite the v1 query error.
        let ann_fresh = new_shared();
        warm_known_snapshots(&rt, &ann_fresh).await;
        assert!(
            ann_fresh.indexes.read().await.contains_key(&key),
            "warm_known_snapshots must warm v2 segments when retrieval_snapshots is absent \
             (regression: a v1 query error must not abort the v2 filesystem pass)"
        );
    }
}
