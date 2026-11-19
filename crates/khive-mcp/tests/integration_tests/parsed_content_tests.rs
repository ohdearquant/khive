#[tokio::test]
async fn issue2757_parsed_content_survives_modes_formats_and_note_aliases() -> anyhow::Result<()> {
    let client = connect().await?;
    let bodies = [
        r#"{"id":"11111111-1111-4111-8111-111111111111","created_at":"2026-09-15T12:34:56.123456+03:00","score":0.123456789,"nil":null,"empty":"","array":[],"object":{},"namespace":"local","full_id":"payload","properties":{"namespace":"local"}}"#,
        r#"[{"namespace":"local","full_id":"one","properties":{"full_id":"one"}},{"namespace":"local","full_id":"two"}]"#,
        "42",
        "false",
        "null",
        "[]",
        "{}",
        r#""""#,
        r#""2026-09-15T12:34:56.123456+03:00""#,
    ];
    for (index, body) in bodies.iter().enumerate() {
        let key = format!("issue2757/{index:02}");
        let kind = if index == 0 { "task" } else { "observation" };
        // The task hook derives stored content from description, or title when absent.
        let task_fields = if kind == "task" {
            format!(", title={}, description={}", json!(key), json!(body))
        } else {
            String::new()
        };
        let created = ok_one(
            &client,
            &format!(
                "create(kind={}, key={}, content={}, tags=[{}], skip_dedup_check=true{task_fields})",
                json!(kind),
                json!(key),
                json!(body),
                json!(key),
            ),
        )
        .await?;
        let baseline = ok_one(&client, &format!("get(id={})", created["id"])).await?;
        assert_eq!(
            baseline["content"], *body,
            "fixture stores the intended bytes"
        );
        let expected = serde_json::from_str::<Value>(body)?;
        for mode in ["agent", "human", "verbose"] {
            for format in ["json", "auto", "table"] {
                let ops = format!(
                    "[get(id={id}), get(id={id}, parse_content=false), get(id={id}, parse_content=true), list(kind={kind}, tags=[{key}]), list(kind={kind}, tags=[{key}], parse_content=false), list(kind={kind}, tags=[{key}], parse_content=true)]",
                    id=created["id"], kind=json!(kind), key=json!(key),
                );
                let response = call(
                    &client,
                    "request",
                    json!({
                        "ops":ops, "presentation":mode, "format":format,
                    }),
                )
                .await?;
                let envelope: Value = serde_json::from_str(&first_text(&response))?;
                assert_eq!(envelope["summary"]["succeeded"], 6, "{envelope}");
                let entries = envelope["results"].as_array().unwrap();
                for (omitted, disabled) in [(0, 1), (3, 4)] {
                    assert_eq!(
                        serde_json::to_vec(&entries[omitted]["result"])?,
                        serde_json::to_vec(&entries[disabled]["result"])?,
                        "{mode}/{format} omitted equals false"
                    );
                }
                let expected = if format == "table" && (expected.is_object() || expected.is_array())
                {
                    json!(expected.to_string())
                } else {
                    expected.clone()
                };
                for (index, entry) in [&entries[2], &entries[5]].into_iter().enumerate() {
                    let result = if format == "json" {
                        entry["result"].clone()
                    } else {
                        serde_json::from_str::<Value>(entry["result"].as_str().unwrap())?
                    };
                    let record = if index == 0 {
                        &result
                    } else {
                        &result["items"][0]
                    };
                    assert_eq!(
                        std::mem::discriminant(record.get("content").unwrap()),
                        std::mem::discriminant(&expected),
                        "{mode}/{format} content type"
                    );
                    assert_eq!(
                        record.get("content"),
                        Some(&expected),
                        "{mode}/{format}, body {body}"
                    );
                    if index == 1 {
                        assert_eq!(result["requested_limit"], 20);
                        assert_eq!(result["effective_limit"], 20);
                        assert_eq!(result["limit_clamped"], false);
                    }
                }
            }
        }
        let raw = ok_one(&client, &format!("get(id={})", created["id"])).await?;
        assert_eq!(raw["content"], *body, "reads do not rewrite storage");
    }
    Ok(())
}

