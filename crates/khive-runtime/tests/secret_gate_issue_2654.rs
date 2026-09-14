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
