//! Frozen SQL oracles are the comm selectors at 68766435897ad0db3c293d5701868d35de91a060.
//! Keep them independent of the new filter compiler and later comm SQL removal.
use std::sync::Arc;

use khive_db::pool::{ConnectionPool, PoolConfig};
use khive_db::sql_bridge::SqlBridge;
use khive_db::stores::note::SqlNoteStore;
use khive_storage::note::{
    FilterOp, Note, NoteExpiryFallback, NoteFilter, NoteInstantSeekAfter, NoteSeekAfter,
    NoteTimeOrder, PropertyFilter, SortDir,
};
use khive_storage::types::{PageRequest, SqlStatement, SqlValue};
use khive_storage::{NoteStore, SqlAccess, StorageCapability, StorageError};
use serde_json::{json, Value};
use uuid::Uuid;

const LEGACY_SQL: &str = "SELECT id FROM notes
WHERE namespace = ?1 AND kind = 'message'
  AND ((expires_at IS NOT NULL AND expires_at <= ?2)
       OR (expires_at IS NULL AND created_at <= ?3))
  AND json_extract(properties, '$.channel_kind') = ?4
  AND (json_type(properties, '$.channel_slug') IS NULL
       OR json_type(properties, '$.channel_slug') = 'null'
       OR (json_type(properties, '$.channel_slug') = 'text'
           AND trim(json_extract(properties, '$.channel_slug')) = ''))
  AND (json_extract(properties, '$.quarantined') = 'true'
       OR json_type(properties, '$.quarantined') = 'true')
ORDER BY COALESCE(expires_at, created_at), id LIMIT 128";
const CHANNEL_SQL: &str = "SELECT id FROM notes
WHERE namespace = ?1 AND kind = 'message' AND deleted_at IS NULL
  AND expires_at IS NOT NULL AND expires_at <= ?2
  AND json_extract(properties, '$.channel_kind') = ?3
  AND json_extract(properties, '$.channel_slug') = ?4
  AND (json_extract(properties, '$.quarantined') = 'true'
       OR json_type(properties, '$.quarantined') = 'true')
ORDER BY expires_at, id LIMIT 128";

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

fn property(path: &str, op: FilterOp, value: SqlValue) -> PropertyFilter {
    PropertyFilter {
        json_path: path.into(),
        op,
        value,
    }
}

fn filter(legacy: bool) -> NoteFilter {
    let mut filter = NoteFilter {
        kind: Some("message".into()),
        property_filters: vec![
            property(
                "$.channel_kind",
                FilterOp::Eq,
                SqlValue::Text("mail".into()),
            ),
            property("$.quarantined", FilterOp::TrueOrTextTrue, SqlValue::Null),
        ],
        ..Default::default()
    };
    if legacy {
        filter.include_deleted = true;
        filter.expiry_fallback = Some(NoteExpiryFallback {
            expires_at_or_before: 1000,
            created_at_or_before: 100,
        });
        filter.time_order = Some(NoteTimeOrder::ExpiresAtOrCreatedAt);
        filter.property_filters.push(property(
            "$.channel_slug",
            FilterOp::MissingNullOrSpaceEmptyText,
            SqlValue::Null,
        ));
    } else {
        filter = filter.expires_before(1000);
        filter.time_order = Some(NoteTimeOrder::ExpiresAt);
        filter.property_filters.push(property(
            "$.channel_slug",
            FilterOp::Eq,
            SqlValue::Text("inbox".into()),
        ));
    }
    filter
}

fn row(id: u128, created: i64, expiry: Option<i64>, properties: Value, dead: bool) -> Note {
    let mut note = Note::new("one", "message", "cleanup fixture");
    note.id = Uuid::from_u128(id);
    note.created_at = created;
    note.updated_at = created;
    note.expires_at = expiry;
    note.deleted_at = dead.then_some(777);
    note.properties = Some(properties);
    note
}

fn ids(rows: &[Note]) -> Vec<Uuid> {
    rows.iter().map(|n| n.id).collect()
}

async fn oracle(pool: Arc<ConnectionPool>, legacy: bool, limit: u32) -> Vec<Uuid> {
    let (sql, params) = if legacy {
        (
            LEGACY_SQL,
            vec![
                SqlValue::Text("one".into()),
                SqlValue::Integer(1000),
                SqlValue::Integer(100),
                SqlValue::Text("mail".into()),
            ],
        )
    } else {
        (
            CHANNEL_SQL,
            vec![
                SqlValue::Text("one".into()),
                SqlValue::Integer(1000),
                SqlValue::Text("mail".into()),
                SqlValue::Text("inbox".into()),
            ],
        )
    };
    SqlBridge::new(pool, false)
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement {
            sql: sql.replace("LIMIT 128", &format!("LIMIT {limit}")),
            params,
            label: Some("cleanup-selector-oracle".into()),
        })
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            let Some(SqlValue::Text(id)) = row.get("id") else {
                panic!("oracle ID type")
            };
            Uuid::parse_str(id).unwrap()
        })
        .collect()
}

