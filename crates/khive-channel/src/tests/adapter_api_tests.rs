#[test]
fn envelope_builder_fields() {
    let ts = Utc::now();
    let env = ChannelEnvelope::new("email:a@example.com", "email:b@example.com", "hello")
        .with_subject("Test")
        .with_sent_at(ts)
        .with_external_id("<msg1@example.com>")
        .with_correlation("correlation-uuid")
        .with_message_id("<abc123@example.com>")
        .with_wire_message_id("<wire123@example.com>")
        .with_wire_references("<ref1@example.com> <ref2@example.com>")
        .with_in_reply_to("<parent123@example.com>")
        .with_references("<ref1@example.com> <parent123@example.com>");

    assert_eq!(env.from, "email:a@example.com");
    assert_eq!(env.to, "email:b@example.com");
    assert_eq!(env.content, "hello");
    assert_eq!(env.subject.as_deref(), Some("Test"));
    assert_eq!(env.sent_at, Some(ts));
    assert_eq!(env.external_id.as_deref(), Some("<msg1@example.com>"));
    assert_eq!(
        env.correlation_external_id.as_deref(),
        Some("correlation-uuid")
    );
    assert_eq!(env.message_id.as_deref(), Some("<abc123@example.com>"));
    assert_eq!(
        env.wire_message_id.as_deref(),
        Some("<wire123@example.com>")
    );
    assert_eq!(
        env.wire_references.as_deref(),
        Some("<ref1@example.com> <ref2@example.com>")
    );
    assert_eq!(env.in_reply_to.as_deref(), Some("<parent123@example.com>"));
    assert_eq!(
        env.references.as_deref(),
        Some("<ref1@example.com> <parent123@example.com>")
    );
}

#[test]
fn envelope_new_defaults_wire_message_id_and_in_reply_to_to_none() {
    let env = ChannelEnvelope::new("email:a@example.com", "email:b@example.com", "hello");
    assert_eq!(env.wire_message_id, None);
    assert_eq!(env.wire_references, None);
    assert_eq!(env.in_reply_to, None);
    assert_eq!(env.references, None);
}

#[test]
fn registry_register_and_get() {
    let mut reg = ChannelRegistry::new();
    let ch = Arc::new(MockChannel::new(vec![]));
    reg.register(ch);
    assert!(reg.get("mock").is_some());
    assert!(reg.get("email").is_none());
    assert_eq!(reg.len(), 1);
    assert!(!reg.is_empty());
}

#[test]
fn registry_replaces_existing_same_composite_identity() {
    // Two registrations with the SAME (kind, slug) — both default to
    // slug() == kind() here — still replace in place, matching the
    // pre-#606 single-credential-per-kind semantics.
    let mut reg = ChannelRegistry::new();
    reg.register(Arc::new(MockChannel::new(vec![])));
    reg.register(Arc::new(MockChannel::new(vec![])));
    assert_eq!(reg.len(), 1, "same (kind, slug) replaces");
}

#[test]
fn registry_does_not_collapse_same_kind_distinct_slug() {
    // #606: two accounts sharing
    // `kind()` but with distinct `slug()` values (e.g. two mailboxes)
    // must coexist as two registered adapters, not collapse into one.
    let mut reg = ChannelRegistry::new();
    reg.register(Arc::new(MockChannel::with_slug(vec![], "a@example.com")));
    reg.register(Arc::new(MockChannel::with_slug(vec![], "b@example.com")));
    assert_eq!(
        reg.len(),
        2,
        "same kind + distinct slug must coexist, not collapse"
    );
    assert!(reg.get_by_slug("mock", "a@example.com").is_some());
    assert!(reg.get_by_slug("mock", "b@example.com").is_some());
}

#[test]
fn registry_iter_yields_all() {
    let mut reg = ChannelRegistry::new();
    reg.register(Arc::new(MockChannel::new(vec![])));
    let kinds: Vec<&str> = reg.iter().map(|(k, _, _)| k).collect();
    assert_eq!(kinds, vec!["mock"]);
}

