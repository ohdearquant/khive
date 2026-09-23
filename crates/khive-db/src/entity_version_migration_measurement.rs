//! Opt-in synthetic measurement of the V36-to-V37 entity-version migration.
//!
//! The two JSON records measure only `run_migrations`; database construction,
//! seeding, checkpointing, cache warming, and result assertions are not timed.

use std::path::Path;
use std::time::Instant;

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use super::{
    attachment_cutover_status, finalize_attachment_cutover, latest_schema_version,
    read_schema_version, run_migrations, stage_attachment_cutover, AttachmentCutoverStatus,
    ATTACHMENT_CUTOVER_VERSION, MIGRATIONS, MIGRATION_TRACKING_TABLE,
};
use crate::stores::blob::try_acquire_database_gc_owner_for_path;

const BASELINE_VERSION: u32 = 36;
const TARGET_VERSION: u32 = 37;
const MAX_ROWS: usize = 1_000_000;

fn requested_rows() -> usize {
    let raw = std::env::var("KHIVE_ENTITY_VERSION_ROWS")
        .expect("set KHIVE_ENTITY_VERSION_ROWS to an integer in 1..=1000000");
    assert!(
        !raw.is_empty() && raw.bytes().all(|byte| byte.is_ascii_digit()),
        "KHIVE_ENTITY_VERSION_ROWS must contain only decimal digits, without whitespace"
    );
    let rows = raw
        .parse::<usize>()
        .expect("KHIVE_ENTITY_VERSION_ROWS is outside the supported integer range");
    assert!(
        (1..=MAX_ROWS).contains(&rows),
        "KHIVE_ENTITY_VERSION_ROWS must be in 1..=1000000; the empty case is measured separately"
    );
    rows
}

fn create_v36(conn: &mut Connection, database_path: &Path) {
    conn.execute_batch(MIGRATION_TRACKING_TABLE)
        .expect("create migration ledger");
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= BASELINE_VERSION)
    {
        if migration.version == ATTACHMENT_CUTOVER_VERSION {
            // V21 includes application-assisted finalization. Its SQL alone
            // does not remove the legacy column or complete the cutover ledger.
            let canonical_path = database_path
                .canonicalize()
                .expect("canonical database path");
            let _owner = try_acquire_database_gc_owner_for_path(canonical_path)
                .expect("acquire V21 database GC owner");
            stage_attachment_cutover(conn).expect("stage historical V21 cutover");
            finalize_attachment_cutover(conn).expect("finalize historical V21 cutover");
            assert_eq!(
                attachment_cutover_status(conn).expect("read V21 cutover status"),
                AttachmentCutoverStatus::Complete
            );
        } else {
            let tx = conn.transaction().expect("begin historical migration");
            tx.execute_batch(migration.up)
                .expect("apply historical migration body");
            tx.execute(
                "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, 0)",
                params![migration.version, migration.name],
            )
            .expect("record historical migration");
            tx.commit().expect("commit historical migration");
        }
    }
    assert_eq!(
        read_schema_version(conn).expect("read baseline schema version"),
        BASELINE_VERSION
    );
    let version_columns: u32 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('entities') WHERE name = 'version'",
            [],
            |row| row.get(0),
        )
        .expect("inspect baseline entity columns");
    assert_eq!(version_columns, 0, "V36 must not already contain V37 DDL");
}

fn seed_entities(conn: &mut Connection, rows: usize) {
    let tx = conn.transaction().expect("begin entity seeding");
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO entities \
                 (id, namespace, kind, name, description, properties, tags, created_at, updated_at) \
                 VALUES (?1, 'measurement', 'concept', ?2, 'Synthetic migration measurement entity', \
                         '{\"synthetic\":true}', '[\"synthetic\"]', 1, 1)",
            )
            .expect("prepare entity seeding");
        for index in 0..rows {
            insert
                .execute(params![
                    format!("00000000-0000-4000-8000-{index:012x}"),
                    format!("Synthetic entity {index}")
                ])
                .expect("seed V36 entity");
        }
    }
    tx.commit().expect("commit entity seeding");
}

