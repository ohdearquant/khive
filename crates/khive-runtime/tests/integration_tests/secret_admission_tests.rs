// =============================================================================
// Secret gate: structured-field bypass regression (#83)
// =============================================================================

#[tokio::test]
async fn entity_create_blocks_secret_in_properties() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    // A fake AWS key embedded in entity properties — must be blocked.
    let props = serde_json::json!({ "api_key": "AKIAFAKEKEY1234567890" });
    let result = rt
        .create_entity_with_embedding_report(
            &tok,
            "concept",
            None,
            "TestEntity",
            None,
            Some(props),
            vec![],
        )
        .await
        .map(|(record, _report)| record);
    assert!(
        result.is_err(),
        "entity create with secret in properties must be blocked"
    );
    assert!(
        matches!(
            result.unwrap_err(),
            khive_runtime::RuntimeError::SecretDetected(_)
        ),
        "error must be SecretDetected"
    );
}

#[tokio::test]
async fn entity_create_blocks_secret_in_tags() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let tags = vec![
        "type:concept".to_string(),
        "AKIAFAKEKEY1234567890".to_string(),
    ];
    let result = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "TestEntity", None, None, tags)
        .await
        .map(|(record, _report)| record);
    assert!(
        result.is_err(),
        "entity create with secret in tags must be blocked"
    );
    assert!(
        matches!(
            result.unwrap_err(),
            khive_runtime::RuntimeError::SecretDetected(_)
        ),
        "error must be SecretDetected"
    );
}

#[tokio::test]
async fn note_create_blocks_secret_in_properties() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let props = serde_json::json!({ "api_key": "AKIAFAKEKEY1234567890" });
    let result = rt
        .create_note(
            &tok,
            "observation",
            None,
            "Safe content",
            None,
            Some(props),
            vec![],
        )
        .await;
    assert!(
        result.is_err(),
        "note create with secret in properties must be blocked"
    );
    assert!(
        matches!(
            result.unwrap_err(),
            khive_runtime::RuntimeError::SecretDetected(_)
        ),
        "error must be SecretDetected"
    );
}

// Regression: pure-hex credential in trigger context must be blocked.
// Pure hex cannot reach entropy threshold (hex max 4.0 < 4.5), so the
// secret gate must detect it via the hex-credential-token path.  This
// test exercises the MCP-reachable write path (create_note → create_note_inner
// → secret_gate::check) to confirm persistence is blocked end-to-end.
#[tokio::test]
async fn note_create_blocks_hex_credential_in_content() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    // 32-char pure hex near the phrase "api key" in the note body.
    let content = "api key 4f9c2e8a1d3b5c7e9f0a2b4d6e8c0a2b"; // gitleaks:allow
    let result = rt
        .create_note(&tok, "observation", None, content, None, None, vec![])
        .await;
    assert!(
        result.is_err(),
        "note create with hex credential in content must be blocked; got Ok"
    );
    assert!(
        matches!(
            result.unwrap_err(),
            khive_runtime::RuntimeError::SecretDetected(_)
        ),
        "error must be SecretDetected"
    );
}

#[tokio::test]
async fn note_create_allows_source_path_near_ordinary_key_prose() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let content = "see <a/path/to/file.py>:~97-103 lists it as a real checkpoint-supplied key";

    rt.create_note(&tok, "question", None, content, None, None, vec![])
        .await
        .expect("source path in technical prose must be stored");
}

#[tokio::test]
async fn note_create_allows_git_revision_near_ordinary_token_prose() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let content = "revision d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf emits one extra token";

    rt.create_note(&tok, "question", None, content, None, None, vec![])
        .await
        .expect("git revision in technical prose must be stored");
}

#[tokio::test]
async fn note_create_blocks_forty_hex_in_value_syntax_behind_vcs_marker() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let content = "api key value is commit d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";

    let result = rt
        .create_note(&tok, "question", None, content, None, None, vec![])
        .await;
    assert!(
        matches!(
            result.unwrap_err(),
            khive_runtime::RuntimeError::SecretDetected(_)
        ),
        "a credential phrase must not be rescued by a VCS marker at the write path"
    );
}

#[tokio::test]
async fn note_create_blocks_path_dressed_base64_credential_in_value_syntax() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let content = "api key value is <Xk9mZ2vQpLrT8nJwYuA/HfBsDcGiONvMabcdefgh>:~97-103";

    let result = rt
        .create_note(&tok, "question", None, content, None, None, vec![])
        .await;
    assert!(
        matches!(
            result.unwrap_err(),
            khive_runtime::RuntimeError::SecretDetected(_)
        ),
        "path dressing must not rescue a credential in value syntax at the write path"
    );
}