#[tokio::test]
async fn issue2757_defaults_per_op_overrides_and_chain_use_canonical_content() -> anyhow::Result<()>
{
    let client = connect().await?;
    let body = " {\n  \"enabled\": true, \"id\": \"11111111-1111-4111-8111-111111111111\"\n} ";
    let note = ok_one(&client, &format!(
        "create(kind=\"observation\", content={}, tags=[\"issue2757-chain\"], skip_dedup_check=true)", json!(body),
    )).await?;
    let ops = format!(
        "[get(id={}), get(id={}, parse_content=false), list(kind=\"note\"), list(kind=\"note\", parse_content=false), list(kind=\"note\", parse_content=true)]",
        note["id"], note["id"],
    );
    for mode in ["agent", "human", "verbose"] {
        let response = call(
            &client,
            "request",
            json!({"ops":ops, "presentation":mode, "format":"json"}),
        )
        .await?;
        let envelope: Value = serde_json::from_str(&first_text(&response))?;
        assert_eq!(envelope["summary"]["succeeded"], 5, "{envelope}");
        let results = envelope["results"].as_array().unwrap();
        assert_eq!(results[0]["result"], results[1]["result"], "{mode} get");
        assert_eq!(results[2]["result"], results[3]["result"], "{mode} list");
        assert_eq!(results[0]["result"]["content"], body);
        assert_eq!(results[2]["result"]["items"][0]["content"], body);
        assert_eq!(
            results[4]["result"]["items"][0]["content"],
            serde_json::from_str::<Value>(body)?
        );
    }

    let response = call(
        &client,
        "request",
        json!({
            "ops":ops, "presentation":"agent", "format":"table",
            "presentation_per_op":["human","verbose","agent","agent","verbose"],
            "format_per_op":["json","json","json","json","auto"],
        }),
    )
    .await?;
    let envelope: Value = serde_json::from_str(&first_text(&response))?;
    let results = envelope["results"].as_array().unwrap();
    assert_eq!(envelope["summary"]["succeeded"], 5, "{envelope}");
    assert_eq!(results[0]["result"]["content"], body);
    assert_eq!(results[1]["result"]["content"], body);
    assert_eq!(results[2]["result"]["items"][0]["content"], body);
    assert_eq!(results[3]["result"]["items"][0]["content"], body);
    let full_id = note["id"].as_str().unwrap();
    assert_eq!(results[2]["result"]["items"][0]["id"], &full_id[..8]);
    let parsed: Value = serde_json::from_str(results[4]["result"].as_str().unwrap())?;
    assert_eq!(parsed["items"][0]["id"], full_id);
    assert_eq!(
        parsed["items"][0]["content"],
        serde_json::from_str::<Value>(body)?
    );

    // The option itself resolves through $prev. The next get uses the full ID
    // from canonical list output even though the returned list is in Agent mode.
    let ops = format!(
        "get(id={}, parse_content=true) | list(kind=\"observation\", tags=[\"issue2757-chain\"], parse_content=$prev.content.enabled) | get(id=$prev.items[0].id, parse_content=true)",
        note["id"],
    );
    let response = call(&client, "request", json!({"ops":ops})).await?;
    let envelope: Value = serde_json::from_str(&first_text(&response))?;
    assert_eq!(envelope["summary"]["succeeded"], 3, "{envelope}");
    let payload: Value = serde_json::from_str(body)?;
    assert_eq!(
        envelope["results"][1]["result"]["items"][0]["content"],
        payload
    );
    assert_eq!(envelope["results"][2]["result"]["content"], payload);
    Ok(())
}

