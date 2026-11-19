use std::sync::Arc;

use khive_storage::types::{OrphanSweepConfig, OrphanSweepResult};
use khive_storage::VectorStore;
use khive_types::SubstrateKind;
use uuid::Uuid;

use super::*;

// ── helpers ──────────────────────────────────────────────────────────────

fn make_pool() -> Arc<crate::pool::ConnectionPool> {
    use crate::pool::{ConnectionPool, PoolConfig};
    crate::extension::ensure_extensions_loaded();
    Arc::new(
        ConnectionPool::new(PoolConfig {
            path: None,
            ..PoolConfig::default()
        })
        .expect("in-memory pool"),
    )
}

/// Create the three core live-subject tables used by the anti-join.
fn create_substrate_tables(pool: &Arc<crate::pool::ConnectionPool>) {
    pool.try_writer()
        .expect("writer")
        .conn()
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS entities \
                     (id TEXT PRIMARY KEY, deleted_at INTEGER); \
                 CREATE TABLE IF NOT EXISTS notes \
                     (id TEXT PRIMARY KEY, deleted_at INTEGER); \
                 CREATE TABLE IF NOT EXISTS knowledge_atoms \
                     (id TEXT PRIMARY KEY, deleted_at INTEGER);",
        )
        .expect("create substrate tables");
}

fn create_vec_table(pool: &Arc<crate::pool::ConnectionPool>, model_key: &str, dims: usize) {
    let ddl = format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS vec_{} USING vec0(\
             subject_id TEXT PRIMARY KEY, \
             namespace TEXT NOT NULL, \
             kind TEXT NOT NULL, \
             field TEXT NOT NULL, \
             embedding_model TEXT NOT NULL, \
             embedding float[{}] distance_metric=cosine)",
        model_key, dims
    );
    let writer = pool.try_writer().expect("writer");
    writer.conn().execute_batch(&ddl).expect("create vec table");
    writer
        .conn()
        .execute_batch(crate::migrations::VECTOR_PROVENANCE_DDL)
        .expect("create vector_provenance");
    writer
        .conn()
        .execute_batch(crate::migrations::ANN_WRITE_LOG_DDL)
        .expect("create ann_write_log");
}

fn make_store(
    pool: Arc<crate::pool::ConnectionPool>,
    model_key: &str,
    dims: usize,
    ns: &str,
) -> SqliteVecStore {
    SqliteVecStore::new(
        pool,
        false,
        model_key.to_string(),
        model_key.to_string(),
        dims,
        ns.to_string(),
    )
    .expect("SqliteVecStore::new")
}

/// Insert a substrate row into `entities`.  `deleted_at = None` → live; `Some(ts)` → soft-deleted.
fn insert_entity(pool: &Arc<crate::pool::ConnectionPool>, id: Uuid, deleted_at: Option<i64>) {
    let id_str = id.to_string();
    pool.try_writer()
        .expect("writer")
        .conn()
        .execute(
            "INSERT INTO entities (id, deleted_at) VALUES (?1, ?2)",
            rusqlite::params![id_str, deleted_at],
        )
        .expect("insert entity");
}

fn vec4(a: f32, b: f32, c: f32, d: f32) -> Vec<f32> {
    vec![a, b, c, d]
}

fn sweep_all(max_delete: u32, dry_run: bool) -> OrphanSweepConfig {
    OrphanSweepConfig {
        subject_id_allowlist: None,
        namespaces: vec![],
        substrate_kinds: vec![],
        max_delete,
        dry_run,
    }
}

// ── test 1: live subject → vector kept ───────────────────────────────────

#[tokio::test]
async fn orphan_sweep_keeps_live_subject() {
    let pool = make_pool();
    create_substrate_tables(&pool);
    create_vec_table(&pool, "sw_live", 4);
    let store = make_store(Arc::clone(&pool), "sw_live", 4, "ns:sw");
    let ns = "ns:sw";

    let id = Uuid::new_v4();
    insert_entity(&pool, id, None); // live

    store
        .insert(
            id,
            SubstrateKind::Entity,
            ns,
            "body",
            vec![vec4(0.1, 0.2, 0.3, 0.4)],
        )
        .await
        .expect("insert vec");

    let r: OrphanSweepResult = store
        .orphan_sweep(&sweep_all(100, false))
        .await
        .expect("sweep");

    assert_eq!(r.scanned, 1, "one vec row exists");
    assert_eq!(r.would_delete, 0, "live subject is not an orphan");
    assert_eq!(r.deleted, 0);
    assert!(!r.max_delete_hit);

    let present = store.batch_exists(&[id], ns).await.expect("exists");
    assert!(present.contains(&id), "live subject's vec must survive");
}

