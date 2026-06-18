//! Warm ANN bridge: wraps `VamanaIndex` per model to cache memory-note vector search.
//! One index per model covers all namespaces; namespace filtering is applied at recall time.

use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_vamana::{CorpusFingerprint, VamanaConfig, VamanaIndex, VamanaSnapshot};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

// ── types ─────────────────────────────────────────────────────────────────────

/// Cache key for a per-model ANN slot (one index per model, all namespaces combined).
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct AnnKey {
    pub(crate) model: String,
}

impl AnnKey {
    pub(crate) fn new(_namespace: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
        }
    }

    pub(crate) fn from_token(_token: &NamespaceToken, model: &str) -> Self {
        Self {
            model: model.to_owned(),
        }
    }
}

pub(crate) struct AnnBridge {
    index: VamanaIndex,
    id_map: Vec<Uuid>,
}

/// Shared ANN state: per-`(namespace, model)` indexes with at-most-one-background-build guard.
pub(crate) struct AnnState {
    indexes: RwLock<HashMap<AnnKey, AnnBridge>>,
    warming: Mutex<HashSet<AnnKey>>,
    /// Counts how many times `search_loaded` returned a warm hit. Test-only;
    /// call `reset_warm_route_count()` between operations to isolate counts.
    #[cfg(test)]
    pub(crate) warm_route_count: AtomicUsize,
}

pub(crate) type SharedAnn = Arc<AnnState>;

pub(crate) fn new_shared() -> SharedAnn {
    Arc::new(AnnState {
        indexes: RwLock::new(HashMap::new()),
        warming: Mutex::new(HashSet::new()),
        #[cfg(test)]
        warm_route_count: AtomicUsize::new(0),
    })
}

#[cfg(test)]
impl AnnState {
    pub(crate) fn warm_route_count(&self) -> usize {
        self.warm_route_count.load(Ordering::SeqCst)
    }

    pub(crate) fn reset_warm_route_count(&self) {
        self.warm_route_count.store(0, Ordering::SeqCst);
    }
}

// ── AnnBridge ─────────────────────────────────────────────────────────────────

impl AnnBridge {
    pub(crate) fn build(
        mut vectors: Vec<f32>,
        dim: usize,
        id_map: Vec<Uuid>,
    ) -> Result<Self, RuntimeError> {
        if dim == 0 {
            return Err(RuntimeError::Internal("dimension must be > 0".into()));
        }
        if vectors.is_empty() || id_map.is_empty() {
            return Err(RuntimeError::Internal(
                "no vectors to build ANN index from".into(),
            ));
        }
        let n = vectors.len() / dim;
        if n != id_map.len() {
            return Err(RuntimeError::Internal(format!(
                "id_map length {} != vector count {}",
                id_map.len(),
                n
            )));
        }
        for row in vectors.chunks_exact_mut(dim) {
            l2_normalize(row);
        }
        let cfg = VamanaConfig::with_dimensions(dim);
        let index =
            VamanaIndex::build(&vectors, cfg).map_err(|e| RuntimeError::Internal(e.to_string()))?;
        Ok(Self { index, id_map })
    }

    pub(crate) fn search(&self, query: &[f32], k: usize) -> Result<Vec<(Uuid, f32)>, RuntimeError> {
        let mut q = query.to_vec();
        l2_normalize(&mut q);
        let raw = self
            .index
            .search(&q, k)
            .map_err(|e| RuntimeError::Internal(format!("memory ANN search: {e}")))?;
        let hits = raw
            .into_iter()
            .filter_map(|(idx, dist)| {
                self.id_map.get(idx as usize).map(|uuid| {
                    // L2² → cosine for unit vectors: cos(a,b) = 1 - ||a-b||²/2
                    let cosine = (1.0 - dist / 2.0).max(0.0);
                    (*uuid, cosine)
                })
            })
            .collect();
        Ok(hits)
    }

    pub(crate) fn to_snapshot(
        &self,
        namespace: &str,
        model: &str,
        fingerprint: CorpusFingerprint,
    ) -> Result<VamanaSnapshot, khive_vamana::VamanaError> {
        let external_ids: Vec<String> = self.id_map.iter().map(|id| id.to_string()).collect();
        self.index
            .to_snapshot(namespace, model, fingerprint, external_ids)
    }

