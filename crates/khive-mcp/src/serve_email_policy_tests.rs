use super::*;
use khive_runtime::{Namespace, RequestIdentity};
use serde_json::{json, Value};

/// Collects formatted log output so a test can read what a code path reported.
#[cfg(feature = "channel-email")]
#[derive(Clone, Default)]
struct CapturedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(feature = "channel-email")]
impl CapturedLog {
    fn capture(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(self.clone())
                .with_ansi(false)
                .without_time()
                .finish(),
        )
    }
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

#[cfg(feature = "channel-email")]
impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(feature = "channel-email")]
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
    type Writer = CapturedLog;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn config() -> RuntimeConfig {
    RuntimeConfig {
        db_path: None,
        actor_id: Some("actor:email-policy-sender".into()),
        brain_profile: None,
        packs: vec!["kg".into(), "comm".into()],
        ..RuntimeConfig::no_embeddings()
    }
}

fn isolated_case(name: &str, run: impl std::future::Future<Output = ()>) {
    if std::env::var("KHIVE_EMAIL_POLICY_TEST_CHILD").as_deref() == Ok(name) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run);
        println!("email-policy-child-complete:{name}");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    struct OwnedChild(Option<std::process::Child>);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if let Some(child) = self.0.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("serve::email_policy_tests::{name}"),
            "--nocapture",
        ])
        .env_clear()
        .env("HOME", home.path())
        .env("TMPDIR", home.path())
        .env("KHIVE_EMAIL_POLICY_TEST_CHILD", name)
        .env("KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS", "allowed@example.com")
        .env("KHIVE_EMAIL_MAINTAINER_ADDRESS", "fallback@example.com")
        .env("KHIVE_NO_DAEMON", "1")
        .current_dir(home.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = OwnedChild(Some(child));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while child.0.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "isolated host fixture timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = child.0.take().unwrap().wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "isolated host fixture failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains(&format!("email-policy-child-complete:{name}")));
}

