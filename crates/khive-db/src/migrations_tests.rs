use super::query_embedding_models_conn;
use super::*;

fn open_memory() -> Connection {
    Connection::open_in_memory().expect("in-memory connection")
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
        rusqlite::params![name],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info(?1) WHERE name = ?2",
        rusqlite::params![table, column],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

#[test]
fn fresh_db_migrates_to_latest() {
    let mut conn = open_memory();
    let version = run_migrations(&mut conn).expect("migrations should succeed");
    let latest = MIGRATIONS.last().expect("at least one migration").version;
    assert_eq!(
        version, latest,
        "run_migrations must reach the latest version"
    );

    let recorded: i64 = conn
        .query_row("SELECT COUNT(*) FROM _schema_migrations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        recorded,
        MIGRATIONS.len() as i64,
        "ledger row count must equal the number of migrations"
    );
}

#[test]
fn v4_creates_consolidated_fts_tables() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations should succeed");
    assert!(
        table_exists(&conn, "fts_entities"),
        "V4 must create fts_entities"
    );
    assert!(table_exists(&conn, "fts_notes"), "V4 must create fts_notes");
}

#[test]
fn rejects_pre_consolidation_ledger() {
    let mut conn = open_memory();
    // Simulate a database carrying the old, pre-consolidation V1..V22 ledger.
    conn.execute_batch(MIGRATION_TRACKING_TABLE).unwrap();
    conn.execute(
        "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (22, 'legacy', 0)",
        [],
    )
    .unwrap();

    let err = run_migrations(&mut conn).expect_err("must reject a version ahead of latest");
    match err {
        SqliteError::InvalidData(msg) => assert!(
            msg.contains("ahead of the latest known migration"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected InvalidData, got {other:?}"),
    }
}

#[test]
fn core_tables_exist() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    for t in [
        "entities",
        "graph_edges",
        "notes",
        "events",
        "event_observations",
        "_embedding_models",
        "proposals_open",
        "brain_profile_snapshots",
        "brain_event_log",
        "knowledge_atoms",
        "knowledge_domains",
        "knowledge_sections",
    ] {
        assert!(table_exists(&conn, t), "missing table: {t}");
    }
}

#[test]
fn knowledge_atoms_has_content_not_description() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    assert!(
        column_exists(&conn, "knowledge_atoms", "content"),
        "knowledge_atoms must have a content column"
    );
    assert!(
        !column_exists(&conn, "knowledge_atoms", "description"),
        "knowledge_atoms must NOT have a description column"
    );
}

#[test]
fn knowledge_sections_has_content_hash() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    assert!(column_exists(&conn, "knowledge_sections", "content_hash"));
}

#[test]
fn knowledge_sections_unique_on_atom_and_content_hash() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    let now = chrono::Utc::now().timestamp_micros();
    conn.execute(
        "INSERT INTO knowledge_atoms (id, namespace, slug, name, content, created_at, updated_at) \
         VALUES ('a1', 'default', 'slug-1', 'Atom', 'body text here', ?1, ?1)",
        rusqlite::params![now],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO knowledge_sections (id, atom_id, namespace, section_type, content, content_hash, created_at, updated_at) \
         VALUES ('s1', 'a1', 'default', 'other', 'X', 'hash-abc', ?1, ?1)",
        rusqlite::params![now],
    )
    .unwrap();
    // Same (atom_id, content_hash) must be rejected.
    let dup = conn.execute(
        "INSERT INTO knowledge_sections (id, atom_id, namespace, section_type, content, content_hash, created_at, updated_at) \
         VALUES ('s2', 'a1', 'default', 'overview', 'Y', 'hash-abc', ?1, ?1)",
        rusqlite::params![now],
    );
    assert!(dup.is_err(), "duplicate (atom_id, content_hash) must fail");
}

#[test]
fn run_migrations_twice_is_idempotent() {
    let mut conn = open_memory();
    let v1 = run_migrations(&mut conn).expect("first run");
    let v2 = run_migrations(&mut conn).expect("second run");
    assert_eq!(v1, v2);
    let recorded: i64 = conn
        .query_row("SELECT COUNT(*) FROM _schema_migrations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        recorded,
        MIGRATIONS.len() as i64,
        "no duplicate migration rows on re-run"
    );
}

// ── V5: external_id unique index tests ──────────────────────────────────────

fn index_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='index' AND name=?1",
        rusqlite::params![name],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

fn index_is_unique(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT \"unique\" FROM pragma_index_list('notes') WHERE name=?1",
        rusqlite::params![name],
        |row| {
            let v: i64 = row.get(0)?;
            Ok(v != 0)
        },
    )
    .unwrap_or(false)
}

#[test]
fn v5_creates_unique_external_id_index() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations should succeed");
    assert!(
        index_exists(&conn, "idx_comm_message_external_id"),
        "V5 must create idx_comm_message_external_id"
    );
    assert!(
        index_is_unique(&conn, "idx_comm_message_external_id"),
        "idx_comm_message_external_id must be UNIQUE"
    );
}

#[test]
fn v5_duplicate_external_id_insert_rejected() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations should succeed");
    let now = chrono::Utc::now().timestamp_micros();
    // Insert a note with external_id
    conn.execute(
        "INSERT INTO notes (id, namespace, kind, status, content, properties, created_at, updated_at) \
         VALUES ('id-ext-1', 'local', 'message', 'active', 'body', \
                 json_object('external_id', 'imap:host:1:1'), ?1, ?1)",
        rusqlite::params![now],
    )
    .expect("first insert");
    // A second note with the same external_id must be rejected by the unique index.
    let dup = conn.execute(
        "INSERT INTO notes (id, namespace, kind, status, content, properties, created_at, updated_at) \
         VALUES ('id-ext-2', 'local', 'message', 'active', 'body2', \
                 json_object('external_id', 'imap:host:1:1'), ?1, ?1)",
        rusqlite::params![now],
    );
    assert!(dup.is_err(), "duplicate external_id must be rejected");
}

