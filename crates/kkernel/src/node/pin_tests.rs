//! Behaviour tests for `kkernel node pin`: consent, the re-check after consent,
//! and the key, epoch and history rules. Each test drives the real command with a
//! scripted contact service and a scripted confirmation, and judges only the
//! observable result: the exit, the prompts shown, the receipt and the bytes of
//! the configuration file.

use std::path::PathBuf;

use khive_channel_node::client_test_server::{Reply, ScriptedServer};
use khive_channel_node::encoding::{CanonicalUuid, Epoch, NodeAddress, Realm};
use khive_channel_node::keychain::testing::TestKeychain;
use khive_channel_node::keys::DevicePublicKeys;
use khive_channel_node::wire::ContactResponse;
use tempfile::TempDir;

use super::test_support::{run_node, LoopbackService, Run, ScriptedConfirmation, TestNodeKeys};
use super::NodeSetupError;

/// A scripted confirmation case: label, prompt answers, expected exit code.
type PromptCase<'a> = (&'a str, Vec<Result<bool, NodeSetupError>>, Option<u8>);

const REALM: &str = "relay.example";
const LOCAL_REF: &str = "node-a";
const LOCAL_AGENT: &str = "0b0f4a6e-9d3c-4b8e-8a7d-5f1c2e3d4a5b";
const LOCAL_DEVICE: &str = "6c1d7e2f-3a4b-4c5d-9e6f-7a8b9c0d1e2f";
const CONTACT_AGENT: &str = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
const DEVICE_ONE: &str = "7d2e8f30-4b5c-4d6e-8f70-8b9c0d1e2f3a";
const DEVICE_TWO: &str = "9e3f9041-5c6d-4e7f-9a81-9c0d1e2f3a4b";
const OTHER_AGENT: &str = "2b3c4d5e-6f7a-4b8c-9d0e-1f2a3b4c5d6e";

fn realm() -> Realm {
    Realm::parse(REALM).expect("realm parses")
}

fn uuid(text: &str) -> CanonicalUuid {
    CanonicalUuid::parse(text).expect("uuid parses")
}

