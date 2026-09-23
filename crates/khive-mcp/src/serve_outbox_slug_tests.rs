use super::outbox_parity_tests::{fixture, props, seed, Outcome, RecordingChannel};
use super::*;
use khive_channel::ChannelRegistry;

async fn pass(runtime: &KhiveRuntime, registry: &ChannelRegistry, kind: &str, slug: &str) {
    let allowlist = vec!["recipient@example.com".to_string()];
    let policy = if kind == "email" {
        outbox::OutboxPolicy::Email {
            mailbox: slug,
            domain: "example.com",
            allowlist: &allowlist,
        }
    } else {
        outbox::OutboxPolicy::Telegram(std::marker::PhantomData)
    };
    outbox::outbox_once(
        outbox::OutboxChannels::Registered { registry, slug },
        policy,
        runtime,
        &khive_runtime::Namespace::local(),
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn exact_slug_isolates_both_kinds() {
    for kind in ["email", "telegram"] {
        let (runtime, token) = fixture();
        let a = Arc::new(RecordingChannel::new(
            kind,
            "a@example.com",
            Outcome::Success,
        ));
        let b = Arc::new(RecordingChannel::new(
            kind,
            "b@example.com",
            Outcome::Success,
        ));
        let mut registry = ChannelRegistry::new();
        registry.register(a.clone());
        registry.register(b.clone());
        let id_a = seed(&runtime, &token, kind, Some("a@example.com")).await;
        let id_b = seed(&runtime, &token, kind, Some("b@example.com")).await;
        pass(&runtime, &registry, kind, "a@example.com").await;
        assert_eq!(
            a.sent.lock().unwrap().len(),
            1,
            "exact slug adapter send count"
        );
        assert!(
            b.sent.lock().unwrap().is_empty(),
            "unselected adapter must not send"
        );
        assert_eq!(props(&runtime, &token, id_a).await["delivery"], "delivered");
        assert!(props(&runtime, &token, id_b)
            .await
            .get("delivery")
            .is_none());
        assert!(props(&runtime, &token, id_b)
            .await
            .get("external_id")
            .is_none());
        pass(&runtime, &registry, kind, "b@example.com").await;
        assert_eq!(
            a.sent.lock().unwrap().len(),
            1,
            "exact slug adapter send count"
        );
        assert_eq!(
            b.sent.lock().unwrap().len(),
            1,
            "exact slug adapter send count"
        );
        assert_eq!(props(&runtime, &token, id_b).await["delivery"], "delivered");
        if kind == "email" {
            assert_eq!(a.sent.lock().unwrap()[0]["from"], "email:a@example.com");
            assert_eq!(b.sent.lock().unwrap()[0]["from"], "email:b@example.com");
        }
    }
}

#[tokio::test]
async fn legacy_row_requires_one_adapter() {
    for kind in ["email", "telegram"] {
        let (runtime, token) = fixture();
        let a = Arc::new(RecordingChannel::new(
            kind,
            "a@example.com",
            Outcome::Success,
        ));
        let mut registry = ChannelRegistry::new();
        registry.register(a.clone());
        let id = seed(&runtime, &token, kind, None).await;
        pass(&runtime, &registry, kind, "a@example.com").await;
        assert_eq!(
            a.sent.lock().unwrap().len(),
            1,
            "exact slug adapter send count"
        );
        assert_eq!(props(&runtime, &token, id).await["delivery"], "delivered");
    }
}

#[tokio::test]
async fn ambiguous_legacy_and_unknown_slug_stay_pending() {
    for kind in ["email", "telegram"] {
        let (runtime, token) = fixture();
        let a = Arc::new(RecordingChannel::new(
            kind,
            "a@example.com",
            Outcome::Success,
        ));
        let b = Arc::new(RecordingChannel::new(
            kind,
            "b@example.com",
            Outcome::Success,
        ));
        let mut registry = ChannelRegistry::new();
        registry.register(a.clone());
        registry.register(b.clone());
        let legacy = seed(&runtime, &token, kind, None).await;
        let unknown = seed(&runtime, &token, kind, Some("missing@example.com")).await;
        let before_legacy = props(&runtime, &token, legacy).await;
        let before_unknown = props(&runtime, &token, unknown).await;
        for slug in ["a@example.com", "b@example.com"] {
            pass(&runtime, &registry, kind, slug).await;
        }
        assert!(
            a.sent.lock().unwrap().is_empty(),
            "ambiguous legacy adapter must not send"
        );
        assert!(
            b.sent.lock().unwrap().is_empty(),
            "ambiguous legacy adapter must not send"
        );
        assert_eq!(props(&runtime, &token, legacy).await, before_legacy);
        assert_eq!(props(&runtime, &token, unknown).await, before_unknown);
    }
}
