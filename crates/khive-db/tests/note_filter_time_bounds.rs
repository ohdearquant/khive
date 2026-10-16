use std::collections::BTreeSet;
use std::sync::Arc;

use khive_db::pool::{ConnectionPool, PoolConfig};
use khive_db::stores::note::SqlNoteStore;
use khive_storage::note::{FilterOp, Note, NoteFilter, NoteSeekAfter, PropertyFilter};
use khive_storage::types::{PageRequest, SqlValue};
use khive_storage::{NoteStore, StorageCapability, StorageError, WriterTaskRequestState};
use serde_json::json;
use uuid::Uuid;

fn setup() -> (Arc<ConnectionPool>, SqlNoteStore) {
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: None,
            write_queue_enabled: Some(false),
            write_routing_strict: false,
            ..PoolConfig::for_test()
        })
        .unwrap(),
    );
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(include_str!("../sql/notes-ddl.sql"))
        .unwrap();
    let store = SqlNoteStore::new(pool.clone(), false);
    (pool, store)
}

fn note(id: u128, namespace: &str, created: i64, expiry: Option<i64>, deleted: bool) -> Note {
    let mut note = Note::new(namespace, "message", "filter fixture");
    note.id = Uuid::from_u128(id);
    note.created_at = created;
    note.updated_at = created;
    note.expires_at = expiry;
    note.deleted_at = deleted.then_some(500);
    note.key = Some(format!("fixture-{id}"));
    note.properties = Some(json!({"to_actor":"reader", "direction":"inbound"}));
    note
}

fn filter() -> NoteFilter {
    NoteFilter {
        kind: Some("message".into()),
        property_filters: vec![
            PropertyFilter {
                json_path: "$.to_actor".into(),
                op: FilterOp::EqOrMissingIndexed,
                value: SqlValue::Text("reader".into()),
            },
            PropertyFilter {
                json_path: "$.direction".into(),
                op: FilterOp::Eq,
                value: SqlValue::Text("inbound".into()),
            },
        ],
        ..Default::default()
    }
}

fn ids(notes: &[Note]) -> BTreeSet<Uuid> {
    notes.iter().map(|note| note.id).collect()
}

fn expected(values: &[u128]) -> BTreeSet<Uuid> {
    values.iter().map(|value| Uuid::from_u128(*value)).collect()
}

async fn assert_reads(store: &SqlNoteStore, filter: &NoteFilter, want: &[u128]) {
    let counted = store
        .query_notes_filtered("one", filter, PageRequest::default())
        .await
        .unwrap();
    assert_eq!(counted.total, Some(want.len() as u64));
    assert_eq!(ids(&counted.items), expected(want));
    let count_free = store
        .query_notes_filtered_count_free("one", filter, PageRequest::default())
        .await
        .unwrap();
    assert_eq!(count_free.total, None);
    assert_eq!(ids(&count_free.items), expected(want));
    assert_eq!(
        store
            .count_notes_filtered_in_snapshot("one", std::slice::from_ref(filter))
            .await
            .unwrap(),
        vec![want.len() as u64]
    );
    let bounded = store
        .count_notes_filtered_bounded_in_snapshot("one", std::slice::from_ref(filter), 1)
        .await
        .unwrap();
    assert_eq!(bounded[0].count, (want.len() as u64).min(1));
    assert_eq!(bounded[0].saturated, want.len() > 1);
    assert_eq!(
        ids(&store
            .query_notes_filtered_bounded("one", filter, 50)
            .await
            .unwrap()),
        expected(want)
    );

    let mut seek = None;
    let mut sequence_ids = BTreeSet::new();
    loop {
        let page = store
            .query_notes_filtered_after("one", filter, seek, 1)
            .await
            .unwrap();
        for note in &page.items {
            assert!(sequence_ids.insert(note.id));
        }
        let Some(next) = page.next_after else { break };
        seek = Some(next);
    }
    assert_eq!(sequence_ids, expected(want));

    let mut seek_filter = filter.clone();
    let mut creation_ids = BTreeSet::new();
    loop {
        let page = store
            .query_notes_filtered_count_free(
                "one",
                &seek_filter,
                PageRequest {
                    offset: 0,
                    limit: 1,
                },
            )
            .await
            .unwrap();
        let Some(note) = page.items.first() else {
            break;
        };
        assert!(creation_ids.insert(note.id));
        seek_filter.after = Some(NoteSeekAfter {
            created_at: note.created_at,
            id: note.id,
        });
    }
    assert_eq!(creation_ids, expected(want));

    if filter.namespaces.is_empty() {
        let mut cursor = None;
        let mut keyed_ids = BTreeSet::new();
        loop {
            let (page, next) = store
                .query_keyed_notes(
                    "one",
                    filter,
                    "fixture-",
                    cursor.as_ref(),
                    PageRequest {
                        offset: 0,
                        limit: 1,
                    },
                )
                .await
                .unwrap();
            for note in &page {
                assert!(keyed_ids.insert(note.id));
            }
            let Some(next) = next else { break };
            cursor = Some(next);
        }
        assert_eq!(keyed_ids, expected(want));
    }
}

