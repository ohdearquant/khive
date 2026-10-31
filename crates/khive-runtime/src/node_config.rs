//! Node channel configuration: the `[channel.node]` table.
//!
//! The table configures this installation as one node of a node channel. It
//! is off by default. When it is disabled or absent the runtime behaves exactly
//! as it did before the table existed and the config identity is unchanged.
//!
//! This module owns the field types, the semantic validation and the config
//! digest. Key material never appears here: the table names a key by its
//! keychain reference and fingerprint only.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::engine_config::ConfigError;

/// Maximum number of peer pins accepted in one table.
pub const MAX_NODE_PINS: usize = 4096;

/// Largest value a key epoch may take.
const MAX_KEY_EPOCH: u64 = 4_294_967_295;

/// The `[channel]` section of the khive config file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelSectionConfig {
    /// The `[channel.node]` table, when present.
    #[serde(default)]
    pub node: Option<NodeChannelConfig>,
}

impl ChannelSectionConfig {
    /// Validate the section. An absent or disabled node table is always valid
    /// once it has parsed.
    pub fn validate(&self) -> Result<(), ConfigError> {
        match &self.node {
            Some(node) => node.validate(),
            None => Ok(()),
        }
    }
}

/// The `[channel.node]` table.
///
/// Unknown keys and wrong types are rejected at parse time even when the table
/// is disabled. Semantic checks run only when `enabled` is true.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct NodeChannelConfig {
    /// Whether the node channel is on. Defaults to false.
    pub enabled: bool,
    /// Short name of this node (lowercase letters, digits, `_`, `-`).
    pub slug: Option<String>,
    /// Namespace the node channel reads and writes. Defaults to `local`.
    pub namespace: String,
    /// Actor label the channel acts as.
    pub actor: Option<String>,
    /// Realm the node belongs to.
    pub realm: Option<String>,
    /// Origin of the channel service.
    pub service_url: Option<String>,
    /// Identifier of this agent, a canonical UUID.
    pub agent_id: Option<String>,
    /// Identifier of this device, a canonical UUID.
    pub device_id: Option<String>,
    /// Epoch of the key this node currently uses.
    pub key_epoch: Option<u64>,
    /// Keychain account that holds the key. The key itself is never in config.
    pub key_reference: Option<String>,
    /// Fingerprint of the key, 64 lowercase hex characters.
    pub key_fingerprint: Option<String>,
    /// Long-poll wait in seconds, 0 to 25.
    pub poll_wait_seconds: u64,
    /// Outbox drain interval in seconds, 1 to 60.
    pub outbox_interval_seconds: u64,
    /// Messages handled per batch, 1 to 256.
    pub batch_size: u32,
    /// Pair policy evaluation mode: `off`, `shadow` or `enforce`. Defaults to
    /// `off`. Pair policy evaluation is not available yet, so with `shadow` or
    /// `enforce` a new node-addressed send is refused before anything is
    /// written.
    pub pair_policy_mode: khive_db::stores::note::transport::PolicyMode,
    /// Pinned peer identities.
    pub pins: Vec<NodePinConfig>,
}

impl Default for NodeChannelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            slug: None,
            namespace: "local".to_string(),
            actor: None,
            realm: None,
            service_url: None,
            agent_id: None,
            device_id: None,
            key_epoch: None,
            key_reference: None,
            key_fingerprint: None,
            poll_wait_seconds: 25,
            outbox_interval_seconds: 5,
            batch_size: 64,
            pair_policy_mode: khive_db::stores::note::transport::PolicyMode::Off,
            pins: Vec::new(),
        }
    }
}

/// One pinned peer identity under `[[channel.node.pins]]`. Every field is
/// required; there are no defaults.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodePinConfig {
    /// Peer agent identifier, a canonical UUID.
    pub agent_id: String,
    /// Peer device identifier, a canonical UUID.
    pub device_id: String,
    /// Peer key epoch.
    pub key_epoch: u64,
    /// Peer key-agreement public key, 64 lowercase hex characters.
    pub kem_public_key: String,
    /// Peer signing public key, 64 lowercase hex characters.
    pub signing_public_key: String,
    /// Peer key fingerprint, 64 lowercase hex characters.
    pub fingerprint: String,
}

fn invalid(reason: impl Into<String>) -> ConfigError {
    ConfigError::InvalidNodeConfig {
        reason: reason.into(),
    }
}

fn required<'a>(value: &'a Option<String>, field: &str) -> Result<&'a str, ConfigError> {
    value
        .as_deref()
        .ok_or_else(|| invalid(format!("{field} is required when enabled")))
}

fn is_canonical_uuid(value: &str) -> bool {
    match uuid::Uuid::parse_str(value) {
        Ok(parsed) => parsed.hyphenated().to_string() == value,
        Err(_) => false,
    }
}

fn is_lower_hex_64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn all_bytes(value: &str, allowed: impl Fn(u8) -> bool) -> bool {
    value.bytes().all(allowed)
}

