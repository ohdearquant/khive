use super::*;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use std::sync::{Arc, Mutex};

// Independent witnesses of the ten pack-created definitions named by #4011.
const LEGACY_INDEXES: [&str; 10] = [
    "CREATE INDEX IF NOT EXISTS idx_git_notes_live_commit_sha ON notes(namespace, kind, json_extract(properties,'$.sha')) WHERE kind='commit' AND deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_git_notes_live_number_project ON notes(namespace, kind, json_extract(properties,'$.number'), json_extract(properties,'$.project_id')) WHERE kind IN ('issue','pull_request') AND deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_schedule_trigger ON notes(namespace, kind, json_extract(properties, '$.trigger_at')) WHERE deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_schedule_creator_provenance ON events(namespace, verb, target_id, outcome)",
    "CREATE INDEX IF NOT EXISTS idx_comm_message_direction ON notes(namespace, kind, json_extract(properties, '$.direction'), json_extract(properties, '$.read'), created_at DESC) WHERE deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_comm_message_thread ON notes(namespace, kind, json_extract(properties, '$.thread_id'), created_at DESC) WHERE deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_comm_message_to_actor ON notes(namespace, kind, json_extract(properties, '$.to_actor'), json_extract(properties, '$.direction'), json_extract(properties, '$.read'), created_at DESC) WHERE deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_comm_message_outbound_ref ON notes(namespace, kind, json_extract(properties, '$.direction'), json_extract(properties, '$.from_actor'), json_extract(properties, '$.outbound_ref')) WHERE deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_comm_message_outbound_recipient ON notes(namespace, kind, json_extract(properties, '$.direction'), json_extract(properties, '$.to_actor'), created_at DESC, id ASC) WHERE deleted_at IS NULL",
    "CREATE INDEX IF NOT EXISTS idx_comm_quarantine_expiry ON notes(namespace, kind, json_extract(properties, '$.channel_kind'), json_extract(properties, '$.channel_slug'), expires_at, id) WHERE deleted_at IS NULL AND expires_at IS NOT NULL",
];

fn names() -> Vec<&'static str> {
    LEGACY_INDEXES
        .iter()
        .map(|sql| sql.split_whitespace().nth(5).unwrap())
        .collect()
}

fn catalog(conn: &Connection) -> Vec<(String, String, i64)> {
    conn.prepare("SELECT name,sql,rootpage FROM sqlite_schema WHERE type='index' AND sql IS NOT NULL ORDER BY name")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<(String, String, i64)>>>()
        .unwrap()
        .into_iter()
        .filter(|row| names().contains(&row.0.as_str()))
        .collect()
}

fn assert_all_ten(conn: &Connection) {
    let rows = catalog(conn);
    let actual: Vec<&str> = rows.iter().map(|row| row.0.as_str()).collect();
    let mut expected = names();
    expected.sort_unstable();
    assert_eq!(
        actual, expected,
        "all ten core indexes exist without loading packs"
    );
    for sql in LEGACY_INDEXES {
        let name = sql.split_whitespace().nth(5).unwrap();
        let definition = &rows.iter().find(|row| row.0 == name).unwrap().1;
        assert_eq!(
            definition.split_whitespace().collect::<Vec<_>>().join(" "),
            sql.replace(" IF NOT EXISTS", ""),
            "legacy definition of {name}"
        );
    }
    let ledger: String = conn
        .query_row(
            "SELECT name FROM _schema_migrations WHERE version=52",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ledger, "comm_core_indexes");
}

fn historical_v51(conn: &mut Connection) {
    conn.execute_batch(MIGRATION_TRACKING_TABLE).unwrap();
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= 51)
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
    assert_eq!(read_schema_version(conn).unwrap(), 51);
}

fn stored_rows(conn: &Connection) -> Vec<(String, String, String, i64, Option<i64>)> {
    conn.prepare("SELECT id,content,properties,version,deleted_at FROM notes ORDER BY id")
        .unwrap()
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn fresh_pack_free_core_installs_all_ten_indexes_and_reopens_readonly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("comm-core.db");
    let mut conn = Connection::open(&path).unwrap();
    run_migrations(&mut conn).unwrap();
    assert_all_ten(&conn);
    let cursor_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='comm_channel_cursor'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        cursor_count, 0,
        "core migration does not install pack auxiliary tables"
    );
    let before = catalog(&conn);
    run_migrations(&mut conn).unwrap();
    assert_eq!(catalog(&conn), before);
    drop(conn);
    let readonly =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    validate_schema_is_current(&readonly).unwrap();
    assert_all_ten(&readonly);
    assert_eq!(catalog(&readonly), before);
}

