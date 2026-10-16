use khive_vcs_adapters::{AdapterError, CsvFormatAdapter, DelimitedFormat, FormatAdapter};
use serde_json::json;

#[test]
fn csv_preserves_quoted_multiline_fields_and_uses_default_kind() {
    let source = "id,Name,description,year\r\n11111111-1111-1111-1111-111111111111,\"Alpha, beta\",\"First line\nSecond \"\"quoted\"\" line\",2026\r\n,東京,,2025\r\n";
    let mut adapter =
        CsvFormatAdapter::new(source, DelimitedFormat::Csv, Some("concept"), &[]).unwrap();
    assert_eq!(adapter.name(), "csv");
    let entities = adapter.entities().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(entities.len(), 2);
    assert_eq!(
        entities[0].id.to_string(),
        "11111111-1111-1111-1111-111111111111"
    );
    assert_eq!(entities[0].name, "Alpha, beta");
    assert_eq!(
        entities[0].description.as_deref(),
        Some("First line\nSecond \"quoted\" line")
    );
    assert_eq!(entities[0].properties, json!({"year":"2026"}));
    assert_eq!(entities[1].name, "東京");
    assert_eq!(entities[1].kind, "concept");
    assert!(!entities[1].id.is_nil());
    assert_ne!(entities[0].id, entities[1].id);
    assert!(adapter.edges().next().is_none());
    assert!(adapter.warnings().is_empty());
}

#[test]
fn nonblank_quoted_payload_bytes_are_not_trimmed() {
    let mut adapter = CsvFormatAdapter::new(
        "name,description,custom\n\"  Alpha  \",\"\n First line\nLast line \n\",\"  payload  \"\n",
        DelimitedFormat::Csv,
        Some("concept"),
        &[],
    )
    .unwrap();
    let entity = adapter.entities().next().unwrap().unwrap();
    assert_eq!(entity.name, "  Alpha  ");
    assert_eq!(
        entity.description.as_deref(),
        Some("\n First line\nLast line \n")
    );
    assert_eq!(entity.properties, json!({"custom":"  payload  "}));
}

#[test]
fn tsv_maps_typed_fields_and_registered_kinds() {
    let mut adapter = CsvFormatAdapter::new(
        "name\tkind\ttags\tproperties\tcreated_at\tyear\nAsset\tcustom_kind\t[\"a\",\"b\"]\t{\"nested\":true}\t2026-01-01T00:00:00Z\t2026\nFallback\t\t\t\t\t2025\n",
        DelimitedFormat::Tsv, Some("concept"), &["custom_kind".into()],
    ).unwrap();
    assert_eq!(adapter.name(), "tsv");
    let entities = adapter.entities().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(entities[0].kind, "custom_kind");
    assert_eq!(entities[0].tags, ["a", "b"]);
    assert_eq!(entities[0].properties, json!({"nested":true,"year":"2026"}));
    assert_eq!(
        entities[0].created_at.as_deref(),
        Some("2026-01-01T00:00:00Z")
    );
    assert_eq!(entities[1].kind, "concept");
}

#[test]
fn malformed_headers_and_record_lengths_are_structural_errors() {
    for input in [
        "",
        "\n",
        "name,NAME\nA,B\n",
        "name,\nA,B\n",
        "description\nx\n",
        "name,year\nA,2026,extra\n",
        "name,year\nA\n",
        "source,target\na,b\n",
    ] {
        assert!(
            matches!(
                CsvFormatAdapter::new(input, DelimitedFormat::Csv, Some("concept"), &[]),
                Err(AdapterError::Parse(_))
            ),
            "{input:?}"
        );
    }
    assert!(CsvFormatAdapter::new("name\nA\n", DelimitedFormat::Csv, None, &[]).is_err());
    let mut empty = CsvFormatAdapter::new("name,kind\n", DelimitedFormat::Csv, None, &[]).unwrap();
    assert!(empty.entities().next().is_none());
}

#[test]
fn invalid_later_records_remain_errors_instead_of_disappearing() {
    for (headers, value) in [
        ("kind", "not_a_kind"),
        ("id", "bad-id"),
        ("created_at", "yesterday"),
        ("tags", "[3]"),
        ("properties", "[]"),
    ] {
        let source = format!("name,{headers}\nGood,\nBad,{value}\n");
        let mut adapter =
            CsvFormatAdapter::new(&source, DelimitedFormat::Csv, Some("concept"), &[]).unwrap();
        let mut records = adapter.entities();
        assert!(records.next().unwrap().is_ok());
        assert!(records.next().unwrap().is_err(), "{headers}");
        assert!(records.next().is_none());
    }
}

#[test]
fn edge_headers_select_edges_and_retain_weight_and_relation_validation() {
    let mut adapter = CsvFormatAdapter::new(
        "Source,Target,relation,weight,evidence\na,b,extends,,source one\nc,d,supports,0.9,source two\n",
        DelimitedFormat::Csv, None, &[],
    ).unwrap();
    assert!(adapter.entities().next().is_none());
    let edges = adapter.edges().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(edges.len(), 2);
    assert_eq!(edges[0].weight, 0.7);
    assert_eq!(edges[0].source, "a");
    assert_eq!(edges[0].target, "b");
    assert_eq!(edges[0].relation, "extends");
    assert_eq!(edges[0].properties, json!({"evidence":"source one"}));
    assert_eq!(edges[1].weight, 0.9);
    assert_ne!(edges[0].edge_id, edges[1].edge_id);
    for weight in ["NaN", "inf", "-0.1", "1.1", "0.5suffix"] {
        let source = format!("source,target,relation,weight\na,b,extends,{weight}\n");
        let mut adapter = CsvFormatAdapter::new(&source, DelimitedFormat::Csv, None, &[]).unwrap();
        assert!(adapter.edges().next().unwrap().is_err(), "{weight}");
    }
    let mut adapter = CsvFormatAdapter::new(
        "source,target,relation\na,b,underpins\n",
        DelimitedFormat::Csv,
        None,
        &[],
    )
    .unwrap();
    assert!(matches!(
        adapter.edges().next().unwrap(),
        Err(AdapterError::UnknownRelation { .. })
    ));
}
