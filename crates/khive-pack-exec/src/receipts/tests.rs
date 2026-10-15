use super::*;
use khive_storage::types::SqlColumn;

#[test]
fn decode_preserves_receipts_without_effective_output_cap() {
    let receipt = json!({
        "id": "historical-receipt",
        "actor": "local",
        "tool": "sh",
        "stdout_produced_bytes": 200,
        "stdout_retained_bytes": 128,
        "stdout_capture": "incomplete",
        "seq": null,
    });
    let row = SqlRow {
        columns: vec![
            SqlColumn {
                name: "receipt".into(),
                value: SqlValue::Text(receipt.to_string()),
            },
            SqlColumn {
                name: "seq".into(),
                value: SqlValue::Null,
            },
        ],
    };
    let decoded = decode(&row).expect("historical receipt decodes");
    assert_eq!(decoded, receipt);
    assert!(decoded.get("effective_max_output_bytes").is_none());
}
