use super::run_migrations_for_test as run_migrations;
use super::*;
use crate::pool::{ConnectionPool, PoolConfig};
use rusqlite::{params, types::Value};

// Explain and page using the production builder, including its pre-LIMIT filter.
#[path = "../../khive-pack-knowledge/src/knowledge/cursor_query.rs"]
mod cursor_query;

const NAME: &str = "knowledge_atom_cursor_all_live_tags";

fn historical_prefix(conn: &mut Connection) {
    let next = MIGRATIONS
        .iter()
        .find(|migration| migration.name == NAME)
        .unwrap();
    let admission = WriteAdmission::for_migration_policy(None, &migration_test_policy()).unwrap();
    let mut writes = RawMigrationTransactions::new(conn, &admission);
    writes
        .admitted(|conn| bootstrap_migration_ledger(conn, &admission))
        .unwrap();
    // Execute real migration units and their application-assisted conversions.
    // Do not manufacture a ledger by deleting rows from a current database.
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version < next.version)
    {
        let step = writes
            .admitted(|conn| {
                apply_versioned_migration(conn, migration, &admission, latest_schema_version())
            })
            .unwrap();
        assert!(matches!(step, MigrationStep::Applied));
    }
}

fn index_sql(conn: &Connection) -> String {
    conn.query_row(
        "SELECT sql FROM sqlite_schema WHERE name='idx_knowledge_atoms_cursor'",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

fn seed(conn: &Connection) {
    for (id, namespace, tags, status, deleted) in [
        (
            "00-near",
            "local",
            r#"["type:domain-extra"]"#,
            "active",
            None,
        ),
        (
            "01-prefix",
            "local",
            r#"["prefix:type:domain"]"#,
            "draft",
            None,
        ),
        ("02-case", "local", r#"["TYPE:DOMAIN"]"#, "active", None),
        (
            "03-escaped",
            "local",
            r#"["type\u003adomain"]"#,
            "active",
            None,
        ),
        ("04-exact", "local", r#"["type:domain"]"#, "active", None),
        ("05-legacy", "local", "broken type:domain", "active", None),
        (
            "06-mixed",
            "local",
            r#"["type:domain-extra",7]"#,
            "active",
            None,
        ),
        (
            "07-mixed-escaped",
            "local",
            r#"["type\u003adomain",7]"#,
            "draft",
            None,
        ),
        (
            "08-foreign",
            "other",
            r#"["type:domain-extra"]"#,
            "active",
            None,
        ),
        (
            "09-deleted",
            "local",
            r#"["type:domain-extra"]"#,
            "active",
            Some(99),
        ),
        ("10-malformed", "local", "ordinary", "active", None),
    ] {
        conn.execute("INSERT INTO knowledge_atoms(id,namespace,slug,name,content,tags,status,created_at,updated_at,deleted_at) VALUES(?1,?2,?1,?1,'unchanged bytes',?3,?4,7,8,?5)", params![id,namespace,tags,status,deleted]).unwrap();
    }
}

fn rows(conn: &Connection) -> Vec<(String, String, String, i64, i64, Option<i64>)> {
    conn.prepare(
        "SELECT id,tags,content,created_at,updated_at,deleted_at FROM knowledge_atoms ORDER BY id",
    )
    .unwrap()
    .query_map([], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        ))
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

fn assert_index(conn: &Connection) {
    let sql = index_sql(conn);
    assert_eq!(sql.split_whitespace().collect::<Vec<_>>().join(" "), "CREATE INDEX idx_knowledge_atoms_cursor ON knowledge_atoms(namespace, created_at, id) WHERE deleted_at IS NULL");
    assert!(
        !sql.contains("khive_tag_contains"),
        "no application-defined schema dependency"
    );
}

fn walk_and_explain(conn: &Connection, status: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut after: Option<(i64, String)> = None;
    loop {
        let query = cursor_query::cursor_query(
            "knowledge_atoms",
            "id, created_at",
            after.is_some(),
            status,
        );
        let mut binds = vec![Value::Text("local".into())];
        if let Some((created_at, id)) = &after {
            binds.extend([Value::Integer(*created_at), Value::Text(id.clone())]);
        }
        binds.push(Value::Integer(2));
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
            .unwrap()
            .query_map(rusqlite::params_from_iter(binds.iter()), |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|line| line.contains("idx_knowledge_atoms_cursor")),
            "{plan:?}"
        );
        assert!(
            !plan.iter().any(|line| line.contains("TEMP B-TREE")),
            "{plan:?}"
        );
        let page: Vec<(String, i64)> = conn
            .prepare(&query)
            .unwrap()
            .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        if page.is_empty() {
            break;
        }
        assert!(ids.len() < 12, "cursor must terminate with timestamp ties");
        for (id, _) in &page {
            assert!(!ids.contains(id), "each atom appears once");
            ids.push(id.clone());
        }
        let (id, created_at) = page.last().unwrap();
        after = Some((*created_at, id.clone()));
    }
    ids
}

#[test]
fn legacy_cursor_index_upgrade_keeps_rows_and_uses_ordered_live_index() {
    let pool = ConnectionPool::new(PoolConfig {
        path: None,
        ..PoolConfig::for_test()
    })
    .unwrap();
    let mut writer = pool.writer().unwrap();
    let conn = writer.conn_mut();
    historical_prefix(conn);
    assert!(index_sql(conn).contains("tags NOT LIKE '%type:domain%'"));
    seed(conn);
    let before = rows(conn);
    let domains_before: (String, i64) = conn
        .query_row(
            "SELECT sql,rootpage FROM sqlite_schema WHERE name='idx_knowledge_domains_cursor'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    run_migrations(conn).unwrap();
    assert_index(conn);
    assert_eq!(rows(conn), before);
    let domains_after: (String, i64) = conn
        .query_row(
            "SELECT sql,rootpage FROM sqlite_schema WHERE name='idx_knowledge_domains_cursor'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(domains_after, domains_before, "domain index is untouched");
    assert_eq!(
        walk_and_explain(conn, ""),
        [
            "00-near",
            "01-prefix",
            "02-case",
            "07-mixed-escaped",
            "10-malformed"
        ]
    );
    assert_eq!(
        walk_and_explain(conn, " AND status = 'active'"),
        ["00-near", "02-case", "10-malformed"]
    );
    let rootpage: i64 = conn
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name='idx_knowledge_atoms_cursor'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    run_migrations(conn).unwrap();
    assert_eq!(rows(conn), before);
    let reopened: i64 = conn
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name='idx_knowledge_atoms_cursor'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        reopened, rootpage,
        "repeat startup does not rebuild a current index"
    );
}

#[test]
fn fresh_schema_needs_no_application_function_for_ddl_or_atom_writes() {
    let mut conn = Connection::open_in_memory().unwrap();
    run_migrations(&mut conn).unwrap();
    assert_index(&conn);
    assert!(conn
        .prepare("SELECT khive_tag_contains('[]','type:domain')")
        .is_err());
    seed(&conn);
    assert_eq!(rows(&conn).len(), 11);
    validate_schema_is_current(&conn).unwrap();
}
