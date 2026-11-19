use super::{
    blob_gc_unowned_attachment_predicate, blob_root_key, claim_blob_gc_batch,
    parse_blob_gc_claim_rows, release_blob_gc_batch, FsBlobStore,
};
use crate::StorageBackend;
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

#[tokio::test]
async fn quarantine_ownership_reaches_the_actual_gc_batch_claim_and_dry_run() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsBlobStore::new(dir.path().join("blobs"), 0).unwrap();
    let canonical = store.put(b"canonical owner".to_vec()).await.unwrap();
    let quarantined = store.put(b"quarantine owner".to_vec()).await.unwrap();
    let orphan = store.put(b"real orphan".to_vec()).await.unwrap();
    let grace = store.put(b"grace orphan".to_vec()).await.unwrap();
    let backend = StorageBackend::memory().expect("isolated batch database");
    backend
        .prepare_core_schema()
        .expect("actual complete core schema");
    let owner = "33333333-3333-4333-8333-333333333333";
    let quarantine_owner = "44444444-4444-4444-8444-444444444444";
    let role = "quarantine-original\u{85}";
    {
        let writer = backend.pool().writer().unwrap();
        writer.conn().execute(
            "INSERT INTO attachments (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at) VALUES (?1, 'note', 'message-attachment:0', ?2, NULL, 15, 1)",
            params![owner, canonical.as_str()],
        ).unwrap();
        writer.conn().execute(
            "INSERT INTO attachment_quarantine (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at, reason) VALUES (?1, 'note', ?2, ?3, NULL, 16, 2, 'invalid_role')",
            params![quarantine_owner, role, quarantined.as_str()],
        ).unwrap();
    }
    let snapshot = || {
        let writer = backend.pool().writer().unwrap();
        let canonical_row: (String, String, String) = writer
            .conn()
            .query_row(
                "SELECT record_uuid, role, content_ref FROM attachments",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let quarantine_row: (String, String, String, String) = writer
            .conn()
            .query_row(
                "SELECT record_uuid, role, content_ref, reason FROM attachment_quarantine",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        (canonical_row, quarantine_row)
    };
    let before = snapshot();
    assert_eq!(
        before.0,
        (
            owner.to_string(),
            "message-attachment:0".into(),
            canonical.to_string()
        )
    );
    assert_eq!(
        before.1,
        (
            quarantine_owner.to_string(),
            role.into(),
            quarantined.to_string(),
            "invalid_role".into()
        )
    );
    let sql = backend.sql();
    let root_key = blob_root_key(store.root());
    let candidates = vec![
        (quarantined.clone(), false),
        (orphan.clone(), false),
        (canonical.clone(), false),
        (grace.clone(), true),
    ];
    // Exercise the internal claim seam without weakening public sweep admission.
    let dry_run = claim_blob_gc_batch(sql.as_ref(), root_key.clone(), &candidates, true)
        .await
        .expect("actual quarantine-aware dry run");
    assert_eq!(
        dry_run.would_delete, 1,
        "only the independent orphan is eligible"
    );
    assert_eq!(dry_run.grace_period_skipped, 1);
    assert!(dry_run.claimed_rows.is_empty());
    let claims: i64 = backend
        .pool()
        .writer()
        .unwrap()
        .conn()
        .query_row("SELECT COUNT(*) FROM blob_gc_claims", [], |row| row.get(0))
        .unwrap();
    assert_eq!(claims, 0, "dry run writes no claims");
    assert_eq!(snapshot(), before);

    let claimed = claim_blob_gc_batch(sql.as_ref(), root_key.clone(), &candidates, false)
        .await
        .expect("actual quarantine-aware claim");
    assert_eq!(claimed.would_delete, 1);
    assert_eq!(claimed.grace_period_skipped, 1);
    assert_eq!(
        parse_blob_gc_claim_rows(claimed.claimed_rows).unwrap(),
        vec![orphan]
    );
    assert_eq!(snapshot(), before, "claiming preserves both owners exactly");
    for (reference, bytes) in [
        (&canonical, b"canonical owner".as_slice()),
        (&quarantined, b"quarantine owner".as_slice()),
        (&grace, b"grace orphan".as_slice()),
    ] {
        assert_eq!(
            store.get_bounded_verified(reference, 64).await.unwrap(),
            bytes
        );
    }
    release_blob_gc_batch(sql.as_ref(), root_key.clone())
        .await
        .unwrap();
    {
        let writer = backend.pool().writer().unwrap();
        assert_eq!(
            writer
                .conn()
                .execute(
                    "DELETE FROM attachment_quarantine WHERE record_uuid = ?1",
                    [quarantine_owner],
                )
                .unwrap(),
            1
        );
    }
    let released = claim_blob_gc_batch(
        sql.as_ref(),
        root_key.clone(),
        &[(quarantined.clone(), false)],
        false,
    )
    .await
    .expect("detached quarantine ownership becomes claimable");
    assert_eq!(released.would_delete, 1);
    assert_eq!(
        parse_blob_gc_claim_rows(released.claimed_rows).unwrap(),
        vec![quarantined]
    );
    release_blob_gc_batch(sql.as_ref(), root_key).await.unwrap();
    let claims: i64 = backend
        .pool()
        .writer()
        .unwrap()
        .conn()
        .query_row("SELECT COUNT(*) FROM blob_gc_claims", [], |row| row.get(0))
        .unwrap();
    assert_eq!(claims, 0);
}
