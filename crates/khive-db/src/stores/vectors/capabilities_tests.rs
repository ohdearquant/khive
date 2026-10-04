use super::*;

fn make_pool() -> Arc<crate::pool::ConnectionPool> {
    use crate::pool::{ConnectionPool, PoolConfig};
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    Arc::new(ConnectionPool::new(config).expect("in-memory pool"))
}

#[test]
fn sqlite_vec_store_capabilities_are_correct() {
    let store = SqliteVecStore::new(
        make_pool(),
        /*is_file_backed=*/ false,
        "test_model".into(),
        "test_model".into(),
        /*dimensions=*/ 4,
        "ns:test".into(),
    )
    .expect("SqliteVecStore::new");

    let caps = store.capabilities();

    assert!(
        !caps.supports_filter,
        "sqlite-vec does not support filter pushdown"
    );
    assert!(
        !caps.supports_batch_search,
        "sqlite-vec does not support native batch search"
    );
    assert!(
        !caps.supports_quantization,
        "sqlite-vec does not support quantization"
    );
    assert!(
        !caps.supports_update,
        "sqlite-vec does not support in-place update"
    );
    assert!(
        caps.supports_orphan_sweep,
        "SqliteVecStore must advertise supports_orphan_sweep = true"
    );
    // sqlite-vec 0.1.9: SQLITE_VEC_VEC0_MAX_DIMENSIONS = 8192.
    assert_eq!(caps.max_dimensions, Some(8192));
    assert_eq!(
        caps.index_kinds,
        vec![VectorIndexKind::SqliteVec],
        "index_kinds should be [SqliteVec]"
    );
}

/// Regression: max_dimensions must equal the sqlite-vec hard limit (8192),
/// not the K_MAX constant (4096). A caller with 5000-dim embeddings must not
/// be falsely told the backend is incapable.
#[test]
fn max_dimensions_reflects_sqlite_vec_hard_limit_not_k_max() {
    let store = SqliteVecStore::new(
        make_pool(),
        false,
        "test_dim_limit".into(),
        "test_dim_limit".into(),
        /*dimensions=*/ 4,
        "ns:test".into(),
    )
    .expect("SqliteVecStore::new");

    let caps = store.capabilities();

    // SQLITE_VEC_VEC0_MAX_DIMENSIONS = 8192 (sqlite-vec.c:3488).
    // The previous incorrect value 4096 was SQLITE_VEC_VEC0_K_MAX (max neighbours),
    // which would falsely reject 4097–8192 dimensional models.
    let max = caps
        .max_dimensions
        .expect("SqliteVecStore must declare a finite dimension limit");
    assert!(
        max >= 8192,
        "max_dimensions ({max}) must be at least 8192 — the sqlite-vec hard limit"
    );
}

/// Capabilities struct is returned by &'static reference; calling twice must
/// return the same value (OnceLock semantics, no allocation on repeat calls).
#[test]
fn capabilities_is_idempotent() {
    let store = SqliteVecStore::new(
        make_pool(),
        false,
        "test_idempotent".into(),
        "test_idempotent".into(),
        4,
        "ns:test".into(),
    )
    .expect("SqliteVecStore::new");

    let caps1 = store.capabilities();
    let caps2 = store.capabilities();
    assert_eq!(
        caps1 as *const _, caps2 as *const _,
        "capabilities() must return the same static reference each call"
    );
}
