use super::run_migrations_for_test as run_migrations;
use super::*;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use std::sync::{Arc, Mutex};

const INDEXES: [&str; 2] = ["idx_schedule_trigger", "idx_schedule_creator_provenance"];

fn expected_definition(name: &str) -> &'static str {
    match name {
        "idx_schedule_trigger" => "CREATE INDEX idx_schedule_trigger ON notes(namespace, kind, json_extract(properties, '$.trigger_at')) WHERE deleted_at IS NULL",
        "idx_schedule_creator_provenance" => "CREATE INDEX idx_schedule_creator_provenance ON events(namespace, verb, target_id, outcome)",
        _ => unreachable!(),
    }
}

fn catalog(conn: &Connection) -> Vec<(String, String, i64)> {
    conn.prepare("SELECT name,sql,rootpage FROM sqlite_schema WHERE type='index' AND name LIKE 'idx_schedule_%' ORDER BY name")
        .unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap().collect::<rusqlite::Result<_>>().unwrap()
}

fn assert_definitions(conn: &Connection) {
    let found = catalog(conn);
    assert_eq!(found.len(), INDEXES.len());
    for name in INDEXES {
        let (_, definition, _) = found.iter().find(|row| row.0 == name).unwrap();
        assert_eq!(
            definition.split_whitespace().collect::<Vec<_>>().join(" "),
            expected_definition(name)
        );
    }
    let ledger: String = conn
        .query_row(
            "SELECT name FROM _schema_migrations WHERE version=51",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ledger, "schedule_core_indexes");
}

fn historical_v50(conn: &mut Connection) {
    conn.execute_batch(MIGRATION_TRACKING_TABLE).unwrap();
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= 50)
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
    assert_eq!(read_schema_version(conn).unwrap(), 50);
}

#[test]
fn fresh_core_without_packs_installs_schedule_indexes_and_replays_readonly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("schedule-core.db");
    let mut conn = Connection::open(&path).unwrap();
    assert_eq!(run_migrations(&mut conn).unwrap(), latest_schema_version());
    assert_definitions(&conn);
    let before = catalog(&conn);
    assert_eq!(run_migrations(&mut conn).unwrap(), latest_schema_version());
    assert_eq!(catalog(&conn), before, "replay does not rebuild indexes");
    drop(conn);
    let readonly =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    validate_schema_is_current(&readonly).unwrap();
    assert_definitions(&readonly);
    assert_eq!(catalog(&readonly), before);
}

#[test]
fn v50_upgrade_keeps_schedule_rows_and_existing_index_btrees() {
    for preexisting in [false, true] {
        let mut conn = Connection::open_in_memory().unwrap();
        historical_v50(&mut conn);
        assert!(catalog(&conn).is_empty());
        conn.execute_batch(r#"
            INSERT INTO notes(id,namespace,kind,properties,content,created_at,updated_at,deleted_at)
            VALUES('held','local','scheduled_event','{"trigger_at":"2099-01-01T00:00:00Z","status":"pending"}','held reminder',1,1,NULL),
                  ('deleted','local','scheduled_event','{"trigger_at":"2099-01-02T00:00:00Z","status":"cancelled"}','deleted reminder',2,2,3);
            INSERT INTO events(id,namespace,verb,substrate,actor,outcome,target_id,created_at)
            VALUES('audit','local','schedule.remind','note','actor:creator','success','held',1);
        "#).unwrap();
        let held_properties: String = conn
            .query_row("SELECT properties FROM notes WHERE id='held'", [], |r| {
                r.get(0)
            })
            .unwrap();
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
            if preexisting && index.is_some_and(|name| INDEXES.contains(&name)) {
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
            "preexisting indexes must not be rebuilt"
        );
        assert_definitions(&conn);
        if preexisting {
            assert_eq!(
                catalog(&conn),
                before,
                "exact definitions and rootpages retained"
            );
        }
        let rows: Vec<(String, String, i64, Option<i64>)> = conn
            .prepare("SELECT id,content,version,deleted_at FROM notes ORDER BY id")
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
                ("deleted".into(), "deleted reminder".into(), 1, Some(3)),
                ("held".into(), "held reminder".into(), 1, None)
            ]
        );
        let actual_properties: String = conn
            .query_row("SELECT properties FROM notes WHERE id='held'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(actual_properties, held_properties);
        let event: (String, String, String, String) = conn
            .query_row(
                "SELECT verb,actor,outcome,target_id FROM events WHERE id='audit'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            event,
            (
                "schedule.remind".into(),
                "actor:creator".into(),
                "success".into(),
                "held".into()
            )
        );
        let after = catalog(&conn);
        assert_eq!(run_migrations(&mut conn).unwrap(), latest_schema_version());
        assert_eq!(catalog(&conn), after);
    }
}
