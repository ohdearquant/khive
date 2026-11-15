//! Archive status selection through the public storage interfaces.
//! Public backend fixture follows tests/contract/backend.rs; mutation admission
//! fixture follows tests/note_filter_time_bounds.rs. No manifest registration change.
use std::sync::Arc;
use std::time::Duration;

use khive_db::pool::{ConnectionPool, PoolConfig};
use khive_db::stores::note::SqlNoteStore;
use khive_db::StorageBackend;
use khive_storage::note::{
    FilterOp, Note, NoteFilter, NoteInstantSeekAfter, NoteSeekAfter, NoteStatusSelector as Status,
    NoteTimeOrder, PropertyFilter, SortDir,
};
use khive_storage::types::{PageRequest, SqlValue};
use khive_storage::{NoteStore, StorageCapability, StorageError};
use serde_json::json;
use uuid::Uuid;

const ONE: &str = "status-selector-one";
const TWO: &str = "status-selector-two";
const KIND: &str = "observation";

async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("selector fixture operation timed out")
}
fn id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}
fn ids(notes: &[Note]) -> Vec<Uuid> {
    notes.iter().map(|note| note.id).collect()
}
fn expected(values: &[u128]) -> Vec<Uuid> {
    values.iter().map(|value| id(*value)).collect()
}
fn page(offset: u64, limit: u32) -> PageRequest {
    PageRequest { offset, limit }
}
fn filter(status: Option<Status>) -> NoteFilter {
    NoteFilter {
        kind: Some(KIND.into()),
        note_status: status,
        ..Default::default()
    }
}
fn row(value: u128, namespace: &str, kind: &str, status: &str, created: i64, dead: bool) -> Note {
    let mut note = Note::new(namespace, kind, "selector fixture");
    note.id = id(value);
    note.name = Some(format!("selector note {value}"));
    note.key = Some(format!("selector-{value:02}"));
    note.status = status.into();
    note.created_at = created;
    note.updated_at = created;
    note.expires_at = Some(value as i64 * 10);
    note.deleted_at = dead.then_some(2000);
    // Deliberately contradict top-level status; workflow filtering stays separate.
    note.properties = Some(json!({
        "status": if status == "active" { "archived" } else { "active" },
        "due": format!("2026-10-27T00:00:{value:02}Z"),
        "untouched": [1, true, null]
    }));
    note
}
async fn seeded_backend() -> StorageBackend {
    let backend = StorageBackend::memory().expect("in-memory backend");
    backend
        .prepare_core_schema()
        .expect("prepare in-memory schema");
    for (value, namespace, kind, status, created, dead) in [
        (1, ONE, KIND, "archived", 900, false),
        (2, ONE, KIND, "active", 800, false),
        (3, ONE, KIND, "archived", 800, false),
        (4, ONE, KIND, "active", 800, false),
        (5, ONE, KIND, "deleted", 500, false),
        (6, ONE, KIND, "active", 1000, true),
        (7, ONE, KIND, "archived", 1100, true),
        (8, TWO, KIND, "active", 750, false),
        (9, TWO, KIND, "archived", 650, false),
        (10, ONE, "other-kind", "active", 1200, false),
        (11, ONE, KIND, "active", 600, false),
    ] {
        bounded(backend.upsert_note(row(value, namespace, kind, status, created, dead)))
            .await
            .unwrap();
    }
    backend
}

