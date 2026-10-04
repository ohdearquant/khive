use super::*;

fn single_target_section(parent_namespace: &str) {
    crate::extension::ensure_extensions_loaded();
    let mut conn = migrated();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    conn.execute(
        "INSERT INTO knowledge_atoms (id, namespace, slug, name, created_at, updated_at) \
         VALUES (?1, ?2, 'outside-parent', 'outside parent', 1, 1)",
        rusqlite::params![ATOM, parent_namespace],
    )
    .unwrap();
    section(&conn, SECTION, ATOM, "source");
    create_vectors(&conn, "vec_partition_model");
    vector(
        &conn,
        "vec_partition_model",
        SECTION,
        "source",
        "entity",
        "knowledge.section",
    );
    let before = places_and_bytes(&conn);
    let request = MoveRequest::new("source", vec![route("atom", "target")]);
    let transaction = conn.transaction().unwrap();
    let counts = move_namespace(&transaction, &request).unwrap();
    assert_eq!(counts.subjects.get("atom"), Some(&0));
    assert_eq!(counts.rows.get("knowledge_atoms"), Some(&0));
    assert_eq!(counts.rows.get("knowledge_sections"), Some(&1));
    assert_eq!(counts.rows.get("vec_partition_model"), Some(&1));
    assert_eq!(counts.ann_log_appended, 2);
    let section_namespace: String = transaction
        .query_row(
            "SELECT namespace FROM knowledge_sections WHERE id = ?1",
            [SECTION],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(section_namespace, "target");
    let atom_namespace: String = transaction
        .query_row(
            "SELECT namespace FROM knowledge_atoms WHERE id = ?1",
            [ATOM],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(atom_namespace, parent_namespace);
    assert_eq!(
        places_and_bytes(&transaction),
        [(SECTION.into(), "target".into(), before[0].2.clone())]
    );
    let log: Vec<(String, String)> = transaction
        .prepare("SELECT namespace, op FROM ann_write_log WHERE subject_id = ?1 ORDER BY seq")
        .unwrap()
        .query_map([SECTION], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        log,
        [
            ("source".into(), "delete".into()),
            ("target".into(), "upsert".into())
        ]
    );
    let again = move_namespace(&transaction, &request).unwrap();
    assert_eq!(again.rows.get("knowledge_sections"), Some(&0));
    assert_eq!(again.rows.get("vec_partition_model"), Some(&0));
    assert_eq!(again.ann_log_appended, 0);
    transaction.commit().unwrap();
}

#[test]
fn single_target_carries_a_source_section_whose_parent_is_in_the_target() {
    single_target_section("target");
}

#[test]
fn single_target_carries_a_source_section_whose_parent_is_in_a_third_namespace() {
    single_target_section("third");
}

#[test]
fn domain_first_keeps_ordinary_atoms_sections_and_vectors_on_the_atom_route() {
    crate::extension::ensure_extensions_loaded();
    let mut conn = migrated();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    conn.execute(
        "INSERT INTO knowledge_atoms (id, namespace, slug, name, created_at, updated_at) \
         VALUES (?1, 'source', 'ordinary', 'ordinary atom', 1, 1)",
        [ATOM],
    )
    .unwrap();
    section(&conn, SECTION, ATOM, "source");
    domain_pair(&conn, DOMAIN, "source");
    const DOMAIN_SECTION: &str = "66666666-6666-4666-8666-000000000002";
    section(&conn, DOMAIN_SECTION, DOMAIN, "source");
    create_vectors(&conn, "vec_partition_model");
    for (id, field) in [
        (ATOM, "knowledge.atom"),
        (SECTION, "knowledge.section"),
        (DOMAIN, "knowledge.atom"),
        (DOMAIN_SECTION, "knowledge.section"),
    ] {
        vector(&conn, "vec_partition_model", id, "source", "entity", field);
    }
    let before = places_and_bytes(&conn);
    let request = MoveRequest::new(
        "source",
        vec![
            route("domain", "domain-target"),
            route("atom", "atom-target"),
        ],
    );
    let transaction = conn.transaction().unwrap();
    let counts = move_namespace(&transaction, &request).unwrap();
    assert_eq!(counts.subjects.get("atom"), Some(&1));
    assert_eq!(counts.subjects.get("domain"), Some(&1));
    assert_eq!(counts.rows.get("knowledge_atoms"), Some(&2));
    assert_eq!(counts.rows.get("knowledge_domains"), Some(&1));
    assert_eq!(counts.rows.get("knowledge_sections"), Some(&2));
    assert_eq!(counts.rows.get("vec_partition_model"), Some(&4));
    assert_eq!(counts.ann_log_appended, 8);
    for (table, id, target) in [
        ("knowledge_atoms", ATOM, "atom-target"),
        ("knowledge_sections", SECTION, "atom-target"),
        ("knowledge_domains", DOMAIN, "domain-target"),
        ("knowledge_atoms", DOMAIN, "domain-target"),
        ("knowledge_sections", DOMAIN_SECTION, "domain-target"),
    ] {
        let namespace: String = transaction
            .query_row(
                &format!("SELECT namespace FROM {table} WHERE id = ?1"),
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(namespace, target, "physical route for {table}/{id}");
    }
    let after = places_and_bytes(&transaction);
    assert_eq!(after.len(), before.len());
    for ((id, _, bytes), (moved_id, namespace, moved_bytes)) in before.iter().zip(&after) {
        let target = if [ATOM, SECTION].contains(&id.as_str()) {
            "atom-target"
        } else {
            "domain-target"
        };
        assert_eq!(id, moved_id);
        assert_eq!(namespace, target);
        assert_eq!(bytes, moved_bytes);
        let log: Vec<(String, String)> = transaction
            .prepare("SELECT namespace, op FROM ann_write_log WHERE subject_id = ?1 ORDER BY seq")
            .unwrap()
            .query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            log,
            [
                ("source".into(), "delete".into()),
                (target.into(), "upsert".into())
            ]
        );
    }
    transaction.commit().unwrap();
}
