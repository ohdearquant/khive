#[test]
fn fts5_syntax_error_at_fts_search_is_classified_as_syntax_error() {
    let e = driver_err("fts_search", "fts5: syntax error near \"@\"");
    assert!(e.is_fts5_syntax_error());
}

#[test]
fn fts5_parser_stack_overflow_is_classified_as_syntax_error() {
    let e = driver_err("fts_search", "fts5: parser stack overflow");
    assert!(e.is_fts5_syntax_error());
}

#[test]
fn fts5_unsupported_column_query_is_classified_as_syntax_error() {
    let e = driver_err(
        "fts_search",
        "fts5: column queries are not supported (detail=none)",
    );
    assert!(e.is_fts5_syntax_error());
}

#[test]
fn timeout_is_not_classified_as_syntax_error() {
    let e = StorageError::Timeout {
        operation: "fts_search".into(),
    };
    assert!(!e.is_fts5_syntax_error());
}

#[test]
fn pool_failure_is_not_classified_as_syntax_error() {
    let e = StorageError::Pool {
        operation: "fts_search".into(),
        message: "pool exhausted".into(),
    };
    assert!(!e.is_fts5_syntax_error());
}

#[test]
fn driver_error_at_non_search_operation_is_not_classified_as_syntax_error() {
    let e = driver_err("open_fts_reader", "fts5: syntax error near \"@\"");
    assert!(!e.is_fts5_syntax_error());
}

#[test]
fn driver_error_with_unrelated_message_is_not_classified_as_syntax_error() {
    let e = driver_err("fts_search", "disk I/O error");
    assert!(!e.is_fts5_syntax_error());
}

#[test]
fn fts5_phrase_detail_query_is_classified_as_syntax_error() {
    let e = driver_err(
        "fts_search",
        "fts5: phrase queries are not supported (detail!=full)",
    );
    assert!(e.is_fts5_syntax_error());
}

#[test]
fn fts5_near_detail_query_is_classified_as_syntax_error() {
    let e = driver_err(
        "fts_search",
        "fts5: NEAR queries are not supported (detail!=full)",
    );
    assert!(e.is_fts5_syntax_error());
}

#[test]
fn unprefixed_detail_message_is_not_classified_as_syntax_error() {
    let e = driver_err(
        "fts_search",
        "phrase queries are not supported (detail!=full)",
    );
    assert!(!e.is_fts5_syntax_error());
}

#[test]
fn fts5_shadow_table_corruption_is_not_classified_as_syntax_error() {
    let e = driver_err(
        "fts_search",
        "fts5: error creating shadow table notes_content: no such table",
    );
    assert!(!e.is_fts5_syntax_error());
}

#[test]
fn non_text_capability_is_not_classified_as_syntax_error() {
    let e = StorageError::Driver {
        capability: StorageCapability::Vectors,
        operation: "fts_search".into(),
        source: Box::new(FakeSource("fts5: syntax error near \"@\"".into())),
    };
    assert!(!e.is_fts5_syntax_error());
}
