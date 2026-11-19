// ── parse_ops_file tests ───────────────────────────────────────────────────

#[test]
fn parse_ops_file_skips_blank_lines() {
    use std::io::Write as _;
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(b"{\"tool\":\"stats\",\"args\":{}}\n").unwrap();
    f.write_all(b"\n").unwrap(); // blank
    f.write_all(b"{\"tool\":\"stats\",\"args\":{}}\n").unwrap();
    let ops = parse_ops_file(f.path()).unwrap();
    assert_eq!(ops.len(), 2);
}

#[test]
fn parse_ops_file_reports_line_number_on_malformed() {
    use std::io::Write as _;
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(b"{\"tool\":\"stats\",\"args\":{}}\n").unwrap();
    f.write_all(b"not-json\n").unwrap(); // line 2 is bad
    let err = parse_ops_file(f.path()).unwrap_err();
    assert_eq!(
        err.downcast_ref::<ExecRefusal>().map(|error| error.reason),
        Some(RefusalReason::ParseError)
    );
    let msg = format!("{err:#}");
    assert!(
        msg.contains("line 2"),
        "error should name the bad line number, got: {msg}"
    );
}

#[test]
fn parse_ops_file_missing_tool_field() {
    use std::io::Write as _;
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(b"{\"notool\":\"x\",\"args\":{}}\n").unwrap();
    let err = parse_ops_file(f.path()).unwrap_err();
    assert_eq!(
        err.downcast_ref::<ExecRefusal>().map(|error| error.reason),
        Some(RefusalReason::ParseError)
    );
    let msg = format!("{err:#}");
    assert!(msg.contains("line 1"), "should report line number: {msg}");
}
