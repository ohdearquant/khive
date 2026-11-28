//! A V20 database fixture for the V21 attachment-cutover tests.
//!
//! Included by `#[path]` from `#[cfg(test)]` modules and integration tests only, so the
//! raw entity insert below never compiles into a shipped build.

/// Materialize the exact canonical V20 prefix without invoking V21.
///
/// Test-only upgrade fixture shared by coordinator and multi-backend boot
/// regressions; this is not an alternate production migration path.
pub(crate) fn create_v20_database_fixture(
    path: &std::path::Path,
    entity_type: &str,
    deleted_at: Option<i64>,
) -> (uuid::Uuid, khive_storage::ContentRef) {
    use khive_db::migrations::{ATTACHMENT_CUTOVER_VERSION, MIGRATIONS};

    let mut conn = rusqlite::Connection::open(path).expect("open V20 fixture");
    conn.execute_batch(
        "CREATE TABLE _schema_migrations (\
             version INTEGER PRIMARY KEY, \
             name TEXT NOT NULL, \
             applied_at INTEGER NOT NULL\
         ) STRICT;",
    )
    .expect("create migration ledger");
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version < ATTACHMENT_CUTOVER_VERSION)
    {
        let tx = conn.transaction().expect("begin fixture migration");
        tx.execute_batch(migration.up)
            .unwrap_or_else(|error| panic!("apply V{}: {error}", migration.version));
        tx.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![
                migration.version,
                migration.name,
                i64::from(migration.version)
            ],
        )
        .unwrap_or_else(|error| panic!("record V{}: {error}", migration.version));
        tx.commit().expect("commit fixture migration");
    }

    let id = uuid::Uuid::new_v4();
    let content_ref =
        khive_storage::ContentRef::from_hex("a".repeat(64)).expect("canonical fixture digest");
    conn.execute(
        "INSERT INTO entities (\
             id, namespace, kind, entity_type, name, tags, created_at, updated_at, \
             deleted_at, content_ref\
         ) VALUES (?1, 'local', 'artifact', ?2, 'legacy fixture', '[]', 1, 1, ?3, ?4)",
        rusqlite::params![
            id.to_string(),
            entity_type,
            deleted_at,
            content_ref.as_str()
        ],
    )
    .expect("insert legacy attachment-bearing entity");
    (id, content_ref)
}