#[tokio::test]
async fn raw_population_explicit_status_and_workflow_predicates_stay_distinct() {
    let backend = seeded_backend().await;
    let raw = bounded(backend.query_notes_filtered(ONE, &NoteFilter::default(), page(0, 30)))
        .await
        .unwrap();
    assert_eq!(ids(&raw.items), expected(&[10, 1, 2, 3, 4, 11, 5]));
    assert_eq!(raw.total, Some(7));
    for selector in [None, Some(Status::Any)] {
        let selected = bounded(backend.query_notes_filtered(ONE, &filter(selector), page(0, 30)))
            .await
            .unwrap();
        assert_eq!(ids(&selected.items), expected(&[1, 2, 3, 4, 11, 5]));
        assert_eq!(selected.total, Some(6));
    }
    for (selector, want) in [
        (Status::Active, vec![2, 4, 11]),
        (Status::Archived, vec![1, 3]),
    ] {
        let selected =
            bounded(backend.query_notes_filtered(ONE, &filter(Some(selector)), page(0, 30)))
                .await
                .unwrap();
        assert_eq!(ids(&selected.items), expected(&want));
        assert_eq!(selected.total, Some(want.len() as u64));
    }
    // This tests storage composition of the shared helper. Ordinary runtime
    // defaulting is covered separately in the runtime crate.
    let defaulted = filter(None).with_default_note_status(Status::Active);
    assert_eq!(
        ids(
            &bounded(backend.query_notes_filtered(ONE, &defaulted, page(0, 30)))
                .await
                .unwrap()
                .items
        ),
        expected(&[2, 4, 11])
    );
    let mut workflow = filter(Some(Status::Archived));
    workflow.property_filters.push(PropertyFilter {
        json_path: "$.status".into(),
        op: FilterOp::Eq,
        value: SqlValue::Text("active".into()),
    });
    assert_eq!(
        ids(
            &bounded(backend.query_notes_filtered(ONE, &workflow, page(0, 30)))
                .await
                .unwrap()
                .items
        ),
        expected(&[1, 3])
    );
    workflow.note_status = Some(Status::Active);
    assert!(
        bounded(backend.query_notes_filtered(ONE, &workflow, page(0, 30)))
            .await
            .unwrap()
            .items
            .is_empty()
    );
    workflow.note_status = None;
    assert_eq!(
        ids(
            &bounded(backend.query_notes_filtered(ONE, &workflow, page(0, 30)))
                .await
                .unwrap()
                .items
        ),
        expected(&[1, 3, 5])
    );

    let with_dead = filter(Some(Status::Active)).include_deleted();
    assert_eq!(
        ids(
            &bounded(backend.query_notes_filtered(ONE, &with_dead, page(0, 30)))
                .await
                .unwrap()
                .items
        ),
        expected(&[6, 2, 4, 11])
    );
    let with_dead = filter(Some(Status::Archived)).include_deleted();
    assert_eq!(
        ids(
            &bounded(backend.query_notes_filtered(ONE, &with_dead, page(0, 30)))
                .await
                .unwrap()
                .items
        ),
        expected(&[7, 1, 3])
    );

    // Identity/key/scalar/raw reads remain status-neutral.
    assert_eq!(
        bounded(backend.get_note(id(1)))
            .await
            .unwrap()
            .unwrap()
            .status,
        "archived"
    );
    assert_eq!(
        ids(
            &bounded(backend.get_live_notes_by_key(ONE, "selector-01", Some(KIND)))
                .await
                .unwrap()
        ),
        expected(&[1])
    );
    let mut batch = ids(&bounded(backend.get_notes_batch(&[id(1), id(3)]))
        .await
        .unwrap());
    batch.sort();
    assert_eq!(batch, expected(&[1, 3]));
    assert!(bounded(backend.note_sequence(id(1)))
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        bounded(backend.count_notes(ONE, Some(KIND))).await.unwrap(),
        6
    );
    assert_eq!(
        bounded(backend.count_notes_in_namespaces(&[ONE.into(), TWO.into()], Some(KIND)))
            .await
            .unwrap(),
        8
    );
    let raw = bounded(backend.query_notes(ONE, Some(KIND), page(0, 30)))
        .await
        .unwrap();
    assert_eq!(ids(&raw.items), expected(&[1, 2, 3, 4, 11, 5]));
    let raw = bounded(backend.query_notes_count_free(ONE, Some(KIND), page(0, 30)))
        .await
        .unwrap();
    assert_eq!(raw.total, None);
    assert_eq!(ids(&raw.items), expected(&[1, 2, 3, 4, 11, 5]));
}