// ── test 2: soft-deleted subject → vector swept ──────────────────────────

#[tokio::test]
async fn orphan_sweep_sweeps_soft_deleted_subject() {
    let pool = make_pool();
    create_substrate_tables(&pool);
    create_vec_table(&pool, "sw_soft", 4);
    let store = make_store(Arc::clone(&pool), "sw_soft", 4, "ns:soft");
    let ns = "ns:soft";

    let id = Uuid::new_v4();
    insert_entity(&pool, id, Some(1_000_000)); // soft-deleted

    store
        .insert(
            id,
            SubstrateKind::Entity,
            ns,
            "body",
            vec![vec4(0.5, 0.5, 0.5, 0.5)],
        )
        .await
        .expect("insert vec");

    let r = store
        .orphan_sweep(&sweep_all(100, false))
        .await
        .expect("sweep");

    assert_eq!(r.scanned, 1);
    assert_eq!(r.would_delete, 1, "soft-deleted subject counts as orphan");
    assert_eq!(r.deleted, 1);
    assert!(!r.max_delete_hit);

    let present = store.batch_exists(&[id], ns).await.expect("exists");
    assert!(
        !present.contains(&id),
        "soft-deleted subject's vec must be swept"
    );
}

// ── test 3: absent subject → vector swept ────────────────────────────────

#[tokio::test]
async fn orphan_sweep_sweeps_absent_subject() {
    let pool = make_pool();
    create_substrate_tables(&pool);
    create_vec_table(&pool, "sw_absent", 4);
    let store = make_store(Arc::clone(&pool), "sw_absent", 4, "ns:absent");
    let ns = "ns:absent";

    let id = Uuid::new_v4(); // no substrate row at all

    store
        .insert(
            id,
            SubstrateKind::Entity,
            ns,
            "body",
            vec![vec4(0.1, 0.2, 0.3, 0.4)],
        )
        .await
        .expect("insert vec");

    let r = store
        .orphan_sweep(&sweep_all(100, false))
        .await
        .expect("sweep");

    assert_eq!(r.scanned, 1);
    assert_eq!(r.would_delete, 1, "absent subject counts as orphan");
    assert_eq!(r.deleted, 1);

    let present = store.batch_exists(&[id], ns).await.expect("exists");
    assert!(!present.contains(&id), "absent subject's vec must be swept");
}

// ── test 4: dry_run → nothing deleted, would_delete populated ────────────

#[tokio::test]
async fn orphan_sweep_dry_run_does_not_delete() {
    let pool = make_pool();
    create_substrate_tables(&pool);
    create_vec_table(&pool, "sw_dry", 4);
    let store = make_store(Arc::clone(&pool), "sw_dry", 4, "ns:dry");
    let ns = "ns:dry";

    let id = Uuid::new_v4(); // absent subject → orphan
    store
        .insert(
            id,
            SubstrateKind::Entity,
            ns,
            "body",
            vec![vec4(0.1, 0.2, 0.3, 0.4)],
        )
        .await
        .expect("insert vec");

    let r = store
        .orphan_sweep(&sweep_all(100, true))
        .await
        .expect("sweep");

    assert_eq!(r.would_delete, 1, "dry-run must still count the orphan");
    assert_eq!(r.deleted, 0, "dry-run must not delete anything");

    let present = store.batch_exists(&[id], ns).await.expect("exists");
    assert!(present.contains(&id), "dry-run must not remove the vec");
}

// ── test 5: max_delete cap ────────────────────────────────────────────────