fn epoch(value: u64) -> Epoch {
    Epoch::new(value).expect("epoch is in range")
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A fresh, valid set of contact public keys. The values differ on every call;
/// the behaviour under test does not depend on which values they are.
fn contact_keys() -> DevicePublicKeys {
    TestKeychain::new()
        .create("contact", realm())
        .expect("contact keys are created")
        .verify()
        .expect("contact bundle verifies")
}

/// The contact service answer for one contact device at one key epoch.
fn response(keys: &DevicePublicKeys, device: &str, key_epoch: u64) -> ContactResponse {
    let agent = uuid(CONTACT_AGENT);
    ContactResponse {
        agent_id: agent,
        address: NodeAddress::new(realm(), agent),
        device_id: uuid(device),
        key_epoch: epoch(key_epoch),
        kem_public_key: keys.kem.clone(),
        signing_public_key: keys.signing.clone(),
        fingerprint: keys.fingerprint(),
        contact_generation: epoch(3),
    }
}

fn reply(answer: &ContactResponse) -> Reply {
    Reply::json(
        200,
        &serde_json::to_string(answer).expect("contact answer serializes"),
    )
}

/// The share card a person would hand over for this contact answer.
fn card_json(answer: &ContactResponse) -> String {
    serde_json::json!({
        "protocol_version": 1,
        "address": answer.address.to_string(),
        "agent_id": answer.agent_id.to_string(),
        "realm": REALM,
        "label": "Contact",
        "key_epoch": answer.key_epoch.get(),
        "fingerprint": hex(answer.fingerprint.as_bytes()),
    })
    .to_string()
}

fn pin_toml(answer: &ContactResponse) -> String {
    format!(
        r#"
[[channel.node.pins]]
agent_id = "{}"
device_id = "{}"
key_epoch = {}
kem_public_key = "{}"
signing_public_key = "{}"
fingerprint = "{}"
"#,
        answer.agent_id,
        answer.device_id,
        answer.key_epoch.get(),
        hex(answer.kem_public_key.as_bytes()),
        hex(answer.signing_public_key.as_bytes()),
        hex(answer.fingerprint.as_bytes()),
    )
}

struct Fixture {
    _dir: TempDir,
    keys: TestNodeKeys,
    config: PathBuf,
    card: PathBuf,
}

/// An assigned node whose configuration already holds the `seeded` pins.
fn fixture(seeded: &[&ContactResponse]) -> Fixture {
    let dir = TempDir::new().expect("temp dir is created");
    let keys = TestNodeKeys::new();
    let bundle = keys
        .keychain()
        .create(LOCAL_REF, realm())
        .expect("local keys are created");
    let fingerprint = hex(bundle
        .verify()
        .expect("local bundle verifies")
        .fingerprint()
        .as_bytes());
    let mut text = format!(
        r#"[channel.node]
enabled = true
slug = "node"
actor = "laptop"
realm = "{REALM}"
service_url = "http://127.0.0.1:9"
agent_id = "{LOCAL_AGENT}"
device_id = "{LOCAL_DEVICE}"
key_epoch = 1
key_reference = "{LOCAL_REF}"
key_fingerprint = "{fingerprint}"
"#
    );
    for answer in seeded {
        text.push_str(&pin_toml(answer));
    }
    let config = dir.path().join("config.toml");
    std::fs::write(&config, text).expect("config is written");
    let card = dir.path().join("card.json");
    Fixture {
        _dir: dir,
        keys,
        config,
        card,
    }
}

fn config_bytes(fx: &Fixture) -> Vec<u8> {
    std::fs::read(&fx.config).expect("config is readable")
}

fn pins(bytes: &[u8]) -> Vec<toml::Value> {
    let text = std::str::from_utf8(bytes).expect("config is utf-8");
    let value: toml::Value = toml::from_str(text).expect("config is valid toml");
    value
        .get("channel")
        .and_then(|c| c.get("node"))
        .and_then(|n| n.get("pins"))
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default()
}

fn assert_pin_is(pin: &toml::Value, answer: &ContactResponse) {
    let table = pin.as_table().expect("a pin is a table");
    assert_eq!(table.len(), 6, "a pin holds exactly six fields");
    let text = |name: &str| table.get(name).and_then(|v| v.as_str()).map(str::to_owned);
    assert_eq!(text("agent_id"), Some(answer.agent_id.to_string()));
    assert_eq!(text("device_id"), Some(answer.device_id.to_string()));
    assert_eq!(
        table.get("key_epoch").and_then(|v| v.as_integer()),
        Some(i64::try_from(answer.key_epoch.get()).expect("epoch fits"))
    );
    assert_eq!(
        text("kem_public_key"),
        Some(hex(answer.kem_public_key.as_bytes()))
    );
    assert_eq!(
        text("signing_public_key"),
        Some(hex(answer.signing_public_key.as_bytes()))
    );
    assert_eq!(
        text("fingerprint"),
        Some(hex(answer.fingerprint.as_bytes()))
    );
}

struct Attempt {
    run: Run,
    prompts: Vec<String>,
    requests: usize,
}

impl Attempt {
    fn exit(&self) -> Option<u8> {
        self.run.result.as_ref().err().map(|e| e.exit)
    }

    fn message(&self) -> Option<&str> {
        self.run.result.as_ref().err().map(|e| e.message.as_str())
    }

    fn assert_ok(&self) {
        assert!(
            self.run.result.is_ok(),
            "pin should succeed, refused with {:?}",
            self.message()
        );
    }

    fn assert_refused(&self) {
        assert!(self.run.result.is_err(), "pin should have been refused");
    }
}

/// Runs `node pin` once for `card`, with the scripted `replies` from the service
/// and the scripted confirmation `answers`.
async fn attempt(
    fx: &Fixture,
    card: &ContactResponse,
    replies: Vec<Reply>,
    answers: Vec<Result<bool, NodeSetupError>>,
) -> Attempt {
    std::fs::write(&fx.card, card_json(card)).expect("card is written");
    let server = ScriptedServer::start(replies)
        .await
        .expect("scripted server starts");
    let service = LoopbackService::new(server.url());
    let mut confirmation = ScriptedConfirmation::new(answers);
    let config = fx.config.to_str().expect("config path is utf-8");
    let card_path = fx.card.to_str().expect("card path is utf-8");
    let run = run_node(
        &["--config", config, "pin", "--card", card_path],
        &fx.keys,
        &mut confirmation,
        &service,
        b"",
    )
    .await;
    let requests = server.requests().await.len();
    Attempt {
        run,
        prompts: confirmation.prompts().to_vec(),
        requests,
    }
}

fn receipt(attempt: &Attempt) -> serde_json::Value {
    serde_json::from_slice(&attempt.run.stdout).expect("stdout is one json receipt")
}

#[tokio::test]
async fn node_pin_requires_grant_and_human_confirmation() {
    let keys = contact_keys();
    let answer = response(&keys, DEVICE_ONE, 1);

    // The service refuses the lookup: no prompt, no write. A card alone gives no keys.
    let fx = fixture(&[]);
    let before = config_bytes(&fx);
    let refused = attempt(
        &fx,
        &answer,
        vec![Reply::json(403, r#"{"error":"forbidden"}"#)],
        vec![Ok(true)],
    )
    .await;
    refused.assert_refused();
    assert_eq!(refused.exit(), Some(5), "a refused lookup exits 5");
    assert!(refused.prompts.is_empty(), "no prompt without a lookup");
    assert_eq!(config_bytes(&fx), before, "a refused lookup writes nothing");
    assert!(pins(&config_bytes(&fx)).is_empty());

    // A valid lookup asks once and a yes appends the pin with its six fields.
    let fx = fixture(&[]);
    let before = config_bytes(&fx);
    let pinned = attempt(
        &fx,
        &answer,
        vec![reply(&answer), reply(&answer)],
        vec![Ok(true)],
    )
    .await;
    pinned.assert_ok();
    assert_eq!(pinned.prompts.len(), 1, "exactly one confirmation is asked");
    assert!(
        pinned.prompts[0].chars().count() > 0,
        "the prompt asks for explicit confirmation"
    );
    let after = config_bytes(&fx);
    assert_ne!(after, before, "a confirmed pin changes the config");
    let stored = pins(&after);
    assert_eq!(stored.len(), 1);
    assert_pin_is(&stored[0], &answer);
    let printed = receipt(&pinned);
    assert_eq!(printed["state"], "pinned");
    assert_eq!(printed["changed"], true);
    assert_eq!(printed["restart_required"], true);

    // Declined, end of input, and a prompt failure each leave the bytes alone.
    let cases: Vec<PromptCase<'_>> = vec![
        ("declined", vec![Ok(false)], Some(3)),
        ("empty script", vec![], Some(3)),
        (
            "prompt failure",
            vec![Err(NodeSetupError {
                exit: 7,
                message: "terminal unavailable".to_owned(),
            })],
            Some(7),
        ),
    ];
    for (name, answers, exit) in cases {
        let fx = fixture(&[]);
        let before = config_bytes(&fx);
        let run = attempt(&fx, &answer, vec![reply(&answer), reply(&answer)], answers).await;
        run.assert_refused();
        assert_eq!(run.exit(), exit, "{name}: exit code");
        assert_eq!(run.prompts.len(), 1, "{name}: the prompt was shown once");
        assert_eq!(config_bytes(&fx), before, "{name}: config bytes changed");
    }
}

#[tokio::test]
async fn node_pin_rechecks_after_confirmation() {
    let keys = contact_keys();
    let other = contact_keys();
    let base = response(&keys, DEVICE_ONE, 1);

    // Control: two identical replies succeed after a yes.
    let fx = fixture(&[]);
    let control = attempt(&fx, &base, vec![reply(&base), reply(&base)], vec![Ok(true)]).await;
    control.assert_ok();
    assert!(
        control.requests >= 2,
        "the contact is looked up again after the confirmation"
    );
    assert_eq!(pins(&config_bytes(&fx)).len(), 1);

    // Each field changed between the two replies refuses and changes nothing.
    let fields = [
        "agent_id",
        "address",
        "device_id",
        "key_epoch",
        "kem_public_key",
        "signing_public_key",
        "fingerprint",
        "contact_generation",
    ];
    for field in fields {
        let mut second = base.clone();
        match field {
            "agent_id" => second.agent_id = uuid(OTHER_AGENT),
            "address" => second.address = NodeAddress::new(realm(), uuid(OTHER_AGENT)),
            "device_id" => second.device_id = uuid(DEVICE_TWO),
            "key_epoch" => second.key_epoch = epoch(2),
            "kem_public_key" => second.kem_public_key = other.kem.clone(),
            "signing_public_key" => second.signing_public_key = other.signing.clone(),
            "fingerprint" => second.fingerprint = other.fingerprint(),
            "contact_generation" => second.contact_generation = epoch(4),
            _ => unreachable!("every listed field has a change"),
        }
        assert_ne!(second, base, "{field}: the second reply must differ");
        let fx = fixture(&[]);
        let before = config_bytes(&fx);
        let run = attempt(
            &fx,
            &base,
            vec![reply(&base), reply(&second)],
            vec![Ok(true)],
        )
        .await;
        run.assert_refused();
        assert_eq!(run.prompts.len(), 1, "{field}: confirmation was asked once");
        assert!(
            run.requests >= 2,
            "{field}: the contact was looked up twice"
        );
        assert_eq!(config_bytes(&fx), before, "{field}: config bytes changed");
        if field == "device_id" || field == "contact_generation" {
            // These pass the card check, so the comparison of the two replies refuses.
            assert_eq!(run.exit(), Some(2), "{field}: exit code");
            assert_eq!(
                run.message(),
                Some("contact changed during confirmation; run node pin again"),
                "{field}: message"
            );
        }
    }
}

#[tokio::test]
async fn node_pin_crypto_epochs_and_history() {
    let keys_a = contact_keys();
    let keys_b = contact_keys();
    let keys_c = contact_keys();
    let device_one_epoch_one = response(&keys_a, DEVICE_ONE, 1);

    // Public keys that do not match their fingerprint are refused before any prompt.
    let mut inconsistent = device_one_epoch_one.clone();
    inconsistent.fingerprint = keys_b.fingerprint();
    let fx = fixture(&[]);
    let before = config_bytes(&fx);
    let run = attempt(
        &fx,
        &inconsistent,
        vec![reply(&inconsistent)],
        vec![Ok(true)],
    )
    .await;
    assert_eq!(run.exit(), Some(5));
    assert_eq!(run.message(), Some("service returned an invalid response"));
    assert!(run.prompts.is_empty());
    assert_eq!(config_bytes(&fx), before);

    // A key that is not a valid point is refused before any prompt.
    let mut body = serde_json::to_value(&device_one_epoch_one).expect("answer serializes");
    body["kem_public_key"] = serde_json::Value::String("00".repeat(32));
    let run = attempt(
        &fx,
        &device_one_epoch_one,
        vec![Reply::json(200, &body.to_string())],
        vec![Ok(true)],
    )
    .await;
    run.assert_refused();
    assert!(run.prompts.is_empty());
    assert_eq!(config_bytes(&fx), before);

    // First pin: device one at epoch one.
    let run = attempt(
        &fx,
        &device_one_epoch_one,
        vec![reply(&device_one_epoch_one), reply(&device_one_epoch_one)],
        vec![Ok(true)],
    )
    .await;
    run.assert_ok();
    let first_state = config_bytes(&fx);
    assert_eq!(pins(&first_state).len(), 1);

    // The exact same pin again is a no-op: no prompt, no change, an unchanged receipt.
    let run = attempt(
        &fx,
        &device_one_epoch_one,
        vec![reply(&device_one_epoch_one)],
        vec![],
    )
    .await;
    run.assert_ok();
    assert!(run.prompts.is_empty(), "an exact repeat asks nothing");
    assert_eq!(
        config_bytes(&fx),
        first_state,
        "an exact repeat changes nothing"
    );
    assert_eq!(receipt(&run)["changed"], false);

    // The same identity and epoch with different keys is refused.
    let conflicting = response(&keys_b, DEVICE_ONE, 1);
    let run = attempt(&fx, &conflicting, vec![reply(&conflicting)], vec![Ok(true)]).await;
    assert_eq!(run.exit(), Some(2));
    assert_eq!(
        run.message(),
        Some("node pin identity already has different confirmed keys")
    );
    assert!(run.prompts.is_empty());
    assert_eq!(config_bytes(&fx), first_state);

    // A newer epoch for the same device is appended; the old epoch stays.
    let device_one_epoch_two = response(&keys_b, DEVICE_ONE, 2);
    let run = attempt(
        &fx,
        &device_one_epoch_two,
        vec![reply(&device_one_epoch_two), reply(&device_one_epoch_two)],
        vec![Ok(true)],
    )
    .await;
    run.assert_ok();
    let stored = pins(&config_bytes(&fx));
    assert_eq!(stored.len(), 2, "both epochs persist");
    assert_pin_is(&stored[0], &device_one_epoch_one);
    assert_pin_is(&stored[1], &device_one_epoch_two);

    // A different device starts again at epoch one and is allowed.
    let device_two_epoch_one = response(&keys_c, DEVICE_TWO, 1);
    let run = attempt(
        &fx,
        &device_two_epoch_one,
        vec![reply(&device_two_epoch_one), reply(&device_two_epoch_one)],
        vec![Ok(true)],
    )
    .await;
    run.assert_ok();
    let stored = pins(&config_bytes(&fx));
    assert_eq!(stored.len(), 3, "a new device adds its own pin");
    assert_pin_is(&stored[2], &device_two_epoch_one);

    // A lower epoch than a confirmed one for the same device is refused.
    let confirmed_epoch_two = response(&keys_b, DEVICE_ONE, 2);
    let fx = fixture(&[&confirmed_epoch_two]);
    let before = config_bytes(&fx);
    let older = response(&keys_a, DEVICE_ONE, 1);
    let run = attempt(&fx, &older, vec![reply(&older)], vec![Ok(true)]).await;
    assert_eq!(run.exit(), Some(2));
    assert_eq!(
        run.message(),
        Some("contact key epoch is older than a confirmed epoch for this device")
    );
    assert!(run.prompts.is_empty());
    assert_eq!(config_bytes(&fx), before);
}