    pub(crate) fn from_snapshot(snapshot: VamanaSnapshot) -> Result<Self, RuntimeError> {
        let id_map: Vec<Uuid> = snapshot
            .external_ids
            .iter()
            .map(|s| {
                Uuid::parse_str(s).map_err(|e| RuntimeError::Internal(format!("bad UUID {s}: {e}")))
            })
            .collect::<Result<_, _>>()?;
        let index = VamanaIndex::from_snapshot(&snapshot)
            .map_err(|e| RuntimeError::Internal(format!("snapshot restore: {e}")))?;
        Ok(Self { index, id_map })
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

// ── helpers ───────────────────────────────────────────────────────────────────

/// Replace non-alphanumeric chars with `_` to produce a valid table-name suffix.
pub(crate) fn sanitize_model_key(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Snapshot key for the global memory Vamana index for a model.
/// Distinct from knowledge's `{ns}::vamana::{model}` to prevent corpus identity collisions.
pub(crate) fn snapshot_key(_namespace: &str, model: &str) -> String {
    format!("global::memory_vamana::{model}")
}

const MEMORY_VAMANA_INDEX_TYPE: &str = "memory_vamana";

/// Status returned by `ensure_ann_for_model` so callers can log/act on the
/// build outcome without parsing log lines.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AnnEnsureStatus {
    AlreadyLoaded,
    LoadedSnapshot,
    Built { vectors: usize },
    EmptyCorpus,
    DiscardedStaleBuild,
}

// ── state operations ──────────────────────────────────────────────────────────

/// Search the already-loaded index for `key`. Returns `Ok(None)` on cache miss,
/// `Ok(Some(hits))` on success, `Err` on ANN search failure (caller falls back).
pub(crate) async fn search_loaded(
    ann: &SharedAnn,
    key: &AnnKey,
    query: &[f32],
    k: usize,
) -> Result<Option<Vec<(Uuid, f32)>>, RuntimeError> {
    let guard = ann.indexes.read().await;
    match guard.get(key) {
        None => Ok(None),
        Some(bridge) => {
            #[cfg(test)]
            ann.warm_route_count.fetch_add(1, Ordering::SeqCst);
            bridge.search(query, k).map(Some)
        }
    }
}

/// Remove a single in-memory ANN slot and its warming guard entry.
pub(crate) async fn clear_key(ann: &SharedAnn, key: &AnnKey) {
    ann.indexes.write().await.remove(key);
    ann.warming.lock().await.remove(key);
}

/// Remove all in-memory ANN slots and warming-guard entries.
/// Because the index is global per model, any namespace write invalidates all slots.
pub(crate) async fn clear_namespace(ann: &SharedAnn, _namespace: &str) {
    ann.indexes.write().await.clear();
    ann.warming.lock().await.clear();
}

/// Clear in-memory cache and delete persisted snapshots.
pub(crate) async fn invalidate_namespace(rt: &KhiveRuntime, ann: &SharedAnn, _namespace: &str) {
    clear_namespace(ann, _namespace).await;
    invalidate_snapshots(rt).await;
}

/// Fire-once per-model background warm. Returns `true` if a new task was started.
pub(crate) async fn ensure_ann_background(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
) -> bool {
    if model.is_empty() {
        return false;
    }
    let key = AnnKey::from_token(token, model);

    // Fast path: already loaded.
    if ann.indexes.read().await.contains_key(&key) {
        return false;
    }

    // Single-flight guard.
    {
        let mut warming = ann.warming.lock().await;
        if warming.contains(&key) {
            return false;
        }
        warming.insert(key.clone());
    }

    let rt = rt.clone();
    let ann = ann.clone();
    let model = model.to_owned();
    tokio::spawn(async move {
        if let Ok(token) = rt.authorize(Namespace::local()) {
            match ensure_ann_for_model(&rt, &token, &ann, &model).await {
                Ok(status) => {
                    tracing::debug!(?status, model = %model, "memory ANN background warm complete");
                }
                Err(e) => {
                    tracing::warn!(error = %e, model = %model, "memory ANN background build failed");
                }
            }
        }
        // If loading/building failed, remove the guard so a later recall retries.
        let loaded = ann.indexes.read().await.contains_key(&key);
        if !loaded {
            ann.warming.lock().await.remove(&key);
        }
    });
    true
}

/// Warm the global per-model ANN indexes at startup — skips already-loaded keys.
pub(crate) async fn warm_existing_memory_indexes(rt: &KhiveRuntime, ann: &SharedAnn) {
    let models = rt.registered_embedding_model_names();
    for model in &models {
        let token = match rt.authorize(Namespace::local()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        match ensure_ann_for_model(rt, &token, ann, model).await {
            Ok(status) => {
                tracing::debug!(?status, model = %model, "memory ANN warm complete");
            }
            Err(e) => {
                tracing::warn!(error = %e, model = %model, "memory ANN warm failed");
            }
        }
    }
}

/// Lazy warm-load for the global index for `model`: snapshot restore or rebuild with double-fingerprint check.
pub(crate) async fn ensure_ann_for_model(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
) -> Result<AnnEnsureStatus, RuntimeError> {
    if model.is_empty() {
        return Ok(AnnEnsureStatus::EmptyCorpus);
    }
    let ns = "global";
    let key = AnnKey::new(ns, model);

    if ann.indexes.read().await.contains_key(&key) {
        return Ok(AnnEnsureStatus::AlreadyLoaded);
    }

    // Try snapshot warm-load.
    if let Some(snapshot) = try_load_snapshot(rt, ns, model).await {
        let current_fp = compute_memory_fingerprint(rt, token, model).await;
        if let Some(fp) = current_fp {
            if snapshot.fingerprint == fp {
                match AnnBridge::from_snapshot(snapshot) {
                    Ok(bridge) => {
                        ann.indexes.write().await.entry(key).or_insert(bridge);
                        tracing::debug!(namespace = %ns, model = %model, "memory ANN loaded from snapshot");
                        return Ok(AnnEnsureStatus::LoadedSnapshot);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "corrupt memory Vamana snapshot; rebuilding");
                    }
                }
            } else {
                tracing::info!(
                    namespace = %ns,
                    model = %model,
                    "stale memory Vamana snapshot (fingerprint mismatch); rebuilding"
                );
            }
        }
    }

    // Rebuild from vector store with double-fingerprint concurrency check.
    let fp_before = compute_memory_fingerprint(rt, token, model).await;
    match load_and_build_from_vector_store(rt, token, model).await {
        Ok(Some(bridge)) => {
            let fp_after = compute_memory_fingerprint(rt, token, model).await;
            // If fingerprint changed during the scan, the corpus raced; discard.
            if fp_before != fp_after {
                tracing::debug!(
                    namespace = %ns,
                    model = %model,
                    "memory ANN corpus mutated during build; discarding"
                );
                return Ok(AnnEnsureStatus::DiscardedStaleBuild);
            }
            let vector_count = bridge.id_map.len();
            if let Some(fingerprint) = fp_after {
                if let Err(e) = persist_snapshot(rt, ns, model, &bridge, fingerprint).await {
                    tracing::warn!(error = %e, "failed to persist memory Vamana snapshot");
                }
            }
            ann.indexes.write().await.entry(key).or_insert(bridge);
            tracing::debug!(namespace = %ns, model = %model, vectors = vector_count, "memory ANN index built");
            Ok(AnnEnsureStatus::Built {
                vectors: vector_count,
            })
        }
        Ok(None) => {
            tracing::debug!(namespace = %ns, model = %model, "memory ANN: no note vectors to build");
            Ok(AnnEnsureStatus::EmptyCorpus)
        }
        Err(e) => {
            tracing::warn!(error = %e, namespace = %ns, model = %model, "memory ANN build failed");
            Err(e)
        }
    }
}

// ── corpus loading ────────────────────────────────────────────────────────────

/// Compute a fingerprint for all non-deleted memory note vectors for `model` (all namespaces).
async fn compute_memory_fingerprint(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Option<CorpusFingerprint> {
    let store = rt.vectors_for_model(token, model).ok()?;
    let info = store.info().await.ok()?;
    let table_name = format!("vec_{}", sanitize_model_key(model));
    let sql = rt.sql();
    let mut reader = sql.reader().await.ok()?;
    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT COUNT(*) AS n FROM {table_name} v \
                 JOIN notes n ON n.id = v.subject_id \
                 WHERE v.embedding_model = ?1 \
                   AND v.kind = 'note' AND v.field = 'note.content' \
                   AND n.deleted_at IS NULL"
            ),
            params: vec![SqlValue::Text(model.to_owned())],
            label: Some("memory_ann_fingerprint".into()),
        })
        .await
        .ok()?;
    let vector_count = match rows.first()?.get("n")? {
        SqlValue::Integer(n) if *n >= 0 => *n as u64,
        _ => return None,
    };
    Some(CorpusFingerprint {
        vector_count,
        dimensions: info.dimensions as u32,
    })
}