#[test]
fn v5_upgrade_from_duplicate_rows_succeeds() {
    // Simulate a V4-state database that already contains duplicate external_id rows.
    // Apply only migrations up to V4, insert duplicates, then run V5 and verify:
    //   - V5 migration completes without error
    //   - The canonical (earliest) row keeps its external_id
    //   - Later duplicate rows survive with external_id cleared to NULL
    let mut conn = open_memory();

    // Apply V1..V4 only.
    conn.execute_batch(MIGRATION_TRACKING_TABLE).unwrap();
    let now = chrono::Utc::now().timestamp_micros();
    for migration in MIGRATIONS.iter().filter(|m| m.version <= 4) {
        let tx = conn.transaction().unwrap();
        tx.execute_batch(migration.up).unwrap();
        tx.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![migration.version, migration.name, now],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    // Insert two notes sharing the same external_id (canonical + duplicate).
    conn.execute(
        "INSERT INTO notes (id, namespace, kind, status, content, properties, created_at, updated_at) \
         VALUES ('canonical-row', 'local', 'message', 'active', 'first', \
                 json_object('external_id', 'imap:h:9:9'), ?1, ?1)",
        rusqlite::params![now],
    )
    .expect("canonical row");
    conn.execute(
        "INSERT INTO notes (id, namespace, kind, status, content, properties, created_at, updated_at) \
         VALUES ('dup-row', 'local', 'message', 'active', 'second', \
                 json_object('external_id', 'imap:h:9:9'), ?1, ?1)",
        rusqlite::params![now],
    )
    .expect("duplicate row (allowed before V5 unique index)");

    // Now run V5.
    let tx = conn.transaction().unwrap();
    let v5 = MIGRATIONS.iter().find(|m| m.version == 5).unwrap();
    tx.execute_batch(v5.up)
        .expect("V5 migration must succeed on a DB with duplicate external_ids");
    tx.execute(
        "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![v5.version, v5.name, now],
    )
    .unwrap();
    tx.commit().unwrap();

    // V5 must have created the unique index.
    assert!(
        index_exists(&conn, "idx_comm_message_external_id"),
        "V5 must create idx_comm_message_external_id"
    );
    assert!(
        index_is_unique(&conn, "idx_comm_message_external_id"),
        "idx_comm_message_external_id must be UNIQUE after V5 upgrade"
    );

    // Canonical row keeps its external_id.
    let canonical_ext: Option<String> = conn
        .query_row(
            "SELECT json_extract(properties, '$.external_id') FROM notes WHERE id='canonical-row'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        canonical_ext.as_deref(),
        Some("imap:h:9:9"),
        "canonical row must retain its external_id"
    );

    // Duplicate row survives but with external_id cleared.
    let dup_content: String = conn
        .query_row("SELECT content FROM notes WHERE id='dup-row'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        dup_content, "second",
        "duplicate row must survive (not deleted)"
    );

    let dup_ext: Option<String> = conn
        .query_row(
            "SELECT json_extract(properties, '$.external_id') FROM notes WHERE id='dup-row'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        dup_ext.is_none(),
        "duplicate row must have external_id cleared (got {:?})",
        dup_ext
    );
}

// ── _embedding_models.dim u32 range tests ───────────────────────────────────

/// Helper: open a migrated in-memory DB and insert a row into `_embedding_models`
/// with the given raw `dim` value (stored as i64 to exercise negative/overflow cases).
fn insert_model_with_dim(conn: &Connection, dim: i64) {
    // id and canonical_key are BLOBs; use distinct values per dim to avoid UNIQUE conflicts.
    let id = dim.to_be_bytes();
    let canonical_key = [(dim % 127) as u8; 8];
    let now = 0i64;
    conn.execute(
        "INSERT INTO _embedding_models \
         (id, engine_name, model_id, key_version, dim, status, canonical_key, created_at) \
         VALUES (?1, 'engine', 'model', 'engine/model', ?2, 'active', ?3, ?4)",
        rusqlite::params![id.as_slice(), dim, canonical_key.as_slice(), now],
    )
    .expect("insert model");
}

/// dim = -1 must be rejected: would silently become u32::MAX via `as u32`.
#[test]
fn embedding_model_dim_negative_is_rejected() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    insert_model_with_dim(&conn, -1);
    let result = query_embedding_models_conn(&conn, None);
    assert!(
        result.is_err(),
        "dim = -1 must be rejected; got: {:?}",
        result
    );
}

/// dim = u32::MAX + 1 must be rejected: would silently truncate to 0 via `as u32`.
#[test]
fn embedding_model_dim_u32_max_plus_one_is_rejected() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    insert_model_with_dim(&conn, i64::from(u32::MAX) + 1);
    let result = query_embedding_models_conn(&conn, None);
    assert!(
        result.is_err(),
        "dim = u32::MAX + 1 must be rejected; got: {:?}",
        result
    );
}

/// dim = u32::MAX (4 294 967 295) is a legal u32 value and must be accepted.
#[test]
fn embedding_model_dim_u32_max_is_accepted() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    insert_model_with_dim(&conn, i64::from(u32::MAX));
    let result = query_embedding_models_conn(&conn, None);
    assert!(
        result.is_ok(),
        "dim = u32::MAX must be accepted; got: {:?}",
        result
    );
    let records = result.unwrap();
    assert_eq!(records[0].dimensions, u32::MAX);
}