async fn assert_pages(
    pool: Arc<ConnectionPool>,
    store: &SqlNoteStore,
    legacy: bool,
    expected: &[Uuid],
) {
    let filter = filter(legacy);
    assert!(
        expected.len() > 128,
        "fixture must cross the cleanup page boundary"
    );
    assert_eq!(oracle(pool.clone(), legacy, 10000).await, expected);
    assert_eq!(oracle(pool, legacy, 128).await, expected[..128]);
    let page = PageRequest {
        offset: 0,
        limit: 128,
    };
    let counted = store
        .query_notes_filtered("one", &filter, page.clone())
        .await
        .unwrap();
    assert_eq!(counted.total, Some(expected.len() as u64));
    assert_eq!(ids(&counted.items), expected[..128]);
    let first = store
        .query_notes_filtered_count_free("one", &filter, page)
        .await
        .unwrap();
    assert_eq!(first.total, None);
    assert_eq!(ids(&first.items), expected[..128]);
    let mut all = Vec::new();
    for offset in (0..expected.len()).step_by(128) {
        let next = store
            .query_notes_filtered_count_free(
                "one",
                &filter,
                PageRequest {
                    offset: offset as u64,
                    limit: 128,
                },
            )
            .await
            .unwrap();
        all.extend(ids(&next.items));
    }
    assert_eq!(all, expected, "no starvation or loss across bounded pages");
    // The bounded API deliberately probes max_rows + 1 to expose overflow.
    let bounded = store
        .query_notes_filtered_bounded("one", &filter, 128)
        .await
        .unwrap();
    assert_eq!(ids(&bounded), expected[..129]);
    assert_eq!(
        store
            .count_notes_filtered_in_snapshot("one", std::slice::from_ref(&filter))
            .await
            .unwrap(),
        vec![expected.len() as u64]
    );
    let counts = store
        .count_notes_filtered_bounded_in_snapshot("one", &[filter], 128)
        .await
        .unwrap();
    assert_eq!(counts[0].count, 128);
    assert!(counts[0].saturated);
}