/// Scan all non-deleted `note.content` vectors across all namespaces for `model` and build an
/// `AnnBridge`. Returns `Ok(None)` when the corpus is empty or inaccessible.
async fn load_and_build_from_vector_store(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
) -> Result<Option<AnnBridge>, RuntimeError> {
    let store = match rt.vectors_for_model(token, model) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let info = store
        .info()
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;
    if info.dimensions == 0 {
        return Ok(None);
    }
    let dims = info.dimensions;

    let model_key = sanitize_model_key(model);
    let table_name = format!("vec_{model_key}");

    let sql = rt.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;

    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT v.subject_id, v.embedding FROM {table_name} v \
                 JOIN notes n ON n.id = v.subject_id \
                 WHERE v.embedding_model = ?1 \
                   AND v.kind = 'note' AND v.field = 'note.content' \
                   AND n.deleted_at IS NULL \
                 ORDER BY v.subject_id"
            ),
            params: vec![SqlValue::Text(model.to_owned())],
            label: Some("memory_ann_corpus_scan".into()),
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

    AnnBridge::build(flat, dims, id_map).map(Some)
}

// ── persistence ───────────────────────────────────────────────────────────────

async fn ensure_snapshot_schema(rt: &KhiveRuntime) -> Result<(), RuntimeError> {
    let sql = rt.sql();
    let mut w = sql
        .writer()
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;
    w.execute_script(
        r#"
        CREATE TABLE IF NOT EXISTS retrieval_snapshots (
            namespace   TEXT NOT NULL,
            index_type  TEXT NOT NULL,
            snapshot    BLOB NOT NULL,
            created_at  INTEGER NOT NULL,
            PRIMARY KEY (namespace, index_type)
        );
        CREATE INDEX IF NOT EXISTS idx_retrieval_snapshots_namespace
            ON retrieval_snapshots(namespace);
        "#
        .into(),
    )
    .await
    .map_err(|e| RuntimeError::Internal(e.to_string()))
}

