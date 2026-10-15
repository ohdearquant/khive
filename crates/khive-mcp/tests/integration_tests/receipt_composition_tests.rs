// #2537: transport witnesses collect all legacy controls before precision assertions.
fn sr2537_write(key: &str, atomic: bool, version: Option<i64>) -> String {
    let member = json!({"op":"write", "kind":"head", "key":key,
        "doc":{"revision":version.unwrap_or(0)+1}, "expected_version":version, "embed":false});
    format!("stream.batch(atomic={atomic},ops={})", json!([member]))
}

async fn sr2537_request(
    client: &impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>,
    args: Value,
) -> anyhow::Result<Value> {
    let response = call(client, "request", args).await?;
    let body: Value = serde_json::from_str(&first_text(&response))?;
    assert!(
        body["results"].is_array(),
        "real MCP result envelope: {body}"
    );
    Ok(body)
}

fn sr2537_success(body: &Value, index: usize, tool: &str) -> Value {
    let entry = &body["results"][index];
    assert_eq!(entry["tool"], tool);
    assert_eq!(entry["ok"], true, "dispatch control: {entry}");
    entry["result"].clone()
}

fn sr2537_iso(value: &Value) -> &str {
    let text = value.as_str().expect("canonical timestamp string");
    chrono::DateTime::parse_from_rfc3339(text).expect("canonical timestamp parses");
    assert_eq!(
        text.split('.').nth(1).unwrap().trim_end_matches('Z').len(),
        6
    );
    text
}

async fn sr2537_stored(
    client: &impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>,
    receipt: &Value,
    short: bool,
) -> anyhow::Result<Value> {
    let visible = receipt["id"].as_str().expect("receipt id");
    let stored = ok_one(client, &format!("get(id={})", json!(visible))).await?;
    let full = stored["id"].as_str().unwrap();
    uuid::Uuid::parse_str(full).expect("stored full UUID");
    assert_eq!(visible, if short { &full[..8] } else { full });
    if receipt.get("version").is_some() {
        assert_eq!(receipt["version"], stored["version"]);
    }
    Ok(stored)
}

fn sr2537_precision(pairs: Vec<(String, Value, Value)>, expected: usize) {
    assert_eq!(
        pairs.len(),
        expected,
        "complete predeclared receipt case inventory"
    );
    println!("SR2537 controls complete: {expected} timestamp cases");
    for (case, shown, canonical) in pairs {
        sr2537_iso(&canonical);
        assert_eq!(shown, canonical, "SR2537 precision: {case}");
    }
}

#[tokio::test]
async fn issue_2537_agent_write_receipts_with_compact_metadata() -> anyhow::Result<()> {
    let mut pairs = Vec::new();
    for atomic in [true, false] {
        let client = connect().await?;
        for expected in [None, Some(1)] {
            let body = sr2537_request(&client, json!({"ops":format!("{} | list(kind=\"head\",limit=10)",sr2537_write("receipt",atomic,expected))})).await?;
            assert_eq!(body["results"].as_array().unwrap().len(), 2);
            assert_eq!(body["summary"]["succeeded"], 2);
            let batch = sr2537_success(&body, 0, "stream.batch");
            assert_eq!(batch["committed"], true);
            assert_eq!(batch["results"].as_array().unwrap().len(), 1);
            let receipt = &batch["results"][0];
            assert_eq!(receipt["version"], expected.unwrap_or(0) + 1);
            let stored = sr2537_stored(&client, receipt, true).await?;
            assert_eq!(
                serde_json::from_str::<Value>(stored["content"].as_str().unwrap())?,
                json!({"revision":expected.unwrap_or(0)+1})
            );
            let listed = sr2537_success(&body, 1, "list");
            assert_eq!(listed["items"].as_array().unwrap().len(), 1);
            let row = &listed["items"][0];
            assert_eq!(row["id"], receipt["id"]);
            assert_eq!(row["version"], stored["version"]);
            assert_eq!(
                row["updated_at"], stored["updated_at"],
                "Amendment 9 renders read metadata as exact UTC"
            );
            pairs.push((
                format!("write atomic={atomic} expected={expected:?}"),
                receipt["updated_at"].clone(),
                stored["updated_at"].clone(),
            ));
        }
    }
    sr2537_precision(pairs, 4);
    Ok(())
}