#[tokio::test]
async fn status_is_applied_before_counted_offsets_and_count_free_seek_limits() {
    let backend = seeded_backend().await;
    let active = filter(Some(Status::Active));
    let counted = bounded(backend.query_notes_filtered(ONE, &active, page(1, 1)))
        .await
        .unwrap();
    assert_eq!(counted.total, Some(3));
    assert_eq!(ids(&counted.items), expected(&[4]));
    let free = bounded(backend.query_notes_filtered_count_free(ONE, &active, page(1, 1)))
        .await
        .unwrap();
    assert_eq!(free.total, None);
    assert_eq!(ids(&free.items), expected(&[4]));
    let mut seek = active;
    for value in [2, 4, 11] {
        let result = bounded(backend.query_notes_filtered_count_free(ONE, &seek, page(0, 1)))
            .await
            .unwrap();
        assert_eq!(result.total, None);
        assert_eq!(ids(&result.items), expected(&[value]));
        let note = &result.items[0];
        seek.after = Some(NoteSeekAfter {
            created_at: note.created_at,
            id: note.id,
        });
    }
    // Includes tied timestamps (2, archived 3, 4) and the lower-timestamp arm.
    let end = bounded(backend.query_notes_filtered_count_free(ONE, &seek, page(0, 1)))
        .await
        .unwrap();
    assert_eq!(end.total, None);
    assert!(end.items.is_empty());
}

#[tokio::test]
async fn status_reaches_namespace_arms_and_single_statement_fallbacks() {
    let backend = seeded_backend().await;
    let mut active = filter(Some(Status::Active));
    active.namespaces = vec![ONE.into(), TWO.into(), ONE.into()];
    let counted = bounded(backend.query_notes_filtered("ignored-primary", &active, page(1, 2)))
        .await
        .unwrap();
    assert_eq!(counted.total, Some(4));
    assert_eq!(ids(&counted.items), expected(&[4, 8]));
    let free =
        bounded(backend.query_notes_filtered_count_free("ignored-primary", &active, page(1, 2)))
            .await
            .unwrap();
    assert_eq!(free.total, None);
    assert_eq!(ids(&free.items), expected(&[4, 8]));
    active.unordered = true;
    let free =
        bounded(backend.query_notes_filtered_count_free("ignored-primary", &active, page(0, 30)))
            .await
            .unwrap();
    let mut found = ids(&free.items);
    found.sort();
    assert_eq!(found, expected(&[2, 4, 8, 11]));
    assert_eq!(free.total, None);
    active.unordered = false;
    active.include_deleted = true;
    let free =
        bounded(backend.query_notes_filtered_count_free("ignored-primary", &active, page(0, 2)))
            .await
            .unwrap();
    assert_eq!(ids(&free.items), expected(&[6, 2]));
    assert_eq!(free.total, None);
}

#[tokio::test]
async fn keyed_and_insertion_sequence_cursors_use_only_selected_rows() {
    let backend = seeded_backend().await;
    for (selector, keyed, sequence) in [
        (None, vec![1, 4, 3, 2, 11, 5], vec![1, 2, 3, 4, 5, 11]),
        (
            Some(Status::Any),
            vec![1, 4, 3, 2, 11, 5],
            vec![1, 2, 3, 4, 5, 11],
        ),
        (Some(Status::Active), vec![4, 2, 11], vec![2, 4, 11]),
        (Some(Status::Archived), vec![1, 3], vec![1, 3]),
    ] {
        let selected = filter(selector);
        let mut cursor = None;
        for (index, value) in keyed.iter().enumerate() {
            let (notes, next) = bounded(backend.query_keyed_notes(
                ONE,
                &selected,
                "selector-",
                cursor.as_ref(),
                page(0, 1),
            ))
            .await
            .unwrap();
            assert_eq!(ids(&notes), expected(&[*value]));
            assert_eq!(next.is_some(), index + 1 < keyed.len());
            if let Some(boundary) = &next {
                assert_eq!(boundary.id, id(*value));
            }
            cursor = next;
        }
        let mut cursor = None;
        for (index, value) in sequence.iter().enumerate() {
            let previous_sequence = cursor
                .as_ref()
                .map(|c: &khive_storage::types::SeekCursor| c.sequence);
            let result = bounded(backend.query_notes_filtered_after(ONE, &selected, cursor, 1))
                .await
                .unwrap();
            assert_eq!(ids(&result.items), expected(&[*value]));
            assert_eq!(result.next_after.is_some(), index + 1 < sequence.len());
            if let Some(boundary) = &result.next_after {
                assert_eq!(boundary.id, id(*value));
                if let Some(previous) = previous_sequence {
                    assert!(boundary.sequence > previous);
                }
            }
            cursor = result.next_after;
        }
    }
}

