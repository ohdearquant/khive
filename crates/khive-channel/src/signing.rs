//! Public signing-key admission and verification shared by channel callers.
use curve25519_dalek::edwards::CompressedEdwardsY;

/// A signing public key or signature refused by the shared verifier.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReceiptSignatureError {
    #[error("invalid signing public key")]
    InvalidKey,
    #[error("invalid signature")]
    InvalidSignature,
}

/// A canonical Ed25519 public key that cannot contain a small-order point.
///
/// Construction validates public key encoding and order. Signature verification
/// uses ring; callers separately check the signed input and its pinned identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiptSigningPublicKey([u8; 32]);

impl ReceiptSigningPublicKey {
    pub fn new(bytes: [u8; 32]) -> Result<Self, ReceiptSignatureError> {
        let point = CompressedEdwardsY(bytes)
            .decompress()
            .ok_or(ReceiptSignatureError::InvalidKey)?;
        if point.is_small_order() || point.compress().to_bytes() != bytes {
            return Err(ReceiptSignatureError::InvalidKey);
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn verify(&self, input: &[u8], signature: &[u8; 64]) -> Result<(), ReceiptSignatureError> {
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, self.as_bytes())
            .verify(input, signature)
            .map_err(|_| ReceiptSignatureError::InvalidSignature)
    }
}
