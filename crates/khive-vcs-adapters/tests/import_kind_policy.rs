use khive_types::ImportKindPolicy;
use khive_vcs_adapters::{CsvFormatAdapter, DelimitedFormat, FormatAdapter, JsonFormatAdapter};
use serde_json::json;

#[test]
fn json_explicit_policy_preserves_only_unknown_kind_spellings() {
    for raw in ["Uninstalled", " Ω kind ", "future:型"] {
        let input = json!([{"kind":raw,"name":"A","entity_type":"subtype","properties":{"keep":true},"tags":["tag"]}]).to_string();
        let mut strict = JsonFormatAdapter::new(&input).unwrap();
        assert!(strict.entities().next().unwrap().is_err());
        let mut relaxed =
            JsonFormatAdapter::new_with_kind_policy(&input, &[], ImportKindPolicy::PreserveUnknown)
                .unwrap();
        let entity = relaxed.entities().next().unwrap().unwrap();
        assert_eq!(entity.kind, raw);
        assert_eq!(entity.entity_type.as_deref(), Some("subtype"));
        assert_eq!(entity.properties, json!({"keep":true}));
        assert_eq!(entity.tags, ["tag"]);
        assert!(
            relaxed.warnings().is_empty(),
            "mandatory kind warnings belong to the importer"
        );
    }
    let input = r#"[{"kind":" Paper ","name":"A"},{"kind":" RESOURCE ","name":"B"}]"#;
    for policy in [ImportKindPolicy::Strict, ImportKindPolicy::PreserveUnknown] {
        let mut adapter =
            JsonFormatAdapter::new_with_kind_policy(input, &["resource".into()], policy).unwrap();
        let records = adapter.entities().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(records[0].kind, "document");
        assert_eq!(records[0].entity_type.as_deref(), Some("paper"));
        assert_eq!(records[1].kind, "resource");
    }
}

#[test]
fn delimited_policy_reaches_rows_and_default_kind_without_normalizing_unknowns() {
    for (format, sep) in [(DelimitedFormat::Csv, ','), (DelimitedFormat::Tsv, '\t')] {
        let source = format!("name{sep}kind\nA{sep} Future型 \nB{sep}\n");
        let mut strict =
            CsvFormatAdapter::new(&source, format, Some("DefaultFuture"), &[]).unwrap();
        assert!(strict.entities().all(|record| record.is_err()));
        let mut relaxed = CsvFormatAdapter::new_with_kind_policy(
            &source,
            format,
            Some("DefaultFuture"),
            &[],
            ImportKindPolicy::PreserveUnknown,
        )
        .unwrap();
        let records = relaxed.entities().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(records[0].kind, " Future型 ");
        assert_eq!(records[1].kind, "DefaultFuture");
    }
}

#[test]
fn relaxed_policy_keeps_closed_relations_and_other_record_failures() {
    for policy in [ImportKindPolicy::Strict, ImportKindPolicy::PreserveUnknown] {
        for record in [
            json!({"kind":" ","name":"A"}),
            json!({"kind":"Future","name":" "}),
            json!({"kind":"Future","name":"A","created_at":"bad"}),
            json!({"kind":"Future","name":"A","id":"bad-uuid"}),
            json!({"source":"a","target":"b","relation":"future"}),
            json!({"source":"a","target":"b","relation":"extends","weight":1.1}),
        ] {
            let mut adapter =
                JsonFormatAdapter::new_with_kind_policy(&json!([record]).to_string(), &[], policy)
                    .unwrap();
            let failed = adapter.entities().any(|record| record.is_err())
                || adapter.edges().any(|record| record.is_err());
            assert!(failed);
        }
        let ambiguous =
            r#"[{"kind":"Future","name":"A","source":"a","target":"b","relation":"extends"}]"#;
        assert!(JsonFormatAdapter::new_with_kind_policy(ambiguous, &[], policy).is_err());
        for (format, sep) in [(DelimitedFormat::Csv, ','), (DelimitedFormat::Tsv, '\t')] {
            let mut adapter = CsvFormatAdapter::new_with_kind_policy(
                &format!("source{sep}target{sep}relation\na{sep}b{sep}future\n"),
                format,
                None,
                &[],
                policy,
            )
            .unwrap();
            assert!(adapter.edges().next().unwrap().is_err());
        }
    }
}
