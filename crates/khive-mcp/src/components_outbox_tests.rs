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
}

impl ScriptedChannel {
    fn new(kind: &'static str, outcomes: impl IntoIterator<Item = Outcome>) -> Self {
        Self {
            kind,
            outcomes: Mutex::new(outcomes.into_iter().collect()),
            sends: AtomicUsize::new(0),
            cancel_after_send: None,
        }
    }
}

#[async_trait]
impl Channel for ScriptedChannel {
    fn kind(&self) -> &'static str {
        self.kind
    }

    async fn send(&self, _envelope: ChannelEnvelope) -> Result<(), ChannelError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
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
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: Some("actor:outbox-supervision".into()),
        brain_profile: None,
        packs: vec!["kg".into(), "comm".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
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
    let mut registration = channel_component_registration(
        "email-outbound",
        Arc::new(move |ctx| {
            Box::pin(crate::serve::channel_outbox_loop(
                channel.clone(),
                runtime.clone(),
                "local".into(),
                "sender@example.com".into(),
                vec!["recipient@example.com".into()],
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
        let captured_runtime = runtime.clone();
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
                            vec![],
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
