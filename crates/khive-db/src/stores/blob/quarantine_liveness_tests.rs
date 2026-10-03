use super::{blob_gc_unowned_attachment_predicate, FsBlobStore};
use khive_storage::BlobStore;
use rusqlite::{params, Connection};

fn unowned_refs(conn: &Connection, candidates: &[&str], quarantine_present: bool) -> Vec<String> {
    let statement = format!(
        "SELECT candidate.value FROM json_each(?1) AS candidate \
         WHERE {} ORDER BY CAST(candidate.key AS INTEGER)",
        blob_gc_unowned_attachment_predicate(quarantine_present)
    );
    let encoded = serde_json::to_string(candidates).expect("encode real candidate refs");
    let mut query = conn
        .prepare(&statement)
        .expect("prepare the production ownership predicate");
    let rows = query
        .query_map([encoded], |row| row.get::<_, String>(0))
        .expect("execute the production ownership predicate");
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .expect("read the production ownership decisions")
}

#[tokio::test]
async fn quarantine_liveness_predicate_keeps_quarantined_blob_live() {
    let dir = tempfile::tempdir().expect("create an isolated object root");
    let store = FsBlobStore::new(dir.path().join("blobs"), 0).expect("open the object store");
    let canonical_bytes = b"canonical attachment owner";
    let quarantined_bytes = b"quarantined attachment owner";
    let orphan_bytes = b"independent unowned object";
    let canonical = store
        .put(canonical_bytes.to_vec())
        .await
        .expect("publish the canonical owner's object");
    let quarantined = store
        .put(quarantined_bytes.to_vec())
        .await
        .expect("publish the quarantined owner's object");
    let orphan = store
        .put(orphan_bytes.to_vec())
        .await
        .expect("publish the independent orphan");
    assert_ne!(canonical, quarantined);
    assert_ne!(canonical, orphan);
    assert_ne!(quarantined, orphan);
    for content_ref in [&canonical, &quarantined, &orphan] {
        assert!(
            store
                .exists(content_ref)
                .await
                .expect("check the real object"),
            "every candidate must name a published object"
        );
    }

    let conn = Connection::open_in_memory().expect("open an isolated ownership database");
    conn.execute_batch(
        "CREATE TABLE blob_gc_claims (
             root_key TEXT NOT NULL,
             content_ref TEXT NOT NULL,
             claimed_at INTEGER NOT NULL,
             PRIMARY KEY (root_key, content_ref)
         ) STRICT;",
    )
    .expect("create the claim table required by the migration fences");
    conn.execute_batch(include_str!("../../../sql/021-attachments-a-stage.sql"))
        .expect("apply the real attachment schema");
    conn.execute_batch(include_str!(
        "../../../sql/021-attachments-b-claim-fences.sql"
    ))
    .expect("apply the real attachment claim fences");

    let legacy_role = "message-attachment:0\u{85}";
    conn.execute(
        "INSERT INTO attachments
             (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at)
         VALUES (?1, 'note', 'message-attachment:0', ?2, NULL, ?3, 1)",
        params![
            "11111111-1111-4111-8111-111111111111",
            canonical.as_str(),
            canonical_bytes.len() as i64
        ],
    )
    .expect("register the canonical owner");
    conn.execute(
        "INSERT INTO attachments
             (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at)
         VALUES (?1, 'note', ?2, ?3, NULL, ?4, 2)",
        params![
            "22222222-2222-4222-8222-222222222222",
            legacy_role,
            quarantined.as_str(),
            quarantined_bytes.len() as i64
        ],
    )
    .expect("register a role accepted by the historical schema");

    let quarantine_tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name = 'attachment_quarantine'",
            [],
            |row| row.get(0),
        )
        .expect("inspect the historical table set");
    assert_eq!(
        quarantine_tables, 0,
        "the false branch must use no quarantine table"
    );
    let candidates = [quarantined.as_str(), orphan.as_str(), canonical.as_str()];
    assert_eq!(
        unowned_refs(&conn, &candidates, false),
        vec![orphan.to_string()],
        "the historical production predicate must keep both attachment owners live"
    );

    conn.execute_batch(include_str!(
        "../../../sql/047-attachment-role-quarantine.sql"
    ))
    .expect("migrate the actual rejected role into durable quarantine ownership");
    let retained: (String, String, String, i64) = conn
        .query_row(
            "SELECT role, content_ref, reason, size_bytes FROM attachment_quarantine
             WHERE record_uuid = ?1",
            ["22222222-2222-4222-8222-222222222222"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("read the migrated owner independently of the liveness predicate");
    assert_eq!(
        retained,
        (
            legacy_role.to_string(),
            quarantined.to_string(),
            "invalid_role".to_string(),
            quarantined_bytes.len() as i64
        ),
        "the migration must preserve the rejected role and its object ref"
    );
    let attachment_refs: Vec<String> = conn
        .prepare("SELECT content_ref FROM attachments ORDER BY record_uuid")
        .expect("prepare the canonical owner census")
        .query_map([], |row| row.get(0))
        .expect("read the canonical owner census")
        .collect::<rusqlite::Result<_>>()
        .expect("collect the canonical owner census");
    assert_eq!(attachment_refs, vec![canonical.to_string()]);

    assert!(
        unowned_refs(&conn, &[canonical.as_str()], true).is_empty(),
        "the production predicate must keep the canonical attachment live"
    );
    assert_eq!(
        unowned_refs(&conn, &[orphan.as_str()], true),
        vec![orphan.to_string()],
        "the independent real orphan must remain collectible by the predicate"
    );
    assert!(
        unowned_refs(&conn, &[quarantined.as_str()], true).is_empty(),
        "the production predicate must keep the quarantined object live"
    );
    assert_eq!(
        unowned_refs(&conn, &candidates, true),
        vec![orphan.to_string()],
        "only the independent orphan may be selected after quarantine migration"
    );
}
