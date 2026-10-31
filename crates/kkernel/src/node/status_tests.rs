//! Tests for `node status`: the five local setup states, the report rules, the
//! local key and metadata checks, and the public exports.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use khive_channel_node::encoding::HexBytes;
use khive_channel_node::keys::EnrolmentBundle;
use serde_json::{json, Value};

use super::formats::{fingerprint_lines, parse_card, parse_enrolment, CardInput};
use super::test_support::{run_node, NoService, Run, ScriptedConfirmation, TestNodeKeys};

const AGENT: &str = "a1111111-1111-4111-8111-11111111111b";
const DEVICE: &str = "c2222222-2222-4222-8222-22222222222d";
const OTHER_DEVICE: &str = "33333333-3333-4333-8333-333333333333";
const SERVICE_URL: &str = "https://service.example";
const REALM: &str = "acme";
const ACTOR: &str = "laptop";

const REPORT_MEMBERS: [&str; 17] = [
    "state",
    "config",
    "enabled",
    "service_url",
    "realm",
    "actor",
    "namespace",
    "slug",
    "address",
    "device_id",
    "key_epoch",
    "fingerprint",
    "pin_count",
    "config_digest",
    "keychain",
    "daemon",
    "delivery",
];
const BUNDLE_MEMBERS: [&str; 4] = [
    "realm",
    "kem_public_key",
    "signing_public_key",
    "enrol_proof",
];
const CARD_MEMBERS: [&str; 7] = [
    "protocol_version",
    "address",
    "agent_id",
    "realm",
    "label",
    "key_epoch",
    "fingerprint",
];
const ENROLMENT_MEMBERS: [&str; 8] = [
    "protocol_version",
    "address",
    "agent_id",
    "realm",
    "label",
    "key_epoch",
    "fingerprint",
    "device_id",
];
const ASSIGNED_RECEIPT_MEMBERS: [&str; 6] = [
    "state",
    "changed",
    "enabled",
    "config",
    "address",
    "restart_required",
];
const EXPORT_RECEIPT_MEMBERS: [&str; 3] = ["state", "kind", "file"];