#[tokio::test]
async fn orphan_sweep_max_delete_caps_deletion() {
    let pool = make_pool();
    create_substrate_tables(&pool);
    create_vec_table(&pool, "sw_cap", 4);
    let store = make_store(Arc::clone(&pool), "sw_cap", 4, "ns:cap");
    let ns = "ns:cap";

    // Insert 5 orphaned vecs (no substrate rows).
    let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
    for (i, &id) in ids.iter().enumerate() {
        let v = i as f32 / 10.0;
        store
            .insert(
                id,
                SubstrateKind::Entity,
                ns,
                "body",
                vec![vec![v, v + 0.1, v + 0.2, v + 0.3]],
            )
            .await
            .expect("insert vec");
    }

    let r = store
        .orphan_sweep(&OrphanSweepConfig {
            subject_id_allowlist: None,
            namespaces: vec![],
            substrate_kinds: vec![],
            max_delete: 2,
            dry_run: false,
        })
        .await
        .expect("sweep");

    assert_eq!(r.scanned, 5);
    assert_eq!(r.would_delete, 5);
    assert_eq!(r.deleted, 2, "cap must stop at max_delete");
    assert!(
        r.max_delete_hit,
        "max_delete_hit must be true when cap triggered"
    );

    // Verify exactly 3 vecs survive.
    let mut surviving = 0usize;
    for &id in &ids {
        if store
            .batch_exists(&[id], ns)
            .await
            .expect("exists")
            .contains(&id)
        {
            surviving += 1;
        }
    }
    assert_eq!(surviving, 3, "3 orphans must survive after cap");
}

#[tokio::test]
async fn orphan_sweep_batches_over_400_with_matching_delete_logs() {
    use std::collections::HashSet;

    let pool = make_pool();
    create_substrate_tables(&pool);
    create_vec_table(&pool, "sw_batches", 4);
    let store = make_store(Arc::clone(&pool), "sw_batches", 4, "ns:batches");

    let ids: Vec<Uuid> = (0..405).map(|_| Uuid::new_v4()).collect();
    for &id in &ids {
        store
            .insert(
                id,
                SubstrateKind::Entity,
                "ns:batches",
                "body",
                vec![vec4(0.1, 0.2, 0.3, 0.4)],
            )
            .await
            .expect("insert orphan vector");
    }

    let result = store
        .orphan_sweep(&sweep_all(403, false))
        .await
        .expect("sweep full and partial batches");
    assert_eq!(result.scanned, 405);
    assert_eq!(result.would_delete, 405);
    assert_eq!(result.deleted, 403);
    assert!(result.max_delete_hit);

    let writer = pool.try_writer().expect("writer");
    let conn = writer.conn();
    let logged: HashSet<String> = conn
        .prepare(
            "SELECT subject_id FROM ann_write_log \
                 WHERE op = 'delete' AND namespace = 'ns:batches' \
                   AND embedding_model = 'sw_batches' AND kind = 'entity' AND field = 'body'",
        )
        .expect("prepare log query")
        .query_map([], |row| row.get(0))
        .expect("query delete logs")
        .collect::<Result<_, _>>()
        .expect("read delete logs");
    let remaining: HashSet<String> = conn
        .prepare("SELECT subject_id FROM vec_sw_batches")
        .expect("prepare remaining query")
        .query_map([], |row| row.get(0))
        .expect("query remaining vectors")
        .collect::<Result<_, _>>()
        .expect("read remaining vectors");
    let log_rows: i64 = conn
        .query_row(
            "SELECT count(*) FROM ann_write_log \
                 WHERE op = 'delete' AND namespace = 'ns:batches' \
                   AND embedding_model = 'sw_batches' AND kind = 'entity' AND field = 'body'",
            [],
            |row| row.get(0),
        )
        .expect("count delete logs");
    assert_eq!(log_rows, result.deleted as i64, "one log row per deletion");
    assert_eq!(logged.len(), 403, "every deleted vector must have one log");
    assert_eq!(remaining.len(), 2);
    assert!(logged.is_disjoint(&remaining));
    assert_eq!(logged.union(&remaining).count(), ids.len());
}

// ── test 6: namespace filter ──────────────────────────────────────────────

#[tokio::test]
async fn orphan_sweep_namespace_filter_scopes_sweep() {
    let pool = make_pool();
    create_substrate_tables(&pool);
    create_vec_table(&pool, "sw_ns", 4);
    let store = make_store(Arc::clone(&pool), "sw_ns", 4, "ns:a");

    let id_a = Uuid::new_v4();
    let id_b = Uuid::new_v4();

    store
        .insert(
            id_a,
            SubstrateKind::Entity,
            "ns:a",
            "body",
            vec![vec4(0.1, 0.2, 0.3, 0.4)],
        )
        .await
        .expect("insert ns:a");
    store
        .insert(
            id_b,
            SubstrateKind::Entity,
            "ns:b",
            "body",
            vec![vec4(0.5, 0.6, 0.7, 0.8)],
        )
        .await
        .expect("insert ns:b");

    // Both are orphans (no substrate rows); sweep scoped to ns:a only.
    let r = store
        .orphan_sweep(&OrphanSweepConfig {
            subject_id_allowlist: None,
            namespaces: vec!["ns:a".to_string()],
            substrate_kinds: vec![],
            max_delete: 100,
            dry_run: false,
        })
        .await
        .expect("sweep");

    assert_eq!(r.scanned, 1, "only ns:a row visible to scoped sweep");
    assert_eq!(r.deleted, 1);

    let exists_a = store.batch_exists(&[id_a], "ns:a").await.expect("exists a");
    let exists_b = store.batch_exists(&[id_b], "ns:b").await.expect("exists b");
    assert!(!exists_a.contains(&id_a), "ns:a orphan must be swept");
    assert!(exists_b.contains(&id_b), "ns:b vec must be untouched");
}

