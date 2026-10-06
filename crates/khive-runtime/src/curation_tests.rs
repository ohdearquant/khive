use std::sync::Arc;
use std::sync::Mutex;

use super::*;
use crate::runtime::{KhiveRuntime, NamespaceToken};
use khive_storage::types::{Direction, TextFilter, TextQueryMode, TextSearchRequest, VectorRecord};
use khive_types::EndpointKind;

fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().unwrap()
}

fn set_merge_event_refusal(rt: &KhiveRuntime, kind: &str, refuse: bool) {
    let pool = rt.backend().pool_arc();
    let guard = pool.writer().expect("acquire fixture writer");
    let sql = if refuse {
        format!(
            "CREATE TRIGGER reject_merge_event BEFORE INSERT ON events \
                 WHEN NEW.kind = '{kind}' BEGIN SELECT RAISE(ABORT, 'blocked merge event'); END"
        )
    } else {
        "DROP TRIGGER reject_merge_event".to_string()
    };
    guard.conn().execute_batch(&sql).expect("set event fault");
}

async fn seed_health_identity_note(runtime: &KhiveRuntime, slug: &str) -> Note {
    let mut note = Note::new("local", "channel_health", "health state");
    note.properties = Some(serde_json::json!({
        "channel_kind": "email", "channel_slug": slug, "operator_note": "original"
    }));
    runtime
        .notes(&NamespaceToken::local())
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    note
}

/// Must fail without the runtime coordinate guard: the first plain update
/// succeeds and changes channel_kind even though no pack hook ran.
#[tokio::test]
async fn channel_health_identity_plain_update_refuses_coordinate_changes() {
    let runtime = rt();
    let token = NamespaceToken::local();
    let note = seed_health_identity_note(&runtime, "original@example.com").await;
    let before = serde_json::to_value(&note).unwrap();
    for properties in [
        serde_json::json!({"channel_kind": "telegram", "operator_note": "changed"}),
        serde_json::json!({"channel_slug": "other@example.com"}),
        serde_json::json!({"channel_kind": null}),
        serde_json::json!({"channel_slug": null}),
        serde_json::json!({"channel_kind": "email"}),
        serde_json::json!({"channel_slug": "original@example.com"}),
        serde_json::json!(null),
        serde_json::json!([]),
    ] {
        let error = runtime
            .update_note(
                &token,
                note.id,
                NotePatch::new(None, None, None, None, Some(properties)),
            )
            .await
            .expect_err("plain runtime update must preserve heartbeat coordinates");
        assert!(error.to_string().contains("channel_health"), "{error}");
        let stored = runtime
            .notes(&token)
            .unwrap()
            .get_note(note.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(serde_json::to_value(stored).unwrap(), before);
    }
    let changed = runtime
        .update_note(
            &token,
            note.id,
            NotePatch::new(
                None,
                None,
                None,
                None,
                Some(serde_json::json!({"operator_note": "changed"})),
            ),
        )
        .await
        .unwrap();
    let properties = changed.properties.unwrap();
    assert_eq!(properties["channel_kind"], "email");
    assert_eq!(properties["channel_slug"], "original@example.com");
    assert_eq!(properties["operator_note"], "changed");
}

#[tokio::test]
async fn channel_health_identity_atomic_prepare_refuses_coordinates() {
    let runtime = rt();
    let token = NamespaceToken::local();
    let note = seed_health_identity_note(&runtime, "original@example.com").await;
    let error = crate::atomic_prepare::prepare_update(
        &runtime,
        &token,
        &serde_json::json!({
            "id": note.id.to_string(),
            "properties": {"channel_slug": "other@example.com"}
        }),
        None,
    )
    .await
    .expect_err("atomic/proposal preparation must use the runtime identity guard");
    assert!(error.to_string().contains("channel_slug"), "{error}");
    let stored = runtime
        .notes(&token)
        .unwrap()
        .get_note(note.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(stored).unwrap(),
        serde_json::to_value(note).unwrap()
    );
}

/// Must fail without merge restoration: PreferFrom replaces the surviving
/// row's coordinates with the absorbed row's values.
#[tokio::test]
async fn channel_health_identity_merge_retains_survivor_coordinates() {
    for strategy in [
        EntityDedupMergePolicy::PreferFrom,
        EntityDedupMergePolicy::Union,
        EntityDedupMergePolicy::PreferInto,
    ] {
        let runtime = rt();
        let token = NamespaceToken::local();
        let into = seed_health_identity_note(&runtime, "survivor@example.com").await;
        let mut from = seed_health_identity_note(&runtime, "absorbed@example.com").await;
        from.properties = Some(serde_json::json!({
            "channel_kind": "telegram", "channel_slug": "absorbed@example.com", "new_metadata": true
        }));
        // Trusted fixture setup establishes a distinct source identity;
        // the public store must refuse changing an existing health row.
        runtime
            .raw_notes(&token)
            .unwrap()
            .upsert_note(from.clone())
            .await
            .unwrap();
        runtime
            .merge_note(
                &token,
                into.id,
                from.id,
                strategy,
                ContentMergeStrategy::PreferInto,
                false,
            )
            .await
            .unwrap();
        let stored = runtime
            .notes(&token)
            .unwrap()
            .get_note(into.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.id, into.id);
        assert_eq!(stored.created_at, into.created_at);
        let properties = stored.properties.unwrap();
        assert_eq!(properties["channel_kind"], "email");
        assert_eq!(properties["channel_slug"], "survivor@example.com");
        assert_eq!(properties["new_metadata"], true);
        assert!(runtime
            .notes(&token)
            .unwrap()
            .get_note(from.id)
            .await
            .unwrap()
            .is_none());
    }
}

async fn entity_update_events(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
) -> Vec<khive_storage::event::Event> {
    runtime
        .events(token)
        .unwrap()
        .query_events(
            khive_storage::event::EventFilter {
                kinds: vec![EventKind::EntityUpdated],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                limit: 100,
                offset: 0,
            },
        )
        .await
        .unwrap()
        .items
}

fn outbound_message_note() -> Note {
    let mut note = Note::new("local", "message", "hello");
    note.properties = Some(serde_json::json!({"direction": "outbound"}));
    note
}

/// Predicate + ordering contract of the non-wire outbox scan: outbound
/// with absent OR explicitly-null `delivered_at` is undelivered; a
/// non-null `delivered_at`, a terminal `delivery` state, an inbound row,
/// and a soft-deleted row are all excluded; results come newest-first
/// and respect `limit`; `limit=0` returns nothing.
#[tokio::test]
async fn list_undelivered_outbound_messages_predicate_and_order() {
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let tok = NamespaceToken::local();
    let store = rt.notes(&tok).expect("note store");

    let mut undelivered_old = outbound_message_note();
    undelivered_old.created_at -= 10;
    let mut undelivered_null = outbound_message_note();
    undelivered_null.properties =
        Some(serde_json::json!({"direction": "outbound", "delivered_at": null}));
    let mut delivered = outbound_message_note();
    delivered.properties =
        Some(serde_json::json!({"direction": "outbound", "delivered_at": "2026-08-28T00:00:00Z"}));
    // ADR-122 terminal states without `delivered_at` are not pending.
    let mut terminal_failed = outbound_message_note();
    terminal_failed.properties =
        Some(serde_json::json!({"direction": "outbound", "delivery": "failed", "last_error": "x"}));
    let mut inbound = Note::new("local", "message", "inbound row");
    inbound.properties = Some(serde_json::json!({"direction": "inbound"}));
    let mut soft_deleted = outbound_message_note();
    soft_deleted.deleted_at = Some(chrono::Utc::now().timestamp_micros());

    let old_id = undelivered_old.id;
    let null_id = undelivered_null.id;
    for note in [
        undelivered_old,
        undelivered_null,
        delivered,
        terminal_failed,
        inbound,
        soft_deleted,
    ] {
        store.upsert_note(note).await.expect("seed note");
    }

    let hits = rt
        .list_undelivered_outbound_messages(&tok, None, 200)
        .await
        .expect("scan succeeds");
    let ids: Vec<_> = hits.iter().map(|n| n.id).collect();
    assert_eq!(
        ids,
        vec![null_id, old_id],
        "only the two undelivered outbound rows, newest-first"
    );

    let capped = rt
        .list_undelivered_outbound_messages(&tok, None, 1)
        .await
        .expect("capped scan succeeds");
    assert_eq!(
        capped.iter().map(|n| n.id).collect::<Vec<_>>(),
        vec![null_id],
        "limit truncates after the newest undelivered row"
    );

    let zero = rt
        .list_undelivered_outbound_messages(&tok, None, 0)
        .await
        .expect("zero-limit scan succeeds");
    assert!(zero.is_empty(), "limit=0 returns no rows, not one");
}

/// Regression guard for the scan window (#1859). The pending predicate
/// admits every actor-to-actor outbound row forever (nothing marks them
/// delivered), so the pending population outgrows any scan cap. With
/// more such rows than the cap, an email row that sorts past the cap in
/// every candidate index order (older `created_at`, later rowid, a
/// `to_actor` that collates after the fillers') must still be returned:
/// the channel prefix has to bound the SQL candidate set, not trim it
/// afterwards. `list_undelivered_outbound_messages_matches_legacy_predicate`
/// is the under-cap control.
#[tokio::test]
async fn list_undelivered_outbound_messages_email_row_past_the_scan_window() {
    const FILLERS: usize = 10_050;
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let tok = NamespaceToken::local();
    let store = rt.notes(&tok).expect("note store");

    let mut email = outbound_message_note();
    email.created_at -= 1_000_000;
    email.updated_at = email.created_at;
    email.properties =
        Some(serde_json::json!({"direction": "outbound", "to_actor": "email:ocean@example.test"}));
    let email_id = email.id;

    let mut fillers = Vec::with_capacity(FILLERS);
    for i in 0..FILLERS {
        let mut filler = outbound_message_note();
        filler.created_at += i as i64;
        filler.updated_at = filler.created_at;
        filler.properties =
            Some(serde_json::json!({"direction": "outbound", "to_actor": "daemon:filler"}));
        fillers.push(filler);
    }
    // Fillers first so the email row also takes the later rowid.
    let summary = store.upsert_notes(fillers).await.expect("seed fillers");
    assert_eq!(
        summary.affected as usize, FILLERS,
        "every filler row seeded"
    );
    store.upsert_note(email).await.expect("seed email row");

    let hits = rt
        .list_undelivered_outbound_messages(&tok, Some("email:"), 200)
        .await
        .expect("scan succeeds");
    assert_eq!(
        hits.iter().map(|n| n.id).collect::<Vec<_>>(),
        vec![email_id],
        "the email row is found behind {FILLERS} pending actor-to-actor rows"
    );

    let daemon_hits = rt
        .list_undelivered_outbound_messages(&tok, Some("daemon:"), 3)
        .await
        .expect("scan succeeds");
    assert_eq!(
        daemon_hits.len(),
        3,
        "the other prefix still sees its own rows"
    );
}

fn legacy_outbox_pending(note: &Note, to_prefix: Option<&str>, now_micros: i64) -> bool {
    if note.deleted_at.is_some() {
        return false;
    }
    let props = note.properties.as_ref().and_then(|value| value.as_object());
    if props
        .and_then(|properties| properties.get("direction"))
        .and_then(Value::as_str)
        != Some("outbound")
    {
        return false;
    }
    if let Some(prefix) = to_prefix {
        let matches = props
            .and_then(|properties| properties.get("to_actor"))
            .and_then(Value::as_str)
            .is_some_and(|actor| actor.starts_with(prefix));
        if !matches {
            return false;
        }
    }
    if props
        .and_then(|properties| properties.get("delivered_at"))
        .is_some_and(|value| !value.is_null())
    {
        return false;
    }
    if props
        .and_then(|properties| properties.get("delivery"))
        .and_then(Value::as_str)
        .is_some_and(|state| state == "delivered" || state == "failed")
    {
        return false;
    }
    let retry_deferred = props
        .and_then(|properties| properties.get("next_attempt_at"))
        .and_then(Value::as_str)
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .is_some_and(|deadline| deadline.timestamp_micros() > now_micros);
    !retry_deferred
}

/// The SQL-prefiltered scan must return exactly what the former full-note
/// scan selected, including every legacy and channel-partition edge case.
#[tokio::test]
async fn list_undelivered_outbound_messages_matches_legacy_predicate() {
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let tok = NamespaceToken::local();
    let store = rt.notes(&tok).expect("note store");

    let make_note = |created_at, properties, deleted_at| {
        let mut note = Note::new("local", "message", "outbox fixture");
        note.created_at = created_at;
        note.updated_at = created_at;
        note.properties = Some(properties);
        note.deleted_at = deleted_at;
        note
    };
    let notes = vec![
        make_note(
            110,
            serde_json::json!({"direction": "outbound", "to_actor": "email:absent"}),
            None,
        ),
        make_note(
            109,
            serde_json::json!({
                "direction": "outbound",
                "to_actor": "email:null",
                "delivered_at": null
            }),
            None,
        ),
        make_note(
            108,
            serde_json::json!({
                "direction": "outbound",
                "to_actor": "email:terminal-failed",
                "delivery": "failed"
            }),
            None,
        ),
        make_note(
            107,
            serde_json::json!({
                "direction": "outbound",
                "to_actor": "email:terminal-delivered",
                "delivery": "delivered"
            }),
            None,
        ),
        make_note(
            106,
            serde_json::json!({
                "direction": "outbound",
                "to_actor": "email:malformed",
                "next_attempt_at": "not-a-timestamp"
            }),
            None,
        ),
        make_note(
            105,
            serde_json::json!({
                "direction": "outbound",
                "to_actor": "email:future",
                "next_attempt_at": "2999-01-01T00:00:00Z"
            }),
            None,
        ),
        make_note(
            104,
            serde_json::json!({
                "direction": "outbound",
                "to_actor": "email:due",
                "next_attempt_at": "2000-01-01T00:00:00Z"
            }),
            None,
        ),
        make_note(
            103,
            serde_json::json!({"direction": "inbound", "to_actor": "email:inbound"}),
            None,
        ),
        make_note(
            102,
            serde_json::json!({"direction": "outbound", "to_actor": "telegram:other"}),
            None,
        ),
        make_note(
            101,
            serde_json::json!({
                "direction": "outbound",
                "to_actor": "email:already-delivered",
                "delivered_at": "2026-08-28T00:00:00Z"
            }),
            None,
        ),
        make_note(
            100,
            serde_json::json!({"direction": "outbound", "to_actor": "email:deleted"}),
            Some(100),
        ),
        make_note(
            99,
            serde_json::json!({"direction": "outbound", "to_actor": "emailx:not-this-channel"}),
            None,
        ),
        make_note(
            98,
            serde_json::json!({"direction": "outbound", "to_actor": 42}),
            None,
        ),
    ];
    for note in &notes {
        store.upsert_note(note.clone()).await.expect("seed note");
    }

    let now_micros = chrono::Utc::now().timestamp_micros();
    let all_rows = rt
        .list_notes(&tok, Some("message"), 200, 0)
        .await
        .expect("legacy scan fixture loads");
    let expected_ids: Vec<_> = all_rows
        .iter()
        .filter(|note| legacy_outbox_pending(note, Some("email:"), now_micros))
        .map(|note| note.id)
        .collect();
    let actual_ids: Vec<_> = rt
        .list_undelivered_outbound_messages(&tok, Some("email:"), 200)
        .await
        .expect("filtered scan succeeds")
        .into_iter()
        .map(|note| note.id)
        .collect();

    assert_eq!(
        expected_ids.len(),
        4,
        "fixture must exercise all exclusions"
    );
    assert_eq!(
        actual_ids, expected_ids,
        "filtered scan changed answer or order"
    );
    let channel_ids: Vec<_> = rt
        .list_undelivered_outbound_messages_for_channel(&tok, "email:", "primary", true, 200)
        .await
        .expect("channel scan succeeds")
        .into_iter()
        .map(|note| note.id)
        .collect();
    assert_eq!(
        channel_ids, expected_ids,
        "legacy channel pass changed answer or order"
    );
}

#[tokio::test]
async fn list_undelivered_outbound_messages_skips_only_future_retry_deadlines() {
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let tok = NamespaceToken::local();
    let store = rt.notes(&tok).expect("note store");

    let mut future = outbound_message_note();
    future.properties = Some(serde_json::json!({
        "direction": "outbound",
        "next_attempt_at": "2999-01-01T00:00:00Z",
    }));
    let future_id = future.id;

    let mut overdue = outbound_message_note();
    overdue.properties = Some(serde_json::json!({
        "direction": "outbound",
        "next_attempt_at": "2000-01-01T00:00:00Z",
    }));
    let overdue_id = overdue.id;

    let mut malformed = outbound_message_note();
    malformed.properties = Some(serde_json::json!({
        "direction": "outbound",
        "next_attempt_at": "not-a-timestamp",
    }));
    let malformed_id = malformed.id;

    let mut relaxed_only = outbound_message_note();
    relaxed_only.properties = Some(serde_json::json!({
        "direction": "outbound",
        "next_attempt_at": "2999-01-01T00:00:00+0000",
    }));
    let relaxed_only_id = relaxed_only.id;

    for note in [future, overdue, malformed, relaxed_only] {
        store.upsert_note(note).await.expect("seed note");
    }

    let ids: HashSet<_> = rt
        .list_undelivered_outbound_messages(&tok, None, 200)
        .await
        .expect("scan succeeds")
        .into_iter()
        .map(|note| note.id)
        .collect();

    assert!(!ids.contains(&future_id), "future retry must stay parked");
    assert!(ids.contains(&overdue_id), "overdue retry must be eligible");
    assert!(
        ids.contains(&malformed_id),
        "malformed legacy retry state must fail open instead of stranding the note"
    );
    assert!(
        ids.contains(&relaxed_only_id),
        "a relaxed-only date is malformed under the former strict RFC 3339 parser"
    );
}

/// Future retries on the same channel must not fill the bounded SQL page
/// and hide an older message whose retry deadline has passed (#1760).
#[tokio::test]
async fn list_undelivered_outbound_messages_due_row_past_future_retry_window() {
    const FUTURE_RETRIES: usize = 10_050;
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let tok = NamespaceToken::local();
    let store = rt.notes(&tok).expect("note store");

    let mut future_rows = Vec::with_capacity(FUTURE_RETRIES);
    for i in 0..FUTURE_RETRIES {
        let mut note = outbound_message_note();
        note.created_at += i as i64;
        note.updated_at = note.created_at;
        note.properties = Some(serde_json::json!({
            "direction": "outbound",
            "to_actor": "email:recipient@example.test",
            "next_attempt_at": "2999-01-01T00:00:00Z",
        }));
        future_rows.push(note);
    }
    let summary = store
        .upsert_notes(future_rows)
        .await
        .expect("seed deferred retries");
    assert_eq!(summary.affected as usize, FUTURE_RETRIES);

    let mut due = outbound_message_note();
    due.created_at -= 1_000_000;
    due.updated_at = due.created_at;
    due.properties = Some(serde_json::json!({
        "direction": "outbound",
        "to_actor": "email:recipient@example.test",
        "next_attempt_at": "2000-01-01T00:00:00Z",
    }));
    let due_id = due.id;
    store
        .upsert_note(due)
        .await
        .expect("seed older due message");

    let hits = rt
        .list_undelivered_outbound_messages(&tok, Some("email:"), 1)
        .await
        .expect("scan succeeds");
    assert_eq!(
        hits.iter().map(|note| note.id).collect::<Vec<_>>(),
        vec![due_id],
        "a due row behind {FUTURE_RETRIES} deferred rows remains deliverable"
    );
    let channel_hits = rt
        .list_undelivered_outbound_messages_for_channel(&tok, "email:", "primary", true, 1)
        .await
        .expect("channel scan succeeds");
    assert_eq!(
        channel_hits.iter().map(|note| note.id).collect::<Vec<_>>(),
        vec![due_id],
        "the delivery pass must reach an older due row behind deferred retries"
    );
}

/// The channel prefix runs BEFORE the limit: a backlog of another
/// channel's pending rows must not consume the scan budget and starve
/// the requested channel (the pre-fix defect: filter-after-limit).
#[tokio::test]
async fn list_undelivered_outbound_messages_prefix_filters_before_limit() {
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let tok = NamespaceToken::local();
    let store = rt.notes(&tok).expect("note store");

    // Older email row behind three newer telegram rows.
    let mut email = outbound_message_note();
    email.created_at -= 100;
    email.properties =
        Some(serde_json::json!({"direction": "outbound", "to_actor": "email:a@b.c"}));
    let email_id = email.id;
    store.upsert_note(email).await.expect("seed email");
    for _ in 0..3 {
        let mut tg = outbound_message_note();
        tg.properties =
            Some(serde_json::json!({"direction": "outbound", "to_actor": "telegram:42"}));
        store.upsert_note(tg).await.expect("seed telegram");
    }

    // With filter-after-limit this would return a telegram row (newest
    // first) and the email loop would see nothing deliverable.
    let hits = rt
        .list_undelivered_outbound_messages(&tok, Some("email:"), 1)
        .await
        .expect("scan succeeds");
    assert_eq!(
        hits.iter().map(|n| n.id).collect::<Vec<_>>(),
        vec![email_id],
        "prefix predicate applies before the limit"
    );

    let telegram_hits = rt
        .list_undelivered_outbound_messages(&tok, Some("telegram:"), 200)
        .await
        .expect("scan succeeds");
    assert_eq!(telegram_hits.len(), 3, "telegram prefix sees its own rows");
}

/// Terminal-outcome markers: `delivered` stamps the ADR-122 §1 property
/// set (with `transport_message_id` only when given), `failed` stamps
/// §2's permanent-failure set, and both refuse targets that are not live
/// outbound message notes.
#[tokio::test]
async fn outbound_delivery_markers_stamp_adr122_properties_and_validate_target() {
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let tok = NamespaceToken::local();
    let store = rt.notes(&tok).expect("note store");

    let delivered_note = outbound_message_note();
    let delivered_id = delivered_note.id;
    let delivered_version = delivered_note.version;
    let failed_note = outbound_message_note();
    let failed_id = failed_note.id;
    let failed_version = failed_note.version;
    let mut inbound = Note::new("local", "message", "inbound row");
    inbound.properties = Some(serde_json::json!({"direction": "inbound"}));
    let inbound_id = inbound.id;
    let wrong_kind = Note::new("local", "observation", "not a message");
    let wrong_kind_id = wrong_kind.id;
    for note in [delivered_note, failed_note] {
        store.upsert_note(note).await.expect("seed note");
    }
    store.upsert_note(inbound).await.expect("seed inbound");
    store
        .upsert_note(wrong_kind)
        .await
        .expect("seed non-message");

    let marked = rt
        .mark_outbound_message_delivered(
            &tok,
            delivered_id,
            "2026-08-28T00:00:00Z".to_string(),
            Some("<mid@example>".to_string()),
        )
        .await
        .expect("mark delivered succeeds");
    assert_eq!(marked.version, delivered_version + 1);
    assert_eq!(marked, store.get_note(delivered_id).await.unwrap().unwrap());
    let props = marked
        .properties
        .as_ref()
        .and_then(|v| v.as_object())
        .unwrap();
    assert_eq!(
        props.get("delivery").and_then(|v| v.as_str()),
        Some("delivered")
    );
    assert_eq!(
        props.get("delivered_at").and_then(|v| v.as_str()),
        Some("2026-08-28T00:00:00Z")
    );
    assert_eq!(
        props.get("transport_message_id").and_then(|v| v.as_str()),
        Some("<mid@example>")
    );

    let failed = rt
        .mark_outbound_message_failed(
            &tok,
            failed_id,
            "2026-08-28T00:00:01Z".to_string(),
            "recipient not in allowlist".to_string(),
        )
        .await
        .expect("mark failed succeeds");
    assert_eq!(failed.version, failed_version + 1);
    assert_eq!(failed, store.get_note(failed_id).await.unwrap().unwrap());
    let props = failed
        .properties
        .as_ref()
        .and_then(|v| v.as_object())
        .unwrap();
    assert_eq!(
        props.get("delivery").and_then(|v| v.as_str()),
        Some("failed")
    );
    assert_eq!(
        props.get("failed_at").and_then(|v| v.as_str()),
        Some("2026-08-28T00:00:01Z")
    );
    assert_eq!(
        props.get("last_error").and_then(|v| v.as_str()),
        Some("recipient not in allowlist")
    );

    // Both marked rows are now terminal: the scan must not return them.
    let pending = rt
        .list_undelivered_outbound_messages(&tok, None, 200)
        .await
        .expect("scan succeeds");
    assert!(
        pending.is_empty(),
        "terminal rows left in scan: {:?}",
        pending.iter().map(|n| n.id).collect::<Vec<_>>()
    );

    // Validation arms: inbound message and non-message kind both refuse.
    for (id, label) in [(inbound_id, "inbound"), (wrong_kind_id, "non-message")] {
        let err = rt
            .mark_outbound_message_delivered(&tok, id, "t".to_string(), None)
            .await
            .expect_err(label);
        assert!(
            matches!(err, RuntimeError::InvalidInput(_)),
            "{label}: expected InvalidInput, got {err:?}"
        );
        let err = rt
            .mark_outbound_message_failed(&tok, id, "t".to_string(), "e".to_string())
            .await
            .expect_err(label);
        assert!(
            matches!(err, RuntimeError::InvalidInput(_)),
            "{label}: expected InvalidInput, got {err:?}"
        );
    }
}

/// Regression: two concurrent outbox workers (two overlapping daemon
/// processes during a restart, per ADR-122 §4/Consequences) can both
/// load the same pending note before either writes. Without a terminal
/// re-check, a worker that lost the race to record a failure still
/// re-fetches a fresh snapshot right before writing -- which already
/// carries the winner's "delivered" outcome -- and the compare-and-swap
/// alone does not stop it from blindly overwriting that success with
/// "failed" (or vice versa).
#[tokio::test]
async fn outbound_delivery_marker_refuses_to_overwrite_existing_terminal_outcome() {
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let tok = NamespaceToken::local();
    let store = rt.notes(&tok).expect("note store");

    let delivered_first = outbound_message_note();
    let delivered_first_id = delivered_first.id;
    let failed_first = outbound_message_note();
    let failed_first_id = failed_first.id;
    for note in [delivered_first, failed_first] {
        store.upsert_note(note).await.expect("seed note");
    }

    rt.mark_outbound_message_delivered(
        &tok,
        delivered_first_id,
        "2026-08-28T00:00:00Z".to_string(),
        None,
    )
    .await
    .expect("first delivery marker wins the race");

    let err = rt
        .mark_outbound_message_failed(
            &tok,
            delivered_first_id,
            "2026-08-28T00:00:01Z".to_string(),
            "duplicate send rejected".to_string(),
        )
        .await
        .expect_err("a losing failure marker must not clobber the recorded success");
    assert!(matches!(err, RuntimeError::InvalidInput(_)));

    let note = store
        .get_note(delivered_first_id)
        .await
        .expect("get note")
        .expect("note exists");
    let props = note
        .properties
        .as_ref()
        .and_then(|v| v.as_object())
        .unwrap();
    assert_eq!(
        props.get("delivery").and_then(|v| v.as_str()),
        Some("delivered"),
        "recorded success must survive the losing worker's overwrite attempt"
    );

    rt.mark_outbound_message_failed(
        &tok,
        failed_first_id,
        "2026-08-28T00:00:00Z".to_string(),
        "recipient rejected".to_string(),
    )
    .await
    .expect("first failure marker wins the race");

    let err = rt
        .mark_outbound_message_delivered(
            &tok,
            failed_first_id,
            "2026-08-28T00:00:01Z".to_string(),
            None,
        )
        .await
        .expect_err("a late success marker must not clobber a recorded failure");
    assert!(matches!(err, RuntimeError::InvalidInput(_)));
}

#[tokio::test]
async fn outbound_transient_failure_increments_attempts_and_arms_bounded_backoff() {
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let tok = NamespaceToken::local();
    let store = rt.notes(&tok).expect("note store");

    let mut note = outbound_message_note();
    note.properties = Some(serde_json::json!({
        "direction": "outbound",
        "delivery_attempts": 3,
        "unrelated": "preserved",
    }));
    let id = note.id;
    let original_version = note.version;
    store.upsert_note(note).await.expect("seed note");

    let attempted_at = chrono::DateTime::parse_from_rfc3339("2026-08-30T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let marked = rt
        .mark_outbound_message_transient_failure(
            &tok,
            id,
            attempted_at,
            "mailbox temporarily unavailable".to_string(),
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(20),
        )
        .await
        .expect("transient marker succeeds");
    assert_eq!(marked.version, original_version + 1);
    assert_eq!(marked, store.get_note(id).await.unwrap().unwrap());
    let props = marked.properties.unwrap();

    assert_eq!(props["delivery_attempts"].as_u64(), Some(4));
    assert_eq!(
        props["next_attempt_at"].as_str(),
        Some("2026-08-30T00:00:20+00:00"),
        "5 * 2^(4-1) is capped at the configured 20-second ceiling"
    );
    assert_eq!(
        props["last_error"].as_str(),
        Some("mailbox temporarily unavailable")
    );
    assert_eq!(props["unrelated"].as_str(), Some("preserved"));
    assert!(
        props.get("delivery").is_none(),
        "a transient failure must remain pending"
    );
}

fn assert_outbound_retry_schedule(
    note: &Note,
    attempted_at: chrono::DateTime<chrono::Utc>,
    expected_attempts: u64,
    expected_delay_seconds: i64,
) {
    let properties = note.properties.as_ref().expect("retry properties");
    assert_eq!(
        properties["delivery_attempts"].as_u64(),
        Some(expected_attempts)
    );
    let next_attempt_at = chrono::DateTime::parse_from_rfc3339(
        properties["next_attempt_at"]
            .as_str()
            .expect("retry deadline"),
    )
    .expect("RFC 3339 retry deadline")
    .with_timezone(&chrono::Utc);
    assert_eq!(
        next_attempt_at.signed_duration_since(attempted_at),
        chrono::TimeDelta::seconds(expected_delay_seconds)
    );
}

#[tokio::test]
async fn outbound_transient_failure_saturates_backoff_and_keeps_ordinary_growth() {
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let token = NamespaceToken::local();
    let store = rt.notes(&token).expect("note store");
    let attempted_at = chrono::DateTime::parse_from_rfc3339("2026-08-30T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    for (prior_attempts, expected_attempts, ceiling_seconds, expected_delay_seconds) in [
        (119, 120, 1800, 1800),
        (u64::MAX, u64::MAX, 1800, 1800),
        (0, 1, 60, 5),
        (1, 2, 60, 10),
        (2, 3, 60, 20),
        (4, 5, 60, 60),
    ] {
        let mut note = outbound_message_note();
        note.properties = Some(serde_json::json!({
            "direction": "outbound",
            "delivery_attempts": prior_attempts,
        }));
        let id = note.id;
        store.upsert_note(note).await.expect("seed pending message");

        let marked = rt
            .mark_outbound_message_transient_failure(
                &token,
                id,
                attempted_at,
                "temporary failure".to_string(),
                std::time::Duration::from_secs(5),
                std::time::Duration::from_secs(ceiling_seconds),
            )
            .await
            .expect("schedule retry");
        assert_outbound_retry_schedule(
            &marked,
            attempted_at,
            expected_attempts,
            expected_delay_seconds,
        );
        assert_eq!(marked, store.get_note(id).await.unwrap().unwrap());
    }
}

#[tokio::test]
async fn outbound_claim_transient_failure_saturates_backoff_and_keeps_ordinary_growth() {
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let token = NamespaceToken::local();
    let store = rt.notes(&token).expect("note store");
    let attempted_at = chrono::DateTime::parse_from_rfc3339("2026-08-30T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    for (prior_attempts, expected_attempts, ceiling_seconds, expected_delay_seconds) in [
        (119, 120, 1800, 1800),
        (u64::MAX, u64::MAX, 1800, 1800),
        (0, 1, 60, 5),
        (1, 2, 60, 10),
        (2, 3, 60, 20),
        (4, 5, 60, 60),
    ] {
        let mut note = outbound_message_note();
        note.properties = Some(serde_json::json!({
            "direction": "outbound",
            "delivery_attempts": prior_attempts,
        }));
        let id = note.id;
        store.upsert_note(note).await.expect("seed pending message");

        let marked = rt
            .mark_outbound_message_claim_transient_failure(
                &token,
                id,
                attempted_at,
                "temporary claim failure".to_string(),
                std::time::Duration::from_secs(5),
                std::time::Duration::from_secs(ceiling_seconds),
            )
            .await
            .expect("schedule claim retry");
        assert_outbound_retry_schedule(
            &marked,
            attempted_at,
            expected_attempts,
            expected_delay_seconds,
        );
        assert_eq!(marked, store.get_note(id).await.unwrap().unwrap());
    }
}

#[tokio::test]
async fn outbound_terminal_markers_clear_retry_schedule() {
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let tok = NamespaceToken::local();
    let store = rt.notes(&tok).expect("note store");

    let mut delivered = outbound_message_note();
    delivered.properties = Some(serde_json::json!({
        "direction": "outbound",
        "delivery_attempts": 2,
        "next_attempt_at": "2999-01-01T00:00:00Z",
        "last_error": "temporary",
    }));
    let delivered_id = delivered.id;

    let mut failed = outbound_message_note();
    failed.properties = Some(serde_json::json!({
        "direction": "outbound",
        "delivery_attempts": 7,
        "next_attempt_at": "2999-01-01T00:00:00Z",
    }));
    let failed_id = failed.id;

    for note in [delivered, failed] {
        store.upsert_note(note).await.expect("seed note");
    }

    let delivered = rt
        .mark_outbound_message_delivered(
            &tok,
            delivered_id,
            "2026-08-30T00:00:00Z".to_string(),
            None,
        )
        .await
        .expect("delivery marker succeeds");
    let delivered_props = delivered.properties.unwrap();
    assert!(delivered_props.get("delivery_attempts").is_none());
    assert!(delivered_props.get("next_attempt_at").is_none());
    assert_eq!(delivered_props["last_error"].as_str(), Some("temporary"));

    let failed = rt
        .mark_outbound_message_failed(
            &tok,
            failed_id,
            "2026-08-30T00:00:01Z".to_string(),
            "recipient rejected".to_string(),
        )
        .await
        .expect("permanent marker succeeds");
    let failed_props = failed.properties.unwrap();
    assert!(failed_props.get("delivery_attempts").is_none());
    assert!(failed_props.get("next_attempt_at").is_none());
    assert_eq!(failed_props["delivery"].as_str(), Some("failed"));
}

#[tokio::test]
async fn claim_outbound_message_external_id_sets_value_and_survives_readback() {
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let tok = NamespaceToken::local();
    let note = outbound_message_note();
    let note_id = note.id;
    rt.notes(&tok)
        .expect("note store")
        .upsert_note(note)
        .await
        .expect("seed note");

    let claimed = rt
        .claim_outbound_message_external_id(&tok, note_id, "<abc@example.com>".to_string())
        .await
        .expect("claim succeeds on a fresh outbound message note");
    assert_eq!(
        claimed
            .properties
            .as_ref()
            .and_then(|p| p.get("external_id"))
            .and_then(|v| v.as_str()),
        Some("<abc@example.com>")
    );

    // Reads the persisted row back independently of the claim call's own
    // return value. This is the check that fails if the fix is reverted
    // to routing the claim through `dispatch("update", ...)`: that path is
    // refused by the owner-established-property gate exercised in
    // `generic_update_still_refuses_external_id_on_message_note` below, so
    // external_id would never actually persist and this read would come
    // back `None`.
    let reread = rt
        .notes(&tok)
        .expect("note store")
        .get_note(note_id)
        .await
        .expect("read note")
        .expect("note still exists");
    assert_eq!(
        reread
            .properties
            .as_ref()
            .and_then(|p| p.get("external_id"))
            .and_then(|v| v.as_str()),
        Some("<abc@example.com>")
    );
}

#[tokio::test]
async fn generic_update_still_refuses_external_id_on_message_note() {
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let tok = NamespaceToken::local();
    let note = outbound_message_note();
    let note_id = note.id;
    rt.notes(&tok)
        .expect("note store")
        .upsert_note(note)
        .await
        .expect("seed note");

    let err = rt
        .update_note(
            &tok,
            note_id,
            NotePatch {
                properties: Some(serde_json::json!({"external_id": "<forged@example.com>"})),
                ..Default::default()
            },
        )
        .await
        .expect_err("caller-facing update must keep refusing external_id on a message note");
    assert!(matches!(err, RuntimeError::InvalidInput(_)), "error: {err}");
    assert!(err.to_string().contains("is not patchable"), "error: {err}");

    // The owner path is unaffected by the caller-side refusal above.
    let claimed = rt
        .claim_outbound_message_external_id(&tok, note_id, "<claimed@example.com>".to_string())
        .await
        .expect("owner-bookkeeping path still claims after a refused caller patch");
    assert_eq!(
        claimed
            .properties
            .as_ref()
            .and_then(|p| p.get("external_id"))
            .and_then(|v| v.as_str()),
        Some("<claimed@example.com>")
    );
}

#[tokio::test]
async fn claim_failure_snapshot_cannot_park_a_concurrent_successful_claim() {
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let token = NamespaceToken::local();
    let mut before = outbound_message_note();
    // The owner must preserve existing trusted transport provenance.
    before
        .properties
        .as_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("channel_kind".into(), serde_json::json!("email"));
    rt.raw_notes(&token)
        .unwrap()
        .upsert_note(before.clone())
        .await
        .unwrap();
    let claimed = rt
        .claim_outbound_message_external_id(&token, before.id, "<winner@example.com>".into())
        .await
        .unwrap();
    assert!(claimed.updated_at > before.updated_at);
    assert_eq!(
        claimed.properties.as_ref().unwrap()["channel_kind"],
        "email"
    );

    rt.mark_outbound_message_claim_failed_from_snapshot(
        &token,
        before,
        "2026-09-22T12:00:00Z".into(),
        "claim refused".into(),
    )
    .await
    .expect_err("the stale pre-claim snapshot cannot overwrite the winner");
    let returned = rt
        .mark_outbound_message_claim_failed(
            &token,
            claimed.id,
            "2026-09-22T12:00:00Z".into(),
            "already claimed".into(),
        )
        .await
        .unwrap();
    let stored = rt
        .notes(&token)
        .unwrap()
        .get_note(claimed.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(returned).unwrap(),
        serde_json::to_value(&claimed).unwrap()
    );
    assert_eq!(
        serde_json::to_value(stored).unwrap(),
        serde_json::to_value(claimed).unwrap()
    );
}

#[tokio::test]
async fn claim_failure_parks_only_unclaimed_pending_messages() {
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let token = NamespaceToken::local();
    for outcome in [None, Some("delivered"), Some("failed")] {
        let mut note = outbound_message_note();
        let props = note.properties.as_mut().unwrap().as_object_mut().unwrap();
        props.insert("channel_kind".into(), serde_json::json!("email"));
        if let Some(outcome) = outcome {
            props.insert("delivery".into(), serde_json::json!(outcome));
        }
        rt.raw_notes(&token)
            .unwrap()
            .upsert_note(note.clone())
            .await
            .unwrap();
        let result = rt
            .mark_outbound_message_claim_failed(
                &token,
                note.id,
                "2026-09-22T12:00:00Z".into(),
                "claim refused".into(),
            )
            .await
            .unwrap();
        if outcome.is_some() {
            assert_eq!(
                serde_json::to_value(&result).unwrap(),
                serde_json::to_value(note).unwrap()
            );
        } else {
            assert_eq!(result.properties.as_ref().unwrap()["delivery"], "failed");
            assert_eq!(result.properties.as_ref().unwrap()["channel_kind"], "email");
            assert!(result.updated_at > note.updated_at);
            assert_eq!(result.version, note.version + 1, "one parking write");
        }
        assert_eq!(
            serde_json::to_value(
                rt.notes(&token)
                    .unwrap()
                    .get_note(result.id)
                    .await
                    .unwrap()
                    .unwrap()
            )
            .unwrap(),
            serde_json::to_value(result).unwrap()
        );
    }
}

#[tokio::test]
async fn claim_refuses_non_message_note() {
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let tok = NamespaceToken::local();
    let mut note = Note::new("local", "observation", "not a message");
    note.properties = Some(serde_json::json!({"direction": "outbound"}));
    let note_id = note.id;
    rt.notes(&tok)
        .expect("note store")
        .upsert_note(note)
        .await
        .expect("seed note");

    let err = rt
        .claim_outbound_message_external_id(&tok, note_id, "<x@example.com>".to_string())
        .await
        .expect_err("a non-message note must never accept the claim");
    assert!(matches!(err, RuntimeError::InvalidInput(_)), "error: {err}");
}

#[tokio::test]
async fn claim_refuses_inbound_message() {
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let tok = NamespaceToken::local();
    let mut note = Note::new("local", "message", "inbound content");
    note.properties = Some(serde_json::json!({"direction": "inbound"}));
    let note_id = note.id;
    rt.notes(&tok)
        .expect("note store")
        .upsert_note(note)
        .await
        .expect("seed note");

    let err = rt
        .claim_outbound_message_external_id(&tok, note_id, "<x@example.com>".to_string())
        .await
        .expect_err("an inbound message note must never accept the claim");
    assert!(matches!(err, RuntimeError::InvalidInput(_)), "error: {err}");
}

#[tokio::test]
async fn claim_refuses_when_external_id_already_set() {
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let tok = NamespaceToken::local();
    let mut note = outbound_message_note();
    note.properties =
        Some(serde_json::json!({"direction": "outbound", "external_id": "<already@example.com>"}));
    let note_id = note.id;
    rt.notes(&tok)
        .expect("note store")
        .upsert_note(note)
        .await
        .expect("seed note");

    let err = rt
        .claim_outbound_message_external_id(&tok, note_id, "<new@example.com>".to_string())
        .await
        .expect_err("a note that already carries external_id must refuse re-claim");
    assert!(matches!(err, RuntimeError::InvalidInput(_)), "error: {err}");
}

#[tokio::test]
async fn generic_update_can_still_patch_delivered_at_on_message_note() {
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let tok = NamespaceToken::local();
    let note = outbound_message_note();
    let note_id = note.id;
    rt.notes(&tok)
        .expect("note store")
        .upsert_note(note)
        .await
        .expect("seed note");

    let updated = rt
        .update_note(
            &tok,
            note_id,
            NotePatch {
                properties: Some(serde_json::json!({"delivered_at": "2026-08-09T00:00:00Z"})),
                ..Default::default()
            },
        )
        .await
        .expect("delivered_at is not owner-established and must remain patchable");
    assert_eq!(
        updated
            .properties
            .as_ref()
            .and_then(|p| p.get("delivered_at"))
            .and_then(|v| v.as_str()),
        Some("2026-08-09T00:00:00Z")
    );
}

fn secret_shaped_reason() -> String {
    const ALPHANUMERIC: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let candidate: String = (0..48)
        .map(|index| char::from(ALPHANUMERIC[(index * 17 + 11) % ALPHANUMERIC.len()]))
        .collect();
    format!("secret value: {candidate}")
}

#[test]
fn note_embedding_text_ref_borrows_stored_content() {
    let note = Note::new("embedding-borrow", "observation", "borrow this content");
    let text = note_embedding_text_ref(&note);

    assert_eq!(text, note.content.as_str());
    assert!(
        std::ptr::eq(text, note.content.as_str()),
        "internal canonical note text must borrow instead of cloning"
    );
    let owned: String = note_embedding_text(&note);
    assert_eq!(owned.as_str(), note.content.as_str());
}

#[tokio::test]
async fn generic_note_update_errors_when_revision_is_already_i64_max() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let mut note = Note::new("local", "observation", "saturated revision");
    note.updated_at = i64::MAX;
    let note_id = note.id;
    rt.notes(&tok)
        .expect("note store")
        .upsert_note(note.clone())
        .await
        .expect("seed saturated note");

    let error = rt
        .update_note(
            &tok,
            note_id,
            NotePatch::new(None, Some("must not land".to_string()), None, None, None),
        )
        .await
        .expect_err("i64::MAX cannot yield a strictly newer CAS revision");
    assert!(
        matches!(&error, RuntimeError::Internal(_)),
        "revision exhaustion is an internal persisted-state error: {error}"
    );
    assert!(error.to_string().contains("i64::MAX"), "error: {error}");

    let persisted = rt
        .notes(&tok)
        .expect("note store")
        .get_note(note_id)
        .await
        .expect("read note")
        .expect("note remains live");
    assert_eq!(persisted, note, "failed revision advance must not mutate");
}

async fn restore_edge_preimage(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    preimage: &MergeEdgePreimage,
) {
    let edge = khive_storage::types::Edge {
        id: preimage.id.into(),
        namespace: preimage.namespace.clone(),
        source_id: preimage.source_id,
        target_id: preimage.target_id,
        relation: preimage.relation.parse().expect("stored relation"),
        weight: preimage.weight,
        created_at: chrono::DateTime::from_timestamp_micros(preimage.created_at)
            .expect("stored created_at"),
        updated_at: chrono::DateTime::from_timestamp_micros(preimage.updated_at)
            .expect("stored updated_at"),
        deleted_at: preimage.deleted_at.map(|value| {
            chrono::DateTime::from_timestamp_micros(value).expect("stored deleted_at")
        }),
        metadata: preimage.metadata.clone(),
        target_backend: preimage.target_backend.clone(),
    };
    rt.graph(token)
        .expect("graph store")
        .upsert_edge(edge)
        .await
        .expect("restore edge preimage");
}

fn assert_edge_matches_preimage(edge: &khive_storage::types::Edge, preimage: &MergeEdgePreimage) {
    assert_eq!(Uuid::from(edge.id), preimage.id);
    assert_eq!(edge.namespace, preimage.namespace);
    assert_eq!(edge.source_id, preimage.source_id);
    assert_eq!(edge.target_id, preimage.target_id);
    assert_eq!(edge.relation.to_string(), preimage.relation);
    assert_eq!(edge.weight, preimage.weight);
    assert_eq!(edge.created_at.timestamp_micros(), preimage.created_at);
    assert_eq!(edge.updated_at.timestamp_micros(), preimage.updated_at);
    assert_eq!(
        edge.deleted_at.map(|value| value.timestamp_micros()),
        preimage.deleted_at
    );
    assert_eq!(edge.metadata, preimage.metadata);
    assert_eq!(edge.target_backend, preimage.target_backend);
}

// Helper: search FTS5 for `query` in a runtime namespace.
async fn fts_hit(rt: &KhiveRuntime, token: &NamespaceToken, query: &str) -> Vec<Uuid> {
    let ns = token.namespace().as_str().to_string();
    rt.text(token)
        .unwrap()
        .search(TextSearchRequest {
            query: query.to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(TextFilter {
                namespaces: vec![ns],
                ..Default::default()
            }),
            top_k: 50,
            snippet_chars: 100,
        })
        .await
        .unwrap()
        .into_iter()
        .map(|h| h.subject_id)
        .collect()
}

#[tokio::test]
async fn update_entity_patch_changes_only_specified_fields() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "OriginalName",
            Some("orig desc"),
            Some(serde_json::json!({"k":"v"})),
            vec![],
        )
        .await
        .unwrap();

    let updated = rt
        .update_entity(
            &tok,
            entity.id,
            EntityPatch {
                description: Some(Some("new desc".to_string())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.name, "OriginalName");
    assert_eq!(updated.description.as_deref(), Some("new desc"));
    assert_eq!(updated.properties, Some(serde_json::json!({"k":"v"})));
}

#[tokio::test]
async fn update_entity_if_unchanged_removes_properties_after_merge() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "LegacyEcho",
            Some("keep description"),
            Some(serde_json::json!({
                "type": " Concept ",
                "nested": {"items": [1, {"value": null}], "keep": true},
                "label": "verbatim"
            })),
            vec!["keep-tag".to_string()],
        )
        .await
        .unwrap();

    let updated = rt
        .update_entity_if_unchanged(
            &tok,
            &entity,
            EntityPatch {
                properties: Some(serde_json::json!({"type": "also remove", "added": 7})),
                ..Default::default()
            },
            &["type", "absent", "type"],
        )
        .await
        .expect("remove only the requested key after merging");

    let mut expected = entity.clone();
    expected.properties = Some(serde_json::json!({
        "nested": {"items": [1, {"value": null}], "keep": true},
        "label": "verbatim",
        "added": 7
    }));
    assert!(updated.updated_at > entity.updated_at);
    expected.updated_at = updated.updated_at;
    expected.version = entity.version + 1;
    assert_eq!(serde_json::json!(updated), serde_json::json!(expected));
    assert_eq!(
        serde_json::json!(rt.get_entity(&tok, entity.id).await.unwrap()),
        serde_json::json!(expected)
    );
    let events = entity_update_events(&rt, &tok).await;
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].payload["changed_fields"],
        serde_json::json!(["properties"])
    );
}

