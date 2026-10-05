use super::*;
use crate::credentials::{
    CredentialCacheLifetime, CredentialConfig, CredentialError, CredentialMaterial,
    CredentialProvider, VisibilityReceiptKeyConfig,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

struct TestProvider {
    keys: BTreeMap<String, String>,
    lookups: Arc<AtomicUsize>,
}

impl CredentialProvider for TestProvider {
    fn resolve(&self, name: &str) -> Result<CredentialMaterial, CredentialError> {
        self.lookups.fetch_add(1, Ordering::Relaxed);
        self.keys
            .get(name)
            .map(|value| CredentialMaterial::new(value.as_bytes().to_vec()))
            .ok_or_else(|| CredentialError::Unavailable {
                name: name.to_owned(),
            })
    }
    fn cache_lifetime(&self) -> CredentialCacheLifetime {
        CredentialCacheLifetime::NoCache
    }
}

fn sealer(encrypt: &str) -> (ReceiptSealer, Arc<AtomicUsize>) {
    sealer_with_keys(
        encrypt,
        [
            ("old".to_owned(), URL_SAFE_NO_PAD.encode([7_u8; 32])),
            ("new".to_owned(), URL_SAFE_NO_PAD.encode([19_u8; 32])),
        ]
        .into_iter()
        .collect(),
    )
}

fn sealer_with_keys(
    encrypt: &str,
    keys: BTreeMap<String, String>,
) -> (ReceiptSealer, Arc<AtomicUsize>) {
    let declarations = ["old", "new"]
        .into_iter()
        .map(|id| CredentialConfig {
            name: id.to_owned(),
            kind: CredentialKind::SigningKey,
            provider: "test".to_owned(),
            env_var: None,
            header: None,
        })
        .collect();
    let lookups = Arc::new(AtomicUsize::new(0));
    let mut credentials = CredentialRegistry::new(declarations).unwrap();
    credentials
        .register_provider(
            "test".to_owned(),
            Arc::new(TestProvider {
                keys,
                lookups: Arc::clone(&lookups),
            }),
        )
        .unwrap();
    let ring = VisibilityReceiptConfig {
        keys: ["old", "new"]
            .into_iter()
            .map(|id| VisibilityReceiptKeyConfig {
                id: id.to_owned(),
                credential: id.to_owned(),
                encrypt: id == encrypt,
            })
            .collect(),
    };
    (
        ReceiptSealer::new(ring, Arc::new(credentials)).unwrap(),
        lookups,
    )
}

#[test]
fn round_trip_stamps_server_time_and_sorts_fences() {
    let (sealer, _) = sealer("new");
    let before = chrono::Utc::now().timestamp_millis();
    let token = sealer
        .seal("lambda:private", &[("z".into(), 900), ("a".into(), 1)])
        .unwrap();
    let receipt = sealer.open(&token).unwrap();
    let after = chrono::Utc::now().timestamp_millis();
    assert_eq!(receipt.namespace, "lambda:private");
    assert_eq!(receipt.fences, [("a".to_owned(), 1), ("z".to_owned(), 900)]);
    assert!((before..=after).contains(&receipt.issued_at));
    assert!(!token.contains("lambda:private"));
    let empty = sealer.open(&sealer.seal("local", &[]).unwrap()).unwrap();
    assert!(empty.fences.is_empty());
}

#[test]
fn rotation_keeps_old_key_decrypt_only() {
    let (old, _) = sealer("old");
    let (new, _) = sealer("new");
    let old_token = old.seal("local", &[("model".into(), 17)]).unwrap();
    let recovered = new.open(&old_token).unwrap();
    assert_eq!(recovered.fences, [("model".to_owned(), 17)]);
    let new_token = new.seal("local", &[]).unwrap();
    let envelope = URL_SAFE_NO_PAD.decode(new_token).unwrap();
    assert_eq!(&envelope[2..5], b"new");
}

#[test]
fn unknown_or_unavailable_key_is_typed_and_never_replaced() {
    let (valid, _) = sealer("new");
    let token = valid
        .seal("hidden-namespace", &[("hidden-model".into(), 834_827)])
        .unwrap();
    let mut envelope = URL_SAFE_NO_PAD.decode(&token).unwrap();
    envelope[2..5].copy_from_slice(b"who");
    let error = valid.open(&URL_SAFE_NO_PAD.encode(envelope)).err().unwrap();
    assert_eq!(error, ReceiptSealError::KeyUnavailable);
    let (missing, _) = sealer_with_keys("new", BTreeMap::new());
    assert_eq!(
        missing.open(&token).err(),
        Some(ReceiptSealError::KeyUnavailable)
    );
    assert_eq!(
        missing.seal("local", &[]),
        Err(ReceiptSealError::KeyUnavailable)
    );
    let diagnostic = format!("{error} {error:?}");
    for value in ["hidden-namespace", "hidden-model", "834827", &token] {
        assert!(!diagnostic.contains(value));
    }
}

#[test]
fn ciphertext_flip_and_appended_byte_fail_authentication() {
    let (sealer, _) = sealer("new");
    let token = sealer.seal("local", &[("model".into(), 1)]).unwrap();
    let envelope = URL_SAFE_NO_PAD.decode(token).unwrap();
    let mut flipped = envelope.clone();
    flipped[2 + 3 + NONCE_BYTES] ^= 0x80;
    let mut appended = envelope;
    appended.push(0);
    for invalid in [flipped, appended] {
        assert_eq!(
            sealer.open(&URL_SAFE_NO_PAD.encode(invalid)).err(),
            Some(ReceiptSealError::InvalidReceipt)
        );
    }
}

#[test]
fn repeated_seals_use_distinct_os_nonces() {
    let (sealer, _) = sealer("new");
    let first = URL_SAFE_NO_PAD
        .decode(sealer.seal("local", &[]).unwrap())
        .unwrap();
    let second = URL_SAFE_NO_PAD
        .decode(sealer.seal("local", &[]).unwrap())
        .unwrap();
    assert_ne!(&first[5..29], &second[5..29]);
}

#[test]
fn nonce_failure_refuses_sealing_without_fallback() {
    let (sealer, _) = sealer("new");
    assert_eq!(
        sealer.seal_with_nonce_source("local", &[], |_| Err(ReceiptSealError::NonceUnavailable)),
        Err(ReceiptSealError::NonceUnavailable)
    );
}

#[test]
fn associated_data_matches_independent_byte_vector() {
    // The expected bytes are independent of PURPOSE, VERSION and envelope encoding.
    assert_eq!(
        associated_data("key-7"),
        [
            0x6b, 0x68, 0x69, 0x76, 0x65, 0x2e, 0x6d, 0x65, 0x6d, 0x6f, 0x72, 0x79, 0x2e, 0x76,
            0x69, 0x73, 0x69, 0x62, 0x69, 0x6c, 0x69, 0x74, 0x79, 0x02, 0x05, 0x6b, 0x65, 0x79,
            0x2d, 0x37,
        ]
    );
}

fn authenticated_token(plaintext: &[u8], aad: &[u8]) -> String {
    let cipher = XChaCha20Poly1305::new_from_slice(&[19_u8; 32]).unwrap();
    // Test-only synthetic key/nonce. Production always obtains a fresh OS nonce.
    let nonce = [31_u8; 24];
    let mut body = plaintext.to_vec();
    let tag = cipher
        .encrypt_in_place_detached(XNonce::from_slice(&nonce), aad, &mut body)
        .unwrap();
    let mut envelope = vec![2, 3, b'n', b'e', b'w'];
    envelope.extend_from_slice(&nonce);
    envelope.extend_from_slice(&body);
    envelope.extend_from_slice(&tag);
    URL_SAFE_NO_PAD.encode(envelope)
}

#[test]
fn open_authenticates_the_purpose_and_header_binding() {
    let (sealer, _) = sealer("new");
    let plain = encode_plaintext("local", 1, &[], 3).unwrap();
    for aad in [
        b"".as_slice(),
        b"khive.memory.visibility\x02\x03old",
        b"other.memory.visibility\x02\x03new",
    ] {
        let token = authenticated_token(&plain, aad);
        assert_eq!(
            sealer.open(&token).err(),
            Some(ReceiptSealError::InvalidReceipt)
        );
    }
}

#[test]
fn structural_refusals_happen_before_provider_lookup() {
    let (sealer, lookups) = sealer("new");
    let token = sealer.seal("local", &[]).unwrap();
    let mut wrong_version = URL_SAFE_NO_PAD.decode(&token).unwrap();
    wrong_version[0] = 1;
    let mut invalid_id = wrong_version.clone();
    invalid_id[0] = 2;
    invalid_id[2] = b'/';
    let mut too_large = vec![0; MAX_ENVELOPE_BYTES + 1];
    too_large[..5].copy_from_slice(&[2, 3, b'n', b'e', b'w']);
    let count = lookups.load(Ordering::Relaxed);
    for malformed in [
        String::new(),
        "AgA".into(),
        format!("{token}="),
        format!(" {token}"),
        URL_SAFE_NO_PAD.encode(wrong_version),
        URL_SAFE_NO_PAD.encode(invalid_id),
        URL_SAFE_NO_PAD.encode([2, 65]),
        URL_SAFE_NO_PAD.encode(too_large),
        "A".repeat(MAX_ENCODED_BYTES + 1),
    ] {
        assert_eq!(
            sealer.open(&malformed).err(),
            Some(ReceiptSealError::InvalidReceipt)
        );
    }
    assert_eq!(lookups.load(Ordering::Relaxed), count);
}

#[test]
fn key_material_requires_canonical_encoding_and_exact_size() {
    for key in [
        "not-a-key".to_owned(),
        URL_SAFE_NO_PAD.encode([7_u8; 31]),
        URL_SAFE_NO_PAD.encode([7_u8; 33]),
        format!("{}=", URL_SAFE_NO_PAD.encode([7_u8; 32])),
    ] {
        let (sealer, _) =
            sealer_with_keys("new", [("new".into(), key.clone())].into_iter().collect());
        let error = sealer.seal("local", &[]).unwrap_err();
        assert_eq!(error, ReceiptSealError::KeyUnavailable);
        assert!(!format!("{error} {error:?}").contains(&key));
    }
}

#[test]
fn plaintext_layout_is_fixed_width_and_strictly_decoded() {
    let bytes = encode_plaintext("ns", 1, &[("x".into(), 9)], 3).unwrap();
    assert_eq!(
        &*bytes,
        &[0, 2, b'n', b's', 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 1, b'x', 0, 0, 0, 0, 0, 0, 0, 9,]
    );
    let other = encode_plaintext("ns", 1, &[("x".into(), u64::MAX)], 3).unwrap();
    assert_eq!(bytes.len(), other.len());
    for cut in 0..bytes.len() {
        assert_eq!(
            decode_plaintext(&bytes[..cut]).err(),
            Some(ReceiptSealError::InvalidReceipt)
        );
    }
    let mut trailing = bytes.to_vec();
    trailing.push(0);
    let mut zero_sequence = bytes.to_vec();
    *zero_sequence.last_mut().unwrap() = 0;
    let mut invalid_utf8 = bytes.to_vec();
    invalid_utf8[2] = 0xff;
    let (sealer, _) = sealer("new");
    for invalid in [trailing, zero_sequence, invalid_utf8] {
        // Even valid authentication never admits invalid plaintext structure.
        let token = authenticated_token(&invalid, b"khive.memory.visibility\x02\x03new");
        assert_eq!(
            sealer.open(&token).err(),
            Some(ReceiptSealError::InvalidReceipt)
        );
    }
}

#[test]
fn fences_refuse_duplicates_zero_sequences_and_received_disorder() {
    for fences in [
        vec![("a".into(), 1), ("a".into(), 2)],
        vec![("a".into(), 0)],
    ] {
        assert!(matches!(
            encode_plaintext("ns", 1, &fences, 3),
            Err(ReceiptSealError::InvalidReceipt)
        ));
    }
    let mut bytes = encode_plaintext("ns", 1, &[("a".into(), 1), ("z".into(), 2)], 3).unwrap();
    bytes[16] = b'z';
    bytes[27] = b'a';
    assert_eq!(
        decode_plaintext(&bytes).err(),
        Some(ReceiptSealError::InvalidReceipt)
    );
    bytes[27] = b'z';
    assert_eq!(
        decode_plaintext(&bytes).err(),
        Some(ReceiptSealError::InvalidReceipt)
    );
}

#[test]
fn envelope_ceiling_is_enforced_before_large_allocation() {
    let (sealer, _) = sealer("new");
    // 54 + key-id(3) + namespace(2) + model framing(10) + model = 65,536.
    let model = "m".repeat(65_467);
    let token = sealer.seal("ns", &[(model.clone(), 1)]).unwrap();
    assert_eq!(
        URL_SAFE_NO_PAD.decode(&token).unwrap().len(),
        MAX_ENVELOPE_BYTES
    );
    assert_eq!(token.len(), MAX_ENCODED_BYTES);
    assert_eq!(sealer.open(&token).unwrap().fences[0].0, model);
    assert_eq!(
        sealer.seal("ns", &[(format!("{model}m"), 1)]),
        Err(ReceiptSealError::InvalidReceipt)
    );
}
