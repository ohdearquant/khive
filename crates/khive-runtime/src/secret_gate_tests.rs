use super::*;

fn openai_project_key_fixture() -> String {
    format!("sk-proj-{}", "A".repeat(80))
}

#[test]
fn url_userinfo_placeholder_refusal_explains_effective_remedies() {
    for content in [
        "postgresql://dbuser:<PASSWORD>@db.example.com:5432/mydb",
        "postgresql://<USER>:<PASSWORD>@db.example.com:5432/mydb",
        "redis://:<PASSWORD>@cache.example.com:6379",
    ] {
        let error = check(content).expect_err("URL credential placeholders still match");
        let RuntimeError::SecretDetected(matched) = &error else {
            panic!("expected a secret refusal, got {error}");
        };
        assert_eq!(matched.detector, "url-userinfo");
        let rendered = error.to_string();
        for guidance in [
            "Placeholders in URL credential positions still match",
            "Replace the whole URL",
            "environment-variable name or config key",
            "remove the entire user/password segment",
        ] {
            assert!(
                rendered.contains(guidance),
                "missing {guidance:?}: {rendered}"
            );
        }
        assert!(!rendered.contains(content), "refusal must not echo the URL");
    }
}

#[test]
fn check_returns_err_for_secret() {
    let fake = "AKIAFAKEKEY1234567890";
    let result = check(fake);
    assert!(result.is_err(), "check must fail for AKIA key");
    let err = result.unwrap_err();
    assert!(
        matches!(err, RuntimeError::SecretDetected(_)),
        "error variant must be SecretDetected"
    );
}

// ── Structured-field gate helpers ────────────────────────────────────────

#[test]
fn check_json_blocks_secret_in_object_value() {
    let props = serde_json::json!({ "api_key": "AKIAFAKEKEY1234567890" });
    assert!(
        check_json(&props).is_err(),
        "secret in properties object value must be blocked"
    );
}

#[test]
fn check_json_blocks_secret_in_nested_object() {
    let props = serde_json::json!({
        "credentials": { "token": openai_project_key_fixture() }
    });
    assert!(
        check_json(&props).is_err(),
        "secret in nested properties object must be blocked"
    );
}

#[test]
fn check_json_blocks_secret_in_array() {
    let props = serde_json::json!(["normal", "AKIAFAKEKEY1234567890"]);
    assert!(
        check_json(&props).is_err(),
        "secret in JSON array must be blocked"
    );
}

// ── Reserved secret-gate property key (ADR-115 Amendment 1) ─────────────

#[test]
fn reject_reserved_key_passes_absent_properties() {
    assert!(reject_reserved_secret_gate_property(None).is_ok());
}

#[test]
fn reject_reserved_key_passes_unrelated_properties() {
    let props = serde_json::json!({"name": "value", "tags": ["a", "b"]});
    assert!(reject_reserved_secret_gate_property(Some(&props)).is_ok());
}

#[test]
fn reject_reserved_key_passes_non_object_properties() {
    // Non-object properties cannot name a top-level key at all.
    let props = serde_json::json!("just a string");
    assert!(reject_reserved_secret_gate_property(Some(&props)).is_ok());
    let arr = serde_json::json!(["a", "b"]);
    assert!(reject_reserved_secret_gate_property(Some(&arr)).is_ok());
}

#[test]
fn reject_reserved_key_blocks_top_level_key_creation() {
    let props = serde_json::json!({"khive:secret_gate": "exempted:content-sha256-manifest-v1"});
    let err = reject_reserved_secret_gate_property(Some(&props)).unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("khive:secret_gate") && msg.contains("runtime-owned")),
        "unexpected error: {err:?}"
    );
}

#[test]
fn reject_reserved_web_receipt_provenance_in_generic_properties() {
    for value in [serde_json::json!("v1"), serde_json::Value::Null] {
        let props = serde_json::json!({"khive:web_receipt": value});
        let error = reject_reserved_secret_gate_property(Some(&props)).unwrap_err();
        assert!(
            matches!(error, RuntimeError::InvalidInput(message) if message.contains("khive:web_receipt"))
        );
    }
}

#[test]
fn reject_reserved_key_blocks_regardless_of_value_shape() {
    // Presence alone is rejected — arbitrary value, null (explicit removal
    // shape), and a value that happens to match the real stamp format are
    // all rejected identically; a caller can never legitimately write this
    // key by any value.
    for value in [
        serde_json::json!(null),
        serde_json::json!(42),
        serde_json::json!({"nested": "object"}),
        serde_json::json!("exempted:content-sha256-manifest-v1"),
    ] {
        let props = serde_json::json!({"khive:secret_gate": value});
        assert!(
            reject_reserved_secret_gate_property(Some(&props)).is_err(),
            "must reject value shape: {props:?}"
        );
    }
}

#[test]
fn reject_reserved_key_allows_nested_non_top_level_occurrence() {
    // The same spelling nested inside a value is ordinary content, not a
    // posture mutation — only the exact top-level key is reserved.
    let props = serde_json::json!({"notes": {"khive:secret_gate": "not-a-stamp"}});
    assert!(reject_reserved_secret_gate_property(Some(&props)).is_ok());
}

#[test]
fn reject_reserved_key_blocks_alongside_other_legitimate_keys() {
    let props = serde_json::json!({
        "name": "value",
        "khive:secret_gate": "exempted:content-sha256-manifest-v1",
    });
    assert!(reject_reserved_secret_gate_property(Some(&props)).is_err());
}

#[test]
fn check_json_passes_safe_properties() {
    let props = serde_json::json!({
        "domain": "attention",
        "status": "researched",
        "year": 2024
    });
    assert!(
        check_json(&props).is_ok(),
        "normal properties must pass; fired: {:?}",
        check_json(&props).err()
    );
}

#[test]
fn check_tags_blocks_credential_tag() {
    let tags = vec![
        "type:concept".to_string(),
        "AKIAFAKEKEY1234567890".to_string(),
    ];
    assert!(
        check_tags(&tags).is_err(),
        "credential-shaped tag must be blocked"
    );
}

#[test]
fn check_tags_passes_normal_tags() {
    let tags = vec!["type:concept".to_string(), "domain:attention".to_string()];
    assert!(
        check_tags(&tags).is_ok(),
        "normal tags must pass; fired: {:?}",
        check_tags(&tags).err()
    );
}

// ── check_json scans object keys ─────────────────────────────────────────

#[test]
fn check_json_blocks_secret_in_object_key() {
    // A credential used as a JSON object key (not a value) must be caught.
    let props = serde_json::json!({ "ghp_FakeGitHubToken0000000000000000000": "redacted" }); // gitleaks:allow
    assert!(
        check_json(&props).is_err(),
        "credential as JSON object key must be blocked"
    );
}

#[test]
fn check_json_blocks_nested_secret_key() {
    // Nested credential key must be caught.
    let props = serde_json::json!({
        "metadata": {
            "AKIAFAKEKEY000000000": "value" // gitleaks:allow
        }
    });
    assert!(
        check_json(&props).is_err(),
        "nested credential as JSON object key must be blocked"
    );
}
