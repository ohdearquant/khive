//! Confirmed peer pins backed by the runtime's retained pin history.

use super::config::invalid;
use async_trait::async_trait;
use khive_channel_node::encoding::{CanonicalUuid, Epoch, HexBytes};
use khive_channel_node::keys::{DevicePublicKeys, KemPublicKey, SigningPublicKey};
use khive_channel_node::pins::{ConfirmedPin, PinIdentity, PinSource, PinSourceError, PinState};
use khive_channel_node::source::NodeClientBinding;
use khive_runtime::node_config::{NodeChannelConfig, NodePinConfig};
use khive_runtime::node_pins::{NodePinKey, NodePinRecord};
use khive_runtime::{KhiveRuntime, RuntimeError};
use tokio::sync::OnceCell;

fn pin_error(detail: &str) -> PinSourceError {
    PinSourceError(detail.to_string())
}

/// Resolves exact device-epoch pins from the runtime's retained history. Configured pins
/// are imported once per instance; a stored record becomes a confirmed pin only through
/// `ConfirmedPin::new`.
pub(crate) struct RuntimePinSource {
    runtime: KhiveRuntime,
    namespace: String,
    slug: String,
    records: Vec<NodePinRecord>,
    retained: OnceCell<()>,
}

impl RuntimePinSource {
    /// Validate every configured pin without I/O.
    pub(crate) fn new(
        runtime: KhiveRuntime,
        config: &NodeChannelConfig,
        binding: &NodeClientBinding,
    ) -> Result<Self, RuntimeError> {
        let mut records = Vec::with_capacity(config.pins.len());
        for pin in &config.pins {
            records.push(configured_record(pin, binding)?);
        }
        Ok(Self {
            runtime,
            namespace: binding.namespace.clone(),
            slug: binding.slug.clone(),
            records,
            retained: OnceCell::new(),
        })
    }

    /// Retain the configured pins; one successful import per instance. A failure leaves the
    /// cell empty so the next call retries.
    pub(crate) async fn retain_configured(&self) -> Result<(), PinSourceError> {
        self.retained
            .get_or_try_init(|| async {
                if self.records.is_empty() {
                    return Ok::<(), PinSourceError>(());
                }
                self.runtime
                    .retain_confirmed_node_pins(self.records.clone())
                    .await
                    .map_err(|_| pin_error("configured pin retention failed"))
            })
            .await
            .map(|_| ())
    }
}

fn configured_record(
    pin: &NodePinConfig,
    binding: &NodeClientBinding,
) -> Result<NodePinRecord, RuntimeError> {
    let agent =
        CanonicalUuid::parse(&pin.agent_id).map_err(|_| invalid("pin agent_id is invalid"))?;
    let device =
        CanonicalUuid::parse(&pin.device_id).map_err(|_| invalid("pin device_id is invalid"))?;
    let epoch = Epoch::new(pin.key_epoch).map_err(|_| invalid("pin key_epoch is invalid"))?;
    let kem = HexBytes::<32>::parse(&pin.kem_public_key)
        .map_err(|_| invalid("pin kem_public_key is invalid"))?;
    let signing = HexBytes::<32>::parse(&pin.signing_public_key)
        .map_err(|_| invalid("pin signing_public_key is invalid"))?;
    let fingerprint = HexBytes::<32>::parse(&pin.fingerprint)
        .map_err(|_| invalid("pin fingerprint is invalid"))?;
    let keys = DevicePublicKeys {
        kem: KemPublicKey::new(*kem.as_bytes())
            .map_err(|_| invalid("pin kem_public_key is invalid"))?,
        signing: SigningPublicKey::new(*signing.as_bytes())
            .map_err(|_| invalid("pin signing_public_key is invalid"))?,
    };
    let identity = PinIdentity {
        realm: binding.realm.clone(),
        agent,
        device,
        epoch,
    };
    ConfirmedPin::new(identity, keys, fingerprint.clone())
        .map_err(|_| invalid("pin fingerprint does not match its keys"))?;
    Ok(NodePinRecord {
        key: NodePinKey {
            namespace: binding.namespace.clone(),
            slug: binding.slug.clone(),
            realm: binding.realm.as_str().to_string(),
            agent_id: agent.into_uuid(),
            device_id: device.into_uuid(),
            key_epoch: epoch.get(),
        },
        kem_public_key: *kem.as_bytes(),
        signing_public_key: *signing.as_bytes(),
        fingerprint: *fingerprint.as_bytes(),
    })
}

#[async_trait]
impl PinSource for RuntimePinSource {
    async fn resolve(&self, identity: &PinIdentity) -> Result<PinState, PinSourceError> {
        self.retain_configured().await?;
        let key = NodePinKey {
            namespace: self.namespace.clone(),
            slug: self.slug.clone(),
            realm: identity.realm.as_str().to_string(),
            agent_id: identity.agent.into_uuid(),
            device_id: identity.device.into_uuid(),
            key_epoch: identity.epoch.get(),
        };
        let record = self
            .runtime
            .confirmed_node_pin(key)
            .await
            .map_err(|_| pin_error("pin read failed"))?;
        let Some(record) = record else {
            return Ok(PinState::UnconfirmedEpoch { candidate: None });
        };
        let keys = DevicePublicKeys {
            kem: KemPublicKey::new(record.kem_public_key)
                .map_err(|_| pin_error("stored pin kem key is invalid"))?,
            signing: SigningPublicKey::new(record.signing_public_key)
                .map_err(|_| pin_error("stored pin signing key is invalid"))?,
        };
        // The only path from a stored record to a confirmed pin. A record that fails it is a
        // source error: never confirmed, never unconfirmed.
        let pin = ConfirmedPin::new(identity.clone(), keys, HexBytes::new(record.fingerprint))
            .map_err(|_| pin_error("stored pin failed confirmation"))?;
        Ok(PinState::Confirmed(pin))
    }
}
