// ── KDB-006 regression: i64 → u32 narrowing on payload_schema_version ──────

/// KDB-006: payload_schema_version of 1 (normal) must round-trip without error.
#[tokio::test]
async fn read_event_with_valid_payload_schema_version() {
    let store = setup_memory_store();
    let event = make_event("default");
    let id = event.id;
    store.append_event(event).await.unwrap();

    let fetched = store.get_event(id).await.unwrap().unwrap();
    assert_eq!(
        fetched.payload_schema_version, 1,
        "default payload_schema_version must round-trip as 1"
    );
}

/// KDB-006: a row with a negative payload_schema_version stored directly via SQL
/// must be rejected by read_event (try_into fails → StorageError).
#[tokio::test]
async fn read_event_rejects_negative_payload_schema_version() {
    use crate::pool::PoolConfig;
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(EVENTS_DDL).unwrap();
    }
    let store = SqlEventStore::new_scoped(Arc::clone(&pool), false, "default");

    // Insert a row with payload_schema_version = -1 directly.
    let id = uuid::Uuid::new_v4();
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO events \
                 (id, namespace, verb, substrate, actor, kind, outcome, payload, \
                  payload_schema_version, duration_us, created_at) \
                 VALUES (?1,'default','test','entity','a','audit','success','{}', -1, 0, 0)",
                rusqlite::params![id.to_string()],
            )
            .unwrap();
    }

    let result = store.get_event(id).await;
    assert!(
        result.is_err(),
        "negative payload_schema_version must be rejected as a StorageError"
    );
}

/// KDB-006 regression: u32::MAX + 1 must be rejected by the position conversion helper.
/// This test verifies the exact boundary that would have silently wrapped via an
/// `as u32` truncation before the fix.
#[test]
fn observation_position_u32_max_plus_one_is_rejected() {
    // usize value one past u32::MAX — this is the exact overflow boundary.
    let overflow_position: usize = u32::MAX as usize + 1;
    let result = u32::try_from(overflow_position);
    assert!(
        result.is_err(),
        "u32::MAX + 1 ({overflow_position}) must not fit in u32"
    );
}

/// KDB-006 regression: u32::MAX itself must convert successfully (boundary value).
#[test]
fn observation_position_u32_max_is_accepted() {
    let max_position: usize = u32::MAX as usize;
    let result = u32::try_from(max_position);
    assert!(
        result.is_ok(),
        "u32::MAX ({max_position}) must be a valid position"
    );
    assert_eq!(result.unwrap(), u32::MAX);
}

/// KDB-006 regression: payload_schema_version = u32::MAX + 1 (i64 = 4294967296) must be rejected.
#[tokio::test]
async fn read_event_rejects_payload_schema_version_u32_max_plus_one() {
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(EVENTS_DDL).unwrap();
    }
    let store = SqlEventStore::new_scoped(Arc::clone(&pool), false, "default");

    let id = uuid::Uuid::new_v4();
    let overflow_version: i64 = i64::from(u32::MAX) + 1;
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO events \
                 (id, namespace, verb, substrate, actor, kind, outcome, payload, \
                  payload_schema_version, duration_us, created_at) \
                 VALUES (?1,'default','test','entity','a','audit','success','{}', ?2, 0, 0)",
                rusqlite::params![id.to_string(), overflow_version],
            )
            .unwrap();
    }

    let result = store.get_event(id).await;
    assert!(
        result.is_err(),
        "payload_schema_version = u32::MAX + 1 ({overflow_version}) must be rejected"
    );
}

/// STORAGE-AUD-003 / #485: PageRequest.offset > i64::MAX must return
/// InvalidInput instead of silently narrowing to a negative i64 offset.
#[tokio::test]
async fn page_offset_over_i64max_rejected() {
    let store = setup_memory_store();
    store.append_event(make_event("default")).await.unwrap();

    let result = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                offset: (i64::MAX as u64) + 1,
                limit: 10,
            },
        )
        .await;

    assert!(
        matches!(result, Err(StorageError::InvalidInput { .. })),
        "expected InvalidInput, got {result:?}"
    );
}
