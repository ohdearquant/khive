use super::*;
use serde_json::json;

#[test]
fn stream_agent_json_preserves_opaque_record_and_empty_page() {
    let record = json!([{"id": "aabbccdd-1234-4321-1234-abcdefabcdef", "score": 0.123456789, "created_at": "2026-09-08T00:00:00.123456Z", "empty": [], "null": null, "namespace": "local", "properties": {"namespace": "local"}}]);
    for payload in [record, Value::Null, json!([]), json!({}), json!("")] {
        let page = json!({"entries": [{"seq": 1, "id": "aabbccdd-1234-4321-1234-abcdefabcdef", "created_at": "2026-09-08T00:00:00.123456Z", "record": payload}], "head_seq": 1, "next_after": null});
        let presented = present(page, PresentationMode::Agent, 0);
        let out = prepare_format_value(presented, OutputFormat::Json, PresentationMode::Agent);
        assert_eq!(out["entries"][0].get("record"), Some(&payload));
        assert!(out.get("next_after").is_some_and(Value::is_null));
    }
    let empty = json!({"entries": [], "head_seq": 0, "next_after": null});
    assert_eq!(present(empty.clone(), PresentationMode::Agent, 0), empty);
}
