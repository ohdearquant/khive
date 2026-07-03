//! RFC 8601 `Authentication-Results` header parsing (structural subset).
//!
//! `mail-parser` has no structured support for this header -- it is not one of
//! the RFC 5322 shapes it recognizes, so it always falls through to
//! `HeaderValue::Text` (or `TextList` when duplicated) via the generic `Other`
//! header path. This module hand-parses only what the attribution gate needs:
//! the `authserv-id`, and the `dmarc`/`spf`/`dkim` method verdicts plus the
//! `header.d` / `smtp.mailfrom` / `header.from` alignment properties.
//!
//! The top-level split into `resinfo` segments is CFWS-aware: it walks the raw
//! header value once, tracking quoted-string state (honoring `\` escapes) and
//! `(...)` comment nesting (RFC 5322 comments nest), and only treats a `;` as a
//! segment boundary when it appears outside both. Comments are stripped
//! entirely before segment text is retained; quoted-string content is kept
//! verbatim (including any `;` or `=` it contains) as part of the single
//! segment it belongs to. This guarantees a `;` inside a `reason="..."`
//! quoted pvalue or inside a `(...)` comment can never manufacture an
//! additional, unintended `method=result` segment -- see
//! `parse_header_reason_quoted_semicolon_does_not_forge_dmarc_pass` and
//! `parse_header_comment_semicolon_does_not_forge_dmarc_pass` below. It does
//! not attempt full ABNF conformance beyond that (e.g. `ptype.property`
//! values containing bare `=`) -- those are tolerated as harmless unmatched
//! tokens, never as false positives against the three keys this module
//! actually reads.

use std::collections::HashMap;

/// One `resinfo` entry: a single method's verdict plus its properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MethodResult {
    pub result: String,
    pub props: HashMap<String, String>,
}

/// A parsed `Authentication-Results` header value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct AuthResults {
    pub authserv_id: String,
    pub dmarc: Vec<MethodResult>,
    pub spf: Vec<MethodResult>,
    pub dkim: Vec<MethodResult>,
}

impl AuthResults {
    /// `dmarc=pass` is present. DMARC's own verdict already encodes alignment
    /// (RFC 7489 §3.1), so no separate domain check applies here.
    pub fn dmarc_pass(&self) -> bool {
        self.dmarc
            .iter()
            .any(|e| e.result.eq_ignore_ascii_case("pass"))
    }

    /// `spf=pass` is present AND its `smtp.mailfrom` domain matches `from_domain`.
    pub fn spf_pass_aligned(&self, from_domain: &str) -> bool {
        Self::method_pass_aligned(&self.spf, "smtp.mailfrom", from_domain)
    }

    /// `dkim=pass` is present AND its `header.d` domain matches `from_domain`.
    pub fn dkim_pass_aligned(&self, from_domain: &str) -> bool {
        Self::method_pass_aligned(&self.dkim, "header.d", from_domain)
    }

    /// True when spf or dkim passed but its alignment domain did not match
    /// `from_domain` -- distinguishes an "unaligned" quarantine reason from a
    /// flat authentication failure.
    pub fn has_unaligned_pass(&self, from_domain: &str) -> bool {
        let spf_passed = self
            .spf
            .iter()
            .any(|e| e.result.eq_ignore_ascii_case("pass"));
        let dkim_passed = self
            .dkim
            .iter()
            .any(|e| e.result.eq_ignore_ascii_case("pass"));
        (spf_passed && !self.spf_pass_aligned(from_domain))
            || (dkim_passed && !self.dkim_pass_aligned(from_domain))
    }

    fn method_pass_aligned(entries: &[MethodResult], prop_key: &str, from_domain: &str) -> bool {
        entries.iter().any(|e| {
            e.result.eq_ignore_ascii_case("pass")
                && e.props
                    .get(prop_key)
                    .map(|v| domain_of(v).eq_ignore_ascii_case(from_domain))
                    .unwrap_or(false)
        })
    }
}

