//! Tests for the node send route: sender binding, pin resolution and sealing.
//!
//! Row 625 (`node_route_binds_sender_and_persists_sealed_bytes`), route half.
//!
//! Kills: in `node/route.rs` the first bind check
//!   old: `request.from_actor != actor || request.namespace != config.namespace`
//!   new: `request.from_actor != actor && request.namespace != config.namespace`
//! A wrong-actor-only case and a wrong-namespace-only case each get past the mutated check and
//! seal successfully, so each of those tests fails. A both-wrong case would not.

use super::pins::RuntimePinSource;
use super::route::NodeContactLookup;
use super::source::RuntimeOutboundSource;
use super::{NodeHostState, RuntimeNodeSendRoute};
use crate::node_route::{NodeSealRequest, NodeSendRefusal, NodeSendRoute};
use async_trait::async_trait;
use khive_channel_node::adapter::NodeChannel;
use khive_channel_node::client::NodeClient;
use khive_channel_node::encoding::{
    Base64Bytes, CanonicalUuid, Epoch, NodeAddress, PollWait, ProtocolVersion, Realm,
};
use khive_channel_node::envelope::{Ciphertext, EnvelopeHeader, SealedEnvelope};
use khive_channel_node::keys::KemPublicKey;
use khive_channel_node::keys::{DevicePublicKeys, InMemoryKeyFacility, KeyFacility};
use khive_channel_node::pins::PinSource;
use khive_channel_node::pins::PinSourceError;
use khive_channel_node::response::{NodeContactResult, NodeError};
use khive_channel_node::source::OutboundSource;
use khive_channel_node::wire::ContactResponse;
use khive_channel_node::ProtocolError;
use khive_runtime::comm_transport::{PolicyMode, SenderEnvelope};
use khive_runtime::node_config::{NodeChannelConfig, NodePinConfig};
use khive_runtime::{AllowAllGate, BackendId, KhiveRuntime, RuntimeConfig};
use khive_storage::{SqlStatement, SqlValue};
use std::sync::Arc;
use uuid::Uuid;

const NS: &str = "local";
const OTHER_NS: &str = "other";
const ACTOR: &str = "actor:node-route-test";
const OTHER_ACTOR: &str = "actor:someone-else";
const REALM: &str = "realm.test";
const OTHER_REALM: &str = "realm.other";
const SENDER_AGENT: u128 = 1;
const SENDER_DEVICE: u128 = 5;
const RECIPIENT_AGENT: u128 = 2;
const RECIPIENT_DEVICE: u128 = 7;
const LOGICAL: u128 = 0xabc;
const THREAD: u128 = 0x77;
const SENT_AT: &str = "2026-01-01T00:00:00Z";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn cu(n: u128) -> CanonicalUuid {
    CanonicalUuid::from_uuid(Uuid::from_u128(n))
}

fn epoch(n: u64) -> Epoch {
    Epoch::new(n).expect("epoch")
}

fn realm(name: &str) -> Realm {
    Realm::parse(name).expect("realm")
}

fn address_in(name: &str) -> String {
    NodeAddress::new(realm(name), cu(RECIPIENT_AGENT)).to_string()
}

fn contact_for(keys: &DevicePublicKeys) -> ContactResponse {
    ContactResponse {
        agent_id: cu(RECIPIENT_AGENT),
        address: NodeAddress::new(realm(REALM), cu(RECIPIENT_AGENT)),
        device_id: cu(RECIPIENT_DEVICE),
        key_epoch: epoch(1),
        kem_public_key: keys.kem.clone(),
        signing_public_key: keys.signing.clone(),
        fingerprint: keys.fingerprint(),
        contact_generation: epoch(3),
    }
}

fn pin_config_for(keys: &DevicePublicKeys) -> NodePinConfig {
    NodePinConfig {
        agent_id: Uuid::from_u128(RECIPIENT_AGENT).to_string(),
        device_id: Uuid::from_u128(RECIPIENT_DEVICE).to_string(),
        key_epoch: 1,
        kem_public_key: hex(keys.kem.as_bytes()),
        signing_public_key: hex(keys.signing.as_bytes()),
        fingerprint: hex(keys.fingerprint().as_bytes()),
    }
}

