use super::{
    add_embedding_truncation_warning, build_references_header, bulk_read_response, channel_stalled,
    heartbeat_note_id, mark_read_target, message_id_match_candidates, parent_references_chain,
    parent_wire_message_id, read_response, read_result_with_body, sanitize_reference_token,
    send_response_thread_id, validate_read_target, wait_for_inbox_response, wrap_message_id,
};
use crate::inbox_signal::InboxSignal;
use khive_storage::note::Note;
use khive_storage::StorageError;
use serde_json::{json, Value};

#[tokio::test]
async fn post_commit_delete_detaches_original_even_if_followup_read_would_fail() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let runtime = super::KhiveRuntime::memory().expect("runtime");
    let note_id = uuid::Uuid::new_v4();
    let content_ref =
        khive_storage::ContentRef::from_hex("a".repeat(64)).expect("fixture content ref");
    let attachments = runtime.core().attachments().expect("main attachments");
    assert!(attachments
        .try_insert_attachment(khive_storage::Attachment::from_new(
            note_id,
            khive_storage::AttachmentSubstrate::Note,
            khive_storage::NewAttachment {
                role: "quarantine-original".into(),
                content_ref: content_ref.clone(),
                media_type: None,
                size_bytes: None,
            },
            1,
        ))
        .await
        .expect("seed routed owner"));
    let original_message = "injected post-commit delete failure";
    let delete_error: khive_runtime::RuntimeError =
        khive_types::KhiveError::internal(original_message)
            .with_details(khive_types::Details::new_owned([
                ("reason", "post_commit_degraded".to_string()),
                ("operation", "delete_note".to_string()),
                ("record_id", note_id.to_string()),
                ("committed", "true".to_string()),
            ]))
            .into();
    let delete_result = Err(delete_error);
    let read_attempted = AtomicBool::new(false);
    let note_deleted = super::note_deleted_after_attempt(&delete_result, note_id, async {
        read_attempted.store(true, Ordering::SeqCst);
        Err(khive_runtime::RuntimeError::Internal(
            "injected follow-up read failure".into(),
        ))
    })
    .await
    .expect("typed committed error settles deletion without a read");
    assert!(note_deleted);
    assert!(!read_attempted.load(Ordering::SeqCst));
    assert!(
        super::detach_deleted_legacy_original(&runtime, note_id, Some(content_ref.as_str()),)
            .await
            .expect("detach routed original")
    );
    assert!(attachments
        .get_attachment(note_id, "quarantine-original")
        .await
        .expect("owner lookup")
        .is_none());
    let returned = match delete_result {
        Ok(_) => panic!("expected the original post-commit error"),
        Err(error) => error,
    };
    let khive_runtime::RuntimeError::Khive(domain) = returned.refusal_source() else {
        panic!("expected the typed post-commit error");
    };
    let details = domain.details().expect("post-commit details");
    assert_eq!(details.get("reason"), Some("post_commit_degraded"));
    assert_eq!(details.get("operation"), Some("delete_note"));
    assert_eq!(details.get("committed"), Some("true"));
    let note_id_str = note_id.to_string();
    assert_eq!(details.get("record_id"), Some(note_id_str.as_str()));
    assert!(returned.to_string().contains(original_message));
}

#[tokio::test]
async fn routed_cleanup_preserves_post_commit_error_after_detaching_original() {
    use std::sync::Arc;

    use khive_runtime::{BackendId, Namespace, RuntimeConfig, StorageBackend};
    use khive_storage::{Attachment, AttachmentSubstrate, ContentRef, NewAttachment};

    let main = Arc::new(StorageBackend::memory().expect("main backend"));
    let comm = Arc::new(StorageBackend::memory().expect("comm backend"));
    main.prepare_core_schema().expect("main schema");
    comm.prepare_core_schema().expect("comm schema");
    let mut config = RuntimeConfig::no_embeddings();
    config.backend_id = BackendId::parse("old-comm").expect("backend id");
    config.packs = vec!["kg".into(), "comm".into()];
    let runtime = super::KhiveRuntime::from_backend(comm, config).with_core_backend(main);
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let content_ref = ContentRef::from_hex("b".repeat(64)).expect("content ref");
    let mut note = khive_storage::note::Note::new("local", "message", "legacy quarantine")
        .with_properties(json!({
            "quarantined": true,
            "channel_kind": "email",
            "quarantine_content_ref": content_ref.to_string(),
        }));
    note.expires_at = Some(note.created_at - 1);
    let note_id = note.id;
    let as_of = note.created_at + 1;
    assert!(runtime
        .backend()
        .notes()
        .expect("comm notes")
        .try_insert_note(note)
        .await
        .expect("seed note"));
    let attachments = runtime.core().attachments().expect("main attachments");
    assert!(attachments
        .try_insert_attachment(Attachment::from_new(
            note_id,
            AttachmentSubstrate::Note,
            NewAttachment {
                role: "quarantine-original".into(),
                content_ref,
                media_type: None,
                size_bytes: None,
            },
            as_of,
        ))
        .await
        .expect("seed original owner"));
    runtime
        .sql()
        .writer()
        .await
        .expect("comm writer")
        .execute_script(
            "CREATE TRIGGER fail_note_deleted_event BEFORE INSERT ON events \
                 WHEN NEW.kind = 'note_deleted' \
                 BEGIN SELECT RAISE(ABORT, 'injected delete event failure'); END;"
                .into(),
        )
        .await
        .expect("install post-commit fault");

    let error = super::handle_cleanup_expired_quarantine(
        &runtime,
        &token,
        json!({
            "channel_kind": "email",
            "channel_slug": "",
            "mode": "legacy_slugless",
            "as_of_micros": as_of,
        }),
        std::time::Duration::ZERO,
    )
    .await
    .expect_err("original post-commit error must surface");
    let khive_runtime::RuntimeError::Khive(original) = error.refusal_source() else {
        panic!("cleanup replaced the typed delete error: {error:?}");
    };
    let details = original.details().expect("post-commit error details");
    assert_eq!(details.get("reason"), Some("post_commit_degraded"));
    assert_eq!(details.get("operation"), Some("delete_note"));
    assert_eq!(details.get("record_id"), Some(note_id.to_string().as_str()));
    assert!(
        details
            .get("post_commit_degradations")
            .is_some_and(|stages| stages.contains("injected delete event failure")),
        "{error}"
    );
    assert!(runtime
        .notes(&token)
        .expect("comm notes")
        .get_note_including_deleted(note_id)
        .await
        .expect("note lookup")
        .is_none());
    assert!(attachments
        .get_attachment(note_id, "quarantine-original")
        .await
        .expect("main owner lookup")
        .is_none());
}

#[tokio::test(start_paused = true)]
async fn inbox_query_crossing_deadline_requeries_after_publish() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let signal = InboxSignal::new();
    let query_started = Arc::new(tokio::sync::Barrier::new(2));
    let release_query = Arc::new(tokio::sync::Notify::new());
    let query_calls = Arc::new(AtomicUsize::new(0));
    let deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_millis(250));

    let query = || {
        let query_started = Arc::clone(&query_started);
        let release_query = Arc::clone(&release_query);
        let query_calls = Arc::clone(&query_calls);
        async move {
            let call = query_calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                query_started.wait().await;
                release_query.notified().await;
                Ok(json!({ "messages": [] }))
            } else {
                Ok(json!({ "messages": [{ "content": "committed during query" }] }))
            }
        }
    };

    let commit_during_query = async {
        query_started.wait().await;
        tokio::time::advance(std::time::Duration::from_millis(250)).await;
        signal.publish();
        release_query.notify_one();
    };

    let (response, ()) = tokio::join!(
        wait_for_inbox_response(&signal, deadline, query),
        commit_during_query,
    );
    let response = response.expect("deadline-edge re-query succeeds");

    assert_eq!(query_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        response["messages"][0]["content"],
        json!("committed during query")
    );
}

#[tokio::test(start_paused = true)]
async fn inbox_wait_rejects_response_without_messages_array() {
    let signal = InboxSignal::new();
    let result = wait_for_inbox_response(&signal, None, || async { Ok(json!({})) }).await;
    let err = result.expect_err("a response without `messages` must error, not panic");
    assert!(
        matches!(err, khive_runtime::RuntimeError::Internal(_)),
        "missing `messages` must surface as an internal error: {err}"
    );
}

