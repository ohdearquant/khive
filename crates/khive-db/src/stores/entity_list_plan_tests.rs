use super::*;
use crate::backend::StorageBackend;
use rusqlite::{Connection, StatementStatus};
use std::time::Duration;

const INDEXES: [&str; 2] = [
    "idx_entities_live_namespace_order",
    "idx_entities_live_namespace_type_order",
];

fn fixture(rows: usize) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(ENTITIES_DDL).unwrap();
    conn.execute_batch(include_str!("../../sql/021-attachments-a-stage.sql"))
        .unwrap();
    for i in 1..=rows {
        conn.execute(
            "INSERT INTO entities(id,namespace,kind,entity_type,name,properties,tags,created_at,updated_at,deleted_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?8,?9)",
            rusqlite::params![Uuid::from_u128(i as u128).to_string(),
                if i % 100 == 0 { "minor" } else { "dominant" },
                if i % 3 == 0 { "document" } else { "concept" },
                if i % 17 == 0 { None } else { Some(if i % 97 == 0 { "rare" } else if i % 89 == 0 { "other" } else { "common" }) },
                if i == 1 { "Name".to_string() } else { format!("Name{i}") },
                if i % 34 == 0 { "invalid JSON" } else if i % 17 == 0 { "{\"type\":\"rare\"}" } else { "{}" },
                if i % 7 == 0 { "[\"Tag\"]" } else { "[]" },
                (i / 3) as i64, if i % 10 == 0 { Some(i as i64) } else { None }],
        ).unwrap();
    }
    conn.execute("INSERT INTO attachments(record_uuid,substrate,role,content_ref,created_at) VALUES(?1,'entity','content',?2,1)",
        rusqlite::params![Uuid::from_u128(1).to_string(), "a".repeat(64)]).unwrap();
    conn
}