fn node_config(pins: Vec<NodePinConfig>) -> NodeChannelConfig {
    NodeChannelConfig {
        enabled: true,
        slug: Some("test".to_string()),
        namespace: NS.to_string(),
        actor: Some(ACTOR.to_string()),
        realm: Some(REALM.to_string()),
        service_url: Some("https://node.example.test".to_string()),
        agent_id: Some(Uuid::from_u128(SENDER_AGENT).to_string()),
        device_id: Some(Uuid::from_u128(SENDER_DEVICE).to_string()),
        key_epoch: Some(1),
        key_reference: Some("test-credential".to_string()),
        key_fingerprint: Some("ab".repeat(32)),
        pair_policy_mode: PolicyMode::Off,
        pins,
        ..Default::default()
    }
}

fn runtime_with(config: NodeChannelConfig) -> KhiveRuntime {
    let backend = khive_db::StorageBackend::memory().expect("in-memory backend");
    backend.pool().run_migrations().expect("migrations");
    KhiveRuntime::from_backend(
        Arc::new(backend),
        RuntimeConfig {
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            web: Default::default(),
            telemetry: Default::default(),
            mounts: Vec::new(),
            brain: Default::default(),
            git_write: Default::default(),
            exec: Default::default(),
            display_timezone: khive_runtime::config::resolve_default_display_timezone(),
            events_split: None,
            db_path: None,
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: khive_runtime::Namespace::parse(NS).unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "comm".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: Some(ACTOR.to_string()),
            node_channel: Some(Arc::new(config)),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        },
    )
}

/// What the fake contact lookup answers.
enum Script {
    Found {
        observed: Box<ContactResponse>,
        owner_pin: bool,
    },
    PinsError,
}

struct FakeContact(Script);

#[async_trait]
impl NodeContactLookup for FakeContact {
    async fn contact(&self, _agent: Uuid) -> Result<NodeContactResult, NodeError> {
        match &self.0 {
            Script::Found {
                observed,
                owner_pin,
            } => Ok(NodeContactResult {
                observed: (**observed).clone(),
                matches_owner_pin: *owner_pin,
            }),
            Script::PinsError => Err(NodeError::Pins(PinSourceError(
                "pin source unavailable".to_string(),
            ))),
        }
    }
}

/// A facility whose seal always fails.
struct FailingSeal(InMemoryKeyFacility);

impl KeyFacility for FailingSeal {
    fn public_keys(&self) -> DevicePublicKeys {
        self.0.public_keys()
    }
    fn sign(&self, input: &[u8]) -> [u8; 64] {
        self.0.sign(input)
    }
    fn seal(
        &self,
        _header: &EnvelopeHeader,
        _id: CanonicalUuid,
        _recipient: &KemPublicKey,
        _plaintext: &[u8],
    ) -> Result<SealedEnvelope, ProtocolError> {
        Err(ProtocolError::Encryption)
    }
    fn open(
        &self,
        header: &EnvelopeHeader,
        id: CanonicalUuid,
        pinned_sender: &KemPublicKey,
        envelope: &SealedEnvelope,
    ) -> Result<Vec<u8>, ProtocolError> {
        self.0.open(header, id, pinned_sender, envelope)
    }
}

struct Fixture {
    runtime: KhiveRuntime,
    route: RuntimeNodeSendRoute,
    sender: Arc<InMemoryKeyFacility>,
    recipient: Arc<InMemoryKeyFacility>,
}

enum Facility {
    Working,
    SealFails,
}

