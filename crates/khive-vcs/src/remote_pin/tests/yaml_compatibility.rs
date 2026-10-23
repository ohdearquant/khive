use super::*;

fn compatibility_schema() -> String {
    format!(
        "{}\nscalars:\n  quoted: 'true'\n  boolean: true\n  legacy_word: yes\n  number: 7\n  quoted_number: '7'\n  empty: null\n  tilde: ~\naliases:\n  original: &settings {{enabled: true}}\n  copy: *settings\ntagged: !Widget {{enabled: true}}\ntyped_keys:\n  7: seven\n  ? [left, right]\n  : pair\n",
        schema()
    )
}

#[test]
fn yaml_value_baseline_preserves_scalars_aliases_tags_and_non_string_keys() {
    let document: Value = yaml_serde::from_str(&compatibility_schema()).unwrap();
    assert_eq!(document["scalars"]["quoted"], Value::String("true".into()));
    assert_eq!(document["scalars"]["boolean"], Value::Bool(true));
    assert_eq!(
        document["scalars"]["legacy_word"],
        Value::String("yes".into())
    );
    assert_eq!(document["scalars"]["number"], Value::Number(7.into()));
    assert_eq!(
        document["scalars"]["quoted_number"],
        Value::String("7".into())
    );
    for key in ["empty", "tilde"] {
        assert!(document["scalars"].as_mapping().unwrap().contains_key(key));
    }
    assert_eq!(document["scalars"]["empty"], Value::Null);
    assert_eq!(document["scalars"]["tilde"], Value::Null);
    assert_eq!(document["aliases"]["original"], document["aliases"]["copy"]);
    assert_eq!(document["aliases"]["copy"]["enabled"], Value::Bool(true));
    let Value::Tagged(tagged) = &document["tagged"] else {
        panic!("the custom YAML tag must not be discarded");
    };
    assert_eq!(tagged.tag, "Widget");
    assert_eq!(tagged.value["enabled"], Value::Bool(true));
    let keys = document["typed_keys"].as_mapping().unwrap();
    assert_eq!(
        keys.get(Value::Number(7.into())),
        Some(&Value::String("seven".into()))
    );
    assert_eq!(
        keys.get(Value::Sequence(vec![
            Value::String("left".into()),
            Value::String("right".into()),
        ])),
        Some(&Value::String("pair".into()))
    );
    assert!(serde_json::to_value(&document).is_err());
}

#[test]
fn remote_pin_preserves_arbitrary_yaml_values_and_exact_noop_bytes() {
    let source = compatibility_schema();
    let (temp, path) = fixture(source.as_bytes());
    let mut expected = value(&path);
    let report = update_remote_pin_with(
        temp.path(),
        &origin(),
        None,
        |_, _, _| Ok(OLD.to_owned()),
        |_| panic!("a no-op must not serialize or publish"),
    )
    .unwrap();
    assert!(!report.updated);
    assert_eq!(fs::read(&path).unwrap(), source.as_bytes());

    expected["remotes"][0]["commit"] = Value::String(NEW.to_owned());
    assert!(apply(&temp, NEW).unwrap().updated);
    assert_eq!(value(&path), expected);
    assert_eq!(pending_count(&path), 0);
}

#[test]
fn duplicate_yaml_keys_and_credential_bearing_syntax_errors_never_resolve_or_write() {
    let baseline = schema();
    for source in [
        format!("{baseline}\nformat_version: '2.0.0'\n"),
        baseline.replace("    ref: stable", "    ref: stable\n    ref: duplicate"),
        baseline.replace(
            "url: ./remote.git",
            "url: [https://fixture-user:fake-credential@example.invalid/repo.git",
        ),
    ] {
        let (temp, path) = fixture(source.as_bytes());
        let error = update_remote_pin_with(
            temp.path(),
            &origin(),
            None,
            |_, _, _| panic!("invalid YAML must not reach remote resolution"),
            |_| panic!("invalid YAML must not reach publication"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.starts_with("invalid KG schema YAML"), "{error}");
        for secret in [
            "fixture-user",
            "fake-credential",
            "example.invalid",
            "https://",
        ] {
            assert!(!error.contains(secret), "{error}");
        }
        assert_eq!(fs::read(&path).unwrap(), source.as_bytes());
        assert!(!temp.path().join(".khive/state").exists());
        assert_eq!(pending_count(&path), 0);
    }
}
