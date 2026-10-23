use super::*;
use crate::{ConnectionPool, SqlBridge, StorageBackend};
use khive_storage::WriterTaskRequestState;
use std::sync::Arc;

#[derive(Clone)]
struct Fixture {
    epoch: AuthorityEpoch,
    anchor: AuthorityRecord,
    grants: [AuthorityRecord; 2],
    changes: [AuthorityChange; 2],
    replay_key: ReplayKey,
    replay: AuthorityReplay,
}

// A write refused inside `transaction` rolls the transaction back, so the
// bound refusal arrives wrapped in the writer's rolled-back request error.
fn rolled_back_input(result: StorageResult<()>) -> String {
    match result {
        Err(StorageError::WriterTaskRequestFailed {
            request_state,
            source,
        }) => {
            assert_eq!(request_state, WriterTaskRequestState::TransactionRolledBack);
            match *source {
                StorageError::InvalidInput { message, .. } => message,
                other => panic!("expected an input refusal, got {other:?}"),
            }
        }
        other => panic!("expected a rolled-back write, got {other:?}"),
    }
}

fn fixture() -> Fixture {
    let epoch_id = Uuid::from_u128(1);
    let anchor_id = Uuid::from_u128(10);
    let anchor = AuthorityRecord {
        id: anchor_id,
        epoch_id,
        kind: RecordKind::Anchor,
        principal: b"administrator".to_vec(),
        namespace: "authority".into(),
        right_types: vec![],
        parent_id: None,
        bootstrap_slot: None,
        root_anchor_id: None,
        created_at: 1,
        body: b"opaque anchor".to_vec(),
    };
    let grants = [11, 12].map(|value| AuthorityRecord {
        id: Uuid::from_u128(value),
        epoch_id,
        kind: RecordKind::Grant,
        principal: b"producer".to_vec(),
        namespace: "evidence".into(),
        right_types: vec!["grant_admin".into(), "record_observation".into()],
        parent_id: None,
        bootstrap_slot: Some("observation-source".into()),
        root_anchor_id: Some(anchor_id),
        created_at: 2,
        body: format!("opaque authenticated grant {value}").into_bytes(),
    });
    let changes = [0, 1].map(|index| AuthorityChange {
        event_id: Uuid::from_u128(100 + index as u128),
        epoch_id,
        action_id: Uuid::from_u128(50),
        kind: ChangeKind::Grant,
        record_id: grants[index].id,
    });
    Fixture {
        epoch: AuthorityEpoch {
            epoch_id,
            marker_id: Uuid::from_u128(2),
            store_id: Uuid::from_u128(3),
            manifest_digest: [7; 32],
            manifest: b"normalized manifest without keys".to_vec(),
            namespace: "authority".into(),
            created_at: 1,
        },
        anchor,
        grants,
        changes,
        replay_key: ReplayKey {
            epoch_id,
            structural_caller: b"structured administrator".to_vec(),
            canonical_verb: "authority.grant".into(),
            request_id: "request-1".into(),
        },
        replay: AuthorityReplay {
            canonical_request: b"canonical request".to_vec(),
            result: b"complete result".to_vec(),
        },
    }
}

fn setup() -> (SqlBridge, Arc<ConnectionPool>) {
    let backend = StorageBackend::memory().unwrap();
    assert_eq!(
        backend.prepare_core_schema().unwrap(),
        crate::migrations::latest_schema_version()
    );
    let pool = backend.pool_arc();
    (SqlBridge::new(Arc::clone(&pool), false), pool)
}

fn event(change: &AuthorityChange) -> Event {
    let mut event = Event::new(
        "evidence",
        "grant",
        EventKind::GrantChange,
        SubstrateKind::Event,
        "administrator",
    );
    event.id = change.event_id;
    event.created_at = 2;
    event.payload_schema_version = 1;
    event.payload =
        serde_json::json!({ "action_id": change.action_id, "record_id": change.record_id });
    event
}

async fn seed(tx: &mut AuthorityTransaction<'_>, data: &Fixture) -> StorageResult<()> {
    tx.insert_epoch(&data.epoch).await?;
    tx.insert_record(&data.anchor).await?;
    for record in &data.grants {
        tx.insert_record(record).await?;
    }
    assert_eq!(tx.advance_revision(data.epoch.epoch_id, 0).await?, 1);
    tx.insert_replay(&data.replay_key, &data.replay).await?;
    for change in &data.changes {
        tx.append_change(change, &event(change)).await?;
    }
    Ok(())
}