#[tokio::test(start_paused = true)]
async fn inbox_final_query_after_timeout_returns_newly_visible_message() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let signal = InboxSignal::new();
    let query_calls = Arc::new(AtomicUsize::new(0));
    let deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_millis(250));

    let query = || {
        let query_calls = Arc::clone(&query_calls);
        async move {
            let call = query_calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                Ok(json!({ "messages": [] }))
            } else {
                Ok(json!({ "messages": [{ "content": "visible at the timeout edge" }] }))
            }
        }
    };

    // No publish happens, so the timer wins the select; the final query
    // must still surface the row that became visible by deadline expiry.
    let response = wait_for_inbox_response(&signal, deadline, query)
        .await
        .expect("timeout-edge final query succeeds");
    assert_eq!(query_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        response["messages"][0]["content"],
        json!("visible at the timeout edge")
    );
}

#[tokio::test]
async fn duplicate_quarantine_repair_preserves_a_competing_attachment() {
    use std::sync::Arc;

    use khive_runtime::{AllowAllGate, BackendId, Namespace, RuntimeConfig};
    use khive_storage::{Attachment, AttachmentSubstrate, BlobStore as _, NewAttachment};
    use tokio::sync::Barrier;
    use uuid::Uuid;

    let namespace = format!("ingest-quarantine-race-{}", Uuid::new_v4().simple());
    let runtime = Arc::new(
        super::KhiveRuntime::new(RuntimeConfig {
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            web: Default::default(),
            telemetry: Default::default(),
            mounts: Vec::new(),
            brain: Default::default(),
            git_write: Default::default(),
            display_timezone: khive_runtime::config::resolve_default_display_timezone(),
            events_split: None,
            db_path: None,
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::parse(&namespace).unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "comm".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
            exec: Default::default(),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        })
        .expect("in-memory runtime"),
    );
    let blob_root = tempfile::tempdir().expect("blob root");
    let blob_store = Arc::new(
        khive_db::stores::blob::FsBlobStore::new(blob_root.path().to_path_buf(), 0)
            .expect("blob store"),
    );
    let original_ref = blob_store
        .put(b"quarantine original".to_vec())
        .await
        .expect("publish original");
    let competing_ref = blob_store
        .put(b"competing original".to_vec())
        .await
        .expect("publish competing bytes");
    runtime
        .install_blob_store(blob_store)
        .expect("install blob store");
    let token = runtime
        .authorize(Namespace::parse(&namespace).unwrap())
        .expect("authorize");
    let signal = crate::inbox_signal::InboxSignal::new();
    let capability = khive_runtime::ChannelIngestCapability::grant_for_direct_composition();
    let body = json!({
        "from": "email:sender@example.com",
        "to": "local",
        "content": "quarantined message",
        "external_id": "imap:quarantine:race:1",
        "metadata": {
            "quarantined": true,
            "quarantine_content_ref": original_ref.to_string(),
        },
    });

    let first = super::handle_ingest(
        &runtime,
        &signal,
        Some(&capability),
        &Ok(None),
        &token,
        body.clone(),
        std::time::Duration::from_secs(14 * 24 * 60 * 60),
    )
    .await
    .expect("initial quarantine ingest");
    let note_id = Uuid::parse_str(first["full_id"].as_str().expect("full note id"))
        .expect("canonical note id");
    let attachments = runtime.core().attachments().expect("attachment store");
    assert!(attachments
        .delete_attachment(note_id, "quarantine-original")
        .await
        .expect("leave legacy metadata-only row"));

    let arrived = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let replay = {
        let runtime = Arc::clone(&runtime);
        let token = token.clone();
        let arrived = Arc::clone(&arrived);
        let resume = Arc::clone(&resume);
        tokio::spawn(super::race_seam::AFTER_QUARANTINE_ROLE_READ.scope(
            (arrived, resume),
            async move {
                let capability =
                    khive_runtime::ChannelIngestCapability::grant_for_direct_composition();
                let signal = crate::inbox_signal::InboxSignal::new();
                super::handle_ingest(
                    &runtime,
                    &signal,
                    Some(&capability),
                    &Ok(None),
                    &token,
                    body,
                    std::time::Duration::from_secs(14 * 24 * 60 * 60),
                )
                .await
            },
        ))
    };
    tokio::time::timeout(std::time::Duration::from_secs(15), arrived.wait())
        .await
        .expect("replay must reach the post-read pause");

    let competing = Attachment::from_new(
        note_id,
        AttachmentSubstrate::Note,
        NewAttachment {
            role: "quarantine-original".to_string(),
            content_ref: competing_ref,
            media_type: None,
            size_bytes: None,
        },
        1,
    );
    attachments
        .upsert_attachment(competing.clone())
        .await
        .expect("competing writer installs role");
    resume.wait().await;

    let result = tokio::time::timeout(std::time::Duration::from_secs(15), replay)
        .await
        .expect("replay must finish")
        .expect("replay task");
    assert!(
        matches!(&result, Err(khive_runtime::RuntimeError::Internal(_))),
        "a stale replay must refuse rather than acknowledge a different owner: {result:?}"
    );
    assert_eq!(
        attachments
            .get_attachment(note_id, "quarantine-original")
            .await
            .expect("stored attachment"),
        Some(competing),
        "the competing writer's role must survive the replay"
    );
}

#[tokio::test]
async fn duplicate_quarantine_repair_refuses_a_changed_property_precondition() {
    use std::sync::Arc;

    use khive_runtime::{ChannelIngestCapability, Namespace, RuntimeError};
    use khive_storage::BlobStore as _;
    use tokio::sync::Barrier;

    let runtime = Arc::new(super::KhiveRuntime::memory().unwrap());
    let blob_root = tempfile::tempdir().unwrap();
    let blob_store = Arc::new(
        khive_db::stores::blob::FsBlobStore::new(blob_root.path().to_path_buf(), 0).unwrap(),
    );
    let content_ref = blob_store
        .put(b"guarded replay original".to_vec())
        .await
        .unwrap();
    runtime.install_blob_store(blob_store).unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let legacy = Note::new("local", "message", "quarantine").with_properties(json!({
        "external_id": "imap:mail.example.com:17:guarded-repair",
        "direction": "inbound",
        "thread_id": uuid::Uuid::new_v4().to_string(),
        "channel_kind": "email",
        "channel_slug": "mailbox@example.com",
        "quarantined": true,
        "unrelated": {"kept": true},
    }));
    let note_id = legacy.id;
    let raw = runtime.backend().notes().unwrap();
    assert!(raw.try_insert_note(legacy.clone()).await.unwrap());
    let body = json!({
        "from": "email:quarantine",
        "to": "local",
        "content": "quarantine replay",
        "channel_kind": "email",
        "channel_slug": "mailbox@example.com",
        "external_id": "imap:mail.example.com:mailbox@example.com:17:guarded-repair",
        "legacy_external_id": "imap:mail.example.com:17:guarded-repair",
        "metadata": {
            "quarantined": true,
            "quarantine_content_ref": content_ref.to_string(),
        },
    });
    let arrived = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let replay = {
        let runtime = Arc::clone(&runtime);
        let token = token.clone();
        let body = body.clone();
        tokio::spawn(super::race_seam::AFTER_QUARANTINE_ROLE_READ.scope(
            (Arc::clone(&arrived), Arc::clone(&resume)),
            async move {
                super::handle_ingest(
                    &runtime,
                    &InboxSignal::new(),
                    Some(&ChannelIngestCapability::grant_for_direct_composition()),
                    &Ok(None),
                    &token,
                    body,
                    std::time::Duration::from_secs(60),
                )
                .await
            },
        ))
    };
    tokio::time::timeout(std::time::Duration::from_secs(15), arrived.wait())
        .await
        .expect("replay must pause after reading the absent attachment");
    assert!(raw
        .set_note_property(note_id, "quarantined", json!(false), legacy.updated_at + 1)
        .await
        .unwrap());
    let changed = raw.get_note(note_id).await.unwrap().unwrap();
    resume.wait().await;
    let error = tokio::time::timeout(std::time::Duration::from_secs(15), replay)
        .await
        .unwrap()
        .unwrap()
        .expect_err("changed quarantine evidence must refuse the repair");
    assert!(matches!(error, RuntimeError::InvalidInput(ref message)
        if message == "ingest: duplicate quarantine changed during retention repair"));
    assert_eq!(raw.get_note(note_id).await.unwrap().unwrap(), changed);
    assert_eq!(
        runtime
            .core()
            .attachments()
            .unwrap()
            .get_attachment(note_id, "quarantine-original")
            .await
            .unwrap()
            .unwrap()
            .content_ref,
        content_ref
    );

    assert!(raw
        .set_note_property(note_id, "quarantined", json!(true), changed.updated_at + 1)
        .await
        .unwrap());
    let ack = super::handle_ingest(
        &runtime,
        &InboxSignal::new(),
        Some(&ChannelIngestCapability::grant_for_direct_composition()),
        &Ok(None),
        &token,
        body,
        std::time::Duration::from_secs(60),
    )
    .await
    .expect("matching evidence must allow the same replay");
    assert_eq!(ack["deduplicated"], true);
    let repaired = raw.get_note(note_id).await.unwrap().unwrap();
    assert_eq!(
        repaired.properties.as_ref().unwrap()["quarantine_content_ref"],
        content_ref.to_string()
    );
    assert_eq!(
        repaired.properties.as_ref().unwrap()["unrelated"],
        json!({"kept": true})
    );
    assert!(repaired.expires_at.is_some());
}

