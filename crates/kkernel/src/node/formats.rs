//! Strict input and output formats for the node setup commands.
//!
//! Input files are parsed with an exact field set, canonical value encodings and
//! duplicate-key rejection. Nothing here reads files or touches the network.
//!
//! The success DTOs and `StatusReport` are the specified output shapes; the
//! command layer may serialize its own equivalents, so unused items are allowed
//! (including in test builds, where the receipts are not constructed).
#![allow(dead_code)]

use super::NodeSetupError;
use khive_channel_node::encoding::{
    CanonicalUuid, Epoch, HexBytes, NodeAddress, ProtocolVersion, Realm,
};
use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

/// Largest input file, in bytes, that the parsers accept.
const MAX_INPUT_BYTES: usize = 65_536;
/// Longest accepted display label, in UTF-8 bytes.
const MAX_LABEL_BYTES: usize = 64;
/// The seven fields of a share card; an enrolment adds `device_id`.
const CARD_FIELDS: [&str; 7] = [
    "protocol_version",
    "address",
    "agent_id",
    "realm",
    "label",
    "key_epoch",
    "fingerprint",
];

const MSG_NOT_UTF8: &str = "input must be UTF-8";
const MSG_TOO_LARGE: &str = "input exceeds 65536 bytes";
const MSG_SHAPE: &str = "input must be one JSON object with exactly the required fields";
const MSG_VERSION: &str = "only protocol version 1 is supported";
const MSG_INVALID: &str = "input contains an invalid identifier, key, fingerprint, label or epoch";
const MSG_ADDRESS: &str = "address does not match the supplied realm and agent";

fn refusal(message: &str) -> NodeSetupError {
    NodeSetupError {
        exit: 2,
        message: message.to_owned(),
    }
}

/// The complete enrolment result: a share card plus the device id.
///
/// Serialization writes the eight members in declared order. Deserialization goes
/// through the same strict parser as [`parse_enrolment`], so unknown, missing,
/// duplicate and nested members are all refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Enrolment {
    pub protocol_version: ProtocolVersion,
    pub address: NodeAddress,
    pub agent_id: CanonicalUuid,
    pub realm: Realm,
    pub label: String,
    pub key_epoch: Epoch,
    pub fingerprint: HexBytes<32>,
    pub device_id: CanonicalUuid,
}

/// A contact's introduction card: an enrolment without the device id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ShareCard {
    pub protocol_version: ProtocolVersion,
    pub address: NodeAddress,
    pub agent_id: CanonicalUuid,
    pub realm: Realm,
    pub label: String,
    pub key_epoch: Epoch,
    pub fingerprint: HexBytes<32>,
}

/// The input accepted by `pin`: seven fields or the full eight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CardInput {
    Card(ShareCard),
    Enrolment(Enrolment),
}

/// Top-level members of an input object. Deserializing it fails on a repeated
/// key, which `serde_json::Value` and derived structs would silently accept.
struct RawFields(BTreeMap<String, Value>);

impl<'de> Deserialize<'de> for RawFields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldsVisitor;
        impl<'de> Visitor<'de> for FieldsVisitor {
            type Value = RawFields;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("one JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<RawFields, A::Error> {
                let mut fields = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, Value>()? {
                    if fields.insert(key, value).is_some() {
                        return Err(A::Error::custom("duplicate member"));
                    }
                }
                Ok(RawFields(fields))
            }
        }
        deserializer.deserialize_map(FieldsVisitor)
    }
}

impl<'de> Deserialize<'de> for Enrolment {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawFields::deserialize(deserializer)?;
        build_enrolment(&raw.0).map_err(|e| D::Error::custom(e.message))
    }
}

impl<'de> Deserialize<'de> for ShareCard {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawFields::deserialize(deserializer)?;
        build_card(&raw.0).map_err(|e| D::Error::custom(e.message))
    }
}

fn read_object(bytes: &[u8]) -> Result<BTreeMap<String, Value>, NodeSetupError> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(refusal(MSG_TOO_LARGE));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| refusal(MSG_NOT_UTF8))?;
    serde_json::from_str::<RawFields>(text)
        .map(|raw| raw.0)
        .map_err(|_| refusal(MSG_SHAPE))
}