#[tokio::test]
async fn update_entity_if_unchanged_missing_removals_are_no_op() {
    let rt = rt();
    let tok = NamespaceToken::local();
    for properties in [
        None,
        Some(serde_json::json!({"keep": [true, null]})),
        Some(serde_json::json!(["not an object"])),
    ] {
        let entity = rt
            .create_entity(&tok, "concept", None, "NoRemoval", None, properties, vec![])
            .await
            .unwrap();
        let unchanged = rt
            .update_entity_if_unchanged(&tok, &entity, EntityPatch::default(), &["absent"])
            .await
            .unwrap();
        assert_eq!(serde_json::json!(unchanged), serde_json::json!(entity));
        assert_eq!(
            serde_json::json!(rt.get_entity(&tok, entity.id).await.unwrap()),
            serde_json::json!(entity)
        );
    }
    assert!(entity_update_events(&rt, &tok).await.is_empty());
}

#[tokio::test]
async fn update_entity_if_unchanged_refuses_stale_full_snapshot() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "StaleBackfill",
            None,
            Some(serde_json::json!({"type": "concept", "keep": true})),
            vec![],
        )
        .await
        .unwrap();
    for field in ["properties", "entity_type", "deleted_at", "updated_at"] {
        let mut stale = entity.clone();
        match field {
            "properties" => stale.properties = Some(serde_json::json!({"type": "algorithm"})),
            "entity_type" => stale.entity_type = Some("algorithm".to_string()),
            "deleted_at" => stale.deleted_at = Some(entity.updated_at),
            "updated_at" => stale.updated_at -= 1,
            _ => unreachable!(),
        }
        let error = rt
            .update_entity_if_unchanged(
                &tok,
                &stale,
                EntityPatch {
                    entity_type: Some(Some("algorithm".to_string())),
                    ..Default::default()
                },
                &["type"],
            )
            .await
            .expect_err("every stale snapshot field must refuse before edits");
        assert!(
            matches!(error, RuntimeError::Khive(ref error) if error.kind() == khive_types::ErrorKind::Conflict),
            "{field}: {error}"
        );
        assert_eq!(
            serde_json::json!(rt.get_entity(&tok, entity.id).await.unwrap()),
            serde_json::json!(entity),
            "{field}"
        );
    }

    let store = rt.entities(&tok).unwrap();
    store
        .delete_entity(entity.id, khive_storage::types::DeleteMode::Soft)
        .await
        .unwrap();
    let tombstone = store
        .get_entity_including_deleted(entity.id)
        .await
        .unwrap()
        .unwrap();
    let error = rt
        .update_entity_if_unchanged(&tok, &entity, EntityPatch::default(), &["type"])
        .await
        .expect_err("a deleted candidate must not be resurrected");
    assert!(
        matches!(error, RuntimeError::Khive(ref error) if error.kind() == khive_types::ErrorKind::Conflict),
        "{error}"
    );
    assert_eq!(
        serde_json::json!(store
            .get_entity_including_deleted(entity.id)
            .await
            .unwrap()
            .unwrap()),
        serde_json::json!(tombstone)
    );
    assert!(entity_update_events(&rt, &tok).await.is_empty());
}

#[tokio::test]
async fn update_entity_if_unchanged_refuses_concurrent_writer_after_snapshot_check() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "RacingBackfill",
            None,
            Some(serde_json::json!({"type": "concept", "keep": true})),
            vec![],
        )
        .await
        .unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (guarded, normal) = tokio::join!(
        race_seam::AFTER_READ_BARRIER.scope(
            Arc::clone(&barrier),
            rt.update_entity_if_unchanged(&tok, &entity, EntityPatch::default(), &["type"]),
        ),
        race_seam::AFTER_READ_BARRIER.scope(
            barrier,
            rt.update_entity(
                &tok,
                entity.id,
                EntityPatch {
                    properties: Some(serde_json::json!({"normal_writer": true})),
                    ..Default::default()
                },
            ),
        ),
    );
    assert_eq!(
        usize::from(guarded.is_ok()) + usize::from(normal.is_ok()),
        1
    );
    let (winner, refused) = match (guarded, normal) {
        (Ok(winner), Err(refused)) | (Err(refused), Ok(winner)) => (winner, refused),
        results => panic!("exactly one writer must win: {results:?}"),
    };
    assert!(
        matches!(refused, RuntimeError::Khive(ref error) if error.kind() == khive_types::ErrorKind::Conflict),
        "{refused}"
    );
    assert_eq!(
        serde_json::json!(rt.get_entity(&tok, entity.id).await.unwrap()),
        serde_json::json!(winner)
    );
    assert_eq!(entity_update_events(&rt, &tok).await.len(), 1);
}

#[tokio::test]
async fn update_entity_if_unchanged_normalizes_type_with_installed_validator() {
    let rt = rt();
    let composed = khive_types::EntityTypeRegistry::with_extra([khive_types::EntityTypeDef {
        kind: khive_types::EntityKind::Document,
        type_name: "backfill_test_report",
        aliases: &["field_report"],
    }]);
    rt.install_entity_type_validator(Arc::new(move |kind, raw| {
        let kind = kind
            .parse::<khive_types::EntityKind>()
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        composed
            .resolve(kind, raw)
            .map(|resolved| resolved.entity_type)
            .map_err(RuntimeError::from)
    }));
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(
            &tok,
            "document",
            None,
            "LegacySubtype",
            None,
            Some(serde_json::json!({"type": " Field-Report ", "keep": [1, 2]})),
            vec![],
        )
        .await
        .unwrap();
    let updated = rt
        .update_entity_if_unchanged(
            &tok,
            &entity,
            EntityPatch {
                entity_type: Some(Some(" Field-Report ".to_string())),
                ..Default::default()
            },
            &[],
        )
        .await
        .expect("normal write validation resolves pack-supplied aliases");
    assert_eq!(updated.entity_type.as_deref(), Some("backfill_test_report"));
    assert_eq!(updated.properties, entity.properties);
    assert_eq!(
        rt.get_entity(&tok, entity.id).await.unwrap().entity_type,
        updated.entity_type
    );
    let error = rt
        .update_entity_if_unchanged(
            &tok,
            &updated,
            EntityPatch {
                entity_type: Some(Some("not_registered".to_string())),
                ..Default::default()
            },
            &["type"],
        )
        .await
        .expect_err("invalid subtype refuses the entire patch and removal");
    assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
    assert_eq!(
        serde_json::json!(rt.get_entity(&tok, entity.id).await.unwrap()),
        serde_json::json!(updated)
    );
    let events = entity_update_events(&rt, &tok).await;
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].payload["changed_fields"],
        serde_json::json!(["entity_type"])
    );
}

#[tokio::test]
async fn update_entity_if_unchanged_refuses_reserved_property_removal() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(&tok, "concept", None, "ReservedRemoval", None, None, vec![])
        .await
        .unwrap();
    let error = rt
        .update_entity_if_unchanged(
            &tok,
            &entity,
            EntityPatch::default(),
            &[crate::secret_gate::RESERVED_SECRET_GATE_KEY],
        )
        .await
        .expect_err("removal must share the reserved-property write validator");
    assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
    assert_eq!(
        serde_json::json!(rt.get_entity(&tok, entity.id).await.unwrap()),
        serde_json::json!(entity)
    );
    assert!(entity_update_events(&rt, &tok).await.is_empty());
}

#[tokio::test]
async fn update_entity_type_patch_validates_preserves_fields_and_requires_reindex() {
    let rt = rt();
    rt.install_entity_type_validator(std::sync::Arc::new(|kind, entity_type| {
        let Some(raw) = entity_type else {
            return Ok(None);
        };
        let normalized = raw.trim().to_ascii_lowercase();
        if kind == "concept" && normalized == "algorithm" {
            Ok(Some(normalized))
        } else {
            Err(RuntimeError::InvalidInput(format!(
                "unknown entity_type {raw:?} for {kind:?}; valid: algorithm"
            )))
        }
    }));
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "HistoricalAlgorithm",
            Some("keep description"),
            Some(serde_json::json!({"type": "algorithm", "keep": true})),
            vec!["keep-tag".to_string()],
        )
        .await
        .unwrap();

    let (prepared, reindex_required, changed_fields, _expected_updated_at, _expected_deleted_at) =
        rt.prepare_update_entity(
            &tok,
            entity.id,
            EntityPatch {
                entity_type: Some(Some(" Algorithm ".to_string())),
                ..Default::default()
            },
        )
        .await
        .expect("registered entity_type must validate");

    assert_eq!(prepared.entity_type.as_deref(), Some("algorithm"));
    assert_eq!(prepared.name, "HistoricalAlgorithm");
    assert_eq!(prepared.description.as_deref(), Some("keep description"));
    assert_eq!(
        prepared.properties,
        Some(serde_json::json!({"type": "algorithm", "keep": true}))
    );
    assert_eq!(prepared.tags, vec!["keep-tag"]);
    assert!(
        reindex_required,
        "changing entity_type must request the normal entity reindex path"
    );
    assert_eq!(changed_fields, vec!["entity_type"]);

    let err = rt
        .prepare_update_entity(
            &tok,
            entity.id,
            EntityPatch {
                entity_type: Some(Some("not_registered".to_string())),
                ..Default::default()
            },
        )
        .await
        .expect_err("unregistered entity_type must be rejected");
    assert!(matches!(err, RuntimeError::InvalidInput(_)), "error: {err}");
}

/// Regression for the entity lost-update race (khive #1753): two writers
/// read the same entity revision, then commit successive patches to
/// independent properties fields. Before the guarded
/// `replace_entity_if_unchanged` primitive, `update_entity_with_embedding_report`
/// wrote an unconditional `entity_upsert_statement`, so both patches
/// "succeeded" and the second silently discarded the first's field
/// (`a=1` was overwritten back to `a=0` when B's stale full-row replace
/// landed). This test forces the interleaving with a `Barrier` — both
/// readers are released together, so both `prepare_update_entity` calls
/// observe the SAME pre-write revision (asserted below) — then commits
/// deterministically in a fixed A-then-B order. It reddens if the guard
/// is dropped from `entity_replace_if_unchanged_statement` ENTIRELY: with
/// an unconditional UPDATE (or `entity_upsert_statement`), B's write would
/// also return `true` and `b` would be lost from the final properties.
///
/// It does NOT redden when only `?8 > updated_at` is removed: with
/// `updated_at = ?13` intact, B is refused by the revision guard whatever
/// the clock did, so this fixture cannot see that conjunct disappear.
///
/// It cannot ATTRIBUTE a failure to `updated_at = ?13` either, but for a
/// different reason, and the difference matters. Both racers take their
/// replacement revision from `prepare_update_entity`'s
/// `max(now_micros, expected + 1)` above; this test pins the two EXPECTED
/// revisions equal, never the two REPLACEMENT revisions. So with
/// `updated_at = ?13` removed, whether B is still refused depends on
/// whether B's wall-clock read happened to exceed A's committed revision.
/// That is a race, not a property of the fixture, and no single run of it
/// establishes either answer.
///
/// Attribution therefore comes from fixtures that force the question:
/// `entity_cas_refuses_a_replacement_revision_that_does_not_advance` for
/// the strict-advance conjunct, and
/// `production_update_entity_refuses_concurrent_stale_writer` for the
/// production wiring. Making this fixture attribute as well would mean
/// pinning both replacement revisions to a common `expected + 1`; it is
/// deliberately left as a whole-guard test instead.
///
/// SCOPE: this exercises the STORE PRIMITIVE directly and never invokes
/// `update_entity`, so it stays green if the production caller is reverted
/// to an unconditional write. The wiring is covered separately by
/// `production_update_entity_refuses_concurrent_stale_writer`; both are
/// required, neither substitutes for the other.
#[tokio::test]
async fn concurrent_entity_property_patches_from_one_revision_only_one_survives() {
    let rt = Arc::new(rt());
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "RaceTarget",
            None,
            Some(serde_json::json!({"a": 0, "b": 0})),
            vec![],
        )
        .await
        .expect("seed entity");
    let id = entity.id;

    let barrier = Arc::new(tokio::sync::Barrier::new(2));

    let reader_a = {
        let rt = Arc::clone(&rt);
        let tok = tok.clone();
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            rt.prepare_update_entity(
                &tok,
                id,
                EntityPatch {
                    properties: Some(serde_json::json!({"a": 1})),
                    ..Default::default()
                },
            )
            .await
        })
    };
    let reader_b = {
        let rt = Arc::clone(&rt);
        let tok = tok.clone();
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            rt.prepare_update_entity(
                &tok,
                id,
                EntityPatch {
                    properties: Some(serde_json::json!({"b": 1})),
                    ..Default::default()
                },
            )
            .await
        })
    };

    let (entity_a, _, _, expected_updated_at_a, expected_deleted_at_a) =
        reader_a.await.unwrap().expect("reader A prepares");
    let (entity_b, _, _, expected_updated_at_b, expected_deleted_at_b) =
        reader_b.await.unwrap().expect("reader B prepares");
    assert_eq!(
        expected_updated_at_a, expected_updated_at_b,
        "both readers must observe the same pre-write revision for this to be a real race"
    );

    let store = rt.entities(&tok).expect("entity store");
    assert!(
        store
            .replace_entity_if_unchanged(entity_a, expected_updated_at_a, expected_deleted_at_a)
            .await
            .expect("writer A CAS query"),
        "the first committer from a shared revision must win"
    );
    assert!(
        !store
            .replace_entity_if_unchanged(entity_b, expected_updated_at_b, expected_deleted_at_b)
            .await
            .expect("writer B CAS query"),
        "the second committer from the SAME stale revision must be refused, not merged"
    );

    let final_entity = rt.get_entity(&tok, id).await.expect("read final entity");
    assert_eq!(
        final_entity.properties,
        Some(serde_json::json!({"a": 1, "b": 0})),
        "writer B's field must not be silently merged into the persisted row: {:?}",
        final_entity.properties
    );
}

/// Same race as `concurrent_entity_property_patches_from_one_revision_only_one_survives`,
/// but driven entirely through the PRODUCTION entry point
/// (`update_entity_with_embedding_report`) rather than the store's
/// `replace_entity_if_unchanged` primitive directly. This closes a gap
/// the primitive-level test cannot: it would still pass unchanged if the
/// production caller were reverted to an unconditional write, since it
/// never invokes that caller at all. Uses `race_seam::pause_after_read`
/// (test-only, compiled out of non-test builds) to force both concurrent
/// callers to observe the identical pre-write revision deterministically —
/// no sleeps, no reliance on scheduler ordering.
#[tokio::test]
async fn production_update_entity_refuses_concurrent_stale_writer() {
    let rt = Arc::new(rt());
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "ProductionRaceTarget",
            None,
            Some(serde_json::json!({"a": 0, "b": 0})),
            vec![],
        )
        .await
        .expect("seed entity");
    let id = entity.id;

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));

    let writer_a = {
        let rt = Arc::clone(&rt);
        let tok = tok.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        tokio::spawn(race_seam::AFTER_READ_BARRIER.scope(barrier, async move {
            rt.update_entity_with_embedding_report(
                &tok,
                id,
                EntityPatch {
                    properties: Some(serde_json::json!({"a": 1})),
                    ..Default::default()
                },
            )
            .await
        }))
    };
    let writer_b = {
        let rt = Arc::clone(&rt);
        let tok = tok.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        tokio::spawn(race_seam::AFTER_READ_BARRIER.scope(barrier, async move {
            rt.update_entity_with_embedding_report(
                &tok,
                id,
                EntityPatch {
                    properties: Some(serde_json::json!({"b": 1})),
                    ..Default::default()
                },
            )
            .await
        }))
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
        Err(RuntimeError::Khive(khive_error)) => {
            assert_eq!(
                khive_error.kind(),
                khive_types::ErrorKind::Conflict,
                "the losing production caller must surface a typed conflict, not \
                     silently overwrite: {refused:?}"
            );
        }
        other => panic!("expected a typed conflict error, got {other:?}"),
    }

    let final_entity = rt.get_entity(&tok, id).await.expect("read final entity");
    assert_ne!(
        final_entity.properties,
        Some(serde_json::json!({"a": 1, "b": 1})),
        "both racers' fields must never both land: that would mean the loser's stale \
             write silently succeeded"
    );
}