/// A pre-retention row stored under the legacy IMAP key has a channel slug
/// and no `expires_at`. Replaying its quarantined message attaches the
/// original bytes to that row, so cleanup must be able to select the row
/// once retention elapses and release the attachment with it.
#[tokio::test]
async fn legacy_key_quarantine_replay_installs_a_deadline_that_cleanup_selects() {
    use std::sync::Arc;

    use khive_runtime::Namespace;
    use khive_storage::{BlobStore as _, Note};

    let retention = std::time::Duration::from_secs(14 * 24 * 60 * 60);
    let runtime = super::KhiveRuntime::memory().expect("in-memory runtime");
    let blob_root = tempfile::tempdir().expect("blob root");
    let blob_store = Arc::new(
        khive_db::stores::blob::FsBlobStore::new(blob_root.path().to_path_buf(), 0)
            .expect("blob store"),
    );
    let original_ref = blob_store
        .put(b"legacy quarantine original".to_vec())
        .await
        .expect("publish original");
    runtime
        .install_blob_store(blob_store)
        .expect("install blob store");
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let old_id = "imap:mail.example.com:17:legacy-retention";
    let new_id = "imap:mail.example.com:a@example.com:17:legacy-retention";

    // Plant the pre-retention row directly: matching slug, no deadline,
    // no attachment.
    let legacy_thread = uuid::Uuid::new_v4().as_hyphenated().to_string();
    let legacy = Note::new("local", "message", "legacy quarantine").with_properties(json!({
        "external_id": old_id,
        "direction": "inbound",
        "thread_id": legacy_thread,
        "channel_kind": "email",
        "channel_slug": "a@example.com",
        "quarantined": true,
    }));
    assert!(legacy.expires_at.is_none(), "fixture must have no deadline");
    let note_id = legacy.id;
    assert!(runtime
        .backend()
        .notes()
        .expect("backend notes")
        .try_insert_note(legacy)
        .await
        .expect("seed legacy row"));

    let signal = crate::inbox_signal::InboxSignal::new();
    let capability = khive_runtime::ChannelIngestCapability::grant_for_direct_composition();
    let replay_started = chrono::Utc::now().timestamp_micros();
    let ack = super::handle_ingest(
        &runtime,
        &signal,
        Some(&capability),
        &Ok(None),
        &token,
        json!({
            "from": "email:quarantine", "to": "local",
            "content": "replayed quarantine", "channel_kind": "email",
            "channel_slug": "a@example.com", "external_id": new_id,
            "legacy_external_id": old_id,
            "metadata": {
                "quarantined": true,
                "quarantine_content_ref": original_ref.to_string(),
            },
        }),
        retention,
    )
    .await
    .expect("legacy-key replay");
    assert_eq!(ack["deduplicated"], true);
    assert_eq!(ack["thread_id"], legacy_thread);
    assert_eq!(
        runtime
            .notes(&token)
            .expect("note store")
            .count_notes("local", Some("message"))
            .await
            .unwrap(),
        1,
        "the replay must repair the legacy row, not create a second one"
    );

    let attachments = runtime.core().attachments().expect("attachment store");
    assert_eq!(
        attachments
            .get_attachment(note_id, "quarantine-original")
            .await
            .expect("attachment lookup")
            .expect("repair roots the original")
            .content_ref,
        original_ref
    );
    let notes = runtime.notes(&token).expect("note store");
    let repaired = notes
        .get_note(note_id)
        .await
        .expect("row lookup")
        .expect("legacy row retained");
    let deadline = repaired
        .expires_at
        .expect("the repair must give the owning row a deadline");
    let retention_us = i64::try_from(retention.as_micros()).unwrap();
    assert!(
        deadline >= replay_started + retention_us,
        "deadline {deadline} must be replay time plus retention"
    );

    let cleanup = |as_of: i64| {
        super::handle_cleanup_expired_quarantine(
            &runtime,
            &token,
            json!({
                "channel_kind": "email",
                "channel_slug": "a@example.com",
                "as_of_micros": as_of,
            }),
            retention,
        )
    };
    assert_eq!(
        cleanup(replay_started).await.expect("early cleanup")["deleted"],
        0,
        "an unexpired row and its attachment must survive"
    );
    assert!(notes.get_note(note_id).await.unwrap().is_some());

    assert_eq!(
        cleanup(deadline + 1).await.expect("expired cleanup")["deleted"],
        1,
        "cleanup must select the repaired legacy row once retention elapses"
    );
    assert!(notes
        .get_note_including_deleted(note_id)
        .await
        .unwrap()
        .is_none());
    assert!(
        attachments
            .get_attachment(note_id, "quarantine-original")
            .await
            .expect("attachment lookup")
            .is_none(),
        "hard deletion must release the original's owner"
    );
}

