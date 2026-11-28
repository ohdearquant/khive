use serde_json::{json, Value};

use super::{clean_lines, clean_log, stored_log, tail, TAIL_BYTES, TAIL_LINES};

#[test]
fn clean_log_keeps_lines_and_tabs_and_removes_escapes() {
    let raw = b"step\tone\r\n\x1b[1;31mred\x1b[0m\n\x1b]0;title\x07after\n\x1b]8;;x\x1b\\link\n\x1b(Bdone\n";
    let (text, altered) = clean_log(raw);
    assert_eq!(text, "step\tone\nred\nafter\nlink\ndone\n");
    assert!(altered);
}

#[test]
fn clean_log_counts_a_leading_mark_and_line_end_returns_as_no_change() {
    assert_eq!(
        clean_log(b"\xEF\xBB\xBFa\r\nb\r\n"),
        ("a\nb\n".to_string(), false)
    );
    // Accented and symbol text is kept whole.
    let text = "caf\u{e9} \u{2713} na\u{ef}ve\n";
    assert_eq!(clean_log(text.as_bytes()), (text.to_string(), false));
    assert_eq!(clean_log(b""), (String::new(), false));
}

#[test]
fn clean_log_reports_every_other_change() {
    for (raw, expected) in [
        (&b"a\rb\n"[..], "ab\n"),
        (&b"a\xFFb"[..], "a\u{FFFD}b"),
        ("a\u{200B}b".as_bytes(), "ab"),
        ("a\u{202E}b".as_bytes(), "ab"),
        // A mark that does not lead the log is a hidden character like any other.
        ("a\u{FEFF}b".as_bytes(), "ab"),
        (&b"a\x07b"[..], "ab"),
        (&b"a\x1b"[..], "a"),
        (&b"a\x1b[31"[..], "a"),
    ] {
        assert_eq!(
            clean_log(raw),
            (expected.to_string(), true),
            "{:?}",
            String::from_utf8_lossy(raw)
        );
    }
}

#[test]
fn a_line_break_after_or_inside_an_escape_is_kept() {
    for (raw, expected) in [
        (&b"a\x1b\nb\n"[..], "a\nb\n"),
        (&b"a\x1b[31\nb\n"[..], "a\nb\n"),
        (&b"a\x1b]0;title\nb\n"[..], "a\nb\n"),
        (&b"a\x1b(\nb\n"[..], "a\nb\n"),
    ] {
        assert_eq!(
            clean_log(raw),
            (expected.to_string(), true),
            "{:?}",
            String::from_utf8_lossy(raw)
        );
    }
}

#[test]
fn the_credential_is_replaced_again_over_the_cleaned_text() {
    let value = concat!("tok", "-split-", "value");
    // An escape sequence inside one occurrence and a hidden character inside another.
    let raw = "a tok-split\u{1b}[0m-value b tok-\u{200B}split-value c\n";
    assert_eq!(
        stored_log(raw.as_bytes().to_vec(), value, false),
        ("a [redacted] b [redacted] c\n".to_string(), true)
    );
    // A window that begins inside an occurrence an escape sequence split.
    assert_eq!(
        stored_log("split\u{1b}[0m-value z".as_bytes().to_vec(), value, true),
        ("[redacted] z".to_string(), true)
    );
    // Control: a whole occurrence is replaced by the first pass and is no change of cleaning.
    assert_eq!(
        stored_log(b"x tok-split-value y".to_vec(), value, false),
        ("x [redacted] y".to_string(), false)
    );
}

#[test]
fn tail_is_bounded_by_lines() {
    assert_eq!(tail(""), "");
    assert_eq!(tail("one\ntwo\n"), "one\ntwo\n");
    let lines = |n: usize| -> String { (0..n).map(|i| format!("line {i}\n")).collect() };
    let exact = lines(TAIL_LINES);
    assert_eq!(tail(&exact), exact);
    let over = lines(TAIL_LINES + 1);
    assert_eq!(tail(&over), &over["line 0\n".len()..]);
    let many = lines(3 * TAIL_LINES);
    let kept = tail(&many);
    assert_eq!(kept.lines().count(), TAIL_LINES);
    assert!(kept.starts_with(&format!("line {}\n", 2 * TAIL_LINES)));
    assert!(many.ends_with(kept));
    // Without a final line break the count is the same.
    let open = many.trim_end_matches('\n');
    assert_eq!(tail(open).lines().count(), TAIL_LINES);
}

#[test]
fn tail_is_bounded_by_bytes_at_a_character_boundary() {
    let wide = "\u{e9}".repeat(TAIL_BYTES) + "x";
    let kept = tail(&wide);
    assert!(kept.len() <= TAIL_BYTES);
    assert!(kept.len() + 1 >= TAIL_BYTES);
    assert!(kept.ends_with("\u{e9}x"));
    assert!(wide.ends_with(kept));
}

#[test]
fn clean_lines_keeps_breaks_bounds_characters_and_reports_changes() {
    let mut altered = false;
    assert_eq!(
        clean_lines(Some(&json!("a\r\nb")), 10, &mut altered),
        json!("a\nb")
    );
    assert!(!altered);
    assert_eq!(clean_lines(None, 10, &mut altered), Value::Null);
    assert_eq!(clean_lines(Some(&json!(7)), 10, &mut altered), Value::Null);
    assert!(!altered);
    assert_eq!(
        clean_lines(Some(&json!("abc")), 2, &mut altered),
        json!("ab")
    );
    assert!(altered);
    let mut altered = false;
    assert_eq!(
        clean_lines(Some(&json!("\u{1b}[1mx")), 10, &mut altered),
        json!("x")
    );
    assert!(altered);
}
