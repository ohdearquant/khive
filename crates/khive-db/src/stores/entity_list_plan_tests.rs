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
    assert_missing_count_free_kind_fallback(store, pool, external, capture).await;
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
        .copied()
        .chain(std::iter::once(KIND_ORDER_INDEX))
        .all(|index| catalog(&external).iter().any(|entry| entry.0 == index)));
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
        .copied()
        .chain(std::iter::once(KIND_ORDER_INDEX))
        .all(|index| !catalog(&external).iter().any(|entry| entry.0 == index)));
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

const KIND_ORDER_INDEX: &str = "idx_entities_live_namespace_kind_order";

type BoundEntitySql = (String, Vec<Box<dyn rusqlite::ToSql>>);

fn ordered_page_sql(
    filter: &EntityFilter,
    limit: i64,
    offset: i64,
    streaming: bool,
) -> BoundEntitySql {
    let (clause, mut params) = if streaming {
        build_entity_streaming_where("local", filter)
    } else {
        build_entity_where("local", filter)
    };
    let order = if let Some(prefix) = &filter.name_prefix {
        params.push(Box::new(prefix.to_ascii_lowercase()));
        format!(
            "CASE WHEN LOWER(name) = ?{} THEN 0 ELSE 1 END, created_at DESC, id DESC",
            params.len()
        )
    } else {
        "created_at DESC, id DESC".into()
    };
    params.push(Box::new(limit));
    params.push(Box::new(offset));
    let builder = if streaming {
        build_entity_count_free_page_query
    } else {
        build_entity_page_query
    };
    (
        builder(
            ENTITY_SELECT_COLUMNS,
            filter,
            &clause,
            &order,
            params.len() - 1,
            params.len(),
        ),
        params,
    )
}

fn assert_streamed_order(plan: &[String]) {
    assert!(
        !plan.iter().any(|line| line.contains("TEMP B-TREE")),
        "unexpected full-match sort: {plan:?}"
    );
    assert!(
        !plan.iter().any(|line| line.contains("LIST SUBQUERY")),
        "unexpected type-ID materialization: {plan:?}"
    );
}

fn ordered_backlog_fixture(older: usize) -> Connection {
    let conn = fixture(0);
    for (id, created) in (1..=96)
        .map(|i| (i, 10_000 + (i / 3) as i64))
        .chain((1..=older).map(|i| (100_000 + i, -(i as i64))))
    {
        conn.execute(
            "INSERT INTO entities(id,namespace,kind,entity_type,name,description,properties,tags,created_at,updated_at) VALUES(?1,'local',?2,?3,?4,'projection',?5,'[\"Tag\"]',?6,?6)",
            rusqlite::params![Uuid::from_u128(id as u128).to_string(),
                if id % 17 == 0 { "document" } else { "concept" },
                if id % 5 == 0 { None } else { Some(if id % 2 == 0 { "a" } else { "b" }) },
                format!("row{id}"), if id % 2 == 0 { "{\"type\":\"a\"}" } else { "{\"type\":\"b\"}" }, created],
        ).unwrap();
    }
    conn
}