#[tokio::test]
async fn opaque_records_replay_and_action_pages_round_trip() {
    let (bridge, pool) = setup();
    let data = fixture();
    let expected = data.clone();
    transaction(&bridge, move |mut tx| {
        Box::pin(async move {
            seed(&mut tx, &data).await?;
            Ok(())
        })
    })
    .await
    .unwrap();
    {
        let reader = pool.reader().unwrap();
        for tag in ["grant_admin", "record_observation"] {
            let count: i64 = reader.conn().query_row(
                "SELECT COUNT(*) FROM authority_record_right_types WHERE epoch_id=?1 AND right_type=?2",
                rusqlite::params![expected.epoch.epoch_id.to_string(), tag],
                |row| row.get(0),
            ).unwrap();
            assert_eq!(
                count, 2,
                "mixed grants must be discoverable by either right type"
            );
        }
    }
    transaction(&bridge, move |mut tx| {
        Box::pin(async move {
            assert_eq!(tx.epoch().await?, Some(expected.epoch.clone()));
            assert_eq!(tx.revision(expected.epoch.epoch_id).await?, Some(1));
            assert_eq!(
                tx.record(expected.epoch.epoch_id, expected.grants[0].id)
                    .await?,
                Some(expected.grants[0].clone())
            );
            let first = tx
                .records_for_principal(expected.epoch.epoch_id, b"producer", None, 1)
                .await?;
            assert_eq!(first.items, vec![expected.grants[0].clone()]);
            assert_eq!(first.next_id, Some(expected.grants[0].id));
            let last = tx
                .records_for_principal(expected.epoch.epoch_id, b"producer", first.next_id, 1)
                .await?;
            assert_eq!(last.items, vec![expected.grants[1].clone()]);
            assert_eq!(last.next_id, None);
            let first = tx
                .changes_for_action(
                    expected.epoch.epoch_id,
                    expected.changes[0].action_id,
                    None,
                    1,
                )
                .await?;
            assert_eq!(first.items, vec![expected.changes[0].clone()]);
            let last = tx
                .changes_for_action(
                    expected.epoch.epoch_id,
                    expected.changes[0].action_id,
                    first.next_id,
                    1,
                )
                .await?;
            assert_eq!(last.items, vec![expected.changes[1].clone()]);
            assert_eq!(last.next_id, None);
            assert_eq!(
                tx.replay(&expected.replay_key).await?,
                Some(expected.replay.clone())
            );
            let mut other = expected.replay_key.clone();
            other.structural_caller.push(b'x');
            assert_eq!(tx.replay(&other).await?, None);
            Ok(())
        })
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn terminal_append_preserves_original_record_and_cannot_repeat() {
    let (bridge, _) = setup();
    let data = fixture();
    let terminal = AuthorityTransition {
        id: Uuid::from_u128(200),
        epoch_id: data.epoch.epoch_id,
        record_id: data.grants[0].id,
        state: TerminalState::Disabled,
        action_id: Uuid::from_u128(51),
        event_id: Uuid::from_u128(102),
        created_at: 3,
        body: b"terminal transition".to_vec(),
    };
    let expected = (data.clone(), terminal.clone());
    transaction(&bridge, move |mut tx| {
        Box::pin(async move {
            seed(&mut tx, &data).await?;
            let change = AuthorityChange {
                event_id: terminal.event_id,
                epoch_id: terminal.epoch_id,
                action_id: terminal.action_id,
                kind: ChangeKind::Grant,
                record_id: terminal.record_id,
            };
            let mut history = event(&change);
            history.verb = "revoke".into();
            tx.append_change(&change, &history).await?;
            tx.insert_transition(&terminal).await?;
            assert_eq!(tx.advance_revision(terminal.epoch_id, 1).await?, 2);
            Ok(())
        })
    })
    .await
    .unwrap();
    let repeated = expected.1.clone();
    assert!(transaction(&bridge, move |mut tx| Box::pin(async move {
        tx.insert_transition(&repeated).await
    }))
    .await
    .is_err());
    transaction(&bridge, move |mut tx| {
        Box::pin(async move {
            assert_eq!(
                tx.record(expected.0.epoch.epoch_id, expected.0.grants[0].id)
                    .await?,
                Some(expected.0.grants[0].clone())
            );
            assert_eq!(
                tx.transition(expected.1.epoch_id, expected.1.record_id)
                    .await?,
                Some(expected.1.clone())
            );
            assert_eq!(tx.revision(expected.1.epoch_id).await?, Some(2));
            Ok(())
        })
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn history_failure_rolls_back_records_revision_receipt_and_prior_event() {
    let (bridge, pool) = setup();
    let data = fixture();
    let failed_id = data.changes[1].event_id.to_string();
    pool.writer()
        .unwrap()
        .transaction(|conn| {
            conn.execute_batch(&format!(
                "CREATE TRIGGER fail_authority_history BEFORE INSERT ON events \
            WHEN NEW.id='{failed_id}' BEGIN SELECT RAISE(ABORT,'history failure'); END;"
            ))?;
            Ok(())
        })
        .unwrap();
    let retry = data.clone();
    assert!(transaction(&bridge, move |mut tx| Box::pin(async move {
        seed(&mut tx, &data).await
    }))
    .await
    .is_err());
    {
        let reader = pool.reader().unwrap();
        for table in [
            "authority_epochs",
            "authority_records",
            "authority_record_right_types",
            "authority_revisions",
            "authority_replays",
            "authority_changes",
            "authority_transitions",
            "events",
        ] {
            let count: i64 = reader
                .conn()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table} survived failed history persistence");
        }
    }
    pool.writer()
        .unwrap()
        .transaction(|conn| {
            conn.execute_batch("DROP TRIGGER fail_authority_history;")?;
            Ok(())
        })
        .unwrap();
    let restarted_service = SqlBridge::new(Arc::clone(&pool), false);
    let expected = retry.clone();
    transaction(&restarted_service, move |mut tx| {
        Box::pin(async move { seed(&mut tx, &retry).await })
    })
    .await
    .unwrap();
    transaction(&restarted_service, move |mut tx| {
        Box::pin(async move {
            assert_eq!(tx.revision(expected.epoch.epoch_id).await?, Some(1));
            assert_eq!(
                tx.replay(&expected.replay_key).await?,
                Some(expected.replay)
            );
            assert_eq!(
                tx.changes_for_action(
                    expected.epoch.epoch_id,
                    expected.changes[0].action_id,
                    None,
                    256
                )
                .await?
                .items
                .len(),
                2
            );
            Ok(())
        })
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn bounds_and_stale_revision_refuse_without_partial_rows() {
    let (bridge, _) = setup();
    for limit in [0, MAX_AUTHORITY_PAGE_SIZE + 1] {
        assert!(transaction(&bridge, move |mut tx| Box::pin(async move {
            tx.records_for_principal(Uuid::from_u128(1), b"producer", None, limit)
                .await
        }))
        .await
        .is_err());
    }
    for tags in [
        vec!["record_observation".into(), "grant_admin".into()],
        vec!["grant_admin".into(), "grant_admin".into()],
    ] {
        let mut data = fixture();
        data.grants[0].right_types = tags;
        assert!(transaction(&bridge, move |mut tx| Box::pin(async move {
            tx.insert_epoch(&data.epoch).await?;
            tx.insert_record(&data.anchor).await?;
            tx.insert_record(&data.grants[0]).await
        }))
        .await
        .is_err());
        assert_eq!(
            transaction(&bridge, |mut tx| Box::pin(async move { tx.epoch().await }))
                .await
                .unwrap(),
            None
        );
    }
    let mut data = fixture();
    data.anchor.body = vec![1; MAX_AUTHORITY_RECORD_BYTES + 1];
    assert!(transaction(&bridge, move |mut tx| Box::pin(async move {
        tx.insert_epoch(&data.epoch).await?;
        tx.insert_record(&data.anchor).await
    }))
    .await
    .is_err());
    assert_eq!(
        transaction(&bridge, |mut tx| Box::pin(async move { tx.epoch().await }))
            .await
            .unwrap(),
        None
    );
    let data = fixture();
    assert!(transaction(&bridge, move |mut tx| Box::pin(async move {
        tx.insert_epoch(&data.epoch).await?;
        tx.insert_record(&data.anchor).await?;
        tx.advance_revision(data.epoch.epoch_id, 1).await
    }))
    .await
    .is_err());
    assert_eq!(
        transaction(&bridge, |mut tx| Box::pin(async move { tx.epoch().await }))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn record_body_bound_is_independent_of_request_replay_and_manifest_bounds() {
    let (bridge, _) = setup();
    let mut data = fixture();
    data.epoch.manifest = vec![7; MAX_AUTHORITY_MANIFEST_BYTES];
    data.grants[0].body = vec![11; MAX_AUTHORITY_REQUEST_BYTES + 1];
    data.grants[1].body = vec![12; MAX_AUTHORITY_RECORD_BYTES];
    data.replay.canonical_request = vec![13; MAX_AUTHORITY_REQUEST_BYTES];
    data.replay.result = vec![14; MAX_AUTHORITY_REPLAY_BYTES];
    let expected = data.clone();
    transaction(&bridge, move |mut tx| {
        Box::pin(async move { seed(&mut tx, &data).await })
    })
    .await
    .unwrap();
    let check = expected.clone();
    transaction(&bridge, move |mut tx| {
        Box::pin(async move {
            assert_eq!(tx.epoch().await?, Some(check.epoch.clone()));
            for record in check.grants {
                assert_eq!(tx.record(record.epoch_id, record.id).await?, Some(record));
            }
            assert_eq!(tx.replay(&check.replay_key).await?, Some(check.replay));
            Ok(())
        })
    })
    .await
    .unwrap();

    let mut oversized_record = expected.grants[1].clone();
    oversized_record.id = Uuid::from_u128(13);
    oversized_record.body.push(1);
    let record_id = oversized_record.id;
    let result = transaction(&bridge, move |mut tx| {
        Box::pin(async move { tx.insert_record(&oversized_record).await })
    })
    .await;
    let message = rolled_back_input(result);
    assert!(
        message.contains("record body")
            && message.contains("2097153")
            && message.contains("1..=2097152"),
        "{message}"
    );
    transaction(&bridge, move |mut tx| {
        Box::pin(async move {
            assert_eq!(tx.record(Uuid::from_u128(1), record_id).await?, None);
            assert_eq!(tx.revision(Uuid::from_u128(1)).await?, Some(1));
            Ok(())
        })
    })
    .await
    .unwrap();

    for oversize_request in [true, false] {
        let mut key = expected.replay_key.clone();
        key.request_id = format!("oversize-{oversize_request}");
        let read_key = key.clone();
        let mut replay = expected.replay.clone();
        let expected_field = if oversize_request {
            replay.canonical_request.push(1);
            "canonical request"
        } else {
            replay.result.push(1);
            "replay result"
        };
        let result = transaction(&bridge, move |mut tx| {
            Box::pin(async move { tx.insert_replay(&key, &replay).await })
        })
        .await;
        let message = rolled_back_input(result);
        assert!(
            message.contains(expected_field)
                && message.contains("1048577")
                && message.contains("1..=1048576"),
            "{message}"
        );
        assert_eq!(
            transaction(&bridge, move |mut tx| Box::pin(async move {
                tx.replay(&read_key).await
            }))
            .await
            .unwrap(),
            None
        );
    }
    let (fresh, _) = setup();
    let mut manifest = expected.epoch;
    manifest.manifest.push(1);
    let result = transaction(&fresh, move |mut tx| {
        Box::pin(async move { tx.insert_epoch(&manifest).await })
    })
    .await;
    let message = rolled_back_input(result);
    assert!(
        message.contains("manifest")
            && message.contains("1048577")
            && message.contains("1..=1048576"),
        "{message}"
    );
    assert_eq!(
        transaction(&fresh, |mut tx| Box::pin(async move { tx.epoch().await }))
            .await
            .unwrap(),
        None
    );
}

fn migrated_connection(path: &std::path::Path) -> rusqlite::Connection {
    crate::extension::ensure_extensions_loaded();
    let mut conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    assert_eq!(
        crate::migrations::run_migrations_for_test(&mut conn).unwrap(),
        crate::migrations::latest_schema_version()
    );
    conn
}

fn insert_marker(conn: &rusqlite::Connection, epoch: u128) -> rusqlite::Result<usize> {
    conn.execute(
        include_str!("../../sql/authority/epoch-insert.sql"),
        rusqlite::params![
            Uuid::from_u128(epoch).to_string(),
            Uuid::from_u128(epoch + 1000).to_string(),
            Uuid::from_u128(500).to_string(),
            vec![7u8; 32],
            b"manifest".to_vec(),
            "authority",
            1
        ],
    )
}

#[test]
fn singleton_constraint_refuses_second_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let conn = migrated_connection(&dir.path().join("authority.db"));
    insert_marker(&conn, 1).unwrap();
    let error = insert_marker(&conn, 2)
        .expect_err("a distinct epoch and marker must still violate the singleton constraint");
    assert!(
        matches!(error, rusqlite::Error::SqliteFailure(code, _) if code.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE)
    );
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM authority_epochs", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn fresh_migration_and_reopen_preserve_empty_seed_marker() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("authority.db");
    {
        let conn = migrated_connection(&path);
        let name: String = conn
            .query_row(
                "SELECT name FROM _schema_migrations WHERE version=62",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name, "authority_store");
        insert_marker(&conn, 1).unwrap();
    }
    let conn = migrated_connection(&path);
    let marker: (String, Vec<u8>) = conn
        .query_row(
            "SELECT epoch_id,manifest FROM authority_epochs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        marker,
        (Uuid::from_u128(1).to_string(), b"manifest".to_vec())
    );
    let grants: i64 = conn
        .query_row("SELECT COUNT(*) FROM authority_records", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(grants, 0);
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name GLOB 'authority_*'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 7);
}
