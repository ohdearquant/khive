//! Tests for KhiveError — structured cross-crate error model.
//!
//! Covers: Display, Error trait, serde wire shape stability, RetryHint,
//! Details, and the ErrorCode domain-scoped code model.

use khive_types::khive_error::{
    Details, ErrorCode, ErrorDomain, ErrorKind, KhiveError, RetryHint, DETAILS_TRUNCATED_KEY,
};

// ---- ErrorKind Display ----

#[test]
fn error_kind_display_not_found() {
    assert_eq!(ErrorKind::NotFound.to_string(), "not_found");
}

#[test]
fn error_kind_display_invalid_input() {
    assert_eq!(ErrorKind::InvalidInput.to_string(), "invalid_input");
}

#[test]
fn error_kind_display_conflict() {
    assert_eq!(ErrorKind::Conflict.to_string(), "conflict");
}

#[test]
fn error_kind_display_internal() {
    assert_eq!(ErrorKind::Internal.to_string(), "internal");
}

#[test]
fn error_kind_display_unauthorized() {
    assert_eq!(ErrorKind::Unauthorized.to_string(), "unauthorized");
}

#[test]
fn error_kind_display_unavailable() {
    assert_eq!(ErrorKind::Unavailable.to_string(), "unavailable");
}

// ---- ErrorDomain Display ----

#[test]
fn error_domain_display() {
    assert_eq!(ErrorDomain::Db.to_string(), "db");
    assert_eq!(ErrorDomain::Query.to_string(), "query");
    assert_eq!(ErrorDomain::Runtime.to_string(), "runtime");
    assert_eq!(ErrorDomain::Types.to_string(), "types");
}

// ---- ErrorCode ----

#[test]
fn error_code_numeric() {
    let code = ErrorCode::new(ErrorDomain::Db, 1);
    assert_eq!(code.domain(), ErrorDomain::Db);
    assert_eq!(code.code(), 1);
}

#[test]
fn error_code_display() {
    let code = ErrorCode::new(ErrorDomain::Query, 42);
    assert_eq!(code.to_string(), "query:42");
}

// ---- KhiveError constructors + Display ----

#[test]
fn khive_error_not_found_display() {
    let e = KhiveError::not_found("entity", "abc123");
    assert!(e.to_string().contains("not found"));
    assert!(e.to_string().contains("entity"));
    assert!(e.to_string().contains("abc123"));
}

#[test]
fn khive_error_invalid_input_display() {
    let e = KhiveError::invalid_input("name is required");
    assert!(e.to_string().contains("invalid input"));
    assert!(e.to_string().contains("name is required"));
}

#[test]
fn khive_error_conflict_display() {
    let e = KhiveError::conflict("duplicate key");
    assert!(e.to_string().contains("conflict"));
    assert!(e.to_string().contains("duplicate key"));
}

#[test]
fn khive_error_internal_display() {
    let e = KhiveError::internal("unexpected state");
    assert!(e.to_string().contains("internal"));
    assert!(e.to_string().contains("unexpected state"));
}

