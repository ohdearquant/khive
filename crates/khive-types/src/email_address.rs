use alloc::string::String;

/// Extract the email connector's normalized addr-spec from a configured address.
/// Preserves its existing display-name stripping and lowercase comparison rules.
pub fn normalize_email_address(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if let Some(start) = trimmed.rfind('<') {
        if let Some(end) = trimmed[start..].find('>') {
            let addr = trimmed[start + 1..start + end].trim().to_lowercase();
            if addr.contains('@') {
                return Some(addr);
            }
        }
    }
    let lower = trimmed.to_lowercase();
    lower.contains('@').then_some(lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_connector_address_normalization() {
        assert_eq!(
            normalize_email_address("  A@Example.COM ").as_deref(),
            Some("a@example.com")
        );
        assert_eq!(
            normalize_email_address("Name < A@Example.COM >").as_deref(),
            Some("a@example.com")
        );
        assert_eq!(normalize_email_address("not-an-address"), None);
        assert_eq!(normalize_email_address(""), None);
    }
}
