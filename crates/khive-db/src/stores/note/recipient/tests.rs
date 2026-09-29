use super::*;
use crate::pool::PoolConfig;
use crate::StorageBackend;
use khive_storage::{DeleteMode, NoteStore};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

fn fixture() -> (StorageBackend, RecipientCommit) {
    let backend = StorageBackend::memory().unwrap();
    crate::run_migrations(backend.pool().writer().unwrap().conn_mut()).unwrap();
    let sender = Uuid::new_v4().to_string();
    let logical = Uuid::new_v4();
    let attempt = Uuid::new_v4();
    let mut note = Note::new("local", "message", "hello");
    note.properties = Some(
        serde_json::json!({"from_actor":format!("khive1:example/{sender}"),"to_actor":"lambda:receiver","direction":"inbound"}),
    );
    let binding = serde_json::json!({"protocol_version":1,"sender_agent_id":sender,"recipient_agent_id":Uuid::new_v4(),"logical_message_id":logical,"recipient_device_id":Uuid::new_v4(),"recipient_key_epoch":1,"contact_generation":1,"delivery_attempt_id":attempt});
    (
        backend,
        RecipientCommit {
            note,
            binding,
            sender_agent_id: sender,
            logical_message_id: logical,
            delivery_attempt_id: attempt,
            disposition: RecipientDisposition::Stored,
            quarantine: None,
            correlation: None,
        },
    )
}
fn counts(backend: &StorageBackend) -> [i64; 4] {
    let guard = backend.pool().writer().unwrap();
    [
        "notes",
        "comm_recipient_replay",
        "comm_ack_work",
        "comm_recipient_quarantine",
    ]
    .map(|table| {
        guard
            .conn()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    })
}
#[tokio::test]
async fn sequential_and_concurrent_duplicates_write_one_note() {
    let (backend, first) = fixture();
    let store = RecipientTransportStore::new(backend.pool_arc());
    let mut second = first.clone();
    second.note.id = Uuid::new_v4();
    let (a, b) = tokio::join!(store.commit(first.clone()), store.commit(second));
    assert_ne!(a.unwrap().created, b.unwrap().created);
    assert!(!store.commit(first).await.unwrap().created);
    assert_eq!(
        counts(&backend),
        [1, 1, 1, 0],
        "duplicate must not write another note or ack"
    );
}
#[tokio::test]
async fn replay_survives_soft_and_hard_deletion_and_acks_new_attempt() {
    for mode in [DeleteMode::Soft, DeleteMode::Hard] {
        let (backend, mut first) = fixture();
        let store = RecipientTransportStore::new(backend.pool_arc());
        let result = store.commit(first.clone()).await.unwrap();
        SqlNoteStore::new(backend.pool_arc(), false)
            .delete_note(result.note_id, mode)
            .await
            .unwrap();
        first.delivery_attempt_id = Uuid::new_v4();
        first.binding["delivery_attempt_id"] = serde_json::json!(first.delivery_attempt_id);
        first.disposition = RecipientDisposition::Quarantined;
        first.quarantine = Some(QuarantineRecord {
            reason: QuarantineReason::InvalidMessage,
            delivery_item: b"{\"opaque\":true}".to_vec(),
        });
        let duplicate = store
            .commit(first.clone())
            .await
            .expect("replay after note deletion must not recreate note");
        assert!(
            !duplicate.created,
            "replay after note deletion must not recreate note"
        );
        assert_eq!(
            duplicate.disposition,
            RecipientDisposition::Stored,
            "first disposition must survive replay"
        );
        assert!(!store.commit(first).await.unwrap().created);
        assert_eq!(
            counts(&backend)[1..],
            [1, 2, 0],
            "new delivery attempt must add exactly one ack"
        );
    }
}
#[tokio::test]
async fn failure_before_ack_rolls_back_message_and_quarantine() {
    for quarantine in [false, true] {
        let (backend, mut first) = fixture();
        let store = RecipientTransportStore::new(backend.pool_arc());
        if quarantine {
            first.disposition = RecipientDisposition::Quarantined;
            first.note.content = "Quarantined authenticated message".into();
            first.quarantine = Some(QuarantineRecord {
                reason: QuarantineReason::InvalidPlaintext,
                delivery_item: b"{ \"opaque\": true }".to_vec(),
            });
        }
        backend.pool().writer().unwrap().conn().execute_batch("CREATE TRIGGER fail_ack BEFORE INSERT ON comm_ack_work BEGIN SELECT RAISE(ABORT,'injected ack failure'); END;").unwrap();
        assert!(store.commit(first.clone()).await.is_err());
        assert_eq!(
            counts(&backend),
            [0, 0, 0, 0],
            "ack failure must roll back every ingest row"
        );
        backend
            .pool()
            .writer()
            .unwrap()
            .conn()
            .execute_batch("DROP TRIGGER fail_ack")
            .unwrap();
        let result = store.commit(first.clone()).await.unwrap();
        assert!(result.created);
        assert_eq!(counts(&backend), [1, 1, 1, i64::from(quarantine)]);
        if quarantine {
            let replay: Vec<u8> = backend
                .pool()
                .writer()
                .unwrap()
                .conn()
                .query_row(
                    "SELECT delivery_item FROM comm_recipient_quarantine",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                replay,
                first.quarantine.unwrap().delivery_item,
                "quarantine bytes must be verbatim"
            );
        }
    }
}
#[tokio::test]
async fn quarantine_bound_refuses_before_writes() {
    let (backend, mut first) = fixture();
    let store = RecipientTransportStore::new(backend.pool_arc());
    first.disposition = RecipientDisposition::Quarantined;
    first.quarantine = Some(QuarantineRecord {
        reason: QuarantineReason::InvalidMessage,
        delivery_item: vec![b' '; 98_305],
    });
    assert!(store.commit(first).await.is_err());
    assert_eq!(counts(&backend), [0, 0, 0, 0]);
}