impl NodeChannelConfig {
    /// Validate the table. A disabled table is accepted without semantic
    /// checks. An enabled table is checked field by field in table order, then
    /// the pins in order, and the first failure is returned.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.enabled {
            return Ok(());
        }

        let slug = required(&self.slug, "slug")?;
        if slug.is_empty()
            || slug.len() > 64
            || !all_bytes(slug, |b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'
            })
        {
            return Err(invalid(
                "slug must contain 1..=64 lowercase letters, digits, '_' or '-'",
            ));
        }

        if khive_types::namespace::Namespace::parse(&self.namespace).is_err() {
            return Err(invalid("namespace is invalid"));
        }

        let actor = required(&self.actor, "actor")?;
        if actor.trim().is_empty() || actor.len() > 256 || actor.chars().any(char::is_control) {
            return Err(invalid(
                "actor must be a nonempty actor label of at most 256 bytes",
            ));
        }

        let realm = required(&self.realm, "realm")?;
        if realm.is_empty()
            || realm.len() > 64
            || !all_bytes(realm, |b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'_' || b == b'-'
            })
        {
            return Err(invalid(
                "realm must contain 1..=64 lowercase letters, digits, '.', '_' or '-'",
            ));
        }

        // The origin rules (HTTPS, no credentials, query or fragment) belong to
        // the channel client that dials the service. The runtime only requires
        // that the field is present and not blank.
        let service_url = required(&self.service_url, "service_url")?;
        if service_url.trim().is_empty() {
            return Err(invalid("service_url is required when enabled"));
        }

        let agent_id = required(&self.agent_id, "agent_id")?;
        if !is_canonical_uuid(agent_id) {
            return Err(invalid("agent_id must be a canonical UUID"));
        }

        let device_id = required(&self.device_id, "device_id")?;
        if !is_canonical_uuid(device_id) {
            return Err(invalid("device_id must be a canonical UUID"));
        }

        let key_epoch = self
            .key_epoch
            .ok_or_else(|| invalid("key_epoch is required when enabled"))?;
        if key_epoch == 0 || key_epoch > MAX_KEY_EPOCH {
            return Err(invalid("key_epoch must be in 1..=4294967295"));
        }

        let key_reference = required(&self.key_reference, "key_reference")?;
        if key_reference.is_empty()
            || key_reference.len() > 128
            || !all_bytes(key_reference, |b| {
                b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-'
            })
        {
            return Err(invalid(
                "key_reference must be a nonempty keychain account identifier of at most 128 bytes",
            ));
        }

        let key_fingerprint = required(&self.key_fingerprint, "key_fingerprint")?;
        if !is_lower_hex_64(key_fingerprint) {
            return Err(invalid(
                "key_fingerprint must be 64 lowercase hex characters",
            ));
        }

        if self.poll_wait_seconds > 25 {
            return Err(invalid("poll_wait_seconds must be in 0..=25"));
        }
        if !(1..=60).contains(&self.outbox_interval_seconds) {
            return Err(invalid("outbox_interval_seconds must be in 1..=60"));
        }
        if !(1..=256).contains(&self.batch_size) {
            return Err(invalid("batch_size must be in 1..=256"));
        }

        if self.pins.len() > MAX_NODE_PINS {
            return Err(invalid("pins exceeds 4096 entries"));
        }
        for (i, pin) in self.pins.iter().enumerate() {
            if !is_canonical_uuid(&pin.agent_id) {
                return Err(invalid(format!(
                    "pins[{i}].agent_id must be a canonical UUID"
                )));
            }
            if !is_canonical_uuid(&pin.device_id) {
                return Err(invalid(format!(
                    "pins[{i}].device_id must be a canonical UUID"
                )));
            }
            if pin.key_epoch == 0 || pin.key_epoch > MAX_KEY_EPOCH {
                return Err(invalid(format!(
                    "pins[{i}].key_epoch must be in 1..=4294967295"
                )));
            }
            for (field, value) in [
                ("kem_public_key", &pin.kem_public_key),
                ("signing_public_key", &pin.signing_public_key),
                ("fingerprint", &pin.fingerprint),
            ] {
                if !is_lower_hex_64(value) {
                    return Err(invalid(format!(
                        "pins[{i}].{field} must be 64 lowercase hex characters"
                    )));
                }
            }
        }

        let mut identities: Vec<(&str, &str, u64)> = self
            .pins
            .iter()
            .map(|pin| (pin.agent_id.as_str(), pin.device_id.as_str(), pin.key_epoch))
            .collect();
        identities.sort_unstable();
        if identities.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(invalid("pins contains a duplicate identity"));
        }

        Ok(())
    }

    /// SHA-256 over a deterministic JSON rendering of this table, as 64
    /// lowercase hex characters.
    ///
    /// Pins are sorted by identity first, so two tables that differ only in pin
    /// order have the same digest. The digest covers every field, including the
    /// actor the channel acts as.
    pub fn config_digest(&self) -> String {
        let mut canonical = self.clone();
        canonical.pins.sort_by(|a, b| {
            (&a.agent_id, &a.device_id, a.key_epoch).cmp(&(&b.agent_id, &b.device_id, b.key_epoch))
        });
        let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
        Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}