fn upgrade(preexisting: bool) {
    let mut conn = Connection::open_in_memory().unwrap();
    historical_v51(&mut conn);
    assert_eq!(
        catalog(&conn).len(),
        4,
        "V51 owns the original git/schedule pairs"
    );
    conn.execute_batch(r#"
        INSERT INTO notes(id,namespace,kind,properties,content,created_at,updated_at,expires_at,deleted_at)
        VALUES('inbound','local','message','{"direction":"inbound","read":false,"to_actor":"actor:recipient","from_actor":"actor:sender","thread_id":"thread","outbound_ref":"outbound"}','inbound body',1,1,NULL,NULL),
              ('outbound','local','message','{"direction":"outbound","read":false,"to_actor":"email:recipient","from_actor":"actor:sender","thread_id":"thread"}','outbound body',2,2,NULL,NULL),
              ('quarantine','local','message','{"direction":"inbound","channel_kind":"email","channel_slug":"support"}','quarantined body',3,3,99,NULL),
              ('deleted','local','message','{"direction":"inbound","read":true,"to_actor":"actor:recipient"}','deleted body',4,4,NULL,5),
              ('commit','local','commit','{"sha":"0123456789abcdef0123456789abcdef01234567"}','commit body',6,6,NULL,NULL),
              ('issue','local','issue','{"number":4011,"project_id":"project"}','issue body',7,7,NULL,NULL),
              ('reminder','local','scheduled_event','{"trigger_at":"2099-01-01T00:00:00Z","status":"pending"}','reminder body',8,8,NULL,NULL);
        INSERT INTO events(id,namespace,verb,substrate,actor,outcome,target_id,created_at)
        VALUES('audit','local','schedule.remind','note','actor:creator','success','reminder',8);
        CREATE TABLE comm_channel_cursor (
            channel_kind TEXT NOT NULL CHECK (length(trim(channel_kind)) > 0),
            channel_slug TEXT NOT NULL CHECK (length(trim(channel_slug)) > 0),
            source TEXT NOT NULL CHECK (length(trim(source)) > 0),
            generation INTEGER NOT NULL CHECK (generation > 0),
            high_water INTEGER CHECK (high_water IS NULL OR high_water > 0),
            updated_at INTEGER NOT NULL, PRIMARY KEY (channel_kind, channel_slug)
        );
        INSERT INTO comm_channel_cursor VALUES('email','support','imap',2,123,9);
    "#).unwrap();
    if preexisting {
        for sql in LEGACY_INDEXES {
            conn.execute_batch(sql).unwrap();
        }
    }
    let rows_before = stored_rows(&conn);
    let indexes_before = catalog(&conn);
    let held: Vec<String> = indexes_before.iter().map(|row| row.0.clone()).collect();
    let refused = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&refused);
    conn.authorizer(Some(move |context: AuthContext<'_>| {
        let index = match context.action {
            AuthAction::CreateIndex { index_name, .. }
            | AuthAction::DropIndex { index_name, .. }
            | AuthAction::Reindex { index_name } => Some(index_name),
            _ => None,
        };
        if index.is_some_and(|name| held.iter().any(|held| held == name)) {
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
        "held B-trees must not be rebuilt"
    );
    assert_all_ten(&conn);
    let after = catalog(&conn);
    for held in indexes_before {
        assert!(
            after.contains(&held),
            "exact catalog SQL/rootpage retained for {}",
            held.0
        );
    }
    assert_eq!(
        stored_rows(&conn),
        rows_before,
        "content/properties/versions/deletion state retained"
    );
    let event: (String, String, String, String) = conn
        .query_row(
            "SELECT verb,actor,outcome,target_id FROM events WHERE id='audit'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        event,
        (
            "schedule.remind".into(),
            "actor:creator".into(),
            "success".into(),
            "reminder".into()
        )
    );
    let cursor: (String, i64, i64, i64) = conn.query_row(
        "SELECT source,generation,high_water,updated_at FROM comm_channel_cursor WHERE channel_kind='email' AND channel_slug='support'", [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    ).unwrap();
    assert_eq!(cursor, ("imap".into(), 2, 123, 9));
    run_migrations(&mut conn).unwrap();
    assert_eq!(catalog(&conn), after);
    assert_eq!(stored_rows(&conn), rows_before);
}

#[test]
fn v51_upgrade_adds_six_comm_indexes_without_rebuilding_existing_four() {
    upgrade(false);
}

#[test]
fn v51_upgrade_preserves_all_ten_legacy_index_btrees_and_stored_rows() {
    upgrade(true);
}
