use super::*;
use crate::secret_gate_finalizer::manifest::fixture::TestOnlyManifestFixture;
use crate::secret_gate_finalizer::manifest::{digest_to_hex, RuntimeFieldScope};
use serde_json::json;

fn key(value: &str) -> PathSegment {
    PathSegment::Key(value.into())
}

fn contextual_token() -> String {
    "0123456789abcdef".repeat(2)
}

fn assert_contextual_refusal(value: &Value) {
    let result = crate::secret_gate::check_json(value);
    assert!(
        matches!(&result, Err(RuntimeError::SecretDetected(matched))
        if matched.detector == "hex-credential-token"),
        "the owning credential label must refuse its contextual scalar in {value}: {result:?}"
    );
}

#[test]
fn property_context_nine_literal_vectors_and_fixture_runtime_parity() {
    // These expected hashes are the accepted ADR vectors, not recomputed oracles.
    let cases = [
        (
            r#""v""#,
            AtomRole::Value,
            vec![],
            "6baa9872cfb3f833c9d30bd33b7068806836662c69fa7dac9e15255deed80a08",
        ),
        (
            r#"{"":"v"}"#,
            AtomRole::Value,
            vec![key("")],
            "de069468674f0725d5bcc394de90455f872bc545056a5eda6d952c0dd2c1b455",
        ),
        (
            r#"{"0":"v"}"#,
            AtomRole::Value,
            vec![key("0")],
            "6d642df340b229234d9e1d1afe43edb1a7ab96cd0335fe207dcdaff3b2082355",
        ),
        (
            r#"["v"]"#,
            AtomRole::Value,
            vec![PathSegment::Index(0)],
            "eaab483f55046f48186f1f1b331cedb8cfb969291e6e550c52b376ff8e78639b",
        ),
        (
            r#"{"v":"v"}"#,
            AtomRole::Key,
            vec![key("v")],
            "cfbb041d6ed6f465e0e4d43ecb9f56ad3eec484a5684c1d18d36dd3370b769fb",
        ),
        (
            r#"{"v":"v"}"#,
            AtomRole::Value,
            vec![key("v")],
            "3f7a18463fa2df89b139cad73fe170c73762853ff505da04c54a845d6165ecc9",
        ),
        (
            r#"{"k":"v","n":1}"#,
            AtomRole::Value,
            vec![key("k")],
            "84fd13741dab27225ac85f2234fe7597e55262d4894f2681fc30019170b58beb",
        ),
        (
            r#"{"k":"v","n":1.0}"#,
            AtomRole::Value,
            vec![key("k")],
            "9efc8e124fc41049943b01e0639cab31305d83f41649817dd70cf6c40d1de0a4",
        ),
        (
            r#"{"é":"x\ny"}"#,
            AtomRole::Value,
            vec![key("é")],
            "ae3bdf06c0415acf49966579a6f1b087684d35d95fbb7313c8be30df987388c9",
        ),
    ];
    for (wire, role, path, expected) in cases {
        let parsed: Value = serde_json::from_str(wire).unwrap();
        let context = PropertyContext::new(&parsed).unwrap();
        assert_eq!(context.canonical, wire, "canonical ADR vector changed");
        let atom = context.atom(role, &path).unwrap();
        assert_eq!(
            digest_to_hex(atom.digest()),
            expected,
            "context/path/role framing changed"
        );
        let fixture = TestOnlyManifestFixture::property_snapshot(wire, &[(role, path)]).unwrap();
        assert!(
            fixture.lookup_property(atom).is_some(),
            "fixture builder and runtime identity disagree"
        );
        assert!(
            fixture
                .lookup(RuntimeFieldScope::JsonPropertiesContextV2, &atom.value)
                .is_none(),
            "a raw scalar must not impersonate the context-bound lookup"
        );
    }
}

