use super::{collection, nonempty, EnvelopeError, EnvelopeResult};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct KeyRef {
    pub id: String,
    pub generation: u64,
}

/// Trusted composition-root material. No bytes appear in Debug or an envelope.
pub struct ProvisionedKey {
    reference: KeyRef,
    native: [u8; 32],
    extension: [u8; 32],
}
impl ProvisionedKey {
    pub fn new(reference: KeyRef, native: [u8; 32], extension: [u8; 32]) -> EnvelopeResult<Self> {
        nonempty(&reference.id)?;
        if native == [0; 32] || extension == [0; 32] || native == extension {
            return Err(EnvelopeError::Unavailable(
                "keys must be distinct and nonzero",
            ));
        }
        Ok(Self {
            reference,
            native,
            extension,
        })
    }
}
impl std::fmt::Debug for ProvisionedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProvisionedKey")
            .field("reference", &self.reference)
            .finish_non_exhaustive()
    }
}

/// An immutable, explicitly supplied key history. It never generates or derives keys.
#[derive(Debug)]
pub struct SuppliedKeys {
    keys: BTreeMap<KeyRef, ProvisionedKey>,
}
impl SuppliedKeys {
    pub fn provision(keys: Vec<ProvisionedKey>) -> EnvelopeResult<Self> {
        collection(&keys, true)?;
        let mut by_ref = BTreeMap::new();
        for key in keys {
            if by_ref.insert(key.reference.clone(), key).is_some() {
                return Err(EnvelopeError::Unavailable("duplicate key reference"));
            }
        }
        Ok(Self { keys: by_ref })
    }
    pub(super) fn native(&self, reference: &KeyRef) -> EnvelopeResult<[u8; 32]> {
        Ok(self.resolve(reference)?.native)
    }
    pub(super) fn extension(&self, reference: &KeyRef) -> EnvelopeResult<&[u8; 32]> {
        Ok(&self.resolve(reference)?.extension)
    }
    fn resolve(&self, reference: &KeyRef) -> EnvelopeResult<&ProvisionedKey> {
        self.keys.get(reference).ok_or(EnvelopeError::Unavailable(
            "required key generation is missing",
        ))
    }
}
