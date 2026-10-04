use super::tests::{migrated, route, seed_note};
use super::*;
use crate::namespace_move_fixture::{self as fixture, *};

const DOMAIN: &str = "66666666-6666-4666-8666-000000000001";
const RESIDENT: &str = "77777777-7777-4777-8777-000000000001";
const MEMORY: &str = "88888888-8888-4888-8888-000000000001";
const SECOND_MEMORY: &str = "99999999-9999-4999-8999-000000000001";

fn create_vectors(conn: &Connection, table: &str) {
    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE {table} USING vec0(\
         subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL, \
         field TEXT NOT NULL, embedding_model TEXT NOT NULL, \
         embedding float[2] distance_metric=cosine)"
    ))
    .unwrap();
}

fn vector(conn: &Connection, table: &str, id: &str, namespace: &str, kind: &str, field: &str) {
    conn.execute(
        &format!(
            "INSERT INTO {table} (subject_id, namespace, kind, field, embedding_model, embedding) \
             VALUES (?1, ?2, ?3, ?4, ?5, '[0.1, 0.2]')"
        ),
        rusqlite::params![id, namespace, kind, field, table],
    )
    .unwrap();
}

fn prepared() -> (Connection, MoveRequest) {
    crate::extension::ensure_extensions_loaded();
    let conn = migrated();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    let mut spec = FixtureSpec::movable("source");
    spec.kg = "target-a".into();
    spec.gtd = "target-b".into();
    spec.knowledge = "target-b".into();
    fixture::build(&conn, &spec).unwrap();
    conn.execute(
        "INSERT INTO knowledge_domains (id, namespace, slug, name, created_at, updated_at) \
         VALUES (?1, 'source', 'domain', 'domain', 1, 1)",
        [DOMAIN],
    )
    .unwrap();
    seed_note(&conn, RESIDENT, "target-a", "observation");
    create_vectors(&conn, "vec_partition_model");
    for (id, kind, field) in [
        (NOTE_OBSERVATION, "note", "note.content"),
        (NOTE_TASK, "note", "note.content"),
        (NOTE_DELETED, "note", "note.content"),
        (ENTITY, "entity", "entity.name"),
        (EDGE, "entity", "edge.identity"),
        (ATOM, "entity", "knowledge.atom"),
        (SECTION, "entity", "knowledge.section"),
        (DOMAIN, "entity", "knowledge.domain"),
    ] {
        vector(&conn, "vec_partition_model", id, "source", kind, field);
    }
    vector(
        &conn,
        "vec_partition_model",
        RESIDENT,
        "target-a",
        "note",
        "note.content",
    );
    for (id, namespace, bytes) in places_and_bytes(&conn) {
        conn.execute(
            "INSERT INTO vector_provenance (model_key, subject_id, namespace, embedding_digest) \
             VALUES ('partition_model', ?1, ?2, ?3)",
            rusqlite::params![id, namespace, blake3::hash(&bytes).to_hex().to_string()],
        )
        .unwrap();
    }
    let mut routes = fixture::routes(&spec)
        .into_iter()
        .map(|(class, target)| route(class, &target))
        .collect::<Vec<_>>();
    routes.push(route("domain", "target-a"));
    (conn, MoveRequest::new("source", routes))
}