fn values(
    conn: &Connection,
    sql: &str,
    params: &[Box<dyn rusqlite::ToSql>],
) -> Vec<Vec<rusqlite::types::Value>> {
    let mut stmt = conn.prepare(sql).unwrap();
    let columns = stmt.column_count();
    let refs = params.iter().map(|p| p.as_ref()).collect::<Vec<_>>();
    stmt.query_map(refs.as_slice(), |row| {
        (0..columns).map(|column| row.get(column)).collect()
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

fn unforced(sql: &str) -> String {
    let mut sql = sql.replace(
        "entities INDEXED BY sqlite_autoindex_entities_1",
        "entities",
    );
    for index in INDEXES {
        sql = sql.replace(&format!("entities INDEXED BY {index}"), "entities");
    }
    sql
}

fn plan_work(
    conn: &Connection,
    sql: &str,
    params: &[Box<dyn rusqlite::ToSql>],
) -> (Vec<String>, i32) {
    let refs = params.iter().map(|p| p.as_ref()).collect::<Vec<_>>();
    let plan = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(refs.as_slice(), |row| row.get(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut stmt = conn.prepare(sql).unwrap();
    {
        let mut rows = stmt.query(refs.as_slice()).unwrap();
        while rows.next().unwrap().is_some() {}
    }
    (plan, stmt.get_status(StatementStatus::VmStep))
}

#[test]
fn namespace_and_type_plans_bound_work_without_statistics() {
    for analyzed in [false, true] {
        let mut observed = Vec::new();
        for size in [1000, 10000] {
            let conn = fixture(size);
            if analyzed {
                conn.execute_batch("ANALYZE").unwrap();
            }
            let mut work = Vec::new();
            for (filter, index) in [
                (EntityFilter::default(), INDEXES[0]),
                (
                    EntityFilter {
                        entity_types: vec!["rare".into()],
                        ..Default::default()
                    },
                    INDEXES[1],
                ),
            ] {
                let (clause, mut params) = build_entity_where("dominant", &filter);
                params.push(Box::new(5_i64));
                params.push(Box::new(0_i64));
                let sql = build_entity_page_query(
                    ENTITY_SELECT_COLUMNS,
                    &filter,
                    &clause,
                    "created_at DESC, id DESC",
                    params.len() - 1,
                    params.len(),
                );
                let (plan, steps) = plan_work(&conn, &sql, &params);
                eprintln!("entity-list page size={size} analyzed={analyzed} index={index} vm_steps={steps} plan={plan:?}");
                assert!(
                    plan.iter().any(|line| line.contains(index)),
                    "matching entity index required: {plan:?}"
                );
                assert!(
                    !plan.iter().any(|line| line.contains("TEMP B-TREE")),
                    "single namespace/type page must stream its total order: {plan:?}"
                );
                assert_eq!(
                    values(&conn, &sql, &params),
                    values(&conn, &unforced(&sql), &params)
                );
                work.push(steps);
            }
            let filter = EntityFilter {
                entity_types: vec!["rare".into()],
                ..Default::default()
            };
            let (clause, params) = build_entity_where("dominant", &filter);
            let sql = build_entity_count_query(&filter, &clause);
            let (plan, count_steps) = plan_work(&conn, &sql, &params);
            eprintln!("entity-list count size={size} analyzed={analyzed} vm_steps={count_steps} plan={plan:?}");
            assert!(
                plan.iter()
                    .any(|line| line.contains(INDEXES[1]) && line.contains("entity_type=?")),
                "type COUNT must seek namespace and type: {plan:?}"
            );
            assert_eq!(
                values(&conn, &sql, &params),
                values(&conn, &unforced(&sql), &params)
            );
            observed.push(work);
        }
        for column in 0..2 {
            assert!(
                observed[1][column] < observed[0][column] * 2,
                "first-page work must not track namespace growth: {observed:?}"
            );
        }
    }
}

#[test]
fn complete_filter_projection_and_count_parity_with_new_list_indexes() {
    let conn = fixture(1000);
    let filters = vec![
        EntityFilter::default(),
        EntityFilter {
            namespaces: vec!["dominant".into()],
            ..Default::default()
        },
        EntityFilter {
            namespaces: vec!["minor".into(), "dominant".into(), "minor".into()],
            ..Default::default()
        },
        EntityFilter {
            kinds: vec!["concept".into()],
            ..Default::default()
        },
        EntityFilter {
            kinds: vec!["concept".into(), "document".into()],
            ..Default::default()
        },
        EntityFilter {
            entity_types: vec!["rare".into()],
            ..Default::default()
        },
        EntityFilter {
            entity_types: vec!["rare".into(), "other".into()],
            namespaces: vec!["dominant".into(), "minor".into()],
            ..Default::default()
        },
        EntityFilter {
            entity_types: vec!["rare".into()],
            legacy_entity_type_fallback: true,
            ..Default::default()
        },
        EntityFilter {
            entity_types_by_kind: [("concept".into(), vec!["rare".into()])]
                .into_iter()
                .collect(),
            legacy_entity_type_fallback: true,
            ..Default::default()
        },
        EntityFilter {
            name_prefix: Some("Name".into()),
            ..Default::default()
        },
        EntityFilter {
            name_exact: Some("Name1".into()),
            ..Default::default()
        },
        EntityFilter {
            tags_any: vec!["tag".into()],
            ..Default::default()
        },
        EntityFilter {
            ids: vec![Uuid::from_u128(1), Uuid::from_u128(10), Uuid::from_u128(97)],
            entity_types: vec!["rare".into(), "common".into()],
            ..Default::default()
        },
    ];
    for filter in filters {
        let (clause, params) = build_entity_where("dominant", &filter);
        let count_sql = build_entity_count_query(&filter, &clause);
        assert_eq!(
            values(&conn, &count_sql, &params),
            values(&conn, &unforced(&count_sql), &params),
            "count filter {filter:?}"
        );
        for offset in [0_i64, 7, 2000] {
            let (clause, mut params) = build_entity_where("dominant", &filter);
            let order = if let Some(prefix) = &filter.name_prefix {
                params.push(Box::new(prefix.to_ascii_lowercase()));
                format!(
                    "CASE WHEN LOWER(name) = ?{} THEN 0 ELSE 1 END, created_at DESC, id DESC",
                    params.len()
                )
            } else {
                "created_at DESC, id DESC".into()
            };
            params.push(Box::new(7_i64));
            params.push(Box::new(offset));
            let sql = build_entity_page_query(
                ENTITY_SELECT_COLUMNS,
                &filter,
                &clause,
                &order,
                params.len() - 1,
                params.len(),
            );
            assert_eq!(
                values(&conn, &sql, &params),
                values(&conn, &unforced(&sql), &params),
                "full projection filter {filter:?}, offset {offset}"
            );
            if filter.name_prefix.is_some() && offset == 0 {
                assert_eq!(
                    values(&conn, &sql, &params)[0][0],
                    rusqlite::types::Value::Text(Uuid::from_u128(1).to_string()),
                    "exact folded prefix match must outrank newer prefix candidates"
                );
            }
        }
    }
    let filter = EntityFilter::default();
    let (clause, mut params) = build_entity_where("dominant", &filter);
    assert_eq!(
        values(&conn, &build_entity_count_query(&filter, &clause), &params),
        vec![vec![rusqlite::types::Value::Integer(900)]],
        "deleted dominant rows must be excluded"
    );
    params.push(Box::new(7_i64));
    params.push(Box::new(0_i64));
    let sql = build_entity_page_query(
        "entities.id, created_at",
        &filter,
        &clause,
        "created_at DESC, id DESC",
        2,
        3,
    );
    let rows = values(&conn, &sql, &params);
    assert_eq!(
        rows[0][0],
        rusqlite::types::Value::Text(Uuid::from_u128(999).to_string())
    );
    assert_eq!(
        rows[1][0],
        rusqlite::types::Value::Text(Uuid::from_u128(998).to_string())
    );
    assert_eq!(
        rows[2][0],
        rusqlite::types::Value::Text(Uuid::from_u128(997).to_string())
    );
}

#[test]
fn candidates_cursor_and_id_plans_keep_their_existing_sources() {
    let conn = fixture(1000);
    for filter in [
        EntityFilter::default(),
        EntityFilter {
            ids: vec![Uuid::from_u128(1), Uuid::from_u128(97)],
            ..Default::default()
        },
    ] {
        let (clause, mut params) = build_entity_where("dominant", &filter);
        params.push(Box::new("name".to_string()));
        params.push(Box::new(5_i64));
        params.push(Box::new(0_i64));
        let sql = build_candidate_entity_query(
            ENTITY_SELECT_COLUMNS,
            &filter,
            &clause,
            &[params.len() - 2],
            "created_at DESC, id DESC",
            params.len() - 1,
            params.len(),
        );
        assert!(
            !INDEXES.iter().any(|index| sql.contains(index)),
            "candidate names retain their existing access path"
        );
        assert_eq!(
            values(&conn, &sql, &params),
            values(&conn, &unforced(&sql), &params)
        );
        assert_eq!(values(&conn, &sql, &params).len(), 1);
        let (clause, mut params) = build_entity_where("dominant", &filter);
        params.push(Box::new(7_i64));
        let sql = build_entity_cursor_query(ENTITY_SELECT_COLUMNS, &filter, &clause, params.len());
        assert!(!INDEXES.iter().any(|index| sql.contains(index)));
        let rows = values(&conn, &sql, &params);
        assert_eq!(
            rows[0][0],
            rusqlite::types::Value::Text(Uuid::from_u128(1).to_string())
        );
        assert!(rows
            .windows(2)
            .all(|pair| matches!((&pair[0][15], &pair[1][15]), (rusqlite::types::Value::Integer(a), rusqlite::types::Value::Integer(b)) if a < b)));
        if !filter.ids.is_empty() {
            for sql in [
                sql,
                build_entity_count_query(&filter, &clause),
                build_entity_page_query(
                    ENTITY_SELECT_COLUMNS,
                    &filter,
                    &clause,
                    "created_at DESC, id DESC",
                    params.len(),
                    params.len() + 1,
                ),
            ] {
                assert!(
                    sql.contains("INDEXED BY sqlite_autoindex_entities_1"),
                    "explicit ID lookup must remain primary-key pinned"
                );
            }
        }
    }
}

fn catalog(conn: &Connection) -> Vec<(String, String)> {
    conn.prepare("SELECT name, sql FROM sqlite_schema WHERE type='index' AND name LIKE 'idx_entities_%' ORDER BY name").unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?))).unwrap().collect::<Result<_, _>>().unwrap()
}

#[derive(Clone, Default)]
struct ListDiagnosticCapture {
    events: Arc<std::sync::Mutex<Vec<std::collections::BTreeMap<String, String>>>>,
}

impl tracing::Subscriber for ListDiagnosticCapture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        #[derive(Default)]
        struct Fields(std::collections::BTreeMap<String, String>);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.insert(field.name().into(), format!("{value:?}"));
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.insert(field.name().into(), value.into());
            }
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        if *event.metadata().level() == tracing::Level::WARN && fields.0.contains_key("index") {
            self.events.lock().unwrap().push(fields.0);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

impl ListDiagnosticCapture {
    fn assert_one(&self, index: &str, operation: &str) {
        let mut events = self.events.lock().unwrap();
        assert_eq!(
            events.len(),
            1,
            "exactly one missing-index diagnostic: {events:?}"
        );
        assert_eq!(events[0]["index"], index);
        assert_eq!(events[0]["operation"], operation);
        assert!(events[0]["message"].contains("retrying without forced index"));
        events.clear();
    }
}

fn seed_fallback_rows(conn: &Connection) {
    conn.execute_batch(include_str!("../../sql/021-attachments-a-stage.sql"))
        .unwrap();
    for (i, namespace, deleted) in [
        (1, "local", false),
        (2, "local", false),
        (3, "local", false),
        (4, "foreign", false),
        (5, "local", true),
    ] {
        conn.execute("INSERT INTO entities(id,namespace,kind,entity_type,name,created_at,updated_at,deleted_at) VALUES(?1,?2,'concept',?3,?4,10,10,?5)",
            rusqlite::params![Uuid::from_u128(i).to_string(), namespace,
                if i == 3 { "common" } else { "rare" }, format!("row{i}"),
                if deleted { Some(20) } else { None }]).unwrap();
    }
}

async fn assert_missing_list_fallback(
    store: &dyn EntityStore,
    pool: &ConnectionPool,
    external: &Connection,
    capture: &ListDiagnosticCapture,
) {
    for (index, filter) in [
        (INDEXES[0], EntityFilter::default()),
        (
            INDEXES[1],
            EntityFilter {
                entity_types: vec!["rare".into()],
                ..Default::default()
            },
        ),
    ] {
        let page = PageRequest {
            limit: 2,
            offset: 0,
        };
        let expected = store
            .query_entities("local", filter.clone(), page.clone())
            .await
            .unwrap();
        let expected_rows = serde_json::to_value(&expected.items).unwrap();
        let expected_count = store.count_entities("local", filter.clone()).await.unwrap();
        assert_eq!(expected_count, if index == INDEXES[0] { 3 } else { 2 });
        assert_eq!(expected.total, Some(expected_count));
        assert!(capture.events.lock().unwrap().is_empty());
        external
            .execute_batch(&format!("DROP INDEX {index}"))
            .unwrap();
        let before = pool.writer_acquisition_snapshot();
        let readers = pool.reader_acquisition_snapshot();
        let actual = store.query_entities("local", filter.clone(), page).await;
        assert!(
            actual.is_ok(),
            "eligible dropped-index read must succeed through unforced fallback: {actual:?}"
        );
        let actual = actual.unwrap();
        assert_eq!(serde_json::to_value(&actual.items).unwrap(), expected_rows);
        assert_eq!(actual.total, expected.total);
        capture.assert_one(index, "query_entities");
        assert!(
            !catalog(external).iter().any(|entry| entry.0 == index),
            "read must leave dropped index absent"
        );
        assert_eq!(
            store.count_entities("local", filter).await.unwrap(),
            expected_count
        );
        capture.assert_one(index, "count_entities");
        assert!(
            !catalog(external).iter().any(|entry| entry.0 == index),
            "COUNT must leave dropped index absent"
        );
        assert_eq!(
            pool.writer_acquisition_snapshot().pooled_acquisitions,
            before.pooled_acquisitions,
            "fallback must never acquire a writer"
        );
        assert_eq!(
            pool.reader_acquisition_snapshot().pooled_checkouts,
            readers.pooled_checkouts + 2,
            "each read uses one existing reader checkout"
        );
    }
}

#[tokio::test]
async fn backend_entity_reads_fallback_without_repair_and_reopen_initializes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("entity-plan.db");
    let backend = StorageBackend::sqlite_for_test(&path).unwrap();
    let store = backend.entities().unwrap();
    let external = Connection::open(&path).unwrap();
    seed_fallback_rows(&external);
    let capture = ListDiagnosticCapture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    assert_missing_list_fallback(store.as_ref(), backend.pool(), &external, &capture).await;
    drop(store);
    drop(backend);
    let reopened = StorageBackend::sqlite_for_test(&path).unwrap();
    let store = reopened.entities().unwrap();
    assert_eq!(
        store
            .count_entities("local", EntityFilter::default())
            .await
            .unwrap(),
        3
    );
    assert!(INDEXES
        .iter()
        .all(|index| catalog(&external).iter().any(|entry| entry.0 == *index)));
    assert!(
        capture.events.lock().unwrap().is_empty(),
        "open-time DDL restores hints without read fallback"
    );
}

#[tokio::test]
async fn readonly_entity_reads_fallback_without_repair_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("readonly-entity-plan.db");
    let writable =
        StorageBackend::sqlite_for_test_with_journal_mode(&path, false, Duration::from_secs(2))
            .unwrap();
    writable.prepare_core_schema().unwrap();
    writable.entities().unwrap();
    let external = Connection::open(&path).unwrap();
    seed_fallback_rows(&external);
    drop(writable);
    let readonly = StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    readonly.prepare_core_schema().unwrap();
    let store = readonly.entities().unwrap();
    let capture = ListDiagnosticCapture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    assert_missing_list_fallback(store.as_ref(), readonly.pool(), &external, &capture).await;
    drop(store);
    drop(readonly);
    let reopened = StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    reopened.prepare_core_schema().unwrap();
    assert_eq!(
        reopened
            .entities()
            .unwrap()
            .count_entities("local", EntityFilter::default())
            .await
            .unwrap(),
        3
    );
    capture.assert_one(INDEXES[0], "count_entities");
    assert!(INDEXES
        .iter()
        .all(|index| !catalog(&external).iter().any(|entry| entry.0 == *index)));
}

