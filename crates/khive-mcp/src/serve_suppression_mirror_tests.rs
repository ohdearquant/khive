//! ADR-200 acceptance A11: the remote suppression mirror, through the real request and outbox
//! paths, with a local HTTP listener as the remote and a recording transport.

use super::*;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use khive_channel::{Channel, ChannelEnvelope, ChannelError};
use khive_runtime::{KhiveRuntime, Namespace, OutboundEmailPolicy, RuntimeConfig};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const ACTOR: &str = "actor:outbox-supervision";
const CREDENTIAL: &str = "mirror-read-credential";

/// A local stand-in for the remote read: answers each request with the next scripted
/// `(status, body)`, or 500 once the script is empty, and counts the requests.
struct RemoteMirror {
    url: String,
    hits: Arc<AtomicUsize>,
    script: Arc<Mutex<VecDeque<(u16, String)>>>,
}

impl RemoteMirror {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let script: Arc<Mutex<VecDeque<(u16, String)>>> = Arc::default();
        let (task_hits, task_script) = (hits.clone(), script.clone());
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
                assert!(
                    request.contains(&format!("authorization: bearer {CREDENTIAL}")),
                    "{request}"
                );
                task_hits.fetch_add(1, Ordering::SeqCst);
                let (status, body) = task_script
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or((500, "{}".to_string()));
                let response = format!(
                    "HTTP/1.1 {status} Scripted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        Self {
            url: format!("http://{addr}/admin/email-suppression"),
            hits,
            script,
        }
    }

    fn answer(&self, status: u16, keys: &[String]) {
        let body = json!({
            "as_of": Utc::now().to_rfc3339(),
            "count": keys.len(),
            "recipient_keys": keys,
        })
        .to_string();
        self.script.lock().unwrap().push_back((status, body));
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

/// A transport that records every envelope handed to it and accepts it.
#[derive(Default)]
struct RecordingEmail {
    sent: Mutex<Vec<ChannelEnvelope>>,
}

impl RecordingEmail {
    fn recipients(&self) -> Vec<String> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .map(|envelope| envelope.to.clone())
            .collect()
    }
}

#[async_trait]
impl Channel for RecordingEmail {
    fn kind(&self) -> &'static str {
        "email"
    }

    fn slug(&self) -> String {
        "sender@example.com".into()
    }

    async fn poll(&self, _: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
        Ok(Vec::new())
    }

    async fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
        self.sent.lock().unwrap().push(envelope);
        Ok(())
    }
}

fn policy(mirror: &RemoteMirror, max_age: Duration) -> OutboundEmailPolicy {
    OutboundEmailPolicy::configured_for_tests(
        vec!["maintainer@example.com".into()],
        vec![ACTOR.into()],
        50,
    )
    .unwrap()
    .with_suppression_mirror_for_tests(&mirror.url, CREDENTIAL, max_age)
}

fn serve(policy: OutboundEmailPolicy) -> (KhiveMcpServer, KhiveRuntime) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: Some(ACTOR.into()),
        brain_profile: None,
        packs: vec!["kg".into(), "comm".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap()
    .with_outbound_email_policy(policy);
    let server = KhiveMcpServer::new(runtime.clone()).unwrap();
    (server, runtime)
}

/// Record a successful empty refresh, as a previous cycle would have.
async fn stamp_fresh(runtime: &KhiveRuntime) {
    khive_pack_comm::handlers::apply_suppression_mirror_refresh(
        runtime,
        &Utc::now().to_rfc3339(),
        &[],
        ACTOR,
    )
    .await
    .unwrap();
}

async fn send(server: &KhiveMcpServer, to: &str) -> Result<uuid::Uuid, String> {
    send_with(server, json!({"to": to, "subject": "s", "content": "body"})).await
}

