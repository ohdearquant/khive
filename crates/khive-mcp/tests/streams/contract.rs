//! Real MCP request transport controls, shared with the integration fixture.
use super::*;

#[tokio::test]
async fn stream_mcp_request_chain_array_and_error_objects() -> anyhow::Result<()> {
    let client = connect().await?;
    let chained = call(&client, "request", json!({"presentation": "verbose", "ops": "stream.append(stream=\"chain\", record=1) | stream.append(stream=\"chain\", record=2) | stream.append(stream=\"chain\", record=3)"})).await?;
    let chained: Value = serde_json::from_str(&first_text(&chained))?;
    let rows = chained["results"].as_array().unwrap();
    assert!(rows.iter().all(|row| row["ok"] == true), "{chained}");
    assert_eq!(
        rows.iter()
            .map(|row| row["result"]["seq"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    let array = json!((1..=3)
        .map(|n| json!({"tool": "stream.append", "args": {"stream": "array", "record": n}}))
        .collect::<Vec<_>>())
    .to_string();
    let array = call(
        &client,
        "request",
        json!({"presentation": "verbose", "ops": array}),
    )
    .await?;
    let array: Value = serde_json::from_str(&first_text(&array))?;
    let mut seqs: Vec<_> = array["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            assert_eq!(row["ok"], true, "{array}");
            row["result"]["seq"].as_i64().unwrap()
        })
        .collect();
    seqs.sort_unstable();
    assert_eq!(seqs, vec![1, 2, 3]);
    let refused = call(&client, "request", json!({"presentation": "verbose", "ops": "stream.append(stream=\"chain\", record=null, expected_seq=3)"})).await?;
    let refused: Value = serde_json::from_str(&first_text(&refused))?;
    assert_eq!(refused["results"][0]["ok"], false);
    let error = &refused["results"][0]["error"];
    assert_eq!(error["kind"], "conflict");
    assert_eq!(
        error["details"],
        json!({"reason": "seq_conflict", "stream": "chain", "expected_seq": "3", "next_seq": "4"})
    );
    Ok(())
}

#[tokio::test]
async fn stream_mcp_default_presentation_keeps_exact_record_and_cursor() -> anyhow::Result<()> {
    let client = connect().await?;
    let record = json!([{"id": "aabbccdd-1234-4321-1234-abcdefabcdef", "score": 0.123456789, "created_at": "2026-09-08T00:00:00.123456Z", "empty": [], "null": null, "namespace": "local", "properties": {"namespace": "local"}}, null]);
    let ops = json!([{"tool": "stream.append", "args": {"stream": "payload", "record": record}}])
        .to_string();
    ok_one(&client, &ops).await?;
    let page = agent_one(&client, "stream.read(stream=\"payload\")").await?;
    assert_eq!(page["entries"][0]["record"], record);
    assert!(page.get("next_after").is_some_and(Value::is_null));
    let empty = agent_one(&client, "stream.read(stream=\"unknown\")").await?;
    assert_eq!(
        empty,
        json!({"entries": [], "head_seq": 0, "next_after": null})
    );
    Ok(())
}

#[tokio::test]
async fn stream_batch_atomic_refusal_carries_not_committed_on_the_wire() -> anyhow::Result<()> {
    // ADR-174 A1.1: a member refusal carries `domain_disposition:
    // not_committed` wherever it surfaces. Per-member mode returns it as the
    // member's own value; atomic mode raises it as the call's error, and the
    // dispatch boundary's own answer for any handler error is `unknown`, so
    // without the refusal naming its own disposition the caller cannot tell a
    // batch that wrote nothing from one whose outcome is unestablished.
    let client = connect().await?;
    let stale = json!([{"tool": "stream.batch", "args": {"ops": [
        {"op": "append", "stream": "atomic", "record": 1},
        {"op": "append", "stream": "atomic", "record": 2, "expected_seq": 9},
    ], "atomic": true}}])
    .to_string();
    let unknown_op =
        json!([{"tool": "stream.batch", "args": {"ops": [{"op": "nope"}], "atomic": true}}])
            .to_string();
    for (ops, reason, member) in [
        (stale, "seq_conflict", "1"),
        (unknown_op, "unknown_op", "0"),
    ] {
        let refused = call(
            &client,
            "request",
            json!({"presentation": "verbose", "ops": ops}),
        )
        .await?;
        let refused: Value = serde_json::from_str(&first_text(&refused))?;
        assert_eq!(refused["results"][0]["ok"], false, "{refused}");
        let error = &refused["results"][0]["error"];
        assert_eq!(error["details"]["reason"], reason, "{refused}");
        assert_eq!(error["details"]["member"], member, "{refused}");
        assert_eq!(error["domain_disposition"], "not_committed", "{refused}");
    }
    // The refusal's claim, read back: the whole batch wrote nothing.
    let page = agent_one(&client, "stream.read(stream=\"atomic\")").await?;
    assert_eq!(page["head_seq"], 0, "{page}");
    Ok(())
}

#[tokio::test]
async fn stream_batch_predicates_preserve_refusal_details_on_the_wire() -> anyhow::Result<()> {
    let client = connect().await?;
    let seeded = ok_one(
        &client,
        &json!([{"tool": "stream.batch", "args": {"ops": [
            {"op": "write", "key": "guard", "kind": "head", "doc": {"held": true}},
        ]}}])
        .to_string(),
    )
    .await?;
    assert_eq!(seeded["results"][0]["version"].as_i64(), Some(1));
    let committed = ok_one(
        &client,
        &json!([{"tool": "stream.batch", "args": {
            "fence": {"key": "guard", "kind": "head", "expected_version": 1},
            "observed": [
                {"key": "guard", "kind": "head", "version": 1},
                {"key": "unheld", "kind": "head", "version": null},
            ],
            "ops": [{"op": "append", "stream": "predicates", "record": 1}],
        }}])
        .to_string(),
    )
    .await?;
    assert_eq!(committed["committed"], true, "{committed}");
    assert_eq!(committed["results"][0]["seq"], 1, "{committed}");

    for (predicate, expected) in [
        (
            json!({"fence": {"key": "guard", "kind": "head", "expected_version": 2}}),
            json!({"reason": "fence_conflict", "key": "guard", "expected_version": "2", "current_version": "1"}),
        ),
        (
            json!({"fence": {"key": "missing", "kind": "head", "expected_version": 1}}),
            json!({"reason": "fence_conflict", "key": "missing", "expected_version": "1"}),
        ),
        (
            json!({"observed": [
                {"key": "unheld", "kind": "head", "version": null},
                {"key": "guard", "kind": "head", "version": 2},
            ]}),
            json!({"reason": "version_conflict", "key": "guard", "expected_version": "2", "current_version": "1", "index": "1"}),
        ),
        (
            json!({"observed": [{"key": "missing", "kind": "head", "version": 1}]}),
            json!({"reason": "version_conflict", "key": "missing", "expected_version": "1", "index": "0"}),
        ),
        (
            json!({"observed": [{"key": "guard", "kind": "head", "version": null}]}),
            json!({"reason": "version_conflict", "key": "guard", "current_version": "1", "index": "0"}),
        ),
    ] {
        let mut args = predicate;
        args["atomic"] = json!(true);
        args["ops"] = json!([
            {"op": "append", "stream": "predicates", "record": 2},
            {"op": "write", "key": "guard", "kind": "head", "doc": {"held": false}, "expected_version": 1},
        ]);
        let refused = call(
            &client,
            "request",
            json!({"presentation": "verbose", "ops": json!([
                {"tool": "stream.batch", "args": args},
            ]).to_string()}),
        )
        .await?;
        let refused: Value = serde_json::from_str(&first_text(&refused))?;
        assert_eq!(refused["results"][0]["ok"], false, "{refused}");
        let error = &refused["results"][0]["error"];
        assert_eq!(error["kind"], "conflict", "{refused}");
        assert_eq!(error["domain_disposition"], "not_committed", "{refused}");
        for (key, value) in expected.as_object().unwrap() {
            assert_eq!(&error["details"][key], value, "{refused}");
        }
        assert!(error["details"].get("member").is_none(), "{refused}");
        let page = ok_one(&client, r#"stream.read(stream="predicates")"#).await?;
        assert_eq!(page["head_seq"], 1, "{page}");
        assert_eq!(page["entries"].as_array().unwrap().len(), 1, "{page}");
        let holder = ok_one(&client, r#"get(key="guard", kind="head")"#).await?;
        assert_eq!(holder["version"].as_i64(), Some(1), "{holder}");
        assert_eq!(
            serde_json::from_str::<Value>(holder["content"].as_str().unwrap())?,
            json!({"held": true}),
        );
    }
    Ok(())
}

#[tokio::test]
async fn stream_batch_write_refusals_follow_mode_on_the_wire() -> anyhow::Result<()> {
    for atomic in [true, false] {
        for (failing_write, kind, reason) in [
            (
                json!({"op": "write", "key": "held", "kind": "head", "doc": null}),
                "conflict",
                "key_conflict",
            ),
            (
                json!({"op": "write", "key": "held", "kind": "head", "doc": null, "expected_version": null}),
                "conflict",
                "key_conflict",
            ),
            (
                json!({"op": "write", "key": "held", "kind": "head", "doc": null, "expected_version": 1}),
                "conflict",
                "version_conflict",
            ),
            (
                json!({"op": "write", "key": "missing", "kind": "head", "doc": null, "expected_version": 1}),
                "not_found",
                "stream_write_not_found",
            ),
        ] {
            let client = connect().await?;
            let seeded = ok_one(
                &client,
                &json!([{"tool": "stream.batch", "args": {"atomic": atomic, "ops": [
                    {"op": "write", "key": "held", "kind": "head", "doc": 1, "expected_version": null},
                ]}}])
                .to_string(),
            )
            .await?;
            let holder_id = seeded["results"][0]["id"].as_str().unwrap();
            assert_eq!(seeded["results"][0]["version"].as_i64(), Some(1));
            let updated = ok_one(
                &client,
                &json!([{"tool": "stream.batch", "args": {"atomic": atomic, "ops": [
                    {"op": "write", "key": "held", "kind": "head", "doc": 2, "expected_version": 1},
                ]}}])
                .to_string(),
            )
            .await?;
            assert_eq!(updated["results"][0]["id"], holder_id, "{updated}");
            assert_eq!(updated["results"][0]["version"].as_i64(), Some(2));

            let response = call(
                &client,
                "request",
                json!({"presentation": "verbose", "ops": json!([
                    {"tool": "stream.batch", "args": {"atomic": atomic, "ops": [
                        {"op": "write", "key": "candidate", "kind": "head", "doc": {"created": true}},
                        {"op": "append", "stream": "writes", "record": 1},
                        failing_write,
                        {"op": "append", "stream": "writes", "record": 2},
                    ]}},
                ]).to_string()}),
            )
            .await?;
            let response: Value = serde_json::from_str(&first_text(&response))?;
            let row = &response["results"][0];
            let error = if atomic {
                assert_eq!(row["ok"], false, "{response}");
                assert_eq!(row["error"]["details"]["member"], "2", "{response}");
                &row["error"]
            } else {
                assert_eq!(row["ok"], true, "{response}");
                assert_eq!(row["result"]["committed"], true, "{response}");
                let members = row["result"]["results"].as_array().unwrap();
                assert_eq!(members.len(), 4, "{response}");
                assert_eq!(members[0]["version"].as_i64(), Some(1), "{response}");
                assert_eq!(members[1]["seq"], 1, "{response}");
                assert_eq!(members[3]["seq"], 2, "{response}");
                &members[2]
            };
            assert_eq!(error["kind"], kind, "{response}");
            assert_eq!(error["details"]["reason"], reason, "{response}");
            assert_eq!(error["domain_disposition"], "not_committed", "{response}");
            if reason == "key_conflict" {
                assert_eq!(error["details"]["key"], "held", "{response}");
                assert_eq!(error["details"]["existing_id"], holder_id, "{response}");
            } else if reason == "version_conflict" {
                assert_eq!(error["details"]["expected_version"], "1", "{response}");
                assert_eq!(error["details"]["current_version"], "2", "{response}");
            } else {
                assert_eq!(error["details"]["key"], "missing", "{response}");
            }

            let page = ok_one(&client, r#"stream.read(stream="writes")"#).await?;
            assert_eq!(page["head_seq"], if atomic { 0 } else { 2 }, "{page}");
            assert_eq!(
                page["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|entry| entry["record"].clone())
                    .collect::<Vec<_>>(),
                if atomic {
                    vec![]
                } else {
                    vec![json!(1), json!(2)]
                },
                "{page}",
            );
            let holder = ok_one(&client, r#"get(key="held", kind="head")"#).await?;
            assert_eq!(holder["version"].as_i64(), Some(2), "{holder}");
            assert_eq!(holder["content"], "2", "{holder}");

            let candidate = call(
                &client,
                "request",
                json!({"presentation": "verbose", "ops": r#"get(key="candidate", kind="head")"#}),
            )
            .await?;
            let candidate: Value = serde_json::from_str(&first_text(&candidate))?;
            let candidate = &candidate["results"][0];
            assert_eq!(candidate["ok"], !atomic, "{candidate}");
            if atomic {
                assert_eq!(candidate["error"]["kind"], "not_found", "{candidate}");
                assert_eq!(
                    candidate["error"]["domain_disposition"], "unknown",
                    "ordinary keyed lookup must retain its own disposition: {candidate}",
                );
            } else {
                assert_eq!(candidate["result"]["version"].as_i64(), Some(1));
            }
            let missing = call(
                &client,
                "request",
                json!({"presentation": "verbose", "ops": r#"get(key="missing", kind="head")"#}),
            )
            .await?;
            let missing: Value = serde_json::from_str(&first_text(&missing))?;
            assert_eq!(missing["results"][0]["ok"], false, "{missing}");
            assert_eq!(
                missing["results"][0]["error"]["kind"], "not_found",
                "{missing}"
            );
            assert_eq!(
                missing["results"][0]["error"]["domain_disposition"], "unknown",
                "ordinary not_found is not a batch refusal: {missing}",
            );
        }
    }
    Ok(())
}
