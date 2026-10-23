//! Authenticated durable envelopes, not current authorization.
//!
//! A successful cryptographic verification does not establish live ancestry,
//! administrator authority, or permission to commit a protected effect. The
//! authority service must check those facts using its active writer transaction.

mod codec;
mod envelope;
mod keys;
mod permission;

pub use envelope::{
    AuthenticatedEnvelope, BootstrapSource, CreationOperation, EnvelopeDraft, EnvelopeIssuer,
    EnvelopeVerifier, GrantBody, IssuanceAssurance, IssuanceContext, NativeCapability,
    ParentBinding,
};
pub use keys::{KeyRef, ProvisionedKey, SuppliedKeys};
pub use permission::*;

pub const MAX_RECORD_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_COLLECTION_ITEMS: usize = 256;
pub const ALIAS_START: u128 = 1_u128 << 126;
pub const ALIAS_END: u128 = 1_u128 << 127;
pub const CARRIER_VERSION: u16 = 1;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EnvelopeError {
    #[error("invalid input: {0}")]
    InvalidInput(&'static str),
    #[error("{field} has {measured} bytes/items; maximum is {bound}")]
    OverLimit {
        field: &'static str,
        measured: usize,
        bound: usize,
    },
    #[error("right not supported by the M1 profile: {0}")]
    RightNotSupported(UnsupportedRight),
    #[error("cryptographic or structural verification rejected: {0}")]
    Reject(&'static str),
    #[error("verification material unavailable: {0}")]
    Unavailable(&'static str),
    #[error("native local identifier capacity exhausted")]
    IdSpaceExhausted,
    #[error("native delegation failed: {0}")]
    Native(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UnsupportedRight {
    #[error("ordinary_namespace_right_not_supported_in_m1")]
    OrdinaryNamespace,
    #[error("claim_time_bundle_not_supported_in_m1")]
    ClaimTimePlatformBundleInM1,
    #[error("internal_producer_not_public_registration")]
    InternalProducer,
}

pub type EnvelopeResult<T> = Result<T, EnvelopeError>;

fn bound(field: &'static str, measured: usize, maximum: usize) -> EnvelopeResult<()> {
    if measured > maximum {
        return Err(EnvelopeError::OverLimit {
            field,
            measured,
            bound: maximum,
        });
    }
    Ok(())
}
fn nonempty(value: &str) -> EnvelopeResult<()> {
    if value.is_empty() {
        return Err(EnvelopeError::InvalidInput("empty exact text"));
    }
    bound("text", value.len(), MAX_RECORD_BYTES)
}
fn collection<T>(values: &[T], required: bool) -> EnvelopeResult<()> {
    bound("collection", values.len(), MAX_COLLECTION_ITEMS)?;
    if required && values.is_empty() {
        return Err(EnvelopeError::InvalidInput("empty collection"));
    }
    Ok(())
}
fn sorted_unique<T: Ord + codec::Wire>(values: &[T], required: bool) -> EnvelopeResult<()> {
    collection(values, required)?;
    let mut previous = None;
    for value in values {
        let encoded = codec::encode(value)?;
        if previous
            .as_ref()
            .is_some_and(|bytes: &Vec<u8>| bytes >= &encoded)
        {
            return Err(EnvelopeError::InvalidInput(
                "collection must be sorted and unique",
            ));
        }
        previous = Some(encoded);
    }
    Ok(())
}

#[cfg(test)]
mod cold_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod fixtures;
