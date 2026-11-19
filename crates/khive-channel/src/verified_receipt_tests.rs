use super::*;
use crate::{DeliveryReceiptBinding, ReceiptDisposition};
use uuid::Uuid;

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).unwrap()
}

fn decode_hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn vector_receipt(disposition: ReceiptDisposition) -> DeliveryReceipt {
    let signature = match disposition {
        ReceiptDisposition::Stored => {
            "00de06d16479cd2a99fd6e66ae90ad66716c9af015fcba6b65db52029cbbe83bc9161dac398929cfb7955e3eb3cd8f4ae69e2cde7331d9aeb2a1f597a6e12f01"
        }
        ReceiptDisposition::Quarantined => {
            "1925fea98935bc24b684df54d625b60748de38292a55a6215802607e51b44fda8190120b7b5460562e1ab3404c7096931144ab6ac2d3201e1b5543b96939aa0c"
        }
    };
    DeliveryReceipt {
        binding: DeliveryReceiptBinding {
            protocol_version: 1,
            logical_message_id: uuid("6f1c2d3e-4a5b-4c6d-8e7f-90a1b2c3d4e5"),
            sender_agent_id: "01920000-0000-7000-8000-00000000a001".into(),
            recipient_agent_id: "01920000-0000-7000-8000-00000000a002".into(),
            recipient_device_id: uuid("01920000-0000-7000-8000-00000000d002"),
            recipient_key_epoch: 2,
            contact_generation: 3,
            delivery_attempt_id: uuid("01920000-0000-7000-8000-0000000e0001"),
        },
        disposition,
        signature: decode_hex(signature),
    }
}

fn vector_key(value: &str) -> [u8; 32] {
    decode_hex(value).try_into().unwrap()
}

fn assert_identity_key_refused(pinned_key: [u8; 32]) {
    let recipient_key =
        vector_key("a914d2b78bbef06e728db06ad577d1c09d04dae4a078ab7b7574187d9dc5d032");
    for disposition in [ReceiptDisposition::Stored, ReceiptDisposition::Quarantined] {
        VerifiedRecipientReceipt::verify(vector_receipt(disposition), &recipient_key)
            .expect("the valid receipt must verify before the forged-key assertion");
        let mut forged = vector_receipt(disposition);
        forged.signature = vec![0; 64];
        forged.signature[0] = 1;
        assert!(
            !receipt_signing_input(&forged).unwrap().is_empty(),
            "the forgery must target an actual valid receipt binding"
        );
        assert!(
            matches!(
                VerifiedRecipientReceipt::verify(forged, &pinned_key),
                Err(ReceiptVerificationError::InvalidSignature)
            ),
            "a degenerate pinned key must not produce a verified {disposition:?} receipt"
        );
    }
}

#[test]
fn identity_pinned_recipient_key_cannot_verify_forged_receipts() {
    let mut identity = [0; 32];
    identity[0] = 1;
    assert_identity_key_refused(identity);
}

#[test]
fn identity_sign_bit_variant_cannot_verify_forged_receipts() {
    let mut identity_with_sign_bit = [0; 32];
    identity_with_sign_bit[0] = 1;
    identity_with_sign_bit[31] = 0x80;
    assert_identity_key_refused(identity_with_sign_bit);
}

#[test]
fn recipient_receipt_vectors_match_signing_input_and_verify() {
    let recipient_key =
        vector_key("a914d2b78bbef06e728db06ad577d1c09d04dae4a078ab7b7574187d9dc5d032");
    let vectors = [
        (
            vector_receipt(ReceiptDisposition::Stored),
            "6b686976652d6e6f64652d76312f7265636569707400000000016f1c2d3e4a5b4c6d8e7f90a1b2c3d4e50192000000007000800000000000a0010192000000007000800000000000a0020192000000007000800000000000d00200000000000000020000000000000003019200000000700080000000000e000101",
        ),
        (
            vector_receipt(ReceiptDisposition::Quarantined),
            "6b686976652d6e6f64652d76312f7265636569707400000000016f1c2d3e4a5b4c6d8e7f90a1b2c3d4e50192000000007000800000000000a0010192000000007000800000000000a0020192000000007000800000000000d00200000000000000020000000000000003019200000000700080000000000e000102",
        ),
    ];
    for (receipt, expected_input) in vectors {
        assert_eq!(
            receipt_signing_input(&receipt).unwrap(),
            decode_hex(expected_input)
        );
        VerifiedRecipientReceipt::verify(receipt, &recipient_key)
            .expect("recipient receipt vector must verify");
    }
}

#[test]
fn stored_node_receipt_verifies_with_pinned_recipient_key() {
    let recipient_key =
        vector_key("a914d2b78bbef06e728db06ad577d1c09d04dae4a078ab7b7574187d9dc5d032");
    VerifiedRecipientReceipt::verify(vector_receipt(ReceiptDisposition::Stored), &recipient_key)
        .expect("stored node receipt must verify with its pinned recipient key");
}

#[test]
fn invalid_recipient_receipt_signatures_are_refused() {
    let recipient_key =
        vector_key("a914d2b78bbef06e728db06ad577d1c09d04dae4a078ab7b7574187d9dc5d032");
    let sender_key = vector_key("9016672157bdb5b3529477312593f8e6fbf59641a52a374d50bd72fdf0f5d2af");
    let stored = vector_receipt(ReceiptDisposition::Stored);

    let mut quarantined_input = stored.clone();
    quarantined_input.disposition = ReceiptDisposition::Quarantined;
    assert!(VerifiedRecipientReceipt::verify(quarantined_input, &recipient_key).is_err());

    let mut different_attempt = stored.clone();
    different_attempt.binding.delivery_attempt_id = uuid("01920000-0000-7000-8000-0000000e0002");
    assert!(VerifiedRecipientReceipt::verify(different_attempt, &recipient_key).is_err());

    let mut different_message = stored.clone();
    different_message.binding.logical_message_id = uuid("6f1c2d3e-4a5b-4c6d-8e7f-90a1b2c3d4e6");
    assert!(VerifiedRecipientReceipt::verify(different_message, &recipient_key).is_err());

    let mut different_sender = stored.clone();
    different_sender.binding.sender_agent_id = "01920000-0000-7000-8000-00000000a003".into();
    assert!(VerifiedRecipientReceipt::verify(different_sender, &recipient_key).is_err());
    assert!(VerifiedRecipientReceipt::verify(stored.clone(), &sender_key).is_err());

    let mut invalid_agent_id = stored.clone();
    invalid_agent_id.binding.sender_agent_id = "not-a-uuid".into();
    assert!(VerifiedRecipientReceipt::verify(invalid_agent_id, &recipient_key).is_err());

    let mut wrong_signature_length = stored;
    wrong_signature_length.signature.pop();
    assert!(VerifiedRecipientReceipt::verify(wrong_signature_length, &recipient_key).is_err());
}