async fn send_with(server: &KhiveMcpServer, params: Value) -> Result<uuid::Uuid, String> {
    server
        .verb_registry_clone()
        .dispatch("comm.send", params)
        .await
        .map(|sent| sent["full_id"].as_str().unwrap().parse().unwrap())
        .map_err(|error| error.to_string())
}

async fn delivery(runtime: &KhiveRuntime, id: uuid::Uuid) -> Value {
    let token = runtime.authorize(Namespace::local()).unwrap();
    let props = runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap()
        .properties
        .unwrap();
    json!({"delivery": props["delivery"], "last_error": props["last_error"]})
}

async fn cycle(runtime: &KhiveRuntime, channel: &RecordingEmail) {
    let domains =
        khive_runtime::EmailMessageIdDomains::from_mailbox_and_history("sender@example.com", "")
            .unwrap();
    let mut pause = None;
    outbox::outbox_once(
        outbox::OutboxChannels::Single(channel),
        outbox::OutboxPolicy::Email {
            mailbox: "sender@example.com",
            domains: &domains,
        },
        runtime,
        &Namespace::local(),
        &CancellationToken::new(),
        &mut pause,
    )
    .await
    .unwrap();
}

fn pending(state: &Value) -> bool {
    state["delivery"] != "delivered" && state["delivery"] != "failed"
}

#[tokio::test]
async fn a_failed_refresh_keeps_every_external_message_pending_until_one_succeeds() {
    let mirror = RemoteMirror::start().await;
    let (server, runtime) = serve(policy(&mirror, Duration::from_secs(3600)));
    stamp_fresh(&runtime).await;
    let first = send(&server, "email:one@example.net").await.unwrap();
    let second = send(&server, "email:two@example.net").await.unwrap();
    let maintainer = send(&server, "email:maintainer@example.com").await.unwrap();
    let channel = RecordingEmail::default();

    mirror.answer(503, &[]);
    cycle(&runtime, &channel).await;
    assert_eq!(
        mirror.hits(),
        1,
        "one refresh per cycle, however many messages"
    );
    assert!(pending(&delivery(&runtime, first).await));
    assert!(pending(&delivery(&runtime, second).await));
    assert_eq!(
        channel.recipients(),
        vec!["email:maintainer@example.com".to_string()],
        "a policy-recipient message does not wait on the mirror"
    );
    assert_eq!(
        delivery(&runtime, maintainer).await["delivery"],
        "delivered"
    );

    mirror.answer(200, &[]);
    cycle(&runtime, &channel).await;
    assert_eq!(mirror.hits(), 2);
    assert_eq!(delivery(&runtime, first).await["delivery"], "delivered");
    assert_eq!(delivery(&runtime, second).await["delivery"], "delivered");
    assert_eq!(channel.recipients().len(), 3);
}

#[tokio::test]
async fn a_malformed_remote_set_is_a_failed_refresh() {
    let mirror = RemoteMirror::start().await;
    let (server, runtime) = serve(policy(&mirror, Duration::from_secs(3600)));
    stamp_fresh(&runtime).await;
    let id = send(&server, "email:one@example.net").await.unwrap();
    let channel = RecordingEmail::default();

    mirror.answer(200, &["not-a-recipient-key".to_string()]);
    cycle(&runtime, &channel).await;
    assert_eq!(mirror.hits(), 1);
    assert!(pending(&delivery(&runtime, id).await));
    assert!(channel.recipients().is_empty());
}

