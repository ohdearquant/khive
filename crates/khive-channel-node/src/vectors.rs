use crate::{encoding::*, envelope::*, keys::*, plaintext::*, receipt::*, request::*, wire::*};
use hpke::{
    aead::ChaCha20Poly1305,
    kdf::HkdfSha256,
    kem::{Kem, X25519HkdfSha256},
    OpModeR, OpModeS, Serializable,
};
use khive_channel::ReceiptDisposition;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

type K = X25519HkdfSha256;
fn fixture() -> Value {
    serde_json::from_str(include_str!("../tests/fixtures/node-v1-vectors.json")).unwrap()
}
fn value<'a>(f: &'a Value, group: &str, key: &str) -> &'a str {
    f["values"][group]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["key"] == key)
        .unwrap_or_else(|| panic!("missing fixture {group}.{key}"))["value"]
        .as_str()
        .unwrap()
}
fn bytes(f: &Value, group: &str, key: &str) -> Vec<u8> {
    decode_hex(value(f, group, key)).unwrap()
}
fn fixed<const N: usize>(f: &Value, group: &str, key: &str) -> [u8; N] {
    bytes(f, group, key).try_into().unwrap()
}
fn id(value: &str) -> CanonicalUuid {
    CanonicalUuid::parse(value).unwrap()
}
fn facility(f: &Value, group: &str) -> InMemoryKeyFacility {
    InMemoryKeyFacility::from_test_seeds(
        &fixed(f, group, "kem_ikm"),
        &fixed(f, group, "signing_seed"),
    )
}
fn header(f: &Value) -> EnvelopeHeader {
    EnvelopeHeader {
        protocol_version: ProtocolVersion::new(
            value(f, "envelope_context", "protocol_version")
                .parse()
                .unwrap(),
        )
        .unwrap(),
        realm: Realm::parse(value(f, "envelope_context", "realm")).unwrap(),
        sender_agent_id: id(value(f, "envelope", "sender_agent_id")),
        sender_device_id: id(value(f, "envelope", "sender_device_id")),
        sender_key_epoch: Epoch::new(1).unwrap(),
        recipient_agent_id: id(value(f, "envelope", "recipient_agent_id")),
        recipient_device_id: id(value(f, "envelope", "recipient_device_id")),
        recipient_key_epoch: Epoch::new(2).unwrap(),
    }
}
fn sealed(f: &Value) -> SealedEnvelope {
    SealedEnvelope {
        enc: Base64Bytes::new(fixed(f, "envelope", "enc")),
        ciphertext: Ciphertext::new(bytes(f, "envelope", "ciphertext")).unwrap(),
    }
}
fn receipt_binding(f: &Value) -> WireReceiptBinding {
    let g = "receipt_binding";
    WireReceiptBinding {
        protocol_version: ProtocolVersion::new(value(f, g, "protocol_version").parse().unwrap())
            .unwrap(),
        logical_message_id: id(value(f, g, "logical_message_id")),
        sender_agent_id: id(value(f, g, "sender_agent_id")),
        recipient_agent_id: id(value(f, g, "recipient_agent_id")),
        recipient_device_id: id(value(f, g, "recipient_device_id")),
        recipient_key_epoch: Epoch::new(value(f, g, "recipient_key_epoch").parse().unwrap())
            .unwrap(),
        contact_generation: Epoch::new(value(f, g, "contact_generation").parse().unwrap()).unwrap(),
        delivery_attempt_id: id(value(f, g, "delivery_attempt_id")),
    }
}
fn stored(f: &Value) -> WireReceipt {
    WireReceipt {
        binding: receipt_binding(f),
        disposition: ReceiptDisposition::Stored,
        signature: Base64Bytes::new(fixed(f, "receipt_stored", "signature")),
    }
}