/// Regression for the heartbeat lost-update race (khive #1753). Two
/// readers observe the SAME existing heartbeat row (deterministic: this
/// test is the only writer, so two sequential `get_note` calls before
/// either write are guaranteed to see one shared revision — no barrier
/// needed to force it). Each derives an independent poll outcome from
/// that snapshot, mirroring exactly what `handle_heartbeat` builds for a
/// "failure" vs. a "success" report, then the two writes commit through
/// the SAME `NoteStore::replace_note_if_unchanged` call `handle_heartbeat`
/// now uses. Before that guard, `handle_heartbeat` called
/// `store.upsert_note` unconditionally, so B's write would also succeed
/// and A's `consecutive_failures`/`last_error` update would be silently
/// discarded. This reddens if the write in `handle_heartbeat`'s `Some(snapshot)`
/// branch reverts to an unconditional `upsert_note`: the second
/// `replace_note_if_unchanged` call below would then need to be an
/// `upsert_note` too, which always returns `()`/succeeds, so the
/// `!... .unwrap()` assertion would fail to compile-flag the regression
/// and the final `consecutive_failures`/`last_failure_at` assertions
/// would observe B's fields instead of A's.
///
/// SCOPE: the race here is two direct `replace_note_if_unchanged` calls
/// against the STORE PRIMITIVE. `handle_heartbeat` IS invoked, once, at the
/// top, and only to seed the row — so reverting the handler's guarded
/// branch to an unconditional `upsert_note` changes the seed and not the
/// race, and this test stays green. That is measured: under exactly that
/// wiring mutation the only test that reddens is
/// `production_handle_heartbeat_refuses_concurrent_stale_writer`, which is
/// what covers the wiring. Both are required, neither substitutes for the
/// other.
#[tokio::test]
async fn concurrent_heartbeats_from_one_revision_only_one_survives() {
    let runtime = khive_runtime::KhiveRuntime::memory().expect("in-memory runtime");
    let token = runtime
        .authorize(khive_runtime::Namespace::parse("local").unwrap())
        .expect("authorize");

    super::handle_heartbeat(
        &runtime,
        &token,
        json!({
            "channel_kind": "email",
            "channel_slug": "race@example.com",
            "poll_interval_secs": 5,
            "outcome": "success",
        }),
    )
    .await
    .expect("seed heartbeat");

    let store = runtime.notes(&token).expect("note store");
    let id = super::heartbeat_note_id("local", "email", "race@example.com");
    let snapshot_for_a = store
        .get_note(id)
        .await
        .unwrap()
        .expect("seeded heartbeat row");
    let snapshot_for_b = store
        .get_note(id)
        .await
        .unwrap()
        .expect("seeded heartbeat row");
    assert_eq!(
        snapshot_for_a.updated_at, snapshot_for_b.updated_at,
        "both readers must observe the same pre-write revision for this to be a real race"
    );

    // Writer A: a "failure" report built from the shared snapshot —
    // mirrors handle_heartbeat's failure branch exactly.
    let mut note_a = snapshot_for_a.clone();
    let mut props_a = note_a.properties.clone().unwrap_or_else(|| json!({}));
    props_a["last_failure_at"] = json!("2026-08-26T00:00:00Z");
    props_a["consecutive_failures"] = json!(1);
    props_a["last_error"] =
        json!({"class": "timeout", "message": "boom", "at": "2026-08-26T00:00:00Z"});
    note_a.properties = Some(props_a);
    note_a.updated_at = snapshot_for_a.updated_at + 1;

    // Writer B: a "success" report ALSO derived from the SAME stale
    // snapshot — as if B's own internal `get_note` raced A's.
    let mut note_b = snapshot_for_b.clone();
    let mut props_b = note_b.properties.clone().unwrap_or_else(|| json!({}));
    props_b["last_success_at"] = json!("2026-08-26T00:00:01Z");
    props_b["consecutive_failures"] = json!(0);
    note_b.properties = Some(props_b);
    note_b.updated_at = snapshot_for_b.updated_at + 1;

    assert!(
        store
            .replace_note_if_unchanged(note_a, snapshot_for_a.updated_at, snapshot_for_a.deleted_at)
            .await
            .expect("writer A CAS query"),
        "the first committer from a shared revision must win"
    );
    assert!(
        !store
            .replace_note_if_unchanged(note_b, snapshot_for_b.updated_at, snapshot_for_b.deleted_at)
            .await
            .expect("writer B CAS query"),
        "the second committer from the SAME stale revision must be refused, not merged"
    );

    let final_note = store
        .get_note(id)
        .await
        .unwrap()
        .expect("heartbeat row still exists");
    let final_props = final_note.properties.expect("heartbeat properties");
    assert_eq!(
        final_props["consecutive_failures"],
        json!(1),
        "writer A's failure count must survive: {final_props:?}"
    );
    assert_eq!(
        final_props["last_failure_at"],
        json!("2026-08-26T00:00:00Z")
    );
    assert!(
            final_props.get("last_success_at") != Some(&json!("2026-08-26T00:00:01Z")),
            "writer B's success timestamp must not be silently merged into the persisted row: {final_props:?}"
        );
}

/// Same race as `concurrent_heartbeats_from_one_revision_only_one_survives`,
/// but driven entirely through the PRODUCTION entry point
/// (`handle_heartbeat`) rather than `replace_note_if_unchanged` directly.
/// This closes a gap the primitive-level test cannot: it would still pass
/// unchanged if `handle_heartbeat` were reverted to an unconditional
/// `upsert_note`, since it never invokes the handler at all. Uses
/// `race_seam::pause_after_read` (test-only, compiled out of non-test
/// builds) to force both concurrent callers to observe the identical
/// pre-write revision deterministically — no sleeps, no reliance on
/// scheduler ordering.
#[tokio::test]
async fn production_handle_heartbeat_refuses_concurrent_stale_writer() {
    let runtime =
        std::sync::Arc::new(khive_runtime::KhiveRuntime::memory().expect("in-memory runtime"));
    let token = runtime
        .authorize(khive_runtime::Namespace::parse("local").unwrap())
        .expect("authorize");

    super::handle_heartbeat(
        &runtime,
        &token,
        json!({
            "channel_kind": "email",
            "channel_slug": "production-race@example.com",
            "poll_interval_secs": 5,
            "outcome": "success",
        }),
    )
    .await
    .expect("seed heartbeat");

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));

    let writer_a = {
        let runtime = std::sync::Arc::clone(&runtime);
        let token = token.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        tokio::spawn(
            super::race_seam::AFTER_READ_BARRIER.scope(barrier, async move {
                super::handle_heartbeat(
                    &runtime,
                    &token,
                    json!({
                        "channel_kind": "email",
                        "channel_slug": "production-race@example.com",
                        "outcome": "failure",
                        "error_class": "timeout",
                        "error_message": "boom",
                    }),
                )
                .await
            }),
        )
    };
    let writer_b = {
        let runtime = std::sync::Arc::clone(&runtime);
        let token = token.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        tokio::spawn(
            super::race_seam::AFTER_READ_BARRIER.scope(barrier, async move {
                super::handle_heartbeat(
                    &runtime,
                    &token,
                    json!({
                        "channel_kind": "email",
                        "channel_slug": "production-race@example.com",
                        "poll_interval_secs": 9,
                        "outcome": "success",
                    }),
                )
                .await
            }),
        )
    };

    let result_a = writer_a.await.expect("writer A task");
    let result_b = writer_b.await.expect("writer B task");
    let successes = [result_a.is_ok(), result_b.is_ok()]
        .into_iter()
        .filter(|ok| *ok)
        .count();
    assert_eq!(
        successes, 1,
        "exactly one production caller must win the race; the other must be refused: \
             a={result_a:?} b={result_b:?}"
    );
    let refused = if result_a.is_err() {
        result_a
    } else {
        result_b
    };
    match &refused {
        Err(khive_runtime::RuntimeError::Khive(khive_error)) => {
            assert_eq!(
                khive_error.kind(),
                khive_types::ErrorKind::Conflict,
                "the losing production caller must surface a typed conflict, not \
                     silently overwrite: {refused:?}"
            );
        }
        other => panic!("expected a typed conflict error, got {other:?}"),
    }

    let store = runtime.notes(&token).expect("note store");
    let id = super::heartbeat_note_id("local", "email", "production-race@example.com");
    let final_note = store
        .get_note(id)
        .await
        .unwrap()
        .expect("heartbeat row still exists");
    let final_props = final_note.properties.expect("heartbeat properties");
    assert!(
        !(final_props.get("last_failure_at").is_some()
            && final_props["poll_interval_secs"] == json!(9)),
        "both racers' fields must never both land: that would mean the loser's stale \
             write silently succeeded: {final_props:?}"
    );
}

