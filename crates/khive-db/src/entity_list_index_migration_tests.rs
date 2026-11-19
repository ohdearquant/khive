use super::run_migrations_for_test as run_migrations;
use super::*;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use std::sync::{Arc, Mutex};

const INDEXES: [&str; 3] = [
    "idx_entities_live_namespace_order",
    "idx_entities_live_namespace_type_order",
    "idx_entities_live_namespace_kind_order",
];

fn expected_definition(index: &str) -> &'static str {
    match index {
        "idx_entities_live_namespace_order" => "CREATE INDEX idx_entities_live_namespace_order ON entities(namespace, created_at DESC, id DESC) WHERE deleted_at IS NULL",
        "idx_entities_live_namespace_type_order" => "CREATE INDEX idx_entities_live_namespace_type_order ON entities(namespace, entity_type, created_at DESC, id DESC) WHERE deleted_at IS NULL",
        "idx_entities_live_namespace_kind_order" => "CREATE INDEX \
            idx_entities_live_namespace_kind_order ON entities(namespace, kind, created_at DESC, \
            id DESC) WHERE deleted_at IS NULL",
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

fn historical_through(conn: &mut Connection, through_version: u32) {
    conn.execute_batch(MIGRATION_TRACKING_TABLE).unwrap();
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= through_version)
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
    assert_eq!(read_schema_version(conn).unwrap(), through_version);
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
    let kind_ledger: String = fresh
        .query_row(
            "SELECT name FROM _schema_migrations WHERE version=53",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(kind_ledger, "entity_kind_list_order");
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
        historical_through(&mut conn, 49);
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

#[test]
fn entity_kind_index_upgrade_preserves_rows_sequences_and_existing_btrees() {
    for through_version in MIGRATIONS
        .iter()
        .filter(|migration| (51..53).contains(&migration.version))
        .map(|migration| migration.version)
    {
        for preexisting in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("upgrade.db");
            let mut conn = Connection::open(&path).unwrap();
            historical_through(&mut conn, through_version);
            assert_eq!(catalog(&conn).len(), 2);
            conn.execute_batch(concat!(
                r#"INSERT INTO entities(id,namespace,kind,entity_type,name,description,"#,
                r#"properties,tags,created_at,updated_at,deleted_at) VALUES('held','local',"#,
                r#"'concept','rare','Held','initial','{"type":"rare"}','["retained"]',1,1,NULL),"#,
                r#"('deleted','other','document',NULL,'Deleted',NULL,NULL,'[]',2,2,"#,
                r#"3); UPDATE entities SET description='revised',updated_at=20,"#,
                r#"version=version+1 WHERE id='held';"#,
            ))
            .unwrap();
            if preexisting {
                conn.execute_batch(expected_definition(
                    "idx_entities_live_namespace_kind_order",
                ))
                .unwrap();
            }
            let snapshot = |conn: &Connection| {
                let mut statement = conn.prepare("SELECT * FROM entities ORDER BY id").unwrap();
                let columns = statement.column_count();
                let rows = statement
                    .query_map([], |row| {
                        (0..columns)
                            .map(|column| row.get::<_, rusqlite::types::Value>(column))
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap();
                let sequences: Vec<(i64, String)> = conn
                    .prepare("SELECT seq,entity_id FROM entities_seq ORDER BY seq")
                    .unwrap()
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                (rows, sequences)
            };
            let before_rows = snapshot(&conn);
            let before_catalog = catalog(&conn);
            let protected: Vec<String> = before_catalog.iter().map(|row| row.0.clone()).collect();
            let refused = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&refused);
            conn.authorizer(Some(move |context: AuthContext<'_>| {
                let index = match context.action {
                    AuthAction::CreateIndex { index_name, .. }
                    | AuthAction::DropIndex { index_name, .. }
                    | AuthAction::Reindex { index_name } => Some(index_name),
                    _ => None,
                };
                if index.is_some_and(|index| protected.iter().any(|name| name == index)) {
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
                "existing indexes must not be rebuilt"
            );
            assert_definitions(&conn);
            assert_eq!(snapshot(&conn), before_rows);
            let upgraded_catalog = catalog(&conn);
            for retained in &before_catalog {
                assert!(
                    upgraded_catalog.contains(retained),
                    "definition and rootpage retained"
                );
            }
            let ledger: String = conn
                .query_row(
                    "SELECT name FROM _schema_migrations WHERE version=53",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(ledger, "entity_kind_list_order");
            let changes = conn.total_changes();
            assert_eq!(run_migrations(&mut conn).unwrap(), latest_schema_version());
            assert_eq!(conn.total_changes(), changes);
            assert_eq!(catalog(&conn), upgraded_catalog);
            assert_eq!(snapshot(&conn), before_rows);
            drop(conn);
            let mut reopened = Connection::open(&path).unwrap();
            assert_eq!(
                run_migrations(&mut reopened).unwrap(),
                latest_schema_version()
            );
            assert_eq!(catalog(&reopened), upgraded_catalog);
            assert_eq!(snapshot(&reopened), before_rows);
            drop(reopened);
            let readonly =
                Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .unwrap();
            let changes = readonly.total_changes();
            validate_schema_is_current(&readonly).unwrap();
            assert_eq!(readonly.total_changes(), changes);
            assert_eq!(catalog(&readonly), upgraded_catalog);
            assert_eq!(snapshot(&readonly), before_rows);
        }
    }
}
