//! Tests for KhiveError — structured cross-crate error model.
//!
//! Covers: Display, Error trait, serde wire shape stability, RetryHint,
//! Details, and the ErrorCode domain-scoped code model.

use khive_types::khive_error::{Details, ErrorCode, ErrorDomain, ErrorKind, KhiveError, RetryHint};

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
}