const NULL_WHEN_UNASSIGNED: [&str; 3] = ["address", "device_id", "config_digest"];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct Fixture {
    dir: tempfile::TempDir,
    keys: TestNodeKeys,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
            keys: TestNodeKeys::new(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn config(&self) -> PathBuf {
        self.path("khive.toml")
    }

    fn metadata(&self) -> PathBuf {
        self.path("khive.toml.node-assignment.json")
    }

    async fn run(&self, argv: &[&str]) -> Run {
        let mut confirmation = ScriptedConfirmation::new(Vec::new());
        run_node(argv, &self.keys, &mut confirmation, &NoService, b"").await
    }

    async fn init(&self) -> Run {
        let config = path_str(&self.config());
        self.run(&[
            "init",
            "--config",
            config.as_str(),
            "--service-url",
            SERVICE_URL,
            "--realm",
            REALM,
            "--actor",
            ACTOR,
        ])
        .await
    }

    async fn assign(&self, fingerprint: &str) -> Run {
        let input = self.path("enrolment.json");
        std::fs::write(
            &input,
            serde_json::to_vec(&enrolment_value(fingerprint)).expect("serialize"),
        )
        .expect("write enrolment");
        let config = path_str(&self.config());
        let input_text = path_str(&input);
        self.run(&[
            "assign",
            "--config",
            config.as_str(),
            "--input",
            input_text.as_str(),
        ])
        .await
    }

    async fn status(&self, extra: &[&str]) -> Run {
        let config = path_str(&self.config());
        let mut argv = vec!["status", "--config", config.as_str()];
        argv.extend_from_slice(extra);
        self.run(&argv).await
    }
}

fn path_str(path: &Path) -> String {
    path.to_str().expect("utf-8 path").to_owned()
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn is_hex(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn enrolment_value(fingerprint: &str) -> Value {
    json!({
        "protocol_version": 1,
        "address": format!("khive1:{REALM}/{AGENT}"),
        "agent_id": AGENT,
        "realm": REALM,
        "label": "Work laptop",
        "key_epoch": 1,
        "fingerprint": fingerprint,
        "device_id": DEVICE,
    })
}

fn fingerprint_of_bundle(stdout: &[u8]) -> String {
    let bundle: EnrolmentBundle = serde_json::from_slice(stdout).expect("bundle json");
    let keys = bundle.verify().expect("bundle verifies");
    hex(keys.fingerprint().as_bytes())
}

fn parsed(fingerprint: &str) -> HexBytes<32> {
    HexBytes::<32>::parse(fingerprint).expect("64 hex digits")
}

fn ok(run: &Run) {
    assert!(run.result.is_ok(), "command failed: {:?}", run.result);
}

/// A refusal: exact exit code and message, and nothing on stdout.
fn refused(run: &Run, exit: u8, message: &str) {
    let error = run.result.as_ref().expect_err("command must be refused");
    assert_eq!(error.exit, exit, "exit code for `{message}`");
    assert_eq!(error.message, message);
    assert!(run.stdout.is_empty(), "a refusal writes no output");
}

fn json(bytes: &[u8]) -> Value {
    assert_eq!(bytes.last(), Some(&b'\n'), "one trailing line feed");
    serde_json::from_slice(bytes).expect("one JSON document")
}

fn members(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .expect("a JSON object")
        .keys()
        .cloned()
        .collect()
}

fn names(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

/// The report has exactly the 17 members and the two fixed `not_checked` ones.
fn assert_report(report: &Value, state: &str) {
    assert_eq!(members(report), names(&REPORT_MEMBERS));
    assert_eq!(report["state"], json!(state));
    assert_eq!(report["daemon"], json!("not_checked"));
    assert_eq!(report["delivery"], json!("not_checked"));
}

fn replace_once(path: &Path, old: &str, new: &str) {
    let text = std::fs::read_to_string(path).expect("read file");
    assert_eq!(
        text.matches(old).count(),
        1,
        "`{old}` must occur once in the file"
    );
    std::fs::write(path, text.replace(old, new)).expect("write file");
}

fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .expect("list dir")
        .map(|entry| {
            let entry = entry.expect("entry");
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).expect("read file"),
            )
        })
        .collect()
}

async fn initialized() -> (Fixture, Run, String) {
    let fx = Fixture::new();
    let init = fx.init().await;
    ok(&init);
    let fingerprint = fingerprint_of_bundle(&init.stdout);
    (fx, init, fingerprint)
}

async fn assigned() -> (Fixture, Run, Run, String) {
    let (fx, init, fingerprint) = initialized().await;
    let assign = fx.assign(&fingerprint).await;
    ok(&assign);
    (fx, init, assign, fingerprint)
}

fn grouped_digits(stderr: &[u8]) -> String {
    let text = String::from_utf8(stderr.to_vec()).expect("utf-8 diagnostics");
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("Fingerprint:"));
    let mut digits = String::new();
    for _ in 0..4 {
        let line = lines.next().expect("a digit line");
        assert!(
            line.starts_with("  ") && !line.starts_with("   "),
            "two-space indent: {line:?}"
        );
        let groups: Vec<&str> = line.trim_start().split(' ').collect();
        assert_eq!(groups.len(), 4, "four groups: {line:?}");
        for group in groups {
            assert_eq!(group.len(), 4, "groups of four digits: {line:?}");
            assert!(is_hex(group), "hex digits: {line:?}");
            digits.push_str(group);
        }
    }
    digits
}

