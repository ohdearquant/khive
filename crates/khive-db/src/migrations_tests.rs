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
    assert_eq!(version, 3, "latest migration version");

    let recorded: i64 = conn
        .query_row("SELECT COUNT(*) FROM _schema_migrations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(recorded, 3);
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
    assert_eq!(recorded, 3, "no duplicate migration rows on re-run");
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