#[test]
fn khive_error_kind_accessor() {
    assert_eq!(KhiveError::not_found("x", "y").kind(), ErrorKind::NotFound);
    assert_eq!(
        KhiveError::invalid_input("z").kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(KhiveError::conflict("c").kind(), ErrorKind::Conflict);
    assert_eq!(KhiveError::internal("i").kind(), ErrorKind::Internal);
}

// ---- std::error::Error trait ----

#[cfg(feature = "std")]
#[test]
fn khive_error_implements_std_error() {
    // Compile-time proof: the trait is implemented when "std" feature is active.
    fn accepts_std_error(_: &dyn std::error::Error) {}
    let e = KhiveError::not_found("record", "id-1");
    accepts_std_error(&e);
}

// ---- RetryHint ----

#[test]
fn retry_hint_non_retryable_by_default_for_not_found() {
    let e = KhiveError::not_found("x", "y");
    assert_eq!(e.retry_hint(), RetryHint::NoRetry);
}

#[test]
fn retry_hint_retryable_for_unavailable() {
    let e = KhiveError::unavailable("db not ready");
    assert_eq!(e.retry_hint(), RetryHint::Retryable);
}

// ---- Details ----

#[test]
fn details_empty_by_default() {
    let e = KhiveError::not_found("entity", "x");
    assert!(e.details().is_none());
}

#[test]
fn details_roundtrip() {
    let details = Details::new([("field", "name"), ("constraint", "required")]);
    let e = KhiveError::invalid_input("missing field").with_details(details.clone());
    let got = e.details().unwrap();
    assert_eq!(got.get("field"), Some("name"));
    assert_eq!(got.get("constraint"), Some("required"));
}

// ---- Serde wire shape stability ----

#[cfg(feature = "serde")]
mod serde_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn error_kind_serializes_as_snake_case_string() {
        let k = ErrorKind::NotFound;
        let v = serde_json::to_value(k).unwrap();
        assert_eq!(v, json!("not_found"));
    }

    #[test]
    fn error_domain_serializes_as_lowercase() {
        let d = ErrorDomain::Db;
        let v = serde_json::to_value(d).unwrap();
        assert_eq!(v, json!("db"));
    }

    #[test]
    fn khive_error_wire_shape_not_found() {
        let e = KhiveError::not_found("entity", "abc");
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], json!("not_found"));
        assert!(v["message"].as_str().is_some());
        // code field should be present (may be null if not set)
        assert!(v.get("code").is_some());
        // details field should be present
        assert!(v.get("details").is_some());
    }

    #[test]
    fn khive_error_wire_shape_with_code() {
        let e = KhiveError::not_found("entity", "abc")
            .with_code(ErrorCode::new(ErrorDomain::Runtime, 10));
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["code"], json!("runtime:10"));
    }

    #[test]
    fn khive_error_roundtrip_not_found() {
        let original = KhiveError::not_found("entity", "abc-123");
        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: KhiveError = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.kind(), ErrorKind::NotFound);
        assert_eq!(deserialized.message(), original.message());
    }

    #[test]
    fn khive_error_roundtrip_invalid_input() {
        let original = KhiveError::invalid_input("name is required");
        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: KhiveError = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.kind(), ErrorKind::InvalidInput);
        assert_eq!(deserialized.message(), original.message());
    }

    #[test]
    fn khive_error_roundtrip_conflict() {
        let original = KhiveError::conflict("duplicate");
        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: KhiveError = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.kind(), ErrorKind::Conflict);
    }

    #[test]
    fn khive_error_roundtrip_internal() {
        let original = KhiveError::internal("oops");
        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: KhiveError = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.kind(), ErrorKind::Internal);
    }

    #[test]
    fn khive_error_roundtrip_unavailable() {
        let original = KhiveError::unavailable("db down");
        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: KhiveError = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.kind(), ErrorKind::Unavailable);
        assert_eq!(deserialized.retry_hint(), RetryHint::Retryable);
    }

    #[test]
    fn details_roundtrip_serde() {
        let details = Details::new([("resource", "entity"), ("id", "x1")]);
        let original = KhiveError::not_found("entity", "x1").with_details(details);
        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: KhiveError = serde_json::from_str(&serialized).unwrap();
        let got = deserialized.details().unwrap();
        assert_eq!(got.get("resource"), Some("entity"));
        assert_eq!(got.get("id"), Some("x1"));
    }

    /// Regression for #487: a details map with 9-10 entries must deserialize
    /// successfully (the visitor must keep draining MapAccess until None, not
    /// stop reading once 8 entries have been retained).
    ///
    /// Follow-up (PR #549 review): truncation must be observable, not silent.
    /// The bounded wire shape stays at 8 entries, but the 8th slot is now the
    /// `details_truncated` indicator carrying the dropped-pair count, so only
    /// the first 7 insertion-order client pairs are retained verbatim.
    #[test]
    fn details_drains_oversized_map_retains_8() {
        let json = serde_json::json!({
            "k0": "v0", "k1": "v1", "k2": "v2", "k3": "v3",
            "k4": "v4", "k5": "v5", "k6": "v6", "k7": "v7",
            "k8": "v8", "k9": "v9"
        })
        .to_string();
        let details: Details =
            serde_json::from_str(&json).expect("oversized map must deserialize successfully");
        let pairs: Vec<(&str, &str)> = details.iter().collect();
        assert_eq!(
            pairs.len(),
            8,
            "must retain exactly 8 pairs (7 client + indicator)"
        );
        for i in 0..7 {
            let key = format!("k{i}");
            assert_eq!(
                details.get(&key),
                Some(format!("v{i}").as_str()),
                "entry {key} must be one of the first 7 insertion-order pairs"
            );
        }
        assert_eq!(
            details.dropped_count(),
            Some(3),
            "10 supplied pairs - 7 retained = 3 dropped, must be observable"
        );
    }

    /// Nine-pair variant embedded inside a full `KhiveError` envelope, proving
    /// the outer struct also deserializes successfully when details overflow.
    #[test]
    fn khive_error_with_nine_details_pairs_deserializes() {
        let json = serde_json::json!({
            "kind": "not_found",
            "message": "missing",
            "code": null,
            "details": {
                "a": "1", "b": "2", "c": "3", "d": "4",
                "e": "5", "f": "6", "g": "7", "h": "8", "i": "9"
            }
        })
        .to_string();
        let deserialized: KhiveError =
            serde_json::from_str(&json).expect("envelope with 9 details pairs must deserialize");
        let got = deserialized.details().unwrap();
        assert_eq!(
            got.iter().count(),
            8,
            "must retain exactly 8 pairs (7 client + truncation indicator)"
        );
        assert_eq!(
            got.dropped_count(),
            Some(2),
            "9 supplied pairs - 7 retained = 2 dropped, must be observable"
        );
    }

    /// PR #549: a client-supplied `details_truncated`
    /// pair must never be retained as an ordinary entry, even when the total
    /// pair count is within the 8-entry bound. Retaining it verbatim let a
    /// client-controlled value flow straight into `dropped_count()` via
    /// first-match `get()` and falsely report truncation that never
    /// happened. The fix reserves the key unconditionally: the colliding
    /// pair is dropped and counted, never stored and never trusted as-is.
    #[test]
    fn details_client_collision_within_bound_is_dropped_not_trusted() {
        let details = Details::new([("a", "1"), (DETAILS_TRUNCATED_KEY, "not_a_real_count")]);
        // The collision is stripped, not stored verbatim — the client's
        // bogus value never appears as the indicator's value.
        assert_eq!(details.get(DETAILS_TRUNCATED_KEY), Some("1"));
        assert_eq!(details.get("a"), Some("1"));
        // Exactly one pair was dropped (the collision) — a true report,
        // not a spoofed one derived from the client's supplied value.
        assert_eq!(details.dropped_count(), Some(1));
        assert_eq!(details.iter().count(), 2);
    }

    /// Companion to the above: a `details_truncated` collision supplied
    /// alongside more than 8 ordinary pairs must not shadow the real
    /// indicator (first-match `get()` previously could return the client's
    /// pair instead of ours) and must not serialize as a duplicate JSON key.
    #[test]
    fn details_client_collision_with_overflow_not_shadowed_no_duplicate_keys() {
        let details = Details::new([
            ("k0", "v0"),
            ("k1", "v1"),
            ("k2", "v2"),
            ("k3", "v3"),
            ("k4", "v4"),
            ("k5", "v5"),
            ("k6", "v6"),
            (DETAILS_TRUNCATED_KEY, "not_a_real_count"),
            ("k7", "v7"),
        ]);
        // 8 ordinary pairs (k0..k7) + 1 reserved-key collision supplied by
        // the client = 2 dropped: k7 (past the 7-pair keep bound) and the
        // collision itself.
        assert_eq!(details.dropped_count(), Some(2));
        assert_eq!(details.iter().count(), 8);
        for i in 0..7 {
            let key = format!("k{i}");
            assert_eq!(details.get(&key), Some(format!("v{i}").as_str()));
        }
        assert_eq!(details.get("k7"), None, "8th ordinary pair must be dropped");

        let serialized = serde_json::to_value(&details).unwrap();
        let obj = serialized.as_object().unwrap();
        assert_eq!(
            obj.keys()
                .filter(|k| k.as_str() == DETAILS_TRUNCATED_KEY)
                .count(),
            1,
            "must serialize exactly one details_truncated key, never a duplicate"
        );
        assert_eq!(
            obj.get(DETAILS_TRUNCATED_KEY).and_then(|v| v.as_str()),
            Some("2")
        );
    }

    /// Serde round-trip of a truncated `Details`: serializing our own
    /// truncation output and reading it back must restore the same drop
    /// count via the internal flag, not silently lose it because the
    /// reserved key on the wire looks like a fresh client collision.
    #[test]
    fn details_truncated_serde_roundtrip_restores_dropped_count() {
        let details = Details::new([
            ("k0", "v0"),
            ("k1", "v1"),
            ("k2", "v2"),
            ("k3", "v3"),
            ("k4", "v4"),
            ("k5", "v5"),
            ("k6", "v6"),
            ("k7", "v7"),
            ("k8", "v8"),
        ]);
        assert_eq!(details.dropped_count(), Some(2));

        let serialized = serde_json::to_string(&details).unwrap();
        let roundtripped: Details = serde_json::from_str(&serialized).unwrap();
        assert_eq!(
            roundtripped.dropped_count(),
            Some(2),
            "truncation state must survive a serialize/deserialize round trip"
        );
        for i in 0..7 {
            let key = format!("k{i}");
            assert_eq!(roundtripped.get(&key), Some(format!("v{i}").as_str()));
        }
    }
}
