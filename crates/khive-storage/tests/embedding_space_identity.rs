use khive_storage::{EmbeddingSpaceIdentity, EmbeddingSpaceIdentityError};

const FINGERPRINT: [u8; 32] = [0xab; 32];

#[test]
fn derives_the_complete_physical_key_and_retains_identity_fields() {
    let identity = EmbeddingSpaceIdentity::new(
        "moodboard",
        "moodboard.visual-descriptor.v1",
        FINGERPRINT,
        "qwen3.5-vlm-pooled-visual",
        1024,
    )
    .expect("valid identity");

    assert_eq!(
        identity.space_key().as_str(),
        "moodboard_abababababababababababababababababababababababababababababababab_1024"
    );
    assert_eq!(
        identity.protocol().as_str(),
        "moodboard.visual-descriptor.v1"
    );
    assert_eq!(identity.fingerprint(), &FINGERPRINT);
    assert_eq!(identity.model_name(), "qwen3.5-vlm-pooled-visual");
    assert_eq!(identity.dimensions().get(), 1024);
}

#[test]
fn validates_every_caller_owned_field_and_the_derived_key_bound() {
    let valid = |prefix: String, protocol: String, model: String, dimensions| {
        EmbeddingSpaceIdentity::new(&prefix, &protocol, FINGERPRINT, &model, dimensions)
    };

    assert!(valid("p".repeat(58), "p".repeat(128), "m".repeat(512), 8192).is_ok());
    assert!(valid(
        "space".to_string(),
        "owner.v1".to_string(),
        "model".to_string(),
        1
    )
    .is_ok());

    for bad_prefix in [String::new(), "bad-key".to_string(), "p".repeat(59)] {
        assert!(valid(
            bad_prefix,
            "owner.v1".to_string(),
            "model".to_string(),
            8192
        )
        .is_err());
    }

    for bad_protocol in [String::new(), "p".repeat(129), "owner/v1".to_string()] {
        assert!(valid("space".to_string(), bad_protocol, "model".to_string(), 4).is_err());
    }

    for bad_model in [String::new(), " model".to_string(), "m".repeat(513)] {
        assert!(valid("space".to_string(), "owner.v1".to_string(), bad_model, 4).is_err());
    }

    for bad_dimensions in [0, 8193] {
        assert!(valid(
            "space".to_string(),
            "owner.v1".to_string(),
            "model".to_string(),
            bad_dimensions
        )
        .is_err());
    }
}

#[test]
fn validation_failures_have_stable_typed_categories() {
    let construct = |prefix: &str, protocol: &str, model: &str, dimensions| {
        EmbeddingSpaceIdentity::new(prefix, protocol, FINGERPRINT, model, dimensions)
    };

    assert_eq!(
        construct("bad-key", "owner.v1", "model", 4),
        Err(EmbeddingSpaceIdentityError::InvalidKeyPrefix)
    );
    assert_eq!(
        construct("space", "owner/v1", "model", 4),
        Err(EmbeddingSpaceIdentityError::InvalidProtocol)
    );
    assert_eq!(
        construct("space", "owner.v1", " model", 4),
        Err(EmbeddingSpaceIdentityError::InvalidModelName)
    );
    assert_eq!(
        construct("space", "owner.v1", "model", 0),
        Err(EmbeddingSpaceIdentityError::InvalidDimensions { dimensions: 0 })
    );
    assert_eq!(
        construct(&"p".repeat(59), "owner.v1", "model", 8192),
        Err(EmbeddingSpaceIdentityError::DerivedKeyTooLong {
            actual_bytes: 129,
            max_bytes: 128,
        })
    );

    let oversized_borrowed_prefix = "p".repeat(1024 * 1024);
    assert_eq!(
        construct(&oversized_borrowed_prefix, "owner.v1", "model", 4),
        Err(EmbeddingSpaceIdentityError::DerivedKeyTooLong {
            actual_bytes: 1024 * 1024 + 67,
            max_bytes: 128,
        })
    );
}