/// Isolating fixture for the `AND ?8 > updated_at` conjunct of
/// `entity_replace_if_unchanged_statement`.
///
/// The concurrent-race tests above cannot cover it. What is measured, and
/// deterministic: tautologizing `?8 > updated_at` alone reddened NOTHING in
/// `khive-runtime` before this test existed, because the revision guard
/// (`updated_at = ?13`) refuses the losing writer on its own whatever the
/// clock did. Tautologizing `updated_at = ?13` + `deleted_at IS ?14`
/// reddens only `production_update_entity_refuses_concurrent_stale_writer`,
/// and defeating all three at once reddens the race tests.
///
/// What is NOT claimed, because the fixture cannot support it: that the two
/// guards are each independently sufficient. The race fixture pins the two
/// racers' EXPECTED revisions equal but never their REPLACEMENT revisions,
/// which both come from `max(now, expected + 1)`. So with `updated_at = ?13`
/// removed, whether strict advance still refuses depends on which clock read
/// won — a race, not a property of the fixture. This test strips the second
/// mechanism by construction instead:
/// it supplies the CORRECT expected revision and deletion marker, so
/// `?13`/`?14` are satisfied by construction, and the ONLY thing that can
/// refuse the write is the strict-advance conjunct.
#[tokio::test]
async fn entity_cas_refuses_a_replacement_revision_that_does_not_advance() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "NonAdvancing",
            None,
            Some(serde_json::json!({"a": 0})),
            vec![],
        )
        .await
        .expect("seed entity");
    let id = entity.id;

    let (mut replacement, _, _, expected_updated_at, expected_deleted_at) = rt
        .prepare_update_entity(
            &tok,
            id,
            EntityPatch {
                properties: Some(serde_json::json!({"a": 1})),
                ..Default::default()
            },
        )
        .await
        .expect("prepare update");

    // Force the replacement revision to EQUAL the stored one, then PROVE the
    // isolation rather than asserting it in prose. Reading the row back is
    // what makes `?13` and `?14` observed facts here instead of values
    // carried out of `prepare_update_entity`'s setup read.
    replacement.updated_at = expected_updated_at;

    let store = rt.entities(&tok).expect("entity store");
    let stored = store
        .get_entity_including_deleted(id)
        .await
        .expect("read stored row")
        .expect("row present before CAS");
    assert_eq!(
        stored.updated_at, expected_updated_at,
        "fixture premise: nothing moved the stored revision between prepare and CAS, \
             otherwise `?13` would refuse and this stops being an isolating fixture"
    );
    assert_eq!(
        stored.deleted_at, expected_deleted_at,
        "fixture premise: the stored deletion marker must still equal the snapshot's, \
             otherwise `deleted_at IS ?14` would refuse and this stops being an isolating \
             fixture"
    );
    assert_eq!(
        replacement.updated_at, stored.updated_at,
        "fixture premise: the replacement revision must NOT advance past the stored one, \
             which is the single condition under test"
    );

    let committed = store
        .replace_entity_if_unchanged(replacement, expected_updated_at, expected_deleted_at)
        .await
        .expect("CAS query");
    assert!(
        !committed,
        "a replacement whose revision does not strictly advance past the stored one must \
             be refused: without `?8 > updated_at` the CAS would accept a write that leaves \
             `updated_at` unmoved, so a later writer holding the same snapshot would still \
             see its expected revision match and overwrite this one"
    );

    let stored = rt.get_entity(&tok, id).await.expect("read back");
    assert_eq!(
        stored.properties,
        Some(serde_json::json!({"a": 0})),
        "the refused write must not have landed"
    );
}

/// Isolate the deletion-marker guard from both timestamp and persisted
/// version guards. Soft deletion leaves updated_at unchanged but advances
/// version; this fixture deliberately supplies the current version with
/// its stale pre-delete marker so dropping that marker alone permits an
/// unintended resurrection.
#[tokio::test]
async fn entity_cas_refuses_a_stale_replacement_that_would_resurrect_a_tombstone() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "Tombstoned",
            None,
            Some(serde_json::json!({"a": 0})),
            vec![],
        )
        .await
        .expect("seed entity");
    let id = entity.id;

    // Snapshot BEFORE the delete: this is the stale writer's view.
    let (mut replacement, _, _, expected_updated_at, expected_deleted_at) = rt
        .prepare_update_entity(
            &tok,
            id,
            EntityPatch {
                properties: Some(serde_json::json!({"a": 1})),
                ..Default::default()
            },
        )
        .await
        .expect("prepare update");
    assert_eq!(
        expected_deleted_at, None,
        "fixture premise: the snapshot must be of a LIVE row"
    );

    rt.delete_entity(&tok, id, false)
        .await
        .expect("soft delete");

    let store = rt.entities(&tok).expect("entity store");
    let tombstoned = store
        .get_entity_including_deleted(id)
        .await
        .expect("read tombstone")
        .expect("row still present after soft delete");

    // Isolate the deletion marker from the independent persisted-version
    // guard added in #2673: this test intentionally supplies the current
    // version while retaining the stale pre-delete marker.
    assert_eq!(tombstoned.version, replacement.version + 1);
    replacement.version = tombstoned.version;

    // Prove the isolation rather than asserting it in prose. `?13` matches
    // because the soft delete left the revision alone, and `?8 > updated_at`
    // holds because the prepared replacement advanced past it. That leaves
    // `deleted_at IS ?14` as the only conjunct able to refuse the write.
    assert_eq!(
        tombstoned.updated_at, expected_updated_at,
        "fixture premise: soft delete must NOT move `updated_at`, otherwise \
             `?13` would refuse and this stops being an isolating fixture"
    );
    assert!(
        replacement.updated_at > tombstoned.updated_at,
        "fixture premise: the replacement revision must still advance, \
             otherwise `?8 > updated_at` would refuse and this stops being an \
             isolating fixture"
    );
    assert!(
        tombstoned.deleted_at.is_some(),
        "fixture premise: the row must actually be tombstoned"
    );

    let committed = store
        .replace_entity_if_unchanged(replacement, expected_updated_at, expected_deleted_at)
        .await
        .expect("CAS query");
    assert!(
        !committed,
        "a replacement carrying a pre-delete snapshot must be refused after the row is \
             soft-deleted: without `deleted_at IS ?14` it would write `deleted_at = NULL` over \
             the tombstone and silently resurrect a deleted entity"
    );

    let after = store
        .get_entity_including_deleted(id)
        .await
        .expect("read back")
        .expect("row present");
    assert!(
        after.deleted_at.is_some(),
        "the tombstone must survive the refused write"
    );
    assert_eq!(
        after.properties,
        Some(serde_json::json!({"a": 0})),
        "the refused write must not have landed"
    );
}

/// Regression: the entity CAS requires the replacement revision to be
/// STRICTLY greater than the stored one. If the replacement is computed
/// as a raw `Utc::now()` read, a stored revision that is at or ahead of
/// wall-clock time (a clock step backward, or — deterministically,
/// reproduced here — a stored revision manufactured slightly ahead of
/// "now") makes the new value fail to advance, and the CAS refuses a
/// write with NO concurrent writer involved at all. The fix must clamp
/// the replacement to `max(now, stored + 1)`; this test sets the stored
/// revision one full second into the future (far outside normal clock
/// skew) and asserts the update still succeeds instead of surfacing a
/// spurious conflict.
///
/// SCOPE: this is a revision-clamp test, NOT CAS regression coverage. Its
/// assertion is that the write SUCCEEDS, which an unconditional UPDATE
/// also satisfies, so it stays green if the `updated_at = ?13` /
/// `?8 > updated_at` guard is dropped entirely. The guard's regression
/// coverage is
/// `concurrent_entity_property_patches_from_one_revision_only_one_survives`
/// (primitive) and `production_update_entity_refuses_concurrent_stale_writer`
/// (production wiring); do not count this test toward it.
#[tokio::test]
async fn update_entity_succeeds_when_stored_revision_is_ahead_of_wall_clock() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "FutureRevisionTarget",
            None,
            None,
            vec![],
        )
        .await
        .expect("seed entity");
    let id = entity.id;

    let future_micros = chrono::Utc::now().timestamp_micros() + 1_000_000;
    let pool = rt.backend().pool_arc();
    let id_str = id.to_string();
    tokio::task::spawn_blocking(move || {
        let guard = pool.writer().expect("writer connection");
        guard
            .execute(
                "UPDATE entities SET version = version + 1, updated_at = ?1 WHERE id = ?2",
                rusqlite::params![future_micros, id_str],
            )
            .expect("force future revision")
    })
    .await
    .expect("join");

    let updated = rt
        .update_entity(
            &tok,
            id,
            EntityPatch {
                description: Some(Some("patched after a forced future revision".to_string())),
                ..Default::default()
            },
        )
        .await
        .expect(
            "update must succeed and advance past the stored revision, not report a \
                 spurious conflict when nothing else wrote to this row",
        );
    assert!(updated.updated_at > future_micros);
}

#[tokio::test]
async fn update_entity_clear_description_with_some_none() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "ClearDesc",
            Some("has description"),
            None,
            vec![],
        )
        .await
        .unwrap();

    let updated = rt
        .update_entity(
            &tok,
            entity.id,
            EntityPatch {
                description: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(
        updated.description.is_none(),
        "description should be cleared"
    );
}

#[tokio::test]
async fn update_entity_reindexes_when_name_changes() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(&tok, "concept", None, "OldName", None, None, vec![])
        .await
        .unwrap();

    let hits_before = fts_hit(&rt, &tok, "OldName").await;
    assert!(
        hits_before.contains(&entity.id),
        "entity should be findable by old name"
    );

    rt.update_entity(
        &tok,
        entity.id,
        EntityPatch {
            name: Some("NewName".to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let hits_old = fts_hit(&rt, &tok, "OldName").await;
    let hits_new = fts_hit(&rt, &tok, "NewName").await;

    assert!(
        !hits_old.contains(&entity.id),
        "old name should no longer match after rename"
    );
    assert!(
        hits_new.contains(&entity.id),
        "new name should be findable after rename"
    );
}

#[tokio::test]
async fn update_entity_properties_merges_preserving_existing_keys() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "MergeProps",
            None,
            Some(serde_json::json!({
                "domain": "inference",
                "repo": "lattice",
                "status": "researched",
            })),
            vec![],
        )
        .await
        .unwrap();

    let updated = rt
        .update_entity(
            &tok,
            entity.id,
            EntityPatch {
                properties: Some(serde_json::json!({"status": "implemented"})),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let props = updated.properties.expect("properties should remain set");
    assert_eq!(props["domain"], "inference", "domain key must be preserved");
    assert_eq!(props["repo"], "lattice", "repo key must be preserved");
    assert_eq!(
        props["status"], "implemented",
        "status key must be updated by patch"
    );
}

#[tokio::test]
async fn update_entity_skips_reindex_when_only_properties_change() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(&tok, "concept", None, "StableIndexed", None, None, vec![])
        .await
        .unwrap();

    let hits_before = fts_hit(&rt, &tok, "StableIndexed").await;
    assert!(hits_before.contains(&entity.id));

    rt.update_entity(
        &tok,
        entity.id,
        EntityPatch {
            properties: Some(serde_json::json!({"new": "prop"})),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let hits_after = fts_hit(&rt, &tok, "StableIndexed").await;
    assert!(
        hits_after.contains(&entity.id),
        "still findable after props-only patch"
    );
}

#[tokio::test]
async fn merge_entity_rewires_edges() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let a = rt
        .create_entity(&tok, "concept", None, "A", None, None, vec![])
        .await
        .unwrap();
    let b = rt
        .create_entity(&tok, "concept", None, "B", None, None, vec![])
        .await
        .unwrap();
    let c = rt
        .create_entity(&tok, "concept", None, "C", None, None, vec![])
        .await
        .unwrap();
    let d = rt
        .create_entity(&tok, "concept", None, "D", None, None, vec![])
        .await
        .unwrap();

    // A→B and C→B; merge B into D → should become A→D and C→D.
    rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();
    rt.link(&tok, c.id, b.id, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();

    let summary = rt
        .merge_entity_with_reason(
            &tok,
            d.id,
            b.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .unwrap();

    assert_eq!(summary.kept_id, d.id);
    assert_eq!(summary.removed_id, b.id);
    assert_eq!(summary.edges_rewired, 2);

    let a_neighbors = rt
        .neighbors(&tok, a.id, Direction::Out, None, None)
        .await
        .unwrap();
    assert_eq!(a_neighbors.len(), 1);
    assert_eq!(a_neighbors[0].node_id, d.id);

    let c_neighbors = rt
        .neighbors(&tok, c.id, Direction::Out, None, None)
        .await
        .unwrap();
    assert_eq!(c_neighbors.len(), 1);
    assert_eq!(c_neighbors[0].node_id, d.id);
}

// khive#1236: edges incident to `from_id` but stamped with a namespace other
// than the merge caller's must still be discovered and rewired — by-ID edge
// endpoints are namespace-agnostic (ADR-007 Rev 6), and `link` stamps an edge
// with its *creator's* namespace, not either endpoint's.
#[tokio::test]
async fn merge_entity_rewires_edges_from_other_namespaces() {
    use crate::Namespace;

    let rt = rt();
    let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
    let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());

    let into_a = rt
        .create_entity(&ns_a, "concept", None, "Into A", None, None, vec![])
        .await
        .unwrap();
    let from_a = rt
        .create_entity(&ns_a, "concept", None, "From A", None, None, vec![])
        .await
        .unwrap();
    let foreign_b = rt
        .create_entity(&ns_b, "concept", None, "Foreign B", None, None, vec![])
        .await
        .unwrap();

    // Edge created by an ns_b caller, stamped with ns_b, whose target lives in
    // ns_a — legal because by-ID link endpoints are namespace-agnostic.
    rt.link(
        &ns_b,
        foreign_b.id,
        from_a.id,
        EdgeRelation::Extends,
        1.0,
        None,
    )
    .await
    .unwrap();

    let summary = rt
        .merge_entity_with_reason(
            &ns_a,
            into_a.id,
            from_a.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        summary.edges_rewired, 1,
        "the ns_b-stamped edge incident to from_id must be discovered and rewired, not missed"
    );

    let foreign_neighbors = rt
        .neighbors(&ns_b, foreign_b.id, Direction::Out, None, None)
        .await
        .unwrap();
    assert_eq!(
        foreign_neighbors.len(),
        1,
        "cross-namespace edge must survive the merge, rewired to point at into_id"
    );
    assert_eq!(foreign_neighbors[0].node_id, into_a.id);
}

// khive#1216: a merge rewire must re-check the pack endpoint contract for the
// POST-rewrite pair, not just carry the pre-merge edge over. into_id and
// from_id share `kind` (enforced by the caller) but may differ in
// `entity_type`, so a pack rule scoped via `EntityOfType` can accept
// `from_id`'s edge yet reject the identical relation once rewritten onto
// `into_id`.
#[tokio::test]
async fn merge_entity_drops_edge_violating_endpoint_contract_after_rewire() {
    let rt = rt();
    let tok = NamespaceToken::local();

    // depends_on is NOT in the base concept->concept allowlist; only this
    // pack rule (theorem -> definition) accepts it.
    rt.install_edge_rules(vec![EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: "theorem",
        },
        target: EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: "definition",
        },
    }]);

    let def_entity = rt
        .create_entity(
            &tok,
            "concept",
            Some("definition"),
            "Def",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let from_theorem = rt
        .create_entity(
            &tok,
            "concept",
            Some("theorem"),
            "FromTheorem",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    // Same base kind ("concept") as from_theorem, but a different entity_type
    // — the merge's same-kind constraint allows this, the endpoint contract
    // (entity_type-scoped) does not.
    let into_lemma = rt
        .create_entity(
            &tok,
            "concept",
            Some("lemma"),
            "IntoLemma",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();

    rt.link(
        &tok,
        from_theorem.id,
        def_entity.id,
        EdgeRelation::DependsOn,
        1.0,
        None,
    )
    .await
    .unwrap();

    let summary = rt
        .merge_entity_with_reason(
            &tok,
            into_lemma.id,
            from_theorem.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        summary.edges_rewired, 0,
        "the contract-violating rewire must not be counted as rewired"
    );
    assert_eq!(
        summary.edges_contract_skipped, 1,
        "the depends_on edge must be dropped, not silently rewritten past the endpoint contract"
    );

    let def_neighbors = rt
        .neighbors(&tok, def_entity.id, Direction::In, None, None)
        .await
        .unwrap();
    assert!(
        def_neighbors.is_empty(),
        "no contract-violating depends_on edge should survive onto into_lemma; got {def_neighbors:?}"
    );
}

// Dry-run counterpart: a contract-violating rewire must be predicted as
// skipped (not rewired), and no write occurs.
#[tokio::test]
async fn merge_entity_dry_run_predicts_contract_skip_without_writing() {
    let rt = rt();
    let tok = NamespaceToken::local();

    rt.install_edge_rules(vec![EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: "theorem",
        },
        target: EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: "definition",
        },
    }]);

    let def_entity = rt
        .create_entity(
            &tok,
            "concept",
            Some("definition"),
            "Def",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let from_theorem = rt
        .create_entity(
            &tok,
            "concept",
            Some("theorem"),
            "FromTheorem",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let into_lemma = rt
        .create_entity(
            &tok,
            "concept",
            Some("lemma"),
            "IntoLemma",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();

    rt.link(
        &tok,
        from_theorem.id,
        def_entity.id,
        EdgeRelation::DependsOn,
        1.0,
        None,
    )
    .await
    .unwrap();

    let summary = rt
        .merge_entity_with_reason(
            &tok,
            into_lemma.id,
            from_theorem.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            true, // dry_run
            None,
        )
        .await
        .unwrap();

    assert_eq!(summary.edges_rewired, 0);
    assert_eq!(summary.edges_contract_skipped, 1);

    // Nothing written: the original edge is untouched.
    let def_neighbors = rt
        .neighbors(&tok, def_entity.id, Direction::In, None, None)
        .await
        .unwrap();
    assert_eq!(def_neighbors.len(), 1);
    assert_eq!(def_neighbors[0].node_id, from_theorem.id);
}

// A conflicting rewire must leave the surviving edge untouched (ADR-039
// DO NOTHING) — the merged-from edge's attributes never overwrite it.
#[tokio::test]
async fn merge_entity_conflict_keeps_survivor_edge_attributes() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", None, None, vec![])
        .await
        .unwrap();
    let shared = rt
        .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
        .await
        .unwrap();

    let survivor = rt
        .link(&tok, into.id, shared.id, EdgeRelation::Extends, 0.9, None)
        .await
        .unwrap();
    rt.link(&tok, from.id, shared.id, EdgeRelation::Extends, 0.2, None)
        .await
        .unwrap();

    rt.merge_entity(
        &tok,
        into.id,
        from.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
    )
    .await
    .unwrap();

    let edges = rt
        .list_edges(
            &tok,
            crate::EdgeListFilter {
                source_id: Some(into.id),
                target_id: Some(shared.id),
                relations: vec![EdgeRelation::Extends],
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].id, survivor.id);
    assert!(
        (edges[0].weight - 0.9).abs() < f64::EPSILON,
        "survivor weight must not be overwritten by the merged-from edge; got {}",
        edges[0].weight
    );
}

#[tokio::test]
async fn merge_entity_conflict_records_restorable_edge_and_annotation_preimages() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", None, None, vec![])
        .await
        .unwrap();
    let shared = rt
        .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
        .await
        .unwrap();
    let annotator = rt
        .create_note(
            &tok,
            "observation",
            None,
            "edge judgment",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let nested_annotator = rt
        .create_note(
            &tok,
            "observation",
            None,
            "judgment review",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();

    let survivor = rt
        .link(
            &tok,
            into.id,
            shared.id,
            EdgeRelation::Extends,
            0.9,
            Some(serde_json::json!({"source": "survivor"})),
        )
        .await
        .unwrap();
    let dropped = rt
        .link(
            &tok,
            from.id,
            shared.id,
            EdgeRelation::Extends,
            0.2,
            Some(serde_json::json!({"source": "dropped"})),
        )
        .await
        .unwrap();
    let annotation = rt
        .link(
            &tok,
            annotator.id,
            dropped.id.into(),
            EdgeRelation::Annotates,
            0.7,
            Some(serde_json::json!({"basis": "manual"})),
        )
        .await
        .unwrap();
    let nested_annotation = rt
        .link(
            &tok,
            nested_annotator.id,
            annotation.id.into(),
            EdgeRelation::Annotates,
            0.6,
            Some(serde_json::json!({"review": "confirmed"})),
        )
        .await
        .unwrap();
    rt.delete_edge(&tok, annotation.id.into(), false)
        .await
        .unwrap();

    let summary = rt
        .merge_entity(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();

    let [conflict] = summary.edge_conflict_preimages.as_slice() else {
        panic!(
            "expected one edge-conflict preimage, got {:?}",
            summary.edge_conflict_preimages
        );
    };
    assert_eq!(conflict.surviving_edge_id, Uuid::from(survivor.id));
    assert_eq!(conflict.dropped_edge.id, Uuid::from(dropped.id));
    assert_eq!(conflict.dropped_edge.source_id, from.id);
    assert_eq!(conflict.dropped_edge.target_id, shared.id);
    assert_eq!(conflict.dropped_edge.relation, "extends");
    assert_eq!(conflict.dropped_edge.weight, 0.2);
    assert_eq!(
        conflict.dropped_edge.metadata,
        Some(serde_json::json!({"source": "dropped"}))
    );
    assert_eq!(conflict.incident_edge_preimages.len(), 2);
    assert_eq!(
        conflict.incident_edge_preimages[0].id,
        Uuid::from(annotation.id)
    );
    assert_eq!(
        conflict.incident_edge_preimages[1].id,
        Uuid::from(nested_annotation.id)
    );

    for id in [dropped.id, annotation.id, nested_annotation.id] {
        assert!(
            rt.get_edge_including_deleted(&tok, id.into())
                .await
                .unwrap()
                .is_none(),
            "merge conflict cascade must leave no dangling edge row for {id}"
        );
    }

    let events = rt
        .events(&tok)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![EventKind::EntityMerged],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(events.items.len(), 1);
    assert_eq!(
        events.items[0].payload["edge_conflict_preimages"],
        serde_json::to_value(&summary.edge_conflict_preimages).unwrap()
    );

    restore_edge_preimage(&rt, &tok, &conflict.dropped_edge).await;
    for preimage in &conflict.incident_edge_preimages {
        restore_edge_preimage(&rt, &tok, preimage).await;
    }
    for preimage in
        std::iter::once(&conflict.dropped_edge).chain(conflict.incident_edge_preimages.iter())
    {
        let restored = rt
            .get_edge_including_deleted(&tok, preimage.id)
            .await
            .unwrap()
            .expect("restored edge");
        assert_edge_matches_preimage(&restored, preimage);
    }
}

// A dry run must predict the same conflict preimages a committing merge
// would produce, without deleting or mutating a single row. The incident
// cascade is two levels deep (an annotation on the dropped edge, and a
// nested annotation on that annotation) so the root-to-leaf ordering
// ADR-014 promises is actually exercised, not just a one-element vec that
// trivially satisfies any order. Every row touched by the merge — both
// entities and every edge — is snapshotted before the dry run and
// compared field-for-field against its post-run state.
#[tokio::test]
async fn merge_entity_dry_run_conflict_returns_preimages_without_mutating() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", None, None, vec![])
        .await
        .unwrap();
    let shared = rt
        .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
        .await
        .unwrap();
    let annotator = rt
        .create_note(
            &tok,
            "observation",
            None,
            "edge judgment",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let nested_annotator = rt
        .create_note(
            &tok,
            "observation",
            None,
            "judgment review",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();

    let survivor = rt
        .link(
            &tok,
            into.id,
            shared.id,
            EdgeRelation::Extends,
            0.9,
            Some(serde_json::json!({"source": "survivor"})),
        )
        .await
        .unwrap();
    let dropped = rt
        .link(
            &tok,
            from.id,
            shared.id,
            EdgeRelation::Extends,
            0.2,
            Some(serde_json::json!({"source": "dropped"})),
        )
        .await
        .unwrap();
    let annotation = rt
        .link(
            &tok,
            annotator.id,
            dropped.id.into(),
            EdgeRelation::Annotates,
            0.7,
            Some(serde_json::json!({"basis": "manual"})),
        )
        .await
        .unwrap();
    let nested_annotation = rt
        .link(
            &tok,
            nested_annotator.id,
            annotation.id.into(),
            EdgeRelation::Annotates,
            0.6,
            Some(serde_json::json!({"basis": "nested"})),
        )
        .await
        .unwrap();
    rt.delete_edge(&tok, nested_annotation.id.into(), false)
        .await
        .unwrap();

    let survivor_before = rt
        .get_edge_including_deleted(&tok, survivor.id.into())
        .await
        .unwrap()
        .expect("survivor edge exists");
    let dropped_before = rt
        .get_edge_including_deleted(&tok, dropped.id.into())
        .await
        .unwrap()
        .expect("dropped edge exists");
    let annotation_before = rt
        .get_edge_including_deleted(&tok, annotation.id.into())
        .await
        .unwrap()
        .expect("annotation edge exists");
    let nested_annotation_before = rt
        .get_edge_including_deleted(&tok, nested_annotation.id.into())
        .await
        .unwrap()
        .expect("nested annotation edge exists");
    let into_before = rt
        .get_entity(&tok, into.id)
        .await
        .expect("into entity exists");
    let from_before = rt
        .get_entity(&tok, from.id)
        .await
        .expect("from entity exists");

    let summary = rt
        .merge_entity(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            true,
        )
        .await
        .unwrap();

    let [conflict] = summary.edge_conflict_preimages.as_slice() else {
        panic!(
            "expected one edge-conflict preimage from the dry run, got {:?}",
            summary.edge_conflict_preimages
        );
    };
    assert_eq!(conflict.surviving_edge_id, Uuid::from(survivor.id));
    assert_eq!(conflict.dropped_edge.id, Uuid::from(dropped.id));
    assert_eq!(conflict.dropped_edge.source_id, from.id);
    assert_eq!(conflict.dropped_edge.target_id, shared.id);
    assert_eq!(conflict.dropped_edge.weight, 0.2);
    // Root-to-leaf order (ADR-014): the direct annotation on the dropped
    // edge must precede the annotation nested on top of it.
    assert_eq!(conflict.incident_edge_preimages.len(), 2);
    assert_eq!(
        conflict.incident_edge_preimages[0].id,
        Uuid::from(annotation.id)
    );
    assert!(
        conflict.incident_edge_preimages[0].deleted_at.is_none(),
        "the direct annotation was never soft-deleted"
    );
    assert_eq!(
        conflict.incident_edge_preimages[1].id,
        Uuid::from(nested_annotation.id)
    );
    assert!(
        conflict.incident_edge_preimages[1].deleted_at.is_some(),
        "dry-run preimage must retain the nested annotation's tombstone state"
    );

    let survivor_after = rt
        .get_edge_including_deleted(&tok, survivor.id.into())
        .await
        .unwrap()
        .expect("dry run must not delete the survivor edge");
    let dropped_after = rt
        .get_edge_including_deleted(&tok, dropped.id.into())
        .await
        .unwrap()
        .expect("dry run must not delete the dropped edge");
    let annotation_after = rt
        .get_edge_including_deleted(&tok, annotation.id.into())
        .await
        .unwrap()
        .expect("dry run must not delete the cascaded annotation");
    let nested_annotation_after = rt
        .get_edge_including_deleted(&tok, nested_annotation.id.into())
        .await
        .unwrap()
        .expect("dry run must not delete the nested cascaded annotation");
    assert_eq!(
        serde_json::to_value(&survivor_before).unwrap(),
        serde_json::to_value(&survivor_after).unwrap(),
        "dry run must not mutate the surviving edge's row at all"
    );
    assert_eq!(
        serde_json::to_value(&dropped_before).unwrap(),
        serde_json::to_value(&dropped_after).unwrap(),
        "dry run must not mutate the would-be-dropped edge's row at all"
    );
    assert_eq!(
        serde_json::to_value(&annotation_before).unwrap(),
        serde_json::to_value(&annotation_after).unwrap(),
        "dry run must not mutate the incident annotation's row at all"
    );
    assert_eq!(
        serde_json::to_value(&nested_annotation_before).unwrap(),
        serde_json::to_value(&nested_annotation_after).unwrap(),
        "dry run must not mutate the nested incident annotation's row at all"
    );

    let into_after = rt
        .get_entity(&tok, into.id)
        .await
        .expect("into entity must remain unmerged after a dry run");
    let from_after = rt
        .get_entity(&tok, from.id)
        .await
        .expect("from entity must not be merged away by a dry run");
    assert_eq!(
        serde_json::to_value(&into_before).unwrap(),
        serde_json::to_value(&into_after).unwrap(),
        "dry run must not mutate the into entity's row at all"
    );
    assert_eq!(
        serde_json::to_value(&from_before).unwrap(),
        serde_json::to_value(&from_after).unwrap(),
        "dry run must not mutate the from entity's row at all"
    );
    assert_eq!(from_after.deleted_at, None);
    assert_eq!(from_after.merged_into, None);
    assert_eq!(from_after.merge_event_id, None);

    let events = rt
        .events(&tok)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![EventKind::EntityMerged],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert!(
        events.items.is_empty(),
        "a dry run must not record a merge audit event"
    );
}

// A soft-deleted surviving edge must not be resurrected by a conflicting
// rewire — the from-edge is dropped and the tombstone stays.
#[tokio::test]
async fn merge_entity_conflict_does_not_resurrect_tombstoned_edge() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", None, None, vec![])
        .await
        .unwrap();
    let shared = rt
        .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
        .await
        .unwrap();

    let survivor = rt
        .link(&tok, into.id, shared.id, EdgeRelation::Extends, 0.9, None)
        .await
        .unwrap();
    rt.delete_edge(&tok, survivor.id.into(), false)
        .await
        .unwrap();
    rt.link(&tok, from.id, shared.id, EdgeRelation::Extends, 0.2, None)
        .await
        .unwrap();

    rt.merge_entity(
        &tok,
        into.id,
        from.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
    )
    .await
    .unwrap();

    for (src, label) in [(into.id, "into"), (from.id, "from")] {
        let edges = rt
            .list_edges(
                &tok,
                crate::EdgeListFilter {
                    source_id: Some(src),
                    target_id: Some(shared.id),
                    relations: vec![EdgeRelation::Extends],
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert!(
            edges.is_empty(),
            "no live {label}→shared edge may exist after merging over a tombstone; got: {edges:?}"
        );
    }
}

// The survivor row write must not null columns it doesn't merge —
// entity_type (and the old entity-owned content_ref) were lost by the old full-row
// INSERT OR REPLACE.
#[tokio::test]
async fn merge_entity_preserves_survivor_entity_type() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "resource", Some("skill"), "Into", None, None, vec![])
        .await
        .unwrap();
    assert_eq!(into.entity_type.as_deref(), Some("skill"));
    let from = rt
        .create_entity(&tok, "resource", None, "From", None, None, vec![])
        .await
        .unwrap();

    rt.merge_entity(
        &tok,
        into.id,
        from.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
    )
    .await
    .unwrap();

    let got = rt.get_entity(&tok, into.id).await.unwrap();
    assert_eq!(
        got.entity_type.as_deref(),
        Some("skill"),
        "merge must not null the survivor's entity_type"
    );
}

#[tokio::test]
async fn merge_entity_preserves_survivor_content_ref() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "document", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "document", None, "From", None, None, vec![])
        .await
        .unwrap();

    let content_ref = khive_storage::ContentRef::from_hex("0".repeat(64)).unwrap();
    let store = rt.entities(&tok).unwrap();
    rt.attachments()
        .unwrap()
        .upsert_attachment(khive_storage::Attachment::from_new(
            into.id,
            khive_storage::AttachmentSubstrate::Entity,
            khive_storage::NewAttachment {
                role: "content".to_string(),
                content_ref: content_ref.clone(),
                media_type: None,
                size_bytes: None,
            },
            into.created_at,
        ))
        .await
        .unwrap();

    rt.merge_entity(
        &tok,
        into.id,
        from.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
    )
    .await
    .unwrap();

    let got = store.get_entity(into.id).await.unwrap().unwrap();
    assert_eq!(
        got.content_ref.as_deref(),
        Some(content_ref.as_str()),
        "merge must not null the survivor's content_ref"
    );
}

#[tokio::test]
async fn merge_entity_self_merge_rejected() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let a = rt
        .create_entity(&tok, "concept", None, "A", None, None, vec![])
        .await
        .unwrap();
    let err = rt
        .merge_entity_with_reason(
            &tok,
            a.id,
            a.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("cannot merge an entity into itself"),
        "expected self-merge rejection, got: {err:?}"
    );
}

#[tokio::test]
async fn merge_entity_prefer_into_strategy() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "Into",
            None,
            Some(serde_json::json!({"a": 1})),
            vec![],
        )
        .await
        .unwrap();
    let from = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "From",
            None,
            Some(serde_json::json!({"a": 2, "b": 3})),
            vec![],
        )
        .await
        .unwrap();

    rt.merge_entity_with_reason(
        &tok,
        into.id,
        from.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
        None,
    )
    .await
    .unwrap();

    let kept = rt.get_entity(&tok, into.id).await.unwrap();
    let props = kept.properties.unwrap();
    // a stays as 1 (into wins), b is added from from.
    assert_eq!(props["a"], 1);
    assert_eq!(props["b"], 3);
}

