//! Real MCP request transport controls, shared with the integration fixture.
use super::*;

#[tokio::test]
async fn stream_mcp_request_chain_array_and_error_objects() -> anyhow::Result<()> {
    let client = connect().await?;
    let chained = call(&client, "request", json!({"presentation": "verbose", "ops": "stream.append(stream=\"chain\", record=1) | stream.append(stream=\"chain\", record=2) | stream.append(stream=\"chain\", record=3)"})).await?;
    let chained: Value = serde_json::from_str(&first_text(&chained))?;
    let rows = chained["results"].as_array().unwrap();
    assert!(rows.iter().all(|row| row["ok"] == true), "{chained}");
    assert_eq!(rows.iter().map(|row| row["result"]["seq"].as_i64().unwrap()).collect::<Vec<_>>(), vec![1, 2, 3]);
    let array = json!((1..=3).map(|n| json!({"tool": "stream.append", "args": {"stream": "array", "record": n}})).collect::<Vec<_>>()).to_string();
    let array = call(&client, "request", json!({"presentation": "verbose", "ops": array})).await?;
    let array: Value = serde_json::from_str(&first_text(&array))?;
    let mut seqs: Vec<_> = array["results"].as_array().unwrap().iter().map(|row| { assert_eq!(row["ok"], true); row["result"]["seq"].as_i64().unwrap() }).collect();
    seqs.sort_unstable();
    assert_eq!(seqs, vec![1, 2, 3]);
    let refused = call(&client, "request", json!({"presentation": "verbose", "ops": "stream.append(stream=\"chain\", record=null, expected_seq=3)"})).await?;
    let refused: Value = serde_json::from_str(&first_text(&refused))?;
    assert_eq!(refused["results"][0]["ok"], false);
    let error = &refused["results"][0]["error"];
    assert_eq!(error["kind"], "conflict");
    assert_eq!(error["details"], json!({"reason": "seq_conflict", "stream": "chain", "expected_seq": "3", "next_seq": "4"}));
    Ok(())
}

#[tokio::test]
async fn stream_mcp_default_presentation_keeps_exact_record_and_cursor() -> anyhow::Result<()> {
    let client = connect().await?;
    let record = json!([{"id": "aabbccdd-1234-4321-1234-abcdefabcdef", "score": 0.123456789, "created_at": "2026-09-08T00:00:00.123456Z", "empty": [], "null": null, "namespace": "local", "properties": {"namespace": "local"}}, null]);
    let ops = json!([{"tool": "stream.append", "args": {"stream": "payload", "record": record}}]).to_string();
    ok_one(&client, &ops).await?;
    let page = agent_one(&client, "stream.read(stream=\"payload\")").await?;
    assert_eq!(page["entries"][0]["record"], record);
    assert!(page.get("next_after").is_some_and(Value::is_null));
    let empty = agent_one(&client, "stream.read(stream=\"unknown\")").await?;
    assert_eq!(empty, json!({"entries": [], "head_seq": 0, "next_after": null}));
    Ok(())
}