fn text<'a>(raw: &'a BTreeMap<String, Value>, key: &str) -> Result<&'a str, NodeSetupError> {
    raw.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| refusal(MSG_SHAPE))
}

fn number<'a>(raw: &'a BTreeMap<String, Value>, key: &str) -> Result<&'a Value, NodeSetupError> {
    raw.get(key)
        .filter(|value| value.is_number())
        .ok_or_else(|| refusal(MSG_SHAPE))
}

fn has_exact_keys(raw: &BTreeMap<String, Value>, with_device: bool) -> bool {
    raw.len() == CARD_FIELDS.len() + usize::from(with_device)
        && CARD_FIELDS.iter().all(|key| raw.contains_key(*key))
        && (!with_device || raw.contains_key("device_id"))
}

fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_LABEL_BYTES
        && !label.trim().is_empty()
        && !label.chars().any(char::is_control)
}

/// Validates the field set and every value, in this order: shape and JSON types,
/// canonical encodings, protocol version, address agreement.
fn build(
    raw: &BTreeMap<String, Value>,
    with_device: bool,
) -> Result<(ShareCard, Option<CanonicalUuid>), NodeSetupError> {
    if !has_exact_keys(raw, with_device) {
        return Err(refusal(MSG_SHAPE));
    }
    let version = number(raw, "protocol_version")?;
    let epoch = number(raw, "key_epoch")?;
    let address = text(raw, "address")?;
    let agent_id = text(raw, "agent_id")?;
    let realm = text(raw, "realm")?;
    let label = text(raw, "label")?;
    let fingerprint = text(raw, "fingerprint")?;
    let device = if with_device {
        Some(text(raw, "device_id")?)
    } else {
        None
    };

    let invalid = |_| refusal(MSG_INVALID);
    let address = NodeAddress::parse(address).map_err(invalid)?;
    let agent_id = CanonicalUuid::parse(agent_id).map_err(invalid)?;
    let realm = Realm::parse(realm).map_err(invalid)?;
    if !valid_label(label) {
        return Err(refusal(MSG_INVALID));
    }
    let key_epoch = epoch
        .as_u64()
        .ok_or_else(|| refusal(MSG_INVALID))
        .and_then(|value| Epoch::new(value).map_err(invalid))?;
    let fingerprint = HexBytes::<32>::parse(fingerprint).map_err(invalid)?;
    let device_id = device
        .map(|value| CanonicalUuid::parse(value).map_err(invalid))
        .transpose()?;
    let protocol_version = version
        .as_u64()
        .ok_or_else(|| refusal(MSG_VERSION))
        .and_then(|value| ProtocolVersion::new(value).map_err(|_| refusal(MSG_VERSION)))?;
    if address.realm() != &realm || address.agent() != agent_id {
        return Err(refusal(MSG_ADDRESS));
    }
    Ok((
        ShareCard {
            protocol_version,
            address,
            agent_id,
            realm,
            label: label.to_owned(),
            key_epoch,
            fingerprint,
        },
        device_id,
    ))
}

fn build_card(raw: &BTreeMap<String, Value>) -> Result<ShareCard, NodeSetupError> {
    build(raw, false).map(|(card, _)| card)
}

fn build_enrolment(raw: &BTreeMap<String, Value>) -> Result<Enrolment, NodeSetupError> {
    let (card, device_id) = build(raw, true)?;
    let device_id = device_id.ok_or_else(|| refusal(MSG_SHAPE))?;
    Ok(Enrolment {
        protocol_version: card.protocol_version,
        address: card.address,
        agent_id: card.agent_id,
        realm: card.realm,
        label: card.label,
        key_epoch: card.key_epoch,
        fingerprint: card.fingerprint,
        device_id,
    })
}

/// Parses the raw eight-member enrolment object. A nested card, a wrapping
/// envelope, a share card alone, or any extra, missing or repeated member is
/// refused with exit 2.
pub(crate) fn parse_enrolment(bytes: &[u8]) -> Result<Enrolment, NodeSetupError> {
    build_enrolment(&read_object(bytes)?)
}

