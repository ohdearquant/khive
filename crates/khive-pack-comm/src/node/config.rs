//! Derivations from the validated `[channel.node]` table: binding, poll wait, key
//! fingerprint and service URL. No I/O. Error details are fixed strings that never carry
//! config values, response bodies or OS errors.

use khive_channel_node::client::validate_service_url;
use khive_channel_node::encoding::{CanonicalUuid, Epoch, HexBytes, PollWait, Realm};
use khive_channel_node::source::NodeClientBinding;
use khive_runtime::node_config::NodeChannelConfig;
use khive_runtime::RuntimeError;

/// A bounded configuration error in the `channel.node:` namespace.
pub(super) fn invalid(detail: &str) -> RuntimeError {
    RuntimeError::InvalidInput(format!("channel.node: {detail}"))
}

fn required<'a>(value: &'a Option<String>, field: &str) -> Result<&'a str, RuntimeError> {
    value
        .as_deref()
        .ok_or_else(|| invalid(&format!("{field} is required when enabled")))
}

/// The local sender binding derived from an enabled config.
pub(super) fn binding(config: &NodeChannelConfig) -> Result<NodeClientBinding, RuntimeError> {
    let realm =
        Realm::parse(required(&config.realm, "realm")?).map_err(|_| invalid("realm is invalid"))?;
    let agent = CanonicalUuid::parse(required(&config.agent_id, "agent_id")?)
        .map_err(|_| invalid("agent_id is invalid"))?;
    let device = CanonicalUuid::parse(required(&config.device_id, "device_id")?)
        .map_err(|_| invalid("device_id is invalid"))?;
    let key_epoch = config
        .key_epoch
        .ok_or_else(|| invalid("key_epoch is required when enabled"))?;
    let key_epoch = Epoch::new(key_epoch).map_err(|_| invalid("key_epoch is invalid"))?;
    Ok(NodeClientBinding {
        namespace: config.namespace.clone(),
        slug: required(&config.slug, "slug")?.to_string(),
        realm,
        agent,
        device,
        key_epoch,
        key_reference: required(&config.key_reference, "key_reference")?.to_string(),
    })
}

/// The configured long-poll wait, validated to the protocol range.
pub(super) fn poll_wait(config: &NodeChannelConfig) -> Result<PollWait, RuntimeError> {
    PollWait::new(config.poll_wait_seconds).map_err(|_| invalid("poll_wait_seconds is invalid"))
}

/// The configured key fingerprint as raw bytes.
pub(super) fn fingerprint_bytes(config: &NodeChannelConfig) -> Result<[u8; 32], RuntimeError> {
    HexBytes::<32>::parse(required(&config.key_fingerprint, "key_fingerprint")?)
        .map(|bytes| *bytes.as_bytes())
        .map_err(|_| invalid("key_fingerprint is invalid"))
}

/// The configured service origin, checked by the one shared URL check.
pub(super) fn service_url(config: &NodeChannelConfig) -> Result<&str, RuntimeError> {
    let url = required(&config.service_url, "service_url")?;
    validate_service_url(url).map_err(|_| invalid("service_url is invalid"))?;
    Ok(url)
}