fn string_leaves(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => items.iter().for_each(|item| string_leaves(item, out)),
        Value::Object(map) => map.values().for_each(|item| string_leaves(item, out)),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// The five states
// ---------------------------------------------------------------------------

// Kills: formats.rs `let delivery = "not_checked";` -> `"verified"` (assert_report
// checks both fixed members in every state test).
#[tokio::test]
async fn node_status_absent_reports_no_setup() {
    let fx = Fixture::new();
    let config = path_str(&fx.config());

    // No configuration file at all.
    let missing = fx.status(&[]).await;
    ok(&missing);
    let report = json(&missing.stdout);
    assert_report(&report, "absent");
    assert_eq!(report["config"], json!(config));
    assert_eq!(report["enabled"], json!(false));
    assert_eq!(report["namespace"], json!("local"));
    assert_eq!(report["pin_count"], json!(0));
    assert_eq!(report["keychain"], json!("not_checked"));
    for member in [
        "service_url",
        "realm",
        "actor",
        "slug",
        "address",
        "device_id",
        "key_epoch",
        "fingerprint",
        "config_digest",
    ] {
        assert!(report[member].is_null(), "{member} is null when absent");
    }
    assert!(missing.stderr.is_empty(), "no fingerprint, no diagnostics");
    assert!(!fx.config().exists(), "status creates no file");
    assert_eq!(fx.keys.calls(), (0, 0, 0), "the keychain is not touched");

    // An empty configuration file reads the same as none.
    std::fs::write(fx.config(), b"").expect("write empty config");
    let empty = fx.status(&[]).await;
    ok(&empty);
    assert_eq!(json(&empty.stdout), report);
    assert_eq!(fx.keys.calls(), (0, 0, 0));

    // Control: the report is not always absent.
    let (_fx, _init, _fp) = initialized().await;
    let live = _fx.status(&[]).await;
    assert_ne!(json(&live.stdout)["state"], json!("absent"));
}

// Kills: formats.rs delivery mutant; commands.rs `config_digest: n.filter(|n| n.enabled)
// .map(|n| n.config_digest()),` -> `config_digest: n.map(|n| n.config_digest()),`
// (a prepared table must report a null digest).
#[tokio::test]
async fn node_status_prepared_reports_without_opening_keychain() {
    let fx = Fixture::new();
    fx.keys.keychain().fail_adds(true);
    let init = fx.init().await;
    assert!(init.result.is_err(), "key creation was made to fail");
    assert!(init.stdout.is_empty(), "a failed init writes no bundle");
    let before = tree(fx.dir.path());
    let calls = fx.keys.calls();

    let status = fx.status(&[]).await;
    ok(&status);
    let report = json(&status.stdout);
    assert_report(&report, "prepared");
    assert_eq!(report["enabled"], json!(false));
    assert_eq!(report["service_url"], json!(SERVICE_URL));
    assert_eq!(report["realm"], json!(REALM));
    assert_eq!(report["actor"], json!(ACTOR));
    assert_eq!(report["slug"], json!("node"));
    assert_eq!(report["namespace"], json!("local"));
    assert_eq!(report["key_epoch"], json!(1));
    assert_eq!(report["pin_count"], json!(0));
    assert!(report["fingerprint"].is_null());
    for member in NULL_WHEN_UNASSIGNED {
        assert!(report[member].is_null(), "{member} is null when prepared");
    }
    assert_eq!(report["keychain"], json!("not_checked"));
    assert!(status.stderr.is_empty(), "no fingerprint to show");
    assert_eq!(
        fx.keys.calls(),
        calls,
        "no keychain call without a fingerprint"
    );
    assert_eq!(tree(fx.dir.path()), before, "status writes nothing");

    // A prepared node has no keys to export and nothing to share.
    let bundle = fx.status(&["--bundle"]).await;
    refused(
        &bundle,
        2,
        "initialization is incomplete; use node init --recover",
    );
    let card = fx.status(&["--card"]).await;
    refused(&card, 2, "assign this node before using this command");
}

// Kills: formats.rs delivery mutant; commands.rs state guard `if !matches!(state,
// SetupState::Assigned | SetupState::Disabled) {` (card on an initialized node).
#[tokio::test]
async fn node_status_initialized_reports_key_with_null_digest() {
    let (fx, init, fingerprint) = initialized().await;
    let calls = fx.keys.calls();

    let status = fx.status(&[]).await;
    ok(&status);
    let report = json(&status.stdout);
    assert_report(&report, "initialized");
    assert_eq!(report["enabled"], json!(false));
    assert_eq!(report["service_url"], json!(SERVICE_URL));
    assert_eq!(report["realm"], json!(REALM));
    assert_eq!(report["actor"], json!(ACTOR));
    assert_eq!(report["slug"], json!("node"));
    assert_eq!(report["key_epoch"], json!(1));
    assert_eq!(report["fingerprint"], json!(fingerprint));
    assert_eq!(report["pin_count"], json!(0));
    for member in NULL_WHEN_UNASSIGNED {
        assert!(
            report[member].is_null(),
            "{member} is null when initialized"
        );
    }
    assert_eq!(report["keychain"], json!("matches"));
    let after = fx.keys.calls();
    assert_eq!(after.0, calls.0, "status never creates keys");
    assert_eq!(after.1, calls.1 + 1, "status opens the stored item once");
    assert_eq!(after.2, calls.2, "status never recovers keys");

    // The bundle is exported again, identical to the one init wrote.
    let bundle = fx.status(&["--bundle"]).await;
    ok(&bundle);
    assert_eq!(bundle.stdout, init.stdout);

    // No contact card exists before assignment.
    let card = fx.status(&["--card"]).await;
    refused(&card, 2, "assign this node before using this command");
}

// Kills: formats.rs delivery mutant; commands.rs `config_digest: n.filter(|n| n.enabled)
// .map(|n| n.config_digest()),` -> `config_digest: n.map(|n| n.config_digest()),`
// is caught by the other state tests; here the enabled digest must be present.
#[tokio::test]
async fn node_status_assigned_reports_address_and_digest() {
    let (fx, _init, assign, fingerprint) = assigned().await;
    let receipt = json(&assign.stdout);
    assert_eq!(members(&receipt), names(&ASSIGNED_RECEIPT_MEMBERS));
    assert_eq!(receipt["state"], json!("assigned"));

    let status = fx.status(&[]).await;
    ok(&status);
    let report = json(&status.stdout);
    assert_report(&report, "assigned");
    assert_eq!(report["enabled"], json!(true));
    assert_eq!(report["service_url"], json!(SERVICE_URL));
    assert_eq!(report["realm"], json!(REALM));
    assert_eq!(report["actor"], json!(ACTOR));
    assert_eq!(report["slug"], json!("node"));
    assert_eq!(report["namespace"], json!("local"));
    assert_eq!(report["address"], json!(format!("khive1:{REALM}/{AGENT}")));
    assert_eq!(report["device_id"], json!(DEVICE));
    assert_eq!(report["key_epoch"], json!(1));
    assert_eq!(report["fingerprint"], json!(fingerprint));
    assert_eq!(report["pin_count"], json!(0));
    assert_eq!(report["keychain"], json!("matches"));
    let digest = report["config_digest"]
        .as_str()
        .expect("an enabled node reports a digest");
    assert!(!digest.is_empty());

    // The digest is a function of the configuration: a second run agrees.
    let again = fx.status(&[]).await;
    ok(&again);
    assert_eq!(again.stdout, status.stdout);
}

// Kills: formats.rs delivery mutant; commands.rs digest rule (a disabled node reports a
// null digest even though it keeps its identity).
#[tokio::test]
async fn node_status_disabled_keeps_identity_with_null_digest() {
    let (fx, _init, _assign, fingerprint) = assigned().await;
    let enabled = json(&fx.status(&[]).await.stdout);
    assert!(enabled["config_digest"].is_string());

    replace_once(&fx.config(), "enabled = true", "enabled = false");
    let status = fx.status(&[]).await;
    ok(&status);
    let report = json(&status.stdout);
    assert_report(&report, "disabled");
    assert_eq!(report["enabled"], json!(false));
    assert_eq!(report["address"], enabled["address"]);
    assert_eq!(report["device_id"], json!(DEVICE));
    assert_eq!(report["key_epoch"], json!(1));
    assert_eq!(report["fingerprint"], json!(fingerprint));
    assert!(
        report["config_digest"].is_null(),
        "digest only when enabled"
    );
    assert_eq!(report["keychain"], json!("matches"));

    // A disabled node still shares its card.
    let card = fx.status(&["--card"]).await;
    ok(&card);
    assert_eq!(members(&json(&card.stdout)), names(&CARD_MEMBERS));
}

// Kills: commands.rs `classify_state` fall-through (`} else {\n        None\n    };`
// replaced by a default state) via the refused combination below.
#[tokio::test]
async fn node_status_refuses_inconsistent_state() {
    let (fx, _init, _fingerprint) = initialized().await;
    // Control: the untouched initialized table reports normally.
    let control = fx.status(&[]).await;
    ok(&control);
    assert_eq!(json(&control.stdout)["state"], json!("initialized"));

    // An agent without a device is neither initialized nor assigned.
    let mut text = std::fs::read_to_string(fx.config()).expect("read config");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("agent_id = \"{AGENT}\"\n"));
    std::fs::write(fx.config(), &text).expect("write config");
    let before = tree(fx.dir.path());

    let status = fx.status(&[]).await;
    let error = status
        .result
        .as_ref()
        .expect_err("inconsistent table refused");
    assert_eq!(error.exit, 2);
    assert!(status.stdout.is_empty());
    assert!(status.stderr.is_empty());
    assert_eq!(tree(fx.dir.path()), before, "a refusal writes nothing");
}

