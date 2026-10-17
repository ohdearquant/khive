use super::*;

#[test]
fn expanded_value_budget_is_shared_across_fields_and_checked_before_append() {
    let source = b"@string{v={1234}} @book{k,title=v,year=v}";
    let limits = Limits {
        expanded_bytes: 8,
        ..Limits::default()
    };
    assert!(BibtexFormatAdapter::read_with_limits(source.as_slice(), limits).is_ok());
    let error = BibtexFormatAdapter::read_with_limits(
        source.as_slice(),
        Limits {
            expanded_bytes: 7,
            ..limits
        },
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("expanded entry"));
    let mut budget = 3;
    let mut macros = HashMap::new();
    macros.insert(Variable::new("v".to_string()).unwrap(), "1234".to_string());
    assert!(matches!(
        expand(vec![RawToken::Variable("v".into())], &macros, &mut budget),
        Err(ExpansionError::Limit)
    ));
}

#[test]
fn macro_budget_counts_names_and_values_and_releases_replaced_entries() {
    let source = b"@string{v={1234}} @string{V={5678}} @book{k,title=v}";
    let limits = Limits {
        macro_bytes: 5,
        ..Limits::default()
    };
    let mut adapter = BibtexFormatAdapter::read_with_limits(source.as_slice(), limits).unwrap();
    assert_eq!(adapter.entities().next().unwrap().unwrap().name, "5678");
    let error = BibtexFormatAdapter::read_with_limits(
        source.as_slice(),
        Limits {
            macro_bytes: 4,
            ..limits
        },
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("macro state"));
    let expansion = b"@string{a={1234}} @string{b=a#a#a#a}";
    let error = BibtexFormatAdapter::read_with_limits(
        expansion.as_slice(),
        Limits {
            macro_bytes: 20,
            ..limits
        },
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("macro state"));
}

#[test]
fn framed_valid_syntax_decodes_with_the_upstream_parser() {
    let source = b"@comment(% {)} text) @string{v={one}} @book(k,title={nested {value}},year=2026)";
    let mut frames = BibtexFrames::new(source.as_slice(), Limits::default());
    let mut count = 0;
    while let Some(frame) = frames.next_frame().unwrap() {
        assert!(frame.complete);
        let parsed: Vec<RawEntry> = serde_bibtex::from_str(&frame.source).unwrap();
        assert_eq!(parsed.len(), 1);
        count += 1;
    }
    assert_eq!(count, 3);
}
