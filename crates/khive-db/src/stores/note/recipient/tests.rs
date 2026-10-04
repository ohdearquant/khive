use super::*;
use crate::pool::PoolConfig;
use crate::stores::note::transport::{SenderAssurance, SenderEnvelope, SenderTransportStore};
use crate::StorageBackend;
use khive_storage::{DeleteMode, NoteStore};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

#[path = "acknowledgement_journal_tests.rs"]
mod acknowledgement_journal_tests;

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
            note: Some(note),
            recipient_actor: "lambda:receiver".into(),
            binding,
            sender_agent_id: sender,
            logical_message_id: logical,
            delivery_attempt_id: attempt,
            disposition: RecipientDisposition::Stored,
            quarantine: None,
            in_reply_to: None,
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
async fn non_khive_outbound_parent_classifies_inbound_message_as_reply() {
    let (backend, mut input) = fixture();
    let parent_logical_id = Uuid::new_v4();
    let mut parent = Note::new("local", "message", "parent");
    parent.properties = Some(serde_json::json!({
        "direction": "outbound",
        "from_actor": "lambda:receiver",
        "to_actor": input.note.as_ref().unwrap().properties.as_ref().unwrap()["from_actor"]
    }));
    let outbound_note_id = parent.id;
    SqlNoteStore::new(backend.pool_arc(), false)
        .upsert_note(parent)
        .await
        .unwrap();
    let envelope = SenderEnvelope {
        namespace: "local".into(),
        logical_message_id: parent_logical_id,
        outbound_note_id,
        kind: "email".into(),
        slug: "test-route".into(),
        credential_ref: "keys/test-route".into(),
        recipient_address: format!("khive1:example/{}", input.sender_agent_id),
        protocol_version: 1,
        sender_agent_id: input.binding["recipient_agent_id"].as_str().unwrap().into(),
        sender_assurance: SenderAssurance::Claimed,
        recipient_agent_id: input.sender_agent_id.clone(),
        recipient_device_id: Uuid::new_v4(),
        recipient_key_epoch: 1,
        contact_generation: 1,
        sender_key_epoch: 1,
        recipient_key_fingerprint: "ab".repeat(32),
        enc: vec![1; 32],
        ciphertext: vec![2],
    };
    SenderTransportStore::new(backend.pool_arc())
        .create(envelope, false)
        .await
        .unwrap();
    input.in_reply_to = Some(parent_logical_id);
    let inbound = RecipientTransportStore::new(backend.pool_arc())
        .commit(input)
        .await
        .unwrap()
        .note
        .unwrap();
    assert_eq!(inbound.properties.unwrap()["message_kind"], "reply");
}

