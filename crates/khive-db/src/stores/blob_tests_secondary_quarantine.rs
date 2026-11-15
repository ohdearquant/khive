// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
use super::*;

use khive_storage::{Attachment, AttachmentSubstrate, NewAttachment, Note};

#[tokio::test]
async fn completed_v21_sweep_keeps_secondary_quarantine_and_deletes_unowned_control() {
    let dir = tempfile::tempdir().unwrap();
    let main = crate::StorageBackend::sqlite_for_test(dir.path().join("main.db")).unwrap();
    {
        let mut writer = main.pool().writer().unwrap();
        super::prepare_completed_v21_gc_fixture(writer.conn_mut());
    }
    let secondary = crate::StorageBackend::memory().unwrap();
    secondary.pool().run_migrations().unwrap();
    let store = FsBlobStore::new(dir.path().join("blobs"), 0)
        .unwrap()
        .with_orphan_sweep_grace(Duration::from_secs(60));
    let original = b"byte-exact secondary quarantine original".to_vec();
    let owned = store.put(original.clone()).await.unwrap();
    let unowned = store
        .put(b"backdated unowned control".to_vec())
        .await
        .unwrap();
    let note = Note::new("local", "message", "quarantined secondary message").with_properties(
        serde_json::json!({
            "quarantined": true,
            "channel_kind": "email",
            "channel_slug": "secondary-quarantine",
            "quarantine_content_ref": owned.to_string(),
        }),
    );
    assert!(secondary
        .notes()
        .unwrap()
        .try_insert_note(note.clone())
        .await
        .unwrap());
    assert!(main
        .attachments()
        .unwrap()
        .try_insert_attachment(Attachment::from_new(
            note.id,
            AttachmentSubstrate::Note,
            NewAttachment {
                role: "quarantine-original".into(),
                content_ref: owned.clone(),
                media_type: None,
                size_bytes: Some(original.len() as u64),
            },
            note.created_at,
        ))
        .await
        .unwrap());

    // Reuse the canonical private completed-V21 fixture. Neither the current
    // migration ledger nor the production GC admission gate is altered.
    for content_ref in [&owned, &unowned] {
        fs::OpenOptions::new()
            .write(true)
            .open(shard_path(store.root(), content_ref))
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
    }
    let report = store
        .transactional_orphan_sweep(main.sql().as_ref(), true)
        .await
        .expect("completed-V21 report-only sweep");
    assert_eq!(report.would_delete, 1);
    assert_eq!(report.deleted, 0);
    assert_eq!(report.grace_period_skipped, 0);
    assert!(store.exists(&owned).await.unwrap());
    assert!(store.exists(&unowned).await.unwrap());

    let sweep = store
        .transactional_orphan_sweep(main.sql().as_ref(), false)
        .await
        .expect("completed-V21 destructive sweep");
    assert_eq!(sweep.deleted, 1);
    assert_eq!(sweep.grace_period_skipped, 0);
    assert_eq!(
        store
            .get_bounded_verified(&owned, original.len() as u64)
            .await
            .unwrap(),
        original
    );
    assert!(!store.exists(&unowned).await.unwrap());
    assert!(secondary
        .notes()
        .unwrap()
        .get_note(note.id)
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        main.attachments()
            .unwrap()
            .get_attachment(note.id, "quarantine-original")
            .await
            .unwrap()
            .unwrap()
            .content_ref,
        owned
    );
}
