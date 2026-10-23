use super::*;

const NAME: &str = "pending_entity_indexes";

fn insert_entity(conn: &Connection) {
    conn.execute_batch("INSERT INTO entities(id, namespace, kind, name, created_at, updated_at) VALUES ('subject', 'local', 'concept', 'subject', 1, 1);").unwrap();
}

fn insert_pending(conn: &Connection) {
    conn.execute_batch("INSERT INTO pending_entity_indexes(entity_id, namespace, source_version, generation, first_pending_at, targets_json) VALUES ('subject', 'local', 1, 'old-generation', 10, '[]');").unwrap();
}

fn pending_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM pending_entity_indexes", [], |row| {
        row.get(0)
    })
    .unwrap()
}

#[test]
fn pending_index_migration_installs_schema_on_fresh_and_existing_databases() {
    let migration = MIGRATIONS
        .iter()
        .find(|migration| migration.name == NAME)
        .unwrap();
    assert_eq!(migration.version, 63);
    let mut fresh = open_memory();
    run_migrations(&mut fresh).unwrap();
    assert!(table_exists(&fresh, "pending_entity_indexes"));

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
    insert_entity(&existing);
    assert!(!table_exists(&existing, "pending_entity_indexes"));
    run_migrations(&mut existing).unwrap();
    insert_pending(&existing);
    run_migrations(&mut existing).unwrap();
    assert_eq!(pending_count(&existing), 1);
    assert_eq!(
        existing
            .query_row("SELECT name FROM entities WHERE id='subject'", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
        "subject"
    );
}

#[test]
fn hard_delete_cleans_pending_with_foreign_keys_off_and_recreated_id_inherits_nothing() {
    for foreign_keys in [false, true] {
        let mut conn = open_memory();
        run_migrations(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", foreign_keys)
            .unwrap();
        conn.pragma_update(None, "recursive_triggers", false)
            .unwrap();
        insert_entity(&conn);
        insert_pending(&conn);
        conn.execute_batch("BEGIN IMMEDIATE; DELETE FROM entities WHERE id='subject';")
            .unwrap();
        assert_eq!(pending_count(&conn), 0);
        conn.execute_batch("ROLLBACK;").unwrap();
        assert_eq!(
            pending_count(&conn),
            1,
            "cleanup and entity deletion roll back together"
        );
        conn.execute_batch("DELETE FROM entities WHERE id='subject';")
            .unwrap();
        assert_eq!(pending_count(&conn), 0);
        insert_entity(&conn);
        assert_eq!(
            pending_count(&conn),
            0,
            "a recreated id inherits no generation or retry state"
        );
    }
}

#[test]
fn pending_index_entity_key_is_unique_across_namespaces() {
    let mut conn = open_memory();
    run_migrations(&mut conn).unwrap();
    insert_entity(&conn);
    insert_pending(&conn);
    assert!(conn.execute_batch("INSERT INTO pending_entity_indexes(entity_id, namespace, source_version, generation, first_pending_at, targets_json) VALUES ('subject', 'other', 1, 'second-generation', 20, '[]');").is_err());
    assert_eq!(pending_count(&conn), 1);
}