#[tokio::test]
async fn merge_entity_prefer_from_strategy() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "Into",
            None,
            Some(serde_json::json!({"a": 1})),
            vec![],
        )
        .await
        .unwrap();
    let from = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "From",
            None,
            Some(serde_json::json!({"a": 2, "b": 3})),
            vec![],
        )
        .await
        .unwrap();

    rt.merge_entity_with_reason(
        &tok,
        into.id,
        from.id,
        EntityDedupMergePolicy::PreferFrom,
        ContentMergeStrategy::Append,
        false,
        None,
    )
    .await
    .unwrap();

    let kept = rt.get_entity(&tok, into.id).await.unwrap();
    let props = kept.properties.unwrap();
    // from wins on a, b also from from.
    assert_eq!(props["a"], 2);
    assert_eq!(props["b"], 3);
}

#[tokio::test]
async fn merge_entity_union_strategy() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "Into",
            None,
            Some(serde_json::json!({"a": 1})),
            vec![],
        )
        .await
        .unwrap();
    let from = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "From",
            None,
            Some(serde_json::json!({"a": 2, "b": 3})),
            vec![],
        )
        .await
        .unwrap();

    rt.merge_entity_with_reason(
        &tok,
        into.id,
        from.id,
        EntityDedupMergePolicy::Union,
        ContentMergeStrategy::Append,
        false,
        None,
    )
    .await
    .unwrap();

    let kept = rt.get_entity(&tok, into.id).await.unwrap();
    let props = kept.properties.unwrap();
    // Scalar conflict: into wins → a=1. b added from from.
    assert_eq!(props["a"], 1);
    assert_eq!(props["b"], 3);
}

#[tokio::test]
async fn merge_entity_unions_tags() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "Into",
            None,
            None,
            vec!["x".to_string(), "y".to_string()],
        )
        .await
        .unwrap();
    let from = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "From",
            None,
            None,
            vec!["y".to_string(), "z".to_string()],
        )
        .await
        .unwrap();

    rt.merge_entity_with_reason(
        &tok,
        into.id,
        from.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
        None,
    )
    .await
    .unwrap();

    let kept = rt.get_entity(&tok, into.id).await.unwrap();
    let mut tags = kept.tags.clone();
    tags.sort();
    assert_eq!(tags, vec!["x", "y", "z"]);
}

/// An event-store failure must roll back the edge deletion and tombstone.
/// Before the transactional event insert, this left a committed merge with
/// no durable preimage, even though the call returned an error.
#[tokio::test]
async fn entity_merge_event_insert_failure_rolls_back_destructive_merge() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", None, None, vec![])
        .await
        .unwrap();
    let edge = rt
        .link(&tok, into.id, from.id, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();
    let event_store = rt.events(&tok).unwrap();
    set_merge_event_refusal(&rt, "entity_merged", true);

    let failed = rt
        .merge_entity(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await;
    assert!(failed.is_err(), "event insert must abort the merge");
    let source = rt.get_entity(&tok, from.id).await.unwrap();
    assert_eq!(source.merge_event_id, None);
    assert!(source.deleted_at.is_none());
    assert!(
        rt.get_edge_including_deleted(&tok, edge.id.into())
            .await
            .unwrap()
            .is_some(),
        "the deleted self-loop must roll back"
    );
    let filter = khive_storage::EventFilter {
        kinds: vec![EventKind::EntityMerged],
        ..Default::default()
    };
    let page = khive_storage::types::PageRequest {
        offset: 0,
        limit: 10,
    };
    assert!(event_store
        .query_events(filter.clone(), page.clone())
        .await
        .unwrap()
        .items
        .is_empty());

    set_merge_event_refusal(&rt, "entity_merged", false);
    let summary = rt
        .merge_entity(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();
    let tombstone = rt
        .entities(&tok)
        .unwrap()
        .get_entity_including_deleted(from.id)
        .await
        .unwrap()
        .unwrap();
    let events = event_store.query_events(filter, page).await.unwrap();
    assert_eq!(events.items.len(), 1);
    assert_eq!(tombstone.merge_event_id, Some(events.items[0].id));
    assert_eq!(
        events.items[0].payload["self_loop_edge_preimages"],
        serde_json::to_value(&summary.self_loop_edge_preimages).unwrap()
    );
}

#[tokio::test]
async fn merge_entity_drops_self_loops() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let a = rt
        .create_entity(&tok, "concept", None, "A", None, None, vec![])
        .await
        .unwrap();
    let b = rt
        .create_entity(&tok, "concept", None, "B", None, None, vec![])
        .await
        .unwrap();

    // A `extends` B — merging B into A would produce A `extends` A → drop it.
    let edge = rt
        .link(
            &tok,
            a.id,
            b.id,
            EdgeRelation::Extends,
            0.6,
            Some(serde_json::json!({"basis": "shared lineage"})),
        )
        .await
        .unwrap();

    let summary = rt
        .merge_entity_with_reason(
            &tok,
            a.id,
            b.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        summary.edges_rewired, 0,
        "self-loop should be dropped, not rewired"
    );

    // The dropped self-loop must be counted and its full preimage
    // captured (khive#2934) — before this fix the edge vanished with
    // neither the counter nor a recoverable row.
    assert_eq!(summary.edges_self_loop_dropped, 1);
    let [preimage] = summary.self_loop_edge_preimages.as_slice() else {
        panic!(
            "expected exactly one self-loop preimage, got {:?}",
            summary.self_loop_edge_preimages
        );
    };
    assert_eq!(preimage.id, Uuid::from(edge.id));
    assert_eq!(preimage.source_id, a.id);
    assert_eq!(preimage.target_id, b.id);
    assert_eq!(preimage.relation, "extends");
    assert_eq!(preimage.weight, 0.6);
    assert_eq!(
        preimage.metadata,
        Some(serde_json::json!({"basis": "shared lineage"}))
    );

    let a_out = rt
        .neighbors(&tok, a.id, Direction::Out, None, None)
        .await
        .unwrap();
    assert!(a_out.is_empty(), "no self-loop should remain");

    let events = rt
        .events(&tok)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![EventKind::EntityMerged],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(events.items.len(), 1);
    assert_eq!(
        events.items[0].payload["edges_self_loop_dropped"],
        serde_json::json!(1)
    );
    assert_eq!(
        events.items[0].payload["self_loop_edge_preimages"],
        serde_json::to_value(&summary.self_loop_edge_preimages).unwrap()
    );
}

// A dry run must predict the exact self-loop-drop count and preimage a
// committed merge produces — before khive#2934 the `continue` in the
// self-loop branch ran before both the write gate and any counter, so a
// dry run and a real run were indistinguishable for this case.
#[tokio::test]
async fn merge_entity_self_loop_dry_run_matches_real_run() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", None, None, vec![])
        .await
        .unwrap();
    let edge = rt
        .link(
            &tok,
            into.id,
            from.id,
            EdgeRelation::Extends,
            0.5,
            Some(serde_json::json!({"basis": "dry-run parity"})),
        )
        .await
        .unwrap();

    let dry_summary = rt
        .merge_entity(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            true,
        )
        .await
        .unwrap();

    assert!(
        rt.get_edge_including_deleted(&tok, edge.id.into())
            .await
            .unwrap()
            .is_some(),
        "dry run must not delete the self-loop edge"
    );

    let real_summary = rt
        .merge_entity(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();

    assert_eq!(dry_summary.edges_self_loop_dropped, 1);
    let [dry_preimage] = dry_summary.self_loop_edge_preimages.as_slice() else {
        panic!(
            "expected exactly one predicted self-loop preimage, got {:?}",
            dry_summary.self_loop_edge_preimages
        );
    };
    assert_eq!(dry_preimage.id, Uuid::from(edge.id));
    assert_eq!(dry_preimage.source_id, into.id);
    assert_eq!(dry_preimage.target_id, from.id);
    assert_eq!(dry_preimage.relation, "extends");
    assert_eq!(dry_preimage.weight, 0.5);
    assert_eq!(
        dry_summary.edges_self_loop_dropped, real_summary.edges_self_loop_dropped,
        "a dry run must predict the same self-loop-drop count the committed merge produces"
    );
    assert_eq!(
        dry_summary.self_loop_edge_preimages, real_summary.self_loop_edge_preimages,
        "a dry run must predict the exact preimage the committed merge produces"
    );

    assert!(
        rt.get_edge_including_deleted(&tok, edge.id.into())
            .await
            .unwrap()
            .is_none(),
        "the committed merge must actually delete the self-loop edge"
    );
}

// Control: no edge exists directly between the merge operands, only one
// that survives the rewire — the self-loop counter must stay at zero
// rather than firing on every rewired edge.
#[tokio::test]
async fn merge_entity_no_self_loop_between_operands_reports_zero() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", None, None, vec![])
        .await
        .unwrap();
    let other = rt
        .create_entity(&tok, "concept", None, "Other", None, None, vec![])
        .await
        .unwrap();

    rt.link(&tok, from.id, other.id, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();

    let summary = rt
        .merge_entity(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();

    assert_eq!(
        summary.edges_rewired, 1,
        "the non-self-loop edge must still rewire"
    );
    assert_eq!(
        summary.edges_self_loop_dropped, 0,
        "no self-loop exists between the merge operands"
    );
    assert!(summary.self_loop_edge_preimages.is_empty());
}

// ---- content_strategy for entity merge ----

#[tokio::test]
async fn merge_entity_append_strategy_concatenates_descriptions() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", Some("desc A"), None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", Some("desc B"), None, vec![])
        .await
        .unwrap();

    let summary = rt
        .merge_entity_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .unwrap();

    assert!(
        summary.content_appended,
        "append strategy with two non-empty descriptions must report content_appended=true"
    );
    let kept = rt.get_entity(&tok, into.id).await.unwrap();
    assert_eq!(kept.description.as_deref(), Some("desc A\n\n---\n\ndesc B"));
}

#[tokio::test]
async fn merge_entity_append_strategy_from_empty_is_noop() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", Some("desc A"), None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", None, None, vec![])
        .await
        .unwrap();

    let summary = rt
        .merge_entity_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .unwrap();

    assert!(
        !summary.content_appended,
        "from's empty description means nothing was appended"
    );
    let kept = rt.get_entity(&tok, into.id).await.unwrap();
    assert_eq!(kept.description.as_deref(), Some("desc A"));
}

#[tokio::test]
async fn merge_entity_append_strategy_into_empty_takes_from() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", Some("desc B"), None, vec![])
        .await
        .unwrap();

    let summary = rt
        .merge_entity_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .unwrap();

    assert!(
        summary.content_appended,
        "taking from's description into an empty into is real content preservation"
    );
    let kept = rt.get_entity(&tok, into.id).await.unwrap();
    assert_eq!(kept.description.as_deref(), Some("desc B"));
}

#[tokio::test]
async fn merge_entity_prefer_into_strategy_still_discards_explicitly() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", Some("desc A"), None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", Some("desc B"), None, vec![])
        .await
        .unwrap();

    let summary = rt
        .merge_entity_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::PreferInto,
            false,
            None,
        )
        .await
        .unwrap();

    assert!(
        !summary.content_appended,
        "explicit PreferInto opt-out must not report an append"
    );
    let kept = rt.get_entity(&tok, into.id).await.unwrap();
    assert_eq!(
        kept.description.as_deref(),
        Some("desc A"),
        "explicit PreferInto opt-out keeps the old discard behavior"
    );
}

/// `content_strategy` must be followed directly, independent of the
/// entity-field `strategy`: with the default entity policy `prefer_into`,
/// an explicit `content_strategy=prefer_from` must still keep the
/// from-description.
#[tokio::test]
async fn merge_entity_prefer_from_content_strategy_wins_over_default_entity_policy() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", Some("desc A"), None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", Some("desc B"), None, vec![])
        .await
        .unwrap();

    let summary = rt
        .merge_entity_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::PreferFrom,
            false,
            None,
        )
        .await
        .unwrap();

    assert!(
        !summary.content_appended,
        "explicit PreferFrom is not an append"
    );
    let kept = rt.get_entity(&tok, into.id).await.unwrap();
    assert_eq!(
        kept.description.as_deref(),
        Some("desc B"),
        "content_strategy=prefer_from must win over the default prefer_into entity policy"
    );
}

#[tokio::test]
async fn merge_entity_dry_run_previews_append() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", Some("desc A"), None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", Some("desc B"), None, vec![])
        .await
        .unwrap();

    let summary = rt
        .merge_entity_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            true,
            None,
        )
        .await
        .unwrap();

    assert!(summary.dry_run);
    assert!(
        summary.content_appended,
        "dry-run must preview the append outcome without writing"
    );
    let kept = rt.get_entity(&tok, into.id).await.unwrap();
    assert_eq!(
        kept.description.as_deref(),
        Some("desc A"),
        "dry_run=true must not mutate the into entity's description"
    );
}

/// Dry-run must be a read-only, accurate preview: it must predict
/// `edges_rewired` without writing, and must not append an `EntityMerged` event.
#[tokio::test]
async fn merge_entity_dry_run_predicts_edges_rewired_without_writing() {
    use khive_storage::EdgeRelation;

    let rt = rt();
    let tok = NamespaceToken::local();
    let a = rt
        .create_entity(&tok, "concept", None, "A", None, None, vec![])
        .await
        .unwrap();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", None, None, vec![])
        .await
        .unwrap();

    rt.link(&tok, a.id, from.id, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();

    let summary = rt
        .merge_entity_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            true,
            None,
        )
        .await
        .unwrap();

    assert!(summary.dry_run);
    assert_eq!(
        summary.edges_rewired, 1,
        "dry-run must predict the edge that would be rewired, not report zero"
    );

    let a_neighbors = rt
        .neighbors(&tok, a.id, Direction::Out, None, None)
        .await
        .unwrap();
    assert_eq!(a_neighbors.len(), 1);
    assert_eq!(
        a_neighbors[0].node_id, from.id,
        "dry_run=true must not rewire any edges"
    );

    let events = rt
        .events(&tok)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![EventKind::EntityMerged],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert!(
        events.items.is_empty(),
        "dry_run=true must not append an EntityMerged event"
    );
}

/// ADR-014: `reason` is additive — when supplied it must land in the
/// `EntityMerged` payload verbatim; the key must be entirely absent (not
/// `null`) when the caller omits it.
#[tokio::test]
async fn merge_entity_event_reason_present_when_supplied_absent_when_not() {
    let rt = rt();
    let tok = NamespaceToken::local();

    let into_a = rt
        .create_entity(&tok, "concept", None, "IntoA", None, None, vec![])
        .await
        .unwrap();
    let from_a = rt
        .create_entity(&tok, "concept", None, "FromA", None, None, vec![])
        .await
        .unwrap();
    rt.merge_entity_with_reason(
        &tok,
        into_a.id,
        from_a.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
        Some("duplicate".to_string()),
    )
    .await
    .unwrap();

    let into_b = rt
        .create_entity(&tok, "concept", None, "IntoB", None, None, vec![])
        .await
        .unwrap();
    let from_b = rt
        .create_entity(&tok, "concept", None, "FromB", None, None, vec![])
        .await
        .unwrap();
    rt.merge_entity_with_reason(
        &tok,
        into_b.id,
        from_b.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
        None,
    )
    .await
    .unwrap();

    let events = rt
        .events(&tok)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![EventKind::EntityMerged],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(events.items.len(), 2);

    let with_reason = events
        .items
        .iter()
        .find(|e| {
            e.payload.get("from_id").and_then(|v| v.as_str())
                == Some(from_a.id.to_string()).as_deref()
        })
        .expect("event for the reasoned merge must exist");
    assert_eq!(
        with_reason.payload.get("reason").and_then(|v| v.as_str()),
        Some("duplicate"),
        "reason must be threaded verbatim into the payload when supplied"
    );

    let without_reason = events
        .items
        .iter()
        .find(|e| {
            e.payload.get("from_id").and_then(|v| v.as_str())
                == Some(from_b.id.to_string()).as_deref()
        })
        .expect("event for the reasonless merge must exist");
    assert!(
        without_reason.payload.get("reason").is_none(),
        "reason key must be absent (never null) when the caller omits it, got: {:?}",
        without_reason.payload
    );
}

/// ADR-018 Amendment 5 says the forced-merge trail names the acting actor.
/// The emission site passes an empty actor string, so reading it alone says
/// the opposite; `KhiveRuntime::events` wraps the store in the attribution
/// decorator, which replaces namespace and actor from the authorized token
/// on every append. This pins the PERSISTED value, and the second arm makes
/// it a reading of the token rather than of a constant.
#[tokio::test]
async fn a_forced_merge_event_names_the_acting_actor_not_an_empty_string() {
    async fn forced_merge_event_actor(actor_id: Option<&str>) -> (String, serde_json::Value) {
        let rt = KhiveRuntime::new(crate::RuntimeConfig {
            db_path: None,
            packs: vec!["kg".to_string()],
            brain_profile: None,
            actor_id: actor_id.map(str::to_string),
            ..crate::RuntimeConfig::no_embeddings()
        })
        .expect("runtime");
        let tok = rt.authorize(crate::Namespace::local()).expect("authorize");
        let into = rt
            .create_entity(&tok, "concept", None, "Flash Attention", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "Paged KV Cache", None, None, vec![])
            .await
            .unwrap();
        rt.merge_entity_with_reason_and_force(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
            true,
        )
        .await
        .expect("the floor refuses this pair, so only force lands it");
        let events = rt
            .events(&tok)
            .unwrap()
            .query_events(
                khive_storage::EventFilter {
                    kinds: vec![EventKind::EntityMerged],
                    ..Default::default()
                },
                khive_storage::types::PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(events.items.len(), 1, "one forced merge, one event");
        let event = &events.items[0];
        (event.actor.clone(), event.payload.clone())
    }

    let (actor, payload) = forced_merge_event_actor(Some("merge-forcer")).await;
    assert_eq!(
        actor, "actor:merge-forcer",
        "the persisted event must name the actor the token carries"
    );
    assert_eq!(
        payload.get("force"),
        Some(&serde_json::Value::Bool(true)),
        "the force marker rides the same event: {payload}"
    );

    let (anonymous, _) = forced_merge_event_actor(None).await;
    assert_eq!(
        anonymous, "anonymous:local",
        "an unconfigured runtime stamps the anonymous fallback, so the field \
             tracks the token rather than a constant"
    );
}

#[tokio::test]
async fn merge_entity_with_reason_preserves_an_explicit_empty_reason() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", None, None, vec![])
        .await
        .unwrap();

    rt.merge_entity_with_reason(
        &tok,
        into.id,
        from.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
        Some(String::new()),
    )
    .await
    .unwrap();

    let events = rt
        .events(&tok)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![EventKind::EntityMerged],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(events.items.len(), 1);
    assert_eq!(
        events.items[0].payload.get("reason"),
        Some(&Value::String(String::new()))
    );
}

#[tokio::test]
async fn merge_entity_with_reason_rejects_secrets_before_reads_or_writes() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", None, None, vec![])
        .await
        .unwrap();
    let secret = secret_shaped_reason();

    let error = rt
        .merge_entity_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            Some(secret),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, RuntimeError::SecretDetected(_)));
    assert_eq!(rt.get_entity(&tok, into.id).await.unwrap().id, into.id);
    assert_eq!(rt.get_entity(&tok, from.id).await.unwrap().id, from.id);
    let event_count = rt
        .events(&tok)
        .unwrap()
        .count_events(khive_storage::EventFilter {
            kinds: vec![EventKind::EntityMerged],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(event_count, 0);
}

/// ADR-014: `merge_note` must be as auditable as `merge_entity` — exactly one
/// `NoteMerged` event, carrying kept/absorbed ids, per note merge.
#[tokio::test]
async fn merge_note_emits_exactly_one_note_merged_event_with_kept_and_absorbed_ids() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_note(&tok, "observation", None, "into note", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_note(&tok, "observation", None, "from note", None, None, vec![])
        .await
        .unwrap();

    let summary = rt
        .merge_note_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            Some("duplicate".to_string()),
        )
        .await
        .unwrap();

    let events = rt
        .events(&tok)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![EventKind::NoteMerged],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        events.items.len(),
        1,
        "merge_note must emit exactly one NoteMerged event"
    );

    let payload = &events.items[0].payload;
    assert_eq!(
        payload.get("into_id").and_then(|v| v.as_str()),
        Some(summary.kept_id.to_string()).as_deref()
    );
    assert_eq!(
        payload.get("from_id").and_then(|v| v.as_str()),
        Some(summary.removed_id.to_string()).as_deref()
    );
    assert_eq!(
        payload.get("reason").and_then(|v| v.as_str()),
        Some("duplicate")
    );
}

#[tokio::test]
async fn merge_note_returns_committed_summary_when_post_commit_reindex_fails() {
    use crate::operations::arm_fts_fail_scoped;

    const DIMS: usize = 4;
    let rt = rt();
    rt.register_embedder(MergeTestVecProvider::new(
        "merge-note-reindex-failure",
        DIMS,
    ));
    let namespace = format!("merge-reindex-failure-{}", Uuid::new_v4().as_simple());
    let tok = NamespaceToken::for_namespace(crate::Namespace::parse(&namespace).unwrap());
    let into = rt
        .create_note(
            &tok,
            "observation",
            None,
            "survivor content",
            None,
            Some(serde_json::json!({"survivor": "retained"})),
            vec![],
        )
        .await
        .expect("create survivor note");
    let from = rt
        .create_note(
            &tok,
            "observation",
            None,
            "source content",
            None,
            Some(serde_json::json!({"merged": "source"})),
            vec![],
        )
        .await
        .expect("create source note");

    let observed_by_hook = Arc::new(Mutex::new(Vec::new()));
    let hook_observations = Arc::clone(&observed_by_hook);
    let event_store = rt.events(&tok).expect("event store");
    rt.install_note_mutation_hook(Arc::new(move |kind, id| {
        let hook_observations = Arc::clone(&hook_observations);
        let event_store = Arc::clone(&event_store);
        Box::pin(async move {
            let events = event_store
                .query_events(
                    khive_storage::EventFilter {
                        kinds: vec![EventKind::NoteMerged],
                        ..Default::default()
                    },
                    khive_storage::types::PageRequest {
                        offset: 0,
                        limit: 10,
                    },
                )
                .await
                .expect("read merge event from mutation hook");
            hook_observations
                .lock()
                .unwrap()
                .push((kind, id, !events.items.is_empty()));
        })
    }));

    let _arm = arm_fts_fail_scoped(&namespace);
    let outcome = rt
        .merge_note(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::Union,
            ContentMergeStrategy::Append,
            false,
        )
        .await;
    assert!(
        outcome.is_ok(),
        "a committed merge must return its summary after reindexing fails: {outcome:?}"
    );
    let summary = outcome.expect("the committed merge summary must be returned");
    assert_eq!(summary.kept_id, into.id);
    assert_eq!(summary.removed_id, from.id);
    assert!(
        summary
            .post_commit_reindex_error
            .as_deref()
            .is_some_and(|error| error.contains("injected FTS failure")),
        "the summary must report the post-commit reindex failure: {:?}",
        summary.post_commit_reindex_error
    );

    assert_eq!(
        observed_by_hook.lock().unwrap().as_slice(),
        &[("observation".to_string(), into.id, true)],
        "the note mutation hook must observe the committed merge event"
    );

    let note_store = rt.notes(&tok).expect("note store");
    let survivor = note_store
        .get_note(into.id)
        .await
        .expect("read survivor")
        .expect("survivor remains live");
    assert_eq!(
        survivor.content,
        "survivor content\n\n---\n\nsource content"
    );
    let properties = survivor.properties.expect("merged properties");
    assert_eq!(properties["survivor"], "retained");
    assert_eq!(properties["merged"], "source");

    let removed = note_store
        .get_note_including_deleted(from.id)
        .await
        .expect("read merge tombstone")
        .expect("source row is retained as a tombstone");
    assert_eq!(removed.status, "deleted");
    assert!(removed.deleted_at.is_some());

    let events = rt
        .events(&tok)
        .expect("event store")
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![EventKind::NoteMerged],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .expect("read merge event");
    assert_eq!(
        events.items.len(),
        1,
        "the merge event must be recorded when reindexing fails"
    );
    assert_eq!(
        events.items[0]
            .payload
            .get("into_id")
            .and_then(|v| v.as_str()),
        Some(summary.kept_id.to_string()).as_deref()
    );
    assert_eq!(
        events.items[0]
            .payload
            .get("from_id")
            .and_then(|v| v.as_str()),
        Some(summary.removed_id.to_string()).as_deref()
    );
}

#[tokio::test]
async fn merge_note_fires_mutation_hook_without_embedding_models() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_note(&tok, "observation", None, "survivor", None, None, vec![])
        .await
        .expect("create survivor note");
    let from = rt
        .create_note(&tok, "observation", None, "source", None, None, vec![])
        .await
        .expect("create source note");
    let hook_calls = Arc::new(Mutex::new(Vec::new()));
    let observed_calls = Arc::clone(&hook_calls);
    rt.install_note_mutation_hook(Arc::new(move |kind, id| {
        let observed_calls = Arc::clone(&observed_calls);
        Box::pin(async move { observed_calls.lock().unwrap().push((kind, id)) })
    }));

    rt.merge_note(
        &tok,
        into.id,
        from.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
    )
    .await
    .expect("merge note without registered embedding models");

    assert_eq!(
        hook_calls.lock().unwrap().as_slice(),
        &[("observation".to_string(), into.id)],
        "every committed note merge must notify mutation hooks without embedding models"
    );
}

#[tokio::test]
async fn merge_note_with_reason_preserves_an_explicit_empty_reason() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_note(&tok, "observation", None, "into note", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_note(&tok, "observation", None, "from note", None, None, vec![])
        .await
        .unwrap();

    rt.merge_note_with_reason(
        &tok,
        into.id,
        from.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
        Some(String::new()),
    )
    .await
    .unwrap();

    let events = rt
        .events(&tok)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![EventKind::NoteMerged],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(events.items.len(), 1);
    assert_eq!(
        events.items[0].payload.get("reason"),
        Some(&Value::String(String::new()))
    );
}

#[tokio::test]
async fn merge_note_with_reason_rejects_secrets_before_reads_or_writes() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_note(&tok, "observation", None, "into note", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_note(&tok, "observation", None, "from note", None, None, vec![])
        .await
        .unwrap();
    let secret = secret_shaped_reason();

    let error = rt
        .merge_note_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            Some(secret),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, RuntimeError::SecretDetected(_)));
    let note_store = rt.notes(&tok).unwrap();
    assert_eq!(
        note_store.get_note(into.id).await.unwrap().unwrap().id,
        into.id
    );
    assert_eq!(
        note_store.get_note(from.id).await.unwrap().unwrap().id,
        from.id
    );
    let event_count = rt
        .events(&tok)
        .unwrap()
        .count_events(khive_storage::EventFilter {
            kinds: vec![EventKind::NoteMerged],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(event_count, 0);
}

#[tokio::test]
async fn legacy_merge_methods_remain_source_compatible() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into_entity = rt
        .create_entity(&tok, "concept", None, "Entity A", None, None, vec![])
        .await
        .unwrap();
    let from_entity = rt
        .create_entity(&tok, "concept", None, "Entity B", None, None, vec![])
        .await
        .unwrap();
    let into_note = rt
        .create_note(&tok, "observation", None, "note A", None, None, vec![])
        .await
        .unwrap();
    let from_note = rt
        .create_note(&tok, "observation", None, "note B", None, None, vec![])
        .await
        .unwrap();

    rt.merge_entity(
        &tok,
        into_entity.id,
        from_entity.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
    )
    .await
    .unwrap();
    rt.merge_note(
        &tok,
        into_note.id,
        from_note.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
    )
    .await
    .unwrap();
}

// ---- interim merged_into miss-hint (data-integrity, precedes ADR-113 chase) ----

#[tokio::test]
async fn get_entity_after_merge_discloses_kept_id() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Kept", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "Absorbed", None, None, vec![])
        .await
        .unwrap();

    rt.merge_entity(
        &tok,
        into.id,
        from.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
    )
    .await
    .unwrap();

    let err = rt.get_entity(&tok, from.id).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("was merged into") && msg.contains(&into.id.to_string()),
        "expected a merged_into disclosure naming {}, got {msg:?}",
        into.id
    );
}

/// A row an earlier restore left live over its merge (deleted_at cleared,
/// merged_into kept) is an invariant violation, not a state restore may
/// report as "already live". Restore names it and writes nothing.
#[tokio::test]
async fn restore_names_a_live_row_that_still_carries_merged_into() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Kept", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "Absorbed", None, None, vec![])
        .await
        .unwrap();
    rt.merge_entity(
        &tok,
        into.id,
        from.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
    )
    .await
    .unwrap();
    // Reproduce what the pre-guard restore wrote: the tombstone cleared,
    // the merge provenance left in place.
    let mut writer = rt.sql().writer().await.expect("sql writer");
    let cleared = writer
        .execute(khive_storage::SqlStatement {
            sql: "UPDATE entities SET version = version + 1, deleted_at = NULL \
                      WHERE id = ?1 AND merged_into IS NOT NULL"
                .to_string(),
            params: vec![SqlValue::Text(from.id.to_string())],
            label: None,
        })
        .await
        .expect("seed the pre-guard state");
    assert_eq!(
        cleared, 1,
        "control: the seed must have found the merge tombstone"
    );
    drop(writer);

    let err = rt.restore_entity(&tok, from.id).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("live_merged_entity") && msg.contains(&into.id.to_string()),
        "restore of a live merged row must be named, not reported already live, got {msg:?}"
    );

    // Nothing was written: the row is still live and still carries the merge.
    let row = rt
        .get_entity_including_deleted(&tok, from.id)
        .await
        .unwrap()
        .expect("row exists");
    assert!(row.deleted_at.is_none());
    assert_eq!(row.merged_into, Some(into.id));
}

#[tokio::test]
async fn restore_refuses_a_merge_tombstone_and_keeps_the_disclosure() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Kept", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "Absorbed", None, None, vec![])
        .await
        .unwrap();
    rt.merge_entity(
        &tok,
        into.id,
        from.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
    )
    .await
    .unwrap();

    let err = rt.restore_entity(&tok, from.id).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("merge_tombstone") && msg.contains(&into.id.to_string()),
        "restore of a merge tombstone must be refused naming the kept id, got {msg:?}"
    );

    // The refusal wrote nothing: the source is still a merge tombstone.
    let err = rt.get_entity(&tok, from.id).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("was merged into") && msg.contains(&into.id.to_string()),
        "after a refused restore the merged_into disclosure must survive, got {msg:?}"
    );
    let tombstone = rt
        .get_entity_including_deleted(&tok, from.id)
        .await
        .unwrap()
        .expect("tombstone row still present");
    assert!(tombstone.deleted_at.is_some());
    assert_eq!(tombstone.merged_into, Some(into.id));
}

#[tokio::test]
async fn merge_tombstone_carries_the_id_of_its_merge_event() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Kept", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "Absorbed", None, None, vec![])
        .await
        .unwrap();
    rt.merge_entity(
        &tok,
        into.id,
        from.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
    )
    .await
    .unwrap();

    let events = rt
        .events(&tok)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![EventKind::EntityMerged],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    let [event] = events.items.as_slice() else {
        panic!(
            "expected one EntityMerged event, got {}",
            events.items.len()
        );
    };
    assert_eq!(event.payload["from_id"], serde_json::json!(from.id));

    let tombstone = rt
        .get_entity_including_deleted(&tok, from.id)
        .await
        .unwrap()
        .expect("tombstone row still present");
    assert_eq!(
        tombstone.merge_event_id,
        Some(event.id),
        "the tombstone must name the event that recorded its merge"
    );
    let kept = rt.get_entity(&tok, into.id).await.unwrap();
    assert_eq!(kept.merge_event_id, None);
}

#[tokio::test]
async fn get_entity_on_plain_soft_delete_stays_bare_not_found() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(&tok, "concept", None, "Deleted", None, None, vec![])
        .await
        .unwrap();
    assert!(rt.delete_entity(&tok, entity.id, false).await.unwrap());

    let err = rt.get_entity(&tok, entity.id).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        !msg.contains("merged into"),
        "plain soft-delete must not gain a merge hint, got {msg:?}"
    );
    assert_eq!(msg, format!("not found: entity {}", entity.id));
}