/// The same production race, but with NO row seeded first, so both callers
/// take the `None` arm.
///
/// `production_handle_heartbeat_refuses_concurrent_stale_writer` seeds the
/// row before racing, so it can only ever exercise the `Some(snapshot)`
/// CAS branch — it stays green against a build whose `None` arm is an
/// unconditional upsert. That is the first-write race: the heartbeat note
/// id is deterministic per channel, so two callers reporting a channel for
/// the first time both read absence, and an upsert resolves that by
/// overwriting, losing the first report with no error to either caller.
#[tokio::test]
async fn production_handle_heartbeat_refuses_concurrent_first_writer() {
    let runtime =
        std::sync::Arc::new(khive_runtime::KhiveRuntime::memory().expect("in-memory runtime"));
    let token = runtime
        .authorize(khive_runtime::Namespace::parse("local").unwrap())
        .expect("authorize");

    let id = super::heartbeat_note_id("local", "email", "first-write-race@example.com");
    // The premise this test rests on: nothing is seeded, so both racers
    // must take the `None` arm. Without this the test could silently
    // degrade into a copy of the seeded one.
    assert!(
        runtime
            .notes(&token)
            .expect("note store")
            .get_note(id)
            .await
            .expect("read the heartbeat row")
            .is_none(),
        "fixture premise: no heartbeat row may exist for this channel before the race, \
             otherwise both callers take the guarded `Some` arm and the first-write race is \
             never exercised"
    );

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));

    let writer_a = {
        let runtime = std::sync::Arc::clone(&runtime);
        let token = token.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        tokio::spawn(
            super::race_seam::AFTER_READ_BARRIER.scope(barrier, async move {
                super::handle_heartbeat(
                    &runtime,
                    &token,
                    json!({
                        "channel_kind": "email",
                        "channel_slug": "first-write-race@example.com",
                        "outcome": "failure",
                        "error_class": "timeout",
                        "error_message": "boom",
                    }),
                )
                .await
            }),
        )
    };
    let writer_b = {
        let runtime = std::sync::Arc::clone(&runtime);
        let token = token.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        tokio::spawn(
            super::race_seam::AFTER_READ_BARRIER.scope(barrier, async move {
                super::handle_heartbeat(
                    &runtime,
                    &token,
                    json!({
                        "channel_kind": "email",
                        "channel_slug": "first-write-race@example.com",
                        "poll_interval_secs": 9,
                        "outcome": "success",
                    }),
                )
                .await
            }),
        )
    };

    let result_a = writer_a.await.expect("writer A task");
    let result_b = writer_b.await.expect("writer B task");
    let successes = [result_a.is_ok(), result_b.is_ok()]
        .into_iter()
        .filter(|ok| *ok)
        .count();
    assert_eq!(
        successes, 1,
        "exactly one first-writer must win; the other must be refused rather than \
             silently replacing the winner's report: a={result_a:?} b={result_b:?}"
    );
    let refused = if result_a.is_err() {
        result_a
    } else {
        result_b
    };
    match &refused {
        Err(khive_runtime::RuntimeError::Khive(khive_error)) => {
            assert_eq!(
                khive_error.kind(),
                khive_types::ErrorKind::Conflict,
                "the losing first-writer must surface a typed conflict: {refused:?}"
            );
        }
        other => panic!("expected a typed conflict error, got {other:?}"),
    }

    let store = runtime.notes(&token).expect("note store");
    let final_note = store
        .get_note(id)
        .await
        .unwrap()
        .expect("the winner's heartbeat row must exist");
    let final_props = final_note.properties.expect("heartbeat properties");
    // The winner's report must be intact, not a blend of both. Each racer
    // reports a distinguishing field the other never sends.
    let is_a = final_props.get("last_failure_at").is_some();
    let is_b = final_props["poll_interval_secs"] == json!(9);
    assert!(
        is_a ^ is_b,
        "the surviving row must be exactly one racer's report, never both racers' fields \
             merged, which is what a losing write silently succeeding would produce: \
             {final_props:?}"
    );
}

/// A heartbeat's replacement revision must strictly advance past the
/// existing row's own `updated_at`, not just past `Utc::now()`: two
/// heartbeats landing in the same stored microsecond, or a backward
/// wall-clock step, are the SAME code path as this test forces (the
/// fix's `max(now, snapshot+1)` does not distinguish "now == snapshot"
/// from "now < snapshot" — both take the `snapshot+1` branch). Before the
/// fix, `handle_heartbeat` derived the replacement revision straight from
/// `Utc::now().timestamp_micros()`, so forcing the stored snapshot ahead
/// of wall-clock time reproduces both scenarios deterministically without
/// a clock-injection seam.
///
/// SCOPE: this is a revision-clamp test, NOT CAS regression coverage. Its
/// assertion is that the heartbeat write SUCCEEDS, which an unconditional
/// `upsert_note` also satisfies, so it stays green if the
/// `updated_at = ?13` / `?10 > updated_at` guard is dropped entirely. The
/// guard's regression coverage is
/// `concurrent_heartbeats_from_one_revision_only_one_survives` (primitive)
/// and `production_handle_heartbeat_refuses_concurrent_stale_writer`
/// (production wiring); do not count this test toward it.
#[tokio::test]
async fn handle_heartbeat_does_not_false_conflict_when_snapshot_is_ahead_of_wall_clock() {
    let runtime = khive_runtime::KhiveRuntime::memory().expect("in-memory runtime");
    let token = runtime
        .authorize(khive_runtime::Namespace::parse("local").unwrap())
        .expect("authorize");

    super::handle_heartbeat(
        &runtime,
        &token,
        json!({
            "channel_kind": "email",
            "channel_slug": "clock-skew@example.com",
            "outcome": "success",
        }),
    )
    .await
    .expect("seed heartbeat");

    let store = runtime.notes(&token).expect("note store");
    let id = super::heartbeat_note_id("local", "email", "clock-skew@example.com");
    let seeded = store
        .get_note(id)
        .await
        .unwrap()
        .expect("seeded heartbeat row");

    // Force the stored revision far ahead of any `Utc::now()` the next
    // handler call will observe — the same condition as an equal-
    // microsecond write or a backward clock step.
    let future_updated_at = seeded.updated_at + 60_000_000; // +60s
    let mut ahead = seeded.clone();
    ahead.updated_at = future_updated_at;
    let forced = store
        .replace_note_if_unchanged(ahead, seeded.updated_at, seeded.deleted_at)
        .await
        .expect("test setup: force the snapshot ahead of wall-clock time");
    assert!(
        forced,
        "test setup CAS must succeed against the freshly seeded row"
    );

    let result = super::handle_heartbeat(
        &runtime,
        &token,
        json!({
            "channel_kind": "email",
            "channel_slug": "clock-skew@example.com",
            "outcome": "failure",
            "error_class": "timeout",
            "error_message": "boom",
        }),
    )
    .await;
    assert!(
        result.is_ok(),
        "a heartbeat with no competing writer must not be refused just because the \
             stored revision is at or ahead of wall-clock now: {result:?}"
    );

    let final_note = store
        .get_note(id)
        .await
        .unwrap()
        .expect("heartbeat row still exists");
    assert!(
        final_note.updated_at > future_updated_at,
        "the replacement revision must still strictly advance past the forced-ahead \
             snapshot: {final_note:?}"
    );
    let final_props = final_note.properties.expect("heartbeat properties");
    assert_eq!(
        final_props["consecutive_failures"],
        json!(1),
        "the accepted write must actually be the failure report just sent: {final_props:?}"
    );
}

#[test]
fn channel_stalled_uses_strict_three_interval_threshold() {
    let props = json!({
        "poll_interval_secs": 5,
        "last_poll_attempt_at": "2026-08-01T12:00:00Z",
        "consecutive_failures": 0,
    });
    let at_threshold = chrono::DateTime::parse_from_rfc3339("2026-08-01T12:00:15Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let overdue = chrono::DateTime::parse_from_rfc3339("2026-08-01T12:00:15.001Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    assert_eq!(channel_stalled(&props, &at_threshold), Some(false));
    assert_eq!(channel_stalled(&props, &overdue), Some(true));
}

#[test]
fn channel_stalled_requires_valid_consecutive_failures() {
    let as_of = chrono::DateTime::parse_from_rfc3339("2026-08-01T12:01:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let mut props = json!({
        "poll_interval_secs": 5,
        "last_poll_attempt_at": "2026-08-01T12:00:00Z",
    });

    assert_eq!(channel_stalled(&props, &as_of), None);

    for malformed in [json!("1"), json!(-1)] {
        props["consecutive_failures"] = malformed;
        assert_eq!(channel_stalled(&props, &as_of), None);
    }
}

#[test]
fn comm_write_response_reports_atomic_note_embedding_truncation() {
    let mut response = json!({"id": "abc123"});
    add_embedding_truncation_warning(
        &mut response,
        &khive_runtime::retrieval::EmbeddingTruncationReport {
            truncated: 2,
            discarded_bytes: 18,
        },
    );
    assert_eq!(
        response["warnings"],
        json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING])
    );
}

// #606: a delimiter-joined
// `format!("...:{a}:{b}:{c}")` id encoding is not injective once
// components may themselves contain `:` — these two distinct triples
// both produced `"khive:channel_health:a:b:c:d"` under the pre-fix
// scheme (`namespace:kind:slug` == `"a:b"` + `"c"` + `"d"` joins to the
// same string as `"a"` + `"b:c"` + `"d"`). The JSON-array encoding must
// keep them distinct.
#[test]
fn heartbeat_note_id_does_not_collide_on_delimiter_bearing_components() {
    let a = heartbeat_note_id("a:b", "c", "d");
    let b = heartbeat_note_id("a", "b:c", "d");
    assert_ne!(
        a, b,
        "distinct (namespace, channel_kind, channel_slug) triples with \
             colons inside a component must never hash to the same id"
    );
}