/// Parses a contact card: exactly the seven card members, or the eight
/// enrolment members. Any other member set is refused with exit 2.
pub(crate) fn parse_card(bytes: &[u8]) -> Result<CardInput, NodeSetupError> {
    let raw = read_object(bytes)?;
    if raw.contains_key("device_id") {
        build_enrolment(&raw).map(CardInput::Enrolment)
    } else {
        build_card(&raw).map(CardInput::Card)
    }
}

/// The fingerprint block: a `Fingerprint:` heading line, then the 64 hex digits
/// as four lines of four groups of four digits. Each digit line is indented by
/// two spaces and there is no trailing newline; the caller prints the block
/// verbatim. Removing the heading, spaces and newlines gives back the stored
/// value.
pub(crate) fn fingerprint_lines(fingerprint: &HexBytes<32>) -> String {
    let digits: String = fingerprint
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let lines: Vec<String> = digits
        .as_bytes()
        .chunks(16)
        .map(|line| {
            let groups: Vec<&str> = line
                .chunks(4)
                .map(|group| std::str::from_utf8(group).unwrap_or_default())
                .collect();
            format!("  {}", groups.join(" "))
        })
        .collect();
    format!("Fingerprint:\n{}", lines.join("\n"))
}

/// Success output of `init`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct InitializedReceipt {
    pub state: &'static str,
    pub enabled: bool,
    pub config: String,
    pub bundle_file: String,
    pub fingerprint: HexBytes<32>,
}

/// Success output of `assign`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct AssignedReceipt {
    pub state: &'static str,
    pub changed: bool,
    pub enabled: bool,
    pub config: String,
    pub address: NodeAddress,
    pub restart_required: bool,
}

/// Success output of `pin`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct PinnedReceipt {
    pub state: &'static str,
    pub changed: bool,
    pub address: NodeAddress,
    pub device_id: CanonicalUuid,
    pub key_epoch: Epoch,
    pub fingerprint: HexBytes<32>,
    pub restart_required: bool,
}

/// Output of `status --bundle` or `--card` when written to a file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ExportReceipt {
    pub state: &'static str,
    pub kind: &'static str,
    pub file: String,
}

/// Local setup state reported by `status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SetupState {
    Absent,
    Prepared,
    Initialized,
    Assigned,
    Disabled,
}

/// Whether the login-store item was opened and matched the saved fingerprint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum KeychainCheck {
    NotChecked,
    Matches,
}

/// The variable part of the `status` output. The `daemon` and `delivery` members
/// are always written as `not_checked`: status only inspects local files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StatusReport {
    pub state: SetupState,
    pub config: String,
    pub enabled: bool,
    pub service_url: Option<String>,
    pub realm: Option<String>,
    pub actor: Option<String>,
    pub namespace: String,
    pub slug: Option<String>,
    pub address: Option<NodeAddress>,
    pub device_id: Option<CanonicalUuid>,
    pub key_epoch: Option<Epoch>,
    pub fingerprint: Option<HexBytes<32>>,
    pub pin_count: usize,
    pub config_digest: Option<String>,
    pub keychain: KeychainCheck,
}

#[derive(Serialize)]
struct StatusWire<'a> {
    state: SetupState,
    config: &'a str,
    enabled: bool,
    service_url: &'a Option<String>,
    realm: &'a Option<String>,
    actor: &'a Option<String>,
    namespace: &'a str,
    slug: &'a Option<String>,
    address: &'a Option<NodeAddress>,
    device_id: &'a Option<CanonicalUuid>,
    key_epoch: &'a Option<Epoch>,
    fingerprint: &'a Option<HexBytes<32>>,
    pin_count: usize,
    config_digest: &'a Option<String>,
    keychain: KeychainCheck,
    daemon: &'static str,
    delivery: &'static str,
}

impl Serialize for StatusReport {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let daemon = "not_checked";
        let delivery = "not_checked";
        StatusWire {
            state: self.state,
            config: &self.config,
            enabled: self.enabled,
            service_url: &self.service_url,
            realm: &self.realm,
            actor: &self.actor,
            namespace: &self.namespace,
            slug: &self.slug,
            address: &self.address,
            device_id: &self.device_id,
            key_epoch: &self.key_epoch,
            fingerprint: &self.fingerprint,
            pin_count: self.pin_count,
            config_digest: &self.config_digest,
            keychain: self.keychain,
            daemon,
            delivery,
        }
        .serialize(serializer)
    }
}
