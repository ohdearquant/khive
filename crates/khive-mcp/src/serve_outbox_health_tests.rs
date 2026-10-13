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

    let server = KhiveMcpServer::new(runtime).unwrap();
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
    let server = KhiveMcpServer::new(runtime).unwrap();
    let health: Value = server
        .verb_registry_clone()
        .dispatch("comm.health", json!({}))
        .await
        .unwrap();
    assert!(health["channels"][0]["outbox"]["oldest_pending_age_secs"]
        .as_u64()
        .is_some());
}
