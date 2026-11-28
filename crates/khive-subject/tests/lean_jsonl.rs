use std::io::{self, BufRead, Cursor, Read};

use khive_subject::{
    Extractor, FormalExtractor, LeanJsonlScanner, OntologySpec, RawDecl, ScanError, Scanner,
};
use serde_json::json;

fn row(name: &str) -> serde_json::Value {
    json!({"name":name,"kind":"def","type":"Nat → Nat","refs":["B","B","outside"],"module":"Mathlib.Data.Nat"})
}

#[test]
fn jsonl_preserves_raw_fields_and_document_presence() {
    let mut rows = [row("A"), row("B"), row("C"), row("D")];
    rows[0]["doc"] = json!("  数学 λ e\u{301}\nline\tend  ");
    rows[1]["doc"] = json!("");
    rows[3]["doc"] = serde_json::Value::Null;
    rows[0]["discipline"] = json!("selection metadata");
    let input = rows
        .iter()
        .map(|row| format!("{row}\r\n"))
        .collect::<String>();
    let actual = LeanJsonlScanner.scan(Cursor::new(input)).unwrap();
    let expected = [Some("  数学 λ e\u{301}\nline\tend  "), Some(""), None, None]
        .into_iter()
        .zip(["A", "B", "C", "D"])
        .map(|(doc, name)| RawDecl {
            name: name.into(),
            kind: "def".into(),
            type_expr: "Nat → Nat".into(),
            refs: vec!["B".into(), "B".into(), "outside".into()],
            doc: doc.map(str::to_owned),
            module: "Mathlib.Data.Nat".into(),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        actual, expected,
        "scanner must preserve literal records and doc distinctions"
    );
}

#[test]
fn reference_alias_is_supported_but_duplicate_spellings_are_refused() {
    let mut alias = row("A");
    let refs = alias.as_object_mut().unwrap().remove("refs").unwrap();
    alias["refs_in_corpus"] = refs;
    let actual = LeanJsonlScanner
        .scan(Cursor::new(alias.to_string()))
        .unwrap();
    assert_eq!(actual[0].refs, ["B", "B", "outside"]);
    for extra in [json!(["B", "B", "outside"]), json!(["different"])] {
        alias["refs"] = extra;
        let result = LeanJsonlScanner.scan(Cursor::new(alias.to_string()));
        assert!(
            matches!(result, Err(ScanError::Json { line: 1, .. })),
            "both refs spellings must fail, even when equal"
        );
    }
}

#[test]
fn malformed_physical_line_never_returns_a_successful_prefix() {
    for bad in ["{", "", "null", r#"{"name":"missing"}"#] {
        let input = format!("{}\n{bad}\n{}\n", row("first"), row("last"));
        let result = LeanJsonlScanner.scan(Cursor::new(input));
        assert!(
            matches!(result, Err(ScanError::Json { line: 2, .. })),
            "malformed physical line 2 must refuse the entire corpus: {bad:?}"
        );
    }
    for (field, value) in [
        ("doc", json!(3)),
        ("refs", json!([1])),
        ("type", json!(null)),
        ("module", json!(false)),
    ] {
        let mut invalid = row("A");
        invalid[field] = value;
        assert!(
            matches!(
                LeanJsonlScanner.scan(Cursor::new(invalid.to_string())),
                Err(ScanError::Json { line: 1, .. })
            ),
            "wrong field type must refuse: {field}"
        );
    }
}

struct FailedAfterLine {
    first: Cursor<Vec<u8>>,
}

impl Read for FailedAfterLine {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("fixture reader failure"))
    }
}

impl BufRead for FailedAfterLine {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.first.position() == self.first.get_ref().len() as u64 {
            return Err(io::Error::other("fixture reader failure"));
        }
        self.first.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        self.first.consume(amount);
    }
}

#[test]
fn reader_failure_and_invalid_utf8_are_line_bound_whole_result_errors() {
    let reader = FailedAfterLine {
        first: Cursor::new(format!("{}\n", row("A")).into_bytes()),
    };
    let result = LeanJsonlScanner.scan(reader);
    assert!(
        matches!(result, Err(ScanError::Read { line: 2, .. })),
        "I/O failure after a valid row must not return that prefix"
    );
    let mut input = format!("{}\n", row("A")).into_bytes();
    input.extend([0xff, b'\n']);
    assert!(matches!(
        LeanJsonlScanner.scan(Cursor::new(input)),
        Err(ScanError::Read { line: 2, .. })
    ));
}

#[test]
fn scanned_and_programmatic_records_use_the_same_per_call_extractor() {
    let scanner = LeanJsonlScanner;
    let first = scanner
        .scan(Cursor::new(format!("{}\n{}", row("A"), row("B"))))
        .unwrap();
    let direct = ["A", "B"].map(|name| RawDecl {
        name: name.into(),
        kind: "def".into(),
        type_expr: "Nat → Nat".into(),
        refs: vec!["B".into(), "B".into(), "outside".into()],
        doc: None,
        module: "Mathlib.Data.Nat".into(),
    });
    let ontology = OntologySpec::formal_math().unwrap();
    let scanned_plan = FormalExtractor.extract(&first, &ontology).unwrap();
    assert_eq!(
        scanned_plan,
        FormalExtractor.extract(&direct, &ontology).unwrap()
    );
    assert_eq!(scanned_plan.edges.len(), 2);
    let second = scanner.scan(Cursor::new(row("A").to_string())).unwrap();
    let second_plan = FormalExtractor.extract(&second, &ontology).unwrap();
    assert!(
        second_plan.edges.is_empty(),
        "corpus membership must not survive a prior scan/extraction"
    );
    assert!(scanner.scan(Cursor::new("")).unwrap().is_empty());
}