#[tokio::test]
async fn quarantine_replay_keeps_first_disposition() {
    let (backend, mut first) = fixture();
    let store = RecipientTransportStore::new(backend.pool_arc());
    first.disposition = RecipientDisposition::Quarantined;
    first.note.content = "Quarantined authenticated message".into();
    first.quarantine = Some(QuarantineRecord {
        reason: QuarantineReason::PolicyRejected,
        delivery_item: b"{ \"opaque\": true }".to_vec(),
    });
    store.commit(first.clone()).await.unwrap();
    first.delivery_attempt_id = Uuid::new_v4();
    first.binding["delivery_attempt_id"] = serde_json::json!(first.delivery_attempt_id);
    first.disposition = RecipientDisposition::Stored;
    first.quarantine = None;
    let duplicate = store.commit(first).await.unwrap();
    assert!(!duplicate.created);
    assert_eq!(
        duplicate.disposition,
        RecipientDisposition::Quarantined,
        "quarantine disposition must survive replay"
    );
    assert_eq!(counts(&backend), [1, 1, 2, 1]);
}

#[tokio::test]
async fn malformed_matched_thread_uses_matched_note_as_root() {
    let (backend, mut input) = fixture();
    let sender = input.note.properties.as_ref().unwrap()["from_actor"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut matched = Note::new("local", "message", "matched");
    matched.properties = Some(serde_json::json!({
        "external_id":"correlation",
        "thread_id":"not-a-uuid",
        "from_actor":"lambda:receiver",
        "to_actor":sender,
        "direction":"outbound"
    }));
    let matched_id = matched.id;
    SqlNoteStore::new(backend.pool_arc(), false)
        .upsert_note(matched)
        .await
        .unwrap();
    input.correlation = Some("correlation".into());

    let note = RecipientTransportStore::new(backend.pool_arc())
        .commit(input)
        .await
        .unwrap()
        .note
        .unwrap();
    assert_eq!(
        note.properties.unwrap()["thread_id"],
        matched_id.to_string()
    );
}

#[tokio::test]
async fn correlation_matches_precanonical_thread_spellings() {
    let thread_root = Uuid::parse_str("abcdefab-cdef-abcd-efab-cdefabcdefab").unwrap();
    let spellings = [
        thread_root.simple().to_string(),
        thread_root.braced().to_string(),
        thread_root.urn().to_string(),
        format!("{:X}", thread_root.as_hyphenated()),
        format!("{:X}", thread_root.simple()),
        format!("{:X}", thread_root.braced()),
        format!("{:X}", thread_root.urn()),
    ];
    for spelling in spellings {
        let (backend, mut input) = fixture();
        let sender = input.note.properties.as_ref().unwrap()["from_actor"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut matched = Note::new("local", "message", "matched");
        matched.properties = Some(serde_json::json!({
            "thread_id":spelling,
            "from_actor":"lambda:receiver",
            "to_actor":sender,
            "direction":"outbound"
        }));
        SqlNoteStore::new(backend.pool_arc(), false)
            .upsert_note(matched)
            .await
            .unwrap();
        input.correlation = Some(thread_root.to_string());
        let note = RecipientTransportStore::new(backend.pool_arc())
            .commit(input)
            .await
            .unwrap()
            .note
            .unwrap();
        assert_eq!(
            note.properties.unwrap()["thread_id"],
            thread_root.to_string(),
            "stored thread spelling {spelling} must correlate"
        );
    }
}

#[tokio::test]
async fn correlation_lookup_uses_reader_before_writer_transaction() {
    let (_memory, mut input) = fixture();
    let directory = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(directory.path().join("recipient-correlation.sqlite3")),
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    crate::run_migrations(pool.writer().unwrap().conn_mut()).unwrap();
    let sender = input.note.properties.as_ref().unwrap()["from_actor"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut matched = Note::new("local", "message", "matched");
    matched.properties = Some(serde_json::json!({
        "external_id":"correlation",
        "from_actor":"lambda:receiver",
        "to_actor":sender,
        "direction":"outbound"
    }));
    let matched_id = matched.id;
    SqlNoteStore::new(Arc::clone(&pool), false)
        .upsert_note(matched)
        .await
        .unwrap();
    input.correlation = Some("correlation".into());

    pool.writer()
        .unwrap()
        .conn()
        .authorizer(Some(|context: AuthContext<'_>| {
            if let AuthAction::Read { table_name, .. } = context.action {
                if table_name == "notes" && context.accessor.is_none() {
                    return Authorization::Deny;
                }
            }
            Authorization::Allow
        }))
        .unwrap();
    let result = RecipientTransportStore::new(Arc::clone(&pool))
        .commit(input)
        .await;
    pool.writer()
        .unwrap()
        .conn()
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();

    let note = result.unwrap().note.unwrap();
    assert_eq!(
        note.properties.unwrap()["thread_id"],
        matched_id.to_string()
    );
}
