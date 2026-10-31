//! Tests for the service seam: production reaches the service over HTTPS only.

use std::sync::Arc;

use khive_channel_node::encoding::Realm;
use khive_channel_node::keychain::testing::TestKeychain;
use khive_channel_node::keys::KeyFacility;
use khive_runtime::node_config::NodeChannelConfig;

use super::test_support::LoopbackService;
use super::{HttpsNodeService, NodeService};

// Mutant: in mod.rs, replace the `HttpsNodeService` body line
//     `        commands::setup_client(config, facility)`
// (it occurs once) with a `NodeClient::test_http(...)` call. The http URL is
// then accepted, the refusal assertions below fail and this test goes red.
#[test]
fn node_setup_production_service_is_https_only() {
    // No connection is made: building a client only validates the origin.
    let url = "http://127.0.0.1:1";
    let keychain = TestKeychain::new();
    let realm = Realm::parse("relay.example").expect("realm parses");
    let bundle = keychain
        .create("node-a", realm)
        .expect("key item is created");
    let fingerprint = *bundle
        .verify()
        .expect("bundle verifies")
        .fingerprint()
        .as_bytes();
    let facility: Arc<dyn KeyFacility> = Arc::new(
        keychain
            .open("node-a", &fingerprint)
            .expect("key item opens"),
    );

    let config = NodeChannelConfig {
        enabled: true,
        slug: Some("node".to_owned()),
        actor: Some("laptop".to_owned()),
        realm: Some("relay.example".to_owned()),
        service_url: Some(url.to_owned()),
        agent_id: Some("0b0f4a6e-9d3c-4b8e-8a7d-5f1c2e3d4a5b".to_owned()),
        device_id: Some("6c1d7e2f-3a4b-4c5d-9e6f-7a8b9c0d1e2f".to_owned()),
        key_epoch: Some(1),
        key_reference: Some("node-a".to_owned()),
        key_fingerprint: Some(fingerprint.iter().map(|b| format!("{b:02x}")).collect()),
        ..NodeChannelConfig::default()
    };
    config.validate().expect("the config is otherwise valid");

    let refused = HttpsNodeService
        .client(&config, facility.clone())
        .err()
        .expect("production refuses a plain-HTTP origin");
    assert_eq!(refused.exit, 2);
    assert_eq!(
        refused.message,
        "service_url must be an HTTPS origin without credentials, query or fragment"
    );

    // Positive control: the same config is accepted by the loopback service.
    let loopback = LoopbackService::new(url);
    assert!(loopback.client(&config, facility).is_ok());
    assert_eq!(loopback.calls(), 1);
}