#[tokio::test]
async fn instant_seek_and_native_expiry_order_filter_before_truncation() {
    let backend = seeded_backend().await;
    let mut instant = filter(Some(Status::Active));
    instant.order_by = Some(("$.due".into(), SortDir::Asc));
    instant.order_by_instant = true;
    for value in [2, 4, 11] {
        let result = bounded(backend.query_notes_filtered_count_free(ONE, &instant, page(0, 1)))
            .await
            .unwrap();
        assert_eq!(result.total, None);
        assert_eq!(ids(&result.items), expected(&[value]));
        instant.after_instant = Some(NoteInstantSeekAfter {
            value: format!("2026-10-27T00:00:{value:02}Z"),
            id: id(value),
        });
    }
    assert!(
        bounded(backend.query_notes_filtered_count_free(ONE, &instant, page(0, 1)))
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let mut expiry = filter(Some(Status::Active));
    expiry.time_order = Some(NoteTimeOrder::ExpiresAt);
    let counted = bounded(backend.query_notes_filtered(ONE, &expiry, page(0, 1)))
        .await
        .unwrap();
    assert_eq!(counted.total, Some(3));
    assert_eq!(ids(&counted.items), expected(&[2]));
    assert_eq!(
        ids(
            &bounded(backend.query_notes_filtered_bounded(ONE, &expiry, 1))
                .await
                .unwrap()
        ),
        expected(&[2, 4])
    );
}

#[tokio::test]
async fn snapshot_counts_and_bounds_apply_to_each_selected_population() {
    let backend = seeded_backend().await;
    let filters = [
        filter(None),
        filter(Some(Status::Any)),
        filter(Some(Status::Active)),
        filter(Some(Status::Archived)),
        filter(Some(Status::Active)).include_deleted(),
    ];
    assert_eq!(
        bounded(backend.count_notes_filtered_in_snapshot(ONE, &filters))
            .await
            .unwrap(),
        vec![6, 6, 3, 2, 4]
    );
    let bounded_counts =
        bounded(backend.count_notes_filtered_bounded_in_snapshot(ONE, &filters, 2))
            .await
            .unwrap();
    assert_eq!(
        bounded_counts
            .iter()
            .map(|count| (count.count, count.cap, count.saturated))
            .collect::<Vec<_>>(),
        vec![
            (2, 2, true),
            (2, 2, true),
            (2, 2, true),
            (2, 2, false),
            (2, 2, true)
        ]
    );
    let exact = bounded(backend.count_notes_filtered_bounded_in_snapshot(
        ONE,
        &[filter(Some(Status::Active))],
        3,
    ))
    .await
    .unwrap();
    assert_eq!((exact[0].count, exact[0].saturated), (3, false));
    let zero = bounded(backend.count_notes_filtered_bounded_in_snapshot(
        ONE,
        &[filter(Some(Status::Archived))],
        0,
    ))
    .await
    .unwrap();
    assert_eq!((zero[0].count, zero[0].saturated), (0, true));
    assert_eq!(
        ids(
            &bounded(backend.query_notes_filtered_bounded(ONE, &filter(Some(Status::Active)), 2))
                .await
                .unwrap()
        ),
        expected(&[2, 4, 11])
    );
    assert_eq!(
        ids(&bounded(backend.query_notes_filtered_bounded(
            ONE,
            &filter(Some(Status::Archived)),
            2
        ))
        .await
        .unwrap()),
        expected(&[1, 3])
    );
    assert_eq!(
        ids(
            &bounded(backend.query_notes_filtered_bounded(ONE, &filter(Some(Status::Active)), 0))
                .await
                .unwrap()
        ),
        expected(&[2])
    );
}

fn mutation_store() -> (Arc<ConnectionPool>, SqlNoteStore) {
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
        .autocommit(|conn| Ok(conn.execute_batch(include_str!("../sql/notes-ddl.sql"))?))
        .unwrap();
    let store = SqlNoteStore::new(pool.clone(), false);
    (pool, store)
}
fn assert_selector_refusal(error: StorageError, expected_operation: &str) {
    match error {
        StorageError::InvalidInput {
            capability: StorageCapability::Notes,
            operation,
            message,
        } => {
            assert_eq!(operation, expected_operation);
            assert_eq!(message, "note_status is a read-only selector");
        }
        other => panic!("expected read-selector refusal before writer admission, got {other:?}"),
    }
}

#[tokio::test]
async fn explicit_read_selectors_refuse_both_mutations_before_writer_admission() {
    let (pool, store) = mutation_store();
    bounded(store.upsert_note(row(21, ONE, KIND, "archived", 100, false)))
        .await
        .unwrap();
    let before = bounded(store.get_note(id(21))).await.unwrap().unwrap();
    for status in [Status::Active, Status::Archived] {
        let selected = filter(Some(status));
        let writes = pool.writer_acquisition_snapshot();
        let error = bounded(store.try_patch_note_property(
            id(21),
            ONE,
            &selected,
            "$.patched",
            json!(true),
            900,
        ))
        .await
        .unwrap_err();
        assert_selector_refusal(error, "try_patch_note_property");
        assert_eq!(pool.writer_acquisition_snapshot(), writes);
        assert_eq!(
            bounded(store.get_note(id(21))).await.unwrap(),
            Some(before.clone())
        );
        let writes = pool.writer_acquisition_snapshot();
        let error = bounded(store.patch_note_property_atomic(
            vec![id(21)],
            ONE,
            &selected,
            "$.patched",
            json!(true),
            901,
        ))
        .await
        .unwrap_err();
        assert_selector_refusal(error, "patch_note_property_atomic");
        assert_eq!(pool.writer_acquisition_snapshot(), writes);
        assert_eq!(
            bounded(store.get_note(id(21))).await.unwrap(),
            Some(before.clone())
        );
    }
}

#[tokio::test]
async fn neutral_mutations_still_patch_archived_live_rows_and_keep_tombstones_safe() {
    let (_pool, store) = mutation_store();
    for value in [21, 22] {
        bounded(store.upsert_note(row(value, ONE, KIND, "archived", 100, false)))
            .await
            .unwrap();
    }
    bounded(store.upsert_note(row(23, ONE, KIND, "archived", 100, true)))
        .await
        .unwrap();
    for selector in [None, Some(Status::Any)] {
        let neutral = filter(selector).include_deleted();
        let dead_before = bounded(store.get_note_including_deleted(id(23)))
            .await
            .unwrap();
        assert!(bounded(store.try_patch_note_property(
            id(21),
            ONE,
            &neutral,
            "$.scalar",
            json!(true),
            900
        ))
        .await
        .unwrap());
        bounded(store.patch_note_property_atomic(
            vec![id(21), id(22)],
            ONE,
            &neutral,
            "$.atomic",
            json!(true),
            901,
        ))
        .await
        .unwrap();
        for value in [21, 22] {
            let note = bounded(store.get_note(id(value))).await.unwrap().unwrap();
            assert_eq!(note.status, "archived");
            assert_eq!(note.properties.as_ref().unwrap()["atomic"], true);
            assert_eq!(
                note.properties.as_ref().unwrap()["untouched"],
                json!([1, true, null])
            );
        }
        assert!(!bounded(store.try_patch_note_property(
            id(23),
            ONE,
            &neutral,
            "$.scalar",
            json!(true),
            902
        ))
        .await
        .unwrap());
        let live_before = bounded(store.get_note(id(21))).await.unwrap();
        assert!(bounded(store.patch_note_property_atomic(
            vec![id(21), id(23)],
            ONE,
            &neutral,
            "$.must_rollback",
            json!(true),
            903
        ))
        .await
        .is_err());
        assert_eq!(bounded(store.get_note(id(21))).await.unwrap(), live_before);
        assert_eq!(
            bounded(store.get_note_including_deleted(id(23)))
                .await
                .unwrap(),
            dead_before
        );
    }
}
