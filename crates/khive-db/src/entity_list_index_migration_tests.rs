use super::run_migrations_for_test as run_migrations;
use super::*;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use std::sync::{Arc, Mutex};

const INDEXES: [&str; 2] = [
    "idx_entities_live_namespace_order",
    "idx_entities_live_namespace_type_order",
];

fn expected_definition(index: &str) -> &'static str {
    match index {
        "idx_entities_live_namespace_order" => "CREATE INDEX idx_entities_live_namespace_order ON entities(namespace, created_at DESC, id DESC) WHERE deleted_at IS NULL",
        "idx_entities_live_namespace_type_order" => "CREATE INDEX idx_entities_live_namespace_type_order ON entities(namespace, entity_type, created_at DESC, id DESC) WHERE deleted_at IS NULL",
        _ => unreachable!(),
    }
}

fn catalog(conn: &Connection) -> Vec<(String, String, i64)> {
    conn.prepare("SELECT name,sql,rootpage FROM sqlite_schema WHERE type='index' AND tbl_name='entities' AND name LIKE 'idx_entities_live_namespace_%' ORDER BY name")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn assert_definitions(conn: &Connection) {
    let actual = catalog(conn);
    assert_eq!(actual.len(), INDEXES.len());
    for index in INDEXES {
        let (_, sql, _) = actual.iter().find(|row| row.0 == index).unwrap();
        assert_eq!(
            sql.split_whitespace().collect::<Vec<_>>().join(" "),
            expected_definition(index)
        );
    }
}

fn historical_v49(conn: &mut Connection) {
    conn.execute_batch(MIGRATION_TRACKING_TABLE).unwrap();
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= 49)
    {
        let tx = conn.transaction().unwrap();
        match migration.version {
            21 => {
                stage_attachment_cutover_on_connection(&tx, 0).unwrap();
                finalize_attachment_cutover_on_connection(&tx, 0).unwrap();
            }
            40 => {
                tx.execute_batch(migration.up).unwrap();
                session_identity_migration::apply(&tx).unwrap();
            }
            44 => migrate_outbound_due_key(&tx).unwrap(),
            48 => migrate_acknowledgement_journal(&tx).unwrap(),
            _ => tx.execute_batch(migration.up).unwrap(),
        }
        tx.execute(
            "INSERT INTO _schema_migrations(version,name,applied_at) VALUES(?1,?2,0)",
            rusqlite::params![migration.version, migration.name],
        )
        .unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(read_schema_version(conn).unwrap(), 49);
}

#[test]
fn fresh_migration_and_direct_store_install_identical_entity_list_indexes() {
    let direct = Connection::open_in_memory().unwrap();
    direct
        .execute_batch(include_str!("../sql/entities-ddl.sql"))
        .unwrap();
    assert_definitions(&direct);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fresh.db");
    let mut fresh = Connection::open(&path).unwrap();
    assert_eq!(run_migrations(&mut fresh).unwrap(), latest_schema_version());
    assert_definitions(&fresh);
    let ledger: String = fresh
        .query_row(
            "SELECT name FROM _schema_migrations WHERE version=50",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ledger, "entity_list_plans");
    let before = catalog(&fresh);
    assert_eq!(run_migrations(&mut fresh).unwrap(), latest_schema_version());
    assert_eq!(catalog(&fresh), before);
    drop(fresh);
    let readonly =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    validate_schema_is_current(&readonly).unwrap();
    assert_eq!(catalog(&readonly), before);
}

#[test]
fn v49_upgrade_preserves_rows_and_preexisting_entity_list_index_btrees() {
    for preexisting in [false, true] {
        let mut conn = Connection::open_in_memory().unwrap();
        historical_v49(&mut conn);
        assert!(catalog(&conn).is_empty());
        conn.execute_batch("INSERT INTO entities(id,namespace,kind,entity_type,name,created_at,updated_at,deleted_at) VALUES('held','local','concept','rare','Held',1,1,NULL),('deleted','local','concept','rare','Deleted',2,2,3)").unwrap();
        if preexisting {
            for index in INDEXES {
                conn.execute_batch(expected_definition(index)).unwrap();
            }
        }
        let before = catalog(&conn);
        let refused = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&refused);
        conn.authorizer(Some(move |context: AuthContext<'_>| {
            let index = match context.action {
                AuthAction::CreateIndex { index_name, .. }
                | AuthAction::DropIndex { index_name, .. }
                | AuthAction::Reindex { index_name } => Some(index_name),
                _ => None,
            };
            if preexisting && index.is_some_and(|index| INDEXES.contains(&index)) {
                recorded.lock().unwrap().push(index.unwrap().to_string());
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .unwrap();
        let upgraded = run_migrations(&mut conn);
        conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
        assert_eq!(upgraded.unwrap(), latest_schema_version());
        assert!(
            refused.lock().unwrap().is_empty(),
            "existing index must not be rebuilt"
        );
        assert_definitions(&conn);
        if preexisting {
            assert_eq!(catalog(&conn), before, "definitions and rootpages retained");
        }
        let rows: Vec<(String, String, i64, Option<i64>)> = conn
            .prepare("SELECT id,name,version,deleted_at FROM entities ORDER BY id")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("deleted".into(), "Deleted".into(), 1, Some(3)),
                ("held".into(), "Held".into(), 1, None)
            ]
        );
        let ledger: String = conn
            .query_row(
                "SELECT name FROM _schema_migrations WHERE version=50",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ledger, "entity_list_plans");
    }
}