#[tokio::test]
async fn get_entity_on_absent_id_stays_bare_not_found() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let absent = Uuid::new_v4();

    let err = rt.get_entity(&tok, absent).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        !msg.contains("merged into"),
        "a never-existed id must not gain a merge hint, got {msg:?}"
    );
    assert_eq!(msg, format!("not found: entity {absent}"));
}

// ---- merge helper unit tests ----

#[test]
fn union_tags_deduplicates() {
    let (tags, added) = union_tags(
        &["x".to_string(), "y".to_string()],
        &["y".to_string(), "z".to_string()],
    );
    let mut sorted = tags.clone();
    sorted.sort();
    assert_eq!(sorted, vec!["x", "y", "z"]);
    assert_eq!(added, 1);
}

#[test]
fn merge_properties_prefer_into_fills_missing_keys() {
    let a = serde_json::json!({"a": 1});
    let b = serde_json::json!({"a": 99, "b": 2});
    let (merged, added) = merge_properties(&Some(a), &Some(b), EntityDedupMergePolicy::PreferInto);
    let m = merged.unwrap();
    assert_eq!(m["a"], 1);
    assert_eq!(m["b"], 2);
    assert_eq!(added, 1);
}

// ---- tombstone and note merge tests ----

#[tokio::test]
async fn merge_entity_tombstones_source_with_provenance() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", None, None, vec![])
        .await
        .unwrap();
    let from_id = from.id;

    rt.merge_entity_with_reason(
        &tok,
        into.id,
        from_id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
        None,
    )
    .await
    .unwrap();

    assert!(
        rt.get_entity(&tok, from_id).await.is_err(),
        "tombstoned source should not be returned by get_entity"
    );

    let pool = rt.backend().pool_arc();
    let (deleted_at, merged_into): (Option<i64>, Option<String>) =
        tokio::task::spawn_blocking(move || {
            let guard = pool.writer().unwrap();
            guard
                .conn()
                .query_row(
                    "SELECT deleted_at, merged_into FROM entities WHERE id = ?1",
                    [from_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap()
        })
        .await
        .unwrap();
    assert!(
        deleted_at.is_some(),
        "tombstoned entity must have deleted_at set"
    );
    assert_eq!(
        merged_into.as_deref(),
        Some(into.id.to_string().as_str()),
        "merged_into must point to into_id"
    );
}

#[tokio::test]
async fn generic_update_and_merge_reject_schedule_managed_notes() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let schedule_a = rt
        .create_note(
            &tok,
            "scheduled_event",
            None,
            "stats()",
            None,
            Some(serde_json::json!({
                "event_type": "schedule",
                "payload": "stats()",
                "status": "pending",
                "trigger_at": "2099-01-01T00:00:00Z"
            })),
            vec![],
        )
        .await
        .unwrap();
    let schedule_b = rt
        .create_note(
            &tok,
            "scheduled_event",
            None,
            "stats()",
            None,
            Some(serde_json::json!({
                "event_type": "schedule",
                "payload": "stats()",
                "status": "pending",
                "trigger_at": "2099-01-02T00:00:00Z"
            })),
            vec![],
        )
        .await
        .unwrap();

    let update_error = rt
        .update_note(
            &tok,
            schedule_a.id,
            NotePatch::new(
                None,
                None,
                None,
                None,
                Some(serde_json::json!({ "payload": "delete(id=\"victim\")" })),
            ),
        )
        .await
        .expect_err("schedule-managed note update must fail");
    assert!(
        update_error.to_string().contains("schedule-managed"),
        "{update_error}"
    );

    for (into_id, from_id) in [
        (schedule_a.id, schedule_b.id),
        (schedule_b.id, schedule_a.id),
    ] {
        let merge_error = rt
            .merge_note(
                &tok,
                into_id,
                from_id,
                EntityDedupMergePolicy::PreferFrom,
                ContentMergeStrategy::PreferFrom,
                false,
            )
            .await
            .expect_err("either schedule-managed merge operand must fail");
        assert!(
            merge_error.to_string().contains("schedule-managed"),
            "{merge_error}"
        );
    }

    let store = rt.notes(&tok).unwrap();
    for (id, trigger_at) in [
        (schedule_a.id, "2099-01-01T00:00:00Z"),
        (schedule_b.id, "2099-01-02T00:00:00Z"),
    ] {
        let note = store
            .get_note(id)
            .await
            .unwrap()
            .expect("rejected generic mutation leaves the schedule intact");
        assert_eq!(note.properties.as_ref().unwrap()["trigger_at"], trigger_at);
    }
}

#[tokio::test]
async fn merge_note_refuses_quarantined_message_in_either_role() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let capability = crate::pack::ChannelIngestCapability { _sealed: () };
    let ordinary = rt
        .create_note(
            &tok,
            "message",
            None,
            "ordinary message",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let quarantined = rt
        .try_create_note_as_trusted_ingest(
            &capability,
            &tok,
            "message",
            None,
            "quarantined transport content",
            Some(serde_json::json!({"quarantined": true})),
            None,
        )
        .await
        .unwrap()
        .expect("quarantined insert");

    for (into_id, from_id) in [(ordinary.id, quarantined.id), (quarantined.id, ordinary.id)] {
        let error = rt
            .merge_note(
                &tok,
                into_id,
                from_id,
                EntityDedupMergePolicy::PreferFrom,
                ContentMergeStrategy::Append,
                false,
            )
            .await
            .expect_err("a quarantined message must not merge in either role");
        assert!(error.to_string().contains("quarantined"), "{error}");
    }

    // Neither operand was mutated by the refused merges.
    let store = rt.notes(&tok).unwrap();
    let kept = store
        .get_note(quarantined.id)
        .await
        .unwrap()
        .expect("quarantined note intact");
    assert_eq!(
        kept.properties.as_ref().unwrap()["quarantined"],
        serde_json::json!(true)
    );
    assert_eq!(kept.content, "quarantined transport content");
}