// ── test 7: substrate_kinds filter ───────────────────────────────────────

#[tokio::test]
async fn orphan_sweep_substrate_kinds_filter_scopes_sweep() {
    let pool = make_pool();
    create_substrate_tables(&pool);
    create_vec_table(&pool, "sw_kind", 4);
    let store = make_store(Arc::clone(&pool), "sw_kind", 4, "ns:kind");
    let ns = "ns:kind";

    let id_ent = Uuid::new_v4();
    let id_note = Uuid::new_v4();

    // Both orphaned; one entity-kind vec, one note-kind vec.
    store
        .insert(
            id_ent,
            SubstrateKind::Entity,
            ns,
            "body",
            vec![vec4(0.1, 0.2, 0.3, 0.4)],
        )
        .await
        .expect("insert entity vec");
    store
        .insert(
            id_note,
            SubstrateKind::Note,
            ns,
            "body",
            vec![vec4(0.5, 0.6, 0.7, 0.8)],
        )
        .await
        .expect("insert note vec");

    // Sweep only entity-kind vecs.
    let r = store
        .orphan_sweep(&OrphanSweepConfig {
            subject_id_allowlist: None,
            namespaces: vec![],
            substrate_kinds: vec![SubstrateKind::Entity],
            max_delete: 100,
            dry_run: false,
        })
        .await
        .expect("sweep");

    assert_eq!(r.scanned, 1, "kind filter restricts scanned count");
    assert_eq!(r.deleted, 1, "only entity-kind orphan is swept");

    let ent_exists = store.batch_exists(&[id_ent], ns).await.expect("ent exists");
    let note_exists = store
        .batch_exists(&[id_note], ns)
        .await
        .expect("note exists");
    assert!(
        !ent_exists.contains(&id_ent),
        "entity-kind orphan must be swept"
    );
    assert!(
        note_exists.contains(&id_note),
        "note-kind vec must be untouched"
    );
}

// ── test 8: subject_id_allowlist filter ──────────────────────────────────

#[tokio::test]
async fn orphan_sweep_allowlist_restricts_eligible_rows() {
    let pool = make_pool();
    create_substrate_tables(&pool);
    create_vec_table(&pool, "sw_allow", 4);
    let store = make_store(Arc::clone(&pool), "sw_allow", 4, "ns:allow");
    let ns = "ns:allow";

    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();
    let id3 = Uuid::new_v4(); // not in allowlist

    for (i, &id) in [id1, id2, id3].iter().enumerate() {
        let v = i as f32 * 0.1 + 0.1;
        store
            .insert(
                id,
                SubstrateKind::Entity,
                ns,
                "body",
                vec![vec![v, v, v, v]],
            )
            .await
            .expect("insert vec");
    }

    // All are orphans; allowlist only allows id1 and id2 to be swept.
    let r = store
        .orphan_sweep(&OrphanSweepConfig {
            subject_id_allowlist: Some(vec![id1, id2]),
            namespaces: vec![],
            substrate_kinds: vec![],
            max_delete: 100,
            dry_run: false,
        })
        .await
        .expect("sweep");

    assert_eq!(r.scanned, 2, "allowlist restricts scanned to 2");
    assert_eq!(r.would_delete, 2);
    assert_eq!(r.deleted, 2, "both allowlisted orphans deleted");

    let e1 = store.batch_exists(&[id1], ns).await.expect("e1");
    let e2 = store.batch_exists(&[id2], ns).await.expect("e2");
    let e3 = store.batch_exists(&[id3], ns).await.expect("e3");
    assert!(!e1.contains(&id1), "id1 must be swept");
    assert!(!e2.contains(&id2), "id2 must be swept");
    assert!(e3.contains(&id3), "id3 not in allowlist must survive");
}

// ── helpers for note substrate rows ─────────────────────────────────────

