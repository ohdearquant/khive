//! Tests for `kkernel node assign`: strict enrolment input, validation of the
//! enabled candidate, and replay after an interrupted save.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::test_support::{run_node, NoService, Run, ScriptedConfirmation, TestNodeKeys};

const AGENT: &str = "a1111111-1111-4111-8111-11111111111b";
const DEVICE: &str = "c2222222-2222-4222-8222-22222222222d";

const SHAPE: &str = "input must be one JSON object with exactly the required fields";
const INVALID: &str = "input contains an invalid identifier, key, fingerprint, label or epoch";
const VERSION: &str = "only protocol version 1 is supported";
const ADDRESS: &str = "address does not match the supplied realm and agent";
const REALM_MISMATCH: &str = "enrolment realm does not match this node";
const EPOCH_ONE: &str = "initial enrolment must use key epoch 1";
const FINGERPRINT_MISMATCH: &str = "enrolment fingerprint does not match this Mac's keys";
const METADATA_MISMATCH: &str = "saved enrolment metadata does not match this node";
const SAVED_NOTE: &str = "Configuration saved. Restart the daemon before using this node.\n";

/// One initialised node in a temporary directory.
struct Fixture {
    _dir: tempfile::TempDir,
    config: PathBuf,
    input: PathBuf,
    metadata: PathBuf,
    keys: TestNodeKeys,
    init_bytes: Vec<u8>,
    fingerprint: String,
}

impl Fixture {
    /// Runs a real `init` so the preparation and the key item exist.
    async fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("khive.toml");
        let input = dir.path().join("enrolment.json");
        let metadata = dir.path().join("khive.toml.node-assignment.json");
        let keys = TestNodeKeys::new();
        let path = config.to_str().expect("utf8 path");
        let mut confirmation = ScriptedConfirmation::new(vec![]);
        let run = run_node(
            &[
                "init",
                "--config",
                path,
                "--service-url",
                "https://service.example",
                "--realm",
                "acme",
                "--actor",
                "laptop",
            ],
            &keys,
            &mut confirmation,
            &NoService,
            b"",
        )
        .await;
        run.result.expect("init succeeds");
        let init_bytes = std::fs::read(&config).expect("config after init");
        let fingerprint = node_table(&init_bytes)
            .get("key_fingerprint")
            .and_then(|value| value.as_str())
            .expect("init records the key fingerprint")
            .to_owned();
        Self {
            _dir: dir,
            config,
            input,
            metadata,
            keys,
            init_bytes,
            fingerprint,
        }
    }

    /// The raw flattened enrolment the service would return for this node.
    fn enrolment(&self) -> Value {
        enrolment_value(&self.fingerprint)
    }

    fn write_input(&self, bytes: &[u8]) {
        std::fs::write(&self.input, bytes).expect("write input");
    }

    fn config_bytes(&self) -> Vec<u8> {
        std::fs::read(&self.config).expect("read config")
    }

    fn metadata_absent(&self) -> bool {
        std::fs::symlink_metadata(&self.metadata).is_err()
    }

    /// Runs `assign` on the written input; any service call would panic.
    async fn assign(&self) -> Run {
        let config = self.config.to_str().expect("utf8 path");
        let input = self.input.to_str().expect("utf8 path");
        let mut confirmation = ScriptedConfirmation::new(vec![]);
        run_node(
            &["assign", "--config", config, "--input", input],
            &self.keys,
            &mut confirmation,
            &NoService,
            b"",
        )
        .await
    }
}

fn enrolment_value(fingerprint: &str) -> Value {
    json!({
        "protocol_version": 1,
        "address": format!("khive1:acme/{AGENT}"),
        "agent_id": AGENT,
        "realm": "acme",
        "label": "Work laptop",
        "key_epoch": 1,
        "fingerprint": fingerprint,
        "device_id": DEVICE,
    })
}

fn with(mut value: Value, key: &str, replacement: Value) -> Value {
    value[key] = replacement;
    value
}

fn without(mut value: Value, key: &str) -> Value {
    value.as_object_mut().expect("object").remove(key);
    value
}

fn to_bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("serialize")
}