#[tokio::test]
async fn bounds_and_tombstones_agree_across_counts_and_every_page_shape() {
    let (_pool, store) = setup();
    for row in [
        note(1, "one", 10, Some(100), false),
        note(2, "one", 20, Some(200), true),
        note(3, "one", 30, Some(300), false),
        note(4, "one", 15, None, false),
        note(5, "other", 10, Some(100), false),
        note(6, "one", 20, Some(200), false),
    ] {
        store.upsert_note(row).await.unwrap();
    }

    assert_reads(&store, &filter(), &[1, 3, 4, 6]).await;
    assert_reads(&store, &filter().created_before(20), &[1, 4, 6]).await;
    assert_reads(&store, &filter().expires_before(200), &[1, 6]).await;
    assert_reads(
        &store,
        &filter()
            .expires_before(200)
            .created_before(20)
            .include_deleted(),
        &[1, 2, 6],
    )
    .await;
    assert_reads(
        &store,
        &filter()
            .expires_before(199)
            .created_before(19)
            .include_deleted(),
        &[1],
    )
    .await;
    let mut bounded = filter()
        .expires_before(200)
        .created_before(20)
        .include_deleted();
    bounded.min_created_at = Some(20);
    assert_reads(&store, &bounded, &[2, 6]).await;
    bounded.min_created_at = Some(21);
    assert_reads(&store, &bounded, &[]).await;
    bounded.min_created_at = None;
    bounded.namespaces = vec!["one".into(), "other".into()];
    assert_reads(&store, &bounded, &[1, 2, 5, 6]).await;
}

#[tokio::test]
async fn bound_parameters_preserve_negative_and_extreme_microseconds() {
    let (_pool, store) = setup();
    for row in [
        note(11, "one", i64::MIN, Some(i64::MIN), false),
        note(12, "one", -1, Some(-1), false),
        note(13, "one", i64::MAX, Some(i64::MAX), false),
    ] {
        store.upsert_note(row).await.unwrap();
    }
    assert_reads(
        &store,
        &filter().created_before(-1).expires_before(-1),
        &[11, 12],
    )
    .await;
    assert_reads(
        &store,
        &filter().created_before(i64::MIN).expires_before(i64::MIN),
        &[11],
    )
    .await;
    assert_reads(
        &store,
        &filter().created_before(i64::MAX).expires_before(i64::MAX),
        &[11, 12, 13],
    )
    .await;
}

#[tokio::test]
async fn tombstone_read_selector_never_relaxes_scalar_or_atomic_patch_guards() {
    let (_pool, store) = setup();
    let live = note(21, "one", 10, Some(100), false);
    let dead = note(22, "one", 20, Some(200), true);
    store.upsert_note(live.clone()).await.unwrap();
    store.upsert_note(dead.clone()).await.unwrap();
    let all = filter().include_deleted();
    assert_reads(&store, &all, &[21, 22]).await;
    assert!(!store
        .try_patch_note_property(dead.id, "one", &all, "$.patched", json!(true), 900)
        .await
        .unwrap());
    let error = store
        .patch_note_property_atomic(
            vec![live.id, dead.id],
            "one",
            &all,
            "$.patched",
            json!(true),
            900,
        )
        .await
        .unwrap_err();
    let StorageError::WriterTaskRequestFailed {
        request_state: WriterTaskRequestState::TransactionRolledBack,
        source,
    } = error
    else {
        panic!("expected confirmed rollback, got {error:?}")
    };
    assert!(matches!(*source, StorageError::Conflict {
        capability: StorageCapability::Notes, operation, ..
    } if operation == "patch_note_property_atomic"));
    assert_eq!(
        store.get_note_including_deleted(dead.id).await.unwrap(),
        Some(dead)
    );
    assert_eq!(store.get_note(live.id).await.unwrap(), Some(live.clone()));
    assert!(store
        .try_patch_note_property(live.id, "one", &all, "$.patched", json!(true), 900)
        .await
        .unwrap());
    assert_eq!(
        store
            .get_note(live.id)
            .await
            .unwrap()
            .unwrap()
            .properties
            .unwrap()["patched"],
        true
    );
}

#[test]
fn serialized_filters_keep_legacy_defaults_and_roundtrip_new_selectors() {
    let legacy = json!({"kind":null, "order_by":null, "min_created_at":null});
    let decoded: NoteFilter = serde_json::from_value(legacy).unwrap();
    assert!(!decoded.include_deleted);
    assert_eq!(decoded.max_created_at, None);
    assert_eq!(decoded.max_expires_at, None);
    let encoded = serde_json::to_value(
        filter()
            .created_before(-7)
            .expires_before(100)
            .include_deleted(),
    )
    .unwrap();
    let decoded: NoteFilter = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), encoded);
}