#[test]
fn heartbeat_note_id_is_deterministic() {
    assert_eq!(
        heartbeat_note_id("local", "email", "recipient@example.com"),
        heartbeat_note_id("local", "email", "recipient@example.com"),
    );
}

#[test]
fn candidates_bare_input_adds_bracketed_form() {
    // A bracket-free correlation key (as delivered by mail_parser) must also
    // try the wire form so it matches an outbound `<id@domain>` external_id.
    assert_eq!(
        message_id_match_candidates("sent-msg@khive.ai"),
        vec![
            "sent-msg@khive.ai".to_string(),
            "<sent-msg@khive.ai>".to_string(),
        ],
    );
}

#[test]
fn candidates_bracketed_input_adds_bare_form() {
    // Reverse direction: a bracketed correlation key must also try the bare
    // form so it matches a stored bracket-free external_id. Guards the `else`
    // branch, which no ingest test exercises directly.
    assert_eq!(
        message_id_match_candidates("<sent-msg@khive.ai>"),
        vec![
            "<sent-msg@khive.ai>".to_string(),
            "sent-msg@khive.ai".to_string(),
        ],
    );
}

#[test]
fn wrap_message_id_adds_brackets_when_absent() {
    assert_eq!(wrap_message_id("id@example.com"), "<id@example.com>");
}

#[test]
fn wrap_message_id_leaves_already_bracketed_form_unchanged() {
    assert_eq!(
        wrap_message_id("<id@example.com>"),
        "<id@example.com>",
        "must not double-wrap an already-bracketed id"
    );
}

#[test]
fn wrap_message_id_trims_whitespace() {
    assert_eq!(wrap_message_id("  id@example.com  "), "<id@example.com>");
}

#[test]
fn parent_wire_message_id_reads_wire_message_id_for_inbound_parent() {
    let props = json!({
        "direction": "inbound",
        "wire_message_id": "inbound-msg@example.com",
        "external_id": "imap:host:1:42",
    });
    assert_eq!(
        parent_wire_message_id(&props).as_deref(),
        Some("<inbound-msg@example.com>"),
        "inbound parent must use wire_message_id, never the IMAP-key external_id"
    );
}

#[test]
fn parent_wire_message_id_reads_external_id_for_outbound_parent() {
    let props = json!({
        "direction": "outbound",
        "external_id": "<outbound-msg@khive.ai>",
    });
    assert_eq!(
        parent_wire_message_id(&props).as_deref(),
        Some("<outbound-msg@khive.ai>"),
        "outbound parent must reuse its self-minted external_id verbatim"
    );
}

#[test]
fn parent_wire_message_id_none_when_outbound_parent_has_no_external_id() {
    let props = json!({ "direction": "outbound" });
    assert_eq!(parent_wire_message_id(&props), None);
}

#[test]
fn parent_wire_message_id_none_when_inbound_parent_has_no_wire_message_id() {
    let props = json!({ "direction": "inbound" });
    assert_eq!(parent_wire_message_id(&props), None);
}

#[test]
fn parent_wire_message_id_none_for_empty_properties() {
    assert_eq!(parent_wire_message_id(&json!({})), None);
}

#[test]
fn parent_references_chain_reads_wire_references_for_inbound_parent() {
    let props = json!({
        "direction": "inbound",
        "wire_references": "<grandparent1@example.com> <parent123@example.com>",
        "references_chain": "should-not-be-read@example.com",
    });
    assert_eq!(
        parent_references_chain(&props),
        Some("<grandparent1@example.com> <parent123@example.com>"),
        "inbound parent must use wire_references, never the outbound-only references_chain"
    );
}

#[test]
fn parent_references_chain_reads_references_chain_for_outbound_parent() {
    let props = json!({
        "direction": "outbound",
        "references_chain": "<grandparent1@example.com> <parent123@example.com>",
        "wire_references": "should-not-be-read@example.com",
    });
    assert_eq!(
        parent_references_chain(&props),
        Some("<grandparent1@example.com> <parent123@example.com>"),
        "outbound parent must use references_chain, never the inbound-only wire_references"
    );
}

#[test]
fn parent_references_chain_none_when_outbound_parent_has_no_chain() {
    let props = json!({ "direction": "outbound" });
    assert_eq!(parent_references_chain(&props), None);
}

#[test]
fn parent_references_chain_none_when_inbound_parent_has_no_chain() {
    let props = json!({ "direction": "inbound" });
    assert_eq!(parent_references_chain(&props), None);
}

#[test]
fn parent_references_chain_none_for_empty_properties() {
    assert_eq!(parent_references_chain(&json!({})), None);
}

#[test]
fn parent_references_chain_none_for_blank_chain() {
    let props = json!({ "direction": "inbound", "wire_references": "   " });
    assert_eq!(
        parent_references_chain(&props),
        None,
        "a whitespace-only stored chain must resolve to None, not an empty References token"
    );
}

#[test]
fn sanitize_reference_token_wraps_bare_id() {
    assert_eq!(
        sanitize_reference_token("id@example.com"),
        Some("<id@example.com>".to_string())
    );
}

#[test]
fn sanitize_reference_token_leaves_bracketed_id_unchanged() {
    assert_eq!(
        sanitize_reference_token("<id@example.com>"),
        Some("<id@example.com>".to_string())
    );
}

#[test]
fn sanitize_reference_token_rejects_crlf() {
    assert_eq!(
        sanitize_reference_token("id@example.com\r\nBcc: evil"),
        None
    );
    assert_eq!(sanitize_reference_token("id@example.com\nBcc: evil"), None);
}

#[test]
fn sanitize_reference_token_rejects_missing_at_sign() {
    assert_eq!(sanitize_reference_token("not-a-message-id"), None);
}

#[test]
fn sanitize_reference_token_rejects_empty() {
    assert_eq!(sanitize_reference_token(""), None);
    assert_eq!(sanitize_reference_token("   "), None);
}

#[test]
fn sanitize_reference_token_rejects_embedded_angle_brackets() {
    assert_eq!(
        sanitize_reference_token("a@example.com<b@example.com>"),
        None
    );
}

#[test]
fn build_references_header_extends_existing_chain_of_two_or_more() {
    // Core spec: a reply whose parent has an existing References
    // chain of 2+ ids must produce chain + parent Message-ID, not just the
    // immediate parent.
    let chain = Some("<grandparent1@example.com> <grandparent2@example.com>");
    assert_eq!(
        build_references_header(chain, "<parent123@example.com>"),
        "<grandparent1@example.com> <grandparent2@example.com> <parent123@example.com>"
    );
}

#[test]
fn build_references_header_falls_back_to_parent_message_id_when_no_chain() {
    assert_eq!(
        build_references_header(None, "<parent123@example.com>"),
        "<parent123@example.com>"
    );
}

#[test]
fn build_references_header_skips_malformed_token_in_chain() {
    // A malformed token embedded in a stored chain (e.g. corrupted data, or a
    // CRLF injection attempt) must be skipped, not propagated into the header.
    let chain = Some("<good1@example.com> not-a-message-id <good2@example.com>");
    assert_eq!(
        build_references_header(chain, "<parent123@example.com>"),
        "<good1@example.com> <good2@example.com> <parent123@example.com>"
    );
}

#[test]
fn build_references_header_bare_chain_tokens_get_wrapped() {
    // Chain tokens stored bracket-free (e.g. from an inbound parent's
    // wire_references, since mail_parser strips brackets) must be
    // normalized to wire form, matching wrap_message_id's contract.
    let chain = Some("bare1@example.com bare2@example.com");
    assert_eq!(
        build_references_header(chain, "<parent123@example.com>"),
        "<bare1@example.com> <bare2@example.com> <parent123@example.com>"
    );
}