// ---------------------------------------------------------------------------
// Key and metadata checks
// ---------------------------------------------------------------------------

// Kills: commands.rs `let opened = env.keys.open(reference, &fp)?;` replaced by a
// skipped check (a status with an unreadable or mismatched item must fail).
#[tokio::test]
async fn node_status_local_key_must_match() {
    let (fx, _init, fingerprint) = initialized().await;
    let control = fx.status(&[]).await;
    ok(&control);
    assert_eq!(json(&control.stdout)["keychain"], json!("matches"));

    // The stored item cannot be read.
    fx.keys.keychain().fail_reads(true);
    let unreadable = fx.status(&[]).await;
    assert!(unreadable.result.is_err(), "an unreadable item is refused");
    assert!(unreadable.stdout.is_empty());
    fx.keys.keychain().fail_reads(false);
    ok(&fx.status(&[]).await);

    // The saved fingerprint belongs to other keys.
    let other = "cd".repeat(32);
    assert_ne!(other, fingerprint);
    replace_once(&fx.config(), &fingerprint, &other);
    let before = tree(fx.dir.path());
    let mismatch = fx.status(&[]).await;
    assert!(
        mismatch.result.is_err(),
        "a mismatched fingerprint is refused"
    );
    assert!(mismatch.stdout.is_empty());
    assert_eq!(tree(fx.dir.path()), before, "a refusal writes nothing");
}