fn insert_note(pool: &Arc<crate::pool::ConnectionPool>, id: Uuid, deleted_at: Option<i64>) {
    let id_str = id.to_string();
    pool.try_writer()
        .expect("writer")
        .conn()
        .execute(
            "INSERT INTO notes (id, deleted_at) VALUES (?1, ?2)",
            rusqlite::params![id_str, deleted_at],
        )
        .expect("insert note");
}

// ── test 9: live note → vector kept ──────────────────────────────────────

#[tokio::test]
async fn orphan_sweep_keeps_live_note() {
    let pool = make_pool();
    create_substrate_tables(&pool);
    create_vec_table(&pool, "sw_note_live", 4);
    let store = make_store(Arc::clone(&pool), "sw_note_live", 4, "ns:nlive");
    let ns = "ns:nlive";

    let id = Uuid::new_v4();
    insert_note(&pool, id, None); // live note row

    store
        .insert(
            id,
            SubstrateKind::Note,
            ns,
            "body",
            vec![vec4(0.1, 0.2, 0.3, 0.4)],
        )
        .await
        .expect("insert vec");

    let r = store
        .orphan_sweep(&sweep_all(100, false))
        .await
        .expect("sweep");

    assert_eq!(r.scanned, 1);
    assert_eq!(r.would_delete, 0, "live note is not an orphan");
    assert_eq!(r.deleted, 0);

    let present = store.batch_exists(&[id], ns).await.expect("exists");
    assert!(present.contains(&id), "live note's vec must survive");
}

// ── test 10: soft-deleted note → vector swept ─────────────────────────────

#[tokio::test]
async fn orphan_sweep_sweeps_soft_deleted_note() {
    let pool = make_pool();
    create_substrate_tables(&pool);
    create_vec_table(&pool, "sw_note_soft", 4);
    let store = make_store(Arc::clone(&pool), "sw_note_soft", 4, "ns:nsoft");
    let ns = "ns:nsoft";

    let id = Uuid::new_v4();
    insert_note(&pool, id, Some(1_000_000)); // soft-deleted note row

    store
        .insert(
            id,
            SubstrateKind::Note,
            ns,
            "body",
            vec![vec4(0.5, 0.5, 0.5, 0.5)],
        )
        .await
        .expect("insert vec");

    let r = store
        .orphan_sweep(&sweep_all(100, false))
        .await
        .expect("sweep");

    assert_eq!(r.scanned, 1);
    assert_eq!(r.would_delete, 1, "soft-deleted note counts as orphan");
    assert_eq!(r.deleted, 1);

    let present = store.batch_exists(&[id], ns).await.expect("exists");
    assert!(
        !present.contains(&id),
        "soft-deleted note's vec must be swept"
    );
}

// ── test 11: mid-transaction error must NOT poison the pooled connection ──
//
// Regression for the transaction-leak bug: if orphan_sweep errors after
// BEGIN IMMEDIATE but before COMMIT, the pooled writer must NOT be left
// with an open transaction.  Without the RAII guard, the next writer
// call fails with "cannot start a transaction within a transaction".
//
// Deterministic injection: we create the vec table but deliberately omit
// the substrate tables.  The anti-join queries reference `entities` and
// `notes`, so the first scan COUNT fails with "no such table: entities".
// After the error, we immediately perform a normal vector insert on the
// same store and assert it succeeds — proving the connection is clean.

#[tokio::test]
async fn orphan_sweep_error_does_not_poison_connection() {
    let pool = make_pool();
    // Note: create_substrate_tables is intentionally NOT called here.
    create_vec_table(&pool, "sw_poison", 4);
    let store = make_store(Arc::clone(&pool), "sw_poison", 4, "ns:poison");
    let ns = "ns:poison";

    // orphan_sweep must fail because `entities` / `notes` do not exist.
    let sweep_result = store.orphan_sweep(&sweep_all(100, false)).await;
    assert!(
        sweep_result.is_err(),
        "sweep must fail when substrate tables are absent"
    );

    // The connection must not be poisoned: a normal vector insert must succeed.
    let id = Uuid::new_v4();
    store
        .insert(
            id,
            SubstrateKind::Entity,
            ns,
            "body",
            vec![vec4(0.1, 0.2, 0.3, 0.4)],
        )
        .await
        .expect("insert after failed sweep must succeed (connection not poisoned)");

    let present = store.batch_exists(&[id], ns).await.expect("exists");
    assert!(
        present.contains(&id),
        "vector inserted after failed sweep must be present"
    );
}