#[tokio::test]
async fn issue2757_table_envelopes_and_non_note_payloads_keep_their_contracts() -> anyhow::Result<()>
{
    let client = connect().await?;
    let mut ids = Vec::new();
    for i in 0..2 {
        let note = ok_one(
            &client,
            &format!(
                "create(kind=\"observation\", key={}, content={}, skip_dedup_check=true)",
                json!(format!("issue2757-page/{i}")),
                json!(json!({"value":i}).to_string()),
            ),
        )
        .await?;
        ids.push(note["id"].clone());
    }
    // One payload, paired formats: JSON retains an object, Table displays its JSON string.
    let response = call(&client, "request", json!({
        "ops":format!("[get(id={}, parse_content=true), get(id={}, parse_content=true)]", ids[0], ids[0]),
        "format_per_op":["json","table"],
    })).await?;
    let envelope: Value = serde_json::from_str(&first_text(&response))?;
    assert_eq!(envelope["summary"]["succeeded"], 2, "{envelope}");
    let json_record = &envelope["results"][0]["result"];
    let table_record: Value =
        serde_json::from_str(envelope["results"][1]["result"].as_str().unwrap())?;
    assert!(json_record["content"].is_object());
    assert!(table_record["content"].is_string());
    assert_eq!(
        serde_json::from_str::<Value>(table_record["content"].as_str().unwrap())?,
        json_record["content"]
    );
    let mut expected_table = json_record.clone();
    expected_table["content"] = json!(json_record["content"].to_string());
    assert_eq!(table_record, expected_table);

    for mode in ["agent", "human", "verbose"] {
        for format in ["json", "auto", "table"] {
            let response = call(
                &client,
                "request",
                json!({
                    "ops":"list(kind=\"note\", parse_content=true, limit=2)",
                    "presentation":mode, "format":format,
                }),
            )
            .await?;
            let envelope: Value = serde_json::from_str(&first_text(&response))?;
            assert_eq!(envelope["summary"]["succeeded"], 1, "{envelope}");
            if format == "table" {
                let table = envelope["results"][0]["result"].as_str().unwrap();
                assert!(table.starts_with('|'), "{table}");
                for content in [r#"{"value":0}"#, r#"{"value":1}"#] {
                    assert!(table.contains(content), "serialized JSON cell: {table}");
                }
                for metadata in [
                    "requested_limit: 2",
                    "effective_limit: 2",
                    "limit_clamped: false",
                ] {
                    assert!(table.contains(metadata), "{table}");
                }
            } else {
                let page = if format == "json" {
                    envelope["results"][0]["result"].clone()
                } else {
                    serde_json::from_str(envelope["results"][0]["result"].as_str().unwrap())?
                };
                assert_eq!(page["items"].as_array().unwrap().len(), 2);
                assert!(page["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|note| note["content"].is_object()));
                assert_eq!(page["requested_limit"], 2);
                assert_eq!(page["effective_limit"], 2);
                assert_eq!(page["limit_clamped"], false);
            }
        }
    }
    // Real continuation tokens, both insertion-order and keyed cursor paths.
    for keyed in [false, true] {
        let mut cursor = String::new();
        for page_index in 0..2 {
            let key_filter = if keyed {
                ", key_prefix=\"issue2757-page/\""
            } else {
                ""
            };
            let args = format!(
                "kind=\"observation\", limit=1, after={}{key_filter}",
                json!(cursor)
            );
            let response = call(&client, "request", json!({
                "ops":format!("[list({args}), list({args}, parse_content=false), list({args}, parse_content=true)]"),
                "presentation":"verbose", "format":"json",
            })).await?;
            let envelope: Value = serde_json::from_str(&first_text(&response))?;
            assert_eq!(envelope["summary"]["succeeded"], 3, "{envelope}");
            let entries = envelope["results"].as_array().unwrap();
            assert_eq!(
                serde_json::to_vec(&entries[0])?,
                serde_json::to_vec(&entries[1])?
            );
            let mut expected = entries[1].clone();
            assert_eq!(expected["result"]["notes"].as_array().unwrap().len(), 1);
            let raw = expected["result"]["notes"][0]["content"].as_str().unwrap();
            expected["result"]["notes"][0]["content"] = serde_json::from_str(raw)?;
            assert_eq!(
                entries[2], expected,
                "only content changes; envelope and pagination are identical"
            );
            let next = &entries[2]["result"]["next_after"];
            if page_index == 0 {
                cursor = next
                    .as_str()
                    .expect("first page has a continuation")
                    .to_owned();
            } else {
                assert!(next.is_null(), "last page has no continuation");
            }
        }
    }
    let entity = ok_one(&client, r#"create(kind="concept", name="issue2757 entity", properties={"content":"null","parse_content":true}, skip_dedup_check=true)"#).await?;
    let response = call(&client, "request", json!({"ops":format!(
        "[get(id={}), get(id={}, parse_content=true), list(kind=\"concept\"), list(kind=\"concept\", parse_content=true)]",
        entity["id"], entity["id"],
    )})).await?;
    let envelope: Value = serde_json::from_str(&first_text(&response))?;
    assert_eq!(envelope["summary"]["succeeded"], 4, "{envelope}");
    assert_eq!(
        envelope["results"][0]["result"],
        envelope["results"][1]["result"]
    );
    assert_eq!(
        envelope["results"][2]["result"],
        envelope["results"][3]["result"]
    );
    Ok(())
}

#[tokio::test]
async fn issue2757_invalid_content_missing_note_and_unknown_fields_are_distinct(
) -> anyhow::Result<()> {
    let client = connect().await?;
    let missing = "ffffffff-ffff-4fff-8fff-ffffffffffff";
    let response = call(
        &client,
        "request",
        json!({"ops":format!("get(id={missing:?}, parse_content=true)")}),
    )
    .await?;
    let missing_envelope: Value = serde_json::from_str(&first_text(&response))?;
    let missing_error = &missing_envelope["results"][0]["error"];
    assert_eq!(missing_envelope["results"][0]["ok"], false);
    assert_eq!(missing_error["kind"], "not_found");
    assert!(missing_error["message"]
        .as_str()
        .unwrap()
        .contains("not found"));
    for (index, body) in ["not JSON", "{broken", ""].into_iter().enumerate() {
        let key = format!("issue2757-invalid/{index}");
        let note = ok_one(
            &client,
            &format!(
            "create(kind=\"observation\", key={}, tags=[{}], content={}, skip_dedup_check=true)",
            json!(key), json!(key), json!(body),
        ),
        )
        .await?;
        for op in [
            format!("get(id={})", note["id"]),
            format!("get(key={})", json!(key)),
            format!("list(kind=\"note\", tags=[{}])", json!(key)),
            format!(
                "list(kind=\"observation\", tags=[{}], after=\"\")",
                json!(key)
            ),
            format!("list(kind=\"note\", key_prefix={})", json!(key)),
            format!(
                "list(kind=\"observation\", key_prefix={}, offset=0)",
                json!(key)
            ),
        ] {
            let args = op.strip_suffix(')').unwrap();
            let response = call(&client, "request", json!({
                "ops":format!("[{op}, {args}, parse_content=false), {args}, parse_content=true)]"),
                "presentation":"verbose",
            })).await?;
            let envelope: Value = serde_json::from_str(&first_text(&response))?;
            assert_eq!(envelope["summary"]["succeeded"], 2, "{envelope}");
            assert_eq!(envelope["summary"]["failed"], 1, "{envelope}");
            let entries = envelope["results"].as_array().unwrap();
            assert_eq!(
                serde_json::to_vec(&entries[0])?,
                serde_json::to_vec(&entries[1])?
            );
            assert_eq!(entries[2]["ok"], false);
            let error = &entries[2]["error"];
            assert_eq!(error["kind"], "invalid_input");
            let message = error["message"].as_str().unwrap();
            assert!(
                message.contains(note["id"].as_str().unwrap()) && message.contains("content"),
                "{message}"
            );
            assert_ne!(error["kind"], missing_error["kind"]);
            assert_ne!(error["message"], missing_error["message"]);
        }
    }
    let valid = ok_one(&client, r#"create(kind="observation", content="null", tags=["issue2757-valid"], skip_dedup_check=true)"#).await?;
    for op in [
        format!("get(id={})", valid["id"]),
        r#"list(kind="note", tags=["issue2757-valid"])"#.to_owned(),
    ] {
        let args = op.strip_suffix(')').unwrap();
        let response = call(&client, "request", json!({
            "ops":format!("[{op}, {args}, parse_content=false), {args}, parse_content=true, unknown_parse_option=true)]"),
        })).await?;
        let envelope: Value = serde_json::from_str(&first_text(&response))?;
        let entries = envelope["results"].as_array().unwrap();
        assert_eq!(envelope["summary"]["succeeded"], 2, "{envelope}");
        assert_eq!(envelope["summary"]["failed"], 1, "{envelope}");
        assert_eq!(
            serde_json::to_vec(&entries[0])?,
            serde_json::to_vec(&entries[1])?
        );
        assert_eq!(entries[2]["ok"], false);
        assert!(entries[2]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown_parse_option"));
    }
    Ok(())
}