// Kills: commands.rs `if !agrees {` (status accepting saved metadata that disagrees with
// the binding).
#[tokio::test]
async fn node_status_metadata_must_match() {
    let (fx, _init, _assign, _fingerprint) = assigned().await;
    let original = std::fs::read(fx.metadata()).expect("metadata exists");
    let message = "saved enrolment metadata does not match this node";

    // Control: matching metadata is accepted by every mode.
    ok(&fx.status(&[]).await);
    ok(&fx.status(&["--card"]).await);

    // The metadata names another device.
    let text = String::from_utf8(original.clone()).expect("utf-8 metadata");
    assert_eq!(text.matches(DEVICE).count(), 1);
    std::fs::write(fx.metadata(), text.replace(DEVICE, OTHER_DEVICE)).expect("write");
    refused(&fx.status(&[]).await, 2, message);
    refused(&fx.status(&["--card"]).await, 2, message);
    refused(&fx.status(&["--bundle"]).await, 2, message);

    // The metadata is not a valid enrolment.
    std::fs::write(fx.metadata(), b"{}\n").expect("write");
    let invalid = fx.status(&[]).await;
    assert_eq!(invalid.result.as_ref().expect_err("refused").exit, 2);
    assert!(invalid.stdout.is_empty());

    // Missing metadata: the report still works, the card does not.
    std::fs::remove_file(fx.metadata()).expect("remove metadata");
    let report = fx.status(&[]).await;
    ok(&report);
    assert_eq!(json(&report.stdout)["state"], json!("assigned"));
    refused(
        &fx.status(&["--card"]).await,
        2,
        "share-card metadata is missing; repeat the original assignment",
    );

    // Restored metadata is accepted again.
    std::fs::write(fx.metadata(), &original).expect("restore");
    ok(&fx.status(&["--card"]).await);
}

// ---------------------------------------------------------------------------
// Exports
// ---------------------------------------------------------------------------