#[test]
fn kind_filtered_offset_pages_stream_order() {
    let cases = [
        (EntityFilter::default(), INDEXES[0]),
        (
            EntityFilter {
                kinds: vec!["concept".into()],
                ..Default::default()
            },
            KIND_ORDER_INDEX,
        ),
        (
            EntityFilter {
                kinds: vec!["document".into()],
                ..Default::default()
            },
            KIND_ORDER_INDEX,
        ),
        (
            EntityFilter {
                kinds: vec!["concept".into(), "document".into()],
                ..Default::default()
            },
            INDEXES[0],
        ),
        (
            EntityFilter {
                kinds: vec!["concept".into()],
                entity_types: vec!["a".into(), "b".into()],
                legacy_entity_type_fallback: true,
                tags_any: vec!["tag".into()],
                ..Default::default()
            },
            KIND_ORDER_INDEX,
        ),
        (
            EntityFilter {
                entity_types_by_kind: [("document".into(), vec!["a".into(), "b".into()])].into(),
                legacy_entity_type_fallback: true,
                ..Default::default()
            },
            KIND_ORDER_INDEX,
        ),
        (
            EntityFilter {
                namespaces: vec!["local".into(), "local".into()],
                entity_types: vec!["a".into(), "a".into()],
                ..Default::default()
            },
            INDEXES[1],
        ),
        (
            EntityFilter {
                entity_types: vec!["a".into(), "b".into()],
                ..Default::default()
            },
            INDEXES[0],
        ),
    ];
    for analyzed in [false, true] {
        let mut work = Vec::new();
        for older in [200, 2_000] {
            let conn = ordered_backlog_fixture(older);
            if analyzed {
                conn.execute_batch("ANALYZE").unwrap();
            }
            let mut first_page_work = Vec::new();
            for (filter, index) in &cases {
                for limit in [3, 7] {
                    for offset in [0, 5] {
                        let (sql, params) = ordered_page_sql(filter, limit, offset, true);
                        let (oracle, oracle_params) =
                            ordered_page_sql(filter, limit, offset, false);
                        assert_eq!(
                            values(&conn, &sql, &params),
                            values(&conn, &oracle, &oracle_params),
                            "complete rows: {filter:?}, {limit}, {offset}"
                        );
                        let (plan, steps) = plan_work(&conn, &sql, &params);
                        assert_streamed_order(&plan);
                        assert!(
                            plan.iter().any(|line| line.contains(*index)),
                            "{index}: {plan:?}"
                        );
                        if offset == 0 {
                            first_page_work.push(steps);
                        }
                    }
                    // Concatenated pages must preserve the oracle's complete ordering.
                    let (oracle, params) = ordered_page_sql(filter, 31, 0, false);
                    let expected = values(&conn, &oracle, &params);
                    let mut joined = Vec::new();
                    while joined.len() < expected.len() {
                        let (sql, params) =
                            ordered_page_sql(filter, limit, joined.len() as i64, true);
                        let page = values(&conn, &sql, &params);
                        assert!(!page.is_empty());
                        joined.extend(page);
                    }
                    joined.truncate(expected.len());
                    assert_eq!(joined, expected);
                }
            }
            // The former multi-type type-order route really sorts, even with the new index present.
            let filter = &cases[7].0;
            let (old, params) = ordered_page_sql(filter, 7, 0, false);
            let (old_plan, _) = plan_work(&conn, &old, &params);
            assert!(
                old_plan.iter().any(|line| line.contains("TEMP B-TREE")),
                "two distinct type prefixes must expose the former sorter: {old_plan:?}"
            );
            // Keep the pre-index kind access shape as a query-plan negative control.
            let filter = &cases[1].0;
            let (sql, params) = ordered_page_sql(filter, 7, 0, true);
            let old = sql.replace(
                entity_count_free_source(filter),
                "entities INDEXED BY idx_entities_kind_entity_type",
            );
            let (old_plan, _) = plan_work(&conn, &old, &params);
            assert!(
                old_plan.iter().any(|line| line.contains("TEMP B-TREE")),
                "pre-kind-order shape must sort: {old_plan:?}"
            );
            assert_eq!(values(&conn, &sql, &params), values(&conn, &old, &params));
            work.push(first_page_work);
        }
        for (small, large) in work[0].iter().zip(&work[1]) {
            assert!(*large < *small * 2, "older matching backlog must not scale first-page work: {small} -> {large}, analyzed={analyzed}");
        }
    }
}