#[tokio::test]
async fn issue_2537_agent_append_receipts_all_producers() -> anyhow::Result<()> {
    let client = connect().await?;
    let mut pairs = Vec::new();
    let one = agent_one(
        &client,
        r#"stream.append(stream="standalone",record={"n":1},embed=false)"#,
    )
    .await?;
    assert_eq!(one["seq"], 1);
    let stored = sr2537_stored(&client, &one, true).await?;
    let page = ok_one(&client, r#"stream.read(stream="standalone",limit=10)"#).await?;
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    assert_eq!(page["entries"][0]["record"], json!({"n":1}));
    assert_eq!(page["entries"][0]["id"], stored["id"]);
    pairs.push((
        "standalone append".into(),
        one["created_at"].clone(),
        stored["created_at"].clone(),
    ));
    for atomic in [true, false] {
        let stream = format!("mixed-{atomic}");
        let ops = json!([{"op":"append","stream":stream,"record":{"n":2},"embed":false},
            {"op":"write","key":format!("head-{atomic}"),"kind":"head","doc":{"n":3},"embed":false}]);
        let batch = agent_one(&client, &format!("stream.batch(atomic={atomic},ops={ops})")).await?;
        assert_eq!(batch["committed"], true);
        assert_eq!(batch["results"].as_array().unwrap().len(), 2);
        assert_eq!(batch["results"][0]["seq"], 1);
        assert_eq!(batch["results"][1]["version"], 1);
        for (index, field) in [(0, "created_at"), (1, "updated_at")] {
            let receipt = &batch["results"][index];
            let stored = sr2537_stored(&client, receipt, true).await?;
            pairs.push((
                format!("mixed atomic={atomic} member={index}"),
                receipt[field].clone(),
                stored[field].clone(),
            ));
        }
        let page = ok_one(&client, &format!("stream.read(stream={})", json!(stream))).await?;
        assert_eq!(page["entries"].as_array().unwrap().len(), 1);
        assert_eq!(page["entries"][0]["record"], json!({"n":2}));
    }
    sr2537_precision(pairs, 5);
    Ok(())
}

#[tokio::test]
async fn issue_2537_receipts_single_parallel_chain() -> anyhow::Result<()> {
    let client = connect().await?;
    let routes = [
        ("single", sr2537_write("single", true, None), 1),
        (
            "parallel",
            format!(
                "[{},{}]",
                sr2537_write("left", false, None),
                sr2537_write("right", false, None)
            ),
            2,
        ),
    ];
    let mut pairs = Vec::new();
    for (route, ops, count) in routes {
        let body = sr2537_request(&client, json!({"ops":ops})).await?;
        assert_eq!(body["results"].as_array().unwrap().len(), count);
        for index in 0..count {
            let batch = sr2537_success(&body, index, "stream.batch");
            assert_eq!(batch["committed"], true);
            assert_eq!(batch["results"].as_array().unwrap().len(), 1);
            let receipt = &batch["results"][0];
            assert_eq!(receipt["version"], 1);
            let stored = sr2537_stored(&client, receipt, true).await?;
            pairs.push((
                format!("{route}-{index}"),
                receipt["updated_at"].clone(),
                stored["updated_at"].clone(),
            ));
        }
    }
    // Nest the whole result: the MCP boundary refuses a bare map-valued $prev argument.
    // Persist the first receipt before a later write supersedes that row.
    let ops = format!(
        "{} | stream.append(stream=\"chain-capture\",record={{\"previous\":$prev}},embed=false) | {}",
        sr2537_write("chain", true, None),
        sr2537_write("chain", true, Some(1))
    );
    let body = sr2537_request(&client, json!({"ops":ops})).await?;
    assert_eq!(
        body["summary"],
        json!({"total":3,"succeeded":3,"failed":0,"aborted":0}),
        "complete chain response: {body}"
    );
    let first = sr2537_success(&body, 0, "stream.batch");
    let middle = sr2537_success(&body, 1, "stream.append");
    let last = sr2537_success(&body, 2, "stream.batch");
    assert_eq!(first["committed"], true);
    assert_eq!(last["committed"], true);
    assert_eq!(first["results"].as_array().unwrap().len(), 1);
    assert_eq!(last["results"].as_array().unwrap().len(), 1);
    assert_eq!(first["results"][0]["version"], 1);
    assert_eq!(last["results"][0]["version"], 2);
    assert_eq!(first["results"][0]["id"], last["results"][0]["id"]);
    assert_eq!(middle["seq"], 1);
    let latest = sr2537_stored(&client, &last["results"][0], true).await?;
    assert_eq!(
        serde_json::from_str::<Value>(latest["content"].as_str().unwrap())?,
        json!({"revision":2})
    );
    let page = ok_one(&client, r#"stream.read(stream="chain-capture")"#).await?;
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    let captured = &page["entries"][0]["record"];
    let first_time = captured["previous"]["results"][0]["updated_at"].clone();
    sr2537_iso(&first_time);
    assert_eq!(
        captured,
        &json!({"previous":{"committed":true,"results":[{"id":latest["id"],"version":1,"updated_at":first_time}]}})
    );
    let append_stored = sr2537_stored(&client, &middle, true).await?;
    pairs.push((
        "chain superseded create".into(),
        first["results"][0]["updated_at"].clone(),
        first_time,
    ));
    pairs.push((
        "chain capture append".into(),
        middle["created_at"].clone(),
        append_stored["created_at"].clone(),
    ));
    pairs.push((
        "chain update".into(),
        last["results"][0]["updated_at"].clone(),
        latest["updated_at"].clone(),
    ));
    sr2537_precision(pairs, 6);
    Ok(())
}

#[tokio::test]
async fn issue_2537_receipt_mode_overrides() -> anyhow::Result<()> {
    let client = connect().await?;
    let mut pairs = Vec::new();
    for (index, mode) in [None, Some("agent"), Some("verbose"), Some("human")]
        .into_iter()
        .enumerate()
    {
        let mut args = json!({"ops":sr2537_write(&format!("mode-{index}"),true,None)});
        if let Some(mode) = mode {
            args["presentation"] = json!(mode);
        }
        let body = sr2537_request(&client, args).await?;
        let batch = sr2537_success(&body, 0, "stream.batch");
        assert_eq!(batch["committed"], true);
        let receipt = &batch["results"][0];
        let stored =
            sr2537_stored(&client, receipt, mode.is_none() || mode == Some("agent")).await?;
        if mode == Some("verbose") || mode == Some("human") {
            assert_eq!(receipt["updated_at"], stored["updated_at"]);
        }
        pairs.push((
            format!("mode-{mode:?}"),
            receipt["updated_at"].clone(),
            stored["updated_at"].clone(),
        ));
    }
    for envelope in ["agent", "verbose"] {
        let ops = format!(
            "[{},{}]",
            sr2537_write(&format!("{envelope}-a"), false, None),
            sr2537_write(&format!("{envelope}-v"), false, None)
        );
        let body = sr2537_request(
            &client,
            json!({"ops":ops,"presentation":envelope,"presentation_per_op":["agent","verbose"]}),
        )
        .await?;
        for index in 0..2 {
            let batch = sr2537_success(&body, index, "stream.batch");
            let receipt = &batch["results"][0];
            let stored = sr2537_stored(&client, receipt, index == 0).await?;
            if index == 1 {
                assert_eq!(receipt["updated_at"], stored["updated_at"]);
            }
            pairs.push((
                format!("override-{envelope}-{index}"),
                receipt["updated_at"].clone(),
                stored["updated_at"].clone(),
            ));
        }
    }
    sr2537_precision(pairs, 8);
    Ok(())
}
