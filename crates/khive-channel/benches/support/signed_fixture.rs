//! Generated, deterministic Ed25519 fixture; construction is always untimed.
use khive_channel::{
    receipt_signing_input, DeliveryReceipt, DeliveryReceiptBinding, ReceiptDisposition,
    ReceiptSigningPublicKey,
};
use ring::signature::{Ed25519KeyPair, KeyPair};
use uuid::Uuid;

pub fn signed_receipt() -> (DeliveryReceipt, [u8; 32]) {
    let key = Ed25519KeyPair::from_seed_unchecked(&[0x52; 32]).expect("fixed seed");
    let mut receipt = DeliveryReceipt {
        binding: DeliveryReceiptBinding {
            protocol_version: 1,
            logical_message_id: Uuid::from_u128(5652),
            sender_agent_id: Uuid::from_u128(1).to_string(),
            recipient_agent_id: Uuid::from_u128(2).to_string(),
            recipient_device_id: Uuid::from_u128(3),
            recipient_key_epoch: 7,
            contact_generation: 11,
            delivery_attempt_id: Uuid::from_u128(13),
        },
        disposition: ReceiptDisposition::Stored,
        signature: Vec::new(),
    };
    let input = receipt_signing_input(&receipt).expect("canonical receipt input");
    receipt.signature = key.sign(&input).as_ref().to_vec();
    let pinned = key.public_key().as_ref().try_into().expect("public key");
    (receipt, pinned)
}

pub struct SignatureFixture {
    pub key: ReceiptSigningPublicKey,
    pub input: Vec<u8>,
    pub signature: [u8; 64],
}

impl SignatureFixture {
    pub fn new() -> Self {
        let (receipt, pinned) = signed_receipt();
        Self {
            key: ReceiptSigningPublicKey::new(pinned).expect("admit generated key"),
            input: receipt_signing_input(&receipt).expect("canonical input"),
            signature: receipt.signature.try_into().expect("signature length"),
        }
    }
}