/// The `[channel.node]` table of a configuration file.
fn node_table(bytes: &[u8]) -> toml::value::Table {
    let text = std::str::from_utf8(bytes).expect("utf8 config");
    let doc: toml::Value = toml::from_str(text).expect("valid toml");
    doc.get("channel")
        .and_then(|channel| channel.get("node"))
        .and_then(|node| node.as_table())
        .expect("node table")
        .clone()
}

/// Sets one integer member of `[channel.node]`, keeping every other byte of the file.
fn with_node_field(bytes: &[u8], field: &str, value: i64) -> Vec<u8> {
    let text = std::str::from_utf8(bytes).expect("utf8 config");
    let line = format!("{field} = {value}");
    let spaced = format!("{field} ");
    let tight = format!("{field}=");
    let lines: Vec<&str> = text.lines().collect();
    let header = lines
        .iter()
        .position(|l| l.trim() == "[channel.node]")
        .expect("node table header");
    let end = lines
        .iter()
        .enumerate()
        .skip(header + 1)
        .find(|(_, l)| l.trim().starts_with('['))
        .map(|(index, _)| index)
        .unwrap_or(lines.len());
    let existing = (header + 1..end).find(|&index| {
        let trimmed = lines[index].trim_start();
        trimmed.starts_with(&spaced) || trimmed.starts_with(&tight)
    });
    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
    match existing {
        Some(index) => out[index] = line,
        None => out.insert(header + 1, line),
    }
    let mut joined = out.join("\n");
    joined.push('\n');
    joined.into_bytes()
}

fn assert_refused(run: &Run, message: &str, case: &str) {
    let error = run.result.as_ref().expect_err(case);
    assert_eq!(error.exit, 2, "exit code for {case}");
    assert_eq!(error.message, message, "message for {case}");
    assert!(run.stdout.is_empty(), "stdout for {case}");
}

fn receipt(run: &Run) -> Value {
    run.result.as_ref().expect("assign succeeds");
    assert_eq!(
        String::from_utf8(run.stderr.clone()).expect("utf8 diagnostics"),
        SAVED_NOTE
    );
    serde_json::from_slice(&run.stdout).expect("receipt is JSON")
}

fn assert_receipt(receipt: &Value, changed: bool, config: &Path) {
    assert_eq!(receipt["state"], "assigned");
    assert_eq!(receipt["changed"], changed);
    assert_eq!(receipt["enabled"], true);
    assert_eq!(receipt["address"], format!("khive1:acme/{AGENT}"));
    assert_eq!(receipt["restart_required"], true);
    let name = config.file_name().and_then(|n| n.to_str()).expect("name");
    assert!(receipt["config"]
        .as_str()
        .expect("config text")
        .ends_with(name));
}

