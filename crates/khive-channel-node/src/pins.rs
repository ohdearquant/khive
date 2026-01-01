use crate::encoding::{CanonicalUuid, Epoch, HexBytes, Realm};
use crate::keys::DevicePublicKeys;
use crate::ProtocolError;
use async_trait::async_trait;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinIdentity {
    pub realm: Realm,
    pub agent: CanonicalUuid,
    pub device: CanonicalUuid,
    pub epoch: Epoch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmedPin {
    identity: PinIdentity,
    keys: DevicePublicKeys,
    fingerprint: HexBytes<32>,
}
impl ConfirmedPin {
    pub fn new(
        identity: PinIdentity,
        keys: DevicePublicKeys,
        fingerprint: HexBytes<32>,
    ) -> Result<Self, ProtocolError> {
        if keys.fingerprint() != fingerprint {
            return Err(ProtocolError::InvalidKey);
        }
        Ok(Self {
            identity,
            keys,
            fingerprint,
        })
    }
    pub fn identity(&self) -> &PinIdentity {
        &self.identity
    }
    pub fn keys(&self) -> &DevicePublicKeys {
        &self.keys
    }
    pub fn fingerprint(&self) -> &HexBytes<32> {
        &self.fingerprint
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnconfirmedPin {
    pub identity: PinIdentity,
    pub keys: DevicePublicKeys,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PinState {
    Confirmed(ConfirmedPin),
    NonContact,
    UnconfirmedEpoch { candidate: Option<UnconfirmedPin> },
    FingerprintMismatch,
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
#[error("pin source unavailable: {0}")]
pub struct PinSourceError(pub String);

#[async_trait]
pub trait PinSource: Send + Sync {
    async fn resolve(&self, identity: &PinIdentity) -> Result<PinState, PinSourceError>;
}
