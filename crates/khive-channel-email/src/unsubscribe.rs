//! Mailto unsubscribe token extraction from an inbound `Subject:` (ADR-200 §5.4 entry point 1).
//!
//! The unsubscribe header the channel emits is
//! `<mailto:<mailbox>?subject=unsubscribe%20<token>>`, which a mail client renders and sends back
//! as a subject of the form `unsubscribe <token>`. This module only *finds* a candidate token in a
//! subject string; whether it verifies is decided by the recipient-key policy, never here.

/// Return the first candidate unsubscribe token in `subject`, or `None`.
///
/// The scan looks for the literal `unsubscribe` (case-insensitive), requires ASCII whitespace
/// immediately after it, and then takes the maximal run of base64url characters (`A-Z`, `a-z`,
/// `0-9`, `-`, `_`) that follows. The first such run that is non-empty is returned verbatim. A
/// subject with `unsubscribe` followed by no whitespace, or by whitespace and no token, yields
/// `None`; `unsubscribe` inside a longer word (`unsubscribed`) does not match.
pub(crate) fn extract_unsubscribe_token(subject: &str) -> Option<&str> {
    const MARKER: &str = "unsubscribe";
    // ASCII lowercasing preserves byte length, so offsets found here index `subject` exactly.
    let lowered = subject.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(found) = lowered[search_from..].find(MARKER) {
        let after_marker = search_from + found + MARKER.len();
        let rest = &subject[after_marker..];
        let whitespace_len = rest.len() - rest.trim_start().len();
        if whitespace_len > 0 {
            let token_and_beyond = &rest[whitespace_len..];
            let token_len = token_and_beyond
                .find(|c: char| !is_base64url(c))
                .unwrap_or(token_and_beyond.len());
            if token_len > 0 {
                return Some(&token_and_beyond[..token_len]);
            }
        }
        search_from = after_marker;
    }
    None
}

fn is_base64url(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

#[cfg(test)]
mod tests {
    use super::extract_unsubscribe_token;

    #[test]
    fn extracts_the_token_after_the_marker_and_whitespace() {
        assert_eq!(
            extract_unsubscribe_token("unsubscribe AQAB_-cD12"),
            Some("AQAB_-cD12")
        );
    }

    #[test]
    fn marker_match_is_case_insensitive_and_may_be_mid_subject() {
        assert_eq!(
            extract_unsubscribe_token("Re: UNSUBSCRIBE AQABtoken"),
            Some("AQABtoken")
        );
    }

    #[test]
    fn stops_the_token_at_the_first_non_base64url_character() {
        assert_eq!(
            extract_unsubscribe_token("unsubscribe abc.def"),
            Some("abc")
        );
    }

    #[test]
    fn requires_whitespace_between_marker_and_token() {
        assert_eq!(extract_unsubscribe_token("unsubscribeabc"), None);
        assert_eq!(extract_unsubscribe_token("unsubscribed AQABtoken"), None);
    }

    #[test]
    fn a_marker_with_no_token_yields_nothing() {
        assert_eq!(extract_unsubscribe_token("unsubscribe"), None);
        assert_eq!(extract_unsubscribe_token("unsubscribe   "), None);
        // A word after the marker is a candidate token, not a match: it simply will not verify
        // and the message continues as ordinary mail.
        assert_eq!(
            extract_unsubscribe_token("please unsubscribe me"),
            Some("me")
        );
    }

    #[test]
    fn an_ordinary_subject_has_no_token() {
        assert_eq!(extract_unsubscribe_token("Hello there"), None);
        assert_eq!(extract_unsubscribe_token(""), None);
    }
}