async fn assert_host_snapshot(server: &KhiveMcpServer, runtime: &KhiveRuntime) {
    std::env::set_var("KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS", "denied@example.com");
    assert!(runtime
        .outbound_email_policy()
        .allows("allowed@example.com"));
    assert!(!runtime.outbound_email_policy().allows("denied@example.com"));
    assert!(
        !runtime
            .outbound_email_policy()
            .allows("fallback@example.com"),
        "explicit policy must take precedence over the maintainer fallback"
    );
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    {
        use clap::Parser as _;
        let args = Args::parse_from(["mcp"]);
        assert_eq!(
            channel_loop_plan(server, &args),
            crate::server::ChannelLoopAdmission::default(),
            "disabled connector loops must not disable admission policy"
        );
    }
    let registry = server.verb_registry_clone();
    let refused = registry
        .dispatch(
            "comm.send",
            json!({"to":"email:denied@example.com","content":"refuse locally"}),
        )
        .await;
    assert!(
        matches!(
            refused,
            Err(khive_runtime::RuntimeError::PermissionDenied { .. })
        ),
        "serving runtime must retain its boot policy"
    );
    let sent = registry
        .dispatch(
            "comm.send",
            json!({"to":"email:Allowed@Example.COM","content":"queue without connector"}),
        )
        .await
        .unwrap();
    let id: uuid::Uuid = sent["full_id"].as_str().unwrap().parse().unwrap();
    let response = server
        .dispatch_request_inner(
            crate::tools::request::RequestParams {
                ops: r#"comm.send(to="email:denied@example.com",content="forwarded refusal")"#
                    .into(),
                presentation: Some("verbose".into()),
                ..Default::default()
            },
            true,
            Some(RequestIdentity {
                namespace: "local".into(),
                actor_id: Some("actor:forwarded-client".into()),
                ..Default::default()
            }),
            crate::server::DispatchOrigin::Daemon,
        )
        .await
        .unwrap();
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(
        response["summary"]["succeeded"], 0,
        "forwarded dispatch must use the serving policy"
    );
    assert_eq!(response["summary"]["failed"], 1);
    let token = runtime.authorize(Namespace::local()).unwrap();
    let pending = runtime
        .list_undelivered_outbound_messages(&token, Some("email:"), 200)
        .await
        .unwrap();
    assert_eq!(
        pending.len(),
        1,
        "denied local and forwarded requests must not queue notes"
    );
    assert_eq!(pending[0].id, id);
    assert_eq!(
        pending[0].properties.as_ref().unwrap()["to_actor"],
        "email:Allowed@Example.COM",
        "normalization must not rewrite the stored recipient"
    );
    #[cfg(feature = "channel-email")]
    {
        let channel = delivery::MockEmail::new([delivery::Outcome::Delivered]);
        delivery::cycle(
            runtime,
            &channel,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert_eq!(
            channel.sent.lock().unwrap().len(),
            1,
            "outbox must share admission's boot snapshot after environment changes"
        );
        assert_eq!(
            channel.sent.lock().unwrap()[0].to,
            "email:Allowed@Example.COM",
            "transport envelope keeps the original recipient"
        );
        assert_eq!(
            delivery::sent_properties(server, id).await["delivery"],
            "delivered"
        );
        delivery::cycle(
            runtime,
            &channel,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert_eq!(
            channel.sent.lock().unwrap().len(),
            1,
            "two ordinary cycles must send once"
        );
    }
}

#[test]
fn single_host_policy_survives_disabled_delivery_and_environment_change() {
    isolated_case(
        "single_host_policy_survives_disabled_delivery_and_environment_change",
        async {
            let runtime = build_single_backend_runtime(config(), &KhiveConfig::default())
                .await
                .unwrap();
            let server = KhiveMcpServer::new(runtime.clone()).unwrap();
            assert_host_snapshot(&server, &runtime).await;
            for explicit in [None, Some(" , ")] {
                match explicit {
                    Some(value) => std::env::set_var("KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS", value),
                    None => std::env::remove_var("KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS"),
                }
                std::env::set_var(
                    "KHIVE_EMAIL_MAINTAINER_ADDRESS",
                    "Owner <Primary@Example.COM>, secondary@example.com",
                );
                let runtime = build_single_backend_runtime(config(), &KhiveConfig::default())
                    .await
                    .unwrap();
                let server = KhiveMcpServer::new(runtime).unwrap();
                let registry = server.verb_registry_clone();
                registry
                    .dispatch(
                        "comm.send",
                        json!({"to":"email:PRIMARY@example.com","content":"normalized default"}),
                    )
                    .await
                    .unwrap();
                for recipient in ["email:secondary@example.com", "email:other@example.com"] {
                    assert!(matches!(
                        registry
                            .dispatch(
                                "comm.send",
                                json!({"to":recipient,"content":"default excludes other addresses"})
                            )
                            .await,
                        Err(khive_runtime::RuntimeError::PermissionDenied { .. })
                    ));
                }
            }
        },
    );
}

#[test]
fn routed_host_shares_policy_and_marks_only_the_comm_backend() {
    isolated_case(
        "routed_host_shares_policy_and_marks_only_the_comm_backend",
        async {
            let cfg: KhiveConfig = toml::from_str("[[backends]]\nname='main'\nkind='memory'\n[[backends]]\nname='mail'\nkind='memory'\n[packs.comm]\nbackend='mail'\nno_embed=true\n").unwrap();
            let multi = build_registry_for_multi_backend_inner(config(), &cfg, None)
                .await
                .unwrap();
            let runtime = multi.per_pack_runtimes["comm"].as_ref().clone();
            let main = multi.default_runtime.clone();
            assert!(main.outbound_email_policy().allows("allowed@example.com"));
            assert!(!runtime
                .core()
                .outbound_email_policy()
                .allows("denied@example.com"));
            let server = build_server_from_multi_backend_registry(multi, &cfg, None);
            assert_host_snapshot(&server, &runtime).await;
            let token = main.authorize(Namespace::local()).unwrap();
            assert!(
                main.list_notes(&token, Some("message"), 20, 0)
                    .await
                    .unwrap()
                    .is_empty(),
                "mail must remain on the assigned comm backend"
            );
        },
    );
}

#[test]
fn invalid_public_policy_refuses_host_boot_without_credentials_or_storage_writes() {
    isolated_case(
        "invalid_public_policy_refuses_host_boot_without_credentials_or_storage_writes",
        async {
            std::env::set_var("KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS", "invalid-address");
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("not-opened.db");
            let mut cfg = config();
            cfg.db_path = Some(path.clone());
            assert!(build_single_backend_runtime(cfg, &KhiveConfig::default())
                .await
                .is_err());
            assert!(
                !path.exists(),
                "invalid policy must fail before opening the database"
            );
            let routed: KhiveConfig =
                toml::from_str("[[backends]]\nname='main'\nkind='memory'\n").unwrap();
            assert!(
                build_registry_for_multi_backend_inner(config(), &routed, None)
                    .await
                    .is_err()
            );
            std::env::set_var("KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS", " , ");
            for maintainer in [None, Some(" , ")] {
                match maintainer {
                    Some(value) => std::env::set_var("KHIVE_EMAIL_MAINTAINER_ADDRESS", value),
                    None => std::env::remove_var("KHIVE_EMAIL_MAINTAINER_ADDRESS"),
                }
                let mut cfg = config();
                cfg.db_path = Some(path.clone());
                assert!(
                    build_single_backend_runtime(cfg, &KhiveConfig::default())
                        .await
                        .is_err(),
                    "blank configured policy must not become absent"
                );
                assert!(!path.exists());
                assert!(
                    build_registry_for_multi_backend_inner(config(), &routed, None)
                        .await
                        .is_err()
                );
            }
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStringExt;
                std::env::set_var(
                    "KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS",
                    std::ffi::OsString::from_vec(vec![0xff]),
                );
                assert!(
                    build_single_backend_runtime(config(), &KhiveConfig::default())
                        .await
                        .is_err(),
                    "non-Unicode policy must not become absent"
                );
                assert!(
                    build_registry_for_multi_backend_inner(config(), &routed, None)
                        .await
                        .is_err()
                );
                std::env::set_var("KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS", "allowed@example.com");
                std::env::set_var(
                    "KHIVE_EMAIL_MAINTAINER_ADDRESS",
                    std::ffi::OsString::from_vec(vec![0xff]),
                );
                let mut cfg = config();
                cfg.db_path = Some(path.clone());
                assert!(
                    build_single_backend_runtime(cfg, &KhiveConfig::default())
                        .await
                        .is_err(),
                    "unreadable maintainer policy must refuse even with a valid explicit list"
                );
                assert!(
                    !path.exists(),
                    "unreadable policy must fail before opening storage"
                );
                assert!(
                    build_registry_for_multi_backend_inner(config(), &routed, None)
                        .await
                        .is_err(),
                    "routed boot must not ignore unreadable public policy"
                );
            }
        },
    );
}

#[test]
fn invalid_maintainer_beside_valid_explicit_policy_refuses_both_hosts_before_storage_opens() {
    isolated_case(
        "invalid_maintainer_beside_valid_explicit_policy_refuses_both_hosts_before_storage_opens",
        async {
            std::env::set_var("KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS", "allowed@example.com");
            let dir = tempfile::tempdir().unwrap();
            let single_path = dir.path().join("single-not-opened.db");
            let routed_path = dir.path().join("routed-not-opened.db");
            let routed: KhiveConfig = toml::from_str(&format!(
                "[[backends]]\nname='main'\nkind='sqlite'\npath='{}'\n",
                routed_path.display()
            ))
            .unwrap();
            for maintainer in [
                "",
                " , ",
                "primary@example.com,invalid",
                "invalid,primary@example.com",
            ] {
                std::env::set_var("KHIVE_EMAIL_MAINTAINER_ADDRESS", maintainer);
                let mut cfg = config();
                cfg.db_path = Some(single_path.clone());
                assert!(
                    build_single_backend_runtime(cfg, &KhiveConfig::default())
                        .await
                        .is_err(),
                    "maintainer {maintainer:?} must refuse the single host"
                );
                assert!(
                    !single_path.exists(),
                    "maintainer {maintainer:?} must fail before the single host opens storage"
                );
                assert!(
                    build_registry_for_multi_backend_inner(config(), &routed, None)
                        .await
                        .is_err(),
                    "maintainer {maintainer:?} must refuse the routed host"
                );
                assert!(
                    !routed_path.exists(),
                    "maintainer {maintainer:?} must fail before the routed host opens storage"
                );
            }
            // Control: a readable maintainer list boots both hosts on the same fixture,
            // so the absent database files above were caused by the refusal.
            std::env::set_var(
                "KHIVE_EMAIL_MAINTAINER_ADDRESS",
                "Primary@Example.com, second@example.com",
            );
            let mut cfg = config();
            cfg.db_path = Some(single_path.clone());
            let runtime = build_single_backend_runtime(cfg, &KhiveConfig::default())
                .await
                .unwrap();
            assert!(single_path.exists());
            assert!(runtime
                .outbound_email_policy()
                .allows("allowed@example.com"));
            assert!(!runtime
                .outbound_email_policy()
                .allows("primary@example.com"));
            build_registry_for_multi_backend_inner(config(), &routed, None)
                .await
                .unwrap();
            assert!(routed_path.exists());
        },
    );
}

async fn assert_unset_maintainer_admission(server: &KhiveMcpServer, runtime: &KhiveRuntime) {
    assert!(runtime.outbound_email_policy().is_configured());
    let registry = server.verb_registry_clone();
    let sent = registry
        .dispatch(
            "comm.send",
            json!({"to":"email:Allowed@Example.COM","content":"queued without a connector"}),
        )
        .await
        .unwrap();
    let id: uuid::Uuid = sent["full_id"].as_str().unwrap().parse().unwrap();
    let refused = registry
        .dispatch(
            "comm.send",
            json!({"to":"email:denied@example.com","content":"refused locally"}),
        )
        .await;
    assert!(
        matches!(
            refused,
            Err(khive_runtime::RuntimeError::PermissionDenied { .. })
        ),
        "an unset maintainer beside an explicit list must still refuse other recipients"
    );
    #[cfg(feature = "channel-email")]
    {
        let log = CapturedLog::default();
        {
            let _capture = log.capture();
            spawn_email_channel_loops(server, crate::server::ChannelLoopAdmission::default());
        }
        let logged = log.text();
        assert!(
            logged.contains("configuration is incomplete")
                && logged.contains("KHIVE_EMAIL_MAINTAINER_ADDRESS"),
            "the start-up warning must name the missing maintainer value: {logged}"
        );
    }
    let token = runtime.authorize(Namespace::local()).unwrap();
    let pending = runtime
        .list_undelivered_outbound_messages(&token, Some("email:"), 200)
        .await
        .unwrap();
    assert_eq!(pending.len(), 1, "only the admitted message is queued");
    assert_eq!(pending[0].id, id);
    assert!(
        pending[0]
            .properties
            .as_ref()
            .unwrap()
            .get("delivery")
            .is_none(),
        "the admitted message stays pending without a connector"
    );
}

#[test]
fn unset_maintainer_beside_valid_explicit_policy_admits_while_the_connector_cannot_start() {
    isolated_case(
        "unset_maintainer_beside_valid_explicit_policy_admits_while_the_connector_cannot_start",
        async {
            std::env::remove_var("KHIVE_EMAIL_MAINTAINER_ADDRESS");
            // Every other required connector value is present, so the start-up
            // failure can only name the maintainer value.
            #[cfg(feature = "channel-email")]
            {
                for name in [
                    "KHIVE_EMAIL_SMTP_HOST",
                    "KHIVE_EMAIL_IMAP_HOST",
                    "KHIVE_EMAIL_PASSWORD",
                    "KHIVE_EMAIL_AUTHSERV_ID",
                ] {
                    std::env::set_var(name, "unused.example.com");
                }
                // The username is a login address; the connector refuses one without '@'.
                std::env::set_var("KHIVE_EMAIL_USERNAME", "unused@example.com");
            }
            let runtime = build_single_backend_runtime(config(), &KhiveConfig::default())
                .await
                .unwrap();
            let server = KhiveMcpServer::new(runtime.clone()).unwrap();
            assert_unset_maintainer_admission(&server, &runtime).await;
            let cfg: KhiveConfig = toml::from_str("[[backends]]\nname='main'\nkind='memory'\n[[backends]]\nname='mail'\nkind='memory'\n[packs.comm]\nbackend='mail'\nno_embed=true\n").unwrap();
            let multi = build_registry_for_multi_backend_inner(config(), &cfg, None)
                .await
                .unwrap();
            let runtime = multi.per_pack_runtimes["comm"].as_ref().clone();
            let server = build_server_from_multi_backend_registry(multi, &cfg, None);
            assert_unset_maintainer_admission(&server, &runtime).await;
        },
    );
}

#[cfg(feature = "channel-email")]
mod delivery {
    use super::*;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use khive_channel::{Channel, ChannelEnvelope, ChannelError};
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    pub(super) enum Outcome {
        Delivered,
        Transient,
        Permanent,
        AcceptedWithoutStamp,
    }
    pub(super) struct MockEmail {
        outcomes: Mutex<VecDeque<Outcome>>,
        pub(super) sent: Mutex<Vec<ChannelEnvelope>>,
        accepted: tokio::sync::Notify,
        cancellation: Option<CancellationToken>,
    }
    impl MockEmail {
        pub(super) fn new(outcomes: impl IntoIterator<Item = Outcome>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes.into_iter().collect()),
                sent: Mutex::new(Vec::new()),
                accepted: tokio::sync::Notify::new(),
                cancellation: None,
            }
        }
    }
    #[async_trait]
    impl Channel for MockEmail {
        fn kind(&self) -> &'static str {
            "email"
        }
        fn slug(&self) -> String {
            "sender@example.com".into()
        }
        async fn poll(&self, _: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
            panic!("no inbound or real transport I/O")
        }
        async fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
            self.sent.lock().unwrap().push(envelope);
            let outcome = self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected transport call");
            if let Some(token) = &self.cancellation {
                token.cancel();
            }
            match outcome {
                Outcome::Delivered => Ok(()),
                Outcome::Transient => Err(ChannelError::Transport(
                    "temporary transport pressure".into(),
                )),
                Outcome::Permanent => Err(ChannelError::PermanentTransport(
                    "post-auth recipient rejected".into(),
                )),
                Outcome::AcceptedWithoutStamp => {
                    self.accepted.notify_one();
                    std::future::pending().await
                }
            }
        }
    }
    pub(super) async fn cycle(
        runtime: &KhiveRuntime,
        channel: &MockEmail,
        cancellation: &CancellationToken,
    ) {
        let domains = khive_runtime::EmailMessageIdDomains::from_mailbox_and_history(
            "sender@example.com",
            "",
        )
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
            cancellation,
            &mut pause,
        )
        .await
        .unwrap();
    }
    async fn fixture(recipient: &str) -> (KhiveMcpServer, KhiveRuntime, uuid::Uuid) {
        let runtime = KhiveRuntime::new(config()).unwrap();
        let server = KhiveMcpServer::new(runtime.clone()).unwrap();
        let sent = server
            .verb_registry_clone()
            .dispatch(
                "comm.send",
                json!({"to":recipient,"subject":"outcome","content":"stored email outcome"}),
            )
            .await
            .unwrap();
        (
            server,
            runtime,
            sent["full_id"].as_str().unwrap().parse().unwrap(),
        )
    }
    pub(super) async fn sent_properties(server: &KhiveMcpServer, id: uuid::Uuid) -> Value {
        let registry = server.verb_registry_clone();
        let sent = registry
            .dispatch(
                "comm.inbox",
                json!({"box":"sent","fields":["full_id","properties"]}),
            )
            .await
            .unwrap();
        let messages = sent["messages"].as_array().unwrap();
        let row = messages
            .iter()
            .find(|row| row["full_id"] == id.to_string())
            .expect("sender sees original receipt ID");
        row["properties"].clone()
    }
    #[tokio::test]
    async fn direct_delivery_requires_policy_and_normalizes_defensive_refusals() {
        let (server, runtime, id) = fixture("email:Recipient@Example.COM").await;
        let before = sent_properties(&server, id).await;
        let channel = MockEmail::new([Outcome::Delivered]);
        let domains = khive_runtime::EmailMessageIdDomains::from_mailbox_and_history(
            "sender@example.com",
            "",
        )
        .unwrap();
        let mut pause_until = None;
        let error = outbox::outbox_once(
            outbox::OutboxChannels::Single(&channel),
            outbox::OutboxPolicy::Email {
                mailbox: "sender@example.com",
                domains: &domains,
            },
            &runtime,
            &Namespace::local(),
            &CancellationToken::new(),
            &mut pause_until,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            crate::components::ComponentError::Permanent(ref message)
                if message == "outbound email delivery requires a configured recipient policy"
        ));
        assert!(channel.sent.lock().unwrap().is_empty());
        assert_eq!(sent_properties(&server, id).await, before);
        let runtime =
            runtime.with_outbound_email_policy(OutboundEmailPolicy::configured(vec![]).unwrap());
        let log = CapturedLog::default();
        {
            let _capture = log.capture();
            cycle(&runtime, &channel, &CancellationToken::new()).await;
        }
        let props = sent_properties(&server, id).await;
        assert_eq!(props["delivery"], "failed");
        assert_eq!(
            props["last_error"],
            "recipient recipient@example.com not in outbound allowlist"
        );
        assert_eq!(props["to_actor"], "email:Recipient@Example.COM");
        assert!(channel.sent.lock().unwrap().is_empty());
        let logged = log.text();
        assert!(
            logged.contains("recipient=recipient@example.com"),
            "{logged}"
        );
        assert!(!logged.contains("Recipient@Example.COM"), "{logged}");

        // A stored recipient with no normalized form is named as requested; an
        // unverifiable stored Message-ID still holds ahead of that refusal.
        for held in [false, true] {
            let (server, runtime, id) = fixture("email:placeholder@example.com").await;
            let token = runtime.authorize(Namespace::local()).unwrap();
            let mut note = runtime
                .notes(&token)
                .unwrap()
                .get_note(id)
                .await
                .unwrap()
                .unwrap();
            note.properties.as_mut().unwrap()["to_actor"] = json!("email:Invalid-Address");
            if held {
                note.properties.as_mut().unwrap()["external_id"] =
                    json!("<not-this-note@example.com>");
            }
            runtime
                .backend()
                .notes_for_namespace("local")
                .unwrap()
                .upsert_note(note)
                .await
                .unwrap();
            let runtime = runtime.with_outbound_email_policy(
                OutboundEmailPolicy::configured(vec!["recipient@example.com".into()]).unwrap(),
            );
            let log = CapturedLog::default();
            {
                let _capture = log.capture();
                cycle(&runtime, &channel, &CancellationToken::new()).await;
            }
            let props = sent_properties(&server, id).await;
            assert_eq!(props["to_actor"], "email:Invalid-Address");
            if held {
                assert_eq!(props["delivery_hold"], "external_id_unverifiable");
                assert_ne!(props["delivery"], "failed");
                assert!(!log.text().contains("not in outbound allowlist"));
            } else {
                assert_eq!(props["delivery"], "failed");
                assert_eq!(
                    props["last_error"],
                    "recipient Invalid-Address not in outbound allowlist"
                );
                assert!(
                    log.text().contains("recipient=Invalid-Address"),
                    "{}",
                    log.text()
                );
            }
            assert!(channel.sent.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn sender_reads_email_outcomes_without_transport_receipts_or_mailbox_leaks() {
        for outcome in ["delivered", "policy-failed", "transient", "held"] {
            let (server, runtime, id) = fixture("email:recipient@example.com").await;
            let allowed = if matches!(outcome, "policy-failed" | "held") {
                "other@example.com"
            } else {
                "recipient@example.com"
            };
            let runtime = runtime.with_outbound_email_policy(
                OutboundEmailPolicy::configured(vec![allowed.into()]).unwrap(),
            );
            if outcome == "held" {
                let token = runtime.authorize(Namespace::local()).unwrap();
                let store = runtime.backend().notes_for_namespace("local").unwrap();
                let mut note = runtime
                    .notes(&token)
                    .unwrap()
                    .get_note(id)
                    .await
                    .unwrap()
                    .unwrap();
                note.properties.as_mut().unwrap()["external_id"] =
                    json!("<not-this-note@example.com>");
                store.upsert_note(note).await.unwrap();
            }
            let channel = MockEmail::new(match outcome {
                "delivered" => vec![Outcome::Delivered],
                "transient" => vec![Outcome::Transient],
                _ => vec![],
            });
            cycle(&runtime, &channel, &CancellationToken::new()).await;
            let props = sent_properties(&server, id).await;
            match outcome {
                "delivered" => {
                    assert_eq!(props["delivery"], "delivered");
                    assert!(props["delivered_at"].is_string());
                    assert!(props["transport_message_id"].is_string());
                }
                "policy-failed" => {
                    assert_eq!(props["delivery"], "failed");
                    assert!(props["failed_at"].is_string());
                    assert!(props["last_error"]
                        .as_str()
                        .unwrap()
                        .contains("outbound allowlist"));
                }
                "transient" => {
                    assert_ne!(props["delivery"], "failed");
                    assert_ne!(props["delivery"], "delivered");
                    assert_eq!(props["delivery_attempts"], 1);
                    assert!(props["next_attempt_at"].is_string());
                    assert!(props["last_error"]
                        .as_str()
                        .unwrap()
                        .contains("temporary transport pressure"));
                }
                "held" => {
                    assert_eq!(props["delivery_hold"], "external_id_unverifiable");
                    assert_ne!(props["delivery"], "failed");
                    assert_eq!(props["external_id"], "<not-this-note@example.com>");
                }
                _ => unreachable!(),
            }
            assert_eq!(
                channel.sent.lock().unwrap().len(),
                usize::from(matches!(outcome, "delivered" | "transient"))
            );
            let registry = server.verb_registry_clone();
            let transport = registry
                .dispatch("comm.transport_status", json!({"id":id}))
                .await
                .unwrap();
            assert_eq!(
                transport["status"], "unknown",
                "SMTP note outcome must not synthesize an ADR-105 receipt"
            );
            let foreign = registry
                .dispatch_with_identity(
                    "comm.inbox",
                    json!({"box":"sent","fields":["full_id","properties"]}),
                    Some(RequestIdentity {
                        namespace: "local".into(),
                        actor_id: Some("actor:other".into()),
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();
            assert!(foreign["messages"].as_array().unwrap().is_empty());
            let other_namespace = registry
                .dispatch(
                    "comm.inbox",
                    json!({"box":"sent","namespace":"elsewhere","fields":["full_id","properties"]}),
                )
                .await
                .unwrap();
            assert!(other_namespace["messages"].as_array().unwrap().is_empty());
        }
    }
    #[tokio::test]
    async fn accepted_send_without_stamp_redelivers_the_same_claimed_message_id() {
        let (server, runtime, id) = fixture("email:recipient@example.com").await;
        let runtime = runtime.with_outbound_email_policy(
            OutboundEmailPolicy::configured(vec!["recipient@example.com".into()]).unwrap(),
        );
        let channel = MockEmail::new([Outcome::AcceptedWithoutStamp, Outcome::Delivered]);
        let cancellation = CancellationToken::new();
        {
            let first = cycle(&runtime, &channel, &cancellation);
            tokio::pin!(first);
            tokio::select! {
                _ = &mut first => panic!("simulated crash window must precede delivery stamp"),
                _ = channel.accepted.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                    panic!("mock never accepted outbound mail")
                }
            }
        }
        let props = sent_properties(&server, id).await;
        let claimed = format!("<{id}@example.com>");
        assert_eq!(props["external_id"], claimed);
        assert!(props.get("delivery").is_none());
        cycle(&runtime, &channel, &cancellation).await;
        cycle(&runtime, &channel, &cancellation).await;
        {
            let sent = channel.sent.lock().unwrap();
            assert_eq!(sent.len(), 2);
            assert_eq!(
                serde_json::to_value(&sent[0]).unwrap()["message_id"],
                claimed
            );
            assert_eq!(
                serde_json::to_value(&sent[1]).unwrap()["message_id"],
                claimed
            );
        }
        assert_eq!(sent_properties(&server, id).await["delivery"], "delivered");
    }
    #[tokio::test]
    async fn permanent_rejection_is_per_message_and_cancellation_settles_inflight_success() {
        for cancel in [false, true] {
            let (server, runtime, first) = fixture("email:recipient@example.com").await;
            let second = server
                .verb_registry_clone()
                .dispatch(
                    "comm.send",
                    json!({"to":"email:recipient@example.com","content":"second queued email"}),
                )
                .await
                .unwrap();
            let second: uuid::Uuid = second["full_id"].as_str().unwrap().parse().unwrap();
            let runtime = runtime.with_outbound_email_policy(
                OutboundEmailPolicy::configured(vec!["recipient@example.com".into()]).unwrap(),
            );
            let cancellation = CancellationToken::new();
            let mut channel = MockEmail::new(if cancel {
                vec![Outcome::Delivered]
            } else {
                vec![Outcome::Permanent, Outcome::Delivered]
            });
            if cancel {
                channel.cancellation = Some(cancellation.clone());
            }
            cycle(&runtime, &channel, &cancellation).await;
            let props = [
                sent_properties(&server, first).await,
                sent_properties(&server, second).await,
            ];
            assert_eq!(
                props
                    .iter()
                    .filter(|p| p["delivery"] == "delivered")
                    .count(),
                1
            );
            if cancel {
                assert_eq!(channel.sent.lock().unwrap().len(), 1);
                assert_eq!(
                    props.iter().filter(|p| p.get("delivery").is_none()).count(),
                    1
                );
            } else {
                assert_eq!(channel.sent.lock().unwrap().len(), 2);
                assert_eq!(
                    props.iter().filter(|p| p["delivery"] == "failed").count(),
                    1
                );
            }
        }
    }
}
