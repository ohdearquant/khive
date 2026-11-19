use super::*;
use rusqlite::types::Value as SqlValue;

const ACK_ROW_SQL: &str = concat!(
    "SELECT delivery_attempt_id,sender_agent_id,logical_message_id,binding,disposition,",
    "state,created_at,updated_at,attempt_count,not_before,retirement_reason ",
    "FROM comm_ack_work WHERE delivery_attempt_id=?1",
);

fn acknowledgement_row(backend: &StorageBackend, attempt: Uuid) -> Option<Vec<SqlValue>> {
    backend
        .pool()
        .writer()
        .unwrap()
        .conn()
        .query_row(ACK_ROW_SQL, [attempt.to_string()], |row| {
            (0..11).map(|column| row.get(column)).collect()
        })
        .optional()
        .unwrap()
}

fn receipt_row(backend: &StorageBackend) -> Vec<SqlValue> {
    backend
        .pool()
        .writer()
        .unwrap()
        .conn()
        .query_row("SELECT * FROM comm_recipient_replay", [], |row| {
            (0..row.as_ref().column_count())
                .map(|column| row.get(column))
                .collect()
        })
        .unwrap()
}

#[tokio::test]
async fn due_acknowledgement_preserves_binding_and_finish_is_idempotent() {
    let (backend, input) = fixture();
    let store = RecipientTransportStore::new(backend.pool_arc());
    store.commit(input.clone()).await.unwrap();
    let entries = store.list_due_acknowledgements(i64::MAX, 10).await.unwrap();
    assert_eq!(
        entries.len(),
        1,
        "one committed delivery must have one due entry"
    );
    assert_eq!(entries[0].delivery_attempt_id, input.delivery_attempt_id);
    assert_eq!(
        entries[0].binding, input.binding,
        "journal must retain exact binding"
    );
    assert_eq!(entries[0].disposition, RecipientDisposition::Stored);
    assert_eq!(entries[0].attempt_count, 0);
    assert_eq!(entries[0].not_before, None);

    assert!(store
        .finish_acknowledgement(input.delivery_attempt_id)
        .await
        .unwrap());
    assert!(
        store
            .list_due_acknowledgements(i64::MAX, 10)
            .await
            .unwrap()
            .is_empty(),
        "finished acknowledgement must not be listed"
    );
    let finished = acknowledgement_row(&backend, input.delivery_attempt_id);
    assert!(!store
        .finish_acknowledgement(input.delivery_attempt_id)
        .await
        .unwrap());
    assert_eq!(
        acknowledgement_row(&backend, input.delivery_attempt_id),
        finished,
        "a second finish must not update even its timestamp"
    );
    assert!(!store.finish_acknowledgement(Uuid::new_v4()).await.unwrap());
}

#[tokio::test]
async fn acknowledgement_retry_waits_until_not_before_and_counts_failed_tries() {
    let (backend, input) = fixture();
    let store = RecipientTransportStore::new(backend.pool_arc());
    store.commit(input.clone()).await.unwrap();
    let now = chrono::Utc::now().timestamp_micros();
    let not_before = now + 1_000_000;
    assert!(store
        .record_acknowledgement_failed_try(input.delivery_attempt_id, not_before)
        .await
        .unwrap());
    assert!(
        store
            .list_due_acknowledgements(now, 10)
            .await
            .unwrap()
            .is_empty(),
        "an acknowledgement before its not-before time must not be due"
    );
    let entries = store
        .list_due_acknowledgements(not_before, 10)
        .await
        .unwrap();
    assert_eq!(
        entries.len(),
        1,
        "an acknowledgement is due exactly at not-before"
    );
    assert_eq!(
        entries[0].attempt_count, 1,
        "one failed try must increment the counter"
    );
    assert_eq!(entries[0].not_before, Some(not_before));
    assert_eq!(entries[0].binding, input.binding);
    assert_eq!(entries[0].disposition, RecipientDisposition::Stored);
}

