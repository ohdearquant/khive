use khive_runtime::secret_gate::{check, mask_for_redaction_surface, RedactionSurface};

const UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

fn long_document(field: &str, value: &str) -> String {
    format!(
        r#"{{"a_secret":"x","padding":"{}","{field}":"{value}"}}"#,
        "a".repeat(500)
    )
}

fn accept(content: &str) {
    assert!(
        check(content).is_ok(),
        "unexpected refusal: {:?}",
        check(content)
    );
    assert_eq!(
        mask_for_redaction_surface(RedactionSurface::SessionMirror, content),
        content
    );
}

#[test]
fn issue_2655_arm_1_long_compact_uuid() {
    accept(&long_document("id", UUID));
}

#[test]
fn issue_2655_arm_2_long_spaced_uuid() {
    accept(&long_document("id", UUID).replace(',', ", "));
}

#[test]
fn issue_2655_arm_3_long_compact_digest() {
    accept(&long_document("digest", &"0123456789abcdef".repeat(4)));
}

#[test]
fn issue_2655_arm_4_long_spaced_digest() {
    accept(&long_document("digest", &"0123456789abcdef".repeat(4)).replace(',', ", "));
}

#[test]
fn issue_2655_minimal_compact_digest() {
    let content = format!(
        r#"{{"a_secret":"x","digest":"{}"}}"#,
        "0123456789abcdef".repeat(4)
    );
    accept(&content);
}

#[test]
fn issue_2655_api_key_uuid_stays_refused() {
    assert!(check(&format!("api_key={UUID}")).is_err());
}

#[test]
fn issue_2655_secret_hex_stays_refused() {
    assert!(check(&format!("secret={}", "0123456789abcdef".repeat(4))).is_err());
}

#[test]
fn issue_2655_known_credential_under_benign_member_stays_refused() {
    let credential = format!("ghp_{}", "A".repeat(36));
    let content = long_document("digest", &credential);
    assert!(check(&content).is_err());
    assert!(
        !mask_for_redaction_surface(RedactionSurface::SessionMirror, &content)
            .contains(&credential)
    );
}

#[test]
fn issue_2655_nonsecret_slug_and_path_stay_accepted() {
    for value in ["daily-report", "docs/guide.md"] {
        accept(&format!("a_secret:{value}"));
    }
}
