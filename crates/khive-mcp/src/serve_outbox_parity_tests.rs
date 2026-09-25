#[cfg(all(test, feature = "channel-email", feature = "channel-telegram"))]
mod outbox_parity_tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use khive_channel::{Channel, ChannelEnvelope, ChannelError};
    use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken};
    use khive_storage::note::Note;
    use serde_json::{Value, json};
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
}
