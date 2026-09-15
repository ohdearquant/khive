use khive_runtime::secret_gate::{check, mask_for_redaction_surface, RedactionSurface};

const HEX: &str = "0123456789abcdef0123456789abcdef";
const UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

fn admit(content: &str) {
    assert!(
        check(content).is_ok(),
        "unexpected refusal: {:?}: {content}",
        check(content)
    );
    assert_eq!(
        mask_for_redaction_surface(RedactionSurface::SessionMirror, content),
        content
    );
}

fn refuse(content: &str, value: &str) {
    assert!(check(content).is_err(), "unexpected admission: {content}");
    assert!(
        !mask_for_redaction_surface(RedactionSurface::SessionMirror, content).contains(value),
        "credential survived masking: {content}"
    );
}

#[test]
fn issue_2756_separate_path_and_principal_scalars_admit() {
    for root in ["/tmp/pytest/auth", "/repo/.worktrees/auth"] {
        let content = format!(
            r#"{{"path": "{root}/workspace/fixture/output", "status": "ready", "phase": "done", "principal": "instance:{HEX}"}}"#
        );
        let distance = content.find(HEX).unwrap() - content.find("auth").unwrap();
        assert!((80..120).contains(&distance), "distance: {distance}");
        admit(&content);
        admit(&content.replace(", ", ","));
        admit(&content.replace(", ", ",\n  "));
    }
}

#[test]
fn issue_2756_sibling_context_is_independent_of_order_and_spacing() {
    for separator in [",", ", ", ",\n", ",\r\n"] {
        for content in [
            format!(r#"{{"path":"/tmp/auth/fixture"{separator}"principal":"instance:{HEX}"}}"#),
            format!(r#"{{"principal":"instance:{HEX}"{separator}"path":"/tmp/auth/fixture"}}"#),
            format!(r#"{{"a_secret":"x"{separator}"digest":"{HEX}"}}"#),
        ] {
            admit(&content);
        }
    }
}

#[test]
fn issue_2756_same_scalar_and_bare_prose_labels_refuse() {
    for content in [
        format!("secret {HEX}"),
        format!("{HEX} is the auth value"),
        format!(r#"{{"message":"secret {HEX}"}}"#),
        format!(r#"{{"message":"secret, {HEX}"}}"#),
        format!(r#"{{"message":"secret,{HEX}"}}"#),
        format!(r#"{{"message":"{HEX} is the auth value"}}"#),
        format!(r#"{{"message":"secret {HEX}", "path":"/tmp/auth/fixture"}}"#),
    ] {
        refuse(&content, HEX);
    }
}

#[test]
fn issue_2756_owning_credential_fields_refuse() {
    for value in [HEX, UUID] {
        for key in [
            "secret",
            "api_key",
            "token",
            "sec\\u0072et",
            "api\\u005fkey",
        ] {
            for gap in ["", " ", "\n"] {
                let content = format!(r#"{{"path":"/tmp/auth/fixture", "{key}":{gap}"{value}"}}"#);
                refuse(&content, value);
            }
        }
        for content in [
            format!(r#"{{"api_key":["{value}"]}}"#),
            format!(r#"{{"secret":{{"value":"{value}"}}}}"#),
            format!(r#"{{"outer":{{"api_key":"{value}"}}}}"#),
        ] {
            refuse(&content, value);
        }
    }
}

#[test]
fn issue_2756_own_path_triggers_admit_without_credential_label() {
    for root in ["/tmp/pytest/auth", "/repo/.worktrees/secret-context"] {
        let path = format!("{root}/{HEX}");
        admit(&path);
        admit(&format!(r#"{{"path":"{path}"}}"#));
        refuse(&format!("secret {path}"), HEX);
        refuse(&format!(r#"{{"api_key":"{path}"}}"#), HEX);
    }
}

#[test]
fn issue_2756_quoted_refusal_does_not_label_sibling_evidence() {
    let quoted = "content matches secret pattern hex-credential-token near 'auth' in member[0].doc";
    for content in [
        format!(r#"{{"error":"{quoted}", "path":"/tmp/pytest/auth/{HEX}"}}"#),
        format!(r#"{{"error":"{quoted}","principal":"instance:{HEX}"}}"#),
        format!(r#"{{"path":"/tmp/pytest/auth/{HEX}", "error":"{quoted}"}}"#),
    ] {
        admit(&content);
    }
    let refusal = check(&format!("auth {HEX}")).unwrap_err().to_string();
    admit(&format!(
        "The gate returned {refusal} Evidence path: /tmp/pytest/auth/{HEX}"
    ));
    refuse(&format!(r#"{{"error":"{quoted}", "secret":"{HEX}"}}"#), HEX);
}

#[test]
fn issue_2756_escaped_quotes_do_not_manufacture_field_boundaries() {
    let content = serde_json::json!({
        "error": "content matches secret pattern near 'auth', \"principal\": \\",
        "principal": format!("instance:{HEX}")
    })
    .to_string();
    admit(&content);
    let same_scalar = serde_json::json!({
        "message": format!("secret \", \\\"principal\\\": {HEX}")
    })
    .to_string();
    refuse(&same_scalar, HEX);
}

#[test]
fn issue_2756_non_json_and_malformed_json_keep_prose_context() {
    for content in [
        format!("a_secret:x digest:{HEX}"),
        format!(r#"{{"path":"/tmp/auth/fixture", "principal":"{HEX}""#),
        format!(r#"{{"path":"/tmp/auth/fixture"}} principal:{HEX}"#),
    ] {
        refuse(&content, HEX);
    }
}

#[test]
fn issue_2756_json_keeps_known_prefix_and_bridge_detection() {
    let known = format!("ghp_{}", "A".repeat(36));
    refuse(
        &format!(r#"{{"path":"/tmp/auth/fixture", "id":"{known}"}}"#),
        &known,
    );
    let first = &HEX[..16];
    let second = &HEX[16..];
    let content = format!("{{\"message\":\"secret {first}\u{200b}{second} \"}}");
    refuse(&content, first);
    refuse(&content, second);
    let content = format!("{{\"secret\":\"{first}\u{200b}{second} \"}}");
    refuse(&content, first);
    refuse(&content, second);
}

#[test]
fn issue_2756_json_siblings_keep_uuid_and_hash_rules() {
    let hash = format!("sha256-{}=", "A".repeat(43));
    for value in [UUID, hash.as_str()] {
        admit(&format!(
            r#"{{"path":"/tmp/auth/fixture", "id":"{value}"}}"#
        ));
        refuse(
            &format!(r#"{{"path":"/tmp/auth/fixture", "secret":"{value}"}}"#),
            value,
        );
    }
}
