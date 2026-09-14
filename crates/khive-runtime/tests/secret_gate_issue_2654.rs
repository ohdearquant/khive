//! Issue #2654 acceptance: `_key` member names that name a lookup identifier
//! no longer arm the UUID rule, while credential compounds, bare `key`, and
//! every `*_key` label outside the closed lookup vocabulary stay refused.
//!
//! A production-corpus replay that is identical before and after a gate change
//! certifies preservation on the corpus population only, never on the shape
//! class the change opens; that class needs its own before/after arms (below).

use khive_runtime::secret_gate::{check, mask_for_redaction_surface, RedactionSurface};

#[test]
fn issue_2654_arm_1() {
    let content = r###"{"association_key":"550e8400-e29b-41d4-a716-446655440000","neighbor":"550e8400-e29b-41d4-a716-446655440000"}"###;
    assert!(check(content).is_ok(), "{:?}", check(content));
    assert_eq!(
        mask_for_redaction_surface(RedactionSurface::SessionMirror, content),
        content
    );
}

#[test]
fn issue_2654_arm_2() {
    let content = r###"{"association_ref":"550e8400-e29b-41d4-a716-446655440000","neighbor":"550e8400-e29b-41d4-a716-446655440000"}"###;
    assert!(check(content).is_ok(), "{:?}", check(content));
    assert_eq!(
        mask_for_redaction_surface(RedactionSurface::SessionMirror, content),
        content
    );
}

#[test]
fn issue_2654_arm_3() {
    let content = r###"{"association_key":"","neighbor":"550e8400-e29b-41d4-a716-446655440000"}"###;
    assert!(check(content).is_ok(), "{:?}", check(content));
    assert_eq!(
        mask_for_redaction_surface(RedactionSurface::SessionMirror, content),
        content
    );
}

#[test]
fn issue_2654_arm_4() {
    let content = r###"{"association_key":"550e8400-e29b-41d4-a716-446655440000", "neighbor":"550e8400-e29b-41d4-a716-446655440000"}"###;
    assert!(check(content).is_ok(), "{:?}", check(content));
    assert_eq!(
        mask_for_redaction_surface(RedactionSurface::SessionMirror, content),
        content
    );
}

#[test]
fn issue_2654_api_key_uuid_stays_refused() {
    let content = "api_key=550e8400-e29b-41d4-a716-446655440000";
    assert!(check(content).is_err());
    assert!(
        !mask_for_redaction_surface(RedactionSurface::SessionMirror, content)
            .contains("550e8400-e29b-41d4-a716-446655440000")
    );
}

#[test]
fn issue_2654_secret_key_uuid_stays_refused() {
    let content = "secret_key: 550e8400-e29b-41d4-a716-446655440000";
    assert!(check(content).is_err());
    assert!(
        !mask_for_redaction_surface(RedactionSurface::SessionMirror, content)
            .contains("550e8400-e29b-41d4-a716-446655440000")
    );
}

#[test]
fn issue_2654_bare_key_uuid_stays_refused() {
    let content = "key=550e8400-e29b-41d4-a716-446655440000";
    assert!(check(content).is_err());
    assert!(
        !mask_for_redaction_surface(RedactionSurface::SessionMirror, content)
            .contains("550e8400-e29b-41d4-a716-446655440000")
    );
}

#[test]
fn issue_2654_unlisted_key_compounds_stay_refused_with_hex_and_opaque_values() {
    // Refused at the base before #2654 and still refused: the lookup exception
    // must not reach these labels.
    let hex = "0123456789abcdef".repeat(4);
    let opaque = "ABCDEFGHIJKLMNOPQRSTUVWXabcdefghijklmnopqrstuvwxyz0123456789";
    for content in [
        format!("hmac_key={hex}"),
        format!("master_key: {hex}"),
        format!("ssh_key={opaque}"),
        format!("jwt_key={hex}"),
        format!(r#"{{"webhook_key":"{hex}"}}"#),
        format!("license_key={opaque}"),
    ] {
        assert!(check(&content).is_err(), "{content}");
        assert!(
            !mask_for_redaction_surface(RedactionSurface::SessionMirror, &content).contains(&hex)
                || !content.contains(&hex),
            "{content}"
        );
    }
}
