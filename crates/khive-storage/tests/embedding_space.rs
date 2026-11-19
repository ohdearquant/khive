use khive_storage::{EmbeddingSpaceIdentity, EmbeddingSpaceIdentityError};

fn identity(
    prefix: &str,
    protocol: &str,
    model: &str,
    dimensions: u32,
) -> Result<EmbeddingSpaceIdentity, EmbeddingSpaceIdentityError> {
    EmbeddingSpaceIdentity::new(prefix, protocol, [0; 32], model, dimensions)
}

#[test]
fn derives_the_existing_moodboard_golden_without_rehashing() {
    // Existing cross-language descriptor golden in moodboard/src/model.rs.
    // This storage test consumes its digest, not its model/canonicalization code.
    let fingerprint = [
        0xb5, 0x7f, 0xb3, 0xcf, 0x43, 0xda, 0x38, 0x7c, 0xde, 0x12, 0x42, 0x5e, 0x6d, 0x7d, 0x44,
        0x2a, 0xf2, 0x69, 0xba, 0x37, 0xec, 0xab, 0xfb, 0xe4, 0xc9, 0x75, 0xcb, 0x80, 0xab, 0xdf,
        0x56, 0xe5,
    ];
    let space = EmbeddingSpaceIdentity::new(
        "moodboard",
        "moodboard.visual-descriptor.v1",
        fingerprint,
        "model",
        4,
    )
    .unwrap();
    let expected = "moodboard_b57fb3cf43da387cde12425e6d7d442af269ba37ecabfbe4c975cb80abdf56e5_4";
    assert_eq!(space.space_key().as_str(), expected);
    assert_eq!(space.space_key().to_string(), expected);
    assert_eq!(AsRef::<str>::as_ref(space.space_key()), expected);
    assert_eq!(space.protocol(), "moodboard.visual-descriptor.v1");
    assert_eq!(space.fingerprint(), &fingerprint);
    assert_eq!(space.model_name(), "model");
    assert_eq!(space.dimensions().get(), 4);
}

#[test]
fn rejects_invalid_prefix_and_protocol_without_normalization() {
    for prefix in ["", "with-dash", "with.dot", " space", "é", "a\0b"] {
        assert_eq!(
            identity(prefix, "owner.v1", "model", 1).unwrap_err(),
            EmbeddingSpaceIdentityError::InvalidKeyPrefix
        );
    }
    for protocol in ["", "owner/v1", " owner.v1", "owner.v1\n", "é", "a\0b"] {
        assert_eq!(
            identity("p", protocol, "model", 1).unwrap_err(),
            EmbeddingSpaceIdentityError::InvalidProtocol
        );
    }
    assert_eq!(
        identity("p", &"p".repeat(129), "model", 1).unwrap_err(),
        EmbeddingSpaceIdentityError::InvalidProtocol
    );
    for protocol in ["P", "Owner-name.v1_2", &"p".repeat(128)] {
        assert_eq!(
            identity("A_z09", protocol, "model", 1).unwrap().protocol(),
            protocol
        );
    }
}

#[test]
fn model_label_bounds_are_bytes_and_retain_existing_whitespace_policy() {
    for model in ["", " ", " model", "model\n", "\u{2003}model"] {
        assert_eq!(
            identity("p", "p.v1", model, 1).unwrap_err(),
            EmbeddingSpaceIdentityError::InvalidModelName
        );
    }
    for model in ["x".repeat(513), "é".repeat(257)] {
        assert_eq!(
            identity("p", "p.v1", &model, 1).unwrap_err(),
            EmbeddingSpaceIdentityError::InvalidModelName
        );
    }
    for model in ["x".repeat(512), "é".repeat(256), "two words".into()] {
        assert_eq!(
            identity("p", "p.v1", &model, 1).unwrap().model_name(),
            model
        );
    }
}

#[test]
fn validates_dimensions_and_exact_derived_key_bound() {
    for dimensions in [0, 8193, u32::MAX] {
        assert_eq!(
            identity("p", "p.v1", "m", dimensions).unwrap_err(),
            EmbeddingSpaceIdentityError::InvalidDimensions { dimensions }
        );
    }
    for (dimensions, prefix_len) in [(1, 61), (8192, 58)] {
        let space = identity(&"p".repeat(prefix_len), "p.v1", "m", dimensions).unwrap();
        assert_eq!(space.space_key().as_str().len(), 128);
        assert_eq!(space.dimensions().get(), dimensions);
        assert_eq!(
            identity(&"p".repeat(prefix_len + 1), "p.v1", "m", dimensions).unwrap_err(),
            EmbeddingSpaceIdentityError::SpaceKeyTooLong { bytes: 129 }
        );
    }
}

#[test]
fn protocol_owner_governs_the_fingerprint_preimage() {
    // A fixture protocol explicitly fingerprints its version and model bytes.
    // The shared identity receives the resulting bytes; it does not hash again.
    let v1 = *blake3::hash(b"example.embedding.v1\0model=fixture").as_bytes();
    let v2 = *blake3::hash(b"example.embedding.v2\0model=fixture").as_bytes();
    let one =
        EmbeddingSpaceIdentity::new("example", "example.embedding.v1", v1, "fixture", 4).unwrap();
    let two =
        EmbeddingSpaceIdentity::new("example", "example.embedding.v2", v2, "fixture", 4).unwrap();
    assert_ne!(one.fingerprint(), two.fingerprint());
    assert_ne!(one.space_key(), two.space_key());

    // A caller that violates its protocol by reusing the digest is not silently
    // repaired by inventing a new hash/key scheme. Such descriptors remain
    // unequal even though the caller supplied the same physical fence.
    let reused =
        EmbeddingSpaceIdentity::new("example", "example.embedding.v2", v1, "fixture", 4).unwrap();
    assert_eq!(one.space_key(), reused.space_key());
    assert_ne!(one, reused);
}

#[test]
fn fingerprint_dimensions_and_prefix_choose_the_space_not_the_label() {
    let original = identity("p", "p.v1", "first label", 4).unwrap();
    let relabelled = identity("p", "p.v1", "second label", 4).unwrap();
    assert_eq!(original.space_key(), relabelled.space_key());
    assert_ne!(original, relabelled);
    let fingerprint_changed =
        EmbeddingSpaceIdentity::new("p", "p.v1", [0xff; 32], "first label", 4).unwrap();
    assert_ne!(original.space_key(), fingerprint_changed.space_key());
    assert_ne!(
        original.space_key(),
        identity("p", "p.v1", "first label", 5).unwrap().space_key()
    );
    assert_ne!(
        original.space_key(),
        identity("q", "p.v1", "first label", 4).unwrap().space_key()
    );
}
