use super::*;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use khive_channel::{Channel, ChannelEnvelope, ChannelError};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
enum Outcome {
    Delivered,
    #[cfg(feature = "channel-email")]
    RefreshableAuth,
    #[cfg(feature = "channel-email")]
    FixedCredentials,
}

struct ScriptedChannel {
    kind: &'static str,
    outcomes: Mutex<VecDeque<Outcome>>,
    sends: AtomicUsize,
    cancel_after_send: Option<CancellationToken>,
    captured: Mutex<Vec<ChannelEnvelope>>,
}

impl ScriptedChannel {
    fn new(kind: &'static str, outcomes: impl IntoIterator<Item = Outcome>) -> Self {
        Self {
            kind,
            outcomes: Mutex::new(outcomes.into_iter().collect()),
            sends: AtomicUsize::new(0),
            cancel_after_send: None,
            captured: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Channel for ScriptedChannel {
    fn kind(&self) -> &'static str {
        self.kind
    }

    async fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        self.captured.lock().unwrap().push(envelope);
        let outcome = self.outcomes.lock().unwrap().pop_front().unwrap();
        if let Some(token) = &self.cancel_after_send {
            token.cancel();
        }
        match outcome {
            Outcome::Delivered => Ok(()),
            #[cfg(feature = "channel-email")]
            Outcome::RefreshableAuth => {
                Err(ChannelError::RetryableAuth("minted token rejected".into()))
            }
            #[cfg(feature = "channel-email")]
            Outcome::FixedCredentials => {
                Err(ChannelError::Auth("configured client refused".into()))
            }
        }
    }

    async fn poll(&self, _since: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
        Ok(Vec::new())
    }
}

async fn fixture(recipient: &str) -> (KhiveMcpServer, KhiveRuntime, uuid::Uuid) {
    fixture_with_policy(recipient, None).await
}

async fn fixture_with_policy(
    recipient: &str,
    email_policy: Option<khive_runtime::OutboundEmailPolicy>,
) -> (KhiveMcpServer, KhiveRuntime, uuid::Uuid) {
    let mut runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: Some("actor:outbox-supervision".into()),
        brain_profile: None,
        packs: vec!["kg".into(), "comm".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    if let Some(policy) = email_policy {
        runtime = runtime.with_outbound_email_policy(policy);
    }
    let server = KhiveMcpServer::new(runtime.clone()).unwrap();
    let sent = server
        .verb_registry_clone()
        .dispatch(
            "comm.send",
            json!({
                "to": recipient,
                "subject": "delivery state",
                "content": "pending supervised delivery"
            }),
        )
        .await
        .unwrap();
    let id = sent["full_id"].as_str().unwrap().parse().unwrap();
    (server, runtime, id)
}

async fn properties(runtime: &KhiveRuntime, id: uuid::Uuid) -> serde_json::Value {
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap()
        .properties
        .unwrap()
}

async fn wait_for_status(
    health: &HealthReporter,
    name: &str,
    predicate: impl Fn(&ComponentStatus) -> bool,
) -> ComponentStatus {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(status) = health.status(name) {
                if predicate(&status) {
                    return status;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("component status: {:?}", health.status(name)))
}

#[cfg(feature = "channel-email")]
fn email_registration(
    runtime: KhiveRuntime,
    channel: Arc<ScriptedChannel>,
) -> ComponentRegistration {
    let runtime = runtime.with_outbound_email_policy(
        khive_runtime::OutboundEmailPolicy::configured(vec!["recipient@example.com".into()])
            .unwrap(),
    );
    email_registration_with_policy(runtime, channel)
}

#[cfg(feature = "channel-email")]
fn email_registration_with_policy(
    runtime: KhiveRuntime,
    channel: Arc<ScriptedChannel>,
) -> ComponentRegistration {
    let mut registration = channel_component_registration(
        "email-outbound",
        Arc::new(move |ctx| {
            Box::pin(crate::serve::channel_outbox_loop(
                channel.clone(),
                runtime.clone(),
                "local".into(),
                "sender@example.com".into(),
                ctx,
            ))
        }),
    );
    assert_eq!(registration.restart, RestartClass::OnFailure);
    assert_eq!(registration.max_restarts, 5);
    registration.max_restarts = 1;
    registration.backoff_initial_ms = 1;
    registration.backoff_max_ms = 1;
    registration
}

#[cfg(feature = "channel-email")]
#[tokio::test]
async fn email_outbound_recovers_after_refreshable_auth_without_daemon_restart() {
    let (server, runtime, id) = fixture("email:recipient@example.com").await;
    let channel = Arc::new(ScriptedChannel::new(
        "email",
        [Outcome::RefreshableAuth, Outcome::Delivered],
    ));
    let health = HealthReporter::default();
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(supervise(
        email_registration(runtime.clone(), channel.clone()),
        server,
        cancellation.clone(),
        health.clone(),
    ));
    let status = wait_for_status(&health, "email-outbound", |s| {
        s.restart_count == 1 && s.last_heartbeat.is_some()
    })
    .await;
    assert_eq!(status.state, ComponentState::Running);
    assert_eq!(status.last_error.as_deref(), Some("minted token rejected"));
    assert_eq!(channel.sends.load(Ordering::SeqCst), 2);
    let props = properties(&runtime, id).await;
    assert_eq!(props["delivery"], "delivered");
    assert!(props["delivered_at"].is_string());
    assert!(props.get("delivery_attempts").is_none());
    cancellation.cancel();
    task.await.unwrap();
    assert_eq!(
        health.status("email-outbound").unwrap().state,
        ComponentState::Stopped
    );
}

#[cfg(feature = "channel-email")]
#[tokio::test]
async fn email_outbound_auth_budget_exhaustion_is_terminal_and_preserves_pending_mail() {
    let (server, runtime, id) = fixture("email:recipient@example.com").await;
    let channel = Arc::new(ScriptedChannel::new(
        "email",
        [Outcome::RefreshableAuth, Outcome::RefreshableAuth],
    ));
    let health = HealthReporter::default();
    let task = tokio::spawn(supervise(
        email_registration(runtime.clone(), channel.clone()),
        server,
        CancellationToken::new(),
        health.clone(),
    ));
    let status = wait_for_status(&health, "email-outbound", |s| {
        s.state == ComponentState::Unhealthy
    })
    .await;
    task.await.unwrap();
    assert_eq!(status.restart_count, 1);
    assert_eq!(status.last_error.as_deref(), Some("minted token rejected"));
    assert_eq!(channel.sends.load(Ordering::SeqCst), 2);
    let props = properties(&runtime, id).await;
    assert!(props.get("delivery").is_none());
    assert!(props.get("delivery_attempts").is_none());
}

#[cfg(feature = "channel-email")]
#[tokio::test]
async fn email_outbound_fixed_credentials_stop_without_consuming_restart_budget() {
    let (server, runtime, id) = fixture("email:recipient@example.com").await;
    let channel = Arc::new(ScriptedChannel::new("email", [Outcome::FixedCredentials]));
    let health = HealthReporter::default();
    let task = tokio::spawn(supervise(
        email_registration(runtime.clone(), channel.clone()),
        server,
        CancellationToken::new(),
        health.clone(),
    ));
    let status = wait_for_status(&health, "email-outbound", |s| {
        s.state == ComponentState::Unhealthy
    })
    .await;
    task.await.unwrap();
    assert_eq!(status.restart_count, 0);
    assert_eq!(
        status.last_error.as_deref(),
        Some("configured client refused")
    );
    assert_eq!(channel.sends.load(Ordering::SeqCst), 1);
    let props = properties(&runtime, id).await;
    assert!(props.get("delivery").is_none());
    assert!(props.get("delivery_attempts").is_none());
}

#[cfg(feature = "channel-telegram")]
#[tokio::test]
async fn telegram_outbound_cancellation_stamps_inflight_delivery_and_stops_before_next_message() {
    let (server, runtime, id) = fixture("telegram:123").await;
    let sent = server
        .verb_registry_clone()
        .dispatch(
            "comm.send",
            json!({"to":"telegram:456", "content":"second"}),
        )
        .await
        .unwrap();
    let second_id = sent["full_id"].as_str().unwrap().parse().unwrap();
    let cancellation = CancellationToken::new();
    let channel = Arc::new(ScriptedChannel {
        cancel_after_send: Some(cancellation.clone()),
        ..ScriptedChannel::new("telegram", [Outcome::Delivered])
    });
    let health = HealthReporter::default();
    let captured_channel = channel.clone();
    let captured_runtime = runtime.clone();
    let registration = channel_component_registration(
        "telegram-outbound",
        Arc::new(move |ctx| {
            Box::pin(crate::serve::telegram_outbox_loop(
                captured_channel.clone(),
                captured_runtime.clone(),
                "local".into(),
                ctx,
            ))
        }),
    );
    let task = tokio::spawn(supervise(
        registration,
        server,
        cancellation,
        health.clone(),
    ));
    let status = wait_for_status(&health, "telegram-outbound", |s| {
        s.state == ComponentState::Stopped
    })
    .await;
    task.await.unwrap();
    assert_eq!(status.restart_count, 0);
    assert_eq!(channel.sends.load(Ordering::SeqCst), 1);
    let outcomes = [
        properties(&runtime, id).await,
        properties(&runtime, second_id).await,
    ];
    assert_eq!(
        outcomes
            .iter()
            .filter(|p| p["delivery"] == "delivered")
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|p| p.get("delivery").is_none())
            .count(),
        1
    );
    assert!(health.status("email-outbound").is_none());
}

#[cfg(feature = "channel-email")]
#[tokio::test]
async fn email_outbound_allowlist_refusal_is_visible_in_sender_sent_mail() {
    let (server, runtime, id) = fixture("email:blocked@example.com").await;
    let channel = Arc::new(ScriptedChannel::new("email", []));
    let health = HealthReporter::default();
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(supervise(
        email_registration(runtime.clone(), channel.clone()),
        server.clone(),
        cancellation.clone(),
        health.clone(),
    ));
    wait_for_status(&health, "email-outbound", |s| s.last_heartbeat.is_some()).await;
    assert_eq!(channel.sends.load(Ordering::SeqCst), 0);
    let sent = server
        .verb_registry_clone()
        .dispatch(
            "comm.inbox",
            json!({
                "box":"sent", "to_actor":"email:blocked@example.com"
            }),
        )
        .await
        .unwrap();
    let messages = sent["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["full_id"], id.to_string());
    assert_eq!(messages[0]["properties"]["delivery"], "failed");
    assert!(messages[0]["properties"]["last_error"]
        .as_str()
        .unwrap()
        .contains("outbound allowlist"));
    let token = runtime.authorize(Namespace::local()).unwrap();
    assert!(runtime
        .list_undelivered_outbound_messages(&token, Some("email:"), 200)
        .await
        .unwrap()
        .is_empty());
    cancellation.cancel();
    task.await.unwrap();
}

#[cfg(feature = "channel-email")]
#[tokio::test]
async fn external_email_suppressed_after_admission_fails_before_smtp() {
    let policy = khive_runtime::OutboundEmailPolicy::configured_for_tests(
        vec!["maintainer@example.com".into()],
        vec!["actor:outbox-supervision".into()],
        5,
    )
    .unwrap();
    let (server, runtime, id) =
        fixture_with_policy("email:external@example.net", Some(policy)).await;
    server
        .verb_registry_clone()
        .dispatch(
            "comm.suppression_add",
            json!({
                "scope":"outbound",
                "kind":"address",
                "value":"external@example.net",
                "source":"operator"
            }),
        )
        .await
        .unwrap();

    let channel = Arc::new(ScriptedChannel::new("email", [Outcome::Delivered]));
    let health = HealthReporter::default();
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(supervise(
        email_registration_with_policy(runtime.clone(), channel.clone()),
        server,
        cancellation.clone(),
        health,
    ));
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let props = properties(&runtime, id).await;
            if props["delivery"] == "failed" {
                assert!(props["last_error"]
                    .as_str()
                    .unwrap()
                    .contains("suppression"));
                assert!(!props["last_error"]
                    .as_str()
                    .unwrap()
                    .contains("external@example.net"));
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("suppressed external message should reach a permanent failure");
    assert_eq!(channel.sends.load(Ordering::SeqCst), 0);
    cancellation.cancel();
    task.await.unwrap();
}

#[cfg(feature = "channel-email")]
#[tokio::test]
async fn external_email_carries_list_unsubscribe_header_and_footer() {
    let policy = khive_runtime::OutboundEmailPolicy::configured_for_tests(
        vec!["maintainer@example.com".into()],
        vec!["actor:outbox-supervision".into()],
        5,
    )
    .unwrap();
    let (server, runtime, id) =
        fixture_with_policy("email:external@example.net", Some(policy.clone())).await;
    let channel = Arc::new(ScriptedChannel::new("email", [Outcome::Delivered]));
    let health = HealthReporter::default();
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(supervise(
        email_registration_with_policy(runtime.clone(), channel.clone()),
        server,
        cancellation.clone(),
        health,
    ));
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if properties(&runtime, id).await["delivery"] == "delivered" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("external message should be delivered");
    cancellation.cancel();
    task.await.unwrap();

    let captured = channel.captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    let envelope = &captured[0];
    let header = envelope
        .list_unsubscribe
        .as_deref()
        .expect("external message must carry List-Unsubscribe");
    assert!(header.starts_with("<mailto:"), "{header}");
    assert!(!header.contains("List-Unsubscribe-Post"), "{header}");
    let token = header
        .rsplit("unsubscribe%20")
        .next()
        .and_then(|value| value.strip_suffix('>'))
        .expect("header must carry the token");
    assert_eq!(
        policy.verify_unsubscribe_token(token).as_deref(),
        policy.recipient_key("external@example.net").as_deref()
    );
    assert!(
        envelope
            .content
            .ends_with(&format!(
                "To unsubscribe, send an email to sender@example.com with the subject: unsubscribe {token}"
            )),
        "body must end with the rendered footer: {}",
        envelope.content
    );
    assert!(!envelope.content.contains("{unsubscribe}"));
}

#[cfg(feature = "channel-email")]
#[tokio::test]
async fn policy_recipient_email_carries_no_unsubscribe_header_or_footer() {
    let policy = khive_runtime::OutboundEmailPolicy::configured_for_tests(
        vec!["maintainer@example.com".into()],
        vec!["actor:outbox-supervision".into()],
        5,
    )
    .unwrap();
    let (server, runtime, id) =
        fixture_with_policy("email:maintainer@example.com", Some(policy)).await;
    let channel = Arc::new(ScriptedChannel::new("email", [Outcome::Delivered]));
    let health = HealthReporter::default();
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(supervise(
        email_registration_with_policy(runtime.clone(), channel.clone()),
        server,
        cancellation.clone(),
        health,
    ));
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if properties(&runtime, id).await["delivery"] == "delivered" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("policy-recipient message should be delivered");
    cancellation.cancel();
    task.await.unwrap();

    let captured = channel.captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert!(captured[0].list_unsubscribe.is_none());
    assert!(!captured[0].content.contains("{unsubscribe}"));
}

#[cfg(feature = "channel-email")]
#[tokio::test]
async fn external_email_delivered_under_unavailable_footer_fails_before_smtp() {
    let admitted = khive_runtime::OutboundEmailPolicy::configured_for_tests(
        vec!["maintainer@example.com".into()],
        vec!["actor:outbox-supervision".into()],
        5,
    )
    .unwrap();
    let (server, runtime, id) =
        fixture_with_policy("email:external@example.net", Some(admitted)).await;
    // The message is now queued; deliver it under a policy whose footer has become unavailable.
    let runtime = runtime.with_outbound_email_policy(
        khive_runtime::OutboundEmailPolicy::configured_for_tests(
            vec!["maintainer@example.com".into()],
            vec!["actor:outbox-supervision".into()],
            5,
        )
        .unwrap()
        .with_external_footer_for_tests(Err("KHIVE_EMAIL_EXTERNAL_FOOTER_FILE could not be read")),
    );
    let channel = Arc::new(ScriptedChannel::new("email", [Outcome::Delivered]));
    let health = HealthReporter::default();
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(supervise(
        email_registration_with_policy(runtime.clone(), channel.clone()),
        server,
        cancellation.clone(),
        health,
    ));
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let props = properties(&runtime, id).await;
            if props["delivery"] == "failed" {
                let last_error = props["last_error"].as_str().unwrap();
                assert!(last_error.contains("footer is unavailable"), "{last_error}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("unavailable footer must reach a permanent failure");
    assert_eq!(channel.sends.load(Ordering::SeqCst), 0);
    cancellation.cancel();
    task.await.unwrap();
}

#[cfg(all(feature = "channel-email", feature = "channel-telegram"))]
#[tokio::test]
async fn mismatched_outbox_kind_is_permanent_before_heartbeat() {
    for kind in ["email", "telegram"] {
        let (server, runtime, _) = fixture(if kind == "email" {
            "email:recipient@example.com"
        } else {
            "telegram:123"
        })
        .await;
        let channel = Arc::new(ScriptedChannel::new("outbox-test", []));
        let health = HealthReporter::default();
        let captured_channel = channel.clone();
        let captured_runtime = runtime.clone().with_outbound_email_policy(
            khive_runtime::OutboundEmailPolicy::configured(vec!["recipient@example.com".into()])
                .unwrap(),
        );
        let registration = channel_component_registration(
            "mismatched-outbox",
            Arc::new(move |ctx| {
                let channel = captured_channel.clone();
                let runtime = captured_runtime.clone();
                Box::pin(async move {
                    if kind == "email" {
                        crate::serve::channel_outbox_loop(
                            channel,
                            runtime,
                            "local".into(),
                            "sender@example.com".into(),
                            ctx,
                        )
                        .await
                    } else {
                        crate::serve::telegram_outbox_loop(channel, runtime, "local".into(), ctx)
                            .await
                    }
                })
            }),
        );
        let ctx = HostContext::new(
            server.clone(),
            CancellationToken::new(),
            "mismatched-outbox",
            health.clone(),
        );
        let outcome = tokio::select! {
            biased;
            outcome=(registration.start)(ctx)=>outcome,
            _=std::future::ready(())=>panic!("mismatched adapter must fail before the first await"),
        };
        assert!(
            matches!(outcome, Err(ComponentError::Permanent(_))),
            "mismatched adapter must return permanent failure"
        );
        supervise(
            registration,
            server,
            CancellationToken::new(),
            health.clone(),
        )
        .await;
        let status = health.status("mismatched-outbox").unwrap();
        assert!(
            status
                .last_error
                .as_deref()
                .unwrap()
                .starts_with("outbox adapter kind"),
            "fixture must reach the adapter-kind guard, not the email policy guard"
        );
        assert_eq!(
            status.state,
            ComponentState::Unhealthy,
            "mismatched adapter must leave Running"
        );
        assert!(
            status.last_heartbeat.is_none(),
            "mismatched adapter must not heartbeat"
        );
        assert_eq!(channel.sends.load(Ordering::SeqCst), 0);
    }
}

#[cfg(feature = "channel-email")]
#[tokio::test]
async fn email_delivery_without_policy_stops_permanently_without_smtp_or_restart() {
    let (server, runtime, id) = fixture("email:recipient@example.com").await;
    assert!(!runtime.outbound_email_policy().is_configured());
    let before = properties(&runtime, id).await;
    let channel = Arc::new(ScriptedChannel::new("email", [Outcome::Delivered]));
    let starts = Arc::new(AtomicUsize::new(0));
    let health = HealthReporter::default();
    let cancellation = CancellationToken::new();
    let registration = channel_component_registration(
        "email-outbound",
        Arc::new({
            let runtime = runtime.clone();
            let channel = channel.clone();
            let starts = starts.clone();
            move |ctx| {
                starts.fetch_add(1, Ordering::SeqCst);
                Box::pin(crate::serve::channel_outbox_loop(
                    channel.clone(),
                    runtime.clone(),
                    "local".into(),
                    "sender@example.com".into(),
                    ctx,
                ))
            }
        }),
    );
    assert!(registration.max_restarts > 0);
    let mut task = tokio::spawn(supervise(
        registration,
        server,
        cancellation.clone(),
        health.clone(),
    ));
    let outcome = tokio::time::timeout(Duration::from_secs(3), &mut task).await;
    if outcome.is_err() {
        cancellation.cancel();
        if tokio::time::timeout(Duration::from_secs(3), &mut task)
            .await
            .is_err()
        {
            task.abort();
            let _ = task.await;
        }
        panic!("absent policy must stop the component at startup");
    }
    outcome.unwrap().unwrap();
    let status = health.status("email-outbound").unwrap();
    assert_eq!(status.state, ComponentState::Unhealthy);
    assert_eq!(status.restart_count, 0);
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert_eq!(
        status.last_error.as_deref(),
        Some("outbound email delivery requires a configured recipient policy")
    );
    assert!(status.last_heartbeat.is_none());
    assert_eq!(channel.sends.load(Ordering::SeqCst), 0);
    assert_eq!(
        properties(&runtime, id).await,
        before,
        "configuration refusal must preserve the queued note"
    );
}

#[cfg(feature = "channel-email")]
async fn fixture_with_stored_email_tags(
    recipient: &str,
    policy: khive_runtime::OutboundEmailPolicy,
    tags: &[&str],
) -> (KhiveMcpServer, KhiveRuntime, uuid::Uuid) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: Some("actor:outbox-supervision".into()),
        brain_profile: None,
        packs: vec!["kg".into(), "comm".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap()
    .with_outbound_email_policy(policy);
    let server = KhiveMcpServer::new(runtime.clone()).unwrap();
    let sent = server
        .verb_registry_clone()
        .dispatch(
            "comm.send",
            json!({
                "to": recipient,
                "subject": "delivery state",
                "content": "pending supervised delivery",
                "tags": tags
            }),
        )
        .await
        .unwrap();
    let id = sent["full_id"].as_str().unwrap().parse().unwrap();
    (server, runtime, id)
}

#[cfg(feature = "channel-email")]
async fn run_stored_email_to_terminal_delivery(
    server: KhiveMcpServer,
    runtime: KhiveRuntime,
    id: uuid::Uuid,
) -> (Arc<ScriptedChannel>, serde_json::Value) {
    let channel = Arc::new(ScriptedChannel::new("email", [Outcome::Delivered]));
    let health = HealthReporter::default();
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(supervise(
        email_registration_with_policy(runtime.clone(), channel.clone()),
        server,
        cancellation.clone(),
        health,
    ));
    let terminal = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let props = properties(&runtime, id).await;
            if props["delivery"] == "delivered" || props["delivery"] == "failed" {
                break props;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("tagged email supervisor must stop after cancellation")
        .unwrap();
    let props = terminal.expect("tagged email must reach a terminal delivery state");
    (channel, props)
}

#[cfg(feature = "channel-email")]
async fn assert_stored_list_footer_delivery(default_footer: Result<&str, &str>) {
    let policy = khive_runtime::OutboundEmailPolicy::configured_for_tests(
        vec!["maintainer@example.com".into()],
        vec!["actor:outbox-supervision".into()],
        5,
    )
    .unwrap()
    .with_external_footer_for_tests(default_footer)
    .with_list_footer_for_tests(Ok("List notice: {unsubscribe}"));
    let (server, runtime, id) = fixture_with_stored_email_tags(
        "email:external@example.net",
        policy.clone(),
        &["unrelated", "traffic:list"],
    )
    .await;
    assert_eq!(
        properties(&runtime, id).await["tags"],
        json!(["unrelated", "traffic:list"])
    );
    let (channel, props) = run_stored_email_to_terminal_delivery(server, runtime, id).await;
    assert_eq!(props["delivery"], "delivered", "{props}");
    assert_eq!(channel.sends.load(Ordering::SeqCst), 1);
    let captured = channel.captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    let envelope = &captured[0];
    let header = envelope
        .list_unsubscribe
        .as_deref()
        .expect("external list message must carry List-Unsubscribe");
    let token = header
        .strip_prefix("<mailto:sender@example.com?subject=unsubscribe%20")
        .and_then(|value| value.strip_suffix('>'))
        .expect("list header must carry the sender mailbox and token");
    assert_eq!(
        policy.verify_unsubscribe_token(token).as_deref(),
        policy.recipient_key("external@example.net").as_deref()
    );
    assert_eq!(
        envelope.content,
        format!(
            "pending supervised delivery\n\nList notice: To unsubscribe, send an email to sender@example.com with the subject: unsubscribe {token}"
        )
    );
    assert!(!envelope.content.contains("{unsubscribe}"));
}

#[cfg(feature = "channel-email")]
#[tokio::test]
async fn external_list_email_renders_stored_list_footer() {
    assert_stored_list_footer_delivery(Ok("Default notice: {unsubscribe}")).await;
}

#[cfg(feature = "channel-email")]
#[tokio::test]
async fn external_list_email_renders_when_default_footer_is_unavailable() {
    assert_stored_list_footer_delivery(Err("KHIVE_EMAIL_EXTERNAL_FOOTER_FILE could not be read"))
        .await;
}

#[cfg(feature = "channel-email")]
#[tokio::test]
async fn external_list_email_missing_footer_after_admission_fails_before_smtp() {
    let admitted = khive_runtime::OutboundEmailPolicy::configured_for_tests(
        vec!["maintainer@example.com".into()],
        vec!["actor:outbox-supervision".into()],
        5,
    )
    .unwrap()
    .with_list_footer_for_tests(Ok("List notice: {unsubscribe}"));
    let (server, runtime, id) =
        fixture_with_stored_email_tags("email:external@example.net", admitted, &["traffic:list"])
            .await;
    let admitted_props = properties(&runtime, id).await;
    assert_eq!(admitted_props["tags"], json!(["traffic:list"]));
    assert!(admitted_props.get("delivery").is_none(), "{admitted_props}");
    // The stored message was admitted while its list footer was available.
    // A fresh legacy constructor has a valid default footer and a missing list footer.
    let runtime = runtime.with_outbound_email_policy(
        khive_runtime::OutboundEmailPolicy::configured_for_tests(
            vec!["maintainer@example.com".into()],
            vec!["actor:outbox-supervision".into()],
            5,
        )
        .unwrap()
        .with_external_footer_for_tests(Ok("Default notice: {unsubscribe}")),
    );
    let (channel, props) = run_stored_email_to_terminal_delivery(server, runtime.clone(), id).await;
    assert_eq!(props["delivery"], "failed", "{props}");
    let last_error = props["last_error"].as_str().unwrap();
    assert!(last_error.contains("footer is unavailable"), "{last_error}");
    assert!(
        last_error.contains("KHIVE_EMAIL_LIST_FOOTER_FILE is not set"),
        "{last_error}"
    );
    assert_eq!(channel.sends.load(Ordering::SeqCst), 0);
    assert!(channel.captured.lock().unwrap().is_empty());
    let token = runtime.authorize(Namespace::local()).unwrap();
    assert!(runtime
        .list_undelivered_outbound_messages(&token, Some("email:"), 200)
        .await
        .unwrap()
        .is_empty());
}

#[cfg(feature = "channel-email")]
#[tokio::test]
async fn external_email_requires_an_exact_stored_list_tag() {
    let policy = khive_runtime::OutboundEmailPolicy::configured_for_tests(
        vec!["maintainer@example.com".into()],
        vec!["actor:outbox-supervision".into()],
        5,
    )
    .unwrap()
    .with_external_footer_for_tests(Ok("Default notice: {unsubscribe}"));
    let (server, runtime, id) = fixture_with_stored_email_tags(
        "email:external@example.net",
        policy,
        &["Traffic:list", "traffic:list-extra", "prefix:traffic:list"],
    )
    .await;
    let (channel, props) = run_stored_email_to_terminal_delivery(server, runtime, id).await;
    assert_eq!(props["delivery"], "delivered", "{props}");
    assert_eq!(channel.sends.load(Ordering::SeqCst), 1);
    let captured = channel.captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert!(captured[0]
        .content
        .contains("\n\nDefault notice: To unsubscribe, send an email to sender@example.com"));
    assert!(!captured[0].content.contains("List notice:"));
    assert!(!captured[0].content.contains("{unsubscribe}"));
}

#[cfg(feature = "channel-email")]
#[tokio::test]
async fn policy_recipient_list_tag_carries_no_external_footer() {
    let policy = khive_runtime::OutboundEmailPolicy::configured_for_tests(
        vec!["maintainer@example.com".into()],
        vec!["actor:outbox-supervision".into()],
        5,
    )
    .unwrap();
    let (server, runtime, id) =
        fixture_with_stored_email_tags("email:maintainer@example.com", policy, &["traffic:list"])
            .await;
    let (channel, props) = run_stored_email_to_terminal_delivery(server, runtime, id).await;
    assert_eq!(props["delivery"], "delivered", "{props}");
    assert_eq!(channel.sends.load(Ordering::SeqCst), 1);
    let captured = channel.captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert!(captured[0].list_unsubscribe.is_none());
    assert_eq!(captured[0].content, "pending supervised delivery");
}
