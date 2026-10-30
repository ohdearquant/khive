const NODE_CHANNEL_AGENT: &str = "11111111-1111-4111-8111-111111111111";
const NODE_CHANNEL_DEVICE: &str = "22222222-2222-4222-8222-222222222222";
const NODE_CHANNEL_PEER_AGENT: &str = "33333333-3333-4333-8333-333333333333";
const NODE_CHANNEL_PEER_DEVICE: &str = "44444444-4444-4444-8444-444444444444";

fn node_channel_pin(
    agent: &str,
    device: &str,
    epoch: u64,
) -> khive_runtime::node_config::NodePinConfig {
    khive_runtime::node_config::NodePinConfig {
        agent_id: agent.to_string(),
        device_id: device.to_string(),
        key_epoch: epoch,
        kem_public_key: "a".repeat(64),
        signing_public_key: "b".repeat(64),
        fingerprint: "c".repeat(64),
    }
}

fn node_channel_table(actor: &str) -> khive_runtime::node_config::NodeChannelConfig {
    khive_runtime::node_config::NodeChannelConfig {
        enabled: true,
        slug: Some("north-1".to_string()),
        namespace: "local".to_string(),
        actor: Some(actor.to_string()),
        realm: Some("realm.one".to_string()),
        service_url: Some("https://channel.example.com".to_string()),
        agent_id: Some(NODE_CHANNEL_AGENT.to_string()),
        device_id: Some(NODE_CHANNEL_DEVICE.to_string()),
        key_epoch: Some(1),
        key_reference: Some("node-key.1".to_string()),
        key_fingerprint: Some("d".repeat(64)),
        poll_wait_seconds: 25,
        outbox_interval_seconds: 5,
        batch_size: 64,
        pins: vec![node_channel_pin(
            NODE_CHANNEL_PEER_AGENT,
            NODE_CHANNEL_PEER_DEVICE,
            1,
        )],
    }
}

fn node_channel_id(node: Option<khive_runtime::node_config::NodeChannelConfig>) -> String {
    let runtime = RuntimeConfig {
        node_channel: node,
        ..RuntimeConfig::no_embeddings()
    };
    compute_config_id(&runtime, None)
}

#[test]
#[serial_test::serial(config_ledger)]
fn node_channel_segment_is_absent_without_an_enabled_table() {
    let absent = node_channel_id(None);
    assert!(!absent.contains("node_channel"));

    let disabled = node_channel_id(Some(
        khive_runtime::node_config::NodeChannelConfig::default(),
    ));
    assert_eq!(
        disabled, absent,
        "a disabled table must leave the identity unchanged"
    );
}

#[test]
#[serial_test::serial(config_ledger)]
fn node_channel_segment_carries_the_digest_after_visibility_receipts() {
    let table = node_channel_table("node-actor");
    let digest = table.config_digest();
    assert_eq!(digest.len(), 64);

    let id = node_channel_id(Some(table));
    assert!(
        id.contains(&format!(";node_channel={digest}")),
        "id must carry the table digest: {id}"
    );
    let receipts = id
        .find(";visibility_receipts=")
        .expect("visibility_receipts segment");
    let node = id.find(";node_channel=").expect("node_channel segment");
    assert!(receipts < node, "node_channel follows visibility_receipts");
    assert_ne!(id, node_channel_id(None));
}

#[test]
#[serial_test::serial(config_ledger)]
fn node_channel_identity_tracks_the_bound_actor_and_the_pins() {
    let base = node_channel_id(Some(node_channel_table("node-actor")));
    assert_ne!(
        base,
        node_channel_id(Some(node_channel_table("other-actor"))),
        "the bound transport actor is part of the identity"
    );

    let mut more_pins = node_channel_table("node-actor");
    more_pins
        .pins
        .push(node_channel_pin(NODE_CHANNEL_AGENT, NODE_CHANNEL_DEVICE, 2));
    assert_ne!(base, node_channel_id(Some(more_pins.clone())));

    let mut reversed = more_pins.clone();
    reversed.pins.reverse();
    assert_eq!(
        node_channel_id(Some(more_pins)),
        node_channel_id(Some(reversed)),
        "pin order must not change the identity"
    );
}