#[test]
fn streaming_type_predicate_preserves_filter_semantics() {
    let conn = fixture(0);
    // Expected membership is an independent fixture annotation, not derived by the new predicate.
    let cases = [
        (Some("a"), Some("invalid JSON"), true),
        (Some("b"), Some("{\"type\":\"a\"}"), false),
        (Some(""), Some("{\"type\":\"a\"}"), false),
        (None, Some("{\"type\":\"a\"}"), true),
        (None, Some("{\"type\":\"alias\"}"), true),
        (None, None, false),
        (None, Some("{}"), false),
        (None, Some("{\"type\":null}"), false),
        (None, Some("{\"type\":3}"), false),
        (None, Some("{\"type\":[\"a\"]}"), false),
        (None, Some("{\"type\":true}"), false),
        (None, Some("{type:'a'}"), false),
        (None, Some("invalid JSON"), false),
        (None, Some("{\"type\":\"A\"}"), false),
        (Some("alias"), Some("{\"type\":\"b\"}"), true),
    ];
    let mut expected = Vec::new();
    for (i, (canonical, properties, matches)) in cases.into_iter().enumerate() {
        let id = Uuid::from_u128((i + 1) as u128).to_string();
        conn.execute("INSERT INTO entities(id,namespace,kind,entity_type,name,properties,tags,created_at,updated_at) VALUES(?1,'local','concept',?2,'fixture',?3,'[\"Tag\"]',?4,?4)", rusqlite::params![id, canonical, properties, i as i64]).unwrap();
        if matches {
            expected.push(id);
        }
    }
    for (id, ns, kind, deleted) in [
        (100, "foreign", "concept", false),
        (101, "local", "concept", true),
        (102, "local", "document", false),
    ] {
        conn.execute("INSERT INTO entities(id,namespace,kind,entity_type,name,tags,created_at,updated_at,deleted_at) VALUES(?1,?2,?3,'a','fixture','[\"Tag\"]',100,100,?4)",rusqlite::params![Uuid::from_u128(id).to_string(),ns,kind,if deleted {Some(1)} else {None}]).unwrap();
    }
    let filter = EntityFilter {
        kinds: vec!["concept".into()],
        entity_types: vec!["a".into(), "alias".into()],
        legacy_entity_type_fallback: true,
        tags_any: vec!["TAG".into()],
        ..Default::default()
    };
    let (sql, params) = ordered_page_sql(&filter, 100, 0, true);
    let rows = values(&conn, &sql, &params);
    expected.reverse();
    assert_eq!(
        rows.iter()
            .map(|row| match &row[0] {
                rusqlite::types::Value::Text(id) => id.clone(),
                value => panic!("{value:?}"),
            })
            .collect::<Vec<_>>(),
        expected
    );
    let filters = [
        filter.clone(),
        EntityFilter {
            entity_types_by_kind: [
                ("concept".into(), vec!["a".into()]),
                ("document".into(), vec!["alias".into()]),
            ]
            .into(),
            ..filter.clone()
        },
        EntityFilter {
            kinds: vec![],
            entity_types_by_kind: [
                ("concept".into(), vec!["alias".into()]),
                ("document".into(), vec!["a".into()]),
            ]
            .into(),
            ..filter.clone()
        },
        EntityFilter {
            entity_types_by_kind: [("concept".into(), vec![])].into(),
            ..filter.clone()
        },
        EntityFilter {
            entity_types_by_kind: [
                ("empty".into(), vec![]),
                ("concept".into(), vec!["a".into()]),
            ]
            .into(),
            ..filter.clone()
        },
        EntityFilter {
            ids: vec![Uuid::from_u128(1), Uuid::from_u128(4), Uuid::from_u128(100)],
            ..filter.clone()
        },
        EntityFilter {
            namespaces: vec!["local".into(), "foreign".into()],
            name_exact: Some("fixture".into()),
            ..filter
        },
    ];
    for filter in filters {
        for offset in [0, 2, 100] {
            let (sql, params) = ordered_page_sql(&filter, 5, offset, true);
            let (old, old_params) = ordered_page_sql(&filter, 5, offset, false);
            assert_eq!(
                values(&conn, &sql, &params),
                values(&conn, &old, &old_params),
                "{filter:?}, offset={offset}"
            );
            let (plan, _) = plan_work(&conn, &sql, &params);
            assert!(
                !plan.iter().any(|line| line.contains("LIST SUBQUERY")),
                "row-local predicate must not materialize IDs: {plan:?}"
            );
        }
    }
}