#[test]
fn build_references_header_dedups_when_chain_already_contains_parent_id() {
    // A stored chain that already contains an equivalent of the parent's own
    // id (e.g. tainted/legacy data) must not yield a literal duplicate: the
    // parent id keeps its original position in the chain (first-seen order)
    // and is not appended a second time at the end.
    let chain = Some("<root1@example.com> <parent123@example.com> <root2@example.com>");
    assert_eq!(
        build_references_header(chain, "<parent123@example.com>"),
        "<root1@example.com> <parent123@example.com> <root2@example.com>"
    );
}

#[test]
fn build_references_header_dedups_bare_and_bracketed_forms_as_equivalent() {
    // The de-dup comparison must strip brackets before comparing, not just
    // compare byte-identical strings -- otherwise a bracket-free chain token
    // and a bracketed parent_message_id (or vice versa) would both survive
    // into the header as two "different" entries for the same id.
    let chain = Some("<parent123@example.com>");
    assert_eq!(
        build_references_header(chain, "parent123@example.com"),
        "<parent123@example.com>"
    );
}

// read_response's three arms are unit-tested directly because the
// `Ok(false)` case (a live row vanishing between handle_read's `get_note`
// and its `set_note_property` call) cannot be arranged honestly
// through the public dispatch path: the two calls are sequential within
// one handler invocation with no seam to inject a concurrent delete.

#[test]
fn read_response_ok_true_reports_read_and_patched_properties() {
    let original = json!({ "direction": "inbound", "read": false });
    let patched = json!({ "direction": "inbound", "read": true });
    let resp = read_response(
        "abc123".to_string(),
        "full-uuid".to_string(),
        Ok(true),
        Some(original),
        patched.clone(),
    );
    assert_eq!(resp["id"], json!("abc123"));
    assert_eq!(resp["full_id"], json!("full-uuid"));
    assert_eq!(resp["status"], json!("success"));
    assert_eq!(resp["read"], json!(true));
    assert_eq!(resp["properties"], patched);
    assert!(
        resp.get("mark_error").is_none(),
        "a successful mark must not carry mark_error; got {resp}"
    );
}

#[test]
fn read_response_ok_false_degrades_without_claiming_the_patch_landed() {
    let original = json!({ "direction": "inbound", "read": false });
    let patched = json!({ "direction": "inbound", "read": true });
    let resp = read_response(
        "abc123".to_string(),
        "full-uuid".to_string(),
        Ok(false),
        Some(original.clone()),
        patched,
    );
    assert_eq!(resp["id"], json!("abc123"));
    assert_eq!(resp["full_id"], json!("full-uuid"));
    assert_eq!(resp["status"], json!("failed"));
    assert_eq!(resp["read"], json!(false));
    assert_eq!(
        resp["mark_error"],
        json!("no live row updated"),
        "got {resp}"
    );
    assert_eq!(
        resp["properties"], original,
        "must report the ORIGINAL stored properties, never the attempted \
             patch, when the write did not land; got {resp}"
    );
}

#[test]
fn read_response_ok_false_preserves_stored_null_properties() {
    let patched = json!({ "read": true });
    let resp = read_response(
        "abc123".to_string(),
        "full-uuid".to_string(),
        Ok(false),
        None,
        patched,
    );
    assert_eq!(resp["id"], json!("abc123"));
    assert_eq!(resp["full_id"], json!("full-uuid"));
    assert_eq!(resp["status"], json!("failed"));
    assert_eq!(resp["read"], json!(false));
    assert_eq!(
        resp["properties"],
        Value::Null,
        "a stored SQL-NULL properties column must round-trip as JSON \
             null, never as {{}}; got {resp}"
    );
}

#[test]
fn read_response_err_degrades_and_reports_the_error_string() {
    let original = json!({ "direction": "inbound", "read": false });
    let patched = json!({ "direction": "inbound", "read": true });
    let err = StorageError::Timeout {
        operation: "set_note_property".into(),
    };
    let err_text = err.to_string();
    let resp = read_response(
        "abc123".to_string(),
        "full-uuid".to_string(),
        Err(err),
        Some(original.clone()),
        patched,
    );
    assert_eq!(resp["id"], json!("abc123"));
    assert_eq!(resp["full_id"], json!("full-uuid"));
    assert_eq!(resp["status"], json!("failed"));
    assert_eq!(resp["read"], json!(false));
    assert_eq!(resp["mark_error"], json!(err_text));
    assert_eq!(
        resp["properties"], original,
        "must report the ORIGINAL stored properties on a write error; got {resp}"
    );
}

#[test]
fn read_response_err_preserves_stored_null_properties() {
    let patched = json!({ "read": true });
    let err = StorageError::Timeout {
        operation: "set_note_property".into(),
    };
    let resp = read_response(
        "abc123".to_string(),
        "full-uuid".to_string(),
        Err(err),
        None,
        patched,
    );
    assert_eq!(resp["id"], json!("abc123"));
    assert_eq!(resp["full_id"], json!("full-uuid"));
    assert_eq!(resp["status"], json!("failed"));
    assert_eq!(resp["read"], json!(false));
    assert_eq!(
        resp["properties"],
        Value::Null,
        "a stored SQL-NULL properties column must round-trip as JSON \
             null, never as {{}}; got {resp}"
    );
}

#[test]
fn read_response_side_effects_unknown_reports_unknown_not_failed() {
    let original = json!({ "direction": "inbound", "read": false });
    let patched = json!({ "direction": "inbound", "read": true });
    let err = StorageError::writer_task_terminated(
        khive_storage::WriterTaskRequestState::SideEffectsUnknown,
    );
    let err_text = err.to_string();
    let resp = read_response(
        "abc123".to_string(),
        "full-uuid".to_string(),
        Err(err),
        Some(original.clone()),
        patched,
    );
    assert_eq!(resp["id"], json!("abc123"));
    assert_eq!(resp["full_id"], json!("full-uuid"));
    assert_eq!(
        resp["status"],
        json!("unknown"),
        "a write whose seam terminated after acceptance may already have \
             landed and must not be reported as a definite failure; got {resp}"
    );
    assert_eq!(
        resp["read"],
        Value::Null,
        "an indeterminate outcome is neither true nor false; got {resp}"
    );
    assert_eq!(resp["mark_error"], json!(err_text));
    assert_eq!(resp["properties"], original);
}

#[test]
fn read_response_writer_task_terminated_rolled_back_still_reports_failed() {
    let original = json!({ "direction": "inbound", "read": false });
    let patched = json!({ "direction": "inbound", "read": true });
    let err = StorageError::writer_task_terminated(
        khive_storage::WriterTaskRequestState::TransactionRolledBack,
    );
    let resp = read_response(
        "abc123".to_string(),
        "full-uuid".to_string(),
        Err(err),
        Some(original),
        patched,
    );
    assert_eq!(
        resp["status"],
        json!("failed"),
        "a proven rollback is a definite failure, not an indeterminate \
             outcome; got {resp}"
    );
    assert_eq!(resp["read"], json!(false));
}

#[test]
fn read_body_is_exposed_only_for_a_successful_mark() {
    let message = json!({"subject": "private subject", "content": "private body"});
    for status in ["failed", "unknown"] {
        let result = read_result_with_body(
            json!({"status": status, "read": Value::Null}),
            Some(message.clone()),
        );
        assert!(result.get("subject").is_none(), "{result}");
        assert!(result.get("content").is_none(), "{result}");
    }
    let success = read_result_with_body(json!({"status": "success", "read": true}), Some(message));
    assert_eq!(success["subject"], "private subject");
    assert_eq!(success["content"], "private body");
}

#[test]
fn bulk_read_response_reports_success_partial_failed_and_unknown_statuses() {
    let response = |outcomes: &[bool]| {
        bulk_read_response(
            outcomes.len(),
            outcomes
                .iter()
                .map(|read| json!({ "read": read }))
                .collect(),
        )
    };

    assert_eq!(response(&[true, true])["status"], "success");
    assert_eq!(response(&[true, false])["status"], "partial");
    assert_eq!(response(&[false, false])["status"], "failed");

    let mixed = bulk_read_response(
        3,
        vec![
            json!({ "status": "success", "read": true }),
            json!({ "status": "failed", "read": false }),
            json!({ "status": "unknown", "read": Value::Null }),
        ],
    );
    assert_eq!(mixed["status"], "partial");
    assert_eq!(mixed["marked_count"], 1);
    assert_eq!(mixed["failed_count"], 1);
    assert_eq!(mixed["unknown_count"], 1);

    let all_unknown = bulk_read_response(
        2,
        vec![
            json!({ "status": "unknown", "read": Value::Null }),
            json!({ "status": "unknown", "read": Value::Null }),
        ],
    );
    assert_eq!(all_unknown["status"], "unknown");
    assert_eq!(all_unknown["marked_count"], 0);
    assert_eq!(all_unknown["failed_count"], 0);
    assert_eq!(all_unknown["unknown_count"], 2);
}

