//! Tests for the strict node setup formats.

use super::formats::{
    fingerprint_lines, parse_card, parse_enrolment, CardInput, Enrolment, KeychainCheck,
    SetupState, ShareCard, StatusReport,
};
use super::NodeSetupError;
use khive_channel_node::encoding::HexBytes;
use serde_json::{json, Value};

const AGENT: &str = "a1111111-1111-4111-8111-11111111111b";
const OTHER_AGENT: &str = "33333333-3333-4333-8333-333333333333";
const DEVICE: &str = "c2222222-2222-4222-8222-22222222222d";

const SHAPE: &str = "input must be one JSON object with exactly the required fields";
const INVALID: &str = "input contains an invalid identifier, key, fingerprint, label or epoch";
const VERSION: &str = "only protocol version 1 is supported";
const ADDRESS: &str = "address does not match the supplied realm and agent";

fn fingerprint() -> String {
    "ab".repeat(32)
}

fn enrolment_value() -> Value {
    json!({
        "protocol_version": 1,
        "address": format!("khive1:acme/{AGENT}"),
        "agent_id": AGENT,
        "realm": "acme",
        "label": "Work laptop",
        "key_epoch": 1,
        "fingerprint": fingerprint(),
        "device_id": DEVICE,
    })
}

fn card_value() -> Value {
    let mut value = enrolment_value();
    value.as_object_mut().expect("object").remove("device_id");
    value
}

fn with(mut value: Value, key: &str, replacement: Value) -> Value {
    value[key] = replacement;
    value
}

fn without(mut value: Value, key: &str) -> Value {
    value.as_object_mut().expect("object").remove(key);
    value
}

fn bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("serialize")
}

fn assert_refused(result: Result<impl std::fmt::Debug, NodeSetupError>, message: &str) {
    match result {
        Err(error) => {
            assert_eq!(error.exit, 2, "exit code for {message}");
            assert_eq!(error.message, message);
        }
        Ok(parsed) => panic!("expected refusal `{message}`, parsed {parsed:?}"),
    }
}

fn assert_enrolment_refused(value: &Value, message: &str) {
    assert_refused(parse_enrolment(&bytes(value)), message);
}