fn places_and_bytes(conn: &Connection) -> Vec<(String, String, Vec<u8>)> {
    conn.prepare(
        "SELECT subject_id, namespace, embedding FROM vec_partition_model ORDER BY subject_id",
    )
    .unwrap()
    .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

#[test]
fn partitioned_vectors_follow_every_source_class_and_its_sections() {
    let (mut conn, request) = prepared();
    let before = places_and_bytes(&conn);
    let transaction = conn.transaction().unwrap();
    let counts = move_namespace(&transaction, &request).unwrap();
    assert_eq!(counts.rows.get("vec_partition_model"), Some(&8));
    assert_eq!(counts.ann_log_appended, 16);
    assert!(!counts.left_behind.contains_key("vec_partition_model"));
    let after = places_and_bytes(&transaction);
    assert_eq!(after.len(), before.len());
    for ((id, _, original), (after_id, namespace, moved)) in before.iter().zip(&after) {
        assert_eq!(after_id, id);
        assert_eq!(
            moved, original,
            "the stored embedding must remain byte-exact"
        );
        let target = if [NOTE_TASK, ATOM, SECTION].contains(&id.as_str()) {
            "target-b"
        } else {
            "target-a"
        };
        assert_eq!(namespace, target, "physical subject route for {id}");
        let log: Vec<(i64, String, String)> = transaction
            .prepare(
                "SELECT seq, namespace, op FROM ann_write_log WHERE subject_id = ?1 ORDER BY seq",
            )
            .unwrap()
            .query_map([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        if id == RESIDENT {
            assert!(
                log.is_empty(),
                "target residents are outside the exact staged set"
            );
        } else {
            assert_eq!(log.len(), 2);
            assert_eq!((log[0].1.as_str(), log[0].2.as_str()), ("source", "delete"));
            assert_eq!((log[1].1.as_str(), log[1].2.as_str()), (target, "upsert"));
            assert!(log[0].0 < log[1].0);
        }
    }
    let section_place: String = transaction
        .query_row(
            "SELECT namespace FROM knowledge_sections WHERE id = ?1",
            [SECTION],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(section_place, "target-b");
    let provenance: Vec<(String, String)> = transaction
        .prepare("SELECT subject_id, namespace FROM vector_provenance ORDER BY subject_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(provenance, [(RESIDENT.into(), "target-a".into())]);
    let rerun = move_namespace(&transaction, &request).unwrap();
    assert!(rerun.subjects.values().all(|count| *count == 0));
    assert_eq!(rerun.rows.get("vec_partition_model"), Some(&0));
    assert_eq!(rerun.ann_log_appended, 0);
    assert_eq!(places_and_bytes(&transaction), after);
    transaction.commit().unwrap();
}

#[test]
fn partitioned_memory_vectors_refresh_exact_fences_and_preserve_the_model_set() {
    let (mut conn, mut request) = prepared();
    seed_note(&conn, MEMORY, "source", "memory");
    seed_note(&conn, SECOND_MEMORY, "source", "memory");
    request.routes.push(route("note:memory", "target-b"));
    create_vectors(&conn, "vec_second_model");
    for table in ["vec_partition_model", "vec_second_model"] {
        vector(&conn, table, MEMORY, "source", "note", "note.content");
    }
    vector(
        &conn,
        "vec_partition_model",
        SECOND_MEMORY,
        "source",
        "note",
        "note.content",
    );
    conn.execute_batch(
        "INSERT INTO memory_visibility_receipts (namespace, note_id, model_count) VALUES \
         ('source', '88888888-8888-4888-8888-000000000001', 2), \
         ('source', '99999999-9999-4999-8999-000000000001', 1), \
         ('source', '11111111-1111-4111-8111-000000000002', 0), \
         ('target-a', '77777777-7777-4777-8777-000000000001', 1); \
         INSERT INTO memory_visibility_fences (namespace, note_id, model, ann_write_log_seq) VALUES \
         ('source', '88888888-8888-4888-8888-000000000001', 'vec_partition_model', 17), \
         ('source', '88888888-8888-4888-8888-000000000001', 'vec_second_model', 18), \
         ('source', '99999999-9999-4999-8999-000000000001', 'vec_partition_model', 19), \
         ('target-a', '77777777-7777-4777-8777-000000000001', 'vec_partition_model', 99); \
         INSERT INTO ann_consumer_watermark (consumer, namespace, embedding_model, watermark) \
         VALUES ('memory-notes:note.content', '*', 'vec_partition_model', 100); \
         INSERT INTO ann_write_log (seq, namespace, embedding_model, kind, field, subject_id, op) \
         VALUES (100, 'target-a', 'vec_partition_model', 'note', 'note.content', \
                 '77777777-7777-4777-8777-000000000001', 'upsert');",
    )
    .unwrap();
    let transaction = conn.transaction().unwrap();
    let counts = move_namespace(&transaction, &request).unwrap();
    assert_eq!(counts.ann_log_appended, 22);
    for (subject, table) in [
        (MEMORY, "vec_partition_model"),
        (MEMORY, "vec_second_model"),
        (SECOND_MEMORY, "vec_partition_model"),
    ] {
        let (fence, upsert): (i64, i64) = transaction
            .query_row(
                "SELECT fence.ann_write_log_seq, log.seq FROM memory_visibility_fences AS fence \
             JOIN ann_write_log AS log ON log.subject_id = fence.note_id \
               AND log.embedding_model = fence.model AND log.namespace = fence.namespace \
               AND log.op = 'upsert' \
             WHERE fence.namespace = 'target-b' AND fence.note_id = ?1 AND fence.model = ?2",
                rusqlite::params![subject, table],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(upsert > 100);
        assert_eq!(
            fence, upsert,
            "each stored fence names its own moved-vector upsert"
        );
        let place: String = transaction
            .query_row(
                &format!("SELECT namespace FROM {table} WHERE subject_id = ?1"),
                [subject],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(place, "target-b");
    }
    for (id, target, models) in [
        (MEMORY, "target-b", 2),
        (SECOND_MEMORY, "target-b", 1),
        (NOTE_TASK, "target-b", 0),
        (RESIDENT, "target-a", 1),
    ] {
        let stored: (String, i64) = transaction
            .query_row(
                "SELECT namespace, model_count FROM memory_visibility_receipts WHERE note_id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored, (target.into(), models));
    }
    let untouched: i64 = transaction
        .query_row(
            "SELECT ann_write_log_seq FROM memory_visibility_fences WHERE note_id = ?1",
            [RESIDENT],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(untouched, 99);
    let invented: i64 = transaction
        .query_row(
            "SELECT COUNT(*) FROM memory_visibility_fences WHERE note_id = ?1",
            [NOTE_TASK],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(invented, 0, "a zero-model receipt does not acquire a fence");
    let foreign_key_errors: i64 = transaction
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(foreign_key_errors, 0);
    transaction.commit().unwrap();
}

fn recall(conn: &Connection, namespace: &str) -> Vec<String> {
    conn.prepare(
        "SELECT subject_id FROM vec_partition_model \
         WHERE embedding MATCH '[0.1, 0.2]' AND k = 20 AND namespace = ?1",
    )
    .unwrap()
    .query_map([namespace], |row| row.get(0))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

#[test]
fn deleting_a_partitioned_record_removes_its_vector_from_both_namespaces() {
    let (mut conn, request) = prepared();
    assert!(recall(&conn, "source")
        .iter()
        .any(|id| id == NOTE_OBSERVATION));
    let transaction = conn.transaction().unwrap();
    move_namespace(&transaction, &request).unwrap();
    assert!(!recall(&transaction, "source")
        .iter()
        .any(|id| id == NOTE_OBSERVATION));
    assert!(recall(&transaction, "target-a")
        .iter()
        .any(|id| id == NOTE_OBSERVATION));
    assert_eq!(
        transaction
            .execute(
                "DELETE FROM notes WHERE id = ?1 AND namespace = 'target-a'",
                [NOTE_OBSERVATION],
            )
            .unwrap(),
        1
    );
    crate::stores::vectors::delete_subject_from_vector_tables(
        &transaction,
        &["vec_partition_model".into()],
        uuid::Uuid::parse_str(NOTE_OBSERVATION).unwrap(),
        "target-a",
    )
    .unwrap();
    for namespace in ["source", "target-a"] {
        assert!(!recall(&transaction, namespace)
            .iter()
            .any(|id| id == NOTE_OBSERVATION));
    }
    transaction.commit().unwrap();
}

#[test]
fn partitioned_orphan_and_competing_subject_routes_refuse_before_any_write() {
    for ambiguous in [false, true] {
        let (conn, mut request) = prepared();
        let subject = if ambiguous {
            NOTE_TASK
        } else {
            "aaaaaaaa-aaaa-4aaa-8aaa-000000000001"
        };
        if ambiguous {
            conn.execute(
                "INSERT INTO entities (id, namespace, kind, name, created_at, updated_at) \
                 VALUES (?1, 'source', 'concept', 'competing source subject', 1, 1)",
                [subject],
            )
            .unwrap();
        } else {
            vector(
                &conn,
                "vec_partition_model",
                subject,
                "source",
                "entity",
                "knowledge.atom",
            );
        }
        request.routes.push(route("note:empty", "unused-target"));
        let before = places_and_bytes(&conn);
        let changes_before: i64 = conn
            .query_row("SELECT total_changes()", [], |row| row.get(0))
            .unwrap();
        let error = move_namespace(&conn, &request).unwrap_err();
        assert!(
            matches!(&error, MoveError::UnroutableVector { table, subject_id, destinations }
            if table == "vec_partition_model" && subject_id == subject
                && *destinations == if ambiguous { 2 } else { 0 })
        );
        assert!(error.to_string().contains("unroutable_vector"));
        assert_eq!(places_and_bytes(&conn), before);
        let changes_after: i64 = conn
            .query_row("SELECT total_changes()", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            changes_after, changes_before,
            "inspect before rollback can hide a write"
        );
    }
}

#[test]
fn partitioned_subject_collision_preserves_vector_bytes_and_the_write_log() {
    let (mut conn, request) = prepared();
    conn.execute(
        "INSERT INTO knowledge_atoms (id, namespace, slug, name, created_at, updated_at) \
         VALUES (?1, 'target-b', 'shared-slug', 'occupied destination', 1, 1)",
        [ATOM_SLUG_HOLDER],
    )
    .unwrap();
    let before = places_and_bytes(&conn);
    let transaction = conn.transaction().unwrap();
    let error = move_namespace(&transaction, &request).unwrap_err();
    assert!(matches!(&error, MoveError::Collisions { collisions }
        if collisions.iter().any(|collision| collision.table == "knowledge_atoms" && collision.target == "target-b")));
    assert_eq!(places_and_bytes(&transaction), before);
    let log_count: i64 = transaction
        .query_row("SELECT COUNT(*) FROM ann_write_log", [], |row| row.get(0))
        .unwrap();
    assert_eq!(log_count, 0);
    transaction.rollback().unwrap();
}

#[test]
fn partitioned_late_failure_rolls_back_through_the_callers_transaction() {
    let (mut conn, request) = prepared();
    let before = places_and_bytes(&conn);
    conn.execute_batch(
        "CREATE TEMP TRIGGER reject_partition_upsert BEFORE INSERT ON ann_write_log \
         WHEN NEW.namespace = 'target-b' AND NEW.op = 'upsert' \
         BEGIN SELECT RAISE(ABORT, 'partition upsert refusal'); END;",
    )
    .unwrap();
    let transaction = conn.transaction().unwrap();
    let error = move_namespace(&transaction, &request).unwrap_err();
    assert!(error.to_string().contains("partition upsert refusal"));
    transaction.rollback().unwrap();
    assert_eq!(places_and_bytes(&conn), before);
    for table in [
        "notes",
        "entities",
        "graph_edges",
        "knowledge_atoms",
        "knowledge_domains",
        "knowledge_sections",
    ] {
        let target_rows: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE namespace != 'source' AND id != ?1"),
                [RESIDENT],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            target_rows, 0,
            "rollback preserves source subjects in {table}"
        );
    }
    let log_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM ann_write_log", [], |row| row.get(0))
        .unwrap();
    assert_eq!(log_count, 0);
}

#[test]
fn an_empty_partitioned_route_map_over_an_empty_source_is_a_noop() {
    crate::extension::ensure_extensions_loaded();
    let conn = migrated();
    create_vectors(&conn, "vec_partition_model");
    let counts = move_namespace(&conn, &MoveRequest::new("empty-source", vec![])).unwrap();
    assert!(counts.subjects.is_empty());
    assert_eq!(counts.rows.get("vec_partition_model"), Some(&0));
    assert_eq!(counts.ann_log_appended, 0);
}