#[tokio::test]
async fn standalone_entity_reads_fallback_without_repair() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("standalone.db");
    let pool = Arc::new(
        ConnectionPool::new(crate::pool::PoolConfig {
            path: Some(path.clone()),
            write_queue_enabled: Some(false),
            ..crate::pool::PoolConfig::for_test()
        })
        .unwrap(),
    );
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(ENTITIES_DDL)
        .unwrap();
    let external = Connection::open(&path).unwrap();
    seed_fallback_rows(&external);
    let store = SqlEntityStore::new(Arc::clone(&pool), true);
    let capture = ListDiagnosticCapture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    assert_missing_list_fallback(&store, &pool, &external, &capture).await;
}

#[tokio::test]
async fn list_fallback_retries_once_and_preserves_unrecognized_sqlite_errors() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let pool = Arc::new(ConnectionPool::new(crate::pool::PoolConfig::for_test()).unwrap());
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(ENTITIES_DDL)
        .unwrap();
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(&format!("DROP INDEX {}", INDEXES[0]))
        .unwrap();
    let store = SqlEntityStore::new(pool, false);
    let capture = ListDiagnosticCapture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    for (sql, expected_attempts, expected_cause) in [
        (
            format!("SELECT COUNT(*) FROM entities INDEXED BY {}", INDEXES[0]),
            2,
            format!("no such index: {}", INDEXES[0]),
        ),
        (
            "SELECT COUNT(*) FROM entities INDEXED BY missing_unrelated_index".into(),
            1,
            "no such index: missing_unrelated_index".into(),
        ),
        (
            "SELECT missing_column FROM entities".into(),
            1,
            "no such column: missing_column".into(),
        ),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let error = store
            .with_list_reader("fallback_cause", Some(INDEXES[0]), move |conn, _| {
                calls.fetch_add(1, Ordering::Relaxed);
                conn.query_row(&sql, [], |row| row.get::<_, i64>(0))
            })
            .await
            .unwrap_err();
        let StorageError::Driver {
            capability,
            operation,
            source,
        } = error
        else {
            panic!("{error}")
        };
        assert_eq!(capability, StorageCapability::Entities);
        assert_eq!(operation.as_ref(), "fallback_cause");
        let sqlite = source
            .downcast_ref::<rusqlite::Error>()
            .expect("original SQLite cause retained");
        assert!(
            sqlite.to_string().contains(&expected_cause),
            "original cause must survive retry: {sqlite}"
        );
        assert_eq!(
            observed.load(Ordering::Relaxed),
            expected_attempts,
            "retry once only for actual selected missing index"
        );
        if expected_attempts == 2 {
            capture.assert_one(INDEXES[0], "fallback_cause");
        } else {
            assert!(capture.events.lock().unwrap().is_empty());
        }
    }
}
