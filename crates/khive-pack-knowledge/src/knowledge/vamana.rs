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
//! Persistence: `ensure_ann_for_model` loads a validated snapshot from `retrieval_snapshots`
//! on first access; stale/missing snapshots trigger a full rebuild + re-persist.
//! `kkernel reindex` actively invalidates snapshots after re-embedding (second
//! line of staleness defence).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_vamana::{CorpusFingerprint, VamanaConfig, VamanaIndex, VamanaSnapshot};
use tokio::sync::RwLock;
use uuid::Uuid;

pub(crate) struct AnnBridge {
    index: VamanaIndex,
    id_map: Vec<Uuid>,
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
}

pub(crate) type SharedAnn = Arc<AnnState>;

pub(crate) fn new_shared() -> SharedAnn {
    Arc::new(AnnState {
        indexes: RwLock::new(HashMap::new()),
        warming: std::sync::Mutex::new(HashSet::new()),
    })
}

/// Insert `bridge` under `key` only if the slot is empty. Returns `true` when
/// the bridge was inserted, `false` if the key was already present.
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
    ann.indexes
        .write()
        .await
        .retain(|k, _| k.namespace != namespace);
    ann.warming
        .lock()
        .expect("warming lock")
        .retain(|k| k.namespace != namespace);
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
    let in_warming = ann.warming.lock().expect("warming lock").contains(key);
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
        Ok(Self { index, id_map })
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

    pub fn to_vamana_snapshot(
        &self,
        namespace: &str,
        model: &str,
        fingerprint: CorpusFingerprint,
    ) -> Result<VamanaSnapshot, khive_vamana::VamanaError> {
        let external_ids: Vec<String> = self.id_map.iter().map(|id| id.to_string()).collect();
        self.index
            .to_snapshot(namespace, model, fingerprint, external_ids)
    }

    pub fn from_vamana_snapshot(snapshot: VamanaSnapshot) -> Result<Self, String> {
        let id_map: Vec<Uuid> = snapshot
            .external_ids
            .iter()
            .map(|s| Uuid::parse_str(s).map_err(|e| format!("bad UUID {s}: {e}")))
            .collect::<Result<_, _>>()?;
        let index =
            VamanaIndex::from_snapshot(&snapshot).map_err(|e| format!("snapshot restore: {e}"))?;
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

// ── persistence helpers ───────────────────────────────────────────────────────

/// Namespace key used in `retrieval_snapshots` for a given ns+model pair.
pub(crate) fn snapshot_key(namespace: &str, model: &str) -> String {
    format!("{namespace}::vamana::{model}")
}

/// Model-key sanitization — must match `khive_runtime::sanitize_key`.
pub(crate) fn sanitize_model_key(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Create `retrieval_snapshots` if it does not exist yet.
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

/// Persist `bridge` as a Vamana snapshot under `{namespace}::vamana::{model}`.
pub(crate) async fn persist_snapshot(
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
        .to_vamana_snapshot(namespace, model, fingerprint)
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
            SqlValue::Text("vamana".into()),
            SqlValue::Blob(blob),
            SqlValue::Integer(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_micros() as i64,
            ),
        ],
        label: Some("persist_vamana_snapshot".into()),
    })
    .await
    .map_err(|e| RuntimeError::Internal(e.to_string()))?;

    Ok(())
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

/// Scan the sqlite-vec table and build a fresh `AnnBridge`.
///
/// Returns `None` when there are no vectors or the model is not configured.
pub(crate) async fn load_and_build_from_vector_store(
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

    AnnBridge::build(flat, dims, id_map)
        .map(Some)
        .map_err(RuntimeError::Internal)
}

/// Delete all Vamana snapshots for `namespace` from `retrieval_snapshots`.
///
/// Called after any vector-corpus mutation to guarantee `ensure_ann_for_model` cannot
/// load a snapshot that no longer matches the live corpus.  Best-effort: if
/// the `retrieval_snapshots` table doesn't exist yet, the call is a no-op.
pub(crate) async fn invalidate_snapshot(rt: &KhiveRuntime, namespace: &str) {
    let pattern = format!("{namespace}::vamana::%");
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
            sql: "DELETE FROM retrieval_snapshots WHERE namespace LIKE ?1".into(),
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
    let sql = rt.sql();
    let mut reader = match sql.reader().await {
        Ok(r) => r,
        Err(_) => return,
    };

    let rows = match reader
        .query_all(SqlStatement {
            sql: "SELECT DISTINCT namespace FROM retrieval_snapshots WHERE namespace LIKE ?1"
                .into(),
            params: vec![SqlValue::Text("%::vamana::%".into())],
            label: None,
        })
        .await
    {
        Ok(r) => r,
        Err(_) => return,
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
        ensure_ann_for_model(rt, &token, ann, model).await;
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
        let mut warming = ann.warming.lock().expect("warming lock");
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
            ann.warming.lock().expect("warming lock").remove(&key);
        }
    });
}

/// Lazy warm-load for a specific `model`. If the `{namespace, model}` key is
/// already in the cache, return immediately. Otherwise attempt to restore from a
/// valid snapshot; on miss/stale/corrupt, rebuild from the full sqlite-vec corpus
/// and persist the new snapshot. Write failures are logged and do not block search.
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

    // Fast path: already loaded.
    if ann.indexes.read().await.contains_key(&key) {
        return;
    }

    // Try snapshot warm-load.
    if let Some(snapshot) = try_load_snapshot(rt, &ns, model).await {
        let current_fp = compute_fingerprint(rt, token, model).await;
        if let Some(fp) = current_fp {
            if snapshot.fingerprint == fp {
                match AnnBridge::from_vamana_snapshot(snapshot) {
                    Ok(bridge) => {
                        // Re-check under write lock to avoid TOCTOU.
                        ann.indexes.write().await.entry(key).or_insert(bridge);
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "corrupt Vamana snapshot; rebuilding");
                    }
                }
            } else {
                tracing::info!(
                    namespace = %ns,
                    model = %model,
                    "stale Vamana snapshot rejected (fingerprint mismatch); rebuilding"
                );
            }
        }
    }

    // Snapshot absent, stale, or corrupt — rebuild from vector store.
    match load_and_build_from_vector_store(rt, token, model).await {
        Ok(Some(bridge)) => {
            let fp = compute_fingerprint(rt, token, model).await;
            if let Some(fingerprint) = fp {
                if let Err(e) = persist_snapshot(rt, &ns, model, &bridge, fingerprint).await {
                    tracing::error!(error = %e, "failed to persist Vamana snapshot after rebuild");
                }
            }
            ann.indexes.write().await.entry(key).or_insert(bridge);
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
    ann.warming.lock().expect("warming lock").insert(key);
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
}