/// Build the route over a real `NodeHostState` with fake services installed.
fn fixture(
    pins: Vec<NodePinConfig>,
    script: Script,
    facility: Facility,
    recipient: Arc<InMemoryKeyFacility>,
) -> Fixture {
    let sender = Arc::new(InMemoryKeyFacility::generate().expect("sender keys"));
    let runtime = runtime_with(node_config(pins));
    let state = NodeHostState::from_runtime(runtime.clone())
        .expect("the fixture node config is valid")
        .expect("the fixture node config is enabled");
    let binding = state.binding().clone();
    let token = runtime
        .authorize(khive_types::Namespace::parse(NS).unwrap())
        .expect("authorize the fixture namespace");
    let source: Arc<dyn OutboundSource> = Arc::new(RuntimeOutboundSource::new(
        runtime.clone(),
        token,
        binding.clone(),
    ));
    let client_pins: Arc<dyn PinSource> = Arc::new(
        RuntimePinSource::new(runtime.clone(), state.config(), &binding).expect("pin source"),
    );
    let client_facility: Arc<dyn KeyFacility> = sender.clone();
    let client = NodeClient::new(
        binding,
        "https://node.example.test",
        source,
        client_pins,
        client_facility,
    )
    .expect("node client");
    let channel = Arc::new(NodeChannel::new(
        client,
        PollWait::new(0).expect("poll wait"),
    ));
    let installed: Arc<dyn KeyFacility> = match facility {
        Facility::Working => sender.clone(),
        Facility::SealFails => Arc::new(FailingSeal(
            InMemoryKeyFacility::generate().expect("failing keys"),
        )),
    };
    let lookup: Arc<dyn NodeContactLookup> = Arc::new(FakeContact(script));
    assert!(
        state.install_test_services(channel, lookup, installed),
        "services were not yet initialized"
    );
    Fixture {
        runtime,
        route: RuntimeNodeSendRoute::new(state),
        sender,
        recipient,
    }
}

/// A fixture where everything is valid: the recipient is pinned and the contact agrees.
fn valid_fixture() -> Fixture {
    let recipient = Arc::new(InMemoryKeyFacility::generate().expect("recipient keys"));
    let keys = recipient.public_keys();
    fixture(
        vec![pin_config_for(&keys)],
        Script::Found {
            observed: Box::new(contact_for(&keys)),
            owner_pin: true,
        },
        Facility::Working,
        recipient,
    )
}

fn request<'a>(address: &'a str, from_actor: &'a str, namespace: &'a str) -> NodeSealRequest<'a> {
    NodeSealRequest {
        address,
        logical_message_id: Uuid::from_u128(LOGICAL),
        from_actor,
        content: "hello node",
        subject: Some("the subject"),
        thread_id: Some("00000000-0000-0000-0000-000000000077"),
        namespace,
        sent_at: SENT_AT,
    }
}

async fn count(runtime: &KhiveRuntime, sql: &str, label: &str) -> i64 {
    let result = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_scalar(SqlStatement {
            sql: sql.into(),
            params: vec![],
            label: Some(label.into()),
        })
        .await
        .unwrap();
    match result {
        Some(SqlValue::Integer(n)) => n,
        other => panic!("unexpected count: {other:?}"),
    }
}

async fn rows(runtime: &KhiveRuntime) -> (i64, i64) {
    (
        count(runtime, "SELECT count(*) FROM notes", "route-note-count").await,
        count(
            runtime,
            "SELECT count(*) FROM comm_sender_transport",
            "route-transport-count",
        )
        .await,
    )
}

async fn seal_ok(fx: &Fixture) -> SenderEnvelope {
    let address = address_in(REALM);
    match fx.route.seal(request(&address, ACTOR, NS)).await {
        Ok(envelope) => envelope,
        Err(refusal) => panic!("expected a sealed envelope, got {refusal:?}"),
    }
}

async fn refusal_for(
    fx: &Fixture,
    address: &str,
    actor: &str,
    namespace: &str,
) -> Option<NodeSendRefusal> {
    fx.route
        .seal(request(address, actor, namespace))
        .await
        .err()
}

// Kills the `||` -> `&&` mutant: only the actor is wrong.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_route_binds_sender_and_persists_sealed_bytes_refuses_a_wrong_actor_when_namespace_matches(
) {
    let fx = valid_fixture();
    let before = rows(&fx.runtime).await;
    let address = address_in(REALM);
    assert_eq!(
        refusal_for(&fx, &address, OTHER_ACTOR, NS).await,
        Some(NodeSendRefusal::SenderBindingMismatch)
    );
    assert_eq!(rows(&fx.runtime).await, before, "a refusal writes nothing");
}

