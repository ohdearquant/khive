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
    let mut pause_until = None;
    outbox::outbox_once(
        outbox::OutboxChannels::Registered { registry, slug },
        policy,
        runtime,
        &khive_runtime::Namespace::local(),
        &tokio_util::sync::CancellationToken::new(),
        &mut pause_until,
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

#[tokio::test]
async fn held_channel_rows_cannot_fill_the_page_ahead_of_an_eligible_row() {
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

        let eligible_id = seed(&runtime, &token, kind, Some("a@example.com")).await;
        let store = runtime
            .backend()
            .notes_for_namespace(token.namespace().as_str())
            .unwrap();
        let mut eligible = store.get_note(eligible_id).await.unwrap().unwrap();
        eligible.created_at -= 10_000_000;
        eligible.updated_at = eligible.created_at;
        store.upsert_note(eligible).await.unwrap();

        // Before the channel-scoped SQL predicate, these 201 newer held
        // rows filled the 200-row page on every pass, so the valid row was
        // never handed to its adapter.
        let mut held_ids = Vec::new();
        for index in 0..201 {
            held_ids.push(
                seed(
                    &runtime,
                    &token,
                    kind,
                    (index % 2 == 0).then_some("missing@example.com"),
                )
                .await,
            );
        }

        pass(&runtime, &registry, kind, "a@example.com").await;
        assert_eq!(a.sent.lock().unwrap().len(), 1, "{kind} eligible send");
        assert!(b.sent.lock().unwrap().is_empty(), "{kind} wrong adapter");
        assert_eq!(
            props(&runtime, &token, eligible_id).await["delivery"],
            "delivered"
        );
        for held_id in held_ids {
            assert!(
                props(&runtime, &token, held_id)
                    .await
                    .get("delivery")
                    .is_none(),
                "{kind} held row remains pending"
            );
        }
    }
}
