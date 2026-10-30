use crate::engine_config::KhiveConfig;
use crate::node_config::{ChannelSectionConfig, NodeChannelConfig, NodePinConfig, MAX_NODE_PINS};

/// One edit applied to a valid table by a table-driven test case.
type ConfigMutation = Box<dyn Fn(&mut NodeChannelConfig)>;

const AGENT: &str = "11111111-1111-4111-8111-111111111111";
const DEVICE: &str = "22222222-2222-4222-8222-222222222222";
const PEER_AGENT: &str = "33333333-3333-4333-8333-333333333333";
const PEER_DEVICE: &str = "44444444-4444-4444-8444-444444444444";

#[derive(serde::Deserialize)]
struct Doc {
    #[serde(default)]
    channel: ChannelSectionConfig,
}

fn hex(ch: char) -> String {
    ch.to_string().repeat(64)
}

fn pin(agent: &str, device: &str, epoch: u64) -> NodePinConfig {
    NodePinConfig {
        agent_id: agent.to_string(),
        device_id: device.to_string(),
        key_epoch: epoch,
        kem_public_key: hex('a'),
        signing_public_key: hex('b'),
        fingerprint: hex('c'),
    }
}

fn valid() -> NodeChannelConfig {
    NodeChannelConfig {
        enabled: true,
        slug: Some("north-1".to_string()),
        namespace: "local".to_string(),
        actor: Some("node-actor".to_string()),
        realm: Some("realm.one".to_string()),
        service_url: Some("https://channel.example.com".to_string()),
        agent_id: Some(AGENT.to_string()),
        device_id: Some(DEVICE.to_string()),
        key_epoch: Some(1),
        key_reference: Some("node-key.1".to_string()),
        key_fingerprint: Some(hex('d')),
        poll_wait_seconds: 25,
        outbox_interval_seconds: 5,
        batch_size: 64,
        pins: vec![pin(PEER_AGENT, PEER_DEVICE, 1)],
    }
}

fn refusal(config: &NodeChannelConfig) -> String {
    config
        .validate()
        .expect_err("config must be refused")
        .to_string()
}

fn parse(text: &str) -> Result<ChannelSectionConfig, toml::de::Error> {
    toml::from_str::<Doc>(text).map(|doc| doc.channel)
}

#[test]
fn a_complete_enabled_table_is_accepted() {
    valid().validate().expect("valid table");
}

#[test]
fn defaults_are_off_local_and_conservative() {
    let node = NodeChannelConfig::default();
    assert!(!node.enabled);
    assert_eq!(node.namespace, "local");
    assert_eq!(node.poll_wait_seconds, 25);
    assert_eq!(node.outbox_interval_seconds, 5);
    assert_eq!(node.batch_size, 64);
    assert!(node.pins.is_empty());
    assert!(node.slug.is_none() && node.actor.is_none() && node.key_reference.is_none());
    assert!(ChannelSectionConfig::default().node.is_none());
}

#[test]
fn an_absent_table_parses_to_no_node_and_validates() {
    let section = parse("").expect("empty document");
    assert!(section.node.is_none());
    section.validate().expect("absent table is valid");
}

#[test]
fn a_table_with_only_the_enable_flag_takes_every_default() {
    let section = parse("[channel.node]\nenabled = false\n").expect("parse");
    assert_eq!(section.node, Some(NodeChannelConfig::default()));
}

#[test]
fn a_disabled_table_skips_semantic_checks() {
    let section = parse(
        "[channel.node]\nenabled = false\nslug = \"NOT VALID\"\nbatch_size = 9999\nkey_epoch = 0\n",
    )
    .expect("parse");
    section.validate().expect("disabled table is not checked");
}

#[test]
fn a_disabled_table_still_rejects_unknown_keys_and_wrong_types() {
    assert!(parse("[channel.node]\nenabled = false\nbogus = 1\n").is_err());
    assert!(parse("[channel.node]\nenabled = false\npoll_wait_seconds = \"soon\"\n").is_err());
    assert!(parse("[channel]\nother = 1\n").is_err());
}