#[tokio::test]
async fn merge_note_refuses_string_encoded_quarantine_marker() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let capability = crate::pack::ChannelIngestCapability { _sealed: () };
    let ordinary = rt
        .create_note(
            &tok,
            "message",
            None,
            "ordinary message",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    // Some channel adapters record the marker as the string "true".
    let quarantined = rt
        .try_create_note_as_trusted_ingest(
            &capability,
            &tok,
            "message",
            None,
            "string-marked quarantined content",
            Some(serde_json::json!({"quarantined": "true"})),
            None,
        )
        .await
        .unwrap()
        .expect("quarantined insert");

    let error = rt
        .merge_note(
            &tok,
            ordinary.id,
            quarantined.id,
            EntityDedupMergePolicy::PreferFrom,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .expect_err("string-encoded quarantine marker must also refuse the merge");
    assert!(error.to_string().contains("quarantined"), "{error}");
}

#[tokio::test]
async fn merge_note_still_merges_unquarantined_messages() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_note(&tok, "message", None, "into message", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_note(&tok, "message", None, "from message", None, None, vec![])
        .await
        .unwrap();
    rt.merge_note(
        &tok,
        into.id,
        from.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
    )
    .await
    .expect("ordinary message merge must still work");
}

#[tokio::test]
async fn merge_note_same_kind_appends_content() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_note(
            &tok,
            "observation",
            None,
            "Into content",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let from = rt
        .create_note(
            &tok,
            "observation",
            None,
            "From content",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let from_id = from.id;

    let summary = rt
        .merge_note_with_reason(
            &tok,
            into.id,
            from_id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .unwrap();

    assert_eq!(summary.kept_id, into.id);
    assert_eq!(summary.removed_id, from_id);
    assert!(summary.content_appended);
    assert!(!summary.dry_run);

    let from_store = rt.notes(&tok).unwrap();
    assert!(
        from_store.get_note(from_id).await.unwrap().is_none(),
        "merged-from note should be soft-deleted"
    );
}

#[tokio::test]
async fn merge_note_preserves_the_kept_memory_key() {
    use crate::keyed_memory::{create_keyed_memory, KeyedMemorySpec};

    let rt = rt();
    let tok = NamespaceToken::local();
    let (into, _, _) = create_keyed_memory(
        &rt,
        &tok,
        KeyedMemorySpec {
            content: "Into keyed memory",
            key: "kept-memory-key",
            salience: 0.7,
            decay_factor: 0.0,
            properties: serde_json::json!({}),
            source_id: None,
            embedding_model: None,
        },
    )
    .await
    .unwrap();
    let from = rt
        .create_note(&tok, "memory", None, "From memory", None, None, vec![])
        .await
        .unwrap();

    let summary = rt
        .merge_note(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .expect("merge binds every stored note field");
    assert_eq!(summary.kept_id, into.id);
    let stored = rt
        .notes(&tok)
        .unwrap()
        .get_note(into.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.key.as_deref(), Some("kept-memory-key"));
    assert!(stored.content.contains("Into keyed memory"));
    assert!(stored.content.contains("From memory"));
    assert!(rt
        .notes(&tok)
        .unwrap()
        .get_note(from.id)
        .await
        .unwrap()
        .is_none());
}

// Note merge must absorb a conflicting edge natural key exactly like entity
// merge does, since both route through the shared EDGE_SYMMETRIC_*_SQL arms.
#[tokio::test]
async fn merge_note_survives_shared_edge_to_third_party() {
    use khive_storage::EdgeRelation;
    let rt = rt();
    let tok = NamespaceToken::local();

    let into = rt
        .create_note(&tok, "observation", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_note(&tok, "observation", None, "From", None, None, vec![])
        .await
        .unwrap();
    let shared = rt
        .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
        .await
        .unwrap();

    // Both into and from annotate the same shared entity — rewiring from's
    // edge onto into during merge produces a duplicate (into, shared,
    // annotates) triple, exercising the conflict-probe/delete arms.
    rt.link(&tok, into.id, shared.id, EdgeRelation::Annotates, 1.0, None)
        .await
        .unwrap();
    rt.link(&tok, from.id, shared.id, EdgeRelation::Annotates, 1.0, None)
        .await
        .unwrap();

    let summary = rt
        .merge_note_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .expect("merge must succeed even when both notes annotate the same entity");

    assert_eq!(summary.kept_id, into.id);
    assert_eq!(summary.removed_id, from.id);

    let into_edges = rt
        .list_edges(
            &tok,
            crate::EdgeListFilter {
                source_id: Some(into.id),
                target_id: Some(shared.id),
                relations: vec![EdgeRelation::Annotates],
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(
        into_edges.len(),
        1,
        "exactly one live into→shared annotates edge must exist after merge; got: {into_edges:?}"
    );
}

#[tokio::test]
async fn merge_note_conflict_records_dropped_edge_and_cascades_annotation() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_note(&tok, "observation", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_note(&tok, "observation", None, "From", None, None, vec![])
        .await
        .unwrap();
    let annotator = rt
        .create_note(
            &tok,
            "observation",
            None,
            "edge annotation",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let shared = rt
        .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
        .await
        .unwrap();

    let survivor = rt
        .link(
            &tok,
            into.id,
            shared.id,
            EdgeRelation::Annotates,
            1.0,
            Some(serde_json::json!({"source": "survivor"})),
        )
        .await
        .unwrap();
    let dropped = rt
        .link(
            &tok,
            from.id,
            shared.id,
            EdgeRelation::Annotates,
            0.4,
            Some(serde_json::json!({"source": "dropped"})),
        )
        .await
        .unwrap();
    let annotation = rt
        .link(
            &tok,
            annotator.id,
            dropped.id.into(),
            EdgeRelation::Annotates,
            0.8,
            Some(serde_json::json!({"why": "duplicate claim"})),
        )
        .await
        .unwrap();
    rt.delete_edge(&tok, annotation.id.into(), false)
        .await
        .unwrap();

    let summary = rt
        .merge_note(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();

    let [conflict] = summary.edge_conflict_preimages.as_slice() else {
        panic!(
            "expected one note-merge edge conflict, got {:?}",
            summary.edge_conflict_preimages
        );
    };
    assert_eq!(conflict.surviving_edge_id, Uuid::from(survivor.id));
    assert_eq!(conflict.dropped_edge.id, Uuid::from(dropped.id));
    assert_eq!(conflict.dropped_edge.source_id, from.id);
    assert_eq!(conflict.dropped_edge.weight, 0.4);
    assert_eq!(
        conflict.dropped_edge.metadata,
        Some(serde_json::json!({"source": "dropped"}))
    );
    assert_eq!(conflict.incident_edge_preimages.len(), 1);
    assert_eq!(
        conflict.incident_edge_preimages[0].id,
        Uuid::from(annotation.id)
    );
    assert_eq!(
        conflict.incident_edge_preimages[0].metadata,
        Some(serde_json::json!({"why": "duplicate claim"}))
    );
    assert!(
        conflict.incident_edge_preimages[0].deleted_at.is_some(),
        "the cascade preimage must retain an annotation's tombstone state"
    );
    assert!(rt
        .get_edge_including_deleted(&tok, dropped.id.into())
        .await
        .unwrap()
        .is_none());
    assert!(
        rt.get_edge_including_deleted(&tok, annotation.id.into())
            .await
            .unwrap()
            .is_none(),
        "annotation targeting the dropped edge must be cascaded, not left dangling"
    );

    let events = rt
        .events(&tok)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![EventKind::NoteMerged],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(events.items.len(), 1);
    assert_eq!(
        events.items[0].payload["edge_conflict_preimages"],
        serde_json::to_value(&summary.edge_conflict_preimages).unwrap()
    );
}

// A dry run must predict the same conflict preimages a committing note
// merge would produce, without deleting or mutating a single row. The
// incident cascade is two levels deep (an annotation on the dropped
// edge, and a nested annotation on that annotation) so the root-to-leaf
// ordering ADR-014 promises is actually exercised, not just a
// one-element vec that trivially satisfies any order. Every row touched
// by the merge — both notes and every edge — is snapshotted before the
// dry run and compared field-for-field against its post-run state.
#[tokio::test]
async fn merge_note_dry_run_conflict_returns_preimages_without_mutating() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_note(&tok, "observation", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_note(&tok, "observation", None, "From", None, None, vec![])
        .await
        .unwrap();
    let annotator = rt
        .create_note(
            &tok,
            "observation",
            None,
            "edge annotation",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let nested_annotator = rt
        .create_note(
            &tok,
            "observation",
            None,
            "nested edge annotation",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let shared = rt
        .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
        .await
        .unwrap();

    let survivor = rt
        .link(
            &tok,
            into.id,
            shared.id,
            EdgeRelation::Annotates,
            1.0,
            Some(serde_json::json!({"source": "survivor"})),
        )
        .await
        .unwrap();
    let dropped = rt
        .link(
            &tok,
            from.id,
            shared.id,
            EdgeRelation::Annotates,
            0.4,
            Some(serde_json::json!({"source": "dropped"})),
        )
        .await
        .unwrap();
    let annotation = rt
        .link(
            &tok,
            annotator.id,
            dropped.id.into(),
            EdgeRelation::Annotates,
            0.8,
            Some(serde_json::json!({"why": "duplicate claim"})),
        )
        .await
        .unwrap();
    let nested_annotation = rt
        .link(
            &tok,
            nested_annotator.id,
            annotation.id.into(),
            EdgeRelation::Annotates,
            0.6,
            Some(serde_json::json!({"why": "nested duplicate claim"})),
        )
        .await
        .unwrap();
    rt.delete_edge(&tok, nested_annotation.id.into(), false)
        .await
        .unwrap();

    let survivor_before = rt
        .get_edge_including_deleted(&tok, survivor.id.into())
        .await
        .unwrap()
        .expect("survivor edge exists");
    let dropped_before = rt
        .get_edge_including_deleted(&tok, dropped.id.into())
        .await
        .unwrap()
        .expect("dropped edge exists");
    let annotation_before = rt
        .get_edge_including_deleted(&tok, annotation.id.into())
        .await
        .unwrap()
        .expect("annotation edge exists");
    let nested_annotation_before = rt
        .get_edge_including_deleted(&tok, nested_annotation.id.into())
        .await
        .unwrap()
        .expect("nested annotation edge exists");
    let into_before = rt
        .get_note_including_deleted(&tok, into.id)
        .await
        .unwrap()
        .expect("into note exists");
    let from_before = rt
        .get_note_including_deleted(&tok, from.id)
        .await
        .unwrap()
        .expect("from note exists");

    let summary = rt
        .merge_note(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            true,
        )
        .await
        .unwrap();

    let [conflict] = summary.edge_conflict_preimages.as_slice() else {
        panic!(
            "expected one note-merge edge conflict from the dry run, got {:?}",
            summary.edge_conflict_preimages
        );
    };
    assert_eq!(conflict.surviving_edge_id, Uuid::from(survivor.id));
    assert_eq!(conflict.dropped_edge.id, Uuid::from(dropped.id));
    assert_eq!(conflict.dropped_edge.source_id, from.id);
    assert_eq!(conflict.dropped_edge.weight, 0.4);
    // Root-to-leaf order (ADR-014): the direct annotation on the dropped
    // edge must precede the annotation nested on top of it.
    assert_eq!(conflict.incident_edge_preimages.len(), 2);
    assert_eq!(
        conflict.incident_edge_preimages[0].id,
        Uuid::from(annotation.id)
    );
    assert!(
        conflict.incident_edge_preimages[0].deleted_at.is_none(),
        "the direct annotation was never soft-deleted"
    );
    assert_eq!(
        conflict.incident_edge_preimages[1].id,
        Uuid::from(nested_annotation.id)
    );
    assert!(
        conflict.incident_edge_preimages[1].deleted_at.is_some(),
        "dry-run preimage must retain the nested annotation's tombstone state"
    );

    let survivor_after = rt
        .get_edge_including_deleted(&tok, survivor.id.into())
        .await
        .unwrap()
        .expect("dry run must not delete the survivor edge");
    let dropped_after = rt
        .get_edge_including_deleted(&tok, dropped.id.into())
        .await
        .unwrap()
        .expect("dry run must not delete the dropped edge");
    let annotation_after = rt
        .get_edge_including_deleted(&tok, annotation.id.into())
        .await
        .unwrap()
        .expect("dry run must not delete the cascaded annotation");
    let nested_annotation_after = rt
        .get_edge_including_deleted(&tok, nested_annotation.id.into())
        .await
        .unwrap()
        .expect("dry run must not delete the nested cascaded annotation");
    assert_eq!(
        serde_json::to_value(&survivor_before).unwrap(),
        serde_json::to_value(&survivor_after).unwrap(),
        "dry run must not mutate the surviving edge's row at all"
    );
    assert_eq!(
        serde_json::to_value(&dropped_before).unwrap(),
        serde_json::to_value(&dropped_after).unwrap(),
        "dry run must not mutate the would-be-dropped edge's row at all"
    );
    assert_eq!(
        serde_json::to_value(&annotation_before).unwrap(),
        serde_json::to_value(&annotation_after).unwrap(),
        "dry run must not mutate the incident annotation's row at all"
    );
    assert_eq!(
        serde_json::to_value(&nested_annotation_before).unwrap(),
        serde_json::to_value(&nested_annotation_after).unwrap(),
        "dry run must not mutate the nested incident annotation's row at all"
    );

    let into_after = rt
        .get_note_including_deleted(&tok, into.id)
        .await
        .unwrap()
        .expect("into note must remain unmerged after a dry run");
    let from_after = rt
        .get_note_including_deleted(&tok, from.id)
        .await
        .unwrap()
        .expect("from note must not be deleted by a dry run");
    assert_eq!(
        serde_json::to_value(&into_before).unwrap(),
        serde_json::to_value(&into_after).unwrap(),
        "dry run must not mutate the into note's row at all"
    );
    assert_eq!(
        serde_json::to_value(&from_before).unwrap(),
        serde_json::to_value(&from_after).unwrap(),
        "dry run must not mutate the from note's row at all"
    );
    assert_eq!(from_after.status, from_before.status);
    assert_eq!(from_after.deleted_at, None);

    let events = rt
        .events(&tok)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![EventKind::NoteMerged],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert!(
        events.items.is_empty(),
        "a dry run must not record a merge audit event"
    );
}

// The rewire contract check must preserve note→note supersedes, supports,
// and refutes — `validate_edge_relation_endpoints` permits any note→note
// pair for these relations, so the merge matcher must too, or a note merge
// deletes valid epistemic/supersession edges.
#[tokio::test]
async fn merge_note_preserves_note_to_note_epistemic_and_supersession_edges() {
    use khive_storage::EdgeRelation;
    let rt = rt();
    let tok = NamespaceToken::local();

    let into = rt
        .create_note(&tok, "observation", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_note(&tok, "observation", None, "From", None, None, vec![])
        .await
        .unwrap();
    let superseded = rt
        .create_note(&tok, "observation", None, "Old", None, None, vec![])
        .await
        .unwrap();
    let claim = rt
        .create_note(&tok, "insight", None, "Claim", None, None, vec![])
        .await
        .unwrap();
    let counter = rt
        .create_note(&tok, "observation", None, "Counter", None, None, vec![])
        .await
        .unwrap();

    // Outgoing from `from` (source rewires) and incoming onto `from`
    // (target rewires) — both directions must survive.
    rt.link(
        &tok,
        from.id,
        superseded.id,
        EdgeRelation::Supersedes,
        1.0,
        None,
    )
    .await
    .unwrap();
    rt.link(&tok, from.id, claim.id, EdgeRelation::Supports, 1.0, None)
        .await
        .unwrap();
    rt.link(&tok, counter.id, from.id, EdgeRelation::Refutes, 1.0, None)
        .await
        .unwrap();

    let summary = rt
        .merge_note_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        summary.edges_rewired, 3,
        "all three note→note edges must be rewired, not contract-dropped"
    );
    assert_eq!(
        summary.edges_contract_skipped, 0,
        "no valid note→note supersedes/supports/refutes edge may be dropped"
    );

    for (src, tgt, rel) in [
        (into.id, superseded.id, EdgeRelation::Supersedes),
        (into.id, claim.id, EdgeRelation::Supports),
        (counter.id, into.id, EdgeRelation::Refutes),
    ] {
        let edges = rt
            .list_edges(
                &tok,
                crate::EdgeListFilter {
                    source_id: Some(src),
                    target_id: Some(tgt),
                    relations: vec![rel],
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(
            edges.len(),
            1,
            "rewired {rel:?} edge {src}→{tgt} must survive the merge; got {edges:?}"
        );
    }
}

/// The note path must leave its source and self-loop edge intact when the
/// only durable preimage copy cannot be inserted into the event store.
#[tokio::test]
async fn note_merge_event_insert_failure_rolls_back_destructive_merge() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_note(&tok, "observation", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_note(&tok, "observation", None, "From", None, None, vec![])
        .await
        .unwrap();
    let edge = rt
        .link(&tok, into.id, from.id, EdgeRelation::Refutes, 1.0, None)
        .await
        .unwrap();
    let event_store = rt.events(&tok).unwrap();
    set_merge_event_refusal(&rt, "note_merged", true);

    let failed = rt
        .merge_note(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await;
    assert!(failed.is_err(), "event insert must abort the note merge");
    let source = rt
        .notes(&tok)
        .unwrap()
        .get_note(from.id)
        .await
        .unwrap()
        .unwrap();
    assert!(source.deleted_at.is_none());
    assert!(
        rt.get_edge_including_deleted(&tok, edge.id.into())
            .await
            .unwrap()
            .is_some(),
        "the deleted self-loop must roll back"
    );
    let filter = khive_storage::EventFilter {
        kinds: vec![EventKind::NoteMerged],
        ..Default::default()
    };
    let page = khive_storage::types::PageRequest {
        offset: 0,
        limit: 10,
    };
    assert!(event_store
        .query_events(filter.clone(), page.clone())
        .await
        .unwrap()
        .items
        .is_empty());

    set_merge_event_refusal(&rt, "note_merged", false);
    let summary = rt
        .merge_note(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();
    let tombstone = rt
        .notes(&tok)
        .unwrap()
        .get_note_including_deleted(from.id)
        .await
        .unwrap()
        .unwrap();
    assert!(tombstone.deleted_at.is_some());
    let events = event_store.query_events(filter, page).await.unwrap();
    assert_eq!(events.items.len(), 1);
    assert_eq!(
        events.items[0].payload["self_loop_edge_preimages"],
        serde_json::to_value(&summary.self_loop_edge_preimages).unwrap()
    );
}

// The note-merge mirror of `merge_entity_drops_self_loops`. `into`
// refutes `from` directly — merging `from` into `into` collapses this
// into an into-refutes-into self-loop, which must be dropped and its
// preimage captured and audited (khive#2934); before this fix a
// refutation between the two merge operands vanished with no counter,
// no preimage, and no audit trail.
#[tokio::test]
async fn merge_note_drops_self_loop_edge_records_preimage() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_note(&tok, "observation", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_note(&tok, "observation", None, "From", None, None, vec![])
        .await
        .unwrap();

    let edge = rt
        .link(
            &tok,
            into.id,
            from.id,
            EdgeRelation::Refutes,
            0.85,
            Some(serde_json::json!({"basis": "direct contradiction"})),
        )
        .await
        .unwrap();

    let summary = rt
        .merge_note(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();

    assert_eq!(
        summary.edges_self_loop_dropped, 1,
        "the into-refutes-from edge becomes a self-loop and must be counted"
    );
    let [preimage] = summary.self_loop_edge_preimages.as_slice() else {
        panic!(
            "expected exactly one self-loop preimage, got {:?}",
            summary.self_loop_edge_preimages
        );
    };
    assert_eq!(preimage.id, Uuid::from(edge.id));
    assert_eq!(preimage.source_id, into.id);
    assert_eq!(preimage.target_id, from.id);
    assert_eq!(preimage.relation, "refutes");
    assert_eq!(preimage.weight, 0.85);
    assert_eq!(
        preimage.metadata,
        Some(serde_json::json!({"basis": "direct contradiction"}))
    );

    assert!(
        rt.get_edge_including_deleted(&tok, edge.id.into())
            .await
            .unwrap()
            .is_none(),
        "the self-loop edge must actually be removed"
    );

    let events = rt
        .events(&tok)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![EventKind::NoteMerged],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(events.items.len(), 1);
    assert_eq!(
        events.items[0].payload["edges_self_loop_dropped"],
        serde_json::json!(1)
    );
    assert_eq!(
        events.items[0].payload["self_loop_edge_preimages"],
        serde_json::to_value(&summary.self_loop_edge_preimages).unwrap()
    );
}

// Note-path counterpart of `merge_entity_self_loop_dry_run_matches_real_run`.
#[tokio::test]
async fn merge_note_self_loop_dry_run_matches_real_run() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_note(&tok, "observation", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_note(&tok, "observation", None, "From", None, None, vec![])
        .await
        .unwrap();
    let edge = rt
        .link(
            &tok,
            from.id,
            into.id,
            EdgeRelation::Supports,
            0.5,
            Some(serde_json::json!({"basis": "dry-run parity"})),
        )
        .await
        .unwrap();

    let dry_summary = rt
        .merge_note(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            true,
        )
        .await
        .unwrap();

    assert!(
        rt.get_edge_including_deleted(&tok, edge.id.into())
            .await
            .unwrap()
            .is_some(),
        "dry run must not delete the self-loop edge"
    );

    let real_summary = rt
        .merge_note(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();

    assert_eq!(dry_summary.edges_self_loop_dropped, 1);
    let [dry_preimage] = dry_summary.self_loop_edge_preimages.as_slice() else {
        panic!(
            "expected exactly one predicted self-loop preimage, got {:?}",
            dry_summary.self_loop_edge_preimages
        );
    };
    assert_eq!(dry_preimage.id, Uuid::from(edge.id));
    assert_eq!(dry_preimage.source_id, from.id);
    assert_eq!(dry_preimage.target_id, into.id);
    assert_eq!(dry_preimage.relation, "supports");
    assert_eq!(dry_preimage.weight, 0.5);
    assert_eq!(
        dry_summary.edges_self_loop_dropped, real_summary.edges_self_loop_dropped,
        "a dry run must predict the same self-loop-drop count the committed merge produces"
    );
    assert_eq!(
        dry_summary.self_loop_edge_preimages, real_summary.self_loop_edge_preimages,
        "a dry run must predict the exact preimage the committed merge produces"
    );

    assert!(
        rt.get_edge_including_deleted(&tok, edge.id.into())
            .await
            .unwrap()
            .is_none(),
        "the committed merge must actually delete the self-loop edge"
    );
}

// Control: no edge exists directly between the merge operands, only one
// that survives the rewire — the self-loop counter must stay at zero.
#[tokio::test]
async fn merge_note_no_self_loop_between_operands_reports_zero() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_note(&tok, "observation", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_note(&tok, "observation", None, "From", None, None, vec![])
        .await
        .unwrap();
    let other = rt
        .create_note(&tok, "observation", None, "Other", None, None, vec![])
        .await
        .unwrap();

    rt.link(&tok, from.id, other.id, EdgeRelation::Supersedes, 1.0, None)
        .await
        .unwrap();

    let summary = rt
        .merge_note(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();

    assert_eq!(
        summary.edges_rewired, 1,
        "the non-self-loop edge must still rewire"
    );
    assert_eq!(
        summary.edges_self_loop_dropped, 0,
        "no self-loop exists between the merge operands"
    );
    assert!(summary.self_loop_edge_preimages.is_empty());
}

// Annotates targets may be edges or events — substrates
// `resolve_merge_edge_endpoint` cannot resolve. The contract check must
// exempt annotates BEFORE endpoint resolution, or a note merge deletes
// valid annotates edges pointing at them.
#[tokio::test]
async fn merge_note_preserves_annotates_edges_targeting_edges_and_events() {
    use khive_storage::EdgeRelation;
    let rt = rt();
    let tok = NamespaceToken::local();

    // An edge to annotate.
    let a = rt
        .create_entity(&tok, "concept", None, "A", None, None, vec![])
        .await
        .unwrap();
    let b = rt
        .create_entity(&tok, "concept", None, "B", None, None, vec![])
        .await
        .unwrap();
    let annotated_edge = rt
        .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();

    // An event to annotate: a throwaway note merge emits a NoteMerged
    // event (creation ops don't emit in this harness).
    let scrap_into = rt
        .create_note(&tok, "observation", None, "ScrapInto", None, None, vec![])
        .await
        .unwrap();
    let scrap_from = rt
        .create_note(&tok, "observation", None, "ScrapFrom", None, None, vec![])
        .await
        .unwrap();
    rt.merge_note_with_reason(
        &tok,
        scrap_into.id,
        scrap_from.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
        None,
    )
    .await
    .unwrap();
    let events = rt
        .events(&tok)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![EventKind::NoteMerged],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 1,
            },
        )
        .await
        .unwrap();
    let annotated_event_id = events.items[0].id;

    let into = rt
        .create_note(&tok, "observation", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_note(&tok, "observation", None, "From", None, None, vec![])
        .await
        .unwrap();

    rt.link(
        &tok,
        from.id,
        annotated_edge.id.0,
        EdgeRelation::Annotates,
        1.0,
        None,
    )
    .await
    .unwrap();
    rt.link(
        &tok,
        from.id,
        annotated_event_id,
        EdgeRelation::Annotates,
        1.0,
        None,
    )
    .await
    .unwrap();

    let summary = rt
        .merge_note_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        summary.edges_rewired, 2,
        "annotates edges targeting an edge and an event must be rewired"
    );
    assert_eq!(
        summary.edges_contract_skipped, 0,
        "no valid annotates edge may be dropped as contract-violating"
    );

    for tgt in [annotated_edge.id.0, annotated_event_id] {
        let edges = rt
            .list_edges(
                &tok,
                crate::EdgeListFilter {
                    source_id: Some(into.id),
                    target_id: Some(tgt),
                    relations: vec![EdgeRelation::Annotates],
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(
            edges.len(),
            1,
            "rewired annotates edge onto target {tgt} must survive the merge; got {edges:?}"
        );
    }
}

// A note dry-run must predict edges_rewired like the entity path does,
// and must not touch topology.
#[tokio::test]
async fn merge_note_dry_run_predicts_edges_rewired() {
    use khive_storage::EdgeRelation;
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_note(&tok, "observation", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_note(&tok, "observation", None, "From", None, None, vec![])
        .await
        .unwrap();
    let shared = rt
        .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
        .await
        .unwrap();
    rt.link(&tok, from.id, shared.id, EdgeRelation::Annotates, 1.0, None)
        .await
        .unwrap();

    let summary = rt
        .merge_note_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            true,
            None,
        )
        .await
        .unwrap();
    assert!(summary.dry_run);
    assert_eq!(
        summary.edges_rewired, 1,
        "note dry-run must predict the rewire count"
    );

    let from_edges = rt
        .list_edges(
            &tok,
            crate::EdgeListFilter {
                source_id: Some(from.id),
                target_id: Some(shared.id),
                relations: vec![EdgeRelation::Annotates],
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(from_edges.len(), 1, "dry-run must leave topology untouched");
}

#[tokio::test]
async fn merge_note_different_kinds_rejected() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_note(&tok, "observation", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_note(&tok, "decision", None, "From", None, None, vec![])
        .await
        .unwrap();

    let result = rt
        .merge_note_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await;
    assert!(result.is_err(), "merging different note kinds must fail");
}

#[tokio::test]
async fn merge_note_dry_run_leaves_notes_unchanged() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_note(
            &tok,
            "observation",
            None,
            "Into content",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let from = rt
        .create_note(
            &tok,
            "observation",
            None,
            "From content",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let into_id = into.id;
    let from_id = from.id;

    let summary = rt
        .merge_note_with_reason(
            &tok,
            into_id,
            from_id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            true,
            None,
        )
        .await
        .unwrap();

    assert!(summary.dry_run);

    let store = rt.notes(&tok).unwrap();
    let into_after = store.get_note(into_id).await.unwrap().unwrap();
    let from_after = store.get_note(from_id).await.unwrap().unwrap();
    assert_eq!(
        into_after.content, "Into content",
        "dry_run must not mutate into-note"
    );
    assert_eq!(
        from_after.content, "From content",
        "dry_run must not mutate from-note"
    );

    let events = rt
        .events(&tok)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![EventKind::NoteMerged],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert!(
        events.items.is_empty(),
        "dry_run=true must not append a NoteMerged event"
    );
}

// Merging two nameless notes with no embedding model configured: a raw SQL FTS
// INSERT binding &merged_name directly would store SQL NULL for a nameless
// note, while Fts5TextSearch::upsert_document stores an empty string:
// note_fts_scalars must keep the round-trip field-identical.
#[tokio::test]
async fn merge_nameless_notes_fts_document_is_parity_correct() {
    use khive_storage::types::TextSearchRequest;

    let rt = rt(); // in-memory runtime — no embedding model configured
    let tok = NamespaceToken::local();

    let into = rt
        .create_note(
            &tok,
            "observation",
            None,
            "intosentinelzxq body",
            None,
            Some(serde_json::json!({"src": "into"})),
            vec![],
        )
        .await
        .expect("create into-note");
    let from = rt
        .create_note(
            &tok,
            "observation",
            None,
            "fromsentinelzxq body",
            None,
            None,
            vec![],
        )
        .await
        .expect("create from-note");

    let into_id = into.id;
    let from_id = from.id;

    rt.merge_note_with_reason(
        &tok,
        into_id,
        from_id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
        None,
    )
    .await
    .expect("merge_note must succeed");

    let note_store = rt.notes(&tok).expect("note store");
    let merged_note = note_store
        .get_note(into_id)
        .await
        .expect("get_note")
        .expect("merged note must exist");

    let expected = note_fts_document(&merged_note);

    let fts = rt.text_for_notes(&tok).expect("FTS store");
    let stored = fts
        .get_document("local", into_id)
        .await
        .expect("get_document must not error")
        .expect("FTS document must exist after merge");

    assert_eq!(stored.subject_id, expected.subject_id, "subject_id");
    assert_eq!(
        stored.title, expected.title,
        "title (None for nameless note)"
    );
    assert_eq!(stored.body, expected.body, "body");
    assert_eq!(stored.namespace, expected.namespace, "namespace");
    assert_eq!(stored.kind, expected.kind, "kind");

    assert!(
        stored.title.is_none(),
        "nameless merged note must have title=None in FTS (was NULL before fix)"
    );

    // The merged note must be searchable by a unique token from the into-note body.
    let hits = fts
        .search(TextSearchRequest {
            query: "intosentinelzxq".to_string(),
            mode: khive_storage::types::TextQueryMode::Plain,
            filter: None,
            top_k: 10,
            snippet_chars: 0,
        })
        .await
        .expect("search");
    assert!(
        hits.iter().any(|h| h.subject_id == into_id),
        "merged note must be searchable by into-note content"
    );
}

#[tokio::test]
async fn update_edge_updates_properties() {
    use khive_storage::EdgeRelation;
    let rt = rt();
    let tok = NamespaceToken::local();
    let a = rt
        .create_entity(&tok, "concept", None, "A", None, None, vec![])
        .await
        .unwrap();
    let b = rt
        .create_entity(&tok, "concept", None, "B", None, None, vec![])
        .await
        .unwrap();
    let edge = rt
        .link(&tok, a.id, b.id, EdgeRelation::Extends, 0.5, None)
        .await
        .unwrap();
    let edge_id: Uuid = edge.id.into();

    let updated = rt
        .update_edge(
            &tok,
            edge_id,
            EdgePatch {
                properties: Some(serde_json::json!({"source": "manual"})),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.metadata.as_ref().unwrap()["source"], "manual");
    assert!((updated.weight - 0.5).abs() < 0.001, "weight unchanged");
}

// Merge must not crash when both entities share a common third-party edge
// (duplicate triple after rewire): a double-ON-CONFLICT INSERT would
// otherwise raise a UNIQUE constraint error and abort mid-transaction.
#[tokio::test]
async fn merge_entity_survives_shared_edge_to_third_party() {
    use khive_storage::EdgeRelation;
    let rt = rt();
    let tok = NamespaceToken::local();

    // A and B will be merged; shared is the common target. `extends` is used
    // since concept→concept is a valid endpoint combination.
    let a = rt
        .create_entity(&tok, "concept", None, "A", None, None, vec![])
        .await
        .unwrap();
    let b = rt
        .create_entity(&tok, "concept", None, "B", None, None, vec![])
        .await
        .unwrap();
    let shared = rt
        .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
        .await
        .unwrap();

    // Both A and B extend the same shared concept — this creates a duplicate
    // triple (A/B → shared, extends) that triggers the crash on rewire.
    rt.link(&tok, a.id, shared.id, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();
    rt.link(&tok, b.id, shared.id, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();

    let summary = rt
        .merge_entity_with_reason(
            &tok,
            a.id,
            b.id,
            crate::EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .expect("C1: merge must succeed even when both entities share an edge to a third party");

    assert_eq!(summary.kept_id, a.id);
    assert_eq!(summary.removed_id, b.id);
    // A already had the Extends edge to shared; rewiring B->shared onto it
    // hits the natural-key conflict arm, which drops the incoming (B-side)
    // duplicate rather than erroring or touching A's surviving row (ADR-039
    // `ON CONFLICT ... DO NOTHING`). The invariant checked below is that
    // exactly one live edge A->shared remains.
    let a_edges = rt
        .list_edges(
            &tok,
            crate::EdgeListFilter {
                source_id: Some(a.id),
                target_id: Some(shared.id),
                relations: vec![EdgeRelation::Extends],
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(
        a_edges.len(),
        1,
        "C1: exactly one live A→shared Extends edge must exist after merge; got: {a_edges:?}"
    );

    // get_entity filters deleted_at IS NULL, so a tombstoned entity returns None.
    let b_after = rt.entities(&tok).unwrap().get_entity(b.id).await.unwrap();
    assert!(
        b_after.is_none(),
        "C3: from_entity must be tombstoned (get_entity returns None for deleted) after merge; got: {b_after:?}"
    );
}

// ADR-039 conflict-arm regression (#1191): on a symmetric-edge merge collision,
// the surviving row's own weight/metadata must never be overwritten with the
// incoming (dropped) duplicate's values.
#[tokio::test]
async fn merge_entity_symmetric_conflict_preserves_survivor_fields() {
    use khive_storage::EdgeRelation;
    let rt = rt();
    let tok = NamespaceToken::local();

    let a = rt
        .create_entity(&tok, "concept", None, "A", None, None, vec![])
        .await
        .unwrap();
    let b = rt
        .create_entity(&tok, "concept", None, "B", None, None, vec![])
        .await
        .unwrap();
    let shared = rt
        .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
        .await
        .unwrap();

    let survivor_edge = rt
        .link(
            &tok,
            a.id,
            shared.id,
            EdgeRelation::Extends,
            1.0,
            Some(serde_json::json!({"source": "survivor"})),
        )
        .await
        .unwrap();
    rt.link(
        &tok,
        b.id,
        shared.id,
        EdgeRelation::Extends,
        0.3,
        Some(serde_json::json!({"source": "loser"})),
    )
    .await
    .unwrap();

    rt.merge_entity_with_reason(
        &tok,
        a.id,
        b.id,
        crate::EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
        None,
    )
    .await
    .expect("merge must succeed across the symmetric-edge collision");

    let after = rt
        .get_edge(&tok, survivor_edge.id.into())
        .await
        .unwrap()
        .expect("survivor edge must still exist after merge");
    assert!(
        (after.weight - 1.0).abs() < 0.001,
        "survivor weight must be untouched by the dropped duplicate; got {}",
        after.weight
    );
    assert_eq!(
        after.metadata.as_ref().unwrap()["source"],
        "survivor",
        "survivor metadata must be untouched by the dropped duplicate; got {:?}",
        after.metadata
    );
}

// ADR-039 conflict-arm regression (#1191): a soft-deleted survivor row must
// stay soft-deleted after a merge collision, never resurrected.
#[tokio::test]
async fn merge_entity_symmetric_conflict_does_not_resurrect_soft_deleted_survivor() {
    use khive_storage::EdgeRelation;
    let rt = rt();
    let tok = NamespaceToken::local();

    let a = rt
        .create_entity(&tok, "concept", None, "A", None, None, vec![])
        .await
        .unwrap();
    let b = rt
        .create_entity(&tok, "concept", None, "B", None, None, vec![])
        .await
        .unwrap();
    let shared = rt
        .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
        .await
        .unwrap();

    let survivor_edge = rt
        .link(&tok, a.id, shared.id, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();
    rt.delete_edge(&tok, survivor_edge.id.into(), false)
        .await
        .unwrap();
    rt.link(&tok, b.id, shared.id, EdgeRelation::Extends, 0.5, None)
        .await
        .unwrap();

    rt.merge_entity_with_reason(
        &tok,
        a.id,
        b.id,
        crate::EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
        None,
    )
    .await
    .expect("merge must succeed even when the surviving edge is soft-deleted");

    let after = rt
        .get_edge_including_deleted(&tok, survivor_edge.id.into())
        .await
        .unwrap()
        .expect("survivor edge row must still exist after merge");
    assert!(
        after.deleted_at.is_some(),
        "soft-deleted survivor must stay soft-deleted after merge collision; got: {after:?}"
    );
}

// merge_entity at the runtime level must reject cross-kind merges: without this
// guard, a direct runtime caller could merge concept+project, silently
// tombstoning the source entity, even though the pack handler also checks it.
#[tokio::test]
async fn merge_entity_cross_kind_rejected_at_runtime() {
    let rt = rt();
    let tok = NamespaceToken::local();

    let concept = rt
        .create_entity(&tok, "concept", None, "H2Concept", None, None, vec![])
        .await
        .unwrap();
    let project = rt
        .create_entity(&tok, "project", None, "H2Project", None, None, vec![])
        .await
        .unwrap();

    let err = rt
        .merge_entity_with_reason(
            &tok,
            concept.id,
            project.id,
            crate::EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .expect_err("H2: cross-kind merge must be rejected by runtime");
    assert!(
        matches!(err, crate::RuntimeError::InvalidInput(_)),
        "H2: expected InvalidInput, got: {err:?}"
    );

    let concept_after = rt.get_entity(&tok, concept.id).await;
    let project_after = rt.get_entity(&tok, project.id).await;
    assert!(
        concept_after.is_ok(),
        "H2: concept must remain live after rejected merge; got: {concept_after:?}"
    );
    assert!(
        project_after.is_ok(),
        "H2: project must remain live after rejected merge; got: {project_after:?}"
    );
}

// Same-kind merge must succeed.
#[tokio::test]
async fn merge_entity_same_kind_succeeds() {
    let rt = rt();
    let tok = NamespaceToken::local();

    let c1 = rt
        .create_entity(&tok, "concept", None, "Concept1", None, None, vec![])
        .await
        .unwrap();
    let c2 = rt
        .create_entity(&tok, "concept", None, "Concept2", None, None, vec![])
        .await
        .unwrap();

    let summary = rt
        .merge_entity_with_reason(
            &tok,
            c1.id,
            c2.id,
            crate::EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .expect("same-kind merge must succeed");
    assert_eq!(summary.kept_id, c1.id);
    assert_eq!(summary.removed_id, c2.id);

    let c2_after = rt.entities(&tok).unwrap().get_entity(c2.id).await.unwrap();
    assert!(c2_after.is_none(), "from_entity must be tombstoned");
}

#[tokio::test]
async fn merge_entity_explicit_policy_rereads_names_before_commit() {
    let rt = rt();
    let tok = NamespaceToken::local();

    let into = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "Transactional Guard",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let from = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "Transactional Guard",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();

    validate_entity_merge_floor(&into, &from)
        .expect("the handler's fast-path validation would initially pass");
    let renamed_into = rt
        .update_entity(
            &tok,
            into.id,
            EntityPatch {
                name: Some("Unrelated Renamed Target".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let expected = validate_entity_merge_floor(&renamed_into, &from)
        .expect_err("the renamed transactional state must violate the name guard");
    let RuntimeError::Khive(expected) = entity_merge_guard_error(expected) else {
        unreachable!("merge guard errors are structured Khive errors")
    };

    let err = rt
        .merge_entity_with_reason_and_force(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
            false,
        )
        .await
        .expect_err("the explicit non-forced path must validate its transactional reread");
    let RuntimeError::Khive(err) = err else {
        panic!("expected a structured merge-guard conflict, got {err:?}");
    };
    assert_eq!(err.kind(), expected.kind());
    assert_eq!(err.message(), expected.message());
    assert_eq!(err.code(), expected.code());
    assert_eq!(err.details(), expected.details());
    assert!(
        rt.get_entity(&tok, from.id).await.is_ok(),
        "a refused merge must leave the source entity live"
    );
}

// Cross-namespace merge_note must be denied on either ID.

#[tokio::test]
async fn merge_note_cross_namespace_either_id_returns_not_found() {
    use crate::error::RuntimeError;
    use crate::Namespace;

    let rt = rt();
    let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
    let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());

    let into_a = rt
        .create_note(&ns_a, "observation", None, "Into A", None, None, vec![])
        .await
        .unwrap();
    let from_a = rt
        .create_note(&ns_a, "observation", None, "From A", None, None, vec![])
        .await
        .unwrap();
    let note_b = rt
        .create_note(&ns_b, "observation", None, "Note B", None, None, vec![])
        .await
        .unwrap();

    // foreign into_id: note_b belongs to ns_b, caller token is ns_a
    let foreign_into = rt
        .merge_note_with_reason(
            &ns_a,
            note_b.id,
            from_a.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await;
    assert!(
        matches!(foreign_into, Err(RuntimeError::NotFound(_))),
        "foreign into_id must be denied before merge, got {foreign_into:?}"
    );

    // foreign from_id: note_b belongs to ns_b, caller token is ns_a
    let foreign_from = rt
        .merge_note_with_reason(
            &ns_a,
            into_a.id,
            note_b.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await;
    assert!(
        matches!(foreign_from, Err(RuntimeError::NotFound(_))),
        "foreign from_id must be denied before merge, got {foreign_from:?}"
    );
}

// Cross-namespace update now succeeds (shared-brain model).

#[tokio::test]
async fn update_entity_cross_namespace_succeeds() {
    use crate::Namespace;

    let rt = rt();
    let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
    let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());

    let entity = rt
        .create_entity(
            &ns_a,
            "concept",
            None,
            "Alpha",
            Some("original"),
            None,
            vec![],
        )
        .await
        .unwrap();

    let result = rt
        .update_entity(
            &ns_b,
            entity.id,
            EntityPatch {
                name: Some("Updated".into()),
                ..Default::default()
            },
        )
        .await;

    assert!(
        result.is_ok(),
        "cross-namespace update must succeed in shared-brain OSS; got {result:?}"
    );
    assert_eq!(result.unwrap().name, "Updated");
}

// merge_entity still requires both entities to be in the same namespace as
// the token's write namespace (enforced at the SQL transaction layer, not the
// runtime layer).  This is a merge-semantic constraint, not tenant isolation.
#[tokio::test]
async fn merge_entity_cross_namespace_ids_fail_at_sql_layer() {
    use crate::Namespace;

    let rt = rt();
    let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
    let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());

    let into_a = rt
        .create_entity(&ns_a, "concept", None, "Into A", None, None, vec![])
        .await
        .unwrap();
    let from_a = rt
        .create_entity(&ns_a, "concept", None, "From A", None, None, vec![])
        .await
        .unwrap();
    let foreign_b = rt
        .create_entity(&ns_b, "concept", None, "Foreign B", None, None, vec![])
        .await
        .unwrap();

    // foreign into_id: SQL read_merge_entity checks ns matches token namespace.
    let foreign_into = rt
        .merge_entity_with_reason(
            &ns_a,
            foreign_b.id,
            from_a.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await;
    assert!(
        foreign_into.is_err(),
        "cross-namespace into_id must still fail at SQL layer; got {foreign_into:?}"
    );

    // foreign from_id: same SQL constraint.
    let foreign_from = rt
        .merge_entity_with_reason(
            &ns_a,
            into_a.id,
            foreign_b.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await;
    assert!(
        foreign_from.is_err(),
        "cross-namespace from_id must still fail at SQL layer; got {foreign_from:?}"
    );

    // All three entities survive the failed merges.
    assert!(rt.get_entity(&ns_a, into_a.id).await.is_ok());
    assert!(rt.get_entity(&ns_a, from_a.id).await.is_ok());
    assert!(rt.get_entity(&ns_b, foreign_b.id).await.is_ok());
}

// Parity: entity_fts_document must produce the same body/title as the
// create_entity and update_entity FTS write paths.
#[test]
fn entity_fts_document_with_description() {
    let mut entity = Entity::new("local", "concept", "MyEntity");
    entity = entity.with_description("some description text");
    let doc = entity_fts_document(&entity);
    assert_eq!(doc.subject_id, entity.id);
    assert_eq!(doc.namespace, "local");
    assert_eq!(doc.title.as_deref(), Some("MyEntity"));
    assert_eq!(doc.body, "MyEntity some description text");
    assert_eq!(doc.kind, khive_types::SubstrateKind::Entity);
}

#[test]
fn entity_fts_document_without_description() {
    let entity = Entity::new("local", "concept", "NameOnly");
    let doc = entity_fts_document(&entity);
    assert_eq!(doc.title.as_deref(), Some("NameOnly"));
    assert_eq!(doc.body, "NameOnly");
}

#[test]
fn entity_fts_document_empty_description_uses_name_only() {
    let mut entity = Entity::new("local", "concept", "TitleOnly");
    entity = entity.with_description("");
    let doc = entity_fts_document(&entity);
    assert_eq!(
        doc.body, "TitleOnly",
        "empty description must not be appended"
    );
}

// Cross-path equality: an entity created through the runtime (operations.rs
// create_entity path) must produce a stored FTS document field-identical to
// entity_fts_document() called on the same Entity.
#[tokio::test]
async fn entity_fts_document_matches_runtime_create_path() {
    let rt = rt();
    let tok = NamespaceToken::local();

    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "CrossPathTitle",
            Some("cross path description body"),
            Some(serde_json::json!({"key": "val"})),
            vec!["tag1".to_string()],
        )
        .await
        .expect("create_entity");

    let fts = rt.text(&tok).expect("FTS store");
    let stored = fts
        .get_document("local", entity.id)
        .await
        .expect("get_document")
        .expect("document must exist after create_entity");

    let expected = entity_fts_document(&entity);

    assert_eq!(stored.subject_id, expected.subject_id, "subject_id");
    assert_eq!(stored.kind, expected.kind, "kind");
    assert_eq!(stored.title, expected.title, "title");
    assert_eq!(stored.body, expected.body, "body");
    assert_eq!(stored.namespace, expected.namespace, "namespace");
}

// Cross-path equality: update_entity must produce a stored FTS document
// field-identical to entity_fts_document() on the updated Entity.
#[tokio::test]
async fn entity_fts_document_matches_runtime_update_path() {
    let rt = rt();
    let tok = NamespaceToken::local();

    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "OldName",
            Some("old desc"),
            None,
            vec![],
        )
        .await
        .expect("create_entity");

    let updated = rt
        .update_entity(
            &tok,
            entity.id,
            EntityPatch {
                name: Some("NewName".to_string()),
                description: Some(Some("new desc".to_string())),
                ..Default::default()
            },
        )
        .await
        .expect("update_entity");

    let fts = rt.text(&tok).expect("FTS store");
    let stored = fts
        .get_document("local", updated.id)
        .await
        .expect("get_document")
        .expect("document must exist after update_entity");

    let expected = entity_fts_document(&updated);

    assert_eq!(stored.title, expected.title, "title after update");
    assert_eq!(stored.body, expected.body, "body after update");
}

// Verify that merge_entity / merge_note delete from_id vectors from ALL
// registered model vec tables, not just the default-model table. Uses the
// same ConstVecProvider/ConstVecService pattern as operations.rs so no
// real model files are required.

struct MergeTestVecService {
    dims: usize,
}

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for MergeTestVecService {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        Ok(texts.iter().map(|_| vec![1.0_f32; self.dims]).collect())
    }

    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "merge-test-const-vec"
    }
}

struct MergeTestVecProvider {
    provider_name: String,
    dims: usize,
}

impl MergeTestVecProvider {
    fn new(name: &str, dims: usize) -> Self {
        Self {
            provider_name: name.to_owned(),
            dims,
        }
    }
}

#[async_trait::async_trait]
impl crate::embedder_registry::EmbedderProvider for MergeTestVecProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    async fn build(
        &self,
    ) -> crate::error::RuntimeResult<std::sync::Arc<dyn lattice_embed::EmbeddingService>> {
        Ok(std::sync::Arc::new(MergeTestVecService { dims: self.dims }))
    }
}

#[tokio::test]
async fn entity_reindex_clears_attribution_when_replacement_blob_is_identical() {
    const MODEL: &str = "entity-provenance-identical";
    let rt = KhiveRuntime::memory().unwrap();
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "UnchangedEmbeddingInput",
            Some("entity description"),
            None,
            vec![],
        )
        .await
        .unwrap();
    rt.register_embedder(MergeTestVecProvider::new(MODEL, 4));
    let vectors = rt.vectors_for_model(&tok, MODEL).unwrap();
    let seeded = rt
        .embed_document_with_model_outcome_for_token(&tok, MODEL, &entity_embedding_text(&entity))
        .await
        .unwrap();
    // The test provider is custom, so the runtime correctly cannot attest
    // its prepared input. Seed a known historical sidecar explicitly to
    // exercise the raw writer's duty to clear it on an identical BLOB.
    assert!(seeded.prepared_text_fingerprint.is_none());
    let prepared = format!(
        "{}{}",
        lattice_embed::EmbeddingModel::default()
            .document_instruction()
            .unwrap_or_default(),
        entity_embedding_text(&entity)
    );
    let fingerprint = VectorRecord::fingerprint_text(&prepared);
    vectors
        .insert_batch(vec![VectorRecord {
            subject_id: entity.id,
            kind: SubstrateKind::Entity,
            namespace: entity.namespace.clone(),
            field: "entity.body".into(),
            embedding_model: Some(MODEL.into()),
            vectors: vec![seeded.vector],
            text_fingerprint: Some(fingerprint.clone()),
            updated_at: chrono::Utc::now(),
        }])
        .await
        .unwrap();
    assert_eq!(
        vectors
            .provenance(entity.id)
            .await
            .unwrap()
            .unwrap()
            .text_fingerprint,
        Some(fingerprint)
    );

    async fn live_blob(rt: &KhiveRuntime, model: &str, subject: Uuid) -> Vec<u8> {
        let table = format!("vec_{}", crate::config::sanitize_key(model));
        let mut reader = rt.sql().reader().await.unwrap();
        let blob = reader
            .query_scalar(SqlStatement {
                sql: format!("SELECT embedding FROM {table} WHERE subject_id = ?1"),
                params: vec![SqlValue::Text(subject.to_string())],
                label: Some("test-entity-reindex-live-blob".into()),
            })
            .await
            .unwrap();
        match blob {
            Some(SqlValue::Blob(blob)) => blob,
            other => panic!("expected live vec0 BLOB, got {other:?}"),
        }
    }

    let before = live_blob(&rt, MODEL, entity.id).await;
    // Property/tag-only updates do not automatically reindex. Explicitly
    // reindex the changed entity to exercise the curation raw writer with
    // the same prepared input and the constant provider's same BLOB.
    let updated = rt
        .update_entity(
            &tok,
            entity.id,
            EntityPatch {
                tags: Some(vec!["new-tag".into()]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        entity_embedding_text(&updated),
        entity_embedding_text(&entity)
    );
    assert!(vectors
        .provenance(entity.id)
        .await
        .unwrap()
        .unwrap()
        .text_fingerprint
        .is_some());
    rt.reindex_entity(&tok, &updated).await.unwrap();
    assert_eq!(live_blob(&rt, MODEL, entity.id).await, before);
    let after = vectors.provenance(entity.id).await.unwrap().unwrap();
    assert_eq!(after.text_fingerprint, None);
    assert_eq!(after.updated_at, None);

    let mut reader = rt.sql().reader().await.unwrap();
    let sidecar_count = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM vector_provenance \
                      WHERE model_key = ?1 AND subject_id = ?2"
                .into(),
            params: vec![
                SqlValue::Text(crate::config::sanitize_key(MODEL)),
                SqlValue::Text(entity.id.to_string()),
            ],
            label: Some("test-entity-reindex-sidecar-clear".into()),
        })
        .await
        .unwrap();
    assert!(matches!(sidecar_count, Some(SqlValue::Integer(0))));
}

#[tokio::test]
async fn entity_type_update_same_blob_reindex_clears_provenance() {
    const MODEL: &str = "entity-type-provenance-identical";
    let rt = KhiveRuntime::memory().unwrap();
    rt.install_entity_type_validator(Arc::new(|kind, entity_type| match (kind, entity_type) {
        ("concept", Some("algorithm")) => Ok(Some("algorithm".into())),
        (_, None) => Ok(None),
        _ => Err(RuntimeError::InvalidInput("invalid entity type".into())),
    }));
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "StableEmbeddingInput",
            Some("stable description"),
            None,
            vec![],
        )
        .await
        .unwrap();
    rt.register_embedder(MergeTestVecProvider::new(MODEL, 4));
    let vectors = rt.vectors_for_model(&tok, MODEL).unwrap();
    vectors
        .insert_batch(vec![VectorRecord {
            subject_id: entity.id,
            kind: SubstrateKind::Entity,
            namespace: entity.namespace.clone(),
            field: "entity.body".into(),
            embedding_model: Some(MODEL.into()),
            vectors: vec![vec![1.0; 4]],
            text_fingerprint: Some(VectorRecord::fingerprint_text("seeded prior text")),
            updated_at: chrono::Utc::now(),
        }])
        .await
        .unwrap();
    let table = format!("vec_{}", crate::config::sanitize_key(MODEL));
    let live_sql = SqlStatement {
        sql: format!("SELECT embedding FROM {table} WHERE subject_id = ?1"),
        params: vec![SqlValue::Text(entity.id.to_string())],
        label: Some("test-entity-type-live-blob".into()),
    };
    let before = {
        let mut reader = rt.sql().reader().await.unwrap();
        reader.query_scalar(live_sql.clone()).await.unwrap()
    };
    let Some(SqlValue::Blob(before_blob)) = before else {
        panic!("the original vector BLOB is missing")
    };

    let updated = rt
        .update_entity(
            &tok,
            entity.id,
            EntityPatch {
                entity_type: Some(Some("algorithm".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.entity_type.as_deref(), Some("algorithm"));
    assert_eq!(
        entity_embedding_text(&updated),
        entity_embedding_text(&entity)
    );
    let after = {
        let mut reader = rt.sql().reader().await.unwrap();
        reader.query_scalar(live_sql).await.unwrap()
    };
    let Some(SqlValue::Blob(after_blob)) = after else {
        panic!("the reindexed vector BLOB is missing")
    };
    assert_eq!(
        after_blob, before_blob,
        "the automatic reindex must use the same BLOB"
    );
    let observed = vectors.provenance(entity.id).await.unwrap().unwrap();
    assert_eq!(observed.text_fingerprint, None);
    assert_eq!(observed.updated_at, None);

    let sidecar_count = {
        let mut reader = rt.sql().reader().await.unwrap();
        reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM vector_provenance \
                      WHERE model_key = ?1 AND subject_id = ?2"
                    .into(),
                params: vec![
                    SqlValue::Text(crate::config::sanitize_key(MODEL)),
                    SqlValue::Text(entity.id.to_string()),
                ],
                label: Some("test-entity-type-sidecar-clear".into()),
            })
            .await
            .unwrap()
    };
    assert!(matches!(sidecar_count, Some(SqlValue::Integer(0))));
}

#[tokio::test]
async fn entity_raw_subject_only_move_clears_old_namespace_provenance() {
    const MODEL: &str = "entity-raw-move-model";
    let rt = KhiveRuntime::memory().unwrap();
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(&tok, "concept", None, "RawMove", None, None, vec![])
        .await
        .unwrap();
    rt.register_embedder(MergeTestVecProvider::new(MODEL, 4));
    let vectors = rt.vectors_for_model(&tok, MODEL).unwrap();
    vectors
        .insert_batch(vec![VectorRecord {
            subject_id: entity.id,
            kind: SubstrateKind::Entity,
            namespace: entity.namespace.clone(),
            field: "entity.body".into(),
            embedding_model: Some(MODEL.into()),
            vectors: vec![vec![1.0; 4]],
            text_fingerprint: Some(VectorRecord::fingerprint_text("old namespace")),
            updated_at: chrono::Utc::now(),
        }])
        .await
        .unwrap();
    let mut moved = entity.clone();
    moved.namespace = "other".into();
    let table = format!("vec_{}", crate::config::sanitize_key(MODEL));
    rt.sql()
        .writer()
        .await
        .unwrap()
        .execute_batch(KhiveRuntime::entity_vector_insert_statements(
            &table, &moved, MODEL, &[1.0; 4],
        ))
        .await
        .unwrap();

    let mut reader = rt.sql().reader().await.unwrap();
    let old_sidecar = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM vector_provenance WHERE model_key=?1 AND namespace=?2 AND subject_id=?3".into(),
            params: vec![
                SqlValue::Text(crate::config::sanitize_key(MODEL)),
                SqlValue::Text(entity.namespace.clone()),
                SqlValue::Text(entity.id.to_string()),
            ],
            label: Some("test-entity-raw-move-old-sidecar".into()),
        })
        .await
        .unwrap();
    assert!(matches!(old_sidecar, Some(SqlValue::Integer(0))));
    let new_vector = reader
        .query_scalar(SqlStatement {
            sql: format!("SELECT COUNT(*) FROM {table} WHERE namespace=?1 AND subject_id=?2"),
            params: vec![
                SqlValue::Text("other".into()),
                SqlValue::Text(entity.id.to_string()),
            ],
            label: Some("test-entity-raw-move-new-vector".into()),
        })
        .await
        .unwrap();
    assert!(matches!(new_vector, Some(SqlValue::Integer(1))));
}

async fn assert_delete_during_entity_reindex_does_not_restore_indexes(pause_vector: bool) {
    const MODEL: &str = "entity-reindex-delete-race";
    let rt = Arc::new(KhiveRuntime::memory().unwrap());
    let tok = NamespaceToken::local();
    rt.register_embedder(MergeTestVecProvider::new(MODEL, 4));
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "DeletedDuringReindex",
            Some("the stale document must not return"),
            None,
            vec![],
        )
        .await
        .unwrap();
    let id = entity.id;
    let barriers = Arc::new((tokio::sync::Barrier::new(2), tokio::sync::Barrier::new(2)));
    let reindex_rt = Arc::clone(&rt);
    let reindex_tok = tok.clone();
    let reindex = async move { reindex_rt.reindex_entity(&reindex_tok, &entity).await };
    let reindex = if pause_vector {
        tokio::spawn(race_seam::BEFORE_ENTITY_VECTOR_PUBLISH.scope(Arc::clone(&barriers), reindex))
    } else {
        tokio::spawn(race_seam::BEFORE_ENTITY_INDEX_PUBLISH.scope(Arc::clone(&barriers), reindex))
    };

    tokio::time::timeout(std::time::Duration::from_secs(10), barriers.0.wait())
        .await
        .expect("reindex reached publication boundary");
    assert!(rt.delete_entity(&tok, id, false).await.unwrap());
    barriers.1.wait().await;
    reindex.await.unwrap().unwrap();

    assert!(rt
        .text(&tok)
        .unwrap()
        .get_document("local", id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        rt.vectors_for_model(&tok, MODEL)
            .unwrap()
            .count()
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn deleted_entity_cannot_restore_fts_at_pending_reindex() {
    assert_delete_during_entity_reindex_does_not_restore_indexes(false).await;
}

#[tokio::test]
async fn deleted_entity_cannot_restore_vector_after_embedding() {
    assert_delete_during_entity_reindex_does_not_restore_indexes(true).await;
}

#[tokio::test]
async fn entity_reindex_with_captured_merge_plan_excludes_late_model() {
    const DIMS: usize = 4;
    const PLANNED: &str = "merge-entity-plan-existing";
    const LATE: &str = "merge-entity-plan-late";
    let rt = KhiveRuntime::memory().unwrap();
    let ns = crate::Namespace::parse("merge-entity-plan-snapshot").unwrap();
    let tok = NamespaceToken::for_namespace(ns);
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "CapturedEntityPlan",
            Some("full source remains indexed"),
            None,
            vec![],
        )
        .await
        .expect("create entity before registering embedders");

    rt.register_embedder(MergeTestVecProvider::new(PLANNED, DIMS));
    let embedding_plan = EmbeddingModelPlan::capture(&rt);
    rt.register_embedder(MergeTestVecProvider::new(LATE, DIMS));

    rt.reindex_entity_with_plan(&tok, &entity, &embedding_plan, None)
        .await
        .expect("reindex entity with captured merge plan");

    assert_eq!(embedding_plan.model_names().len(), 1);
    assert_eq!(embedding_plan.model_names()[0].as_str(), PLANNED);
    assert_eq!(
        rt.vectors_for_model(&tok, PLANNED)
            .unwrap()
            .count()
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        rt.vectors_for_model(&tok, LATE)
            .unwrap()
            .count()
            .await
            .unwrap(),
        0,
        "a provider registered after plan capture must not join survivor reindex"
    );
}

#[tokio::test]
async fn note_reindex_with_captured_merge_plan_excludes_late_model() {
    const DIMS: usize = 4;
    const PLANNED: &str = "merge-note-plan-existing";
    const LATE: &str = "merge-note-plan-late";
    let rt = KhiveRuntime::memory().unwrap();
    let ns = crate::Namespace::parse("merge-note-plan-snapshot").unwrap();
    let tok = NamespaceToken::for_namespace(ns);
    let note = rt
        .create_note(
            &tok,
            "observation",
            None,
            "full note source remains indexed",
            None,
            None,
            vec![],
        )
        .await
        .expect("create note before registering embedders");

    rt.register_embedder(MergeTestVecProvider::new(PLANNED, DIMS));
    let embedding_plan = EmbeddingModelPlan::capture(&rt);
    rt.register_embedder(MergeTestVecProvider::new(LATE, DIMS));

    rt.reindex_note_with_plan(&tok, &note, &embedding_plan)
        .await
        .expect("reindex note with captured merge plan");

    assert_eq!(embedding_plan.model_names().len(), 1);
    assert_eq!(embedding_plan.model_names()[0].as_str(), PLANNED);
    assert_eq!(
        rt.vectors_for_model(&tok, PLANNED)
            .unwrap()
            .count()
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        rt.vectors_for_model(&tok, LATE)
            .unwrap()
            .count()
            .await
            .unwrap(),
        0,
        "a provider registered after plan capture must not join survivor reindex"
    );
}

#[tokio::test]
async fn note_reindex_removes_stale_vectors_from_every_excluded_plan_model() {
    use crate::{NoteEmbeddingPolicy, NoteEmbeddingPolicySpec, RuntimeConfig};
    use khive_storage::types::VectorSearchRequest;
    use lattice_embed::EmbeddingModel;

    let primary = EmbeddingModel::AllMiniLmL6V2;
    let primary_name = primary.to_string();
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: Some(primary),
        packs: vec![],
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let tok = NamespaceToken::local();
    rt.register_embedder(MergeTestVecProvider::new(
        &primary_name,
        primary.dimensions(),
    ));
    for model in ["excluded-reindex-a", "excluded-reindex-b"] {
        rt.register_embedder(MergeTestVecProvider::new(model, 4));
    }

    let note = Note::new(
        "local",
        "message",
        "message content once indexed everywhere",
    );
    rt.notes(&tok)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    rt.reindex_note(&tok, &note).await.unwrap();
    for model in [
        primary_name.as_str(),
        "excluded-reindex-a",
        "excluded-reindex-b",
    ] {
        assert_eq!(
            rt.vectors_for_model(&tok, model)
                .unwrap()
                .count()
                .await
                .unwrap(),
            1
        );
    }
    for model in ["excluded-reindex-a", "excluded-reindex-b"] {
        let hits = rt
            .vectors_for_model(&tok, model)
            .unwrap()
            .search(VectorSearchRequest {
                query_vectors: vec![vec![1.0_f32; 4]],
                top_k: 10,
                namespace: Some("local".into()),
                kind: Some(SubstrateKind::Note),
                embedding_model: Some(model.into()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert!(hits.iter().any(|hit| hit.subject_id == note.id));
    }
    // Reindex writes raw vectors without a sidecar. Seed historical
    // provenance with a mismatched namespace: its primary key is only
    // (model_key, subject_id), so cleanup must not require a namespace match.
    let mut writer = rt.sql().writer().await.unwrap();
    for model in ["excluded-reindex-a", "excluded-reindex-b"] {
        let inserted = writer
            .execute(SqlStatement {
                sql: "INSERT INTO vector_provenance \
                          (model_key, subject_id, namespace, embedding_digest) \
                          VALUES (?1, ?2, ?3, ?4)"
                    .into(),
                params: vec![
                    SqlValue::Text(crate::config::sanitize_key(model)),
                    SqlValue::Text(note.id.to_string()),
                    SqlValue::Text("old-namespace".into()),
                    SqlValue::Text("0".repeat(64)),
                ],
                label: Some("test-excluded-reindex-seed-stale-provenance".into()),
            })
            .await
            .unwrap();
        assert_eq!(inserted, 1);
    }
    drop(writer);

    rt.install_note_embedding_policies(&[NoteEmbeddingPolicySpec {
        kind: "message",
        policy: NoteEmbeddingPolicy::DefaultModel,
    }]);
    let changed = rt
        .update_note(
            &tok,
            note.id,
            NotePatch {
                content: Some("changed message content after policy narrowing".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_ne!(changed.content, note.content);
    assert!(changed.version > note.version);
    assert_eq!(
        rt.text_for_notes(&tok)
            .unwrap()
            .get_document("local", note.id)
            .await
            .unwrap()
            .unwrap()
            .body,
        changed.content
    );

    assert_eq!(
        rt.vectors_for_model(&tok, &primary_name)
            .unwrap()
            .count()
            .await
            .unwrap(),
        1,
        "eligible default-space vector must survive"
    );
    for model in ["excluded-reindex-a", "excluded-reindex-b"] {
        let hits = rt
            .vectors_for_model(&tok, model)
            .unwrap()
            .search(VectorSearchRequest {
                query_vectors: vec![vec![1.0_f32; 4]],
                top_k: 10,
                namespace: Some("local".into()),
                kind: Some(SubstrateKind::Note),
                embedding_model: Some(model.into()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert!(
            hits.iter().all(|hit| hit.subject_id != note.id),
            "named-model search must not return a stale message from {model}"
        );
        assert_eq!(
            rt.vectors_for_model(&tok, model)
                .unwrap()
                .count()
                .await
                .unwrap(),
            0,
            "reindex must remove a stale row from {model}"
        );
    }
    let mut reader = rt.sql().reader().await.unwrap();
    let deletes = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM ann_write_log \
                      WHERE subject_id=?1 AND op='delete' AND \
                      embedding_model IN ('excluded-reindex-a', 'excluded-reindex-b')"
                .into(),
            params: vec![SqlValue::Text(note.id.to_string())],
            label: Some("test-excluded-reindex-delete-log".into()),
        })
        .await
        .unwrap();
    assert!(matches!(deletes, Some(SqlValue::Integer(2))));
    for model in ["excluded-reindex-a", "excluded-reindex-b"] {
        let sidecar = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM vector_provenance \
                          WHERE model_key=?1 AND subject_id=?2"
                    .into(),
                params: vec![
                    SqlValue::Text(crate::config::sanitize_key(model)),
                    SqlValue::Text(note.id.to_string()),
                ],
                label: Some("test-excluded-reindex-sidecar-clear".into()),
            })
            .await
            .unwrap();
        assert!(matches!(sidecar, Some(SqlValue::Integer(0))));
    }
}

#[tokio::test]
async fn excluded_model_key_collision_preserves_default_vector_on_embed_failure() {
    use crate::{NoteEmbeddingPolicy, NoteEmbeddingPolicySpec, RuntimeConfig};
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    struct FailingProvider {
        name: String,
        dimensions: usize,
        attempts: Arc<AtomicUsize>,
    }

    struct FailingService(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl EmbeddingService for FailingService {
        async fn embed(
            &self,
            _texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(EmbedError::InferenceFailed(
                "injected default embed failure".into(),
            ))
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "failing-default-vector"
        }
    }

    #[async_trait::async_trait]
    impl crate::embedder_registry::EmbedderProvider for FailingProvider {
        fn name(&self) -> &str {
            &self.name
        }

        fn dimensions(&self) -> usize {
            self.dimensions
        }

        async fn build(&self) -> crate::error::RuntimeResult<Arc<dyn EmbeddingService>> {
            Ok(Arc::new(FailingService(Arc::clone(&self.attempts))))
        }
    }

    let primary = EmbeddingModel::AllMiniLmL6V2;
    let primary_name = primary.to_string();
    let excluded_name = "all.minilm.l6.v2";
    assert_eq!(
        crate::config::sanitize_key(&primary_name),
        crate::config::sanitize_key(excluded_name)
    );

    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: Some(primary),
        packs: vec![],
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let tok = NamespaceToken::local();
    rt.register_embedder(MergeTestVecProvider::new(
        &primary_name,
        primary.dimensions(),
    ));
    let note = Note::new("local", "message", "message before policy narrowing");
    rt.notes(&tok)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    rt.reindex_note(&tok, &note).await.unwrap();
    assert_eq!(
        rt.vectors_for_model(&tok, &primary_name)
            .unwrap()
            .count()
            .await
            .unwrap(),
        1,
        "fixture must seed the default vector before the failing update"
    );

    let model_key = crate::config::sanitize_key(&primary_name);
    let mut writer = rt.sql().writer().await.unwrap();
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO vector_provenance \
                      (model_key, subject_id, namespace, embedding_digest) \
                      VALUES (?1, ?2, ?3, ?4)"
                .into(),
            params: vec![
                SqlValue::Text(model_key.clone()),
                SqlValue::Text(note.id.to_string()),
                SqlValue::Text("local".into()),
                SqlValue::Text("0".repeat(64)),
            ],
            label: Some("test-colliding-excluded-seed-provenance".into()),
        })
        .await
        .unwrap();
    drop(writer);

    rt.register_embedder(MergeTestVecProvider::new(
        excluded_name,
        primary.dimensions(),
    ));
    let attempts = Arc::new(AtomicUsize::new(0));
    rt.register_embedder(FailingProvider {
        name: primary_name.clone(),
        dimensions: primary.dimensions(),
        attempts: Arc::clone(&attempts),
    });
    rt.install_note_embedding_policies(&[NoteEmbeddingPolicySpec {
        kind: "message",
        policy: NoteEmbeddingPolicy::DefaultModel,
    }]);

    rt.update_note(
        &tok,
        note.id,
        NotePatch {
            content: Some("message after policy narrowing".into()),
            ..Default::default()
        },
    )
    .await
    .expect("best-effort default embed failure does not fail note update");
    assert!(attempts.load(Ordering::SeqCst) > 0);

    let table = format!("vec_{model_key}");
    let mut reader = rt.sql().reader().await.unwrap();
    let retained_model = reader
        .query_scalar(SqlStatement {
            sql: format!(
                "SELECT embedding_model FROM {table} WHERE subject_id=?1 AND namespace=?2"
            ),
            params: vec![
                SqlValue::Text(note.id.to_string()),
                SqlValue::Text("local".into()),
            ],
            label: Some("test-colliding-excluded-default-retained".into()),
        })
        .await
        .unwrap();
    assert!(
        matches!(retained_model.as_ref(), Some(SqlValue::Text(model)) if model == &primary_name),
        "default vector row was lost or replaced: {retained_model:?}"
    );
    let provenance = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM vector_provenance \
                      WHERE model_key=?1 AND subject_id=?2"
                .into(),
            params: vec![
                SqlValue::Text(model_key),
                SqlValue::Text(note.id.to_string()),
            ],
            label: Some("test-colliding-excluded-provenance-retained".into()),
        })
        .await
        .unwrap();
    assert!(matches!(provenance, Some(SqlValue::Integer(1))));
}

/// merge_entity must delete from_id vectors from ALL registered model tables.
///
/// Two custom embedders ("merge-vec-a", "merge-vec-b") are registered.  Both
/// entities are embedded so each has a row in both model tables.  After merge,
/// from_id must have zero surviving rows in either table.
#[tokio::test]
async fn merge_entity_clears_vectors_from_all_registered_models() {
    const DIMS: usize = 4;
    let rt = KhiveRuntime::memory().unwrap();
    rt.register_embedder(MergeTestVecProvider::new("merge-vec-a", DIMS));
    rt.register_embedder(MergeTestVecProvider::new("merge-vec-b", DIMS));

    let ns_str = "merge-entity-vec-cleanup";
    let ns = crate::Namespace::parse(ns_str).unwrap();
    let tok = NamespaceToken::for_namespace(ns);

    let into_e = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "IntoVecEntity",
            Some("desc a"),
            None,
            vec![],
        )
        .await
        .expect("create into");
    let from_e = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "FromVecEntity",
            Some("desc b"),
            None,
            vec![],
        )
        .await
        .expect("create from");

    // Confirm both entities have vectors in both model tables before merge.
    let vs_a = rt.vectors_for_model(&tok, "merge-vec-a").unwrap();
    let vs_b = rt.vectors_for_model(&tok, "merge-vec-b").unwrap();
    use khive_storage::types::VectorSearchRequest;
    let query = vec![1.0_f32; DIMS];
    let pre_a = vs_a
        .search(VectorSearchRequest {
            query_vectors: vec![query.clone()],
            top_k: 100,
            namespace: Some(ns_str.to_string()),
            kind: Some(khive_types::SubstrateKind::Entity),
            embedding_model: Some("merge-vec-a".to_string()),
            filter: None,
            backend_hints: None,
        })
        .await
        .unwrap();
    assert!(
        pre_a.iter().any(|h| h.subject_id == into_e.id)
            && pre_a.iter().any(|h| h.subject_id == from_e.id),
        "both entities must be in model-a before merge; got {pre_a:?}"
    );

    // model-b must ALSO hold both entities pre-merge, else the post-merge
    // model-b emptiness check below is vacuous (nothing to delete).
    let pre_b = vs_b
        .search(VectorSearchRequest {
            query_vectors: vec![query.clone()],
            top_k: 100,
            namespace: Some(ns_str.to_string()),
            kind: Some(khive_types::SubstrateKind::Entity),
            embedding_model: Some("merge-vec-b".to_string()),
            filter: None,
            backend_hints: None,
        })
        .await
        .unwrap();
    assert!(
        pre_b.iter().any(|h| h.subject_id == into_e.id)
            && pre_b.iter().any(|h| h.subject_id == from_e.id),
        "both entities must be in model-b before merge; got {pre_b:?}"
    );

    rt.merge_entity_with_reason(
        &tok,
        into_e.id,
        from_e.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::Append,
        false,
        None,
    )
    .await
    .expect("merge_entity");

    let post_a = vs_a
        .search(VectorSearchRequest {
            query_vectors: vec![query.clone()],
            top_k: 100,
            namespace: Some(ns_str.to_string()),
            kind: Some(khive_types::SubstrateKind::Entity),
            embedding_model: Some("merge-vec-a".to_string()),
            filter: None,
            backend_hints: None,
        })
        .await
        .unwrap();
    let from_ids_a: Vec<_> = post_a
        .iter()
        .filter(|h| h.subject_id == from_e.id)
        .collect();
    assert!(
        from_ids_a.is_empty(),
        "from_id must have no vectors in model-a after merge; got {from_ids_a:?}"
    );

    let post_b = vs_b
        .search(VectorSearchRequest {
            query_vectors: vec![query],
            top_k: 100,
            namespace: Some(ns_str.to_string()),
            kind: Some(khive_types::SubstrateKind::Entity),
            embedding_model: Some("merge-vec-b".to_string()),
            filter: None,
            backend_hints: None,
        })
        .await
        .unwrap();
    let from_ids_b: Vec<_> = post_b
        .iter()
        .filter(|h| h.subject_id == from_e.id)
        .collect();
    assert!(
        from_ids_b.is_empty(),
        "from_id must have no vectors in model-b after merge; got {from_ids_b:?}"
    );
}

/// merge_note must delete from_id vectors from ALL registered model tables.
///
/// Two custom embedders ("merge-note-vec-a", "merge-note-vec-b") are registered.
/// Both notes are embedded so each has a row in both model tables.  After merge,
/// from_id must have zero surviving rows in either table.
#[tokio::test]
async fn merge_note_clears_vectors_from_all_registered_models() {
    const DIMS: usize = 4;
    let rt = KhiveRuntime::memory().unwrap();
    rt.register_embedder(MergeTestVecProvider::new("merge-note-vec-a", DIMS));
    rt.register_embedder(MergeTestVecProvider::new("merge-note-vec-b", DIMS));

    let ns_str = "merge-note-vec-cleanup";
    let ns = crate::Namespace::parse(ns_str).unwrap();
    let tok = NamespaceToken::for_namespace(ns);

    let into_n = rt
        .create_note(
            &tok,
            "observation",
            None,
            "IntoVecNote content",
            None,
            None,
            vec![],
        )
        .await
        .expect("create into note");
    let from_n = rt
        .create_note(
            &tok,
            "observation",
            None,
            "FromVecNote content",
            None,
            None,
            vec![],
        )
        .await
        .expect("create from note");

    let vs_a = rt.vectors_for_model(&tok, "merge-note-vec-a").unwrap();
    let vs_b = rt.vectors_for_model(&tok, "merge-note-vec-b").unwrap();
    use khive_storage::types::VectorSearchRequest;
    let query = vec![1.0_f32; DIMS];

    let pre_a = vs_a
        .search(VectorSearchRequest {
            query_vectors: vec![query.clone()],
            top_k: 100,
            namespace: Some(ns_str.to_string()),
            kind: Some(khive_types::SubstrateKind::Note),
            embedding_model: Some("merge-note-vec-a".to_string()),
            filter: None,
            backend_hints: None,
        })
        .await
        .unwrap();
    assert!(
        pre_a.iter().any(|h| h.subject_id == into_n.id)
            && pre_a.iter().any(|h| h.subject_id == from_n.id),
        "both notes must be in model-a before merge; got {pre_a:?}"
    );

    // model-b must ALSO hold both notes pre-merge, else the post-merge
    // model-b emptiness check below is vacuous (nothing to delete).
    let pre_b = vs_b
        .search(VectorSearchRequest {
            query_vectors: vec![query.clone()],
            top_k: 100,
            namespace: Some(ns_str.to_string()),
            kind: Some(khive_types::SubstrateKind::Note),
            embedding_model: Some("merge-note-vec-b".to_string()),
            filter: None,
            backend_hints: None,
        })
        .await
        .unwrap();
    assert!(
        pre_b.iter().any(|h| h.subject_id == into_n.id)
            && pre_b.iter().any(|h| h.subject_id == from_n.id),
        "both notes must be in model-b before merge; got {pre_b:?}"
    );

    rt.merge_note_with_reason(
        &tok,
        into_n.id,
        from_n.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::PreferInto,
        false,
        None,
    )
    .await
    .expect("merge_note");

    let post_a = vs_a
        .search(VectorSearchRequest {
            query_vectors: vec![query.clone()],
            top_k: 100,
            namespace: Some(ns_str.to_string()),
            kind: Some(khive_types::SubstrateKind::Note),
            embedding_model: Some("merge-note-vec-a".to_string()),
            filter: None,
            backend_hints: None,
        })
        .await
        .unwrap();
    let from_ids_a: Vec<_> = post_a
        .iter()
        .filter(|h| h.subject_id == from_n.id)
        .collect();
    assert!(
        from_ids_a.is_empty(),
        "from_id must have no vectors in model-a after merge; got {from_ids_a:?}"
    );

    let post_b = vs_b
        .search(VectorSearchRequest {
            query_vectors: vec![query],
            top_k: 100,
            namespace: Some(ns_str.to_string()),
            kind: Some(khive_types::SubstrateKind::Note),
            embedding_model: Some("merge-note-vec-b".to_string()),
            filter: None,
            backend_hints: None,
        })
        .await
        .unwrap();
    let from_ids_b: Vec<_> = post_b
        .iter()
        .filter(|h| h.subject_id == from_n.id)
        .collect();
    assert!(
        from_ids_b.is_empty(),
        "from_id must have no vectors in model-b after merge; got {from_ids_b:?}"
    );
}

// Cross-path equality: merge_entity must produce a stored FTS document for
// the kept entity that is field-identical to entity_fts_document().
#[tokio::test]
async fn entity_fts_document_matches_runtime_merge_path() {
    let rt = rt();
    let tok = NamespaceToken::local();

    let into_e = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "IntoEntity",
            Some("into desc"),
            None,
            vec![],
        )
        .await
        .expect("create into");
    let from_e = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "FromEntity",
            Some("from desc"),
            None,
            vec![],
        )
        .await
        .expect("create from");

    let summary = rt
        .merge_entity_with_reason(
            &tok,
            into_e.id,
            from_e.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .expect("merge_entity");

    let kept = rt
        .get_entity(&tok, summary.kept_id)
        .await
        .expect("get kept");

    let fts = rt.text(&tok).expect("FTS store");
    let stored = fts
        .get_document("local", kept.id)
        .await
        .expect("get_document")
        .expect("FTS document must exist for kept entity after merge");

    let expected = entity_fts_document(&kept);

    assert_eq!(stored.title, expected.title, "title after merge");
    assert_eq!(stored.body, expected.body, "body after merge");
}

/// The recomputed `properties_merged` count must agree with the fold on
/// whole-value replacement.
///
/// `merge_json` scores a `PreferFrom` replace of one properties value by a
/// differently-shaped one as a single contribution. An earlier version of
/// `count_new_property_keys` returned 0 for that shape, which under-reported
/// every such merge — including on note kinds with no owner-established
/// properties, which never enter the restoration path and were being counted
/// correctly before the recompute was introduced. Measured at the time:
/// fold=1, recompute=0, on both orderings.
///
/// The flat-overwrite control is load-bearing: it is what distinguishes this
/// test from one that a function returning 1 unconditionally would also pass.
#[test]
fn recomputed_count_agrees_with_fold_on_whole_value_replacement() {
    use serde_json::json;

    for (into, from, label) in [
        (json!({"a": 1}), json!(5), "object replaced by scalar"),
        (
            json!(7),
            json!({"b": 2}),
            "scalar replaced by single-key object",
        ),
        // A whole-value replacement is ONE contribution however many keys
        // the replacing object carries. The single-key vector above cannot
        // see the difference between that rule and counting the object's
        // keys, so it stayed green while the count was wrong for ordinary
        // notes. This vector is the one that distinguishes them.
        (
            json!(7),
            json!({"b": 2, "c": 3}),
            "scalar replaced by multi-key object",
        ),
    ] {
        let (merged, fold_count) = merge_json(&into, &from, EntityDedupMergePolicy::PreferFrom);
        let recomputed = count_new_property_keys(
            Some(&into),
            Some(&merged),
            EntityDedupMergePolicy::PreferFrom,
        );
        assert_eq!(
            recomputed, fold_count,
            "{label}: recomputed count must match the fold's own count",
        );
        assert_eq!(recomputed, 1, "{label}: one value was contributed");
    }

    // Control: an ordinary overwrite of an existing key contributes nothing
    // under BOTH rules. Without this arm, a function returning 1 for every
    // differing pair would pass the loop above.
    let (merged, fold_count) = merge_json(
        &json!({"a": 1}),
        &json!({"a": 2}),
        EntityDedupMergePolicy::PreferFrom,
    );
    let recomputed = count_new_property_keys(
        Some(&json!({"a": 1})),
        Some(&merged),
        EntityDedupMergePolicy::PreferFrom,
    );
    assert_eq!(fold_count, 0, "control: overwrite is never a fold addition");
    assert_eq!(recomputed, 0, "control: overwrite is never a new key");

    // Control: equal values mean nothing was contributed (the `from` note
    // carrying no properties at all).
    assert_eq!(
        count_new_property_keys(
            Some(&json!({"a": 1})),
            Some(&json!({"a": 1})),
            EntityDedupMergePolicy::PreferFrom,
        ),
        0,
        "control: an unchanged properties object contributes nothing",
    );
}

/// Recursion into a same-named nested object is only correct under `Union`.
///
/// `merge_json` descends into a nested object ONLY for `Union`. Under
/// `PreferFrom` an existing top-level key is replaced wholesale and under
/// `PreferInto` it is kept wholesale, so nothing is merged beneath that key
/// and nothing beneath it may be counted. An earlier version of the
/// recomputation recursed unconditionally and reported 1 for the
/// `PreferFrom` case below, where one existing property was replaced and
/// none was added. This affects ordinary notes with no owner-established
/// properties, which never reach the restoration path at all.
#[test]
fn recomputed_count_recurses_into_nested_objects_only_under_union() {
    use serde_json::json;

    let into = json!({"meta": {"old": 1}});
    let from = json!({"meta": {"new": 2}});

    for (strategy, expected, label) in [
        (
            EntityDedupMergePolicy::PreferFrom,
            0,
            "prefer_from replaces the whole key",
        ),
        (
            EntityDedupMergePolicy::PreferInto,
            0,
            "prefer_into keeps the whole key",
        ),
        (
            EntityDedupMergePolicy::Union,
            1,
            "union merges beneath the key",
        ),
    ] {
        let (merged, fold_count) = merge_json(&into, &from, strategy);
        let recomputed = count_new_property_keys(Some(&into), Some(&merged), strategy);
        assert_eq!(
            recomputed, fold_count,
            "{label}: recomputed count must match the fold's own count",
        );
        assert_eq!(recomputed, expected, "{label}");
    }
}

/// An object emptied by restoration contributed nothing, and the count must
/// say so.
///
/// When the surviving record's properties were not an object and the fold
/// installed the from-note's object, restoration removes the owner keys the
/// survivor never had — which can leave `{}`. Scoring that as a whole-value
/// replacement would report 1 for a record holding no properties at all.
#[test]
fn recomputed_count_is_zero_when_restoration_empties_the_object() {
    use serde_json::json;

    assert_eq!(
        count_new_property_keys(
            Some(&json!("scalar-properties")),
            Some(&json!({})),
            EntityDedupMergePolicy::PreferFrom,
        ),
        0,
        "an emptied object retains nothing from the absorbed record",
    );

    // Control: the same shape with a surviving key counts that key, so the
    // arm above is not simply returning 0 for every non-object original.
    assert_eq!(
        count_new_property_keys(
            Some(&json!("scalar-properties")),
            Some(&json!({"kept": 1})),
            EntityDedupMergePolicy::PreferFrom,
        ),
        1,
        "a surviving key is still counted",
    );
}

// ---- merge transaction budget tests ----

/// Run `merge_entity_sql` directly on the writer connection with explicit
/// limits, mapping the two-variant error the way the production fallback
/// path does. The budget refusal must surface as the SQLite-side error
/// whose message carries the observed counts.
async fn run_entity_merge_with_limits(
    rt: &KhiveRuntime,
    into_id: Uuid,
    from_id: Uuid,
    limits: MergeTxLimits,
) -> Result<(MergeSummary, Entity), SqliteError> {
    let pack_rules = rt.pack_edge_rules();
    let pool = rt.backend().pool_arc();
    tokio::task::spawn_blocking(move || {
        let guard = pool.writer().unwrap();
        guard.transaction(|conn| {
            merge_entity_sql(
                conn,
                "local".to_string(),
                "fts_entities".to_string(),
                Vec::new(),
                into_id,
                from_id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                pack_rules,
                EntityMergeValidation::LegacyKind,
                limits,
                Uuid::new_v4(),
                None,
            )
            .map_err(|error| match error {
                MergeSqlError::Sqlite(error) => error,
                MergeSqlError::Refusal(_) => {
                    SqliteError::InvalidData("unexpected transactional policy refusal".to_string())
                }
            })
        })
    })
    .await
    .unwrap()
}

/// Preview-only variant of [`run_entity_merge_with_limits`]: runs
/// `merge_entity_sql` with `dry_run = true` and an unlimited budget, and
/// returns the observed byte charge without committing any write. Lets a
/// test read back the probe's true cost for a record and then reuse that
/// exact number to place a tight `MergeTxLimits` threshold, instead of
/// guessing at fanout/overhead constants.
async fn preview_entity_merge_bytes(rt: &KhiveRuntime, into_id: Uuid, from_id: Uuid) -> usize {
    let pack_rules = rt.pack_edge_rules();
    let pool = rt.backend().pool_arc();
    let (summary, _) = tokio::task::spawn_blocking(move || {
        let guard = pool.writer().unwrap();
        guard.transaction(|conn| {
            merge_entity_sql(
                conn,
                "local".to_string(),
                "fts_entities".to_string(),
                Vec::new(),
                into_id,
                from_id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                true,
                pack_rules,
                EntityMergeValidation::LegacyKind,
                MergeTxLimits {
                    max_rows: usize::MAX,
                    max_bytes: usize::MAX,
                },
                Uuid::new_v4(),
                None,
            )
            .map_err(|error| match error {
                MergeSqlError::Sqlite(error) => error,
                MergeSqlError::Refusal(_) => {
                    SqliteError::InvalidData("unexpected transactional policy refusal".to_string())
                }
            })
        })
    })
    .await
    .unwrap()
    .unwrap();
    summary.tx_budget.bytes_charged
}

/// The byte-budget probe must count actual UTF-8 bytes, not SQLite's
/// `LENGTH(text)` character count. Two records with an identical
/// character count but different UTF-8 byte sizes (an ASCII control vs.
/// a CJK payload, each 200 characters) must charge the budget
/// differently — proving the probe casts to BLOB before measuring —
/// and a budget threshold placed strictly between the two true costs
/// must accept the ASCII control and reject the multibyte payload.
#[tokio::test]
async fn merge_entity_byte_budget_rejects_multibyte_properties_char_count_would_pass() {
    let rt = rt();
    let tok = NamespaceToken::local();

    let ascii_payload = "x".repeat(200);
    let multibyte_payload = "\u{4e2d}".repeat(200);
    assert_eq!(
        ascii_payload.chars().count(),
        multibyte_payload.chars().count(),
        "control and payload must share one character count"
    );
    assert!(
        multibyte_payload.len() > ascii_payload.len(),
        "multibyte payload must have more UTF-8 bytes than the ASCII control"
    );

    let into_ascii = rt
        .create_entity(&tok, "concept", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from_ascii = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "From",
            None,
            Some(serde_json::json!({ "note": ascii_payload })),
            vec![],
        )
        .await
        .unwrap();
    let into_multi = rt
        .create_entity(&tok, "concept", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from_multi = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "From",
            None,
            Some(serde_json::json!({ "note": multibyte_payload })),
            vec![],
        )
        .await
        .unwrap();

    let ascii_total_bytes = preview_entity_merge_bytes(&rt, into_ascii.id, from_ascii.id).await;
    let multi_total_bytes = preview_entity_merge_bytes(&rt, into_multi.id, from_multi.id).await;
    assert!(
        multi_total_bytes > ascii_total_bytes,
        "byte-accurate probe must charge more for the multibyte record: \
             ascii={ascii_total_bytes} multi={multi_total_bytes}"
    );

    // A threshold pinned exactly at the ASCII control's true cost must
    // accept it and reject the multibyte record, which a character-
    // counting probe would have under-charged into passing too.
    let limits = MergeTxLimits {
        max_rows: usize::MAX,
        max_bytes: ascii_total_bytes,
    };

    run_entity_merge_with_limits(&rt, into_ascii.id, from_ascii.id, limits)
        .await
        .expect("ASCII control's true byte cost must fit its own threshold");

    let error = run_entity_merge_with_limits(&rt, into_multi.id, from_multi.id, limits)
        .await
        .unwrap_err();
    let msg = error.to_string();
    assert!(
        msg.contains("merge transaction budget exceeded"),
        "multibyte properties must be rejected by the byte-accurate probe; got: {msg}"
    );
    assert!(msg.contains("reading merge records"), "got: {msg}");
    assert!(
        rt.get_entity(&tok, from_multi.id).await.is_ok(),
        "from-entity must survive a budget-rejected merge"
    );
}

async fn run_note_merge_with_limits(
    rt: &KhiveRuntime,
    into_id: Uuid,
    from_id: Uuid,
    pack_rules: Vec<khive_types::EdgeEndpointRule>,
    limits: MergeTxLimits,
) -> Result<(MergeSummary, Note), SqliteError> {
    let pool = rt.backend().pool_arc();
    tokio::task::spawn_blocking(move || {
        let guard = pool.writer().unwrap();
        guard.transaction(|conn| {
            merge_note_sql(
                conn,
                "local".to_string(),
                "fts_notes".to_string(),
                Vec::new(),
                into_id,
                from_id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                pack_rules,
                false,
                limits,
                None,
                None,
            )
            .map_err(|error| match error {
                MergeSqlError::Sqlite(error) => error,
                MergeSqlError::Refusal(_) => {
                    SqliteError::InvalidData("unexpected transactional policy refusal".to_string())
                }
            })
        })
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn merge_entity_rejects_row_budget_while_collecting_incident_edges() {
    use khive_storage::EdgeRelation;
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", None, None, vec![])
        .await
        .unwrap();
    for name in ["T1", "T2", "T3"] {
        let target = rt
            .create_entity(&tok, "concept", None, name, None, None, vec![])
            .await
            .unwrap();
        rt.link(&tok, from.id, target.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
    }

    // Two merge records charge first; the cap of 4 admits the first two
    // incident edges and trips on the third, before it is retained.
    let error = run_entity_merge_with_limits(
        &rt,
        into.id,
        from.id,
        MergeTxLimits {
            max_rows: 4,
            max_bytes: usize::MAX,
        },
    )
    .await
    .unwrap_err();
    let msg = error.to_string();
    assert!(
        msg.contains("merge transaction budget exceeded"),
        "got: {msg}"
    );
    assert!(msg.contains("collecting incident edges"), "got: {msg}");

    // The rejected transaction must roll back completely.
    assert!(
        rt.get_entity(&tok, from.id).await.is_ok(),
        "from-entity must survive a budget-rejected merge"
    );
    let edges = rt
        .list_edges(
            &tok,
            EdgeListFilter {
                source_id: Some(from.id),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(
        edges.len(),
        3,
        "every incident edge must survive a budget-rejected merge"
    );
}

#[tokio::test]
async fn merge_entity_rejects_row_budget_while_collecting_conflict_cascade_rows() {
    use khive_storage::EdgeRelation;
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", None, None, vec![])
        .await
        .unwrap();
    let shared = rt
        .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
        .await
        .unwrap();
    let annotator = rt
        .create_note(
            &tok,
            "observation",
            None,
            "annotator note",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let nested_annotator = rt
        .create_note(&tok, "observation", None, "nested note", None, None, vec![])
        .await
        .unwrap();
    rt.link(&tok, into.id, shared.id, EdgeRelation::Extends, 0.9, None)
        .await
        .unwrap();
    let dropped = rt
        .link(&tok, from.id, shared.id, EdgeRelation::Extends, 0.2, None)
        .await
        .unwrap();
    let annotation = rt
        .link(
            &tok,
            annotator.id,
            dropped.id.into(),
            EdgeRelation::Annotates,
            0.7,
            None,
        )
        .await
        .unwrap();
    let nested_annotation = rt
        .link(
            &tok,
            nested_annotator.id,
            annotation.id.into(),
            EdgeRelation::Annotates,
            0.6,
            None,
        )
        .await
        .unwrap();

    // Row walk under a cap of 5: two merge records, one incident edge,
    // one endpoint-contract resolution, then the natural-key conflict's
    // recursive cascade collection charges the annotation chain and trips
    // on its second (nested) row.
    let error = run_entity_merge_with_limits(
        &rt,
        into.id,
        from.id,
        MergeTxLimits {
            max_rows: 5,
            max_bytes: usize::MAX,
        },
    )
    .await
    .unwrap_err();
    let msg = error.to_string();
    assert!(
        msg.contains("merge transaction budget exceeded"),
        "got: {msg}"
    );
    assert!(
        msg.contains("collecting conflict cascade rows"),
        "got: {msg}"
    );

    // Roll back means the whole annotation chain is still present.
    for id in [dropped.id, annotation.id, nested_annotation.id] {
        assert!(
            rt.get_edge_including_deleted(&tok, id.into())
                .await
                .unwrap()
                .is_some(),
            "edge {id} must survive a budget-rejected merge"
        );
    }
    assert!(
        rt.get_entity(&tok, from.id).await.is_ok(),
        "from-entity must survive a budget-rejected merge"
    );
}

#[tokio::test]
async fn merge_note_rejects_byte_budget_while_reading_merge_records() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_note(
            &tok,
            "observation",
            None,
            "into content",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let fat = "x".repeat(8192);
    let from = rt
        .create_note(&tok, "observation", None, &fat, None, None, vec![])
        .await
        .unwrap();

    let error = run_note_merge_with_limits(
        &rt,
        into.id,
        from.id,
        Vec::new(),
        MergeTxLimits {
            max_rows: usize::MAX,
            max_bytes: 4096,
        },
    )
    .await
    .unwrap_err();
    let msg = error.to_string();
    assert!(
        msg.contains("merge transaction budget exceeded"),
        "got: {msg}"
    );
    assert!(msg.contains("reading merge records"), "got: {msg}");

    assert!(
        rt.notes(&tok)
            .unwrap()
            .get_note(from.id)
            .await
            .unwrap()
            .is_some(),
        "from-note must survive a budget-rejected merge"
    );
}

/// The byte budget must be charged from a cheap SQL-side length probe
/// BEFORE the merge fully loads and JSON-parses a record's `properties`
/// column — never after. Prove it adversarially: store an oversized
/// `properties` value that is also invalid JSON directly on `from`,
/// bypassing the create path's own validation. If the budget were still
/// charged only after `read_merge_entity`'s full load-and-parse (the
/// pre-fix ordering), this merge would fail with a JSON parse error
/// instead of a budget error, because the parse would run before the
/// stale post-read charge was ever reached. Charging from the pre-parse
/// length probe must reject on budget first, so `serde_json::from_str`
/// never runs on this column at all.
#[tokio::test]
async fn merge_entity_rejects_byte_budget_before_parsing_oversized_malformed_properties() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", None, None, vec![])
        .await
        .unwrap();

    let huge_malformed_properties = format!("{{not valid json: {}", "x".repeat(8192));
    let pool = rt.backend().pool_arc();
    let from_id = from.id;
    tokio::task::spawn_blocking(move || {
        let guard = pool.writer().unwrap();
        guard.transaction(|conn| {
            conn.execute(
                "UPDATE entities SET version = version + 1, properties = ?1 WHERE id = ?2",
                rusqlite::params![huge_malformed_properties, from_id.to_string()],
            )?;
            Ok(())
        })
    })
    .await
    .unwrap()
    .unwrap();

    let error = run_entity_merge_with_limits(
        &rt,
        into.id,
        from.id,
        MergeTxLimits {
            max_rows: usize::MAX,
            max_bytes: 4096,
        },
    )
    .await
    .unwrap_err();
    let msg = error.to_string();
    assert!(
        msg.contains("merge transaction budget exceeded"),
        "expected an early budget rejection, not a JSON parse failure; got: {msg}"
    );
    assert!(msg.contains("reading merge records"), "got: {msg}");

    // `get_entity` would itself fail to parse the malformed properties this
    // test deliberately stored, so check survival via a raw row count
    // instead of the parsing read path.
    let pool = rt.backend().pool_arc();
    let still_present: i64 = tokio::task::spawn_blocking(move || {
        let guard = pool.writer().unwrap();
        guard.transaction(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM entities WHERE id = ?1 AND deleted_at IS NULL",
                rusqlite::params![from_id.to_string()],
                |row| row.get(0),
            )
            .map_err(SqliteError::Rusqlite)
        })
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        still_present, 1,
        "from-entity must survive a budget-rejected merge"
    );
}

#[tokio::test]
async fn merge_note_rejects_row_budget_while_collecting_incident_edges() {
    use khive_storage::EdgeRelation;
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_note(&tok, "observation", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_note(&tok, "observation", None, "From", None, None, vec![])
        .await
        .unwrap();
    for name in ["T1", "T2", "T3"] {
        let target = rt
            .create_entity(&tok, "concept", None, name, None, None, vec![])
            .await
            .unwrap();
        rt.link(&tok, from.id, target.id, EdgeRelation::Annotates, 1.0, None)
            .await
            .unwrap();
    }

    let error = run_note_merge_with_limits(
        &rt,
        into.id,
        from.id,
        rt.pack_edge_rules(),
        MergeTxLimits {
            max_rows: 4,
            max_bytes: usize::MAX,
        },
    )
    .await
    .unwrap_err();
    let msg = error.to_string();
    assert!(
        msg.contains("merge transaction budget exceeded"),
        "got: {msg}"
    );
    assert!(msg.contains("collecting incident edges"), "got: {msg}");

    let edges = rt
        .list_edges(
            &tok,
            EdgeListFilter {
                source_id: Some(from.id),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(
        edges.len(),
        3,
        "every incident edge must survive a budget-rejected merge"
    );
}

// The post-commit budget logs are captured by the process-global tracing
// subscriber owned by `crate::pack::tests` — one test binary supports at
// most one `set_global_default`, and a thread-local `set_default` guard
// here proved lossy under parallel tests (the same event-loss class the
// pack tests' subscriber documents). Each test selects its own rows from
// the append-only sink by the merge's `into_id`.
use crate::pack::tests::budget_log_events;

#[tokio::test]
async fn merge_entity_reports_and_logs_tx_budget_after_commit() {
    use khive_storage::EdgeRelation;
    let events = budget_log_events();

    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_entity(&tok, "concept", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", None, None, vec![])
        .await
        .unwrap();
    let target = rt
        .create_entity(&tok, "concept", None, "Target", None, None, vec![])
        .await
        .unwrap();
    rt.link(&tok, from.id, target.id, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();

    // A dry run reports the same predictive budget usage but must not
    // emit the post-commit log: nothing committed.
    let preview = rt
        .merge_entity(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            true,
        )
        .await
        .unwrap();
    assert!(preview.tx_budget.rows_charged >= 2);
    assert_eq!(preview.tx_budget.max_rows, MERGE_TX_MAX_ROWS);
    assert_eq!(preview.tx_budget.max_bytes, MERGE_TX_MAX_BYTES);
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .all(|e| e.into_id != into.id.to_string()),
        "a dry-run preview must not emit the post-commit budget log"
    );

    let summary = rt
        .merge_entity(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();
    assert!(summary.tx_budget.rows_charged >= 2);
    assert!(summary.tx_budget.bytes_charged > 0);

    let captured = events.lock().unwrap();
    let row = captured
        .iter()
        .find(|e| {
            e.into_id == summary.kept_id.to_string()
                && e.message == "merge_entity: transaction materialization budget"
        })
        .expect("committing entity merge must emit the post-commit budget log");
    assert_eq!(
        row.budget_rows as usize, summary.tx_budget.rows_charged,
        "the log must carry the same observed row count the summary reports"
    );
}

#[tokio::test]
async fn merge_note_reports_and_logs_tx_budget_after_commit() {
    let events = budget_log_events();

    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_note(&tok, "observation", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_note(&tok, "observation", None, "From", None, None, vec![])
        .await
        .unwrap();

    let summary = rt
        .merge_note(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();
    assert!(summary.tx_budget.rows_charged >= 2);
    assert!(summary.tx_budget.bytes_charged > 0);

    let captured = events.lock().unwrap();
    let row = captured
        .iter()
        .find(|e| {
            e.into_id == summary.kept_id.to_string()
                && e.message == "merge_note: transaction materialization budget"
        })
        .expect("committing note merge must emit the post-commit budget log");
    assert_eq!(
        row.budget_rows as usize, summary.tx_budget.rows_charged,
        "the log must carry the same observed row count the summary reports"
    );
}

// ── Universal reserved-key reservation (ADR-115 Amendment 1, first rung) ──

fn reserved_key_props() -> serde_json::Value {
    serde_json::json!({"khive:secret_gate": "exempted:content-sha256-manifest-v1"})
}

#[tokio::test]
async fn outbound_property_replacements_refuse_carried_reserved_key() {
    let rt = rt();
    rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let token = NamespaceToken::local();

    for owner_path in [false, true] {
        let mut note = outbound_message_note();
        note.properties = Some(serde_json::json!({
            "direction": "outbound",
            "khive:secret_gate": "exempted:content-sha256-manifest-v1"
        }));
        let id = note.id;
        rt.raw_notes(&token)
            .unwrap()
            .upsert_note(note)
            .await
            .unwrap();
        let before = rt
            .raw_notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap();

        let error = if owner_path {
            rt.claim_outbound_message_external_id(&token, id, "<message@example.com>".into())
                .await
                .expect_err("owner claim must refuse the carried reserved key")
        } else {
            rt.mark_outbound_message_delivered(&token, id, "2026-09-28T00:00:00Z".into(), None)
                .await
                .expect_err("delivery outcome must refuse the carried reserved key")
        };
        assert!(
            matches!(error, RuntimeError::InvalidInput(ref message) if message.contains("khive:secret_gate")),
            "unexpected error: {error:?}"
        );
        let after = rt
            .raw_notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(after).unwrap(),
            serde_json::to_value(before).unwrap()
        );
    }
}

#[tokio::test]
async fn update_entity_rejects_reserved_secret_gate_key() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "reservation-target-entity",
            None,
            Some(serde_json::json!({"k": "v"})),
            vec![],
        )
        .await
        .unwrap();

    let err = rt
        .update_entity(
            &tok,
            entity.id,
            EntityPatch {
                properties: Some(reserved_key_props()),
                ..Default::default()
            },
        )
        .await
        .expect_err("caller-supplied reserved key must be rejected on patch update");
    assert!(
        matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("khive:secret_gate")),
        "unexpected error: {err:?}"
    );

    // No partial mutation: the original properties must be unchanged.
    let unchanged = rt
        .entities(&tok)
        .unwrap()
        .get_entity(entity.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.properties, Some(serde_json::json!({"k": "v"})));
}

#[tokio::test]
async fn persist_prepared_entity_update_rejects_reserved_final_object() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "reserved-final-object",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let mut prepared = entity.clone();
    prepared.properties = Some(reserved_key_props());

    let error = rt
        .persist_prepared_entity_update(
            &tok,
            prepared,
            false,
            vec!["properties"],
            entity.updated_at,
            entity.deleted_at,
            None,
        )
        .await
        .expect_err("the persistence boundary must reject a reserved final property");
    assert!(
        matches!(error, RuntimeError::InvalidInput(ref message) if message.contains("khive:secret_gate")),
        "unexpected error: {error:?}"
    );
    let unchanged = rt.get_entity(&tok, entity.id).await.unwrap();
    assert_eq!(
        serde_json::to_value(unchanged).unwrap(),
        serde_json::to_value(entity).unwrap()
    );
    assert!(entity_update_events(&rt, &tok).await.is_empty());
}

#[tokio::test]
async fn update_note_rejects_reserved_secret_gate_key() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let note = rt
        .create_note(
            &tok,
            "observation",
            None,
            "reservation target note",
            None,
            Some(serde_json::json!({"k": "v"})),
            vec![],
        )
        .await
        .unwrap();

    let err = rt
        .update_note(
            &tok,
            note.id,
            NotePatch::new(None, None, None, None, Some(reserved_key_props())),
        )
        .await
        .expect_err("caller-supplied reserved key must be rejected on patch update");
    assert!(
        matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("khive:secret_gate")),
        "unexpected error: {err:?}"
    );

    let unchanged = rt
        .notes(&tok)
        .unwrap()
        .get_note(note.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.properties, Some(serde_json::json!({"k": "v"})));
}

// -----------------------------------------------------------------
// #2943: prepare_guarded_entity_update dispatches an installed
// entity-kind KindHook against the post-merge properties.
// -----------------------------------------------------------------

/// Test-only `KindHook` whose `validate_entity_update` refuses unless
/// `properties.ok == true`. Proves `prepare_guarded_entity_update`
/// actually dispatches to whichever hook `entity_kind_hook` resolves
/// for the entity's kind, and that the properties it sees are the
/// MERGED (post-patch) value rather than the caller's raw patch.
///
/// Mutation prediction: removing the dispatch call this test exercises
/// (the `if let Some(hook) = self.entity_kind_hook(...)` block added at
/// the seam) makes `entity_update_dispatches_installed_kind_hook_refusal`
/// fail — the refusing hook never runs, so the update that should be
/// refused instead succeeds and `expect_err` panics.
#[derive(Debug, Default)]
struct RefusingKindHook;

#[async_trait::async_trait]
impl crate::pack::KindHook for RefusingKindHook {
    async fn prepare_create(
        &self,
        _runtime: &KhiveRuntime,
        _args: &mut Value,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn validate_entity_update(
        &self,
        _runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        _entity: &Entity,
        properties: Option<&Value>,
    ) -> Result<(), RuntimeError> {
        let ok = properties
            .and_then(Value::as_object)
            .and_then(|p| p.get("ok"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if ok {
            Ok(())
        } else {
            Err(RuntimeError::InvalidInput(
                "widget update requires properties.ok == true".into(),
            ))
        }
    }
}

/// Test-only `KindHook` implementing only the one required method, so
/// its entity-update path is the trait's inherited default. Proves the
/// default is a default (issue #2943 acceptance item 5): a kind can
/// register a hook for `create` without that hook opting into
/// update-time validation, and a generic entity `update` must still
/// succeed.
///
/// Mutation prediction: if the trait default stopped returning `Ok(())`
/// (e.g. it were changed to re-run `prepare_create`-shaped logic),
/// `entity_update_with_hook_missing_the_default_method_still_succeeds`
/// fails, because this hook has nothing else to satisfy any such check.
#[derive(Debug, Default)]
struct SilentKindHook;

#[async_trait::async_trait]
impl crate::pack::KindHook for SilentKindHook {
    async fn prepare_create(
        &self,
        _runtime: &KhiveRuntime,
        _args: &mut Value,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}

#[tokio::test]
async fn entity_update_dispatches_installed_kind_hook_refusal() {
    let rt = rt();
    rt.install_entity_kind_hooks(vec![(
        "widget".to_string(),
        Arc::new(RefusingKindHook) as Arc<dyn crate::pack::KindHook>,
    )]);
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(
            &tok,
            "widget",
            None,
            "Gadget",
            None,
            Some(serde_json::json!({"ok": true})),
            vec![],
        )
        .await
        .unwrap();

    let error = rt
        .update_entity(
            &tok,
            entity.id,
            EntityPatch {
                properties: Some(serde_json::json!({"ok": false})),
                ..Default::default()
            },
        )
        .await
        .expect_err("installed hook must refuse the merged properties");
    assert!(
        matches!(error, RuntimeError::InvalidInput(ref msg) if msg.contains("requires properties.ok")),
        "unexpected error: {error:?}"
    );
    let unchanged = rt.get_entity(&tok, entity.id).await.unwrap();
    assert_eq!(
        unchanged.properties, entity.properties,
        "a refused update must not mutate storage"
    );
}

#[tokio::test]
async fn entity_update_dispatches_installed_kind_hook_acceptance() {
    let rt = rt();
    rt.install_entity_kind_hooks(vec![(
        "widget".to_string(),
        Arc::new(RefusingKindHook) as Arc<dyn crate::pack::KindHook>,
    )]);
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(
            &tok,
            "widget",
            None,
            "Gadget",
            None,
            Some(serde_json::json!({"ok": false})),
            vec![],
        )
        .await
        .unwrap();

    let updated = rt
        .update_entity(
            &tok,
            entity.id,
            EntityPatch {
                properties: Some(serde_json::json!({"ok": true})),
                ..Default::default()
            },
        )
        .await
        .expect("installed hook accepts a merged properties value satisfying its check");
    assert_eq!(updated.properties, Some(serde_json::json!({"ok": true})));
}

#[tokio::test]
async fn entity_update_with_hook_missing_the_default_method_still_succeeds() {
    let rt = rt();
    rt.install_entity_kind_hooks(vec![(
        "widget".to_string(),
        Arc::new(SilentKindHook) as Arc<dyn crate::pack::KindHook>,
    )]);
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(&tok, "widget", None, "Gadget", None, None, vec![])
        .await
        .unwrap();

    let updated = rt
        .update_entity(
            &tok,
            entity.id,
            EntityPatch {
                name: Some("Renamed Gadget".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("a hook that does not override validate_entity_update must not block the update");
    assert_eq!(updated.name, "Renamed Gadget");
}

/// A kind with no installed hook at all (the pre-#2943 behaviour) must
/// still update freely — `entity_kind_hook` returns `None` and the
/// dispatch site's `if let Some(hook) = ...` is skipped entirely.
#[tokio::test]
async fn entity_update_with_no_installed_hook_for_kind_succeeds() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let entity = rt
        .create_entity(&tok, "concept", None, "Plain", None, None, vec![])
        .await
        .unwrap();

    let updated = rt
        .update_entity(
            &tok,
            entity.id,
            EntityPatch {
                name: Some("Plain Renamed".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("no hook installed for this kind must not block the update");
    assert_eq!(updated.name, "Plain Renamed");
}

/// Search only complete edge preimage objects, regardless of which merge
/// payload field groups a cascade. This lets the regression specify the
/// audit contract without prescribing a new field name before the fix.
fn merge_payload_has_edge_preimage(value: &Value, id: Uuid) -> bool {
    let id_string = id.to_string();
    match value {
        Value::Object(object) => {
            (object.get("id").and_then(Value::as_str) == Some(id_string.as_str())
                && object.contains_key("source_id")
                && object.contains_key("target_id"))
                || object
                    .values()
                    .any(|child| merge_payload_has_edge_preimage(child, id))
        }
        Value::Array(items) => items
            .iter()
            .any(|child| merge_payload_has_edge_preimage(child, id)),
        _ => false,
    }
}

#[tokio::test]
async fn committed_entity_merge_reports_reindex_failure_with_event_preimages() {
    use crate::operations::arm_fts_fail_scoped;

    let rt = rt();
    rt.register_embedder(MergeTestVecProvider::new("entity-merge-reindex-failure", 4));
    let namespace = format!("entity-merge-reindex-{}", Uuid::new_v4().as_simple());
    let tok = NamespaceToken::for_namespace(crate::Namespace::parse(&namespace).unwrap());
    let into = rt
        .create_entity(&tok, "concept", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", None, "From", None, None, vec![])
        .await
        .unwrap();
    let dropped = rt
        .link(&tok, into.id, from.id, EdgeRelation::Extends, 0.8, None)
        .await
        .unwrap();

    let _arm = arm_fts_fail_scoped(&namespace);
    let outcome = rt
        .merge_entity(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await;
    let events = rt
        .events(&tok)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![EventKind::EntityMerged],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        events.items.len(),
        1,
        "the merge event committed with the row"
    );
    assert!(merge_payload_has_edge_preimage(
        &events.items[0].payload,
        Uuid::from(dropped.id),
    ));
    let summary = outcome.expect("a committed merge must return its summary after reindex failure");
    assert_eq!(summary.kept_id, into.id);
    assert_eq!(summary.removed_id, from.id);
    assert!(
        summary
            .post_commit_reindex_error
            .as_deref()
            .is_some_and(|error| error.contains("injected FTS failure")),
        "the committed summary must surface the post-commit error: {:?}",
        summary.post_commit_reindex_error
    );
    assert!(rt
        .entities(&tok)
        .unwrap()
        .get_entity_including_deleted(from.id)
        .await
        .unwrap()
        .unwrap()
        .deleted_at
        .is_some());
}

#[tokio::test]
async fn committed_entity_update_records_event_before_reindex_failure() {
    use crate::operations::arm_fts_fail_scoped;

    let rt = rt();
    rt.register_embedder(MergeTestVecProvider::new(
        "entity-update-reindex-failure",
        4,
    ));
    let namespace = format!("entity-update-reindex-{}", Uuid::new_v4().as_simple());
    let tok = NamespaceToken::for_namespace(crate::Namespace::parse(&namespace).unwrap());
    let entity = rt
        .create_entity(&tok, "concept", None, "Before", None, None, vec![])
        .await
        .unwrap();

    let _arm = arm_fts_fail_scoped(&namespace);
    let error = rt
        .update_entity(
            &tok,
            entity.id,
            EntityPatch {
                name: Some("After".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect_err("the injected reindex error must be surfaced");
    assert!(error.to_string().contains("injected FTS failure"));
    assert_eq!(rt.get_entity(&tok, entity.id).await.unwrap().name, "After");
    let events = rt
        .events(&tok)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![EventKind::EntityUpdated],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        events.items.len(),
        1,
        "the committed update must retain its event"
    );
    assert_eq!(events.items[0].target_id, Some(entity.id));
}

async fn assert_merge_removed_edges_and_audited_preimages(
    rt: &KhiveRuntime,
    tok: &NamespaceToken,
    kind: EventKind,
    edge_ids: &[Uuid],
) {
    for id in edge_ids {
        assert!(
            rt.get_edge_including_deleted(tok, *id)
                .await
                .unwrap()
                .is_none(),
            "merge left a dangling edge row for {id}"
        );
    }
    let events = rt
        .events(tok)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![kind],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(events.items.len(), 1, "one committed merge event");
    for id in edge_ids {
        assert!(
            merge_payload_has_edge_preimage(&events.items[0].payload, *id),
            "merge event omitted the deleted edge's complete preimage: {id}"
        );
    }
}

#[tokio::test]
async fn entity_merge_cascades_annotated_self_loop_and_contract_drop_with_preimages() {
    let rt = rt();
    let tok = NamespaceToken::local();
    rt.install_edge_rules(vec![EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: "theorem",
        },
        target: EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: "definition",
        },
    }]);
    let definition = rt
        .create_entity(
            &tok,
            "concept",
            Some("definition"),
            "Def",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let from = rt
        .create_entity(&tok, "concept", Some("theorem"), "From", None, None, vec![])
        .await
        .unwrap();
    let into = rt
        .create_entity(&tok, "concept", Some("lemma"), "Into", None, None, vec![])
        .await
        .unwrap();
    let annotator = rt
        .create_note(&tok, "observation", None, "edge review", None, None, vec![])
        .await
        .unwrap();

    let self_loop = rt
        .link(&tok, into.id, from.id, EdgeRelation::Extends, 0.6, None)
        .await
        .unwrap();
    let self_loop_annotation = rt
        .link(
            &tok,
            annotator.id,
            self_loop.id.into(),
            EdgeRelation::Annotates,
            1.0,
            None,
        )
        .await
        .unwrap();
    let contract_drop = rt
        .link(
            &tok,
            from.id,
            definition.id,
            EdgeRelation::DependsOn,
            0.7,
            None,
        )
        .await
        .unwrap();
    let contract_annotation = rt
        .link(
            &tok,
            annotator.id,
            contract_drop.id.into(),
            EdgeRelation::Annotates,
            1.0,
            None,
        )
        .await
        .unwrap();

    let summary = rt
        .merge_entity(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();
    assert_eq!(summary.edges_self_loop_dropped, 1);
    assert_eq!(summary.edges_contract_skipped, 1);
    assert_merge_removed_edges_and_audited_preimages(
        &rt,
        &tok,
        EventKind::EntityMerged,
        &[
            self_loop.id.into(),
            self_loop_annotation.id.into(),
            contract_drop.id.into(),
            contract_annotation.id.into(),
        ],
    )
    .await;
}

#[tokio::test]
async fn note_merge_cascades_annotated_self_loop_and_contract_drop_with_preimages() {
    let rt = rt();
    let tok = NamespaceToken::local();
    rt.install_edge_rules(vec![EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::NoteOfKind("observation"),
        target: EndpointKind::EntityOfKind("concept"),
    }]);
    let into = rt
        .create_note(&tok, "observation", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_note(&tok, "observation", None, "From", None, None, vec![])
        .await
        .unwrap();
    let target = rt
        .create_entity(&tok, "concept", None, "Legacy target", None, None, vec![])
        .await
        .unwrap();
    let annotator = rt
        .create_note(&tok, "observation", None, "edge review", None, None, vec![])
        .await
        .unwrap();
    let self_loop = rt
        .link(&tok, into.id, from.id, EdgeRelation::Refutes, 0.6, None)
        .await
        .unwrap();
    let self_loop_annotation = rt
        .link(
            &tok,
            annotator.id,
            self_loop.id.into(),
            EdgeRelation::Annotates,
            1.0,
            None,
        )
        .await
        .unwrap();
    let contract_drop = rt
        .link(&tok, from.id, target.id, EdgeRelation::DependsOn, 0.7, None)
        .await
        .unwrap();
    let contract_annotation = rt
        .link(
            &tok,
            annotator.id,
            contract_drop.id.into(),
            EdgeRelation::Annotates,
            1.0,
            None,
        )
        .await
        .unwrap();

    // Seed a legacy dangling endpoint without invoking the public hard
    // delete cascade. The note rewire must take its contract-drop arm.
    let mut writer = rt.sql().writer().await.unwrap();
    assert_eq!(
        writer
            .execute(khive_storage::SqlStatement {
                sql: "DELETE FROM entities WHERE id = ?1".into(),
                params: vec![SqlValue::Text(target.id.to_string())],
                label: Some("seed-dangling-merge-endpoint".into()),
            })
            .await
            .unwrap(),
        1
    );
    drop(writer);

    let summary = rt
        .merge_note(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();
    assert_eq!(summary.edges_self_loop_dropped, 1);
    assert_eq!(summary.edges_contract_skipped, 1);
    assert_merge_removed_edges_and_audited_preimages(
        &rt,
        &tok,
        EventKind::NoteMerged,
        &[
            self_loop.id.into(),
            self_loop_annotation.id.into(),
            contract_drop.id.into(),
            contract_annotation.id.into(),
        ],
    )
    .await;
}

#[tokio::test]
async fn note_merge_dry_run_does_not_recapture_an_earlier_planned_cascade() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let into = rt
        .create_note(&tok, "observation", None, "Into", None, None, vec![])
        .await
        .unwrap();
    let from = rt
        .create_note(&tok, "observation", None, "From", None, None, vec![])
        .await
        .unwrap();
    let annotator = rt
        .create_note(&tok, "observation", None, "Review", None, None, vec![])
        .await
        .unwrap();
    let self_loop = rt
        .link(&tok, into.id, from.id, EdgeRelation::Refutes, 0.7, None)
        .await
        .unwrap();
    let survivor_annotation = rt
        .link(
            &tok,
            into.id,
            self_loop.id.into(),
            EdgeRelation::Annotates,
            0.8,
            None,
        )
        .await
        .unwrap();
    let duplicate_annotation = rt
        .link(
            &tok,
            from.id,
            self_loop.id.into(),
            EdgeRelation::Annotates,
            0.6,
            None,
        )
        .await
        .unwrap();
    let nested_annotation = rt
        .link(
            &tok,
            annotator.id,
            duplicate_annotation.id.into(),
            EdgeRelation::Annotates,
            0.5,
            None,
        )
        .await
        .unwrap();
    rt.delete_edge(&tok, nested_annotation.id.into(), false)
        .await
        .unwrap();

    let dry = rt
        .merge_note(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            true,
        )
        .await
        .unwrap();
    let committed = rt
        .merge_note(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();

    assert_eq!(dry.edges_self_loop_dropped, 1);
    assert_eq!(
        dry.self_loop_edge_preimages,
        committed.self_loop_edge_preimages
    );
    assert_eq!(
        dry.self_loop_incident_edge_preimages,
        committed.self_loop_incident_edge_preimages
    );
    assert_eq!(
        dry.edge_conflict_preimages,
        committed.edge_conflict_preimages
    );
    assert_eq!(
        dry.self_loop_incident_edge_preimages
            .iter()
            .map(|edge| edge.id)
            .collect::<Vec<_>>(),
        vec![Uuid::from(survivor_annotation.id)],
        "the earlier conflict already planned the duplicate annotation's deletion"
    );
    let [conflict] = dry.edge_conflict_preimages.as_slice() else {
        panic!("one annotation conflict expected");
    };
    assert_eq!(
        conflict.dropped_edge.id,
        Uuid::from(duplicate_annotation.id)
    );
    assert_eq!(
        conflict
            .incident_edge_preimages
            .iter()
            .map(|edge| edge.id)
            .collect::<Vec<_>>(),
        vec![Uuid::from(nested_annotation.id)]
    );
    assert_merge_removed_edges_and_audited_preimages(
        &rt,
        &tok,
        EventKind::NoteMerged,
        &[
            self_loop.id.into(),
            survivor_annotation.id.into(),
            duplicate_annotation.id.into(),
            nested_annotation.id.into(),
        ],
    )
    .await;
}