/// Extract the domain component of a property value. Values may be a bare
/// domain (`header.d=example.com`) or a full mailbox
/// (`smtp.mailfrom=alice@example.com`) -- only the part after `@`, if any, is
/// significant for alignment either way.
fn domain_of(value: &str) -> String {
    let v = value.trim().trim_end_matches('.');
    match v.rsplit_once('@') {
        Some((_, domain)) => domain.to_lowercase(),
        None => v.to_lowercase(),
    }
}

/// Split a raw `Authentication-Results` header value into top-level
/// `resinfo` segments on `;`, tracking RFC 5322 quoted-string state (with `\`
/// escapes) and `(...)` comment nesting so a `;` inside either is never
/// treated as a segment boundary. Comment text is stripped entirely (CFWS is
/// insignificant for token purposes); quoted-string text -- including any `;`
/// or `=` inside it -- is preserved verbatim as part of its enclosing
/// segment.
fn split_top_level_segments(raw: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut comment_depth: u32 = 0;
    let mut chars = raw.chars();

    while let Some(c) = chars.next() {
        if in_quotes {
            current.push(c);
            match c {
                '\\' => {
                    // Quoted-pair: the following character is escaped and never
                    // terminates the quoted string, regardless of what it is.
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                '"' => in_quotes = false,
                _ => {}
            }
            continue;
        }

        if comment_depth > 0 {
            match c {
                '\\' => {
                    // Quoted-pair inside a comment: consume and discard the
                    // escaped character without altering comment depth.
                    chars.next();
                }
                '(' => comment_depth += 1,
                ')' => comment_depth -= 1,
                _ => {}
            }
            continue;
        }

        match c {
            '"' => {
                in_quotes = true;
                current.push(c);
            }
            '(' => comment_depth += 1,
            ';' => segments.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    segments.push(current);

    segments
}

/// Parse one raw `Authentication-Results` header value.
///
/// Returns `None` only when no `authserv-id` token can be extracted at all
/// (an empty or malformed header). A bare `authserv-id; none` or an
/// `authserv-id` with no method the gate recognizes both parse successfully
/// to an `AuthResults` with empty method vectors -- the gate treats "no
/// recognized passing method" uniformly regardless of which of those it was.
pub(crate) fn parse_header(raw: &str) -> Option<AuthResults> {
    let mut segments = split_top_level_segments(raw).into_iter();
    let authserv_id = segments.next()?.split_whitespace().next()?.to_string();
    if authserv_id.is_empty() {
        return None;
    }

    let mut out = AuthResults {
        authserv_id,
        ..Default::default()
    };

    for segment in segments {
        let segment = segment.trim();
        if segment.is_empty() || segment.eq_ignore_ascii_case("none") {
            continue;
        }
        let mut tokens = segment.split_whitespace();
        let Some(methodspec) = tokens.next() else {
            continue;
        };
        let Some((method, result)) = methodspec.split_once('=') else {
            continue;
        };
        // RFC 8601 §2.2 permits an optional "/version" suffix on the method name
        // (method-version = 1*DIGIT). §2.6 requires consumers to IGNORE resinfo
        // for a method version they do not support -- an unsupported or
        // non-numeric version must never be silently trusted as the current
        // version. This module supports only version 1 (absent suffix is
        // implicitly version 1); anything else skips the whole segment.
        let method = match method.split_once('/') {
            None => method,
            Some((base, "1")) => base,
            Some(_) => continue,
        };

        let mut props = HashMap::new();
        for token in tokens {
            if let Some((ptype_property, pvalue)) = token.split_once('=') {
                props.insert(ptype_property.to_lowercase(), pvalue.to_string());
            }
        }

        let entry = MethodResult {
            result: result.to_lowercase(),
            props,
        };
        match method.to_lowercase().as_str() {
            "dmarc" => out.dmarc.push(entry),
            "spf" => out.spf.push(entry),
            "dkim" => out.dkim.push(entry),
            _ => {}
        }
    }

    Some(out)
}

/// Select the first (topmost) `Authentication-Results` header, in document
/// order, whose `authserv-id` matches `configured_id` (case-insensitive).
///
/// Topmost wins: a receiving MTA prepends its own stamp on each hop, so the
/// header nearest the top of the document is the one added by the final,
/// trusted receiving boundary -- PROVIDED that boundary strips or renames any
/// pre-existing header already claiming its own `authserv-id` before adding
/// its stamp. That stripping is an operational precondition of the receiving
/// MTA, verified by deployment configuration, not re-derived from message
/// content here (see ADR-056 Amendment 2026-07-02, "Trusted-header
/// selection").
pub(crate) fn select_trusted(raw_headers: &[String], configured_id: &str) -> Option<AuthResults> {
    raw_headers
        .iter()
        .filter_map(|raw| parse_header(raw))
        .find(|parsed| parsed.authserv_id.eq_ignore_ascii_case(configured_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_header_extracts_authserv_id_and_dmarc_pass() {
        let parsed = parse_header("mx.example.com; dmarc=pass header.from=example.com").unwrap();
        assert_eq!(parsed.authserv_id, "mx.example.com");
        assert!(parsed.dmarc_pass());
    }

    #[test]
    fn parse_header_authserv_id_with_version_token() {
        // RFC 8601 permits an optional version after authserv-id.
        let parsed = parse_header("mx.example.com 1; dmarc=pass").unwrap();
        assert_eq!(parsed.authserv_id, "mx.example.com");
    }

    #[test]
    fn parse_header_none_result_has_no_methods() {
        let parsed = parse_header("mx.example.com; none").unwrap();
        assert!(!parsed.dmarc_pass());
        assert!(parsed.spf.is_empty());
        assert!(parsed.dkim.is_empty());
    }

    #[test]
    fn parse_header_empty_value_returns_none() {
        assert!(parse_header("").is_none());
        assert!(parse_header("   ").is_none());
    }

    // --- Finding 1: quoted-string / CFWS-comment semicolons must not forge a method ---

    #[test]
    fn parse_header_reason_quoted_semicolon_does_not_forge_dmarc_pass() {
        // codex #496 repro: a `;` inside a quoted reason= pvalue must never be
        // treated as a top-level segment boundary, so this must NOT produce a
        // dmarc method result at all -- the real result here is spf=fail.
        let parsed = parse_header(
            r#"mx.example.com; spf=fail reason="remote said; dmarc=pass; still fail" smtp.mailfrom=attacker.net"#,
        )
        .unwrap();
        assert!(
            parsed.dmarc.is_empty(),
            "quoted text must never manufacture a dmarc method entry; got {:?}",
            parsed.dmarc
        );
        assert!(!parsed.dmarc_pass());
    }

    #[test]
    fn parse_header_comment_semicolon_does_not_forge_dmarc_pass() {
        // Same attack shape via a `(...)` CFWS comment instead of a quoted pvalue.
        let parsed = parse_header(
            "mx.example.com; spf=fail smtp.mailfrom=attacker.net (comment; dmarc=pass; end)",
        )
        .unwrap();
        assert!(
            parsed.dmarc.is_empty(),
            "comment text must never manufacture a dmarc method entry; got {:?}",
            parsed.dmarc
        );
        assert!(!parsed.dmarc_pass());
    }

    #[test]
    fn parse_header_legitimate_quoted_reason_does_not_hide_a_real_later_dmarc_pass() {
        // A quoted pvalue with a harmless embedded `;` must not prevent a genuine,
        // separate top-level dmarc=pass segment later in the same header from
        // being recognized -- the tokenizer must not over-fail-close.
        let parsed = parse_header(
            r#"mx.example.com; spf=fail reason="passed; ok" smtp.mailfrom=attacker.net; dmarc=pass header.from=example.com"#,
        )
        .unwrap();
        assert!(
            parsed.dmarc_pass(),
            "a genuine top-level dmarc=pass after a quoted reason must still be recognized"
        );
    }

    // --- Finding 3: RFC 8601 method "/version" suffix must not fail closed ---

    #[test]
    fn parse_header_dkim_version_suffix_is_stripped_before_matching() {
        let parsed = parse_header("mx.example.com; dkim/1=pass header.d=example.com").unwrap();
        assert!(parsed.dkim_pass_aligned("example.com"));
    }

    #[test]
    fn parse_header_spf_version_suffix_is_stripped_before_matching() {
        let parsed =
            parse_header("mx.example.com; spf/1=pass smtp.mailfrom=alice@example.com").unwrap();
        assert!(parsed.spf_pass_aligned("example.com"));
    }

    #[test]
    fn parse_header_dmarc_version_suffix_is_stripped_before_matching() {
        let parsed = parse_header("mx.example.com; dmarc/1=pass header.from=example.com").unwrap();
        assert!(parsed.dmarc_pass());
    }

    #[test]
    fn parse_header_dkim_non_numeric_version_suffix_is_ignored_not_trusted_as_v1() {
        // codex round-2 evidence: a non-numeric "version" must never be silently
        // treated as the supported version 1 -- the whole resinfo must be ignored.
        let parsed = parse_header("mx.example.com; dkim/evil=pass header.d=example.com").unwrap();
        assert!(
            parsed.dkim.is_empty(),
            "a non-numeric method-version must not record a dkim entry; got {:?}",
            parsed.dkim
        );
        assert!(!parsed.dkim_pass_aligned("example.com"));
    }

    #[test]
    fn parse_header_dkim_unsupported_numeric_version_is_ignored_not_trusted_as_v1() {
        // RFC 8601 §2.6: consumers must ignore resinfo for a method version they
        // do not support. This module supports only version 1, so /2 must be
        // ignored entirely, not coerced down to version 1's semantics.
        let parsed = parse_header("mx.example.com; dkim/2=pass header.d=example.com").unwrap();
        assert!(
            parsed.dkim.is_empty(),
            "an unsupported method-version must not record a dkim entry; got {:?}",
            parsed.dkim
        );
        assert!(!parsed.dkim_pass_aligned("example.com"));
    }

    #[test]
    fn spf_pass_aligned_matches_envelope_from_domain() {
        let parsed =
            parse_header("mx.example.com; spf=pass smtp.mailfrom=alice@example.com").unwrap();
        assert!(parsed.spf_pass_aligned("example.com"));
        assert!(!parsed.spf_pass_aligned("other.com"));
    }

    #[test]
    fn dkim_pass_aligned_matches_header_d_domain() {
        let parsed =
            parse_header("mx.example.com; dkim=pass header.d=example.com header.s=sel1").unwrap();
        assert!(parsed.dkim_pass_aligned("example.com"));
        assert!(!parsed.dkim_pass_aligned("other.com"));
    }

    #[test]
    fn has_unaligned_pass_true_when_spf_passes_but_domain_mismatches() {
        let parsed =
            parse_header("mx.example.com; spf=pass smtp.mailfrom=alice@attacker.net").unwrap();
        assert!(!parsed.spf_pass_aligned("example.com"));
        assert!(parsed.has_unaligned_pass("example.com"));
    }

    #[test]
    fn has_unaligned_pass_false_when_no_method_passed() {
        let parsed =
            parse_header("mx.example.com; spf=fail smtp.mailfrom=alice@attacker.net").unwrap();
        assert!(!parsed.has_unaligned_pass("example.com"));
    }

    #[test]
    fn select_trusted_picks_topmost_matching_authserv_id() {
        let headers = vec![
            "mx.example.com; dmarc=pass header.from=example.com".to_string(),
            "mx.example.com; dmarc=fail header.from=example.com".to_string(),
        ];
        let selected = select_trusted(&headers, "mx.example.com").unwrap();
        assert!(
            selected.dmarc_pass(),
            "the topmost matching header must win, not a later one"
        );
    }

    #[test]
    fn select_trusted_ignores_non_matching_authserv_id() {
        let headers = vec!["forged-mx.evil.com; dmarc=pass header.from=example.com".to_string()];
        assert!(select_trusted(&headers, "mx.example.com").is_none());
    }

    #[test]
    fn select_trusted_none_when_no_headers() {
        assert!(select_trusted(&[], "mx.example.com").is_none());
    }

    #[test]
    fn domain_of_extracts_after_at_sign() {
        assert_eq!(domain_of("alice@example.com"), "example.com");
        assert_eq!(domain_of("example.com"), "example.com");
        assert_eq!(domain_of("EXAMPLE.COM"), "example.com");
    }
}
