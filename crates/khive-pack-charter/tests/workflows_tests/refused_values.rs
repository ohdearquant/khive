use khive_runtime::secret_gate::{mask_for_redaction_surface, RedactionSurface};

use super::*;

async fn stored_rows(f: &Fixture) -> BTreeMap<String, Vec<Value>> {
    let mut reader = f.runtime.sql().reader().await.unwrap();
    let tables = reader
        .query_all(SqlStatement::new(
            "SELECT name FROM sqlite_schema WHERE type = 'table' \
             AND (name LIKE 'charter_%' OR name IN ('entities', 'notes', 'graph_edges', 'events')) \
             ORDER BY name",
            vec![],
        ))
        .await
        .unwrap();
    assert_eq!(
        tables.len(),
        13,
        "nine Charter tables and four substrate tables"
    );
    let mut snapshot = BTreeMap::new();
    for table in tables {
        let name = table.text("name").unwrap();
        let mut rows: Vec<_> = reader
            .query_all(SqlStatement::new(format!("SELECT * FROM {name}"), vec![]))
            .await
            .unwrap()
            .into_iter()
            .map(|row| serde_json::to_value(row).unwrap())
            .collect();
        rows.sort_by_cached_key(Value::to_string);
        snapshot.insert(name.to_owned(), rows);
    }
    assert!(
        snapshot.values().filter(|rows| !rows.is_empty()).count() >= 3,
        "published definitions and an actual run must populate the witness"
    );
    snapshot
}

fn shown_operand(value: &Value, secret: &str, masked: bool) -> String {
    let serialized = value.to_string();
    let shown = mask_for_redaction_surface(RedactionSurface::GateProbe, &serialized);
    if masked {
        assert!(
            serialized.contains(secret),
            "synthetic credential is present"
        );
        assert!(
            !shown.contains(secret),
            "canonical masker recognizes the fixture"
        );
        assert!(shown.contains("***MASKED***"));
    } else {
        assert_eq!(shown, serialized, "benign JSON remains unchanged");
    }
    shown.into_owned()
}

async fn refuse_without_writes(
    f: &Fixture,
    verb: &str,
    params: Value,
    expected: &str,
    secret: &str,
    masked: bool,
) {
    let before = stored_rows(f).await;
    let error = f
        .registry
        .dispatch(verb, params)
        .await
        .expect_err("wrong JSON type");
    let RuntimeError::InvalidInput(message) = error else {
        panic!("wrong refusal kind: {error:?}");
    };
    assert_eq!(message, expected, "raw diagnostic before error projection");
    assert!(
        !message.contains(secret),
        "raw refusal exposed a credential"
    );
    if masked {
        assert!(
            message.contains("***MASKED***"),
            "raw refusal omitted its mask"
        );
    }
    assert_eq!(
        stored_rows(f).await,
        before,
        "refusal changed persisted values"
    );
}

#[tokio::test]
async fn wrong_type_charter_operands_mask_credentials_without_mutation() {
    let f = Fixture::new();
    f.publish_review_set().await;
    let run = f.start_review().await;
    assert_eq!(f.at(&run).await.0, "open");
    let secret = format!("sk-proj-{}", "A".repeat(80));

    for (value, masked) in [
        (json!(17), false),
        (json!("ordinary"), false),
        (json!(["ordinary"]), false),
        (json!(secret.clone()), true),
        (json!([{"credential": secret.clone()}]), true),
    ] {
        let shown = shown_operand(&value, &secret, masked);
        refuse_without_writes(
            &f,
            "charter.claim",
            value.clone(),
            &format!("charter.claim: parameters must be an object, got {shown}"),
            &secret,
            masked,
        )
        .await;

        let mut claim = claim_params(&run);
        claim["descriptor"] = value.clone();
        refuse_without_writes(
            &f,
            "charter.claim",
            claim,
            &format!("charter.claim: descriptor must be an object, got {shown}"),
            &secret,
            masked,
        )
        .await;

        refuse_without_writes(
            &f,
            "charter.start",
            json!({
                "charter": "pr_review", "subject_kind": "pull_request", "subject_ref": PR,
                "bindings": value,
            }),
            &format!("charter.start: bindings must be an object, got {shown}"),
            &secret,
            masked,
        )
        .await;
    }

    for (value, masked) in [
        (json!(17), false),
        (json!({"label": "ordinary"}), false),
        (json!(["ordinary"]), false),
        (json!({"credential": secret.clone()}), true),
        (json!([{"credential": secret.clone()}]), true),
    ] {
        let shown = shown_operand(&value, &secret, masked);
        let mut claim = claim_params(&run);
        claim["run_id"] = value.clone();
        refuse_without_writes(
            &f,
            "charter.claim",
            claim,
            &format!("charter.claim: run_id must be a string, got {shown}"),
            &secret,
            masked,
        )
        .await;

        let mut claim = claim_params(&run);
        claim["descriptor"]["expected_head"] = value;
        refuse_without_writes(
            &f,
            "charter.claim",
            claim,
            &format!("charter.claim: descriptor.expected_head must be a string, got {shown}"),
            &secret,
            masked,
        )
        .await;
    }

    refuse_without_writes(
        &f,
        "charter.claim",
        claim_params(&run),
        "charter.claim: AssuranceInsufficient: action_admitted=false, diagnostic_evaluation_committed=false; the charter pack is at the recording milestone and claims are not admitted",
        &secret,
        false,
    )
    .await;
    f.call(
        "charter.cancel",
        json!({"run_id": run, "reason": "ordinary cancellation"}),
    )
    .await;
    assert_eq!(
        f.at(&run).await.0,
        "cancelled",
        "valid string operands still work"
    );
}