fn measure_case(rows: usize) -> Value {
    let temp = tempfile::tempdir().expect("create isolated measurement directory");
    let database_path = temp.path().join("entity-version.sqlite3");
    let mut conn = Connection::open(&database_path).expect("open measurement database");
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
        .expect("configure WAL/NORMAL");
    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("read journal mode");
    let synchronous: u32 = conn
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .expect("read synchronous mode");
    assert_eq!(journal_mode, "wal");
    assert_eq!(synchronous, 1, "measurement requires synchronous=NORMAL");

    create_v36(&mut conn, &database_path);
    seed_entities(&mut conn, rows);

    // Materialize all seed pages into the database file and reset the WAL so
    // its post-migration size excludes fixture setup. Both are outside timing.
    let checkpoint: (i64, i64, i64) = conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .expect("checkpoint seeded V36 database");
    assert_eq!(checkpoint, (0, 0, 0), "seed checkpoint must finish");

    // Read entity payloads after seeding on the same connection. This is a
    // warm-cache pass, not a claim that every page fits in SQLite's page cache.
    let (seeded_rows, _payload_bytes): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(length(id) + length(namespace) + length(kind) + \
             length(name) + length(description) + length(properties) + length(tags)), 0) \
             FROM entities",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("warm seeded entity payloads");
    let seeded_rows = usize::try_from(seeded_rows).expect("seeded row count fits usize");
    assert_eq!(seeded_rows, rows);

    let sqlite_version: String = conn
        .query_row("SELECT sqlite_version()", [], |row| row.get(0))
        .expect("read linked SQLite version");
    let page_size: i64 = conn
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .expect("read SQLite page size");
    let page_size = u64::try_from(page_size).expect("SQLite page size is nonnegative");
    let db_size_before = std::fs::metadata(&database_path)
        .expect("read seeded database size")
        .len();

    let started = Instant::now();
    let migration_result = run_migrations(&mut conn);
    let elapsed_us = u64::try_from(started.elapsed().as_micros()).expect("elapsed time fits u64");

    let schema_version = migration_result.expect("migrate V36 to V37");
    assert_eq!(schema_version, TARGET_VERSION);
    let mut wal_path = database_path.as_os_str().to_os_string();
    wal_path.push("-wal");
    let wal_bytes_after = std::fs::metadata(Path::new(&wal_path))
        .expect("read WAL size before closing migrated connection")
        .len();

    let (migrated_rows, invalid_versions): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(CASE \
             WHEN typeof(version) = 'integer' AND version = 1 THEN 0 ELSE 1 END), 0) \
             FROM entities",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("inspect every migrated entity version");
    let migrated_rows = usize::try_from(migrated_rows).expect("migrated row count fits usize");
    let invalid_versions =
        usize::try_from(invalid_versions).expect("invalid version count fits usize");
    assert_eq!(migrated_rows, rows, "migration must preserve all entities");
    assert_eq!(
        invalid_versions, 0,
        "every existing entity must be version 1"
    );
    assert_eq!(
        read_schema_version(&conn).expect("read migrated schema ledger"),
        TARGET_VERSION
    );

    json!({
        "label": "synthetic_v36_to_v37",
        "rows": rows,
        "elapsed_us": elapsed_us,
        "sqlite_version": sqlite_version,
        "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "page_size": page_size,
        "db_size_before": db_size_before,
        "wal_bytes_after": wal_bytes_after,
        "schema_version": schema_version,
    })
}

#[test]
#[ignore = "opt-in migration measurement; set KHIVE_ENTITY_VERSION_ROWS=1..1000000"]
fn entity_version_migration_measurement() {
    let rows = requested_rows();
    assert_eq!(
        latest_schema_version(),
        TARGET_VERSION,
        "this fixture measures exactly V36 to V37; use its pinned source revision"
    );
    let records = [measure_case(0), measure_case(rows)];
    for record in records {
        println!("{record}");
    }
}
