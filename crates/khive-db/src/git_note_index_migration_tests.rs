use super::*;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use std::sync::{Arc, Mutex};

const INDEXES: [&str; 4] = [
    "idx_git_notes_live_commit_sha",
    "idx_git_notes_live_number_project",
    "idx_git_notes_history_canonical_sha",
    "idx_git_notes_history_noncanonical",
];

fn catalog(conn: &Connection) -> Vec<(String, String, i64)> {
    conn.prepare("SELECT name,sql,rootpage FROM sqlite_master WHERE type='index' AND tbl_name='notes' AND name LIKE 'idx_git_notes_%' ORDER BY name")
        .unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap().collect::<rusqlite::Result<_>>().unwrap()
}

fn expected_definition(name: &str) -> &'static str {
    match name {
            "idx_git_notes_live_commit_sha" => "CREATE INDEX idx_git_notes_live_commit_sha\nON notes(namespace, kind, json_extract(properties,'$.sha'))\nWHERE kind='commit' AND deleted_at IS NULL",
            "idx_git_notes_live_number_project" => "CREATE INDEX idx_git_notes_live_number_project\nON notes(namespace, kind, json_extract(properties,'$.number'),\n         json_extract(properties,'$.project_id'))\nWHERE kind IN ('issue','pull_request') AND deleted_at IS NULL",
            "idx_git_notes_history_canonical_sha" => "CREATE INDEX idx_git_notes_history_canonical_sha\nON notes(namespace, json_extract(properties, '$.sha'))\nWHERE kind = 'commit'\n  AND CASE WHEN json_valid(properties) = 1\n           THEN json_type(properties, '$.sha') = 'text' ELSE 0 END",
            "idx_git_notes_history_noncanonical" => "CREATE INDEX idx_git_notes_history_noncanonical\nON notes(namespace, kind)\nWHERE kind = 'commit' AND json_valid(properties) IS NOT 1",
            _ => unreachable!(),
    }
}

fn assert_definitions(conn: &Connection) {
    let found = catalog(conn);
    assert_eq!(found.len(), 4);
    for name in INDEXES {
        let (_, actual, _) = found.iter().find(|row| row.0 == name).unwrap();
        assert_eq!(
            actual,
            expected_definition(name),
            "exact stored definition for {name}"
        );
    }
}

fn historical_core(conn: &mut Connection) {
    // Apply the historical chain's real special helpers too, including the
    // empty attachment cutover, without depending on a future terminal version.
    conn.execute_batch(MIGRATION_TRACKING_TABLE).unwrap();
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= 48)
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
}

#[test]
fn full_chain_without_packs_installs_exact_git_note_indexes_and_replays() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("core.db");
    let mut conn = Connection::open(&path).unwrap();
    assert_eq!(run_migrations(&mut conn).unwrap(), latest_schema_version());
    assert_definitions(&conn);
    let before = catalog(&conn);
    let pack_tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('git_receipts','git_mirror_cursor')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pack_tables, 0, "no pack schema loaded");
    let ledger: String = conn
        .query_row(
            "SELECT name FROM _schema_migrations WHERE version=49",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ledger, "git_note_property_indexes");
    assert_eq!(run_migrations(&mut conn).unwrap(), latest_schema_version());
    assert_eq!(catalog(&conn), before);
    drop(conn);
    let readonly =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    validate_schema_is_current(&readonly).unwrap();
    assert_definitions(&readonly);
    assert_eq!(catalog(&readonly), before);
}

#[test]
fn upgrade_keeps_preexisting_live_index_btrees_without_rebuilding() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = Connection::open(dir.path().join("upgrade.db")).unwrap();
    historical_core(&mut conn);
    for &name in &INDEXES[..2] {
        conn.execute_batch(expected_definition(name)).unwrap();
    }
    conn.execute("INSERT INTO notes(id,namespace,kind,properties,created_at,updated_at) VALUES('held','local','commit',?1,1,1)", [r#"{"sha":"held"}"#]).unwrap();
    let live_before = catalog(&conn);
    assert_eq!(live_before.len(), 2);
    let refused = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&refused);
    conn.authorizer(Some(move |ctx: AuthContext<'_>| {
        let name = match ctx.action {
            AuthAction::CreateIndex { index_name, .. }
            | AuthAction::DropIndex { index_name, .. }
            | AuthAction::Reindex { index_name } => Some(index_name),
            _ => None,
        };
        if name.is_some_and(|name| INDEXES[..2].contains(&name)) {
            recorded.lock().unwrap().push(name.unwrap().to_string());
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
        "no live CREATE/DROP/REINDEX attempt"
    );
    assert_definitions(&conn);
    for row in live_before {
        assert!(
            catalog(&conn).contains(&row),
            "definition and rootpage retained"
        );
    }
}