#[test]
fn legacy_type_materialization_control_detects_older_backlog() {
    let filter = EntityFilter {
        kinds: vec!["concept".into()],
        entity_types: vec!["a".into(), "b".into()],
        legacy_entity_type_fallback: true,
        ..Default::default()
    };
    for analyzed in [false, true] {
        let mut work = Vec::new();
        for older in [200, 2_000] {
            let conn = ordered_backlog_fixture(older);
            if analyzed {
                conn.execute_batch("ANALYZE").unwrap();
            }
            let (new_sql, new_params) = ordered_page_sql(&filter, 5, 0, true);
            let (old_where, mut old_params) = build_entity_where("local", &filter);
            old_params.push(Box::new(5_i64));
            old_params.push(Box::new(0_i64));
            let old_sql = build_entity_count_free_page_query(
                ENTITY_SELECT_COLUMNS,
                &filter,
                &old_where,
                "created_at DESC, id DESC",
                old_params.len() - 1,
                old_params.len(),
            );
            assert_eq!(
                values(&conn, &new_sql, &new_params),
                values(&conn, &old_sql, &old_params)
            );
            let (new_plan, new_steps) = plan_work(&conn, &new_sql, &new_params);
            let (old_plan, old_steps) = plan_work(&conn, &old_sql, &old_params);
            assert_streamed_order(&new_plan);
            assert!(
                old_plan.iter().any(|line| line.contains("LIST SUBQUERY")),
                "negative arm must expose the original type-ID materialization: {old_plan:?}"
            );
            work.push((new_steps, old_steps));
        }
        assert!(work[1].0 < work[0].0 * 2, "streaming work: {work:?}");
        assert!(
            work[1].1 > work[0].1 * 2,
            "negative materialization arm must grow: {work:?}"
        );
    }
}

fn seed_kind_cursor_rows(conn: &Connection, prefix: usize) -> (SeekCursor, Vec<Uuid>) {
    conn.execute_batch(ENTITIES_DDL).unwrap();
    conn.execute_batch(include_str!("../../sql/021-attachments-a-stage.sql"))
        .unwrap();
    for i in 1..=prefix {
        // Keep the selected kind rare in both actual rows and ANALYZE's
        // average rows-per-kind estimate; all prefix rows remain nonmatches.
        conn.execute("INSERT INTO entities(id,namespace,kind,name,created_at,updated_at) VALUES(?1,'local',?2,'prefix',1,1)", rusqlite::params![Uuid::from_u128(i as u128).to_string(), format!("irrelevant{}", i % 16)]).unwrap();
    }
    let boundary = SeekCursor {
        sequence: conn
            .query_row("SELECT MAX(seq) FROM entities_seq", [], |row| row.get(0))
            .unwrap(),
        id: Uuid::from_u128(prefix as u128),
    };
    let mut expected = Vec::new();
    for i in 1..=81 {
        let id = Uuid::from_u128(100_000 + i);
        let matches = i.is_multiple_of(3);
        conn.execute("INSERT INTO entities(id,namespace,kind,entity_type,name,properties,tags,created_at,updated_at) VALUES(?1,'local',?2,?3,?4,?5,'[\"Tag\"]',?6,?6)",rusqlite::params![id.to_string(),if matches {"concept"} else {"irrelevant"},if i%2==0 {Some("a")} else {None},format!("tail{i}"),if i%2==0 {"{}"} else {"{\"type\":\"alias\"}"}, (100-i) as i64]).unwrap();
        if matches {
            expected.push(id);
        }
    }
    (boundary, expected)
}

fn kind_cursor_filter() -> EntityFilter {
    EntityFilter {
        kinds: vec!["concept".into()],
        entity_types_by_kind: [("concept".into(), vec!["a".into(), "alias".into()])].into(),
        legacy_entity_type_fallback: true,
        tags_any: vec!["tag".into()],
        ..Default::default()
    }
}