#[test]
fn a11_device_key_vectors() {
    let f = fixture();
    for group in ["device_keys_sender", "device_keys_recipient"] {
        let facility = facility(&f, group);
        let keys = facility.public_keys();
        assert_eq!(
            keys.kem.as_bytes().as_slice(),
            bytes(&f, group, "kem_public_key")
        );
        assert_eq!(
            keys.signing.as_bytes().as_slice(),
            bytes(&f, group, "signing_public_key")
        );
        assert_eq!(
            keys.fingerprint().as_bytes().as_slice(),
            bytes(&f, group, "fingerprint")
        );
        let realm = Realm::parse("relay.example").unwrap();
        assert_eq!(
            keys.enrol_signing_input(&realm).unwrap(),
            bytes(&f, group, "enrol_proof_input")
        );
        let bundle = facility.enrolment_bundle(realm).unwrap();
        assert_eq!(
            bundle.enrol_proof.as_bytes().as_slice(),
            bytes(&f, group, "enrol_proof")
        );
        assert_eq!(bundle.verify().unwrap(), keys);
        let mut bad = bundle;
        bad.enrol_proof = Base64Bytes::new([0; 64]);
        assert!(bad.verify().is_err());
    }
    assert!(KemPublicKey::new([0; 32]).is_err());
    assert!(KemPublicKey::new({
        let mut v = [0; 32];
        v[0] = 1;
        v
    })
    .is_err());
    // Compressed y=2 is not an Ed25519 point.
    assert!(SigningPublicKey::new({
        let mut v = [0; 32];
        v[0] = 2;
        v
    })
    .is_err());
}
#[test]
fn a11_envelope_vector() {
    let f = fixture();
    let sender = facility(&f, "device_keys_sender");
    let recipient = facility(&f, "device_keys_recipient");
    let h = header(&f);
    let logical = id(value(&f, "envelope", "logical_message_id"));
    assert_eq!(h.header().unwrap(), bytes(&f, "envelope", "header"));
    assert_eq!(h.info().unwrap(), bytes(&f, "envelope", "info"));
    assert_eq!(h.info().unwrap().len(), 55);
    assert_eq!(aad(logical).unwrap(), bytes(&f, "envelope", "aad"));
    let plaintext = bytes(&f, "envelope", "plaintext");
    assert_eq!(plaintext, value(&f, "plaintext_json", "utf8").as_bytes());
    assert!(matches!(
        classify_plaintext(&plaintext),
        PlaintextClassification::Valid(_)
    ));
    let actual = sender
        .seal_test(
            &h,
            logical,
            &recipient.public_keys().kem,
            &plaintext,
            fixed(&f, "envelope", "ephemeral_ikm"),
        )
        .unwrap();
    assert_eq!(actual, sealed(&f));
    assert_eq!(
        actual.ciphertext.as_bytes().len(),
        value(&f, "envelope", "ciphertext_len")
            .parse::<usize>()
            .unwrap()
    );
    assert_eq!(
        recipient
            .open(&h, logical, &sender.public_keys().kem, &actual)
            .unwrap(),
        plaintext
    );
    let (sk_s, pk_s) = K::derive_keypair(&bytes(&f, "device_keys_sender", "kem_ikm"));
    let (_, pk_r) = K::derive_keypair(&bytes(&f, "device_keys_recipient", "kem_ikm"));
    let (secret, enc) = K::encap(
        &pk_r,
        Some((&sk_s, &pk_s)),
        &mut TestEphemeral(fixed(&f, "envelope", "ephemeral_ikm")),
    )
    .unwrap();
    assert_eq!(secret.0.as_slice(), bytes(&f, "envelope", "shared_secret"));
    assert_eq!(enc.to_bytes().as_slice(), bytes(&f, "envelope", "enc"));
}
#[test]
fn a11_receipt_vectors() {
    let f = fixture();
    let recipient = facility(&f, "device_keys_recipient");
    for (disposition, group) in [
        (ReceiptDisposition::Stored, "receipt_stored"),
        (ReceiptDisposition::Quarantined, "receipt_quarantined"),
    ] {
        let receipt = WireReceipt::sign(receipt_binding(&f), disposition, &recipient).unwrap();
        assert_eq!(
            receipt.signing_input().unwrap(),
            bytes(&f, group, "signing_input")
        );
        assert_eq!(
            receipt.signature.as_bytes().as_slice(),
            bytes(&f, group, "signature")
        );
        receipt.verify(&recipient.public_keys().signing).unwrap();
        assert_eq!(
            WireReceipt::from_channel(&receipt.to_channel()).unwrap(),
            receipt
        );
    }
}
#[test]
fn a11_request_authentication_vector() {
    let f = fixture();
    let sender = facility(&f, "device_keys_sender");
    let g = "request_authentication";
    let body = value(&f, "request_body_json", "utf8").as_bytes();
    assert_eq!(Sha256::digest(body).as_slice(), bytes(&f, g, "body_sha256"));
    let device = id(value(&f, g, "device_id"));
    let timestamp = value(&f, g, "timestamp").parse::<u64>().unwrap();
    let nonce = fixed(&f, g, "nonce");
    let input = request_signing_input(
        device,
        timestamp,
        &nonce,
        value(&f, g, "method"),
        value(&f, g, "path_and_query"),
        body,
    )
    .unwrap();
    assert_eq!(input, bytes(&f, g, "signing_input"));
    let headers = RequestHeaders::sign(
        &sender,
        device,
        timestamp,
        nonce,
        value(&f, g, "method"),
        value(&f, g, "path_and_query"),
        body,
    )
    .unwrap();
    assert_eq!(
        headers.signature.as_bytes().as_slice(),
        bytes(&f, g, "signature")
    );
    sender
        .public_keys()
        .signing
        .verify(&input, headers.signature.as_bytes())
        .unwrap();
    let fields = headers.fields();
    assert_eq!(fields.len(), 4);
    assert_eq!(
        fields[0],
        ("Khive-Device", value(&f, g, "device_id").into())
    );
    assert_eq!(
        fields[1],
        ("Khive-Timestamp", value(&f, g, "timestamp").into())
    );
    assert_eq!(fields[2], ("Khive-Nonce", value(&f, g, "nonce").into()));
    assert_eq!(fields[3].1, encode_base64url(&bytes(&f, g, "signature")));
    let parsed =
        RequestHeaders::parse(&fields[0].1, &fields[1].1, &fields[2].1, &fields[3].1).unwrap();
    assert_eq!(parsed, headers);
    parsed
        .verify(
            &sender.public_keys().signing,
            value(&f, g, "method"),
            value(&f, g, "path_and_query"),
            body,
        )
        .unwrap();
    assert!(
        RequestHeaders::parse(&fields[0].1, "01790193600", &fields[2].1, &fields[3].1).is_err()
    );
    assert!(RequestHeaders::parse(
        &fields[0].1,
        &fields[1].1,
        &fields[2].1.to_uppercase(),
        &fields[3].1
    )
    .is_err());
    assert!(RequestHeaders::parse(
        &fields[0].1,
        &fields[1].1,
        &fields[2].1,
        &format!("{}=", fields[3].1)
    )
    .is_err());
    let request: SubmitRequest = serde_json::from_slice(body).unwrap();
    assert_eq!(
        request.enc.as_bytes().as_slice(),
        bytes(&f, "envelope", "enc")
    );
    assert_eq!(
        request.ciphertext.as_bytes(),
        bytes(&f, "envelope", "ciphertext")
    );
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        serde_json::from_slice::<Value>(body).unwrap()
    );
}
#[test]
fn all_eleven_a11_negative_vectors_are_refused() {
    let f = fixture();
    assert_eq!(f["negative_vectors"].as_array().unwrap().len(), 11);
    let sender = facility(&f, "device_keys_sender");
    let recipient = facility(&f, "device_keys_recipient");
    let h = header(&f);
    let logical = id(value(&f, "envelope", "logical_message_id"));
    let envelope = sealed(&f);
    let mut altered = envelope.clone();
    let mut ciphertext = altered.ciphertext.as_bytes().to_vec();
    *ciphertext.last_mut().unwrap() ^= 1;
    altered.ciphertext = Ciphertext::new(ciphertext).unwrap();
    assert!(
        recipient
            .open(&h, logical, &sender.public_keys().kem, &altered)
            .is_err(),
        "negative 1: tag"
    );
    let mut changed_epoch = h.clone();
    changed_epoch.recipient_key_epoch = Epoch::new(3).unwrap();
    assert!(
        recipient
            .open(
                &changed_epoch,
                logical,
                &sender.public_keys().kem,
                &envelope
            )
            .is_err(),
        "negative 2: epoch"
    );
    let wrong_logical = id("6f1c2d3e-4a5b-4c6d-8e7f-90a1b2c3d4e6");
    assert!(
        recipient
            .open(&h, wrong_logical, &sender.public_keys().kem, &envelope)
            .is_err(),
        "negative 3: aad"
    );
    let impostor = K::derive_keypair(&Sha256::digest(b"khive-node-v1 vector impostor kem")).1;
    assert!(
        recipient
            .open(
                &h,
                logical,
                &KemPublicKey::new(impostor.to_bytes().into()).unwrap(),
                &envelope
            )
            .is_err(),
        "negative 4: sender"
    );
    let receipt = stored(&f);
    let key = recipient.public_keys().signing;
    let mut changed = receipt.clone();
    changed.disposition = ReceiptDisposition::Quarantined;
    assert!(changed.verify(&key).is_err(), "negative 5: disposition");
    changed = receipt.clone();
    changed.binding.delivery_attempt_id = id("01920000-0000-7000-8000-0000000e0002");
    assert!(changed.verify(&key).is_err(), "negative 6: attempt");
    assert!(
        receipt.verify(&sender.public_keys().signing).is_err(),
        "negative 7: signer"
    );
    changed = receipt.clone();
    changed.binding.logical_message_id = wrong_logical;
    assert!(changed.verify(&key).is_err(), "negative 8: logical");
    changed = receipt;
    changed.binding.sender_agent_id = id("01920000-0000-7000-8000-00000000a003");
    assert!(changed.verify(&key).is_err(), "negative 9: agent");
    let g = "request_authentication";
    let body = value(&f, "request_body_json", "utf8").as_bytes();
    let device = id(value(&f, g, "device_id"));
    let timestamp = value(&f, g, "timestamp").parse::<u64>().unwrap();
    let nonce = fixed(&f, g, "nonce");
    let signature = fixed(&f, g, "signature");
    let changed =
        request_signing_input(device, timestamp, &nonce, "POST", "/node/v1/receipts", body)
            .unwrap();
    assert!(
        sender
            .public_keys()
            .signing
            .verify(&changed, &signature)
            .is_err(),
        "negative 10: path"
    );
    let mut body = body.to_vec();
    body.push(b' ');
    let changed = request_signing_input(
        device,
        timestamp,
        &nonce,
        "POST",
        "/node/v1/messages",
        &body,
    )
    .unwrap();
    assert!(
        sender
            .public_keys()
            .signing
            .verify(&changed, &signature)
            .is_err(),
        "negative 11: body"
    );
}
#[test]
fn strict_boundary_encodings_refuse_noncanonical_values() {
    assert!(CanonicalUuid::parse("6F1C2D3E-4A5B-4C6D-8E7F-90A1B2C3D4E5").is_err());
    assert!(CanonicalUuid::parse("{6f1c2d3e-4a5b-4c6d-8e7f-90a1b2c3d4e5}").is_err());
    assert!(CanonicalUuid::parse("6f1c2d3e4a5b4c6d8e7f90a1b2c3d4e5").is_err());
    assert!(decode_hex("ABCD").is_err());
    assert!(decode_base64url("AA==").is_err());
    assert!(decode_base64url("AA+").is_err());
    assert!(decode_base64url("_x").is_err());
    assert_eq!(decode_base64url("AA").unwrap(), [0]);
    assert_eq!(decode_hex("abcd").unwrap(), [0xab, 0xcd]);
    assert!(Epoch::new(0).is_err());
    assert!(Epoch::new(u32::MAX as u64 + 1).is_err());
    assert!(JsonInteger::new(MAX_JSON_INTEGER + 1).is_err());
    assert!(serde_json::from_str::<Epoch>("1.0").is_err());
    assert!(serde_json::from_str::<Epoch>("-1").is_err());
    assert!(PollWait::new(26).is_err());
    assert!(Realm::parse("UPPER").is_err());
    assert!(Realm::parse("").is_err());
    assert_eq!(length_prefix(b"x").unwrap(), [0, 1, b'x']);
    assert!(length_prefix(&vec![0; 65536]).is_err());
}
#[test]
fn plaintext_classification_preserves_closed_reasons() {
    let f = fixture();
    let mut valid: Value = serde_json::from_str(value(&f, "plaintext_json", "utf8")).unwrap();
    assert!(matches!(
        classify_plaintext(b"[]"),
        PlaintextClassification::Invalid(InvalidPlaintextReason::NotObject)
    ));
    assert_eq!(
        classify_plaintext(b"{\"v\":1,\"v\":1}"),
        PlaintextClassification::Invalid(InvalidPlaintextReason::DuplicateMember)
    );
    for key in [
        "from",
        "sender",
        "to",
        "recipient",
        "tenant",
        "namespace",
        "actor",
        "project",
        "device",
        "delegation",
    ] {
        let mut value = valid.clone();
        value[key] = json!("untrusted");
        assert_eq!(
            classify_plaintext(&serde_json::to_vec(&value).unwrap()),
            PlaintextClassification::Invalid(InvalidPlaintextReason::ReservedIdentityMember),
            "reserved {key}"
        );
    }
    for kind in [
        Value::Null,
        json!("reply"),
        json!("unspecified"),
        json!("new"),
        json!(1),
    ] {
        let mut value = valid.clone();
        value["kind"] = kind;
        assert_eq!(
            classify_plaintext(&serde_json::to_vec(&value).unwrap()),
            PlaintextClassification::Invalid(InvalidPlaintextReason::InvalidKind)
        );
    }
    for kind in ["announce", "report", "ask"] {
        valid["kind"] = json!(kind);
        assert!(matches!(
            classify_plaintext(&serde_json::to_vec(&valid).unwrap()),
            PlaintextClassification::Valid(_)
        ));
    }
    valid.as_object_mut().unwrap().remove("kind");
    valid["extension"] = json!({"x":1});
    assert!(matches!(
        classify_plaintext(&serde_json::to_vec(&valid).unwrap()),
        PlaintextClassification::Valid(_)
    ));
    valid.as_object_mut().unwrap().remove("subject");
    assert_eq!(
        classify_plaintext(&serde_json::to_vec(&valid).unwrap()),
        PlaintextClassification::Invalid(InvalidPlaintextReason::NotObject)
    );
    let nested = format!(
        "{},\"extension\":{{\"x\":1,\"x\":2}}}}",
        value(&f, "plaintext_json", "utf8").trim_end_matches('}')
    );
    assert_eq!(
        classify_plaintext(nested.as_bytes()),
        PlaintextClassification::Invalid(InvalidPlaintextReason::DuplicateMember)
    );
}
#[test]
fn wire_shapes_refuse_unknown_duplicate_missing_and_oversized_values() {
    let f = fixture();
    let body = value(&f, "request_body_json", "utf8");
    for suffix in [",\"extra\":1}", ",\"sender_key_epoch\":1}"] {
        assert!(serde_json::from_str::<SubmitRequest>(&format!(
            "{}{suffix}",
            body.trim_end_matches('}')
        ))
        .is_err());
    }
    let mut payload: Value = serde_json::from_str(body).unwrap();
    payload.as_object_mut().unwrap().remove("recipient");
    assert!(serde_json::from_value::<SubmitRequest>(payload).is_err());
    assert!(Ciphertext::new(vec![0; MAX_CIPHERTEXT_BYTES + 1]).is_err());
    assert!(BoundedList::<u8, 16>::new(vec![0; 17]).is_err());
    assert!(UtcTimestamp::parse("2026-09-23T20:00:00+01:00").is_err());
    assert_eq!(
        serde_json::from_str::<RefusalResponse>("{\"error\":\"new_service_code\"}")
            .unwrap()
            .error,
        RefusalCode::InvalidRequest
    );
    let sender = facility(&f, "device_keys_sender");
    let recipient = facility(&f, "device_keys_recipient");
    let h = header(&f);
    let logical = id(value(&f, "envelope", "logical_message_id"));
    assert!(sender
        .seal_test(
            &h,
            logical,
            &recipient.public_keys().kem,
            &vec![0; MAX_PLAINTEXT_BYTES + 1],
            [1; 32]
        )
        .is_err());
}
#[test]
fn response_models_cover_each_endpoint_and_enforce_limits() {
    let f = fixture();
    let sender = facility(&f, "device_keys_sender");
    let recipient = facility(&f, "device_keys_recipient");
    let h = header(&f);
    let keys = recipient.public_keys();
    let contact = ContactResponse {
        agent_id: h.recipient_agent_id,
        address: NodeAddress::new(h.realm.clone(), h.recipient_agent_id),
        device_id: h.recipient_device_id,
        key_epoch: h.recipient_key_epoch,
        kem_public_key: keys.kem.clone(),
        signing_public_key: keys.signing.clone(),
        fingerprint: keys.fingerprint(),
        contact_generation: Epoch::new(3).unwrap(),
    };
    let roundtrip: ContactResponse =
        serde_json::from_value(serde_json::to_value(&contact).unwrap()).unwrap();
    assert_eq!(roundtrip.validated_keys(&h.realm).unwrap(), keys);
    let mut inconsistent = contact;
    inconsistent.fingerprint = HexBytes::new([0; 32]);
    assert!(inconsistent.validated_keys(&h.realm).is_err());
    let mut directory = serde_json::to_value(roundtrip).unwrap();
    directory["extra"] = json!(1);
    assert!(serde_json::from_value::<ContactResponse>(directory).is_err());
    let delivery = Delivery {
        delivery_attempt_id: receipt_binding(&f).delivery_attempt_id,
        logical_message_id: id(value(&f, "envelope", "logical_message_id")),
        protocol_version: h.protocol_version,
        sender_agent_id: h.sender_agent_id,
        sender_device_id: h.sender_device_id,
        sender_key_epoch: h.sender_key_epoch,
        recipient_agent_id: h.recipient_agent_id,
        recipient_device_id: h.recipient_device_id,
        recipient_key_epoch: h.recipient_key_epoch,
        contact_generation: Epoch::new(3).unwrap(),
        enc: sealed(&f).enc,
        ciphertext: sealed(&f).ciphertext,
    };
    let now = ServerTimestamp::parse("2026-09-23T20:00:00Z").unwrap();
    let page = PollResponse {
        deliveries: PollItems::new(vec![delivery]).unwrap(),
        receipts: PollItems::new(vec![ReceiptItem {
            seq: JsonInteger::new(41).unwrap(),
            receipt: stored(&f),
            recorded_at: now.clone(),
        }])
        .unwrap(),
        receipts_cursor: JsonInteger::new(41).unwrap(),
        server_time: now.clone(),
    };
    let encoded = serde_json::to_value(&page).unwrap();
    assert_eq!(
        serde_json::from_value::<PollResponse>(encoded.clone()).unwrap(),
        page
    );
    let mut oversized = encoded;
    oversized["deliveries"] = json!(vec![oversized["deliveries"][0].clone(); 17]);
    assert!(serde_json::from_value::<PollResponse>(oversized).is_err());
    let admission = AdmissionResponse {
        state: PendingState::Pending,
        delivery_attempt_id: receipt_binding(&f).delivery_attempt_id,
        admitted_at: now,
    };
    assert_eq!(
        serde_json::from_value::<AdmissionResponse>(serde_json::to_value(&admission).unwrap())
            .unwrap(),
        admission
    );
    let status = StatusResponse {
        logical_message_id: id(value(&f, "envelope", "logical_message_id")),
        state: MessageState::RecipientStored,
        delivery_attempt_id: Some(receipt_binding(&f).delivery_attempt_id),
        receipt: Some(stored(&f)),
    };
    assert_eq!(
        serde_json::from_value::<StatusResponse>(serde_json::to_value(&status).unwrap()).unwrap(),
        status
    );
    assert!(serde_json::from_str::<AcknowledgeResponse>("{\"recorded\":false}").is_err());
    assert_eq!(
        serde_json::to_string(&AcknowledgeResponse { recorded: Recorded }).unwrap(),
        "{\"recorded\":true}"
    );
    struct FixedClock;
    impl Clock for FixedClock {
        fn unix_seconds(&self) -> Result<u64, crate::ProtocolError> {
            Ok(1790193600)
        }
    }
    let signed = sign_request(
        &sender,
        &FixedClock,
        h.sender_device_id,
        "GET",
        "/node/v1/poll?wait=0&receipts_after=41",
        b"",
    )
    .unwrap();
    assert_eq!(signed.timestamp, 1790193600);
    let input = request_signing_input(
        signed.device,
        signed.timestamp,
        signed.nonce.as_bytes(),
        "GET",
        "/node/v1/poll?wait=0&receipts_after=41",
        b"",
    )
    .unwrap();
    sender
        .public_keys()
        .signing
        .verify(&input, signed.signature.as_bytes())
        .unwrap();
}