// Kills the `||` -> `&&` mutant: only the namespace is wrong.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_route_binds_sender_and_persists_sealed_bytes_refuses_a_wrong_namespace_when_actor_matches(
) {
    let fx = valid_fixture();
    let before = rows(&fx.runtime).await;
    let address = address_in(REALM);
    assert_eq!(
        refusal_for(&fx, &address, ACTOR, OTHER_NS).await,
        Some(NodeSendRefusal::SenderBindingMismatch)
    );
    assert_eq!(rows(&fx.runtime).await, before, "a refusal writes nothing");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_route_binds_sender_and_persists_sealed_bytes_refuses_a_recipient_in_another_realm() {
    let fx = valid_fixture();
    let before = rows(&fx.runtime).await;
    let address = address_in(OTHER_REALM);
    assert_eq!(
        refusal_for(&fx, &address, ACTOR, NS).await,
        Some(NodeSendRefusal::SenderBindingMismatch)
    );
    assert_eq!(rows(&fx.runtime).await, before, "a refusal writes nothing");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_route_binds_sender_and_persists_sealed_bytes_refuses_a_recipient_with_no_pin() {
    let recipient = Arc::new(InMemoryKeyFacility::generate().expect("recipient keys"));
    let keys = recipient.public_keys();
    let fx = fixture(
        Vec::new(),
        Script::Found {
            observed: Box::new(contact_for(&keys)),
            owner_pin: true,
        },
        Facility::Working,
        recipient,
    );
    let before = rows(&fx.runtime).await;
    let address = address_in(REALM);
    assert_eq!(
        refusal_for(&fx, &address, ACTOR, NS).await,
        Some(NodeSendRefusal::UnknownRecipientPin(address.clone()))
    );
    assert_eq!(rows(&fx.runtime).await, before, "a refusal writes nothing");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_route_binds_sender_and_persists_sealed_bytes_refuses_a_pin_that_does_not_match_the_contact_keys(
) {
    // The pin sits at the contact's exact identity but holds a different key pair.
    let recipient = Arc::new(InMemoryKeyFacility::generate().expect("recipient keys"));
    let other = InMemoryKeyFacility::generate().expect("other keys");
    let contact_keys = recipient.public_keys();
    let pinned_keys = other.public_keys();
    assert_ne!(contact_keys.fingerprint(), pinned_keys.fingerprint());
    let fx = fixture(
        vec![pin_config_for(&pinned_keys)],
        Script::Found {
            observed: Box::new(contact_for(&contact_keys)),
            owner_pin: true,
        },
        Facility::Working,
        recipient,
    );
    let before = rows(&fx.runtime).await;
    let address = address_in(REALM);
    assert_eq!(
        refusal_for(&fx, &address, ACTOR, NS).await,
        Some(NodeSendRefusal::UnknownRecipientPin(address.clone()))
    );
    assert_eq!(rows(&fx.runtime).await, before, "a refusal writes nothing");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_route_binds_sender_and_persists_sealed_bytes_refuses_when_the_contact_does_not_match_the_owner_pin(
) {
    let recipient = Arc::new(InMemoryKeyFacility::generate().expect("recipient keys"));
    let keys = recipient.public_keys();
    let fx = fixture(
        vec![pin_config_for(&keys)],
        Script::Found {
            observed: Box::new(contact_for(&keys)),
            owner_pin: false,
        },
        Facility::Working,
        recipient,
    );
    let address = address_in(REALM);
    assert_eq!(
        refusal_for(&fx, &address, ACTOR, NS).await,
        Some(NodeSendRefusal::UnknownRecipientPin(address.clone()))
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_route_binds_sender_and_persists_sealed_bytes_valid_seal_decrypts_to_the_request_content(
) {
    let fx = valid_fixture();
    let before = rows(&fx.runtime).await;
    let envelope = seal_ok(&fx).await;

    assert_eq!(envelope.namespace, NS);
    assert_eq!(envelope.logical_message_id, Uuid::from_u128(LOGICAL));
    assert_eq!(
        envelope.outbound_note_id,
        Uuid::nil(),
        "the handler assigns the note id"
    );
    assert_eq!(envelope.kind, "khive");
    assert_eq!(envelope.slug, "test");
    assert_eq!(envelope.credential_ref, "test-credential");
    assert_eq!(envelope.recipient_address, address_in(REALM));
    assert_eq!(
        envelope.recipient_device_id,
        Uuid::from_u128(RECIPIENT_DEVICE)
    );
    assert_eq!(envelope.recipient_key_epoch, 1);
    assert_eq!(envelope.contact_generation, 3);
    assert_eq!(envelope.sender_key_epoch, 1);
    assert_eq!(
        envelope.recipient_key_fingerprint,
        hex(fx.recipient.public_keys().fingerprint().as_bytes())
    );

    let header = EnvelopeHeader {
        protocol_version: ProtocolVersion::new(1).expect("version"),
        realm: realm(REALM),
        sender_agent_id: cu(SENDER_AGENT),
        sender_device_id: cu(SENDER_DEVICE),
        sender_key_epoch: epoch(1),
        recipient_agent_id: cu(RECIPIENT_AGENT),
        recipient_device_id: cu(RECIPIENT_DEVICE),
        recipient_key_epoch: epoch(1),
    };
    let enc: [u8; 32] = envelope.enc.clone().try_into().expect("32-byte enc");
    let sealed = SealedEnvelope {
        enc: Base64Bytes::new(enc),
        ciphertext: Ciphertext::new(envelope.ciphertext.clone()).expect("ciphertext"),
    };
    let sender_kem = fx.sender.public_keys().kem;
    let opened = fx
        .recipient
        .open(&header, cu(LOGICAL), &sender_kem, &sealed)
        .expect("the pinned recipient opens the envelope");
    let plaintext: serde_json::Value = serde_json::from_slice(&opened).expect("plaintext json");
    assert_eq!(plaintext["subject"], "the subject");
    assert_eq!(plaintext["body"], "hello node");
    assert_eq!(
        plaintext["thread_id"],
        Uuid::from_u128(THREAD).to_string().as_str()
    );
    let sent_at = plaintext["sent_at"].as_str().expect("sent_at string");
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(sent_at).expect("rfc3339"),
        chrono::DateTime::parse_from_rfc3339(SENT_AT).expect("rfc3339")
    );

    // The logical id is bound into the ciphertext: another id does not open it.
    assert!(fx
        .recipient
        .open(&header, cu(LOGICAL + 1), &sender_kem, &sealed)
        .is_err());

    assert_eq!(
        rows(&fx.runtime).await,
        before,
        "the route itself writes nothing"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_route_binds_sender_and_persists_sealed_bytes_contact_source_failure_is_not_reported_as_an_unknown_pin(
) {
    let recipient = Arc::new(InMemoryKeyFacility::generate().expect("recipient keys"));
    let keys = recipient.public_keys();
    let fx = fixture(
        vec![pin_config_for(&keys)],
        Script::PinsError,
        Facility::Working,
        recipient,
    );
    let address = address_in(REALM);
    let refusal = refusal_for(&fx, &address, ACTOR, NS).await;
    assert_eq!(refusal, Some(NodeSendRefusal::SourceUnavailable));
    assert!(!matches!(
        refusal,
        Some(NodeSendRefusal::UnknownRecipientPin(_))
    ));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_route_binds_sender_and_persists_sealed_bytes_key_facility_failure_is_not_reported_as_an_unknown_pin(
) {
    let recipient = Arc::new(InMemoryKeyFacility::generate().expect("recipient keys"));
    let keys = recipient.public_keys();
    let fx = fixture(
        vec![pin_config_for(&keys)],
        Script::Found {
            observed: Box::new(contact_for(&keys)),
            owner_pin: true,
        },
        Facility::SealFails,
        recipient,
    );
    let address = address_in(REALM);
    let refusal = refusal_for(&fx, &address, ACTOR, NS).await;
    assert_eq!(refusal, Some(NodeSendRefusal::SealFailed));
    assert!(!matches!(
        refusal,
        Some(NodeSendRefusal::UnknownRecipientPin(_))
    ));
}