#[test]
fn registry_iter_yields_both_slugs_for_same_kind() {
    let mut reg = ChannelRegistry::new();
    reg.register(Arc::new(MockChannel::with_slug(vec![], "a@example.com")));
    reg.register(Arc::new(MockChannel::with_slug(vec![], "b@example.com")));
    let mut slugs: Vec<&str> = reg.iter().map(|(_, s, _)| s).collect();
    slugs.sort_unstable();
    assert_eq!(slugs, vec!["a@example.com", "b@example.com"]);
}

#[test]
fn channel_error_display() {
    let e = ChannelError::Config("missing host".into());
    assert!(e.to_string().contains("missing host"));
    let e2 = ChannelError::UnauthorizedSender("attacker@example.com".into());
    assert!(e2.to_string().contains("attacker@example.com"));
}

#[test]
fn outbound_delivery_failure_classification_separates_pressure_from_rejection() {
    assert_eq!(
        ChannelError::Transport("connection reset".into()).delivery_failure_class(),
        DeliveryFailureClass::Transient
    );
    assert_eq!(
        ChannelError::RetryableAuth("minted token rejected".into()).delivery_failure_class(),
        DeliveryFailureClass::Transient
    );
    assert_eq!(
        ChannelError::PermanentTransport("550 recipient rejected".into()).delivery_failure_class(),
        DeliveryFailureClass::Permanent
    );
    for error in [
        ChannelError::Config("missing host".into()),
        ChannelError::Auth("invalid credentials".into()),
        ChannelError::UnauthorizedSender("denied".into()),
        ChannelError::InvalidEnvelope("bad recipient".into()),
    ] {
        assert_eq!(
            error.delivery_failure_class(),
            DeliveryFailureClass::Permanent,
            "static failure must not remain in a raw-cadence retry loop: {error}"
        );
    }
}

#[test]
fn new_thread_correlation_id_is_uuid() {
    let id = new_thread_correlation_id();
    assert!(
        id.parse::<Uuid>().is_ok(),
        "correlation id must be a valid UUID"
    );
}

#[test]
fn is_configured_default_returns_true() {
    let ch = MockChannel::new(vec![]);
    // Default impl must return true; concrete adapters may override.
    assert!(
        ch.is_configured(),
        "default is_configured() must return true"
    );
}

#[tokio::test]
async fn default_poll_page_wraps_legacy_poll_without_checkpoint() {
    // #449: an adapter that implements only `poll` (the pre-checkpoint
    // shape) must still work through `poll_page`, returning the same
    // envelopes with no checkpoint to persist.
    let inbound =
        vec![
            ChannelEnvelope::new("email:sender@example.com", "email:me@example.com", "body")
                .with_external_id("<id1@example.com>"),
        ];
    let ch = MockChannel::new(inbound.clone());
    let page = ch.poll_page(Utc::now(), None).await.expect("poll_page ok");
    assert_eq!(page.envelopes.len(), 1);
    assert_eq!(
        page.envelopes[0].external_id.as_deref(),
        Some("<id1@example.com>")
    );
    assert!(
        page.next_checkpoint.is_none(),
        "default poll_page must never produce a checkpoint"
    );
}

#[tokio::test]
async fn mock_channel_send_and_poll() {
    let inbound =
        vec![
            ChannelEnvelope::new("email:sender@example.com", "email:me@example.com", "body")
                .with_external_id("<id1@example.com>"),
        ];
    let ch = Arc::new(MockChannel::new(inbound.clone()));
    let env_out = ChannelEnvelope::new("email:me@example.com", "email:them@example.com", "reply");
    ch.send(env_out).await.expect("send ok");
    assert_eq!(ch.sent.lock().unwrap().len(), 1);

    let polled = ch.poll(Utc::now()).await.expect("poll ok");
    assert_eq!(polled.len(), 1);
    assert_eq!(polled[0].external_id.as_deref(), Some("<id1@example.com>"));
}
