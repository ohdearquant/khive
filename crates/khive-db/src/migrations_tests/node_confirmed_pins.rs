use super::*;

const NAME: &str = "node_confirmed_pins";
const TABLE: &str = "comm_node_confirmed_pin";

fn insert(
    conn: &Connection,
    epoch: i64,
    kem: u32,
    signing: u32,
    fingerprint: u32,
) -> rusqlite::Result<usize> {
    conn.execute(
        &format!(
            "INSERT INTO comm_node_confirmed_pin(namespace, slug, realm, agent_id, device_id, \
             key_epoch, kem_public_key, signing_public_key, fingerprint) VALUES ('local', 'peer', \
             'example', '00000000-0000-0000-0000-000000000007', \
             '00000000-0000-0000-0000-000000000001', {epoch}, zeroblob({kem}), \
             zeroblob({signing}), zeroblob({fingerprint}))"
        ),
        [],
    )
}

fn is_constraint(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

fn pin_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM comm_node_confirmed_pin", [], |row| {
        row.get(0)
    })
    .unwrap()
}

#[test]
fn confirmed_pin_migration_installs_on_fresh_and_v63_databases() {
    let migration = MIGRATIONS
        .iter()
        .find(|migration| migration.name == NAME)
        .unwrap();
    assert_eq!(migration.version, 64);
    let mut fresh = open_memory();
    run_migrations(&mut fresh).unwrap();
    assert!(table_exists(&fresh, TABLE));

    let mut existing = open_memory();
    let admission = WriteAdmission::for_migration_policy(None, &migration_test_policy()).unwrap();
    {
        let mut writes = RawMigrationTransactions::new(&mut existing, &admission);
        writes
            .admitted(|conn| bootstrap_migration_ledger(conn, &admission))
            .unwrap();
        for prior in MIGRATIONS
            .iter()
            .take_while(|migration| migration.name != NAME)
        {
            writes
                .admitted(|conn| {
                    apply_versioned_migration(conn, prior, &admission, latest_schema_version())
                })
                .unwrap();
        }
    }
    assert!(!table_exists(&existing, TABLE));
    run_migrations(&mut existing).unwrap();
    assert!(table_exists(&existing, TABLE));
    insert(&existing, 1, 32, 32, 32).unwrap();
    run_migrations(&mut existing).unwrap();
    assert_eq!(pin_count(&existing), 1);
}

#[test]
fn confirmed_pin_checks_refuse_short_keys_and_out_of_range_epochs() {
    let mut conn = open_memory();
    run_migrations(&mut conn).unwrap();
    insert(&conn, 1, 32, 32, 32).unwrap();
    insert(&conn, 4_294_967_295, 32, 32, 32).unwrap();
    for (epoch, kem, signing, fingerprint) in [
        (2, 31, 32, 32),
        (2, 33, 32, 32),
        (2, 32, 31, 32),
        (2, 32, 32, 31),
        (0, 32, 32, 32),
        (-1, 32, 32, 32),
        (4_294_967_296, 32, 32, 32),
    ] {
        let error = insert(&conn, epoch, kem, signing, fingerprint).unwrap_err();
        assert!(
            is_constraint(&error),
            "epoch {epoch} lengths {kem}/{signing}/{fingerprint}: {error:?}"
        );
    }
    assert_eq!(pin_count(&conn), 2);
}

#[test]
fn confirmed_pin_identity_cannot_be_inserted_twice() {
    let mut conn = open_memory();
    run_migrations(&mut conn).unwrap();
    insert(&conn, 1, 32, 32, 32).unwrap();
    let error = insert(&conn, 1, 32, 32, 32).unwrap_err();
    assert!(is_constraint(&error), "{error:?}");
    assert_eq!(pin_count(&conn), 1);
}