// Kills: formats.rs / commands.rs card construction that drops or adds a member (the
// four, seven and eight member sets are asserted exactly).
#[tokio::test]
async fn node_status_exports_have_exact_field_sets() {
    let (fx, init, _assign, fingerprint) = assigned().await;

    let bundle = fx.status(&["--bundle"]).await;
    ok(&bundle);
    assert!(bundle.stderr.is_empty());
    assert_eq!(members(&json(&bundle.stdout)), names(&BUNDLE_MEMBERS));
    assert_eq!(bundle.stdout, init.stdout, "the bundle is deterministic");

    let card = fx.status(&["--card"]).await;
    ok(&card);
    assert!(card.stderr.is_empty());
    let card_value = json(&card.stdout);
    assert_eq!(members(&card_value), names(&CARD_MEMBERS));
    assert!(matches!(parse_card(&card.stdout), Ok(CardInput::Card(_))));
    assert!(!String::from_utf8_lossy(&card.stdout).contains(DEVICE));

    let metadata_bytes = std::fs::read(fx.metadata()).expect("metadata exists");
    let metadata_value = json(&metadata_bytes);
    assert_eq!(members(&metadata_value), names(&ENROLMENT_MEMBERS));
    assert_eq!(metadata_value, enrolment_value(&fingerprint));
    assert!(parse_enrolment(&metadata_bytes).is_ok());
    let mut expected = metadata_value.clone();
    expected
        .as_object_mut()
        .expect("object")
        .remove("device_id");
    assert_eq!(
        card_value, expected,
        "the card is the enrolment minus the device"
    );

    // Both exports go to a create-only file with a receipt.
    for (flag, kind, expected_bytes) in [
        ("--bundle", "bundle", &bundle.stdout),
        ("--card", "card", &card.stdout),
    ] {
        let out = fx.path(&format!("{kind}-export.json"));
        let out_text = path_str(&out);
        let export = fx.status(&[flag, "--out", out_text.as_str()]).await;
        ok(&export);
        assert!(export.stderr.is_empty());
        let receipt = json(&export.stdout);
        assert_eq!(members(&receipt), names(&EXPORT_RECEIPT_MEMBERS));
        assert_eq!(receipt["state"], json!("exported"));
        assert_eq!(receipt["kind"], json!(kind));
        assert_eq!(receipt["file"], json!(out_text));
        assert_eq!(&std::fs::read(&out).expect("exported file"), expected_bytes);

        // An existing file is never replaced.
        let again = fx.status(&[flag, "--out", out_text.as_str()]).await;
        refused(&again, 2, "the output file already exists");
        assert_eq!(&std::fs::read(&out).expect("exported file"), expected_bytes);
    }
}

// Kills: formats.rs `fingerprint_lines` grouping used by status (digits dropped or
// regrouped so the block no longer joins back to the 64-hex value).
#[tokio::test]
async fn node_status_grouped_fingerprint_joins_back() {
    let (fx, init, assign, fingerprint) = assigned().await;
    assert_eq!(fingerprint.len(), 64);
    assert!(is_hex(&fingerprint));

    // The grouped block is the whole of status stderr and joins back to the value.
    let status = fx.status(&[]).await;
    ok(&status);
    assert_eq!(grouped_digits(&status.stderr), fingerprint);
    assert_eq!(status.stderr.iter().filter(|b| **b == b'\n').count(), 5);
    assert_eq!(
        String::from_utf8(status.stderr.clone()).expect("utf-8"),
        format!("{}\n", fingerprint_lines(&parsed(&fingerprint)))
    );
    assert_eq!(json(&status.stdout)["fingerprint"], json!(fingerprint));

    // init showed the same block first; assign shows no fingerprint.
    assert_eq!(grouped_digits(&init.stderr), fingerprint);
    assert!(String::from_utf8_lossy(&init.stderr)
        .starts_with(&fingerprint_lines(&parsed(&fingerprint))));
    assert!(!String::from_utf8_lossy(&assign.stderr).contains("Fingerprint:"));
}