#[test]
fn rfc9180_appendix_a2_3_auth_sequence_zero() {
    let f: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/rfc9180-auth-x25519-chacha.json"
    ))
    .unwrap();
    assert_eq!(f["kdf_id"], 1);
    assert_eq!(f["aead_id"], 3);
    assert_eq!(f["sequence"], 0);
    let b = |key: &str| decode_hex(f[key].as_str().unwrap()).unwrap();
    assert_eq!((K::KEM_ID, 2u8), (32, f["mode"].as_u64().unwrap() as u8));
    let (sk_s, pk_s) = K::derive_keypair(&b("ikmS"));
    let (sk_r, pk_r) = K::derive_keypair(&b("ikmR"));
    let (_, pk_e) = K::derive_keypair(&b("ikmE"));
    assert_eq!(pk_s.to_bytes().as_slice(), b("pkSm"));
    assert_eq!(pk_r.to_bytes().as_slice(), b("pkRm"));
    assert_eq!(pk_e.to_bytes().as_slice(), b("pkEm"));
    let ikm: [u8; 32] = b("ikmE").try_into().unwrap();
    let (shared, enc) = K::encap(&pk_r, Some((&sk_s, &pk_s)), &mut TestEphemeral(ikm)).unwrap();
    assert_eq!(shared.0.as_slice(), b("shared_secret"));
    assert_eq!(enc.to_bytes().as_slice(), b("enc"));
    let (enc, mut sender) = hpke::setup_sender::<ChaCha20Poly1305, HkdfSha256, K, _>(
        &OpModeS::Auth((sk_s, pk_s.clone())),
        &pk_r,
        &b("info"),
        &mut TestEphemeral(ikm),
    )
    .unwrap();
    assert_eq!(sender.seal(&b("pt"), &b("aad")).unwrap(), b("ct"));
    let mut recipient = hpke::setup_receiver::<ChaCha20Poly1305, HkdfSha256, K>(
        &OpModeR::Auth(pk_s),
        &sk_r,
        &enc,
        &b("info"),
    )
    .unwrap();
    assert_eq!(recipient.open(&b("ct"), &b("aad")).unwrap(), b("pt"));
    let mut exported = [0; 32];
    sender.export(&[], &mut exported).unwrap();
    assert_eq!(exported.as_slice(), b("exported_empty_32"));
}
#[test]
fn ed25519_signing_matches_ring_for_all_a11_vectors_and_random_seed() {
    use rand_core::RngCore;
    use zeroize::Zeroizing;
    let f = fixture();
    let check = |seed: &[u8; 32], input: &[u8], expected: Option<&[u8]>| {
        let facility = InMemoryKeyFacility::from_test_seeds(&[0x42; 32], seed);
        let public = facility.public_keys().signing;
        let signature = facility.sign(input);
        // Production verification and runtime verification share ring acceptance.
        public.verify(input, &signature).unwrap();
        let ring_key =
            ring::signature::Ed25519KeyPair::from_seed_and_public_key(seed, public.as_bytes())
                .unwrap();
        assert_eq!(ring_key.sign(input).as_ref(), signature.as_slice());
        if let Some(expected) = expected {
            assert_eq!(signature.as_slice(), expected);
        }
    };
    for group in ["device_keys_sender", "device_keys_recipient"] {
        check(
            &fixed::<32>(&f, group, "signing_seed"),
            &bytes(&f, group, "enrol_proof_input"),
            Some(&bytes(&f, group, "enrol_proof")),
        );
    }
    for group in ["receipt_stored", "receipt_quarantined"] {
        check(
            &fixed::<32>(&f, "device_keys_recipient", "signing_seed"),
            &bytes(&f, group, "signing_input"),
            Some(&bytes(&f, group, "signature")),
        );
    }
    check(
        &fixed::<32>(&f, "device_keys_sender", "signing_seed"),
        &bytes(&f, "request_authentication", "signing_input"),
        Some(&bytes(&f, "request_authentication", "signature")),
    );
    let mut seed = Zeroizing::new([0; 32]);
    rand_core::OsRng.try_fill_bytes(seed.as_mut()).unwrap();
    check(
        &seed,
        b"node protocol random-seed signature interoperability",
        None,
    );
}

