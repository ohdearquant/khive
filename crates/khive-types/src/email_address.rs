use alloc::string::String;

/// Normalize an email address for comparison: the addr-spec inside angle
/// brackets when they enclose one, else the trimmed value, with ASCII letters
/// lowercased; every other character compares exactly. Unicode lowercasing can turn a different
/// character into an ASCII letter (U+212A KELVIN SIGN lowercases to `k`), so a
/// comparison that must never admit a different address folds ASCII case only.
pub fn normalize_email_recipient(raw: &str) -> Option<String> {
    addr_spec(raw).map(str::to_ascii_lowercase)
}

/// The addr-spec inside angle brackets when they enclose one, else the trimmed
/// value; `None` when neither contains `@`.
fn addr_spec(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if let Some(start) = trimmed.rfind('<') {
        if let Some(end) = trimmed[start..].find('>') {
            let addr = trimmed[start + 1..start + end].trim();
            if addr.contains('@') {
                return Some(addr);
            }
        }
    }
    trimmed.contains('@').then_some(trimmed)
}

#[cfg(test)]
mod tests {
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
}