#[tokio::test]
async fn kind_filtered_cursor_pages_seek_in_sequence_order() {
    for analyzed in [false, true] {
        let mut work = Vec::new();
        for prefix in [200, 2_000] {
            let pool = Arc::new(ConnectionPool::new(crate::pool::PoolConfig::for_test()).unwrap());
            let (boundary, expected) = {
                let writer = pool.writer().unwrap();
                let result = seed_kind_cursor_rows(writer.conn(), prefix);
                if analyzed {
                    writer.conn().execute_batch("ANALYZE").unwrap();
                }
                result
            };
            let filter = kind_cursor_filter();
            {
                let reader = pool.reader().unwrap();
                let conn = reader.conn();
                let (mut clause, mut params) = build_entity_streaming_where("local", &filter);
                params.push(Box::new(boundary.sequence));
                clause.push_str(&format!(" AND entities_seq.seq > ?{}", params.len()));
                params.push(Box::new(6_i64));
                let sql = build_entity_cursor_query(
                    ENTITY_SELECT_COLUMNS,
                    &filter,
                    &clause,
                    params.len(),
                );
                let (plan, steps) = plan_work(conn, &sql, &params);
                assert_streamed_order(&plan);
                let sequence = plan
                    .iter()
                    .position(|line| {
                        line.contains("SEARCH entities_seq")
                            && line.contains("INTEGER PRIMARY KEY")
                            && line.contains("rowid>?")
                    })
                    .expect("real sequence range seek");
                let entity = plan
                    .iter()
                    .position(|line| {
                        line.contains("SEARCH entities USING INDEX sqlite_autoindex_entities_1")
                            && line.contains("id=?")
                    })
                    .expect("one entity primary-key probe per sequence");
                assert!(
                    sequence < entity,
                    "sequence must drive entity probes: {plan:?}"
                );
                let rows = values(conn, &sql, &params);
                assert_eq!(rows.len(), 6, "limit+1 probe");
                for (row, id) in rows.iter().zip(&expected) {
                    assert_eq!(row[0], rusqlite::types::Value::Text(id.to_string()));
                }
                // Literal pre-fix kind-filtered FROM clause, without an index
                // hint. Reuse the row-local predicate to isolate join ordering.
                let old_unforced=format!("SELECT {ENTITY_SELECT_COLUMNS}, entities_seq.seq FROM entities_seq JOIN entities ON entities.id = entities_seq.entity_id{clause} ORDER BY entities_seq.seq ASC LIMIT ?{}",params.len());
                let (old_unforced_plan, _) = plan_work(conn, &old_unforced, &params);
                assert!(
                    old_unforced_plan
                        .iter()
                        .any(|line| line.contains("TEMP B-TREE")),
                    "unforced pre-fix JOIN must expose sequence sort: {old_unforced_plan:?}"
                );
                assert_eq!(
                    values(conn, &old_unforced, &params),
                    rows,
                    "unforced pre-fix arm must retain complete rows"
                );
                // A supplementary forced-index control keeps that inefficient
                // access shape explicit; it is not the literal old-query control.
                let old=format!("SELECT {ENTITY_SELECT_COLUMNS}, entities_seq.seq FROM entities_seq JOIN entities INDEXED BY idx_entities_kind_entity_type ON entities.id = entities_seq.entity_id{clause} ORDER BY entities_seq.seq ASC LIMIT ?{}",params.len());
                let (old_plan, _) = plan_work(conn, &old, &params);
                assert!(
                    old_plan.iter().any(|line| line.contains("TEMP B-TREE")),
                    "negative JOIN must expose sequence sort: {old_plan:?}"
                );
                assert_eq!(
                    values(conn, &old, &params),
                    rows,
                    "negative arm must retain complete rows"
                );
                work.push(steps);
            }
            let store = SqlEntityStore::new(Arc::clone(&pool), false);
            for limit in [1, 5] {
                let mut after = Some(boundary);
                let mut walked = Vec::new();
                for _ in 0..=expected.len() {
                    let page = store
                        .query_entities_after("local", filter.clone(), after, limit)
                        .await
                        .unwrap();
                    for entity in &page.items {
                        let direct = store.get_entity(entity.id).await.unwrap().unwrap();
                        assert_eq!(
                            serde_json::to_value(entity).unwrap(),
                            serde_json::to_value(direct).unwrap()
                        );
                    }
                    if let Some(next) = &page.next_after {
                        assert_eq!(
                            next.id,
                            page.items.last().unwrap().id,
                            "cursor UUID must match page boundary"
                        );
                        assert!(
                            next.sequence > after.as_ref().unwrap().sequence,
                            "cursor must advance"
                        );
                        let actual_seq: i64 = pool
                            .reader()
                            .unwrap()
                            .conn()
                            .query_row(
                                "SELECT seq FROM entities_seq WHERE entity_id=?1",
                                [next.id.to_string()],
                                |row| row.get(0),
                            )
                            .unwrap();
                        assert_eq!(
                            next.sequence, actual_seq,
                            "cursor sequence/UUID pair must match ledger"
                        );
                    }
                    walked.extend(page.items.into_iter().map(|entity| entity.id));
                    after = page.next_after;
                    if after.is_none() {
                        break;
                    }
                }
                assert!(after.is_none(), "full walk must terminate");
                assert_eq!(
                    walked, expected,
                    "full sequence order, including interspersed nonmatches"
                );
            }
            let empty = store
                .query_entities_after("local", filter, Some(boundary), 0)
                .await
                .unwrap();
            assert!(empty.items.is_empty() && empty.next_after.is_none());
        }
        assert!(
            work[1] < work[0] * 2,
            "only the older prefix grew, subsequent page must seek past it: {work:?}"
        );
    }
}