#[test]
fn a11_seeds_regenerate_from_published_labels() {
    let f = fixture();
    for (label, group, key) in [
        ("sender_kem_label", "device_keys_sender", "kem_ikm"),
        ("recipient_kem_label", "device_keys_recipient", "kem_ikm"),
        ("sender_signing_label", "device_keys_sender", "signing_seed"),
        (
            "recipient_signing_label",
            "device_keys_recipient",
            "signing_seed",
        ),
        ("ephemeral_label", "envelope", "ephemeral_ikm"),
    ] {
        assert_eq!(
            Sha256::digest(value(&f, "seed_derivation", label).as_bytes()).as_slice(),
            bytes(&f, group, key)
        );
    }
    assert_eq!(
        &Sha256::digest(value(&f, "seed_derivation", "nonce_label").as_bytes())[..16],
        bytes(&f, "request_authentication", "nonce")
    );
}
#[test]
fn w0_fixture_matches_authoritative_adr_a11_blocks() {
    let f = fixture();
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/adr/ADR-105-cross-node-comm-transport.md");
    let adr = fs::read_to_string(path).unwrap();
    let a11 = a11_section(&adr);
    assert_eq!(
        hex::encode(Sha256::digest(a11.as_bytes())),
        f["adr_sha256"].as_str().unwrap()
    );
    let groups = [
        "device_keys_sender",
        "device_keys_recipient",
        "envelope",
        "plaintext_json",
        "receipt_binding",
        "receipt_stored",
        "receipt_quarantined",
        "request_authentication",
        "request_body_json",
    ];
    let mut fences = Vec::new();
    let mut lines = a11.split_inclusive('\n');
    while let Some(line) = lines.next() {
        if let Some(language) = line.strip_prefix("```") {
            let language = language.trim_end_matches('\n');
            let mut content = String::new();
            for line in lines.by_ref() {
                if line == "```\n" {
                    break;
                }
                content.push_str(line);
            }
            fences.push((language.to_owned(), content));
        }
    }
    assert_eq!(fences.len(), groups.len());
    assert_eq!(f["blocks"].as_array().unwrap().len(), groups.len());
    assert_eq!(f["values"].as_object().unwrap().len(), groups.len() + 2);
    assert_eq!(f["label"], "test-only fixture");
    let envelope_context = a11.split_once("**Envelope.** Realm `").unwrap().1;
    let (realm, version) = envelope_context.split_once("`, protocol version ").unwrap();
    assert_eq!(value(&f, "envelope_context", "realm"), realm);
    assert_eq!(
        value(&f, "envelope_context", "protocol_version"),
        version.split_once('.').unwrap().0
    );
    for ((language, content), group) in fences.iter().zip(groups) {
        let block = f["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["group"] == group)
            .unwrap();
        assert_eq!(block["language"], *language);
        assert_eq!(block["content"], *content);
        let pairs: Vec<Value> = if language == "text" {
            content
                .lines()
                .map(|line| {
                    let (key, value) = line.split_once('=').unwrap();
                    json!({"key":key.trim(),"value":value.trim()})
                })
                .collect()
        } else {
            vec![json!({"key":"utf8","value":content.trim_end_matches('\n')})]
        };
        assert_eq!(f["values"][group], json!(pairs));
    }
    let table = |heading: &str, first: &str| -> Vec<Value> {
        let section = a11.split_once(heading).unwrap().1;
        section
            .lines()
            .filter(|line| {
                line.starts_with("| ")
                    && !line.starts_with("| input |")
                    && !line.starts_with("| case |")
                    && !line.starts_with("| ---")
            })
            .take_while(|line| !line.is_empty())
            .map(|line| {
                let cells: Vec<&str> = line.split('|').collect();
                json!({(first):cells[1].trim(),"required_outcome":cells[2].trim()})
            })
            .collect()
    };
    // Bound the negative table before the conformance heading, never co-mingle its rows.
    let negative_section = a11
        .split_once("**Negative vectors.**")
        .unwrap()
        .1
        .split_once("**Conformance cases")
        .unwrap()
        .0;
    let negatives: Vec<Value> = negative_section
        .lines()
        .filter(|l| l.starts_with("| ") && !l.starts_with("| input |") && !l.starts_with("| ---"))
        .map(|l| {
            let cells: Vec<&str> = l.split('|').collect();
            json!({"input":cells[1].trim(),"required_outcome":cells[2].trim()})
        })
        .collect();
    assert_eq!(f["negative_vectors"], json!(negatives));
    assert_eq!(negatives.len(), 11);
    let cases = table("**Conformance cases", "case");
    let fixture_cases: Vec<Value> = f["conformance_cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| json!({"case":row["case"],"required_outcome":row["required_outcome"]}))
        .collect();
    assert_eq!(json!(fixture_cases), json!(cases));
    assert_eq!(cases.len(), 52);
    for key in [
        "sender_kem_label",
        "recipient_kem_label",
        "sender_signing_label",
        "recipient_signing_label",
        "ephemeral_label",
        "nonce_label",
        "seed_rule",
        "nonce_rule",
    ] {
        assert!(a11.contains(value(&f, "seed_derivation", key)));
    }
}

fn a11_section(adr: &str) -> &str {
    adr.split_once("### A.11 Test vectors\n")
        .expect("fixed A.11 heading")
        .1
        .split_once("### A.12 What this appendix does not change\n")
        .expect("fixed A.12 heading")
        .0
}

#[test]
fn a11_pin_ignores_changes_outside_fixed_section() {
    let f = fixture();
    let adr = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/adr/ADR-105-cross-node-comm-transport.md"),
    )
    .unwrap();
    let changed = format!("Unrelated documentation metadata.\n\n{adr}");
    assert_ne!(
        Sha256::digest(adr.as_bytes()),
        Sha256::digest(changed.as_bytes())
    );
    assert_eq!(a11_section(&adr), a11_section(&changed));
    assert_eq!(
        hex::encode(Sha256::digest(a11_section(&changed).as_bytes())),
        f["adr_sha256"].as_str().unwrap()
    );
}

#[test]
fn delivery_unknown_member_is_refused_after_valid_parse() {
    let f = fixture();
    let h = header(&f);
    let mut delivery = json!({
        "delivery_attempt_id": receipt_binding(&f).delivery_attempt_id,
        "logical_message_id": value(&f, "envelope", "logical_message_id"),
        "protocol_version": 1,
        "sender_agent_id": h.sender_agent_id,
        "sender_device_id": h.sender_device_id,
        "sender_key_epoch": h.sender_key_epoch,
        "recipient_agent_id": h.recipient_agent_id,
        "recipient_device_id": h.recipient_device_id,
        "recipient_key_epoch": h.recipient_key_epoch,
        "contact_generation": 3,
        "enc": sealed(&f).enc,
        "ciphertext": sealed(&f).ciphertext,
    });
    let parsed: Delivery = serde_json::from_value(delivery.clone()).unwrap();
    assert_eq!(
        parsed.delivery_attempt_id,
        receipt_binding(&f).delivery_attempt_id
    );
    delivery["unexpected_member"] = json!(true);
    assert!(serde_json::from_value::<Delivery>(delivery).is_err());
}

#[test]
fn contact_foreign_realm_is_refused_after_valid_key_pin() {
    let f = fixture();
    let h = header(&f);
    let keys = facility(&f, "device_keys_recipient").public_keys();
    let contact = ContactResponse {
        agent_id: h.recipient_agent_id,
        address: NodeAddress::new(h.realm.clone(), h.recipient_agent_id),
        device_id: h.recipient_device_id,
        key_epoch: h.recipient_key_epoch,
        kem_public_key: keys.kem.clone(),
        signing_public_key: keys.signing.clone(),
        fingerprint: keys.fingerprint(),
        contact_generation: Epoch::new(3).unwrap(),
    };
    assert_eq!(contact.validated_keys(&h.realm).unwrap(), keys);
    assert_eq!(
        contact.validated_keys(&Realm::parse("foreign.example").unwrap()),
        Err(crate::ProtocolError::InvalidEncoding)
    );
}
