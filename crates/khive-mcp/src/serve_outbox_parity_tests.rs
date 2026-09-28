#[cfg(all(test, feature = "channel-email", feature = "channel-telegram"))]
mod outbox_parity_tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use khive_channel::{Channel, ChannelEnvelope, ChannelError};
    use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken};
    use khive_storage::note::Note;
    use serde_json::{json, Value};
    use std::sync::Mutex;

    #[derive(Clone, Copy)]
    pub(super) enum Outcome {
        Success,
        Transient,
        Permanent,
        Auth,
    }

    pub(super) struct RecordingChannel {
        pub kind: &'static str,
        pub slug: String,
        pub outcome: Mutex<Outcome>,
        pub sent: Mutex<Vec<Value>>,
        pub observed: Mutex<Vec<Value>>,
        pub watch: Mutex<Option<(KhiveRuntime, uuid::Uuid)>>,
    }

    impl RecordingChannel {
        pub fn new(kind: &'static str, slug: &str, outcome: Outcome) -> Self {
            Self {
                kind,
                slug: slug.into(),
                outcome: Mutex::new(outcome),
                sent: Mutex::new(Vec::new()),
                observed: Mutex::new(Vec::new()),
                watch: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl Channel for RecordingChannel {
        fn kind(&self) -> &'static str {
            self.kind
        }
        fn slug(&self) -> String {
            self.slug.clone()
        }
        async fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
            let watch = self.watch.lock().unwrap().clone();
            if let Some((runtime, id)) = watch {
                let token = runtime.authorize(Namespace::local()).unwrap();
                let observed = props(&runtime, &token, id).await;
                self.observed.lock().unwrap().push(observed);
            }
            assert!(envelope.quarantine_replay.is_none());
            self.sent
                .lock()
                .unwrap()
                .push(serde_json::to_value(envelope).unwrap());
            match *self.outcome.lock().unwrap() {
                Outcome::Success => Ok(()),
                Outcome::Transient => Err(ChannelError::Transport("temporary pressure".into())),
                Outcome::Permanent => Err(ChannelError::PermanentTransport(
                    "recipient rejected".into(),
                )),
                Outcome::Auth => Err(ChannelError::Auth("authentication failed".into())),
            }
        }
        async fn poll(&self, _: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
            Ok(Vec::new())
        }
    }

    pub(super) fn fixture() -> (KhiveRuntime, NamespaceToken) {
        let runtime = KhiveRuntime::memory().unwrap();
        runtime.install_pack_owned_note_kinds(vec!["message".into()]);
        let token = runtime.authorize(Namespace::local()).unwrap();
        (runtime, token)
    }

    pub(super) async fn seed(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        kind: &str,
        slug: Option<&str>,
    ) -> uuid::Uuid {
        let mut note = Note::new("local", "message", "body\nwith unicode: 文");
        let mut props = json!({"direction":"outbound", "to_actor":format!("{kind}:recipient@example.com"),
        "subject":"subject", "thread_id":"thread", "in_reply_to_message_id":"<parent@example.com>",
        "references_chain":"<ancestor@example.com> <parent@example.com>"});
        if let Some(slug) = slug {
            props["channel_slug"] = json!(slug);
        }
        note.properties = Some(props);
        let id = note.id;
        runtime
            .backend()
            .notes_for_namespace(token.namespace().as_str())
            .unwrap()
            .upsert_note(note)
            .await
            .unwrap();
        id
    }

    pub(super) async fn props(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        id: uuid::Uuid,
    ) -> Value {
        runtime
            .notes(token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap()
            .properties
            .unwrap()
    }

    async fn cycle(
        runtime: &KhiveRuntime,
        channel: &RecordingChannel,
    ) -> Result<(), crate::components::ComponentError> {
        let cancellation = tokio_util::sync::CancellationToken::new();
        if channel.kind == "email" {
            channel_outbox_once(
                channel,
                runtime,
                &Namespace::local(),
                "sender@example.com",
                "example.com",
                &["recipient@example.com".into()],
                &cancellation,
            )
            .await
        } else {
            telegram_outbox_once(channel, runtime, &Namespace::local(), &cancellation).await
        }
    }

    async fn email_cycle_with_domains(
        runtime: &KhiveRuntime,
        channel: &RecordingChannel,
        domains: &khive_runtime::EmailMessageIdDomains,
    ) {
        outbox::outbox_once(
            outbox::OutboxChannels::Single(channel),
            outbox::OutboxPolicy::Email {
                mailbox: domains.mailbox(),
                domains,
                allowlist: &["recipient@example.com".into()],
            },
            runtime,
            &Namespace::local(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    }

    fn expected(kind: &str, id: uuid::Uuid) -> Value {
        let mut envelope = ChannelEnvelope::new(
            if kind == "email" {
                "email:sender@example.com"
            } else {
                "telegram:bot"
            },
            format!("{kind}:recipient@example.com"),
            "body\nwith unicode: 文",
        );
        if kind == "email" {
            envelope = envelope
                .with_subject("subject")
                .with_message_id(format!("<{id}@example.com>"))
                .with_correlation("thread")
                .with_in_reply_to("<parent@example.com>")
                .with_references("<ancestor@example.com> <parent@example.com>");
        }
        serde_json::to_value(envelope).unwrap()
    }

    #[tokio::test]
    async fn delivered_transcript_per_kind() {
        for kind in ["email", "telegram"] {
            let (runtime, token) = fixture();
            let id = seed(&runtime, &token, kind, None).await;
            let channel = RecordingChannel::new(kind, "sender@example.com", Outcome::Success);
            *channel.watch.lock().unwrap() = Some((runtime.clone(), id));
            cycle(&runtime, &channel).await.unwrap();
            assert_eq!(
                *channel.sent.lock().unwrap(),
                vec![expected(kind, id)],
                "full {kind} envelope"
            );
            let before_send = channel.observed.lock().unwrap()[0].clone();
            assert!(before_send.get("delivery").is_none());
            assert!(before_send.get("delivered_at").is_none());
            if kind == "email" {
                assert_eq!(before_send["external_id"], format!("<{id}@example.com>"));
            } else {
                assert!(before_send.get("external_id").is_none());
            }
            let p = props(&runtime, &token, id).await;
            assert_eq!(p["delivery"], "delivered");
            assert!(p["delivered_at"].as_str().is_some());
            if kind == "email" {
                assert_eq!(p["external_id"], format!("<{id}@example.com>"));
            } else {
                assert!(p.get("external_id").is_none());
            }
            cycle(&runtime, &channel).await.unwrap();
            assert_eq!(channel.sent.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn transient_retry_transcript_per_kind() {
        for kind in ["email", "telegram"] {
            let (runtime, token) = fixture();
            let id = seed(&runtime, &token, kind, None).await;
            let channel = RecordingChannel::new(kind, "sender@example.com", Outcome::Transient);
            cycle(&runtime, &channel).await.unwrap();
            let p = props(&runtime, &token, id).await;
            assert_eq!(p["delivery_attempts"], 1);
            assert!(p["next_attempt_at"].as_str().is_some());
            assert!(p.get("delivery").is_none());
            cycle(&runtime, &channel).await.unwrap();
            assert_eq!(channel.sent.lock().unwrap().len(), 1);
            let mut note = runtime
                .notes(&token)
                .unwrap()
                .get_note(id)
                .await
                .unwrap()
                .unwrap();
            note.properties.as_mut().unwrap()["next_attempt_at"] = json!("2000-01-01T00:00:00Z");
            runtime
                .notes(&token)
                .unwrap()
                .upsert_note(note)
                .await
                .unwrap();
            *channel.outcome.lock().unwrap() = Outcome::Success;
            cycle(&runtime, &channel).await.unwrap();
            assert_eq!(
                *channel.sent.lock().unwrap(),
                vec![expected(kind, id), expected(kind, id)]
            );
            assert_eq!(props(&runtime, &token, id).await["delivery"], "delivered");
        }
    }

    #[tokio::test]
    async fn permanent_transcript_per_kind() {
        for kind in ["email", "telegram"] {
            let (runtime, token) = fixture();
            let id = seed(&runtime, &token, kind, None).await;
            let channel = RecordingChannel::new(kind, "sender@example.com", Outcome::Permanent);
            cycle(&runtime, &channel).await.unwrap();
            assert_eq!(*channel.sent.lock().unwrap(), vec![expected(kind, id)]);
            assert_eq!(props(&runtime, &token, id).await["delivery"], "failed");
            cycle(&runtime, &channel).await.unwrap();
            assert_eq!(channel.sent.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn auth_preserves_delivery_state_per_kind() {
        for kind in ["email", "telegram"] {
            let (runtime, token) = fixture();
            let id = seed(&runtime, &token, kind, None).await;
            let channel = RecordingChannel::new(kind, "sender@example.com", Outcome::Auth);
            assert!(
                matches!(
                    cycle(&runtime, &channel).await,
                    Err(crate::components::ComponentError::Permanent(_))
                ),
                "Auth must return permanent component error"
            );
            assert_eq!(*channel.sent.lock().unwrap(), vec![expected(kind, id)]);
            let p = props(&runtime, &token, id).await;
            for field in [
                "delivery",
                "delivery_attempts",
                "next_attempt_at",
                "delivered_at",
                "failed_at",
                "last_error",
            ] {
                assert!(p.get(field).is_none(), "Auth changed {kind} {field}");
            }
            if kind == "email" {
                assert_eq!(p["external_id"], format!("<{id}@example.com>"));
            } else {
                assert!(p.get("external_id").is_none());
            }
        }
    }

    #[tokio::test]
    async fn outbox_reuses_current_and_configured_historical_own_message_ids() {
        let domains = khive_runtime::EmailMessageIdDomains::from_mailbox_and_history(
            "sender@example.com",
            "former.example",
        )
        .unwrap();
        for domain in ["example.com", "former.example"] {
            let (runtime, token) = fixture();
            let id = seed(&runtime, &token, "email", None).await;
            let message_id = format!("<{id}@{domain}>");
            let mut note = runtime
                .notes(&token)
                .unwrap()
                .get_note(id)
                .await
                .unwrap()
                .unwrap();
            note.properties.as_mut().unwrap()["external_id"] = json!(message_id);
            runtime
                .notes(&token)
                .unwrap()
                .upsert_note(note)
                .await
                .unwrap();
            let channel = RecordingChannel::new("email", "sender@example.com", Outcome::Success);
            email_cycle_with_domains(&runtime, &channel, &domains).await;
            {
                let sent = channel.sent.lock().unwrap();
                assert_eq!(sent.len(), 1);
                assert_eq!(sent[0]["message_id"], message_id);
            }
            assert_eq!(props(&runtime, &token, id).await["external_id"], message_id);
        }
    }

    #[tokio::test]
    async fn outbox_parks_copied_victim_id_without_smtp_and_links_one_diagnostic() {
        let domains = khive_runtime::EmailMessageIdDomains::from_mailbox_and_history(
            "sender@example.com",
            "former.example",
        )
        .unwrap();
        let (runtime, token) = fixture();
        let victim = seed(&runtime, &token, "email", None).await;
        let copier = seed(&runtime, &token, "email", None).await;
        let mut victim_note = runtime
            .notes(&token)
            .unwrap()
            .get_note(victim)
            .await
            .unwrap()
            .unwrap();
        victim_note.properties.as_mut().unwrap()["delivery"] = json!("delivered");
        runtime
            .notes(&token)
            .unwrap()
            .upsert_note(victim_note)
            .await
            .unwrap();
        let mut copied = runtime
            .notes(&token)
            .unwrap()
            .get_note(copier)
            .await
            .unwrap()
            .unwrap();
        copied.properties.as_mut().unwrap()["external_id"] =
            json!(format!("<{victim}@former.example>"));
        runtime
            .notes(&token)
            .unwrap()
            .upsert_note(copied)
            .await
            .unwrap();
        let channel = RecordingChannel::new("email", "sender@example.com", Outcome::Success);

        email_cycle_with_domains(&runtime, &channel, &domains).await;
        assert!(
            channel.sent.lock().unwrap().is_empty(),
            "no SMTP call is allowed"
        );
        let held = props(&runtime, &token, copier).await;
        assert_eq!(held["delivery_hold"], "external_id_unverifiable");
        assert!(held["delivery_hold_reason"].as_str().is_some());
        assert!(held.get("delivered_at").is_none());
        assert_ne!(held["delivery"], "failed");
        let diagnostics = runtime
            .list_notes(&token, Some("observation"), 20, 0)
            .await
            .unwrap();
        assert_eq!(diagnostics.len(), 1, "one keyed diagnostic must be visible");
        let edges = runtime
            .list_edges(
                &token,
                khive_runtime::EdgeListFilter {
                    source_id: Some(diagnostics[0].id),
                    target_id: Some(copier),
                    ..Default::default()
                },
                20,
                0,
            )
            .await
            .unwrap();
        assert_eq!(edges.len(), 1, "diagnostic must annotate the offending row");
        email_cycle_with_domains(&runtime, &channel, &domains).await;
        assert!(channel.sent.lock().unwrap().is_empty());
        assert_eq!(
            runtime
                .list_notes(&token, Some("observation"), 20, 0)
                .await
                .unwrap()
                .len(),
            1,
            "repeated passes must not duplicate the keyed diagnostic"
        );
    }

    #[tokio::test]
    async fn outbox_parks_malformed_and_unconfigured_ids_even_when_recipient_is_denied() {
        let domains = khive_runtime::EmailMessageIdDomains::from_mailbox_and_history(
            "sender@example.com",
            "former.example",
        )
        .unwrap();
        for malformed in [
            "not-a-message-id".to_string(),
            "<own@unconfigured.example>".to_string(),
        ] {
            let (runtime, token) = fixture();
            let id = seed(&runtime, &token, "email", None).await;
            let mut note = runtime
                .notes(&token)
                .unwrap()
                .get_note(id)
                .await
                .unwrap()
                .unwrap();
            note.properties.as_mut().unwrap()["external_id"] = json!(malformed);
            runtime
                .notes(&token)
                .unwrap()
                .upsert_note(note)
                .await
                .unwrap();
            let channel = RecordingChannel::new("email", "sender@example.com", Outcome::Success);
            outbox::outbox_once(
                outbox::OutboxChannels::Single(&channel),
                outbox::OutboxPolicy::Email {
                    mailbox: domains.mailbox(),
                    domains: &domains,
                    allowlist: &["someone-else@example.com".into()],
                },
                &runtime,
                &Namespace::local(),
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(channel.sent.lock().unwrap().is_empty());
            let held = props(&runtime, &token, id).await;
            assert_eq!(held["delivery_hold"], "external_id_unverifiable");
            assert_ne!(held["delivery"], "failed");
            assert_eq!(
                runtime
                    .list_notes(&token, Some("observation"), 20, 0)
                    .await
                    .unwrap()
                    .len(),
                1
            );
        }
    }

    #[tokio::test]
    async fn outbox_reports_diagnostic_write_failure_and_recovers_without_smtp() {
        let domains = khive_runtime::EmailMessageIdDomains::from_mailbox_and_history(
            "sender@example.com",
            "",
        )
        .unwrap();
        let (runtime, token) = fixture();
        let id = seed(&runtime, &token, "email", None).await;
        let mut note = runtime
            .notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap();
        note.properties.as_mut().unwrap()["external_id"] =
            json!(format!("<{}@example.com>", uuid::Uuid::new_v4()));
        runtime
            .notes(&token)
            .unwrap()
            .upsert_note(note)
            .await
            .unwrap();
        // Deny the observation kind to force the keyed diagnostic write to fail
        // after the owner hold succeeds.
        runtime.install_kind_registry(vec![], vec!["message".into()]);
        let channel = RecordingChannel::new("email", "sender@example.com", Outcome::Success);
        let first = outbox::outbox_once(
            outbox::OutboxChannels::Single(&channel),
            outbox::OutboxPolicy::Email {
                mailbox: domains.mailbox(),
                domains: &domains,
                allowlist: &["recipient@example.com".into()],
            },
            &runtime,
            &Namespace::local(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(matches!(
            first,
            Err(crate::components::ComponentError::Retryable(_))
        ));
        assert!(channel.sent.lock().unwrap().is_empty());
        assert_eq!(
            props(&runtime, &token, id).await["delivery_hold"],
            "external_id_unverifiable"
        );
        assert!(props(&runtime, &token, id)
            .await
            .get("external_id_diagnostic_note_id")
            .is_none());

        runtime.install_kind_registry(vec![], vec!["message".into(), "observation".into()]);
        email_cycle_with_domains(&runtime, &channel, &domains).await;
        assert!(channel.sent.lock().unwrap().is_empty());
        assert!(
            props(&runtime, &token, id).await["external_id_diagnostic_note_id"]
                .as_str()
                .is_some()
        );
        assert_eq!(
            runtime
                .list_notes(&token, Some("observation"), 20, 0)
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