#[tokio::test]
async fn legacy_fallback_and_json_types_match_original_sql_before_limit() {
    let (pool, store) = setup();
    // These rows lead both creation-descending and effective-expiry-ascending order,
    // so post-filtering any 128-row precursor page would starve all valid results.
    for id in 1..=160 {
        store
            .upsert_note(row(
                id,
                10000,
                Some(-10000),
                json!({"channel_kind":"mail", "quarantined":1}),
                false,
            ))
            .await
            .unwrap();
    }
    let mut accepted = Vec::new();
    for n in 0..260_u128 {
        let id = 1000 + n;
        let created = 100 - (n % 25) as i64;
        let expiry = (n % 2 == 0).then_some(1000 - (n % 25) as i64);
        let slug = match n % 3 {
            0 => Value::Null,
            1 => json!(""),
            _ => json!("   "),
        };
        let truth = if n % 2 == 0 {
            json!(true)
        } else {
            json!("true")
        };
        store
            .upsert_note(row(
                id,
                created,
                expiry,
                json!({"channel_kind":"mail", "channel_slug":slug, "quarantined":truth}),
                n % 3 == 0,
            ))
            .await
            .unwrap();
        accepted.push((expiry.unwrap_or(created), Uuid::from_u128(id)));
    }
    // Inclusive boundaries, missing slug, expiry branch precedence, and JSON types.
    let cases = [
        (
            2001,
            100,
            None,
            json!({"channel_kind":"mail", "quarantined":true}),
            true,
        ),
        (
            2002,
            5000,
            Some(1000),
            json!({"channel_kind":"mail", "channel_slug":null, "quarantined":"true"}),
            true,
        ),
        (
            2003,
            101,
            None,
            json!({"channel_kind":"mail", "quarantined":true}),
            false,
        ),
        (
            2004,
            -5000,
            Some(1001),
            json!({"channel_kind":"mail", "quarantined":true}),
            false,
        ),
        (
            2005,
            0,
            None,
            json!({"channel_kind":"mail", "channel_slug":"\t", "quarantined":true}),
            false,
        ),
        (
            2006,
            0,
            None,
            json!({"channel_kind":"mail", "channel_slug":"\u{a0}", "quarantined":true}),
            false,
        ),
        (
            2007,
            0,
            None,
            json!({"channel_kind":"mail", "channel_slug":false, "quarantined":true}),
            false,
        ),
        (
            2008,
            0,
            None,
            json!({"channel_kind":"mail", "channel_slug":[], "quarantined":true}),
            false,
        ),
        (
            2009,
            0,
            None,
            json!({"channel_kind":"mail", "channel_slug":0, "quarantined":true}),
            false,
        ),
        (
            2010,
            0,
            None,
            json!({"channel_kind":"mail", "quarantined":false}),
            false,
        ),
        (
            2011,
            0,
            None,
            json!({"channel_kind":"mail", "quarantined":"TRUE"}),
            false,
        ),
        (
            2012,
            0,
            None,
            json!({"channel_kind":"mail", "quarantined":null}),
            false,
        ),
        (2013, 0, None, json!({"channel_kind":"mail"}), false),
        (
            2014,
            0,
            None,
            json!({"channel_kind":"other", "quarantined":true}),
            false,
        ),
        (
            2015,
            0,
            None,
            json!({"channel_kind":"mail", "channel_slug":"inbox", "quarantined":true}),
            false,
        ),
    ];
    for (id, created, expiry, props, include) in cases {
        store
            .upsert_note(row(id, created, expiry, props, true))
            .await
            .unwrap();
        if include {
            accepted.push((expiry.unwrap_or(created), Uuid::from_u128(id)));
        }
    }
    for (id, namespace, kind) in [(3001, "other", "message"), (3002, "one", "task")] {
        let mut note = row(
            id,
            0,
            None,
            json!({"channel_kind":"mail", "quarantined":true}),
            false,
        );
        note.namespace = namespace.into();
        note.kind = kind.into();
        store.upsert_note(note).await.unwrap();
    }
    accepted.sort();
    let expected: Vec<_> = accepted.into_iter().map(|(_, id)| id).collect();
    assert_eq!(expected.len(), 262);
    assert_pages(pool, &store, true, &expected).await;
    // Existing upper bounds are additional constraints, not overwritten by fallback.
    let mut restricted = filter(true).created_before(100).expires_before(1000);
    restricted.time_order = None;
    assert_eq!(
        store
            .count_notes_filtered_in_snapshot("one", &[restricted])
            .await
            .unwrap(),
        vec![130]
    );
}

#[tokio::test]
async fn channel_expiry_order_and_live_only_rows_match_original_sql_before_limit() {
    let (pool, store) = setup();
    for id in 1..=160 {
        store
            .upsert_note(row(
                id,
                10000,
                Some(-10000),
                json!({"channel_kind":"mail", "channel_slug":"other", "quarantined":true}),
                false,
            ))
            .await
            .unwrap();
    }
    let mut accepted = Vec::new();
    for n in 0..260_u128 {
        let id = 1000 + n;
        let expiry = 1000 - (n % 25) as i64;
        store.upsert_note(row(id, n as i64, Some(expiry),
            json!({"channel_kind":"mail", "channel_slug":"inbox", "quarantined":if n%2==0 {json!(true)} else {json!("true")}}), false)).await.unwrap();
        accepted.push((expiry, Uuid::from_u128(id)));
    }
    for (id, expiry, dead, truth) in [
        (2001, Some(1000), true, json!(true)),
        (2002, None, false, json!(true)),
        (2003, Some(1001), false, json!(true)),
        (2004, Some(0), false, json!(1)),
        (2005, Some(0), false, json!(false)),
    ] {
        store
            .upsert_note(row(
                id,
                -100,
                expiry,
                json!({"channel_kind":"mail", "channel_slug":"inbox", "quarantined":truth}),
                dead,
            ))
            .await
            .unwrap();
    }
    accepted.sort();
    let expected: Vec<_> = accepted.into_iter().map(|(_, id)| id).collect();
    assert_pages(pool, &store, false, &expected).await;
}

fn assert_invalid(error: StorageError, operation: &str) {
    assert!(matches!(error, StorageError::InvalidInput {
        capability: StorageCapability::Notes, operation: actual, ..
    } if actual == operation));
}