#[test]
fn a_pin_rejects_unknown_keys_and_missing_fields() {
    let missing = format!(
        "[[channel.node.pins]]\nagent_id = \"{AGENT}\"\ndevice_id = \"{DEVICE}\"\nkey_epoch = 1\n"
    );
    assert!(parse(&missing).is_err());
}

#[test]
fn an_enabled_table_missing_a_required_field_names_the_field() {
    let cases: Vec<(&str, ConfigMutation)> = vec![
        ("slug", Box::new(|c| c.slug = None)),
        ("actor", Box::new(|c| c.actor = None)),
        ("realm", Box::new(|c| c.realm = None)),
        ("service_url", Box::new(|c| c.service_url = None)),
        ("agent_id", Box::new(|c| c.agent_id = None)),
        ("device_id", Box::new(|c| c.device_id = None)),
        ("key_epoch", Box::new(|c| c.key_epoch = None)),
        ("key_reference", Box::new(|c| c.key_reference = None)),
        ("key_fingerprint", Box::new(|c| c.key_fingerprint = None)),
    ];
    for (field, mutate) in cases {
        let mut config = valid();
        mutate(&mut config);
        assert_eq!(
            refusal(&config),
            format!("channel.node: {field} is required when enabled")
        );
    }
}

#[test]
fn each_field_refusal_carries_its_exact_text() {
    let cases: Vec<(ConfigMutation, &str)> = vec![
        (
            Box::new(|c| c.slug = Some(String::new())),
            "slug must contain 1..=64 lowercase letters, digits, '_' or '-'",
        ),
        (
            Box::new(|c| c.slug = Some("Upper".to_string())),
            "slug must contain 1..=64 lowercase letters, digits, '_' or '-'",
        ),
        (
            Box::new(|c| c.slug = Some("a".repeat(65))),
            "slug must contain 1..=64 lowercase letters, digits, '_' or '-'",
        ),
        (
            Box::new(|c| c.namespace = String::new()),
            "namespace is invalid",
        ),
        (
            Box::new(|c| c.actor = Some("   ".to_string())),
            "actor must be a nonempty actor label of at most 256 bytes",
        ),
        (
            Box::new(|c| c.actor = Some("a".repeat(257))),
            "actor must be a nonempty actor label of at most 256 bytes",
        ),
        (
            Box::new(|c| c.actor = Some("bad\nactor".to_string())),
            "actor must be a nonempty actor label of at most 256 bytes",
        ),
        (
            Box::new(|c| c.realm = Some("Realm".to_string())),
            "realm must contain 1..=64 lowercase letters, digits, '.', '_' or '-'",
        ),
        (
            Box::new(|c| c.realm = Some("r".repeat(65))),
            "realm must contain 1..=64 lowercase letters, digits, '.', '_' or '-'",
        ),
        (
            Box::new(|c| c.service_url = Some("  ".to_string())),
            "service_url is required when enabled",
        ),
        (
            Box::new(|c| c.agent_id = Some("AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA".to_string())),
            "agent_id must be a canonical UUID",
        ),
        (
            Box::new(|c| c.agent_id = Some("not-a-uuid".to_string())),
            "agent_id must be a canonical UUID",
        ),
        (
            Box::new(|c| c.device_id = Some(DEVICE.replace('-', ""))),
            "device_id must be a canonical UUID",
        ),
        (
            Box::new(|c| c.key_epoch = Some(0)),
            "key_epoch must be in 1..=4294967295",
        ),
        (
            Box::new(|c| c.key_epoch = Some(4_294_967_296)),
            "key_epoch must be in 1..=4294967295",
        ),
        (
            Box::new(|c| c.key_reference = Some(String::new())),
            "key_reference must be a nonempty keychain account identifier of at most 128 bytes",
        ),
        (
            Box::new(|c| c.key_reference = Some("bad ref".to_string())),
            "key_reference must be a nonempty keychain account identifier of at most 128 bytes",
        ),
        (
            Box::new(|c| c.key_reference = Some("k".repeat(129))),
            "key_reference must be a nonempty keychain account identifier of at most 128 bytes",
        ),
        (
            Box::new(|c| c.key_fingerprint = Some("abc".to_string())),
            "key_fingerprint must be 64 lowercase hex characters",
        ),
        (
            Box::new(|c| c.key_fingerprint = Some(hex('D'))),
            "key_fingerprint must be 64 lowercase hex characters",
        ),
        (
            Box::new(|c| c.poll_wait_seconds = 26),
            "poll_wait_seconds must be in 0..=25",
        ),
        (
            Box::new(|c| c.outbox_interval_seconds = 0),
            "outbox_interval_seconds must be in 1..=60",
        ),
        (
            Box::new(|c| c.outbox_interval_seconds = 61),
            "outbox_interval_seconds must be in 1..=60",
        ),
        (
            Box::new(|c| c.batch_size = 0),
            "batch_size must be in 1..=256",
        ),
        (
            Box::new(|c| c.batch_size = 257),
            "batch_size must be in 1..=256",
        ),
    ];
    for (mutate, expected) in cases {
        let mut config = valid();
        mutate(&mut config);
        assert_eq!(refusal(&config), format!("channel.node: {expected}"));
    }
}