async fn assert_missing_count_free_kind_fallback(
    store: &dyn EntityStore,
    pool: &ConnectionPool,
    external: &Connection,
    capture: &ListDiagnosticCapture,
) {
    let filter = EntityFilter {
        kinds: vec!["concept".into()],
        ..Default::default()
    };
    let request = PageRequest {
        limit: 2,
        offset: 1,
    };
    let catalog_before = catalog(external);
    let writers = pool.writer_acquisition_snapshot().pooled_acquisitions;
    let expected = store
        .query_entities_count_free("local", filter.clone(), request.clone())
        .await
        .unwrap();
    assert_eq!(expected.total, None);
    assert_eq!(expected.items.len(), 2);
    assert_eq!(
        catalog(external),
        catalog_before,
        "ordinary count-free read changes no schema"
    );
    assert_eq!(
        pool.writer_acquisition_snapshot().pooled_acquisitions,
        writers
    );
    assert!(capture.events.lock().unwrap().is_empty());
    external
        .execute_batch(&format!("DROP INDEX {KIND_ORDER_INDEX}"))
        .unwrap();
    let missing_catalog = catalog(external);
    let writers = pool.writer_acquisition_snapshot().pooled_acquisitions;
    let readers = pool.reader_acquisition_snapshot().pooled_checkouts;
    let actual = store
        .query_entities_count_free("local", filter, request)
        .await
        .unwrap();
    assert_eq!(actual.total, None);
    assert_eq!(
        serde_json::to_value(actual.items).unwrap(),
        serde_json::to_value(expected.items).unwrap()
    );
    capture.assert_one(KIND_ORDER_INDEX, "query_entities_count_free");
    assert_eq!(
        catalog(external),
        missing_catalog,
        "retry must not repair the missing index"
    );
    assert_eq!(
        pool.writer_acquisition_snapshot().pooled_acquisitions,
        writers
    );
    assert_eq!(
        pool.reader_acquisition_snapshot().pooled_checkouts,
        readers + 1,
        "retry reuses its reader"
    );
}

#[tokio::test]
async fn count_free_kind_fallback_preserves_unrelated_errors_and_retries_once() {
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
        .execute_batch(&format!("DROP INDEX {KIND_ORDER_INDEX}"))
        .unwrap();
    let store = SqlEntityStore::new(Arc::clone(&pool), false);
    let capture = ListDiagnosticCapture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    let before = catalog(pool.reader().unwrap().conn());
    for (sql, attempts, cause) in [
        (
            format!("SELECT id FROM entities INDEXED BY {KIND_ORDER_INDEX}"),
            2,
            format!("no such index: {KIND_ORDER_INDEX}"),
        ),
        (
            "SELECT id FROM entities INDEXED BY missing_unrelated_index".into(),
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
            .with_list_reader(
                "count_free_fallback_cause",
                Some(KIND_ORDER_INDEX),
                move |conn, _| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    conn.query_row(&sql, [], |row| row.get::<_, String>(0))
                },
            )
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
        assert_eq!(operation.as_ref(), "count_free_fallback_cause");
        assert!(source
            .downcast_ref::<rusqlite::Error>()
            .unwrap()
            .to_string()
            .contains(&cause));
        assert_eq!(observed.load(Ordering::Relaxed), attempts);
        if attempts == 2 {
            capture.assert_one(KIND_ORDER_INDEX, "count_free_fallback_cause");
        } else {
            assert!(capture.events.lock().unwrap().is_empty());
        }
        assert_eq!(catalog(pool.reader().unwrap().conn()), before);
    }
}