async fn persist_snapshot(
    rt: &KhiveRuntime,
    namespace: &str,
    model: &str,
    bridge: &AnnBridge,
    fingerprint: CorpusFingerprint,
) -> Result<(), RuntimeError> {
    if let Err(e) = ensure_snapshot_schema(rt).await {
        tracing::warn!(error = %e, "failed to create retrieval_snapshots schema");
        return Err(e);
    }
    let snapshot = bridge
        .to_snapshot(namespace, model, fingerprint)
        .map_err(|e| RuntimeError::Internal(format!("to_snapshot: {e}")))?;
    let blob = serde_json::to_vec(&snapshot)
        .map_err(|e| RuntimeError::Internal(format!("snapshot serialize: {e}")))?;
    let key = snapshot_key(namespace, model);
    let sql = rt.sql();
    let mut w = sql
        .writer()
        .await
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;
    w.execute(SqlStatement {
        sql: "INSERT OR REPLACE INTO retrieval_snapshots \
              (namespace, index_type, snapshot, created_at) VALUES (?1, ?2, ?3, ?4)"
            .into(),
        params: vec![
            SqlValue::Text(key),
            SqlValue::Text(MEMORY_VAMANA_INDEX_TYPE.into()),
            SqlValue::Blob(blob),
            SqlValue::Integer(0),
        ],
        label: Some("persist_memory_vamana_snapshot".into()),
    })
    .await
    .map(|_| ())
    .map_err(|e| RuntimeError::Internal(e.to_string()))
}

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
            params: vec![
                SqlValue::Text(key),
                SqlValue::Text(MEMORY_VAMANA_INDEX_TYPE.into()),
            ],
            label: None,
        })
        .await
        .ok()?;
    let row = rows.into_iter().next()?;
    let blob = match row.get("snapshot")? {
        SqlValue::Blob(b) => b.clone(),
        _ => return None,
    };
    match serde_json::from_slice::<VamanaSnapshot>(&blob) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(error = %e, "corrupt memory Vamana snapshot blob");
            None
        }
    }
}