#[tokio::test]
async fn native_order_refuses_incompatible_pages_cursors_and_mutations() {
    let (_pool, store) = setup();
    let mut variants = Vec::new();
    let mut f = filter(true);
    f.order_by = Some(("$.rank".into(), SortDir::Asc));
    variants.push(f);
    let mut f = filter(true);
    f.order_by_instant = true;
    variants.push(f);
    let mut f = filter(true);
    f.unordered = true;
    variants.push(f);
    let mut f = filter(true);
    f.after = Some(NoteSeekAfter {
        created_at: 0,
        id: Uuid::nil(),
    });
    variants.push(f);
    let mut f = filter(true);
    f.after_instant = Some(NoteInstantSeekAfter {
        value: "2020-01-01T00:00:00Z".into(),
        id: Uuid::nil(),
    });
    variants.push(f);
    for f in variants {
        assert_invalid(
            store
                .query_notes_filtered("one", &f, PageRequest::default())
                .await
                .unwrap_err(),
            "query_notes_filtered",
        );
        assert_invalid(
            store
                .query_notes_filtered_count_free("one", &f, PageRequest::default())
                .await
                .unwrap_err(),
            "query_notes_filtered_count_free",
        );
        assert_invalid(
            store
                .query_notes_filtered_bounded("one", &f, 128)
                .await
                .unwrap_err(),
            "query_notes_filtered_bounded",
        );
    }
    let f = filter(true);
    assert_invalid(
        store
            .query_keyed_notes("one", &f, "", None, PageRequest::default())
            .await
            .unwrap_err(),
        "query_keyed_notes",
    );
    assert_invalid(
        store
            .query_notes_filtered_after("one", &f, None, 0)
            .await
            .unwrap_err(),
        "query_notes_filtered_after",
    );
    let note = row(
        1,
        0,
        None,
        json!({"channel_kind":"mail","quarantined":true}),
        false,
    );
    store.upsert_note(note.clone()).await.unwrap();
    assert_invalid(
        store
            .try_patch_note_property(note.id, "one", &f, "$.touched", json!(true), 900)
            .await
            .unwrap_err(),
        "try_patch_note_property",
    );
    assert_invalid(
        store
            .patch_note_property_atomic(vec![note.id], "one", &f, "$.touched", json!(true), 900)
            .await
            .unwrap_err(),
        "patch_note_property_atomic",
    );
    assert_eq!(store.get_note(note.id).await.unwrap(), Some(note.clone()));
    let dead = row(
        2,
        0,
        None,
        json!({"channel_kind":"mail","quarantined":true}),
        true,
    );
    store.upsert_note(dead.clone()).await.unwrap();
    for op in [
        FilterOp::MissingNullOrSpaceEmptyText,
        FilterOp::TrueOrTextTrue,
    ] {
        let f = NoteFilter {
            property_filters: vec![property("$.bad'path", op, SqlValue::Null)],
            ..Default::default()
        };
        assert_invalid(
            store
                .query_notes_filtered_count_free("one", &f, PageRequest::default())
                .await
                .unwrap_err(),
            "query_notes_filtered",
        );
        assert_invalid(
            store
                .try_patch_note_property(
                    Uuid::from_u128(1),
                    "one",
                    &f,
                    "$.touched",
                    json!(true),
                    900,
                )
                .await
                .unwrap_err(),
            "query_notes_filtered",
        );
        assert_invalid(
            store
                .patch_note_property_atomic(
                    vec![note.id, dead.id],
                    "one",
                    &f,
                    "$.touched",
                    json!(true),
                    900,
                )
                .await
                .unwrap_err(),
            "query_notes_filtered",
        );
        assert_invalid(
            store
                .try_patch_note_property(dead.id, "one", &f, "$.touched", json!(true), 900)
                .await
                .unwrap_err(),
            "query_notes_filtered",
        );
        assert_eq!(store.get_note(note.id).await.unwrap(), Some(note.clone()));
        assert_eq!(
            store.get_note_including_deleted(dead.id).await.unwrap(),
            Some(dead.clone())
        );
    }
    let mut valid = filter(true);
    valid.time_order = None;
    assert!(store
        .try_patch_note_property(note.id, "one", &valid, "$.touched", json!(true), 900)
        .await
        .unwrap());
    let changed = store.get_note(note.id).await.unwrap().unwrap();
    assert_eq!(changed.properties.unwrap()["touched"], true);
    assert_eq!(changed.version, note.version + 1);
    assert_eq!(
        store.get_note_including_deleted(dead.id).await.unwrap(),
        Some(dead)
    );
}

#[test]
fn new_typed_selectors_keep_legacy_defaults_and_roundtrip() {
    let legacy: NoteFilter = serde_json::from_value(json!({})).unwrap();
    assert!(legacy.expiry_fallback.is_none());
    assert!(legacy.time_order.is_none());
    for legacy in [true, false] {
        let value = serde_json::to_value(filter(legacy)).unwrap();
        let decoded: NoteFilter = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), value);
    }
}