// Regression for the bulk-read lost-update: prevalidation (`validate_read_target`)
// snapshots a `Note`, but bulk read's validate-then-mark window can span up to
// 500 targets, during which another writer can change an unrelated property.
// `mark_read_target` must never write that stale snapshot's `properties` back —
// only the `read` key may change, and any property that landed after the
// snapshot but before the mark must survive.
#[tokio::test]
async fn mark_read_target_preserves_a_property_written_after_prevalidation() {
    use khive_runtime::{AllowAllGate, BackendId, Namespace, RuntimeConfig};
    use khive_storage::note::Note;
    use uuid::Uuid;

    let ns = format!("mark-read-cas-{}", Uuid::new_v4().simple());
    let runtime = super::KhiveRuntime::new(RuntimeConfig {
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
        db_path: None,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::parse(&ns).unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: std::sync::Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "comm".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    let token = runtime
        .authorize(Namespace::parse(&ns).unwrap())
        .expect("authorize");
    let store = runtime.notes(&token).expect("notes store");

    let id = Uuid::new_v4();
    let created_at = chrono::Utc::now().timestamp_micros();
    store
        .upsert_note(Note {
            version: 1,
            key: None,
            id,
            namespace: ns.clone(),
            kind: "message".to_string(),
            status: "active".to_string(),
            name: None,
            content: "concurrency regression".to_string(),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(json!({
                "direction": "inbound",
                // `actor_id: None` in the config below resolves to the
                // anonymous actor, whose id is always "local" regardless
                // of namespace — see `khive_runtime::actor_identity::resolve_actor`.
                "to_actor": "local",
                "read": false,
            })),
            created_at,
            updated_at: created_at,
            deleted_at: None,
        })
        .await
        .expect("insert message");

    // Prevalidation snapshot — this is what a bulk read's validate phase
    // would have captured for this target before iterating the rest of a
    // (possibly large) id list.
    let (validated_id, stale_note) = validate_read_target(&runtime, &token, &id.to_string())
        .await
        .expect("prevalidation");
    assert_eq!(validated_id, id);

    // Simulate a concurrent write landing after prevalidation but before
    // this target's mark step: another property changes, `read` stays false.
    let concurrent_updated_at = created_at + 1;
    store
        .update_note_properties(
            id,
            Some(json!({
                "direction": "inbound",
                "to_actor": stale_note.properties.as_ref().unwrap()["to_actor"].clone(),
                "read": false,
                "flagged": true,
            })),
            concurrent_updated_at,
        )
        .await
        .expect("concurrent property write");

    // Mark using the now-stale snapshot, exactly as the bulk mark loop does.
    let result = mark_read_target(&runtime, &token, id, stale_note)
        .await
        .expect("mark_read_target");
    assert_eq!(result["read"], json!(true), "got {result}");

    let stored = store
        .get_note(id)
        .await
        .expect("get_note")
        .expect("note still present");
    let props = stored.properties.expect("properties present");
    assert_eq!(
        props["read"],
        json!(true),
        "the mark itself must still land; got {props}"
    );
    assert_eq!(
        props["flagged"],
        json!(true),
        "a property written after prevalidation but before the mark must \
             survive — the mark must never write back the stale snapshot; got {props}"
    );
}

// Regression: a message whose stored `properties` document is not a JSON
// object (scalar or array) must never be reported as read. `json_set`
// silently leaves such a document unchanged while still returning it, so
// without a non-object guard the `UPDATE` would still match the row and
// `comm.read` would falsely report `read: true` for a patch that stored
// nothing.
#[tokio::test]
async fn mark_read_target_reports_unread_for_non_object_properties() {
    use khive_runtime::{AllowAllGate, BackendId, Namespace, RuntimeConfig};
    use khive_storage::note::Note;
    use uuid::Uuid;

    for (case, properties) in [
        ("scalar", json!(1)),
        ("array", json!(["not", "an", "object"])),
    ] {
        let ns = format!("mark-read-non-object-{case}-{}", Uuid::new_v4().simple());
        let runtime = super::KhiveRuntime::new(RuntimeConfig {
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            web: Default::default(),
            telemetry: Default::default(),
            mounts: Vec::new(),
            brain: Default::default(),
            git_write: Default::default(),
            display_timezone: khive_runtime::config::resolve_default_display_timezone(),
            events_split: None,
            db_path: None,
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::parse(&ns).unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: std::sync::Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "comm".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
            exec: Default::default(),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        })
        .expect("in-memory runtime");
        let token = runtime
            .authorize(Namespace::parse(&ns).unwrap())
            .expect("authorize");
        let store = runtime.notes(&token).expect("notes store");

        let id = Uuid::new_v4();
        let created_at = chrono::Utc::now().timestamp_micros();
        let note = Note {
            version: 1,
            key: None,
            id,
            namespace: ns.clone(),
            kind: "message".to_string(),
            status: "active".to_string(),
            name: None,
            content: format!("{case} properties"),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(properties.clone()),
            created_at,
            updated_at: created_at,
            deleted_at: None,
        };
        store
            .upsert_note(note.clone())
            .await
            .expect("insert message");

        let result = mark_read_target(&runtime, &token, id, note)
            .await
            .expect("mark_read_target");
        assert_eq!(
            result["read"],
            json!(false),
            "{case} properties document must not be reported as read; got {result}"
        );
        assert_eq!(
            result["properties"], properties,
            "{case} properties must round-trip unchanged; got {result}"
        );

        let stored = store
            .get_note(id)
            .await
            .expect("get_note")
            .expect("note still present");
        assert_eq!(
            stored.properties,
            Some(properties),
            "{case} properties must remain unchanged in storage"
        );
        assert_eq!(
            stored.updated_at, created_at,
            "{case} updated_at must not advance when the patch is refused"
        );
    }
}

fn note_with_thread_id(thread_id: Value) -> Note {
    Note::new("local", "message", "body").with_properties(json!({ "thread_id": thread_id }))
}

fn note_without_thread_id() -> Note {
    Note::new("local", "message", "body").with_properties(json!({}))
}

#[test]
fn send_response_thread_id_returns_stored_value_when_present() {
    let note = note_with_thread_id(json!("stored-thread-root"));
    let resolved = send_response_thread_id(Some("supplied-root"), &note)
        .expect("a stored thread_id is authoritative");
    assert_eq!(resolved, "stored-thread-root");
}

#[test]
fn send_response_thread_id_roots_new_thread_when_unsupplied() {
    let note = note_without_thread_id();
    let resolved =
        send_response_thread_id(None, &note).expect("a root send reports the note's own UUID");
    assert_eq!(resolved, note.id.as_hyphenated().to_string());
}

#[test]
fn send_response_thread_id_treats_empty_stored_value_as_absent() {
    let note = note_with_thread_id(json!(""));
    let resolved = send_response_thread_id(None, &note)
        .expect("an empty stored value must not surface as an empty thread_id");
    assert_eq!(resolved, note.id.as_hyphenated().to_string());
}

#[test]
fn send_response_thread_id_fails_closed_on_missing_stored_value_after_supply() {
    let note = note_without_thread_id();
    let err = send_response_thread_id(Some("supplied-root"), &note)
        .expect_err("silently rooting a new thread would corrupt the caller's continuation");
    let message = err.to_string();
    assert!(
        message.contains("without the caller-supplied thread_id"),
        "{message}"
    );
}

#[test]
fn send_response_thread_id_fails_closed_on_empty_stored_value_after_supply() {
    let note = note_with_thread_id(json!(""));
    let err = send_response_thread_id(Some("supplied-root"), &note)
        .expect_err("an empty stored value is not a persisted root");
    let message = err.to_string();
    assert!(
        message.contains("without the caller-supplied thread_id"),
        "{message}"
    );
}