#[test]
fn property_context_canonical_order_typed_paths_and_whole_context_are_binding() {
    let a: Value = serde_json::from_str(r#"{ "z": 1, "é": "v", "a": ["v","v"] }"#).unwrap();
    let b: Value = serde_json::from_str(r#"{"a":["v","v"],"z":1,"\u00e9":"v"}"#).unwrap();
    let a = PropertyContext::new(&a).unwrap();
    let b = PropertyContext::new(&b).unwrap();
    assert_eq!(a.canonical, r#"{"a":["v","v"],"z":1,"é":"v"}"#);
    let path = vec![key("a"), PathSegment::Index(0)];
    assert_eq!(
        a.atom(AtomRole::Value, &path).unwrap().digest(),
        b.atom(AtomRole::Value, &path).unwrap().digest()
    );
    assert_ne!(
        a.atom(AtomRole::Value, &path).unwrap().digest(),
        a.atom(AtomRole::Value, &[key("a"), PathSegment::Index(1)])
            .unwrap()
            .digest(),
        "array positions must be part of identity"
    );
    let changed = PropertyContext::new(&json!({"a":["v","v"], "z":2, "é":"v"})).unwrap();
    assert_ne!(
        a.atom(AtomRole::Value, &path).unwrap().digest(),
        changed.atom(AtomRole::Value, &path).unwrap().digest(),
        "an unrelated sibling still changes the bound context"
    );
    assert_eq!(
        a.atoms.iter().map(|atom| atom.role).collect::<Vec<_>>(),
        vec![
            AtomRole::Key,
            AtomRole::Value,
            AtomRole::Value,
            AtomRole::Key,
            AtomRole::Key,
            AtomRole::Value
        ]
    );
    for atom in &a.atoms {
        assert_eq!(
            serde_json::from_str::<String>(&a.canonical[atom.span.clone()]).unwrap(),
            atom.value
        );
    }
}

#[test]
fn property_context_owning_ancestry_arrays_and_sibling_isolation() {
    let token = contextual_token();
    assert!(
        crate::secret_gate::check(&token).is_ok(),
        "fixture must require credential context"
    );
    for value in [
        json!({"secret": token}),
        json!({"secret":{"child":token}}),
        json!({"secret":[{"child":token}]}),
        json!({"secret":{"password":token}}),
    ] {
        assert_contextual_refusal(&value);
    }
    for value in [
        json!({"secret":"ordinary", "z":token}),
        json!({"secret":{"empty":true}, "z":[token]}),
        json!([token]),
        json!(token),
        json!({"secret":17,"password":null,"auth":false}),
    ] {
        assert!(
            crate::secret_gate::check_json(&value).is_ok(),
            "siblings and non-string values do not inherit labels: {value}"
        );
    }
    let matched =
        crate::secret_gate::check_json(&json!({"secret":{"password":token}})).unwrap_err();
    assert!(
        matches!(matched, RuntimeError::SecretDetected(ref m) if m.trigger == Some("password")),
        "the child's own credential label takes precedence"
    );
}

fn escaped_token() -> String {
    contextual_token()
        .chars()
        .map(|c| format!("\\u{:04x}", c as u32))
        .collect()
}

fn first_escaped_token_origin(canonical: &str) -> usize {
    let mut start = canonical
        .find("u0030")
        .expect("literal escaped token fixture");
    while start > 0 && canonical.as_bytes()[start - 1] == b'\\' {
        start -= 1;
    }
    start
}

#[test]
fn property_context_original_radius_120_121_survives_nested_surrogate_and_recovery_views() {
    let escaped = escaped_token();
    let inner = format!("\"{}{}\"", "\\u0020".repeat(3), escaped);
    let nested = serde_json::to_string(&inner).unwrap();
    let surrogate = format!("\"\\ud83d\\ude00 {}\"", escaped);
    let recovered = format!("broken! {{\"payload\":\"{escaped}\"}}");
    for encoded in [inner, nested, surrogate, recovered] {
        for distance in [120, 121] {
            let (value, atom, actual_distance) = (0..160)
                .find_map(|padding| {
                    let atom = format!("{}{encoded}", " ".repeat(padding));
                    let value = json!({"secret":atom});
                    let canonical = serde_json::to_string(&value).unwrap();
                    let label_end = canonical.find("\"secret\"").unwrap() + "\"secret\"".len() - 1;
                    let actual = first_escaped_token_origin(&canonical).saturating_sub(label_end);
                    (actual == distance).then_some((value, atom, actual))
                })
                .expect("construct exact original-source radius");
            assert_eq!(actual_distance, distance);
            assert!(
                crate::secret_gate::check(&atom).is_ok(),
                "the isolated source atom must admit before context is applied"
            );
            // Every form contracts escapes. The shortened view remains inside 120
            // in both arms, so a decoded-offset implementation cannot pass both.
            assert!(
                atom.find("\\u0030").unwrap() < 120,
                "decoded-view prefix precondition"
            );
            if distance == 120 {
                assert_contextual_refusal(&value);
            } else {
                assert!(
                    crate::secret_gate::check_json(&value).is_ok(),
                    "original byte 121 must remain outside the owning label radius"
                );
            }
        }
    }
}

#[test]
fn property_context_sorted_sibling_counts_in_inherited_original_radius() {
    let token = contextual_token();
    assert!(crate::secret_gate::check(&token).is_ok());
    for distance in [120, 121] {
        let value = (0..160)
            .find_map(|padding| {
                let wire = format!(
                    r#"{{"secret":{{"z":"{token}","a{}":""}}}}"#,
                    "x".repeat(padding)
                );
                let value: Value = serde_json::from_str(&wire).unwrap();
                let context = PropertyContext::new(&value).unwrap();
                let label_end = context.canonical.find("\"secret\"").unwrap() + 7;
                let start = context.canonical.find(&token).unwrap();
                (start - label_end == distance).then_some(value)
            })
            .expect("sorted sibling boundary");
        if distance == 120 {
            assert_contextual_refusal(&value);
        } else {
            assert!(
                crate::secret_gate::check_json(&value).is_ok(),
                "the sorted intervening sibling consumes original-source radius"
            );
        }
    }
}

#[test]
fn property_context_raw_and_decoded_known_patterns_still_scan_keys_and_roots() {
    let fixed = TestOnlyManifestFixture::new().exact_value().to_owned();
    let escaped: String = fixed
        .chars()
        .map(|c| format!("\\u{:04x}", c as u32))
        .collect();
    for value in [
        json!(fixed),
        json!([fixed]),
        json!({(fixed.clone()):false}),
        json!({"value":format!("\"{escaped}\"")}),
    ] {
        assert!(
            matches!(
                crate::secret_gate::check_json(&value),
                Err(RuntimeError::SecretDetected(_))
            ),
            "known-pattern scanning must cover original atoms and decoded views"
        );
    }
}

#[test]
fn property_context_scope_is_closed_schema_one_and_cannot_use_scalar_lookup() {
    use crate::secret_gate_finalizer::manifest::{parse_document, resolve_match, scoped_digest};
    let value = json!({"k":"v"});
    let context = PropertyContext::new(&value).unwrap();
    let atom = context.atom(AtomRole::Value, &[key("k")]).unwrap();
    let mut wire = json!({"schema_version":1, "manifest_id":"context-scope-test", "algorithm":"sha256",
        "corpus_identity_sha256":"00".repeat(32), "entries":[{"field_scope":"json-properties-context-v2",
        "digest_sha256":digest_to_hex(atom.digest()), "overridden_detector":"fixture"}]});
    let parsed = parse_document(&serde_json::to_vec(&wire).unwrap(), Some([0; 32])).unwrap();
    assert!(
        parsed.lookup_property(atom).is_some(),
        "schema one must recognize the exact context scope"
    );
    wire["entries"][0]["field_scope"] = "json-properties-context-v3".into();
    assert!(
        parse_document(&serde_json::to_vec(&wire).unwrap(), Some([0; 32])).is_err(),
        "the context field scope remains closed"
    );
    wire["entries"][0]["field_scope"] = "json-properties".into();
    wire["entries"][0]["digest_sha256"] =
        digest_to_hex(&scoped_digest(RuntimeFieldScope::JsonProperties, "v")).into();
    let legacy = parse_document(&serde_json::to_vec(&wire).unwrap(), Some([0; 32])).unwrap();
    assert!(
        legacy.lookup_property(atom).is_none(),
        "parse-valid legacy entries must be inert for context admission"
    );
    let forged = TestOnlyManifestFixture::for_scope(RuntimeFieldScope::JsonPropertiesContextV2);
    assert!(
        resolve_match(
            &forged.snapshot(),
            &[(
                RuntimeFieldScope::JsonPropertiesContextV2,
                forged.exact_value()
            )]
        )
        .unwrap()
        .is_none(),
        "the old raw-scalar resolver cannot manufacture a context identity"
    );
}

#[test]
fn property_context_benign_outer_preserves_inner_credential_labels() {
    let token = contextual_token();
    assert!(
        crate::secret_gate::check(&token).is_ok(),
        "the isolated inner token must require credential context"
    );
    let inner = json!({"secret": token}).to_string();
    for value in [
        json!({"payload": {"secret": token}}),
        json!({"payload": inner}),
        json!({"payload": serde_json::to_string(&inner).unwrap()}),
    ] {
        let result = crate::secret_gate::check_json(&value);
        assert!(
            matches!(&result, Err(RuntimeError::SecretDetected(matched))
                if matched.detector == "hex-credential-token" && matched.trigger == Some("secret")),
            "a benign outer property must retain its inner credential label in {value}: {result:?}"
        );
    }
}

#[test]
fn property_context_quoted_keys_and_refusal_siblings_do_not_create_context() {
    let token = contextual_token();
    assert!(crate::secret_gate::check(&token).is_ok());
    for name in [
        "payload\"\n},{\"extra\":\"ordinary",
        "payload\n[ordinary]",
        "λ\"{ordinary}\n",
    ] {
        let value = json!({(name): token});
        let context = PropertyContext::new(&value).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&context.canonical).unwrap(),
            value,
            "quoted and newline keys must retain the actual parsed tree"
        );
        assert_eq!(
            context.atoms.len(),
            2,
            "key punctuation must not invent source atoms"
        );
        assert!(context.atom(AtomRole::Key, &[key(name)]).is_some());
        assert!(
            crate::secret_gate::check_json(&value).is_ok(),
            "a benign quoted or newline key must not manufacture a credential label: {value}"
        );
    }
    for value in [
        json!({"diagnostic": "write blocked near 'password': remove the credential", "value": token}),
        json!([{"secret": "ordinary"}, {"value": token}]),
        json!([{"value": token}, {"secret": "ordinary"}]),
    ] {
        assert!(
            crate::secret_gate::check_json(&value).is_ok(),
            "quoted refusals and separate array objects must not label a sibling: {value}"
        );
    }
}