#[tokio::test]
async fn a_remote_key_suppresses_its_address_before_its_row_is_sent() {
    let mirror = RemoteMirror::start().await;
    let policy = policy(&mirror, Duration::from_secs(3600));
    let key = policy.recipient_key("gone@example.net").unwrap();
    let (server, runtime) = serve(policy);
    stamp_fresh(&runtime).await;
    let gone = send(&server, "email:gone@example.net").await.unwrap();
    let kept = send(&server, "email:kept@example.net").await.unwrap();
    let channel = RecordingEmail::default();

    mirror.answer(200, std::slice::from_ref(&key));
    cycle(&runtime, &channel).await;
    let state = delivery(&runtime, gone).await;
    assert_eq!(state["delivery"], "failed");
    assert!(
        state["last_error"].as_str().unwrap().contains("suppress"),
        "{state}"
    );
    assert_eq!(delivery(&runtime, kept).await["delivery"], "delivered");
    assert_eq!(
        channel.recipients(),
        vec!["email:kept@example.net".to_string()]
    );

    let listed = server
        .verb_registry_clone()
        .dispatch(
            "comm.suppression_list",
            json!({"scope": "outbound", "kind": "recipient_key"}),
        )
        .await
        .unwrap();
    let items = listed["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{listed}");
    assert_eq!(items[0]["value"], key.as_str());
    assert_eq!(items[0]["source"], "one_click");

    // The same remote set applied again adds nothing, and the address stays refused.
    let added = khive_pack_comm::handlers::apply_suppression_mirror_refresh(
        &runtime,
        &Utc::now().to_rfc3339(),
        std::slice::from_ref(&key),
        ACTOR,
    )
    .await
    .unwrap();
    assert_eq!(added, 0);
    let error = send(&server, "email:gone@example.net").await.unwrap_err();
    assert!(error.contains("recipient is suppressed"), "{error}");
}

#[tokio::test]
async fn external_admission_refuses_once_the_last_refresh_is_older_than_the_maximum_age() {
    let mirror = RemoteMirror::start().await;
    let (server, runtime) = serve(policy(&mirror, Duration::from_millis(300)));

    // No refresh recorded yet.
    let error = send(&server, "email:one@example.net").await.unwrap_err();
    assert!(error.contains("suppression mirror stale"), "{error}");
    // A policy recipient never reads the mirror.
    send(&server, "email:maintainer@example.com").await.unwrap();

    stamp_fresh(&runtime).await;
    let keyed = json!({
        "to": "email:one@example.net",
        "subject": "s",
        "content": "body",
        "idempotency_key": "mirror-replay",
    });
    let original = send_with(&server, keyed.clone()).await.unwrap();

    tokio::time::sleep(Duration::from_millis(400)).await;
    let error = send(&server, "email:two@example.net").await.unwrap_err();
    assert!(error.contains("suppression mirror stale"), "{error}");
    // An exact keyed replay is answered from its holder before any admission condition.
    assert_eq!(send_with(&server, keyed).await.unwrap(), original);
    assert_eq!(mirror.hits(), 0, "admission performs no network I/O");
}

#[tokio::test]
async fn an_idle_cycle_refreshes_a_mirror_that_is_due() {
    let mirror = RemoteMirror::start().await;
    let (server, runtime) = serve(policy(&mirror, Duration::from_secs(3600)));
    let channel = RecordingEmail::default();

    // A failed idle refresh records nothing, so admission stays closed.
    mirror.answer(500, &[]);
    cycle(&runtime, &channel).await;
    assert_eq!(mirror.hits(), 1);
    let error = send(&server, "email:one@example.net").await.unwrap_err();
    assert!(error.contains("suppression mirror stale"), "{error}");

    // A successful idle refresh opens it.
    mirror.answer(200, &[]);
    cycle(&runtime, &channel).await;
    assert_eq!(mirror.hits(), 2);
    send(&server, "email:one@example.net").await.unwrap();

    // A pending external message refreshes its own cycle even when the mirror is fresh.
    mirror.answer(200, &[]);
    cycle(&runtime, &channel).await;
    assert_eq!(mirror.hits(), 3);
    assert_eq!(
        channel.recipients(),
        vec!["email:one@example.net".to_string()]
    );

    // A fresh mirror with nothing external pending is not read again.
    cycle(&runtime, &channel).await;
    assert_eq!(
        mirror.hits(),
        3,
        "an idle cycle under half the maximum age reads nothing"
    );
}