#[test]
fn node_formats_strict_shape() {
    // Matching controls: the raw flattened enrolment and both card shapes pass.
    let enrolment = parse_enrolment(&bytes(&enrolment_value())).expect("raw enrolment");
    assert_eq!(enrolment.agent_id.to_string(), AGENT);
    assert_eq!(enrolment.device_id.to_string(), DEVICE);
    assert_eq!(enrolment.realm.as_str(), "acme");
    assert_eq!(enrolment.label, "Work laptop");
    assert_eq!(enrolment.key_epoch.get(), 1);
    assert_eq!(enrolment.protocol_version.get(), 1);
    assert_eq!(enrolment.fingerprint.as_bytes(), &[0xab; 32]);
    match parse_card(&bytes(&card_value())).expect("seven-field card") {
        CardInput::Card(card) => assert_eq!(card.agent_id.to_string(), AGENT),
        other => panic!("expected a share card, got {other:?}"),
    }
    match parse_card(&bytes(&enrolment_value())).expect("eight-field card") {
        CardInput::Enrolment(parsed) => assert_eq!(parsed, enrolment),
        other => panic!("expected an enrolment, got {other:?}"),
    }

    // Output keeps the declared field order and round-trips.
    let text = serde_json::to_string(&enrolment).expect("serialize");
    assert!(text.starts_with("{\"protocol_version\":1,\"address\":\"khive1:acme/"));
    assert!(text.ends_with(&format!("\"device_id\":\"{DEVICE}\"}}")));
    assert_eq!(
        serde_json::to_value(&enrolment).expect("value"),
        enrolment_value()
    );

    // The serde entry points are strict too.
    assert_eq!(
        serde_json::from_slice::<Enrolment>(&bytes(&enrolment_value())).expect("serde"),
        enrolment
    );
    assert!(serde_json::from_slice::<Enrolment>(&bytes(&card_value())).is_err());
    assert!(serde_json::from_slice::<ShareCard>(&bytes(&enrolment_value())).is_err());
    let duplicated = text.replacen(
        "\"label\":\"Work laptop\"",
        "\"label\":\"a\",\"label\":\"b\"",
        1,
    );
    assert!(serde_json::from_str::<Enrolment>(&duplicated).is_err());

    // A nested card, an MCP envelope and a share card alone are not enrolments.
    let nested = json!({ "card": card_value(), "device_id": DEVICE });
    assert_enrolment_refused(&nested, SHAPE);
    let envelope = json!({
        "content": [{ "type": "text", "text": serde_json::to_string(&enrolment_value()).unwrap() }],
        "isError": false,
    });
    assert_enrolment_refused(&envelope, SHAPE);
    assert_enrolment_refused(&json!({ "result": enrolment_value() }), SHAPE);
    assert_enrolment_refused(&card_value(), SHAPE);
    assert_refused(parse_card(&bytes(&nested)), SHAPE);
    assert_refused(parse_card(&bytes(&envelope)), SHAPE);

    // Unknown, missing and wrongly typed members.
    for key in ["service_url", "tenant_id", "extra"] {
        let extra = with(enrolment_value(), key, json!("x"));
        assert_enrolment_refused(&extra, SHAPE);
        assert_refused(parse_card(&bytes(&extra)), SHAPE);
        let extra_card = with(card_value(), key, json!("x"));
        assert_refused(parse_card(&bytes(&extra_card)), SHAPE);
    }
    for key in [
        "protocol_version",
        "address",
        "agent_id",
        "realm",
        "label",
        "key_epoch",
        "fingerprint",
        "device_id",
    ] {
        assert_enrolment_refused(&without(enrolment_value(), key), SHAPE);
    }
    assert_refused(parse_card(&bytes(&without(card_value(), "label"))), SHAPE);
    assert_enrolment_refused(
        &with(enrolment_value(), "protocol_version", json!("1")),
        SHAPE,
    );
    assert_enrolment_refused(&with(enrolment_value(), "key_epoch", json!("1")), SHAPE);
    assert_enrolment_refused(&with(enrolment_value(), "label", json!(7)), SHAPE);
    assert_enrolment_refused(&with(enrolment_value(), "realm", Value::Null), SHAPE);

    // Duplicate members are refused even when both values are valid.
    let base = serde_json::to_string(&enrolment_value()).unwrap();
    let duplicate = base.replacen(
        "\"realm\":\"acme\"",
        "\"realm\":\"acme\",\"realm\":\"acme\"",
        1,
    );
    assert_ne!(duplicate, base);
    assert_refused(parse_enrolment(duplicate.as_bytes()), SHAPE);
    assert_refused(parse_card(duplicate.as_bytes()), SHAPE);

    // Not one JSON object.
    for input in ["[]", "null", "\"text\"", "", "{", "{} {}"] {
        assert_refused(parse_enrolment(input.as_bytes()), SHAPE);
        assert_refused(parse_card(input.as_bytes()), SHAPE);
    }
    let trailing = format!("{base} x");
    assert_refused(parse_enrolment(trailing.as_bytes()), SHAPE);

    // Size and encoding limits.
    assert_refused(parse_enrolment(&[0xff, 0xfe]), "input must be UTF-8");
    assert_refused(parse_card(&[0xff, 0xfe]), "input must be UTF-8");
    assert_refused(
        parse_enrolment(&vec![b' '; 65_537]),
        "input exceeds 65536 bytes",
    );
    assert_refused(parse_card(&vec![b' '; 65_537]), "input exceeds 65536 bytes");

    // Protocol version.
    for version in [json!(0), json!(2), json!(1.5), json!(-1)] {
        assert_enrolment_refused(
            &with(enrolment_value(), "protocol_version", version),
            VERSION,
        );
    }

    // Canonical values.
    let upper_agent = AGENT.to_uppercase();
    let braced = format!("{{{AGENT}}}");
    let invalid_cases = [
        ("agent_id", json!(upper_agent)),
        ("agent_id", json!(braced)),
        ("agent_id", json!("not-a-uuid")),
        ("device_id", json!(DEVICE.to_uppercase())),
        ("device_id", json!("")),
        ("realm", json!("ACME")),
        ("realm", json!("")),
        ("realm", json!("a".repeat(65))),
        ("address", json!(format!("acme/{AGENT}"))),
        ("address", json!(format!("khive2:acme/{AGENT}"))),
        ("address", json!("khive1:acme")),
        ("key_epoch", json!(0)),
        ("key_epoch", json!(4_294_967_296_u64)),
        ("key_epoch", json!(-1)),
        ("key_epoch", json!(1.5)),
        ("fingerprint", json!(fingerprint().to_uppercase())),
        ("fingerprint", json!("ab".repeat(31))),
        ("fingerprint", json!("ab".repeat(33))),
        ("fingerprint", json!("g".repeat(64))),
        ("label", json!("")),
        ("label", json!("   ")),
        ("label", json!("a".repeat(65))),
        ("label", json!("a\u{7}b")),
        ("label", json!("line\nbreak")),
    ];
    for (key, replacement) in invalid_cases {
        let value = with(enrolment_value(), key, replacement.clone());
        assert_enrolment_refused(&value, INVALID);
        if key != "device_id" {
            let card = with(card_value(), key, replacement);
            assert_refused(parse_card(&bytes(&card)), INVALID);
        }
    }

    // Label boundaries that must still pass: 64 bytes, and multibyte text.
    let edge = with(enrolment_value(), "label", json!("a".repeat(64)));
    assert!(parse_enrolment(&bytes(&edge)).is_ok());
    let wide = with(enrolment_value(), "label", json!("\u{e9}".repeat(32)));
    assert!(parse_enrolment(&bytes(&wide)).is_ok());
    let too_wide = with(enrolment_value(), "label", json!("\u{e9}".repeat(33)));
    assert_enrolment_refused(&too_wide, INVALID);

    // The address must agree with the realm and agent members.
    let other_realm = with(
        enrolment_value(),
        "address",
        json!(format!("khive1:other/{AGENT}")),
    );
    assert_enrolment_refused(&other_realm, ADDRESS);
    assert_refused(
        parse_card(&bytes(&with(
            card_value(),
            "address",
            json!(format!("khive1:other/{AGENT}")),
        ))),
        ADDRESS,
    );
    let other_agent = with(
        enrolment_value(),
        "address",
        json!(format!("khive1:acme/{OTHER_AGENT}")),
    );
    assert_enrolment_refused(&other_agent, ADDRESS);
}

