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
#[path = "email_address_tests.rs"]
mod tests;
