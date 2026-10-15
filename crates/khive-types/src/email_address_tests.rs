use super::*;

#[test]
fn strips_display_names_and_surrounding_whitespace() {
    assert_eq!(
        normalize_email_recipient("  A@Example.COM ").as_deref(),
        Some("a@example.com")
    );
    assert_eq!(
        normalize_email_recipient("Name < A@Example.COM >").as_deref(),
        Some("a@example.com")
    );
}

#[test]
fn recipient_form_folds_ascii_case_only() {
    assert_eq!(
        normalize_email_recipient("Name < A@Example.COM >").as_deref(),
        Some("a@example.com")
    );
    // U+212A KELVIN SIGN stays itself instead of becoming ASCII `k`.
    assert_eq!(
        normalize_email_recipient("\u{212A}evin@example.com").as_deref(),
        Some("\u{212A}evin@example.com")
    );
    assert_eq!(
        normalize_email_recipient("\u{C9}mile@Example.COM").as_deref(),
        Some("\u{C9}mile@example.com")
    );
    assert_eq!(normalize_email_recipient("not-an-address"), None);
    assert_eq!(normalize_email_recipient(""), None);
}