// Kills the mutant that turns the fingerprint comparison in `assign` around
// (`enrolment.fingerprint.as_bytes() != &local_fp` to `==`): the matching control
// would be refused and the mismatched fingerprint case would be accepted.
#[tokio::test]
async fn node_assign_strict_shape_and_binding() {
    let fx = Fixture::new().await;
    let fp = fx.fingerprint.clone();
    let good = fx.enrolment();
    let card = without(good.clone(), "device_id");
    assert_ne!(fp.to_uppercase(), fp);

    let base = serde_json::to_string(&good).expect("serialize");
    let duplicate = base.replacen(
        "\"realm\":\"acme\"",
        "\"realm\":\"acme\",\"realm\":\"acme\"",
        1,
    );
    assert_ne!(duplicate, base);
    let other_address = json!(format!("khive1:other/{AGENT}"));

    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        (
            "nested card",
            to_bytes(&json!({ "card": card.clone(), "device_id": DEVICE })),
            SHAPE,
        ),
        (
            "mcp envelope",
            to_bytes(&json!({
                "content": [{ "type": "text", "text": base.clone() }],
                "isError": false,
            })),
            SHAPE,
        ),
        ("share card only", to_bytes(&card), SHAPE),
        (
            "unknown field",
            to_bytes(&with(good.clone(), "extra", json!("x"))),
            SHAPE,
        ),
        ("duplicate field", duplicate.into_bytes(), SHAPE),
        (
            "bad version",
            to_bytes(&with(good.clone(), "protocol_version", json!(2))),
            VERSION,
        ),
        (
            "bad address",
            to_bytes(&with(good.clone(), "address", other_address.clone())),
            ADDRESS,
        ),
        (
            "bad realm",
            to_bytes(&with(
                with(good.clone(), "realm", json!("other")),
                "address",
                other_address,
            )),
            REALM_MISMATCH,
        ),
        (
            "epoch zero",
            to_bytes(&with(good.clone(), "key_epoch", json!(0))),
            INVALID,
        ),
        (
            "epoch two",
            to_bytes(&with(good.clone(), "key_epoch", json!(2))),
            EPOCH_ONE,
        ),
        (
            "uppercase fingerprint",
            to_bytes(&with(good.clone(), "fingerprint", json!(fp.to_uppercase()))),
            INVALID,
        ),
        (
            "other fingerprint",
            to_bytes(&with(good.clone(), "fingerprint", json!("cd".repeat(32)))),
            FINGERPRINT_MISMATCH,
        ),
        (
            "bad agent uuid",
            to_bytes(&with(good.clone(), "agent_id", json!("not-a-uuid"))),
            INVALID,
        ),
        (
            "bad device uuid",
            to_bytes(&with(
                good.clone(),
                "device_id",
                json!(DEVICE.to_uppercase()),
            )),
            INVALID,
        ),
    ];

    for (case, bytes, message) in cases {
        fx.write_input(&bytes);
        let run = fx.assign().await;
        assert_refused(&run, message, case);
        assert_eq!(
            fx.config_bytes(),
            fx.init_bytes,
            "config bytes changed for {case}"
        );
        assert!(fx.metadata_absent(), "metadata written for {case}");
    }

    // Matching control: the raw flattened enrolment is accepted and saved.
    fx.write_input(&to_bytes(&good));
    let run = fx.assign().await;
    assert_receipt(&receipt(&run), true, &fx.config);
    assert_ne!(fx.config_bytes(), fx.init_bytes);
    let saved = std::fs::read(&fx.metadata).expect("metadata saved");
    assert_eq!(serde_json::from_slice::<Value>(&saved).expect("json"), good);
}

// Kills the mutant that saves the candidate disabled
// (`candidate.enabled = true;` to `candidate.enabled = false;`).
#[tokio::test]
async fn node_assign_validates_enabled_candidate() {
    let fx = Fixture::new().await;
    let good = fx.enrolment();
    fx.write_input(&to_bytes(&good));

    // An initialised, disabled config with invalid timing is refused by validation.
    let timing = [
        (
            "poll_wait_seconds",
            26,
            "poll_wait_seconds must be in 0..=25",
        ),
        (
            "outbox_interval_seconds",
            0,
            "outbox_interval_seconds must be in 1..=60",
        ),
        (
            "outbox_interval_seconds",
            61,
            "outbox_interval_seconds must be in 1..=60",
        ),
        ("batch_size", 0, "batch_size must be in 1..=256"),
        ("batch_size", 257, "batch_size must be in 1..=256"),
    ];
    for (field, value, reason) in timing {
        let broken = with_node_field(&fx.init_bytes, field, value);
        assert_ne!(broken, fx.init_bytes, "{field} edit changed nothing");
        std::fs::write(&fx.config, &broken).expect("write broken config");
        let run = fx.assign().await;
        assert_refused(&run, &format!("channel.node: {reason}"), field);
        assert_eq!(
            fx.config_bytes(),
            broken,
            "config changed for {field}={value}"
        );
        assert!(fx.metadata_absent(), "metadata written for {field}={value}");
    }

    // Valid control: the initialised config assigns, enabling every field.
    std::fs::write(&fx.config, &fx.init_bytes).expect("restore config");
    let before = node_table(&fx.init_bytes);
    assert_eq!(before.get("enabled"), Some(&toml::Value::Boolean(false)));
    let run = fx.assign().await;
    assert_receipt(&receipt(&run), true, &fx.config);
    let assigned = fx.config_bytes();
    let after = node_table(&assigned);
    assert_eq!(after.get("enabled"), Some(&toml::Value::Boolean(true)));
    assert_eq!(
        after.get("agent_id"),
        Some(&toml::Value::String(AGENT.into()))
    );
    assert_eq!(
        after.get("device_id"),
        Some(&toml::Value::String(DEVICE.into()))
    );
    assert_eq!(after.get("key_epoch"), Some(&toml::Value::Integer(1)));
    assert_eq!(
        after.get("key_fingerprint"),
        Some(&toml::Value::String(fx.fingerprint.clone()))
    );
    // The actor keeps the value chosen at init; the label is not used as the actor.
    assert_eq!(
        after.get("actor"),
        Some(&toml::Value::String("laptop".into()))
    );
    for key in [
        "slug",
        "namespace",
        "actor",
        "realm",
        "service_url",
        "key_reference",
        "poll_wait_seconds",
        "outbox_interval_seconds",
        "batch_size",
    ] {
        assert_eq!(after.get(key), before.get(key), "{key} changed");
    }
    assert!(after.get("actor") != Some(&toml::Value::String("Work laptop".into())));
    let metadata = std::fs::read(&fx.metadata).expect("metadata saved");

    // An exact replay is a no-op.
    let run = fx.assign().await;
    assert_receipt(&receipt(&run), false, &fx.config);
    assert_eq!(fx.config_bytes(), assigned, "replay changed the config");
    assert_eq!(
        std::fs::read(&fx.metadata).expect("metadata"),
        metadata,
        "replay changed the metadata"
    );
}