// Kills: commands.rs status output that adds any member or value beyond the public ones,
// and `let delivery = "not_checked";` in formats.rs -> `"verified"` (the default report
// is one of the documents checked). Secrecy is checked on the public surface: the
// keychain's stored 65-byte item is not reachable from this crate's tests (its store
// is private to the keychain test double), so a raw-byte comparison against it is a
// recorded gap, not a claim.
#[tokio::test]
async fn node_status_exports_public_material_only() {
    let (fx, init, assign, fingerprint) = assigned().await;
    let status = fx.status(&[]).await;
    let bundle = fx.status(&["--bundle"]).await;
    let card = fx.status(&["--card"]).await;
    let bundle_file = fx.path("bundle-out.json");
    let card_file = fx.path("card-out.json");
    let bundle_out = fx
        .status(&["--bundle", "--out", path_str(&bundle_file).as_str()])
        .await;
    let card_out = fx
        .status(&["--card", "--out", path_str(&card_file).as_str()])
        .await;
    for run in [&status, &bundle, &card, &bundle_out, &card_out] {
        ok(run);
    }
    let metadata = std::fs::read(fx.metadata()).expect("metadata exists");
    let bundle_file_bytes = std::fs::read(&bundle_file).expect("bundle file");
    let card_file_bytes = std::fs::read(&card_file).expect("card file");

    // Every output document, with the member set it may carry.
    let documents: Vec<(&[u8], BTreeSet<String>)> = vec![
        (init.stdout.as_slice(), names(&BUNDLE_MEMBERS)),
        (assign.stdout.as_slice(), names(&ASSIGNED_RECEIPT_MEMBERS)),
        (status.stdout.as_slice(), names(&REPORT_MEMBERS)),
        (bundle.stdout.as_slice(), names(&BUNDLE_MEMBERS)),
        (card.stdout.as_slice(), names(&CARD_MEMBERS)),
        (bundle_out.stdout.as_slice(), names(&EXPORT_RECEIPT_MEMBERS)),
        (card_out.stdout.as_slice(), names(&EXPORT_RECEIPT_MEMBERS)),
        (bundle_file_bytes.as_slice(), names(&BUNDLE_MEMBERS)),
        (card_file_bytes.as_slice(), names(&CARD_MEMBERS)),
        (metadata.as_slice(), names(&ENROLMENT_MEMBERS)),
    ];

    // The only 64-digit hex values anywhere are the fingerprint, the two public keys and
    // the config digest; a 32-byte seed or a 65-byte item written in hex would show up as
    // another value.
    let bundle_value = json(&init.stdout);
    let mut allowed = BTreeSet::from([fingerprint.clone()]);
    for member in ["kem_public_key", "signing_public_key"] {
        let key = bundle_value[member].as_str().expect("public key text");
        allowed.insert(key.to_owned());
    }
    // The assigned report also carries the SHA-256 digest of the public node table.
    let digest = json(&status.stdout)["config_digest"]
        .as_str()
        .expect("an assigned node reports its config digest")
        .to_owned();
    allowed.insert(digest);
    for (bytes, expected) in &documents {
        let value = json(bytes);
        assert_eq!(&members(&value), expected, "exact member set");
        let mut leaves = Vec::new();
        string_leaves(&value, &mut leaves);
        for leaf in leaves {
            if is_hex(&leaf) && leaf.len() >= 64 {
                assert!(allowed.contains(&leaf), "unexpected key-like value {leaf}");
            }
        }
    }

    // No output names the keychain item or carries a secret-looking word.
    let config_text = std::fs::read_to_string(fx.config()).expect("read config");
    let reference = config_text
        .lines()
        .find(|line| line.trim_start().starts_with("key_reference"))
        .and_then(|line| line.split('"').nth(1))
        .expect("the table records a key reference")
        .to_owned();
    assert!(reference.starts_with("node-"));
    let dir_text = path_str(fx.dir.path());
    // Bundle documents carry random key text, so the word scan skips them; every
    // other stream is fixed text or hex and is scanned.
    let bundle_set = names(&BUNDLE_MEMBERS);
    let mut streams: Vec<&[u8]> = documents
        .iter()
        .filter(|(_, expected)| *expected != bundle_set)
        .map(|(bytes, _)| *bytes)
        .collect();
    streams.extend([
        init.stderr.as_slice(),
        assign.stderr.as_slice(),
        status.stderr.as_slice(),
    ]);
    for bytes in streams {
        let text = String::from_utf8_lossy(bytes)
            .replace(&dir_text, "")
            .to_lowercase();
        assert!(
            !text.contains(&reference),
            "the item reference is not public"
        );
        for word in ["seed", "secret", "private", "password"] {
            assert!(!text.contains(word), "output must not mention `{word}`");
        }
    }
}

// Kills: commands.rs status reaching the service or writing state (NoService panics on any
// client request; the directory must be byte-identical after every read-only mode).
#[tokio::test]
async fn node_status_is_local_and_read_only() {
    let (fx, _init, _assign, _fingerprint) = assigned().await;
    let before = tree(fx.dir.path());
    let calls = fx.keys.calls();

    for extra in [&[][..], &["--bundle"][..], &["--card"][..]] {
        let run = fx.status(extra).await;
        ok(&run);
        assert!(!run.stdout.is_empty());
    }

    assert_eq!(tree(fx.dir.path()), before, "status wrote nothing");
    let after = fx.keys.calls();
    assert_eq!(
        (after.0, after.2),
        (calls.0, calls.2),
        "no key creation or recovery"
    );
    for name in before.keys() {
        let lower = name.to_lowercase();
        assert!(
            !lower.ends_with(".db") && !lower.contains("sqlite"),
            "no database file: {name}"
        );
    }
    let names: BTreeSet<&str> = before.keys().map(String::as_str).collect();
    for expected in [
        "khive.toml",
        "khive.toml.node-assignment.json",
        "enrolment.json",
    ] {
        assert!(names.contains(expected), "{expected} is present");
    }
}
