use super::*;

#[tokio::test]
async fn yaml_frontmatter_preserves_scalar_alias_tag_and_numeric_key_semantics() {
    let f = fixture();
    let root = TempDir::new().unwrap();
    let body = markdown("YAML Compatibility");
    let source = format!(
        "---\nid: yaml-compatibility\nname: YAML Compatibility\ntags: &tags [quoted, alias]\nproperties:\n  quoted: 'true'\n  boolean: true\n  legacy_word: yes\n  number: 7\n  quoted_number: '7'\n  empty: null\n  tilde: ~\n  list: *tags\n  tagged: !Widget {{enabled: true}}\n  numeric_keys: {{7: seven}}\n---\n{body}"
    );
    let path = root.path().join("compatibility.md");
    std::fs::write(&path, source).unwrap();

    let response = f
        .dispatch(
            "knowledge.import",
            json!({"path": path.to_str().unwrap(), "chunk_strategy": "atom"}),
        )
        .await
        .unwrap();
    assert_eq!(response["imported_atoms"], 1);
    let atom = f
        .dispatch("knowledge.get", json!({"id": "yaml-compatibility"}))
        .await
        .unwrap();
    assert_eq!(atom["content"], body);
    assert_eq!(atom["tags"], json!(["quoted", "alias"]));
    for (key, expected) in [
        ("quoted", json!("true")),
        ("boolean", json!(true)),
        ("legacy_word", json!("yes")),
        ("number", json!(7)),
        ("quoted_number", json!("7")),
        ("empty", Value::Null),
        ("tilde", Value::Null),
        ("list", json!(["quoted", "alias"])),
        ("tagged", json!({"!Widget": {"enabled": true}})),
        ("numeric_keys", json!({"7": "seven"})),
    ] {
        assert!(atom["properties"].as_object().unwrap().contains_key(key));
        assert_eq!(atom["properties"][key], expected, "{key}: {atom}");
    }
}

#[tokio::test]
async fn closed_invalid_yaml_and_unrepresentable_keys_refuse_before_any_import_write() {
    for strategy in ["atom", "section"] {
        for header in [
            "title: [unterminated\n",
            "title: first\ntitle: duplicate\n",
            "? [left, right]\n: cannot-be-a-json-key\n",
        ] {
            let f = fixture();
            let root = TempDir::new().unwrap();
            let valid = markdown("Valid First");
            let invalid = format!("---\n{header}---\n{}", markdown("Invalid Last"));
            let valid_path = root.path().join("a-valid.md");
            let invalid_path = root.path().join("z-invalid.md");
            std::fs::write(&valid_path, &valid).unwrap();
            std::fs::write(&invalid_path, &invalid).unwrap();

            let error = f
                .dispatch(
                    "knowledge.import",
                    json!({"path": root.path().to_str().unwrap(), "chunk_strategy": strategy}),
                )
                .await
                .expect_err("all sources must pass YAML preflight before any write");
            let message = error.to_string();
            assert!(message.contains("frontmatter"), "{message}");
            assert!(message.contains("z-invalid.md"), "{message}");
            assert!(!message.contains("missing closing"), "{message}");
            for table in ["knowledge_atoms", "knowledge_sections"] {
                assert_eq!(
                    f.count(&format!("SELECT COUNT(*) AS count FROM {table}"))
                        .await,
                    0,
                    "{strategy}: {header:?}: {table}"
                );
            }
            assert_eq!(std::fs::read_to_string(valid_path).unwrap(), valid);
            assert_eq!(std::fs::read_to_string(invalid_path).unwrap(), invalid);
        }
    }
}