#[test]
fn the_range_edges_are_accepted() {
    let mut config = valid();
    config.poll_wait_seconds = 0;
    config.outbox_interval_seconds = 1;
    config.batch_size = 1;
    config.key_epoch = Some(4_294_967_295);
    config.slug = Some("s".repeat(64));
    config.realm = Some("r".repeat(64));
    config.key_reference = Some("k".repeat(128));
    config.actor = Some("a".repeat(256));
    config.validate().expect("edge values are valid");
    config.poll_wait_seconds = 25;
    config.outbox_interval_seconds = 60;
    config.batch_size = 256;
    config.validate().expect("upper edge values are valid");
}

#[test]
fn the_first_failure_in_table_order_is_reported() {
    let mut config = valid();
    config.slug = None;
    config.realm = Some("BAD".to_string());
    config.batch_size = 0;
    assert_eq!(
        refusal(&config),
        "channel.node: slug is required when enabled"
    );

    config.slug = Some("ok".to_string());
    assert!(refusal(&config).starts_with("channel.node: realm must contain"));

    config.realm = Some("ok".to_string());
    assert_eq!(
        refusal(&config),
        "channel.node: batch_size must be in 1..=256"
    );
}

#[test]
fn pin_fields_are_refused_with_their_index() {
    let mut config = valid();
    config.pins = vec![pin(PEER_AGENT, PEER_DEVICE, 1), pin("bad", PEER_DEVICE, 2)];
    assert_eq!(
        refusal(&config),
        "channel.node: pins[1].agent_id must be a canonical UUID"
    );

    config.pins = vec![pin(PEER_AGENT, "bad", 1)];
    assert_eq!(
        refusal(&config),
        "channel.node: pins[0].device_id must be a canonical UUID"
    );

    config.pins = vec![pin(PEER_AGENT, PEER_DEVICE, 0)];
    assert_eq!(
        refusal(&config),
        "channel.node: pins[0].key_epoch must be in 1..=4294967295"
    );

    for field in ["kem_public_key", "signing_public_key", "fingerprint"] {
        let mut bad = pin(PEER_AGENT, PEER_DEVICE, 1);
        match field {
            "kem_public_key" => bad.kem_public_key = "short".to_string(),
            "signing_public_key" => bad.signing_public_key = hex('G'),
            _ => bad.fingerprint = hex('F'),
        }
        config.pins = vec![bad];
        assert_eq!(
            refusal(&config),
            format!("channel.node: pins[0].{field} must be 64 lowercase hex characters")
        );
    }
}

#[test]
fn duplicate_pin_identities_are_refused_but_epochs_may_differ() {
    let mut config = valid();
    config.pins = vec![
        pin(PEER_AGENT, PEER_DEVICE, 1),
        pin(PEER_AGENT, PEER_DEVICE, 2),
    ];
    config
        .validate()
        .expect("different epochs are distinct identities");

    config.pins = vec![
        pin(PEER_AGENT, PEER_DEVICE, 1),
        pin(AGENT, DEVICE, 1),
        pin(PEER_AGENT, PEER_DEVICE, 1),
    ];
    assert_eq!(
        refusal(&config),
        "channel.node: pins contains a duplicate identity"
    );
}