/// Delete the global memory Vamana snapshot rows from `retrieval_snapshots`.
/// Best-effort — missing table is silently ignored.
async fn invalidate_snapshots(rt: &KhiveRuntime) {
    let pattern = "global::memory_vamana::%".to_string();
    let sql = rt.sql();
    let mut w = match sql.writer().await {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "failed to open writer for memory ANN snapshot invalidation");
            return;
        }
    };
    match w
        .execute(SqlStatement {
            sql: "DELETE FROM retrieval_snapshots WHERE namespace LIKE ?1".into(),
            params: vec![SqlValue::Text(pattern)],
            label: Some("invalidate_memory_vamana_snapshot".into()),
        })
        .await
    {
        Ok(_) => {}
        Err(e) if e.to_string().contains("no such table") => {}
        Err(e) => {
            tracing::warn!(error = %e, "failed to invalidate memory Vamana snapshots");
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ann_key_is_model_only() {
        // After FTS+ANN consolidation AnnKey is model-only; namespace is ignored.
        let k1 = AnnKey::new("ns:a", "model-x");
        let k2 = AnnKey::new("ns:b", "model-x"); // same model, different ns → same key
        let k3 = AnnKey::new("ns:a", "model-y"); // different model → different key
        assert_eq!(
            k1, k2,
            "same model, different namespace must produce the same key"
        );
        assert_ne!(k1, k3, "different models must produce different keys");
    }

    #[test]
    fn ann_bridge_maps_vamana_ids_to_uuids() {
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let id_c = Uuid::new_v4();

        // 3 orthogonal unit vectors in 3D
        let vectors = vec![
            1.0f32, 0.0, 0.0, // id_a
            0.0, 1.0, 0.0, // id_b
            0.0, 0.0, 1.0, // id_c
        ];
        let bridge = AnnBridge::build(vectors, 3, vec![id_a, id_b, id_c]).expect("build");

        // query close to id_a
        let hits = bridge.search(&[1.0, 0.0, 0.0], 1).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, id_a, "nearest to [1,0,0] must be id_a");
        assert!(hits[0].1 > 0.9, "cosine must be close to 1.0");
    }

    #[test]
    fn ann_search_dimension_error_returns_err() {
        let id = Uuid::new_v4();
        let bridge =
            AnnBridge::build(vec![1.0f32, 0.0, 0.0], 3, vec![id]).expect("build 3-dim bridge");
        // query with wrong dimension (2 instead of 3)
        let result = bridge.search(&[1.0, 0.0], 1);
        assert!(result.is_err(), "wrong dimension must return Err");
    }

    #[test]
    fn snapshot_key_does_not_collide_with_knowledge_vamana() {
        let mem_key = snapshot_key("local", "all-minilm-l6-v2");
        assert!(
            mem_key.contains("::memory_vamana::"),
            "memory key must contain ::memory_vamana:: but got: {mem_key}"
        );
        assert!(
            !mem_key.contains("::vamana::"),
            "memory key must not match knowledge pattern ::vamana:: but got: {mem_key}"
        );
    }

    #[tokio::test]
    async fn invalidate_namespace_clears_global_index() {
        // After FTS+ANN consolidation, AnnKey is model-only (namespace is ignored).
        // invalidate_namespace clears the single global index for all models.
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ann = new_shared();
        let model_a = "model-a";
        let model_b = "model-b";

        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();

        let bridge_a =
            AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![id_a]).expect("build a");
        let bridge_b =
            AnnBridge::build(vec![0.0f32, 1.0, 0.0, 0.0], 4, vec![id_b]).expect("build b");

        // Keys are model-only; namespace arg is ignored.
        let key_a = AnnKey::new("any-ns", model_a);
        let key_b = AnnKey::new("any-ns", model_b);

        {
            let mut idxs = ann.indexes.write().await;
            idxs.insert(key_a.clone(), bridge_a);
            idxs.insert(key_b.clone(), bridge_b);
        }
        {
            let mut warming = ann.warming.lock().await;
            warming.insert(key_a.clone());
            warming.insert(key_b.clone());
        }

        // invalidate_namespace evicts ALL in-memory indexes (global index serves all namespaces).
        invalidate_namespace(&rt, &ann, "any-ns").await;

        assert!(
            ann.indexes.read().await.is_empty(),
            "all indexes must be cleared after invalidation"
        );
        assert!(
            ann.warming.lock().await.is_empty(),
            "all warming guards must be cleared after invalidation"
        );
    }
}