#[tokio::test]
async fn sequential_and_concurrent_duplicates_write_one_note() {
    let (backend, first) = fixture();
    let store = RecipientTransportStore::new(backend.pool_arc());
    let mut second = first.clone();
    second.note.as_mut().unwrap().id = Uuid::new_v4();
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
            .delete_note(result.note_id.unwrap(), mode)
            .await
            .unwrap();
        first.delivery_attempt_id = Uuid::new_v4();
        first.binding["delivery_attempt_id"] = serde_json::json!(first.delivery_attempt_id);
        first.disposition = RecipientDisposition::Quarantined;
        first.note = None;
        first.quarantine = Some(QuarantineRecord {
            reason: QuarantineReason::InvalidMessage,
            delivery_item: b"{\"opaque\":true}".to_vec(),
            parsed_plaintext: None,
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
            first.note = None;
            first.quarantine = Some(QuarantineRecord {
                reason: QuarantineReason::InvalidPlaintext,
                delivery_item: b"{ \"opaque\": true }".to_vec(),
                parsed_plaintext: None,
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
        assert_eq!(
            counts(&backend),
            [i64::from(!quarantine), 1, 1, i64::from(quarantine)]
        );
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
    // Valid JSON leaves the Rust byte bound as the only pre-write refusal.
    // The SQL length constraint alone must not make this control pass.
    let delivery_item = format!("{{\"p\":\"{}\"}}", "x".repeat(98_297)).into_bytes();
    assert_eq!(delivery_item.len(), 98_305);
    assert!(serde_json::from_slice::<Value>(&delivery_item)
        .unwrap()
        .is_object());
    first.note = None;
    first.quarantine = Some(QuarantineRecord {
        reason: QuarantineReason::InvalidMessage,
        delivery_item,
        parsed_plaintext: None,
    });
    let error = store.commit(first).await.unwrap_err();
    assert!(
        matches!(error, StorageError::InvalidInput { message, .. } if message.contains("98304"))
    );
    assert_eq!(counts(&backend), [0, 0, 0, 0]);
}

#[tokio::test]
async fn quarantine_replay_keeps_first_disposition() {
    let (backend, mut first) = fixture();
    let store = RecipientTransportStore::new(backend.pool_arc());
    let note = first.note.take();
    first.disposition = RecipientDisposition::Quarantined;
    first.quarantine = Some(QuarantineRecord {
        reason: QuarantineReason::PolicyRejected,
        delivery_item: b"{ \"opaque\": true }".to_vec(),
        parsed_plaintext: Some(serde_json::json!({"v":1,"body":"refused"})),
    });
    store.commit(first.clone()).await.unwrap();
    first.delivery_attempt_id = Uuid::new_v4();
    first.binding["delivery_attempt_id"] = serde_json::json!(first.delivery_attempt_id);
    first.disposition = RecipientDisposition::Stored;
    first.quarantine = None;
    first.note = note;
    let duplicate = store.commit(first).await.unwrap();
    assert!(!duplicate.created);
    assert_eq!(
        duplicate.disposition,
        RecipientDisposition::Quarantined,
        "quarantine disposition must survive replay"
    );
    assert_eq!(counts(&backend), [0, 1, 2, 1]);
}

#[tokio::test]
async fn malformed_matched_thread_uses_matched_note_as_root() {
    let (backend, mut input) = fixture();
    let sender = input.note.as_ref().unwrap().properties.as_ref().unwrap()["from_actor"]
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
        let sender = input.note.as_ref().unwrap().properties.as_ref().unwrap()["from_actor"]
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
    let sender = input.note.as_ref().unwrap().properties.as_ref().unwrap()["from_actor"]
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

/// A quarantined delivery for `recipient` from `sender`, with no message note.
fn quarantined(sender: &str, recipient: &str, reason: QuarantineReason) -> RecipientCommit {
    let logical = Uuid::new_v4();
    let attempt = Uuid::new_v4();
    RecipientCommit {
        note: None,
        recipient_actor: "lambda:receiver".into(),
        binding: serde_json::json!({"protocol_version":1,"sender_agent_id":sender,"recipient_agent_id":recipient,"logical_message_id":logical,"recipient_device_id":Uuid::nil(),"recipient_key_epoch":1,"contact_generation":1,"delivery_attempt_id":attempt}),
        sender_agent_id: sender.into(),
        logical_message_id: logical,
        delivery_attempt_id: attempt,
        disposition: RecipientDisposition::Quarantined,
        quarantine: Some(QuarantineRecord {
            reason,
            delivery_item: b"{\"item\":true}".to_vec(),
            parsed_plaintext: (reason == QuarantineReason::PolicyRejected)
                .then(|| serde_json::json!({"v":1,"subject":null,"body":"refused"})),
        }),
        in_reply_to: None,
        correlation: None,
    }
}

fn held_quarantine(backend: &StorageBackend) -> Vec<(String, String, String)> {
    let guard = backend.pool().writer().unwrap();
    let mut stmt = guard
        .conn()
        .prepare(
            "SELECT sender_agent_id,logical_message_id,reason FROM comm_recipient_quarantine \
             ORDER BY created_at,sender_agent_id,logical_message_id",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap();
    rows.collect::<Result<_, _>>().unwrap()
}

#[tokio::test]
async fn quarantined_commit_writes_no_message_note_and_replays_without_one() {
    for reason in [
        QuarantineReason::InvalidPlaintext,
        QuarantineReason::InvalidMessage,
        QuarantineReason::PolicyRejected,
    ] {
        let (backend, _) = fixture();
        let store = RecipientTransportStore::new(backend.pool_arc());
        let sender = Uuid::new_v4().to_string();
        let recipient = Uuid::new_v4().to_string();
        let first = quarantined(&sender, &recipient, reason);
        let committed = store.commit(first.clone()).await.unwrap();
        assert!(committed.created);
        assert_eq!(committed.note_id, None, "{reason:?}");
        assert!(committed.note.is_none(), "{reason:?}");
        assert_eq!(counts(&backend), [0, 1, 1, 1], "{reason:?}");
        let replay_note: Option<String> = backend
            .pool()
            .writer()
            .unwrap()
            .conn()
            .query_row("SELECT note_id FROM comm_recipient_replay", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(replay_note, None, "{reason:?}");

        let mut binding = first.binding.clone();
        binding["delivery_attempt_id"] = serde_json::json!(Uuid::new_v4());
        let replay = store
            .ack_if_replayed(binding, "lambda:receiver")
            .await
            .unwrap()
            .expect("a quarantined identity is claimed");
        assert_eq!(replay.disposition, RecipientDisposition::Quarantined);
        assert_eq!(replay.note_id, None);
        assert_eq!(counts(&backend), [0, 1, 2, 1], "{reason:?}");
    }
}

#[tokio::test]
async fn quarantined_commit_refuses_a_message_note() {
    let (backend, mut input) = fixture();
    input.disposition = RecipientDisposition::Quarantined;
    input.quarantine = quarantined(
        &input.sender_agent_id,
        "unused",
        QuarantineReason::InvalidMessage,
    )
    .quarantine;
    let error = RecipientTransportStore::new(backend.pool_arc())
        .commit(input)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, StorageError::InvalidInput { message, .. } if message.contains("cannot carry a message note")),
        "{error:?}"
    );
    assert_eq!(counts(&backend), [0, 0, 0, 0]);
}

#[tokio::test]
async fn only_policy_refused_quarantine_keeps_parsed_plaintext() {
    let (backend, _) = fixture();
    let store = RecipientTransportStore::new(backend.pool_arc());
    let recipient = Uuid::new_v4().to_string();
    let sender = Uuid::new_v4().to_string();

    let refused = quarantined(&sender, &recipient, QuarantineReason::PolicyRejected);
    store.commit(refused.clone()).await.unwrap();
    let invalid = quarantined(&sender, &recipient, QuarantineReason::InvalidMessage);
    store.commit(invalid.clone()).await.unwrap();
    let stored_plaintext = |logical: Uuid| -> Option<String> {
        backend
            .pool()
            .writer()
            .unwrap()
            .conn()
            .query_row(
                "SELECT parsed_plaintext FROM comm_recipient_quarantine WHERE logical_message_id=?1",
                [logical.to_string()],
                |r| r.get(0),
            )
            .unwrap()
    };
    let kept: Value =
        serde_json::from_str(&stored_plaintext(refused.logical_message_id).unwrap()).unwrap();
    assert_eq!(
        Some(kept),
        refused.quarantine.as_ref().unwrap().parsed_plaintext
    );
    assert_eq!(stored_plaintext(invalid.logical_message_id), None);

    let mut without = quarantined(&sender, &recipient, QuarantineReason::PolicyRejected);
    without.quarantine.as_mut().unwrap().parsed_plaintext = None;
    let mut with = quarantined(&sender, &recipient, QuarantineReason::InvalidPlaintext);
    with.quarantine.as_mut().unwrap().parsed_plaintext = Some(serde_json::json!({"body":"x"}));
    for (input, expected) in [
        (without, "requires its parsed plaintext"),
        (with, "only a policy-refused quarantine"),
    ] {
        let error = store.commit(input).await.unwrap_err();
        assert!(
            matches!(&error, StorageError::InvalidInput { message, .. } if message.contains(expected)),
            "{error:?}"
        );
    }
    assert_eq!(counts(&backend), [0, 2, 2, 2]);

    // The schema holds the same rule for any writer.
    let guard = backend.pool().writer().unwrap();
    for (reason, plaintext) in [
        ("policy_rejected", None),
        ("invalid_message", Some("{\"body\":\"x\"}")),
    ] {
        assert!(guard
            .conn()
            .execute(
                "INSERT INTO comm_recipient_quarantine \
                 (sender_agent_id,logical_message_id,recipient_agent_id,delivery_item,reason,\
                  parsed_plaintext,created_at) VALUES ('s',?1,'r',X'7B7D',?2,?3,1)",
                params![Uuid::new_v4().to_string(), reason, plaintext],
            )
            .is_err());
    }
    for (disposition, note_id) in [("stored", None), ("quarantined", Some("note"))] {
        assert!(guard
            .conn()
            .execute(
                "INSERT INTO comm_recipient_replay \
                 (sender_agent_id,logical_message_id,recipient_agent_id,recipient_actor,\
                  note_id,disposition,created_at) VALUES ('s',?1,'r','a',?2,?3,1)",
                params![Uuid::new_v4().to_string(), note_id, disposition],
            )
            .is_err());
    }
}

#[tokio::test]
async fn local_quarantine_bound_evicts_oldest_non_policy_item_only() {
    let (backend, _) = fixture();
    let store = RecipientTransportStore::new(backend.pool_arc());
    let recipient = Uuid::new_v4().to_string();
    let other_recipient = Uuid::new_v4().to_string();
    let sender = Uuid::new_v4().to_string();
    // Oldest of all: another local recipient's item and this recipient's policy refusals.
    let foreign = quarantined(&sender, &other_recipient, QuarantineReason::InvalidMessage);
    store.commit(foreign.clone()).await.unwrap();
    let mut refused = Vec::new();
    for _ in 0..3 {
        let input = quarantined(&sender, &recipient, QuarantineReason::PolicyRejected);
        store.commit(input.clone()).await.unwrap();
        refused.push(input.logical_message_id);
    }
    let mut local = Vec::new();
    for i in 0..LOCAL_QUARANTINE_BOUND {
        let reason = if i % 2 == 0 {
            QuarantineReason::InvalidPlaintext
        } else {
            QuarantineReason::InvalidMessage
        };
        let input = quarantined(&Uuid::new_v4().to_string(), &recipient, reason);
        let result = store.commit(input.clone()).await.unwrap();
        assert!(result.evicted.is_empty(), "under the bound at {i}");
        local.push(input);
    }
    let overflow = quarantined(&sender, &recipient, QuarantineReason::InvalidMessage);
    let result = store.commit(overflow.clone()).await.unwrap();
    assert_eq!(
        result.evicted,
        vec![EvictedQuarantine {
            sender_agent_id: local[0].sender_agent_id.clone(),
            logical_message_id: local[0].logical_message_id,
            reason: QuarantineReason::InvalidPlaintext,
        }]
    );
    let held = held_quarantine(&backend);
    let held_ids: Vec<&str> = held.iter().map(|(_, id, _)| id.as_str()).collect();
    assert!(!held_ids.contains(&local[0].logical_message_id.to_string().as_str()));
    assert!(held_ids.contains(&foreign.logical_message_id.to_string().as_str()));
    for id in &refused {
        assert!(held_ids.contains(&id.to_string().as_str()));
    }
    let local_held = held
        .iter()
        .filter(|(_, id, reason)| {
            reason != "policy_rejected" && *id != foreign.logical_message_id.to_string()
        })
        .count();
    assert_eq!(local_held, LOCAL_QUARANTINE_BOUND);
}

#[tokio::test]
async fn per_sender_bound_evicts_only_that_senders_oldest_refusal() {
    let (backend, _) = fixture();
    let store = RecipientTransportStore::new(backend.pool_arc());
    let recipient = Uuid::new_v4().to_string();
    let quiet = Uuid::new_v4().to_string();
    let noisy = Uuid::new_v4().to_string();
    // The quiet sender's refusals and one invalid item are older than every noisy one.
    let mut quiet_ids = Vec::new();
    for _ in 0..2 {
        let input = quarantined(&quiet, &recipient, QuarantineReason::PolicyRejected);
        store.commit(input.clone()).await.unwrap();
        quiet_ids.push(input.logical_message_id);
    }
    let invalid = quarantined(&noisy, &recipient, QuarantineReason::InvalidMessage);
    store.commit(invalid.clone()).await.unwrap();
    let mut noisy_ids = Vec::new();
    for i in 0..POLICY_REFUSED_PER_SENDER_BOUND {
        let input = quarantined(&noisy, &recipient, QuarantineReason::PolicyRejected);
        let result = store.commit(input.clone()).await.unwrap();
        assert!(result.evicted.is_empty(), "under the bound at {i}");
        noisy_ids.push(input.logical_message_id);
    }
    let result = store
        .commit(quarantined(
            &noisy,
            &recipient,
            QuarantineReason::PolicyRejected,
        ))
        .await
        .unwrap();
    assert_eq!(
        result.evicted,
        vec![EvictedQuarantine {
            sender_agent_id: noisy.clone(),
            logical_message_id: noisy_ids[0],
            reason: QuarantineReason::PolicyRejected,
        }]
    );
    let held = held_quarantine(&backend);
    let held_ids: Vec<String> = held.iter().map(|(_, id, _)| id.clone()).collect();
    for id in quiet_ids.iter().chain([&invalid.logical_message_id]) {
        assert!(held_ids.contains(&id.to_string()), "{id} must stay");
    }
    assert!(!held_ids.contains(&noisy_ids[0].to_string()));
    let noisy_refusals = held
        .iter()
        .filter(|(sender, _, reason)| *sender == noisy && reason == "policy_rejected")
        .count();
    assert_eq!(noisy_refusals, POLICY_REFUSED_PER_SENDER_BOUND);
}

#[tokio::test]
async fn eviction_keeps_the_replay_identity_claimed() {
    let (backend, _) = fixture();
    let store = RecipientTransportStore::new(backend.pool_arc());
    let recipient = Uuid::new_v4().to_string();
    let sender = Uuid::new_v4().to_string();
    let first = quarantined(&sender, &recipient, QuarantineReason::PolicyRejected);
    store.commit(first.clone()).await.unwrap();
    for _ in 0..POLICY_REFUSED_PER_SENDER_BOUND {
        store
            .commit(quarantined(
                &sender,
                &recipient,
                QuarantineReason::PolicyRejected,
            ))
            .await
            .unwrap();
    }
    let held = held_quarantine(&backend);
    assert!(!held
        .iter()
        .any(|(_, id, _)| *id == first.logical_message_id.to_string()));
    let before = counts(&backend);

    // A later delivery of the evicted message under a new attempt, now valid and
    // admitted, is still answered from its first disposition.
    let (_, mut stored) = fixture();
    let mut redelivery = first.clone();
    let attempt = Uuid::new_v4();
    redelivery.delivery_attempt_id = attempt;
    redelivery.binding["delivery_attempt_id"] = serde_json::json!(attempt);
    redelivery.disposition = RecipientDisposition::Stored;
    redelivery.quarantine = None;
    stored.note.as_mut().unwrap().properties = Some(
        serde_json::json!({"from_actor":format!("khive1:example/{sender}"),"to_actor":"lambda:receiver","direction":"inbound"}),
    );
    redelivery.note = stored.note;
    let replay = store.commit(redelivery).await.unwrap();
    assert!(!replay.created);
    assert_eq!(replay.disposition, RecipientDisposition::Quarantined);
    assert_eq!(replay.note_id, None);
    let after = counts(&backend);
    assert_eq!(after, [before[0], before[1], before[2] + 1, before[3]]);
}