#[tokio::test]
async fn due_acknowledgements_are_oldest_first_bounded_and_indexed() {
    let (backend, first) = fixture();
    let (_, second) = fixture();
    let store = RecipientTransportStore::new(backend.pool_arc());
    store.commit(first.clone()).await.unwrap();
    store.commit(second.clone()).await.unwrap();
    {
        let guard = backend.pool().writer().unwrap();
        for (id, created_at) in [
            (first.delivery_attempt_id, 20),
            (second.delivery_attempt_id, 10),
        ] {
            guard
                .conn()
                .execute(
                    "UPDATE comm_ack_work SET created_at=?1 WHERE delivery_attempt_id=?2",
                    params![created_at, id.to_string()],
                )
                .unwrap();
        }
    }
    let one = store.list_due_acknowledgements(i64::MAX, 1).await.unwrap();
    assert_eq!(one.len(), 1, "due listing must obey the requested limit");
    assert_eq!(one[0].delivery_attempt_id, second.delivery_attempt_id);
    let both = store.list_due_acknowledgements(i64::MAX, 2).await.unwrap();
    assert_eq!(
        both.iter()
            .map(|entry| entry.delivery_attempt_id)
            .collect::<Vec<_>>(),
        vec![second.delivery_attempt_id, first.delivery_attempt_id],
        "due listing must return the oldest entry first"
    );
    assert!(store
        .list_due_acknowledgements(i64::MAX, 0)
        .await
        .unwrap()
        .is_empty());
    backend
        .pool()
        .writer()
        .unwrap()
        .conn()
        .execute("UPDATE comm_ack_work SET created_at=10", [])
        .unwrap();
    let tied = store.list_due_acknowledgements(i64::MAX, 2).await.unwrap();
    let mut expected = [first.delivery_attempt_id, second.delivery_attempt_id];
    expected.sort();
    assert_eq!(
        tied.iter()
            .map(|entry| entry.delivery_attempt_id)
            .collect::<Vec<_>>(),
        expected,
        "equal timestamps must have a stable attempt-identifier order"
    );
    let guard = backend.pool().writer().unwrap();
    let mut statement = guard
        .conn()
        .prepare(&format!("EXPLAIN QUERY PLAN {ACK_DUE_SQL}"))
        .unwrap();
    let plan = statement
        .query_map(params![i64::MAX, 2], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(
        plan.iter().any(|line| line.contains("idx_comm_ack_due")),
        "the production due query must use the due index: {plan:?}"
    );
    assert!(
        !plan.iter().any(|line| line.contains("USE TEMP B-TREE")),
        "the due index must provide the listing order: {plan:?}"
    );
}

#[tokio::test]
async fn acknowledged_and_retired_attempts_remain_unchanged_on_replay() {
    for retired in [false, true] {
        let (backend, input) = fixture();
        let store = RecipientTransportStore::new(backend.pool_arc());
        store.commit(input.clone()).await.unwrap();
        store
            .record_acknowledgement_failed_try(input.delivery_attempt_id, 99)
            .await
            .unwrap();
        if retired {
            assert!(store
                .retire_acknowledgement(
                    input.delivery_attempt_id,
                    AcknowledgementRetirementReason::PermanentTransport,
                )
                .await
                .unwrap());
        } else {
            assert!(store
                .finish_acknowledgement(input.delivery_attempt_id)
                .await
                .unwrap());
        }
        let terminal = acknowledgement_row(&backend, input.delivery_attempt_id);
        assert!(
            terminal.is_some(),
            "terminal acknowledgement must be retained"
        );
        assert!(!store.commit(input.clone()).await.unwrap().created);
        assert_eq!(
            acknowledgement_row(&backend, input.delivery_attempt_id),
            terminal,
            "replay must preserve terminal state, retry counter and timestamps"
        );
        assert!(store
            .list_due_acknowledgements(i64::MAX, 10)
            .await
            .unwrap()
            .is_empty());
        assert!(!store
            .record_acknowledgement_failed_try(input.delivery_attempt_id, 100)
            .await
            .unwrap());
        assert_eq!(
            acknowledgement_row(&backend, input.delivery_attempt_id),
            terminal,
            "a failed-try call must not mutate a terminal row"
        );
    }
}

#[tokio::test]
async fn retiring_acknowledgement_keeps_reason_binding_message_and_receipt() {
    let (backend, input) = fixture();
    let store = RecipientTransportStore::new(backend.pool_arc());
    let committed = store.commit(input.clone()).await.unwrap();
    let note_store = SqlNoteStore::new(backend.pool_arc(), false);
    let note_id = committed.note_id.unwrap();
    let note_before = note_store.get_note(note_id).await.unwrap();
    let receipt_before = receipt_row(&backend);
    let original = acknowledgement_row(&backend, input.delivery_attempt_id).unwrap();
    assert!(store
        .retire_acknowledgement(
            input.delivery_attempt_id,
            AcknowledgementRetirementReason::PermanentTransport,
        )
        .await
        .unwrap());
    let retained = acknowledgement_row(&backend, input.delivery_attempt_id);
    assert!(
        retained.is_some(),
        "retirement must retain the acknowledgement row"
    );
    let retained = retained.unwrap();
    assert_eq!(
        &retained[..5],
        &original[..5],
        "retirement must keep identity and binding"
    );
    assert_eq!(retained[5], SqlValue::Text("retired".into()));
    assert_eq!(retained[10], SqlValue::Text("permanent_transport".into()));
    assert!(
        store
            .list_due_acknowledgements(i64::MAX, 10)
            .await
            .unwrap()
            .is_empty(),
        "retired acknowledgements must never be retried"
    );
    assert_eq!(note_store.get_note(note_id).await.unwrap(), note_before);
    assert_eq!(
        receipt_row(&backend),
        receipt_before,
        "retirement must retain the receipt"
    );
    assert!(!store
        .retire_acknowledgement(
            input.delivery_attempt_id,
            AcknowledgementRetirementReason::PermanentTransport,
        )
        .await
        .unwrap());
    assert_eq!(
        acknowledgement_row(&backend, input.delivery_attempt_id),
        Some(retained)
    );
    assert!(!store
        .finish_acknowledgement(input.delivery_attempt_id)
        .await
        .unwrap());
}

#[tokio::test]
async fn acknowledgement_retry_bookkeeping_survives_file_backed_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("acknowledgements.db");
    let (_, input) = fixture();
    let not_before = chrono::Utc::now().timestamp_micros() + 1_000_000;
    let pending_before;
    {
        let backend = StorageBackend::sqlite_for_test(&path).unwrap();
        backend.pool().run_migrations().unwrap();
        let store = RecipientTransportStore::new(backend.pool_arc());
        store.commit(input.clone()).await.unwrap();
        store
            .record_acknowledgement_failed_try(input.delivery_attempt_id, not_before)
            .await
            .unwrap();
        pending_before = acknowledgement_row(&backend, input.delivery_attempt_id);
    }
    let reopened = StorageBackend::sqlite_for_test(&path).unwrap();
    reopened.pool().run_migrations().unwrap();
    let store = RecipientTransportStore::new(reopened.pool_arc());
    assert_eq!(
        acknowledgement_row(&reopened, input.delivery_attempt_id),
        pending_before,
        "reopening must preserve the complete pending journal row"
    );
    let due = store
        .list_due_acknowledgements(not_before, 10)
        .await
        .unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(
        due[0].attempt_count, 1,
        "the failed-try counter must survive restart"
    );
    assert_eq!(
        due[0].not_before,
        Some(not_before),
        "not-before must survive restart"
    );
    assert_eq!(due[0].binding, input.binding);
    assert_eq!(due[0].disposition, RecipientDisposition::Stored);
    assert!(store
        .list_due_acknowledgements(not_before - 1, 10)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn acknowledgement_failed_try_counter_overflow_keeps_the_row_unchanged() {
    let (backend, input) = fixture();
    let store = RecipientTransportStore::new(backend.pool_arc());
    store.commit(input.clone()).await.unwrap();
    backend
        .pool()
        .writer()
        .unwrap()
        .conn()
        .execute(
            "UPDATE comm_ack_work SET attempt_count=?1 WHERE delivery_attempt_id=?2",
            params![i64::MAX, input.delivery_attempt_id.to_string()],
        )
        .unwrap();
    let before = acknowledgement_row(&backend, input.delivery_attempt_id);
    assert!(
        store
            .record_acknowledgement_failed_try(input.delivery_attempt_id, 100)
            .await
            .is_err(),
        "retry counter overflow must be refused"
    );
    assert_eq!(
        acknowledgement_row(&backend, input.delivery_attempt_id),
        before,
        "a refused counter overflow must not change the retry time or timestamp"
    );
}
