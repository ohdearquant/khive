//! Private operations stay inside a facility (ADR-105 A.3, A.5).
use crate::encoding::{context, length_prefix, Base64Bytes, CanonicalUuid, HexBytes, Realm};
use crate::envelope::{aad, Ciphertext, EnvelopeHeader, SealedEnvelope, MAX_PLAINTEXT_BYTES};
use crate::ProtocolError;
#[cfg(test)]
use curve25519_dalek::edwards::CompressedEdwardsY;
use ed25519_dalek::{Signer, SigningKey};
use hpke::{
    aead::ChaCha20Poly1305,
    kdf::HkdfSha256,
    kem::{Kem, X25519HkdfSha256},
    Deserializable, OpModeR, OpModeS, Serializable,
};
use khive_channel::ReceiptSigningPublicKey;
use rand_core::{OsRng, RngCore};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

type NodeKem = X25519HkdfSha256;
type PrivateKemKey = <NodeKem as Kem>::PrivateKey;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KemPublicKey(HexBytes<32>);
impl KemPublicKey {
    pub fn new(bytes: [u8; 32]) -> Result<Self, ProtocolError> {
        // RFC 7748 section 6.1: small-order points produce all-zero DH.
        // This fixed, public probe scalar is not facility private material.
        if x25519_dalek::x25519([0x42; 32], bytes) == [0; 32] {
            return Err(ProtocolError::InvalidKey);
        }
        Ok(Self(HexBytes::new(bytes)))
    }
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }
}
impl Serialize for KemPublicKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for KemPublicKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(*HexBytes::<32>::deserialize(deserializer)?.as_bytes()).map_err(D::Error::custom)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SigningPublicKey(ReceiptSigningPublicKey);
impl SigningPublicKey {
    pub fn new(bytes: [u8; 32]) -> Result<Self, ProtocolError> {
        ReceiptSigningPublicKey::new(bytes)
            .map(Self)
            .map_err(|_| ProtocolError::InvalidKey)
    }
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }
    pub fn verify(&self, input: &[u8], signature: &[u8; 64]) -> Result<(), ProtocolError> {
        self.0
            .verify(input, signature)
            .map_err(|_| ProtocolError::InvalidSignature)
    }
}
impl Serialize for SigningPublicKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        HexBytes::new(*self.as_bytes()).serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for SigningPublicKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(*HexBytes::<32>::deserialize(deserializer)?.as_bytes()).map_err(D::Error::custom)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevicePublicKeys {
    pub kem: KemPublicKey,
    pub signing: SigningPublicKey,
}
impl DevicePublicKeys {
    pub fn fingerprint(&self) -> HexBytes<32> {
        let mut hash = Sha256::new();
        hash.update(b"khive-node-v1/device-keys\0");
        hash.update(self.kem.as_bytes());
        hash.update(self.signing.as_bytes());
        HexBytes::new(hash.finalize().into())
    }
    pub fn enrol_signing_input(&self, realm: &Realm) -> Result<Vec<u8>, ProtocolError> {
        let mut bytes = context("enrol")?;
        bytes.extend_from_slice(&length_prefix(realm.as_str().as_bytes())?);
        bytes.extend_from_slice(self.kem.as_bytes());
        bytes.extend_from_slice(self.signing.as_bytes());
        Ok(bytes)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrolmentBundle {
    pub realm: Realm,
    pub kem_public_key: KemPublicKey,
    pub signing_public_key: SigningPublicKey,
    pub enrol_proof: Base64Bytes<64>,
}
impl EnrolmentBundle {
    pub fn verify(&self) -> Result<DevicePublicKeys, ProtocolError> {
        let keys = DevicePublicKeys {
            kem: self.kem_public_key.clone(),
            signing: self.signing_public_key.clone(),
        };
        keys.signing
            .verify(
                &keys.enrol_signing_input(&self.realm)?,
                self.enrol_proof.as_bytes(),
            )
            .map_err(|_| ProtocolError::InvalidProof)?;
        Ok(keys)
    }
}

/// A private-operation capability; implementations never return private key bytes.
pub trait KeyFacility: Send + Sync {
    fn public_keys(&self) -> DevicePublicKeys;
    fn sign(&self, input: &[u8]) -> [u8; 64];
    fn seal(
        &self,
        header: &EnvelopeHeader,
        logical_message_id: CanonicalUuid,
        recipient: &KemPublicKey,
        plaintext: &[u8],
    ) -> Result<SealedEnvelope, ProtocolError>;
    /// `pinned_sender` must be the contact key confirmed by the caller at the header's epoch.
    fn open(
        &self,
        header: &EnvelopeHeader,
        logical_message_id: CanonicalUuid,
        pinned_sender: &KemPublicKey,
        envelope: &SealedEnvelope,
    ) -> Result<Vec<u8>, ProtocolError>;
    fn enrolment_bundle(&self, realm: Realm) -> Result<EnrolmentBundle, ProtocolError> {
        let keys = self.public_keys();
        let proof = self.sign(&keys.enrol_signing_input(&realm)?);
        Ok(EnrolmentBundle {
            realm,
            kem_public_key: keys.kem,
            signing_public_key: keys.signing,
            enrol_proof: Base64Bytes::new(proof),
        })
    }
}

// SigningKey exists only inside this private operation and wipes its expanded
// secret on drop. The retained seed is separately Zeroizing storage.
fn signing_operation(seed: &[u8; 32], input: &[u8]) -> ([u8; 64], [u8; 32]) {
    let key = SigningKey::from_bytes(seed);
    (key.sign(input).to_bytes(), key.verifying_key().to_bytes())
}

/// Volatile keys with zeroized private storage and no Debug or export API.
pub struct InMemoryKeyFacility {
    kem: PrivateKemKey,
    signing_seed: Box<Zeroizing<[u8; 32]>>,
    signing_public_key: SigningPublicKey,
}
impl InMemoryKeyFacility {
    pub fn generate() -> Result<Self, ProtocolError> {
        Self::generate_with_rng(&mut OsRng)
    }
    fn generate_with_rng<R: RngCore + rand_core::CryptoRng>(
        rng: &mut R,
    ) -> Result<Self, ProtocolError> {
        let mut ikm = Zeroizing::new([0u8; 32]);
        // Allocate before filling so facility moves relocate only the pointer.
        let mut seed = Box::new(Zeroizing::new([0u8; 32]));
        rng.try_fill_bytes(ikm.as_mut())
            .map_err(|_| ProtocolError::Randomness)?;
        rng.try_fill_bytes(&mut seed[..])
            .map_err(|_| ProtocolError::Randomness)?;
        let (kem, _) = NodeKem::derive_keypair(ikm.as_ref());
        Ok(Self {
            kem,
            signing_public_key: SigningPublicKey::new(signing_operation(&seed, &[]).1)?,
            signing_seed: seed,
        })
    }
    #[cfg(test)]
    pub(crate) fn from_test_seeds(ikm: &[u8; 32], seed: &[u8; 32]) -> Self {
        let mut signing_seed = Box::new(Zeroizing::new([0u8; 32]));
        signing_seed.copy_from_slice(seed);
        Self {
            kem: NodeKem::derive_keypair(ikm).0,
            signing_public_key: SigningPublicKey::new(signing_operation(seed, &[]).1).unwrap(),
            signing_seed,
        }
    }
    #[cfg(test)]
    pub(crate) fn seal_test(
        &self,
        header: &EnvelopeHeader,
        id: CanonicalUuid,
        recipient: &KemPublicKey,
        plaintext: &[u8],
        ikm: [u8; 32],
    ) -> Result<SealedEnvelope, ProtocolError> {
        self.seal_with_rng(header, id, recipient, plaintext, &mut TestEphemeral(ikm))
    }
    fn seal_with_rng<R: RngCore + rand_core::CryptoRng>(
        &self,
        header: &EnvelopeHeader,
        id: CanonicalUuid,
        recipient: &KemPublicKey,
        plaintext: &[u8],
        rng: &mut R,
    ) -> Result<SealedEnvelope, ProtocolError> {
        if plaintext.len() > MAX_PLAINTEXT_BYTES {
            return Err(ProtocolError::EnvelopeTooLarge);
        }
        let recipient = <NodeKem as Kem>::PublicKey::from_bytes(recipient.as_bytes())
            .map_err(|_| ProtocolError::InvalidKey)?;
        let mode = OpModeS::Auth((self.kem.clone(), NodeKem::sk_to_pk(&self.kem)));
        let mut rng = FallibleRng {
            inner: rng,
            failed: false,
        };
        let setup = hpke::setup_sender::<ChaCha20Poly1305, HkdfSha256, NodeKem, _>(
            &mode,
            &recipient,
            &header.info()?,
            &mut rng,
        );
        if rng.failed {
            return Err(ProtocolError::Randomness);
        }
        let (enc, mut context) = setup.map_err(|_| ProtocolError::Encryption)?;
        // Fresh context, exactly one seal: sequence number zero.
        let ciphertext = context
            .seal(plaintext, &aad(id)?)
            .map_err(|_| ProtocolError::Encryption)?;
        Ok(SealedEnvelope {
            enc: Base64Bytes::new(enc.to_bytes().into()),
            ciphertext: Ciphertext::new(ciphertext)?,
        })
    }
}
impl KeyFacility for InMemoryKeyFacility {
    fn public_keys(&self) -> DevicePublicKeys {
        DevicePublicKeys {
            kem: KemPublicKey::new(NodeKem::sk_to_pk(&self.kem).to_bytes().into())
                .expect("generated KEM public key"),
            signing: self.signing_public_key.clone(),
        }
    }
    fn sign(&self, input: &[u8]) -> [u8; 64] {
        signing_operation(&self.signing_seed, input).0
    }
    fn seal(
        &self,
        header: &EnvelopeHeader,
        id: CanonicalUuid,
        recipient: &KemPublicKey,
        plaintext: &[u8],
    ) -> Result<SealedEnvelope, ProtocolError> {
        // The production API exposes no caller RNG or deterministic ephemeral input.
        self.seal_with_rng(header, id, recipient, plaintext, &mut OsRng)
    }
    fn open(
        &self,
        header: &EnvelopeHeader,
        id: CanonicalUuid,
        pinned_sender: &KemPublicKey,
        envelope: &SealedEnvelope,
    ) -> Result<Vec<u8>, ProtocolError> {
        let sender = <NodeKem as Kem>::PublicKey::from_bytes(pinned_sender.as_bytes())
            .map_err(|_| ProtocolError::InvalidKey)?;
        let enc = <NodeKem as Kem>::EncappedKey::from_bytes(envelope.enc.as_bytes())
            .map_err(|_| ProtocolError::Decryption)?;
        let mut context = hpke::setup_receiver::<ChaCha20Poly1305, HkdfSha256, NodeKem>(
            &OpModeR::Auth(sender),
            &self.kem,
            &enc,
            &header.info()?,
        )
        .map_err(|_| ProtocolError::Decryption)?;
        context
            .open(envelope.ciphertext.as_bytes(), &aad(id)?)
            .map_err(|_| ProtocolError::Decryption)
    }
}

// HPKE requires infallible fills. A failed fill cannot release a context or
// ciphertext; its placeholder is discarded before any seal operation.
struct FallibleRng<'a, R> {
    inner: &'a mut R,
    failed: bool,
}
impl<R: rand_core::CryptoRng> rand_core::CryptoRng for FallibleRng<'_, R> {}
impl<R: RngCore> RngCore for FallibleRng<'_, R> {
    fn next_u32(&mut self) -> u32 {
        rand_core::impls::next_u32_via_fill(self)
    }
    fn next_u64(&mut self) -> u64 {
        rand_core::impls::next_u64_via_fill(self)
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        let _ = self.try_fill_bytes(dest);
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.inner.try_fill_bytes(dest).inspect_err(|_| {
            dest.fill(0);
            self.failed = true;
        })
    }
}

#[cfg(test)]
pub(crate) struct TestEphemeral(pub(crate) [u8; 32]);
#[cfg(test)]
impl rand_core::CryptoRng for TestEphemeral {}
#[cfg(test)]
impl RngCore for TestEphemeral {
    fn next_u32(&mut self) -> u32 {
        panic!("test vector uses one 32-byte fill")
    }
    fn next_u64(&mut self) -> u64 {
        panic!("test vector uses one 32-byte fill")
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        assert_eq!(dest.len(), 32);
        dest.copy_from_slice(&self.0);
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

#[cfg(test)]
#[path = "keys_r3_tests.rs"]
mod r3_tests;
