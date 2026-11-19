use super::outbox_parity_tests::{fixture, seed, Outcome, RecordingChannel};
use super::*;
use chrono::{DateTime, Utc};
use khive_channel::ChannelRegistry;
use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken};
use khive_storage::note::{Note, NoteFilter};
use khive_storage::types::{PageRequest, SqlStatement, SqlValue};
use serde_json::{json, Value};

async fn pass(
    runtime: &KhiveRuntime,
    channels: &ChannelRegistry,
    kind: &str,
    pause: &mut Option<tokio::time::Instant>,
) -> Result<(), crate::components::ComponentError> {
    let slug = "sender@example.com";
    let domains = khive_runtime::EmailMessageIdDomains::from_mailbox_and_history(slug, "").unwrap();
    let policy = if kind == "email" {
        outbox::OutboxPolicy::Email {
            mailbox: slug,
            domains: &domains,
        }
    } else {
        outbox::OutboxPolicy::Telegram(std::marker::PhantomData)
    };
    outbox::outbox_once(
        outbox::OutboxChannels::Registered {
            registry: channels,
            slug,
        },
        policy,
        runtime,
        &Namespace::local(),
        &tokio_util::sync::CancellationToken::new(),
        pause,
    )
    .await
}

async fn health_note(runtime: &KhiveRuntime, token: &NamespaceToken) -> Note {
    let page = runtime
        .notes(token)
        .unwrap()
        .query_notes_filtered_count_free(
            token.namespace().as_str(),
            &NoteFilter {
                kind: Some("channel_health".into()),
                ..Default::default()
            },
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    page.items.into_iter().next().unwrap()
}

fn channels(kind: &'static str, outcome: Outcome) -> ChannelRegistry {
    let mut channels = ChannelRegistry::new();
    channels.register(Arc::new(RecordingChannel::new(
        kind,
        "sender@example.com",
        outcome,
    )));
    channels
}

#[tokio::test]
async fn empty_real_outbox_scans_advance_completed_and_success_times_for_both_transports() {
    for kind in ["email", "telegram"] {
        let (runtime, token) = fixture();
        let channels = channels(kind, Outcome::Success);
        pass(&runtime, &channels, kind, &mut None).await.unwrap();
        let first = health_note(&runtime, &token).await;
        let mut aged = first.clone();
        let prior = (Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
        let props = aged.properties.as_mut().unwrap();
        props["outbox"]["last_scan_completed_at"] = json!(prior);
        props["outbox"]["last_success_at"] = json!(prior);
        runtime
            .notes(&token)
            .unwrap()
            .upsert_note(aged)
            .await
            .unwrap();
        pass(&runtime, &channels, kind, &mut None).await.unwrap();
        let after = health_note(&runtime, &token).await;
        let props = after.properties.unwrap();
        assert_eq!(props["channel_kind"], kind);
        assert_eq!(props["channel_slug"], "sender@example.com");
        assert_eq!(props["outbox"]["pending_rows_seen"], 0);
        assert_eq!(props["outbox"]["pending_rows_limit"], 200);
        assert!(props["outbox"]["oldest_pending_created_at"].is_null());
        assert_eq!(props["outbox"]["last_scan_succeeded"], true);
        assert_eq!(
            props["outbox"]["last_scan_completed_at"],
            props["outbox"]["last_success_at"]
        );
        assert_ne!(props["outbox"]["last_success_at"], prior);
        assert!(after.updated_at > first.updated_at);
        assert!(props.get("last_poll_attempt_at").is_none());
    }
}

#[tokio::test]
async fn paused_real_scan_keeps_aged_observation_and_health_reports_stalled() {
    let (runtime, token) = fixture();
    let channels = channels("telegram", Outcome::Success);
    pass(&runtime, &channels, "telegram", &mut None)
        .await
        .unwrap();
    let mut note = health_note(&runtime, &token).await;
    // An aged persisted receipt represents time passing without a scan;
    // no wall-clock sleep is required to exercise the actual pause branch.
    note.properties.as_mut().unwrap()["outbox"]["last_scan_completed_at"] =
        json!((Utc::now() - chrono::Duration::seconds(30)).to_rfc3339());
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note)
        .await
        .unwrap();
    let before = health_note(&runtime, &token).await;
    let mut pause = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(60));
    pass(&runtime, &channels, "telegram", &mut pause)
        .await
        .unwrap();
    let after = health_note(&runtime, &token).await;
    assert_eq!(before.properties, after.properties);
    assert_eq!(before.updated_at, after.updated_at);

    let server = KhiveMcpServer::with_packs(runtime, &["kg".into(), "comm".into()]).unwrap();
    let health = server
        .verb_registry_clone()
        .dispatch("comm.health", json!({}))
        .await
        .unwrap();
    assert_eq!(health["channels"][0]["outbox"]["stalled"], true);
    assert!(health["channels"][0]["last_poll_attempt_at"].is_null());
}