#[test]
fn the_pin_count_is_capped() {
    let mut config = valid();
    config.pins = (0..=MAX_NODE_PINS)
        .map(|i| pin(PEER_AGENT, PEER_DEVICE, i as u64 + 1))
        .collect();
    assert_eq!(config.pins.len(), 4097);
    assert_eq!(refusal(&config), "channel.node: pins exceeds 4096 entries");

    config.pins.truncate(MAX_NODE_PINS);
    config.validate().expect("exactly the cap is accepted");
}

#[test]
fn the_digest_is_64_lowercase_hex_and_stable() {
    let digest = valid().config_digest();
    assert_eq!(digest.len(), 64);
    assert!(digest
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    assert_eq!(digest, valid().config_digest());
}

#[test]
fn the_digest_ignores_pin_order() {
    let mut forward = valid();
    forward.pins = vec![
        pin(AGENT, DEVICE, 1),
        pin(PEER_AGENT, PEER_DEVICE, 1),
        pin(PEER_AGENT, PEER_DEVICE, 2),
    ];
    let mut reversed = forward.clone();
    reversed.pins.reverse();
    assert_ne!(forward.pins, reversed.pins);
    assert_eq!(forward.config_digest(), reversed.config_digest());
}

#[test]
fn the_digest_changes_with_every_field() {
    let base = valid().config_digest();
    let variants: Vec<(&str, ConfigMutation)> = vec![
        (
            "actor",
            Box::new(|c| c.actor = Some("other-actor".to_string())),
        ),
        ("slug", Box::new(|c| c.slug = Some("south".to_string()))),
        (
            "realm",
            Box::new(|c| c.realm = Some("realm.two".to_string())),
        ),
        ("namespace", Box::new(|c| c.namespace = "other".to_string())),
        ("epoch", Box::new(|c| c.key_epoch = Some(2))),
        ("batch", Box::new(|c| c.batch_size = 65)),
        ("poll", Box::new(|c| c.poll_wait_seconds = 24)),
        ("outbox", Box::new(|c| c.outbox_interval_seconds = 6)),
        ("pin", Box::new(|c| c.pins.push(pin(AGENT, DEVICE, 9)))),
    ];
    for (name, mutate) in variants {
        let mut config = valid();
        mutate(&mut config);
        assert_ne!(
            config.config_digest(),
            base,
            "{name} must change the digest"
        );
    }
}

#[test]
fn the_runtime_config_keeps_only_an_enabled_table() {
    let mut enabled = KhiveConfig::default();
    enabled.channel.node = Some(valid());
    let mapped = crate::config::runtime_config_from_khive_config(
        &enabled,
        crate::config::RuntimeConfig::default(),
    );
    assert_eq!(mapped.node_channel.as_deref(), Some(&valid()));

    let mut disabled = KhiveConfig::default();
    disabled.channel.node = Some(NodeChannelConfig::default());
    let mapped = crate::config::runtime_config_from_khive_config(
        &disabled,
        crate::config::RuntimeConfig::default(),
    );
    assert!(mapped.node_channel.is_none());

    let absent = KhiveConfig::default();
    let mapped = crate::config::runtime_config_from_khive_config(
        &absent,
        crate::config::RuntimeConfig::default(),
    );
    assert!(mapped.node_channel.is_none());
}

#[test]
fn the_runtime_config_holds_the_node_table_behind_one_pointer() {
    // Runtime handles carry their config inline and the recall path holds
    // several of them across awaits. With the table inline, a debug daemon's
    // recall overflowed its 2 MiB worker stack.
    assert_eq!(
        std::mem::size_of_val(&crate::config::RuntimeConfig::default().node_channel),
        std::mem::size_of::<usize>()
    );
}

#[test]
fn the_runtime_config_default_has_no_node_channel() {
    assert!(crate::config::RuntimeConfig::default()
        .node_channel
        .is_none());
}

#[test]
fn the_engine_config_validation_refuses_an_invalid_enabled_table() {
    let mut config = KhiveConfig::default();
    let mut node = valid();
    node.batch_size = 0;
    config.channel.node = Some(node);
    let error = config
        .validate()
        .expect_err("invalid table must be refused");
    assert_eq!(
        error.to_string(),
        "channel.node: batch_size must be in 1..=256"
    );
}