// Kills the mutant that stops comparing saved metadata with the enrolment
// (`existing != payload` to `existing == payload`): the exact replay would be
// refused and changed metadata would be accepted.
#[tokio::test]
async fn node_assign_metadata_crash_replay() {
    let fx = Fixture::new().await;
    let good = fx.enrolment();
    fx.write_input(&to_bytes(&good));

    // Reach the crash state: metadata saved, configuration still as initialised.
    let run = fx.assign().await;
    assert_receipt(&receipt(&run), true, &fx.config);
    let assigned = fx.config_bytes();
    let metadata = std::fs::read(&fx.metadata).expect("metadata saved");
    std::fs::write(&fx.config, &fx.init_bytes).expect("write the kept bytes back");

    // The metadata holds public fields only.
    let saved: Value = serde_json::from_slice(&metadata).expect("metadata is JSON");
    let mut names: Vec<&str> = saved
        .as_object()
        .expect("metadata object")
        .keys()
        .map(String::as_str)
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "address",
            "agent_id",
            "device_id",
            "fingerprint",
            "key_epoch",
            "label",
            "protocol_version",
            "realm",
        ]
    );
    assert_eq!(saved, good);

    // The exact replay succeeds and completes the configuration.
    let run = fx.assign().await;
    assert_receipt(&receipt(&run), true, &fx.config);
    assert_eq!(
        fx.config_bytes(),
        assigned,
        "replay did not finish the save"
    );
    assert_eq!(std::fs::read(&fx.metadata).expect("metadata"), metadata);

    // Changed metadata refuses with the exact message and writes nothing.
    std::fs::write(&fx.config, &fx.init_bytes).expect("write the kept bytes back");
    let text = String::from_utf8(metadata).expect("utf8 metadata");
    let edited = text.replacen("Work laptop", "Other laptop", 1);
    assert_ne!(edited, text);
    std::fs::remove_file(&fx.metadata).expect("remove metadata");
    std::fs::write(&fx.metadata, edited.as_bytes()).expect("write edited metadata");
    let run = fx.assign().await;
    assert_refused(&run, METADATA_MISMATCH, "changed metadata");
    assert_eq!(fx.config_bytes(), fx.init_bytes);
    assert_eq!(
        std::fs::read(&fx.metadata).expect("metadata"),
        edited.as_bytes()
    );

    // No key was created after init, and the service was never reached.
    let (create, _open, recover) = fx.keys.calls();
    assert_eq!((create, recover), (1, 0));
}