#[test]
fn node_formats_fingerprint_lines_group_without_changing_the_value() {
    let digits = "0123456789abcdef".repeat(4);
    let fingerprint = HexBytes::<32>::parse(&digits).expect("fingerprint");
    let text = fingerprint_lines(&fingerprint);
    let all: Vec<&str> = text.split('\n').collect();
    assert_eq!(all.len(), 5);
    assert_eq!(all[0], "Fingerprint:");
    assert!(!text.ends_with('\n'));
    let lines = &all[1..];
    for line in lines {
        let rest = line.strip_prefix("  ").expect("two-space indent");
        let groups: Vec<&str> = rest.split(' ').collect();
        assert_eq!(groups.len(), 4);
        assert!(groups.iter().all(|group| group.len() == 4));
    }
    assert_eq!(lines[0], "  0123 4567 89ab cdef");
    assert_eq!(lines[3], "  0123 4567 89ab cdef");
    let joined: String = all[1..]
        .concat()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert_eq!(joined, digits);

    // Distinct groups stay in order.
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = index as u8;
    }
    let ordered = fingerprint_lines(&HexBytes::new(bytes));
    let joined: String = ordered
        .strip_prefix("Fingerprint:")
        .expect("heading")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let expected: String = (0..32u8).map(|b| format!("{b:02x}")).collect();
    assert_eq!(joined, expected);
    assert!(ordered.starts_with("Fingerprint:\n  0001 0203 0405 0607\n  0809 0a0b 0c0d 0e0f\n"));
}

#[test]
fn node_formats_status_never_claims_daemon_or_delivery_checks() {
    let report = StatusReport {
        state: SetupState::Absent,
        config: "/tmp/khive.toml".into(),
        enabled: false,
        service_url: None,
        realm: None,
        actor: None,
        namespace: "local".into(),
        slug: None,
        address: None,
        device_id: None,
        key_epoch: None,
        fingerprint: None,
        pin_count: 0,
        config_digest: None,
        keychain: KeychainCheck::NotChecked,
    };
    let value = serde_json::to_value(&report).expect("status");
    let object = value.as_object().expect("object");
    assert_eq!(object.len(), 17);
    assert_eq!(object["state"], "absent");
    assert_eq!(object["keychain"], "not_checked");
    assert_eq!(object["daemon"], "not_checked");
    assert_eq!(object["delivery"], "not_checked");
    assert_eq!(object["service_url"], Value::Null);
    assert_eq!(object["pin_count"], 0);
}
