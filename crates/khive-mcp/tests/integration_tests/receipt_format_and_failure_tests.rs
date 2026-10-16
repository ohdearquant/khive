#[tokio::test]
async fn issue_2537_receipt_formats() -> anyhow::Result<()> {
    let client = connect().await?;
    let mut pairs = Vec::new();
    for mode in ["agent", "verbose", "human"] {
        for format in ["json", "auto", "table"] {
            let stream = format!("format-{mode}-{format}");
            let body=sr2537_request(&client,json!({"ops":format!("stream.append(stream={},record={{\"format\":true}},embed=false)",json!(stream)),"presentation":mode,"format":format})).await?;
            let shown = sr2537_success(&body, 0, "stream.append");
            let scalar = if format == "json" {
                assert!(shown.is_object());
                shown.clone()
            } else {
                assert!(shown.is_string());
                serde_json::from_str::<Value>(shown.as_str().unwrap())?
            };
            assert_eq!(scalar["seq"], 1);
            let stored = sr2537_stored(&client, &scalar, mode == "agent").await?;
            pairs.push((
                format!("{mode}/{format}"),
                scalar["created_at"].clone(),
                stored["created_at"].clone(),
            ));
        }
    }
    let body=sr2537_request(&client,json!({"ops":r#"[stream.append(stream="override-json",record=1,embed=false),stream.append(stream="override-table",record=2,embed=false)]"#,"format":"auto","format_per_op":["json","table"]})).await?;
    for index in 0..2 {
        let shown = sr2537_success(&body, index, "stream.append");
        let scalar = if index == 0 {
            assert!(shown.is_object());
            shown
        } else {
            assert!(shown.is_string());
            serde_json::from_str::<Value>(shown.as_str().unwrap())?
        };
        assert_eq!(scalar["seq"], 1);
        let stored = sr2537_stored(&client, &scalar, true).await?;
        pairs.push((
            format!("format override {index}"),
            scalar["created_at"].clone(),
            stored["created_at"].clone(),
        ));
    }
    for mode in ["agent", "verbose", "human"] {
        for format in ["json", "auto", "table"] {
            let stream = format!("rows-{mode}-{format}");
            let ops = json!([{"op":"append","stream":stream,"record":1,"embed":false},
                {"op":"append","stream":stream,"record":2,"embed":false}]);
            let body=sr2537_request(&client,json!({"ops":format!("stream.batch(atomic=true,ops={ops})"),"presentation":mode,"format":format})).await?;
            let shown = sr2537_success(&body, 0, "stream.batch");
            let page = ok_one(&client, &format!("stream.read(stream={})", json!(stream))).await?;
            let entries = page["entries"].as_array().unwrap();
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0]["record"], 1);
            assert_eq!(entries[1]["record"], 2);
            let receipts = if format == "json" {
                assert_eq!(shown["committed"], true);
                shown["results"].as_array().unwrap().clone()
            } else {
                let text = shown.as_str().expect("real table result");
                assert!(text.ends_with("committed: true\n"));
                let lines: Vec<_> = text.lines().filter(|line| line.starts_with('|')).collect();
                assert_eq!(lines.len(), 4, "header, separator, two rows: {text}");
                let cells = |line: &str| {
                    line.split('|')
                        .skip(1)
                        .take(3)
                        .map(|cell| cell.trim().to_owned())
                        .collect::<Vec<_>>()
                };
                let headers = cells(lines[0]);
                assert_eq!(headers.len(), 3);
                for key in ["id", "seq", "created_at"] {
                    assert!(headers.iter().any(|header| header == key));
                }
                lines[2..]
                    .iter()
                    .map(|line| {
                        let row = cells(line);
                        let mut obj = serde_json::Map::new();
                        for (key, cell) in headers.iter().zip(row) {
                            obj.insert(
                                key.clone(),
                                if key == "seq" {
                                    json!(cell.parse::<i64>().unwrap())
                                } else {
                                    json!(cell)
                                },
                            );
                        }
                        Value::Object(obj)
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(receipts.len(), 2);
            for index in 0..2 {
                assert_eq!(receipts[index]["seq"], index + 1);
                let full = entries[index]["id"].as_str().unwrap();
                uuid::Uuid::parse_str(full).unwrap();
                assert_eq!(
                    receipts[index]["id"].as_str().unwrap(),
                    if mode == "agent" { &full[..8] } else { full }
                );
                pairs.push((
                    format!("row {mode}/{format}/{index}"),
                    receipts[index]["created_at"].clone(),
                    entries[index]["created_at"].clone(),
                ));
            }
        }
    }
    sr2537_precision(pairs, 29);
    Ok(())
}

#[tokio::test]
async fn issue_2537_receipt_errors_partial_members() -> anyhow::Result<()> {
    let client = connect().await?;
    let bad = json!([{"op":"append","stream":"refused","record":0,"expected_seq":9,"embed":false},
        {"op":"append","stream":"survivor","record":1,"embed":false},
        {"op":"write","kind":"head","key":"survivor-head","doc":{"ok":true},"embed":false}]);
    let rejected = sr2537_request(
        &client,
        json!({"ops":format!("stream.batch(atomic=true,ops={bad})")}),
    )
    .await?;
    assert_eq!(rejected["results"][0]["ok"], false);
    assert!(rejected["results"][0]["error"].is_object());
    for stream in ["refused", "survivor"] {
        let page = ok_one(&client, &format!("stream.read(stream={})", json!(stream))).await?;
        assert_eq!(page["entries"].as_array().unwrap().len(), 0);
    }
    let empty = ok_one(&client, r#"list(kind="head")"#).await?;
    assert_eq!(empty["items"].as_array().unwrap().len(), 0);
    let body = sr2537_request(
        &client,
        json!({"ops":format!("stream.batch(atomic=false,ops={bad})")}),
    )
    .await?;
    let batch = sr2537_success(&body, 0, "stream.batch");
    assert_eq!(batch["committed"], true);
    assert_eq!(batch["results"].as_array().unwrap().len(), 3);
    assert_eq!(batch["results"][0]["details"]["reason"], "seq_conflict");
    assert_eq!(batch["results"][1]["seq"], 1);
    assert_eq!(batch["results"][2]["version"], 1);
    let canonical_client = connect().await?;
    let canonical_rejected = sr2537_request(
        &canonical_client,
        json!({"ops":format!("stream.batch(atomic=true,ops={bad})"),"presentation":"verbose"}),
    )
    .await?;
    assert_eq!(
        rejected["results"][0]["error"],
        canonical_rejected["results"][0]["error"]
    );
    let canonical_partial = ok_one(
        &canonical_client,
        &format!("stream.batch(atomic=false,ops={bad})"),
    )
    .await?;
    let mut expected_refusal = canonical_partial["results"][0].clone();
    assert_eq!(
        expected_refusal.as_object_mut().unwrap().remove("code"),
        Some(Value::Null),
        "Agent omits the existing non-lifecycle null code field"
    );
    assert_eq!(
        batch["results"][0], expected_refusal,
        "whole nested refusal retains every other canonical field"
    );
    let help = agent_one(&client, r#"stream.batch(help=true)"#).await?;
    assert!(help.to_string().contains("atomic"));
    assert!(help.get("updated_at").is_none());
    let mut pairs = Vec::new();
    for (index, field) in [(1, "created_at"), (2, "updated_at")] {
        let receipt = &batch["results"][index];
        let stored = sr2537_stored(&client, receipt, true).await?;
        pairs.push((
            format!("partial member {index}"),
            receipt[field].clone(),
            stored[field].clone(),
        ));
    }
    sr2537_precision(pairs, 2);
    Ok(())
}

#[tokio::test]
async fn issue_2537_prev_canonical_receipt() -> anyhow::Result<()> {
    let client = connect().await?;
    let mut pairs = Vec::new();
    for whole in [true, false] {
        let key = format!("prev-{whole}");
        let record = if whole {
            "{\"previous\":$prev}"
        } else {
            "{\"time\":$prev.results[0].updated_at}"
        };
        let ops = format!(
            "{} | stream.append(stream={},record={record},embed=false)",
            sr2537_write(&key, true, None),
            json!(key)
        );
        let body = sr2537_request(&client, json!({"ops":ops})).await?;
        assert_eq!(
            body["summary"],
            json!({"total":2,"succeeded":2,"failed":0,"aborted":0}),
            "complete chain response: {body}"
        );
        let batch = sr2537_success(&body, 0, "stream.batch");
        let appended = sr2537_success(&body, 1, "stream.append");
        assert_eq!(appended["seq"], 1);
        let receipt = &batch["results"][0];
        let stored = sr2537_stored(&client, receipt, true).await?;
        let page = ok_one(&client, &format!("stream.read(stream={})", json!(key))).await?;
        assert_eq!(page["entries"].as_array().unwrap().len(), 1);
        let captured = &page["entries"][0]["record"];
        if whole {
            assert_eq!(
                captured,
                &json!({"previous":{"committed":true,"results":[{"id":stored["id"],"version":1,"updated_at":stored["updated_at"]}]}})
            );
        } else {
            assert_eq!(captured, &json!({"time":stored["updated_at"]}));
        }
        pairs.push((
            format!("canonical prev whole={whole}"),
            receipt["updated_at"].clone(),
            stored["updated_at"].clone(),
        ));
    }
    sr2537_precision(pairs, 2);
    Ok(())
}
