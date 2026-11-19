#[tokio::test]
async fn operation_attribution_round_trips_and_changes_idempotent_identity() {
    use khive_storage::operation_context::scope_operation_attribution;
    use khive_types::{OperationAttribution, RefResolution};

    let store = setup_memory_store();
    let direct = make_event("default");
    assert_eq!((direct.op_index, direct.ref_resolution), (None, None));
    store.append_event(direct.clone()).await.unwrap();

    let attributed = scope_operation_attribution(
        OperationAttribution {
            op_index: u32::MAX,
            ref_resolution: RefResolution::Resolved,
        },
        async { make_event("default") },
    )
    .await;
    store.append_event(attributed.clone()).await.unwrap();
    for expected in [&direct, &attributed] {
        let stored = store.get_event(expected.id).await.unwrap().unwrap();
        assert_eq!(&stored, expected);
    }
    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert!(page.items.contains(&direct));
    assert!(page.items.contains(&attributed));

    let mut changed = attributed;
    changed.op_index = Some(0);
    let result = store.append_events_idempotent(vec![changed]).await.unwrap();
    assert_eq!(result.rows[0], EventAppendDisposition::IdentityConflict);
}

const PRE_ATTRIBUTION_EVENTS_TABLE: &str = "CREATE TABLE events (
    id TEXT PRIMARY KEY, namespace TEXT NOT NULL, verb TEXT NOT NULL, substrate TEXT NOT NULL,
    actor TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'audit', outcome TEXT NOT NULL,
    payload TEXT NOT NULL DEFAULT '{}', payload_schema_version INTEGER NOT NULL DEFAULT 1,
    profile_state_version INTEGER, duration_us INTEGER NOT NULL DEFAULT 0, target_id TEXT,
    session_id TEXT, aggregate_kind TEXT, aggregate_id TEXT, created_at INTEGER NOT NULL)";

#[tokio::test]
async fn store_schema_adds_operation_attribution_to_a_pre_attribution_events_table() {
    use khive_storage::operation_context::scope_operation_attribution;
    use khive_types::{OperationAttribution, RefResolution};

    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: None,
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    let legacy = make_event("default");
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute_batch(PRE_ATTRIBUTION_EVENTS_TABLE)
            .unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO events \
                 (id, namespace, verb, substrate, actor, kind, outcome, payload, created_at) \
                 VALUES (?1, ?2, ?3, 'note', ?4, 'search_executed', 'success', ?5, ?6)",
                rusqlite::params![
                    legacy.id.to_string(),
                    legacy.namespace,
                    legacy.verb,
                    legacy.actor,
                    legacy.payload.to_string(),
                    legacy.created_at,
                ],
            )
            .unwrap();
        ensure_events_schema(writer.conn()).unwrap();
        ensure_events_schema(writer.conn()).unwrap();
    }

    let store = SqlEventStore::new_scoped(pool, false, "default");
    let attributed = scope_operation_attribution(
        OperationAttribution {
            op_index: 3,
            ref_resolution: RefResolution::Literal,
        },
        async { make_event("default") },
    )
    .await;
    store.append_event(attributed.clone()).await.unwrap();
    assert_eq!(
        store.get_event(attributed.id).await.unwrap().unwrap(),
        attributed
    );
    assert_eq!(store.get_event(legacy.id).await.unwrap().unwrap(), legacy);
}

#[test]
fn store_schema_refuses_an_events_table_with_one_attribution_column() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(PRE_ATTRIBUTION_EVENTS_TABLE).unwrap();
    conn.execute_batch("ALTER TABLE events ADD COLUMN op_index INTEGER")
        .unwrap();
    let error = ensure_events_schema(&conn).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("only one operation attribution column"),
        "{error}"
    );
}

#[tokio::test]
async fn operation_attribution_rejects_unpaired_values_before_append() {
    let store = setup_memory_store();
    for (op_index, ref_resolution) in [
        (Some(0), None),
        (None, Some(khive_types::RefResolution::Literal)),
    ] {
        let mut event = make_event("default");
        event.op_index = op_index;
        event.ref_resolution = ref_resolution;
        assert!(store.preflight_event(&event).is_err());
        assert!(store.append_event(event).await.is_err());
    }
    assert_eq!(store.count_events(EventFilter::default()).await.unwrap(), 0);
}
