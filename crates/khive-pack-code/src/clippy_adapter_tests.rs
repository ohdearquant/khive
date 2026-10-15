use super::*;

#[test]
fn repeated_snippet_on_many_lines_has_bounded_span_comparisons() {
    const OCCURRENCES: usize = 512;
    let input = (1..=OCCURRENCES)
        .map(|line| {
            json!({
                "reason": "compiler-message",
                "message": {
                    "message": "this borrow is unnecessary",
                    "code": {"code": "clippy::needless_borrow"},
                    "level": "warning",
                    "spans": [{
                        "file_name": "src/lib.rs",
                        "line_start": line,
                        "line_end": line,
                        "column_start": 5,
                        "column_end": 13,
                        "is_primary": true,
                        "text": [{"text": "    consume(&value);"}]
                    }]
                }
            })
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let observed_at = "2026-09-29T00:00:00Z"
        .parse()
        .expect("fixed observation time");
    PRIMARY_SPAN_COMPARISONS.with(|count| count.set(0));
    let batch = ingest_clippy_json_lines(
        input.as_bytes(),
        ClippyProvenance {
            repo: "example",
            branch: "main",
            commit: "example-commit",
            scope: "example-crate",
        },
        CodeIngestOptions {
            namespace: "local",
            observed_at,
            source_run: None,
        },
    )
    .expect("all distinct source positions are valid");
    let comparisons = PRIMARY_SPAN_COMPARISONS.with(std::cell::Cell::get);

    assert_eq!(batch.notes.len(), OCCURRENCES);
    assert!(comparisons > 0, "span comparison hook was not exercised");
    assert!(
        comparisons <= OCCURRENCES * 128,
        "{comparisons} span comparisons for {OCCURRENCES} findings exceeds the ordered-map bound"
    );
}
