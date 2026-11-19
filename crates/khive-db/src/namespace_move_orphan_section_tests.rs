use super::{move_namespace, MoveRequest, MoveRoute, SubjectClass};
use crate::migrations::run_migrations;
use rusqlite::Connection;

fn fixture(parent_namespace: Option<&str>) -> Connection {
    crate::extension::ensure_extensions_loaded();
    let mut conn = Connection::open_in_memory().unwrap();
    run_migrations(&mut conn).unwrap();
    // Historical orphan sections require the original missing-parent state.
    conn.pragma_update(None, "foreign_keys", parent_namespace.is_some())
        .unwrap();
    if let Some(namespace) = parent_namespace {
        conn.execute(
            "INSERT INTO knowledge_atoms \
             (id, namespace, slug, name, created_at, updated_at) \
             VALUES ('parent', ?1, 'parent', 'parent', 1, 1)",
            [namespace],
        )
        .unwrap();
    }
    conn.execute_batch(
        "INSERT INTO knowledge_sections \
         (id, atom_id, namespace, section_type, heading, content, content_hash, \
          created_at, updated_at) \
         VALUES ('section', 'parent', 'source', 'body', 'section heading', \
                 'section searchable content', 'section-hash', 1, 1); \
         CREATE VIRTUAL TABLE vec_section_model USING vec0(\
             subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, \
             kind TEXT NOT NULL, field TEXT NOT NULL, embedding_model TEXT NOT NULL, \
             embedding float[4] distance_metric=cosine\
         ); \
         INSERT INTO vec_section_model \
         (subject_id, namespace, kind, field, embedding_model, embedding) \
         VALUES ('section', 'source', 'entity', 'knowledge.section', \
                 'section-model', '[1.0, 0.0, 0.0, 0.0]');",
    )
    .unwrap();
    conn
}

fn seed_note(conn: &Connection) {
    conn.execute_batch(
        "INSERT INTO notes (id, namespace, kind, name, content, created_at, updated_at) \
         VALUES ('note', 'source', 'observation', 'note', 'note content', 1, 1);",
    )
    .unwrap();
}

fn seed_domain(conn: &Connection) {
    conn.execute_batch(
        "INSERT INTO knowledge_domains \
         (id, namespace, slug, name, created_at, updated_at) \
         VALUES ('domain', 'source', 'domain', 'domain', 1, 1);",
    )
    .unwrap();
}

fn assert_section_and_vector_move(conn: &mut Connection, keys: &[&str]) {
    let routes = keys
        .iter()
        .map(|key| MoveRoute {
            class: SubjectClass::parse(key).unwrap(),
            target: "target".into(),
        })
        .collect();
    let request = MoveRequest::new("source", routes);
    let tx = conn.transaction().unwrap();
    let counts = move_namespace(&tx, &request).unwrap();
    tx.commit().unwrap();
    let namespaces: Vec<String> = [
        "SELECT namespace FROM knowledge_sections WHERE id = 'section'",
        "SELECT namespace FROM vec_section_model WHERE subject_id = 'section'",
    ]
    .iter()
    .map(|sql| conn.query_row(sql, [], |row| row.get(0)).unwrap())
    .collect();
    assert_eq!(
        namespaces,
        ["target", "target"],
        "section and vector must move together"
    );
    assert_eq!(counts.rows.get("knowledge_sections"), Some(&1));
    assert_eq!(counts.rows.get("vec_section_model"), Some(&1));
    assert_eq!(counts.ann_log_appended, 2);
    let source_sections: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM knowledge_sections WHERE namespace = 'source'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(source_sections, 0);
    let source_vectors: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM vec_section_model WHERE namespace = 'source'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(source_vectors, 0);
}

#[test]
fn a_note_only_move_carries_sections_whose_parent_is_already_in_the_target() {
    let mut conn = fixture(Some("target"));
    seed_note(&conn);
    assert_section_and_vector_move(&mut conn, &["note:observation"]);
    let parent_namespace: String = conn
        .query_row(
            "SELECT namespace FROM knowledge_atoms WHERE id = 'parent'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(parent_namespace, "target");
}

#[test]
fn a_domain_only_move_keeps_an_orphan_section_with_its_vector() {
    let mut conn = fixture(None);
    seed_domain(&conn);
    assert_section_and_vector_move(&mut conn, &["domain"]);
}

#[test]
fn several_routes_to_one_target_keep_an_orphan_section_with_its_vector() {
    let mut conn = fixture(None);
    seed_note(&conn);
    seed_domain(&conn);
    assert_section_and_vector_move(&mut conn, &["note:observation", "domain"]);
}