#[tokio::test]
async fn failed_real_scan_preserves_success_without_hiding_the_failure() {
    let (runtime, token) = fixture();
    let channels = channels("telegram", Outcome::Success);
    pass(&runtime, &channels, "telegram", &mut None)
        .await
        .unwrap();
    let before = health_note(&runtime, &token).await.properties.unwrap()["outbox"].clone();
    let id = seed(&runtime, &token, "telegram", Some("sender@example.com")).await;
    // Corrupt only this fixture row's numeric column so the real pending
    // query fails decoding; the heartbeat row remains independently writable.
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement {
            sql: "UPDATE notes SET salience = 'invalid-number' WHERE id = ?1".into(),
            params: vec![SqlValue::Text(id.to_string())],
            label: Some("outbox_scan_error_fixture".into()),
        })
        .await
        .unwrap();
    assert!(matches!(
        pass(&runtime, &channels, "telegram", &mut None).await,
        Err(crate::components::ComponentError::Retryable(_))
    ));
    let after = health_note(&runtime, &token).await.properties.unwrap()["outbox"].clone();
    assert_eq!(after["last_success_at"], before["last_success_at"]);
    assert_eq!(after["pending_rows_seen"], before["pending_rows_seen"]);
    for field in [
        "backlog_observed_at",
        "undelivered_rows",
        "backoff_rows",
        "oldest_undelivered_created_at",
        "oldest_backoff_created_at",
        "earliest_backoff_next_attempt_at",
        "max_backoff_delivery_attempts",
    ] {
        assert_eq!(after[field], before[field], "{field}");
    }
    assert_eq!(after["last_scan_succeeded"], false);
    assert_eq!(after["consecutive_failures"], 1);
    assert_eq!(after["last_scan_completed_at"], after["last_failure_at"]);
    assert!(!after["last_error"]["message"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn real_scan_records_the_selected_page_before_failed_delivery() {
    let (runtime, token) = fixture();
    let channels = channels("telegram", Outcome::Transient);
    let id = seed(&runtime, &token, "telegram", Some("sender@example.com")).await;
    let created = runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap()
        .created_at;
    seed(&runtime, &token, "telegram", Some("different@example.com")).await;
    pass(&runtime, &channels, "telegram", &mut None)
        .await
        .unwrap();
    let outbox = health_note(&runtime, &token).await.properties.unwrap()["outbox"].clone();
    assert_eq!(
        outbox["last_scan_succeeded"], true,
        "scan success is independent of send success"
    );
    assert_eq!(outbox["pending_rows_seen"], 1);
    let observed =
        DateTime::parse_from_rfc3339(outbox["oldest_pending_created_at"].as_str().unwrap())
            .unwrap();
    assert_eq!(observed.timestamp_micros(), created);
    let server = KhiveMcpServer::with_packs(runtime, &["kg".into(), "comm".into()]).unwrap();
    let health: Value = server
        .verb_registry_clone()
        .dispatch("comm.health", json!({}))
        .await
        .unwrap();
    assert!(health["channels"][0]["outbox"]["oldest_pending_age_secs"]
        .as_u64()
        .is_some());
}

async fn set_retry(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: uuid::Uuid,
    created: DateTime<Utc>,
    next: DateTime<Utc>,
    attempts: u64,
) {
    let mut note = runtime
        .notes(token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    let props = note.properties.as_mut().unwrap();
    props["next_attempt_at"] = json!(next.to_rfc3339());
    props["delivery_attempts"] = json!(attempts);
    runtime
        .backend()
        .notes_for_namespace(token.namespace().as_str())
        .unwrap()
        .upsert_note(note)
        .await
        .unwrap();

    // Upserts preserve the original creation time. Age this fixture row
    // explicitly while leaving retry-property/index updates on the real path.
    let changed = runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement {
            sql: "UPDATE notes SET created_at = ?1 WHERE id = ?2 AND namespace = ?3".into(),
            params: vec![
                SqlValue::Integer(created.timestamp_micros()),
                SqlValue::Text(id.to_string()),
                SqlValue::Text(token.namespace().as_str().into()),
            ],
            label: Some("outbox_backlog_age_fixture".into()),
        })
        .await
        .unwrap();
    assert_eq!(changed, 1);
    assert_eq!(
        runtime
            .notes(token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap()
            .created_at,
        created.timestamp_micros()
    );
}

async fn note_state(runtime: &KhiveRuntime) -> Value {
    let rows = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement {
            sql: "SELECT * FROM notes ORDER BY id".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    serde_json::to_value(rows).unwrap()
}

#[tokio::test]
async fn future_retry_is_visible_through_real_scan_and_read_only_health_for_both_transports() {
    for kind in ["email", "telegram"] {
        let (runtime, token) = fixture();
        let channel = Arc::new(RecordingChannel::new(
            kind,
            "sender@example.com",
            Outcome::Success,
        ));
        let mut channels = ChannelRegistry::new();
        channels.register(channel.clone());
        let id = seed(&runtime, &token, kind, Some("sender@example.com")).await;
        let created = Utc::now() - chrono::Duration::hours(10);
        let next = Utc::now() + chrono::Duration::hours(1);
        set_retry(&runtime, &token, id, created, next, u64::MAX).await;
        pass(&runtime, &channels, kind, &mut None).await.unwrap();
        assert!(channel.sent.lock().unwrap().is_empty());
        let server =
            KhiveMcpServer::with_packs(runtime.clone(), &["kg".into(), "comm".into()]).unwrap();
        let before = note_state(&runtime).await;
        for _ in 0..3 {
            let health = server
                .verb_registry_clone()
                .dispatch("comm.health", json!({}))
                .await
                .unwrap();
            let outbox = &health["channels"][0]["outbox"];
            assert_eq!(outbox["pending_rows_seen"], 0);
            assert!(outbox["oldest_pending_created_at"].is_null());
            assert_eq!(outbox["undelivered_rows"], 1);
            assert_eq!(outbox["backoff_rows"], 1);
            assert_eq!(outbox["max_backoff_delivery_attempts"], u64::MAX);
            assert_eq!(
                DateTime::parse_from_rfc3339(
                    outbox["earliest_backoff_next_attempt_at"].as_str().unwrap()
                )
                .unwrap(),
                next
            );
            assert_eq!(
                DateTime::parse_from_rfc3339(outbox["oldest_backoff_created_at"].as_str().unwrap())
                    .unwrap()
                    .timestamp_micros(),
                created.timestamp_micros()
            );
            assert!(outbox["oldest_undelivered_age_secs"].as_u64().unwrap() >= 36000);
            assert!(outbox["backlog_observed_at"].is_string());
            assert!(health["channels"][0]["no_success_age_secs"].is_null());
        }
        assert_eq!(note_state(&runtime).await, before);
        // Becoming due changes only the partition; the real delivery loop then sends it.
        set_retry(
            &runtime,
            &token,
            id,
            created,
            Utc::now() - chrono::Duration::seconds(1),
            7,
        )
        .await;
        pass(&runtime, &channels, kind, &mut None).await.unwrap();
        let after = health_note(&runtime, &token).await.properties.unwrap()["outbox"].clone();
        assert_eq!(after["pending_rows_seen"], 1);
        assert_eq!(after["undelivered_rows"], 1);
        assert_eq!(after["backoff_rows"], 0);
        assert!(after["earliest_backoff_next_attempt_at"].is_null());
        assert!(after["max_backoff_delivery_attempts"].is_null());
        assert_eq!(channel.sent.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn real_scan_keeps_due_page_capped_while_counting_future_rows() {
    let (runtime, token) = fixture();
    for _ in 0..201 {
        seed(&runtime, &token, "telegram", Some("sender@example.com")).await;
    }
    let future = seed(&runtime, &token, "telegram", Some("sender@example.com")).await;
    set_retry(
        &runtime,
        &token,
        future,
        Utc::now() - chrono::Duration::days(1),
        Utc::now() + chrono::Duration::days(1),
        10,
    )
    .await;
    let channels = channels("telegram", Outcome::Transient);
    pass(&runtime, &channels, "telegram", &mut None)
        .await
        .unwrap();
    let observed = health_note(&runtime, &token).await.properties.unwrap()["outbox"].clone();
    assert_eq!(observed["pending_rows_seen"], 200);
    assert_eq!(observed["pending_rows_limit"], 200);
    assert_eq!(observed["undelivered_rows"], 202);
    assert_eq!(observed["backoff_rows"], 1);
    assert_eq!(observed["max_backoff_delivery_attempts"], 10);
}

#[tokio::test]
async fn real_scan_excludes_ambiguous_missing_slugs_from_backlog() {
    let (runtime, token) = fixture();
    let mut channels = channels("telegram", Outcome::Success);
    channels.register(Arc::new(RecordingChannel::new(
        "telegram",
        "second@example.com",
        Outcome::Success,
    )));
    for slug in [None, Some("second@example.com"), Some("sender@example.com")] {
        let id = seed(&runtime, &token, "telegram", slug).await;
        set_retry(
            &runtime,
            &token,
            id,
            Utc::now(),
            Utc::now() + chrono::Duration::days(1),
            10,
        )
        .await;
    }
    pass(&runtime, &channels, "telegram", &mut None)
        .await
        .unwrap();
    let observed = health_note(&runtime, &token).await.properties.unwrap()["outbox"].clone();
    assert_eq!(observed["pending_rows_seen"], 0);
    assert_eq!(observed["undelivered_rows"], 1);
    assert_eq!(observed["backoff_rows"], 1);
}

#[tokio::test]
async fn backlog_query_failure_keeps_dated_observation_and_does_not_stop_delivery() {
    let (runtime, token) = fixture();
    let channel = Arc::new(RecordingChannel::new(
        "telegram",
        "sender@example.com",
        Outcome::Success,
    ));
    let mut channels = ChannelRegistry::new();
    channels.register(channel.clone());
    let future = seed(&runtime, &token, "telegram", Some("sender@example.com")).await;
    set_retry(
        &runtime,
        &token,
        future,
        Utc::now(),
        Utc::now() + chrono::Duration::days(1),
        9,
    )
    .await;
    pass(&runtime, &channels, "telegram", &mut None)
        .await
        .unwrap();
    let before = health_note(&runtime, &token).await.properties.unwrap()["outbox"].clone();
    assert_eq!(before["backoff_rows"], 1);
    // This future row stays outside the real due page, but its invalid metadata refuses the aggregate.
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement {
            sql: "UPDATE notes SET created_at='invalid-timestamp' WHERE id=?1".into(),
            params: vec![SqlValue::Text(future.to_string())],
            label: None,
        })
        .await
        .unwrap();
    seed(&runtime, &token, "telegram", Some("sender@example.com")).await;
    pass(&runtime, &channels, "telegram", &mut None)
        .await
        .unwrap();
    assert_eq!(channel.sent.lock().unwrap().len(), 1);
    let after = health_note(&runtime, &token).await.properties.unwrap()["outbox"].clone();
    assert_eq!(after["last_scan_succeeded"], false);
    assert_eq!(after["consecutive_failures"], 1);
    for field in [
        "last_success_at",
        "pending_rows_seen",
        "backlog_observed_at",
        "undelivered_rows",
        "backoff_rows",
        "oldest_undelivered_created_at",
        "oldest_backoff_created_at",
        "earliest_backoff_next_attempt_at",
        "max_backoff_delivery_attempts",
    ] {
        assert_eq!(after[field], before[field], "{field}");
    }
}
