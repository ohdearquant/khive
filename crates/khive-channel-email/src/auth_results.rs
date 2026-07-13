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

use crate::config::TrustAnchor;

/// One `resinfo` entry: a single method's verdict plus its properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MethodResult {
    pub result: String,
    pub props: HashMap<String, String>,
}

/// A parsed `Authentication-Results` header value.
///
/// `authserv_id: None` means the no-authserv-id form (e.g. Exchange Online's
/// internal-hop stamp); `Some(id)` means an RFC 8601-compliant boundary's id.
/// `parse_header` only ever produces `Some` from a non-empty first token, so
/// `Some` is always non-empty -- this is a type-level invariant, not just a
/// convention: an "empty configured id" can never be *represented* as a
/// matchable authserv_id, let alone accidentally match one (example actor, design review,
/// 2026-07-03: "guards decay; types don't").
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct AuthResults {
    pub authserv_id: Option<String>,
    pub dmarc: Vec<MethodResult>,
    pub spf: Vec<MethodResult>,
    pub dkim: Vec<MethodResult>,
}

impl AuthResults {
    /// `dmarc=pass` is present, without regard to `header.from` alignment.
    ///
    /// This is the raw method verdict only. The attribution gate does NOT use
    /// this directly -- reading `dmarc=pass` without confirming its
    /// `header.from` equals the From domain the gate is about to attribute
    /// mail to is a latent alignment gap (a dmarc=pass entry's `header.from`
    /// is the domain DMARC itself aligned against, which is not necessarily
    /// the domain the gate is attributing to). Use [`Self::dmarc_pass_aligned`]
    /// for the gate's aligned check; this method remains for callers (tests,
    /// diagnostics) that want the unaligned raw verdict.
    // Only exercised by `#[cfg(test)]` callers now that the gate uses
    // `dmarc_pass_aligned`; not dead code, just test/diagnostic-only.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn dmarc_pass(&self) -> bool {
        self.dmarc
            .iter()
            .any(|e| e.result.eq_ignore_ascii_case("pass"))
    }

    /// `dmarc=pass` is present AND its `header.from` domain matches `from_domain`.
    pub fn dmarc_pass_aligned(&self, from_domain: &str) -> bool {
        Self::method_pass_aligned(&self.dmarc, "header.from", from_domain)
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

/// Split one top-level `resinfo` segment (already produced by
/// [`split_top_level_segments`], so comments are already stripped and only
/// quoted semicolons can remain) into whitespace-delimited tokens, tracking
/// RFC 5322 quoted-string state (with `\` escapes) so quoted whitespace is
/// never treated as a token boundary.
///
/// This is the second of two delimiter layers: the segment scanner above
/// finds `;` boundaries; this one finds whitespace boundaries within a
/// segment. Both share the same quoted-pair semantics (`\` plus the
/// following character is one atomic unit, regardless of what that character
/// is), but this layer never sees `(...)` comments -- those were already
/// discarded by the segment scanner. A token is returned verbatim, including
/// any retained quote and backslash characters; it is never unquoted.
///
/// Malformed input (an unmatched `"`, or a `\` as the final character while
/// quoted) is handled conservatively: the remainder of the segment is
/// retained as one atomic token through EOF rather than resuming whitespace
/// splitting, so a malformed quoted tail can never be reinterpreted as
/// additional tokens -- see `split_top_level_ws_keeps_malformed_quoted_tail_atomic`.
fn split_top_level_ws(segment: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = segment.chars();

    while let Some(c) = chars.next() {
        if in_quotes {
            current.push(c);
            match c {
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                '"' => in_quotes = false,
                _ => {}
            }
            continue;
        }

        match c {
            '"' => {
                in_quotes = true;
                current.push(c);
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

/// Returns true if `target` occurs anywhere in `token` *outside* of a
/// quoted-string span, using the same quoted-pair (`\`) escaping semantics
/// as [`split_top_level_ws`]. A plain `str::contains` would treat a `=`
/// inside a quoted value (e.g. a quoted `authserv-id` like `"id=foo"`, which
/// RFC 8601 §2.2 permits as a valid `value`) the same as an unquoted `=`,
/// even though the tokenizer that produced `token` already knows the
/// difference -- this keeps the classification consistent with that state.
fn contains_unquoted(token: &str, target: char) -> bool {
    let mut in_quotes = false;
    let mut chars = token.chars();

    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '\\' => {
                    chars.next();
                }
                '"' => in_quotes = false,
                _ => {}
            }
            continue;
        }

        match c {
            '"' => in_quotes = true,
            c if c == target => return true,
            _ => {}
        }
    }

    false
}

/// Parse one raw `Authentication-Results` header value.
///
/// Detects two shapes for the first `resinfo`-or-authserv-id segment:
///
/// - **RFC 8601 form**: the first whitespace token of the first top-level
///   segment is the `authserv-id` (a dot-atom/value that can never contain an
///   unquoted `=`). The remaining segments are parsed as `resinfo` entries.
/// - **No-authserv-id form** (observed from Exchange Online's internal-hop
///   stamp): the first whitespace token of the first segment itself contains
///   an unquoted `=`, which is impossible for a valid authserv-id and
///   unambiguous for a `resinfo` (`method[/version]=result`). In this case
///   `authserv_id` is set to `None` and segment 0 is parsed as a `resinfo`
///   entry through the *same* loop as every other segment -- it is never
///   discarded as a (nonexistent) authserv-id.
///
/// Returns `None` when no signal can be extracted at all: an empty/whitespace
/// header (no first token), or -- in the no-authserv-id form only -- a header
/// whose segments contain no recognized `dmarc`/`spf`/`dkim` method entry
/// anywhere. In the RFC 8601 form, a non-empty authserv-id with no method the
/// gate recognizes still parses successfully to an `AuthResults` with empty
/// method vectors (unchanged from prior behavior) -- the gate treats "no
/// recognized passing method" uniformly regardless of which of those it was.
pub(crate) fn parse_header(raw: &str) -> Option<AuthResults> {
    let mut all_segments = split_top_level_segments(raw).into_iter();
    let first_segment = all_segments.next()?;
    let first_token = split_top_level_ws(&first_segment).into_iter().next()?;

    // A valid RFC 8601 authserv-id can never contain an *unquoted* `=`; a
    // resinfo always begins `method[/version]=result`, also unquoted. An
    // unquoted `=` in the first whitespace token is therefore unambiguous
    // evidence that this boundary emits no authserv-id at all, regardless of
    // whether an optional method-version suffix (`method/version=result`) is
    // also present -- the `=` survives that suffix either way. This must use
    // the same quote-state machine as `split_top_level_ws` (via
    // `contains_unquoted`), not a raw `str::contains`: RFC 8601 §2.2 permits
    // a quoted-string `authserv-id` value, and a `=` sealed inside that
    // quoting is not a resinfo delimiter -- treating it as one would let a
    // quoted authserv-id be misclassified as the no-authserv-id form, which
    // `TrustAnchor::TopmostNoAuthservId` treats as a strictly weaker,
    // position-only trust signal than a real authserv-id match.
    let is_no_authserv_id_form = contains_unquoted(&first_token, '=');

    let (authserv_id, method_segments): (Option<String>, Box<dyn Iterator<Item = String>>) =
        if is_no_authserv_id_form {
            (
                None,
                Box::new(std::iter::once(first_segment).chain(all_segments)),
            )
        } else {
            (Some(first_token), Box::new(all_segments))
        };

    let mut out = AuthResults {
        authserv_id,
        ..Default::default()
    };
    let mut recognized_any = false;

    for segment in method_segments {
        let segment = segment.trim();
        if segment.is_empty() || segment.eq_ignore_ascii_case("none") {
            continue;
        }
        let mut tokens = split_top_level_ws(segment).into_iter();
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
        let method_lower = method.to_lowercase();
        if !matches!(method_lower.as_str(), "dmarc" | "spf" | "dkim") {
            continue;
        }
        recognized_any = true;

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
        match method_lower.as_str() {
            "dmarc" => out.dmarc.push(entry),
            "spf" => out.spf.push(entry),
            "dkim" => out.dkim.push(entry),
            _ => unreachable!("filtered to dmarc|spf|dkim above"),
        }
    }

    // In the no-authserv-id form there is no authserv-id signal at all, so a
    // header that also carries no recognized method entry is genuine zero
    // signal (equivalent to the empty/malformed case) and must fail closed to
    // `None`, not a vacuously-successful empty `AuthResults`.
    if is_no_authserv_id_form && !recognized_any {
        return None;
    }

    Some(out)
}

/// Select the trusted `Authentication-Results` header per the configured
/// [`TrustAnchor`] (ADR-056 Amendment 2026-07-03, "EXO no-authserv-id trust
/// anchor").
///
/// - [`TrustAnchor::AuthservId`]: the first (topmost) header, in document
///   order, whose `authserv-id` matches the configured id (case-insensitive).
///   Topmost wins: a receiving MTA prepends its own stamp on each hop, so the
///   header nearest the top of the document is the one added by the final,
///   trusted receiving boundary -- PROVIDED that boundary strips or renames
///   any pre-existing header already claiming its own `authserv-id` before
///   adding its stamp. That stripping is an operational precondition of the
///   receiving MTA, verified by deployment configuration, not re-derived from
///   message content here.
/// - [`TrustAnchor::TopmostNoAuthservId`]: the boundary emits no authserv-id
///   at all (e.g. Exchange Online's internal-hop stamp), so position is the
///   *sole* discriminator. Only the literal topmost `Authentication-Results`
///   header (`raw_headers[0]`) is ever considered -- never a later one, even
///   if the topmost fails to parse. It is trusted only if it parses AND is
///   itself in the no-authserv-id form (`authserv_id.is_none()`); if the
///   topmost carries any authserv-id, or fails to parse, that violates the
///   invariant that this boundary's own stamp is topmost and unadorned, so the
///   message quarantines (fails closed) rather than falling through to a
///   lower header.
pub(crate) fn select_trusted(raw_headers: &[String], anchor: &TrustAnchor) -> Option<AuthResults> {
    match anchor {
        TrustAnchor::AuthservId(configured_id) => raw_headers
            .iter()
            .filter_map(|raw| parse_header(raw))
            .find(|parsed| {
                parsed
                    .authserv_id
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(configured_id))
            }),
        TrustAnchor::TopmostNoAuthservId => {
            let topmost = parse_header(raw_headers.first()?)?;
            if topmost.authserv_id.is_none() {
                Some(topmost)
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_header_extracts_authserv_id_and_dmarc_pass() {
        let parsed = parse_header("mx.example.com; dmarc=pass header.from=example.com").unwrap();
        assert_eq!(parsed.authserv_id.as_deref(), Some("mx.example.com"));
        assert!(parsed.dmarc_pass());
    }

    #[test]
    fn parse_header_authserv_id_with_version_token() {
        // RFC 8601 permits an optional version after authserv-id.
        let parsed = parse_header("mx.example.com 1; dmarc=pass").unwrap();
        assert_eq!(parsed.authserv_id.as_deref(), Some("mx.example.com"));
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

    // --- quoted-string / CFWS-comment semicolons must not forge a method ---

    #[test]
    fn parse_header_reason_quoted_semicolon_does_not_forge_dmarc_pass() {
        // A `;` inside a quoted reason= pvalue must never be
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

    // --- RFC 8601 method "/version" suffix must not fail closed ---

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
        // A non-numeric "version" must never be silently
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
        let anchor = TrustAnchor::AuthservId("mx.example.com".to_string());
        let selected = select_trusted(&headers, &anchor).unwrap();
        assert!(
            selected.dmarc_pass(),
            "the topmost matching header must win, not a later one"
        );
    }

    #[test]
    fn select_trusted_ignores_non_matching_authserv_id() {
        let headers = vec!["forged-mx.evil.com; dmarc=pass header.from=example.com".to_string()];
        let anchor = TrustAnchor::AuthservId("mx.example.com".to_string());
        assert!(select_trusted(&headers, &anchor).is_none());
    }

    #[test]
    fn select_trusted_none_when_no_headers() {
        let anchor = TrustAnchor::AuthservId("mx.example.com".to_string());
        assert!(select_trusted(&[], &anchor).is_none());
    }

    #[test]
    fn domain_of_extracts_after_at_sign() {
        assert_eq!(domain_of("alice@example.com"), "example.com");
        assert_eq!(domain_of("example.com"), "example.com");
        assert_eq!(domain_of("EXAMPLE.COM"), "example.com");
    }

    // --- EXO no-authserv-id trust anchor (ADR-056 Amendment 2026-07-03) ---

    /// The plain (non-ARC) `Authentication-Results` header value Exchange
    /// Online stamps on its internal hop, taken verbatim from a local test
    /// mailbox fixture (unfolded), minus the leading `Authentication-Results: `
    /// field name.
    const EXO_FIXTURE_HEADER_VALUE: &str = "spf=pass (sender IP is 2607:f8b0:4864:20::1129) smtp.mailfrom=gmail.com; dkim=pass (signature was verified) header.d=gmail.com;dmarc=pass action=none header.from=gmail.com;compauth=pass reason=100";

    #[test]
    fn parse_header_exo_fixture_has_empty_authserv_id_and_recognizes_all_methods() {
        let parsed = parse_header(EXO_FIXTURE_HEADER_VALUE).unwrap();
        assert!(
            parsed.authserv_id.is_none(),
            "EXO's plain A-R stamp carries no authserv-id"
        );
        assert!(
            parsed.dmarc_pass(),
            "segment 0 (spf) must not have eaten the dmarc segment; dmarc=pass must be recognized"
        );
        assert!(
            parsed.spf_pass_aligned("gmail.com"),
            "segment 0 itself (spf=pass smtp.mailfrom=gmail.com) must be parsed as a method, not consumed as authserv-id"
        );
    }

    #[test]
    fn select_trusted_topmost_no_authserv_id_mode_picks_topmost_no_id_header() {
        let headers = vec![EXO_FIXTURE_HEADER_VALUE.to_string()];
        let selected = select_trusted(&headers, &TrustAnchor::TopmostNoAuthservId).unwrap();
        assert!(selected.dmarc_pass());
        assert!(selected.spf_pass_aligned("gmail.com"));
    }

    #[test]
    fn select_trusted_topmost_no_authserv_id_mode_quarantines_when_topmost_carries_an_id() {
        // The topmost header unexpectedly carries an authserv-id -- this
        // violates the invariant that EXO's plain stamp is topmost and
        // unadorned, so it must quarantine (None), not fall through.
        let headers = vec!["mx.example.com; dmarc=pass header.from=example.com".to_string()];
        assert!(select_trusted(&headers, &TrustAnchor::TopmostNoAuthservId).is_none());
    }

    #[test]
    fn select_trusted_topmost_no_authserv_id_mode_forged_second_header_does_not_override_topmost() {
        // EXO's genuine no-id dmarc=fail stamp is topmost; a forged no-id
        // dmarc=pass header sits below it. Position alone decides -- the
        // topmost (fail) must be the one selected, staying quarantined.
        let headers = vec![
            "spf=fail smtp.mailfrom=evil.com; dmarc=fail header.from=gmail.com".to_string(),
            "dmarc=pass header.from=gmail.com".to_string(),
        ];
        let selected = select_trusted(&headers, &TrustAnchor::TopmostNoAuthservId).unwrap();
        assert!(
            !selected.dmarc_pass(),
            "the topmost (failing) no-id header must win over a forged passing header below it"
        );
    }

    #[test]
    fn parse_header_empty_and_whitespace_still_none() {
        assert!(parse_header("").is_none());
        assert!(parse_header("   ").is_none());
    }

    #[test]
    fn dmarc_pass_aligned_rejects_mismatched_header_from_domain() {
        let parsed = parse_header("mx.example.com; dmarc=pass header.from=attacker.net").unwrap();
        assert!(parsed.dmarc_pass(), "raw dmarc_pass is unaligned by design");
        assert!(
            !parsed.dmarc_pass_aligned("example.com"),
            "dmarc_pass_aligned must reject a header.from domain that does not match the message From domain"
        );
    }

    #[test]
    fn authserv_id_mode_no_id_header_never_matches_a_configured_id_fail_closed() {
        // An attacker header shaped like `evil=x; dmarc=pass ...` now parses to
        // the no-id form (authserv_id == ""). In AuthservId mode, "" must never
        // match a configured non-empty id -- no new bypass.
        let headers = vec!["evil=x; dmarc=pass header.from=gmail.com".to_string()];
        let parsed = parse_header(&headers[0]).unwrap();
        assert!(parsed.authserv_id.is_none());
        assert!(parsed.dmarc_pass());

        let anchor = TrustAnchor::AuthservId("mx.example.com".to_string());
        assert!(
            select_trusted(&headers, &anchor).is_none(),
            "no-id-form header must never match a configured non-empty authserv-id"
        );
    }

    #[test]
    fn select_trusted_authserv_id_mode_empty_configured_id_never_matches_no_id_header() {
        // the exact review point (design review 2026-07-03), independent of the
        // config-layer require_nonempty_env guard: construct
        // TrustAnchor::AuthservId(String::new()) directly, bypassing
        // from_env entirely, against a header that parses to
        // authserv_id == None. Even if config validation were somehow
        // bypassed, an empty configured id can never match a no-id header --
        // there is no `"" == ""` comparison left to perform, because a no-id
        // header's authserv_id is not the empty string, it is the *absence*
        // of a value.
        let headers = vec![EXO_FIXTURE_HEADER_VALUE.to_string()];
        let parsed = parse_header(&headers[0]).unwrap();
        assert!(parsed.authserv_id.is_none());

        let anchor = TrustAnchor::AuthservId(String::new());
        assert!(
            select_trusted(&headers, &anchor).is_none(),
            "an empty configured authserv-id must never match a no-id header, \
             even bypassing from_env's require_nonempty_env guard entirely"
        );
    }

    #[test]
    fn parse_header_no_id_form_with_zero_recognized_method_is_none() {
        // No authserv-id (first token contains '=') AND no recognized
        // dmarc/spf/dkim method anywhere -- genuine zero signal, must be None.
        assert!(parse_header("evil=x").is_none());
    }

    // --- #501: quote/backslash-aware whitespace tokenization ---

    #[test]
    fn split_top_level_ws_preserves_unquoted_whitespace_behavior() {
        let tokens = split_top_level_ws(
            " \tspf=pass  smtp.mailfrom=alice@example.com\u{00a0}header.from=example.com \n",
        );
        assert_eq!(
            tokens,
            vec![
                "spf=pass",
                "smtp.mailfrom=alice@example.com",
                "header.from=example.com"
            ]
        );
    }

    #[test]
    fn split_top_level_ws_preserves_empty_quoted_value_and_drops_empty_fields() {
        let tokens = split_top_level_ws(r#"  spf=pass   reason=""  smtp.mailfrom=evil.com  "#);
        assert_eq!(
            tokens,
            vec!["spf=pass", r#"reason="""#, "smtp.mailfrom=evil.com"]
        );
    }

    #[test]
    fn split_top_level_ws_keeps_quoted_whitespace_atomic() {
        let tokens = split_top_level_ws(r#"spf=pass smtp.mailfrom="a b c"@evil.com extra=1"#);
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[1], r#"smtp.mailfrom="a b c"@evil.com"#);
    }

    #[test]
    fn split_top_level_ws_escaped_quote_keeps_quote_open() {
        let tokens = split_top_level_ws(
            r#"spf=pass reason="said \"still quoted smtp.mailfrom=khive.ai" smtp.mailfrom=evil.com"#,
        );
        assert_eq!(tokens.len(), 3);
        assert!(tokens[1].contains("smtp.mailfrom=khive.ai"));
        assert_eq!(tokens[2], "smtp.mailfrom=evil.com");
    }

    #[test]
    fn split_top_level_ws_escaped_backslash_allows_later_quote_to_close() {
        let tokens = split_top_level_ws(r#"spf=pass reason="path \\" smtp.mailfrom=evil.com"#);
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[1].matches('\\').count(), 2);
        assert_eq!(tokens[2], "smtp.mailfrom=evil.com");
    }

    #[test]
    fn split_top_level_ws_keeps_malformed_quoted_tail_atomic() {
        let unclosed_mid =
            split_top_level_ws(r#"spf=pass reason="unterminated smtp.mailfrom=khive.ai"#);
        assert_eq!(unclosed_mid.len(), 2);

        let trailing_backslash = r#"reason="unterminated\"#;
        let tokens = split_top_level_ws(trailing_backslash);
        assert_eq!(tokens, vec![trailing_backslash.to_string()]);
    }

    #[test]
    fn parse_header_uses_quote_atomic_first_token() {
        let parsed = parse_header(r#""mx example.com"; none"#).unwrap();
        assert_eq!(parsed.authserv_id.as_deref(), Some(r#""mx example.com""#));
        assert!(parsed.dmarc.is_empty());
        assert!(parsed.spf.is_empty());
        assert!(parsed.dkim.is_empty());
    }

    #[test]
    fn parse_header_quoted_whitespace_in_smtp_mailfrom_does_not_forge_spf_alignment() {
        let parsed = parse_header(
            r#"mx.example.com; spf=pass smtp.mailfrom="attacker smtp.mailfrom=khive.ai "@evil.com"#,
        )
        .unwrap();
        assert!(!parsed.spf_pass_aligned("khive.ai"));
        assert!(parsed.spf_pass_aligned("evil.com"));
        assert_eq!(parsed.spf.len(), 1);
        assert_eq!(parsed.spf[0].props.len(), 1);
    }

    #[test]
    fn parse_header_quoted_whitespace_in_header_from_does_not_forge_dmarc_alignment() {
        let parsed = parse_header(
            r#"mx.example.com; dmarc=pass header.from="attacker header.from=khive.ai "@evil.com"#,
        )
        .unwrap();
        assert!(!parsed.dmarc_pass_aligned("khive.ai"));
        assert!(parsed.dmarc_pass_aligned("evil.com"));
        assert_eq!(parsed.dmarc.len(), 1);
        assert_eq!(parsed.dmarc[0].props.len(), 1);
    }

    #[test]
    fn parse_header_quoted_whitespace_in_header_d_does_not_forge_dkim_alignment() {
        let parsed = parse_header(
            r#"mx.example.com; dkim=pass header.d="evil.com header.d=khive.ai " header.s=sel1"#,
        )
        .unwrap();
        assert!(!parsed.dkim_pass_aligned("khive.ai"));
        assert_eq!(
            parsed.dkim[0].props.get("header.d").map(String::as_str),
            Some(r#""evil.com header.d=khive.ai ""#)
        );
        assert_eq!(
            parsed.dkim[0].props.get("header.s").map(String::as_str),
            Some("sel1")
        );
    }

    #[test]
    fn parse_header_unclosed_quote_does_not_forge_later_property() {
        let parsed = parse_header(
            r#"mx.example.com; spf=pass smtp.mailfrom=evil.com reason="unterminated smtp.mailfrom=khive.ai"#,
        )
        .unwrap();
        assert!(parsed.spf_pass_aligned("evil.com"));
        assert!(!parsed.spf_pass_aligned("khive.ai"));
        assert!(parsed.spf[0]
            .props
            .get("reason")
            .is_some_and(|v| v.contains("smtp.mailfrom=khive.ai")));
    }

    #[test]
    fn parse_header_explicit_top_level_duplicate_property_remains_last_write_wins() {
        let parsed =
            parse_header("mx.example.com; spf=pass smtp.mailfrom=evil.com smtp.mailfrom=khive.ai")
                .unwrap();
        assert!(parsed.spf_pass_aligned("khive.ai"));
        assert!(!parsed.spf_pass_aligned("evil.com"));
    }

    #[test]
    fn parse_header_first_token_escaped_quote_with_whitespace_stays_atomic() {
        // The first token contains an escaped quote (`\"`) followed by more
        // whitespace-separated text before the real closing quote. The escape
        // must keep the token open across both the escaped quote and the
        // whitespace around it, so the whole quoted string -- not a
        // whitespace- or escaped-quote-truncated prefix -- becomes authserv_id.
        let raw = r#""id \"part\" two"; none"#;
        let parsed = parse_header(raw).unwrap();
        assert_eq!(
            parsed.authserv_id.as_deref(),
            Some(r#""id \"part\" two""#),
            "escaped quotes and internal whitespace must not truncate the first token"
        );
    }

    #[test]
    fn parse_header_no_authserv_id_form_quoted_whitespace_injection_does_not_forge_spf_alignment() {
        // Same quoted-whitespace injection shape as the RFC-id-form regressions
        // above, but in the no-authserv-id form (segment 0 itself is the first
        // resinfo, so the injection lands in the very token that decides
        // is_no_authserv_id_form). Must not forge alignment either way.
        let parsed = parse_header(
            r#"spf=pass smtp.mailfrom="attacker smtp.mailfrom=khive.ai "@evil.com; dkim=pass header.d=example.com"#,
        )
        .unwrap();
        assert!(parsed.authserv_id.is_none());
        assert!(!parsed.spf_pass_aligned("khive.ai"));
        assert!(parsed.spf_pass_aligned("evil.com"));
        assert_eq!(parsed.spf[0].props.len(), 1);
        assert!(parsed.dkim_pass_aligned("example.com"));
    }

    #[test]
    fn parse_header_escaped_whitespace_inside_quoted_property_remains_one_token() {
        // A backslash-escaped space just before embedded injected-looking text,
        // still inside the same quoted pvalue. The escape must not create a
        // token boundary at the escaped space, and the injected-looking text
        // must never become its own property.
        let parsed = parse_header(
            r#"mx.example.com; spf=pass smtp.mailfrom=evil.com reason="a\ b header.from=evil""#,
        )
        .unwrap();
        assert_eq!(parsed.spf.len(), 1);
        assert!(parsed.spf[0]
            .props
            .get("reason")
            .is_some_and(|v| v == r#""a\ b header.from=evil""#));
        assert!(
            !parsed.spf[0].props.contains_key("header.from"),
            "escaped whitespace inside a quoted reason must not let embedded text become its own property"
        );
        assert!(parsed.spf_pass_aligned("evil.com"));
    }

    // --- #501 remediation: reviewer finding (MEDIUM) -- quoted `=` must not
    // be misclassified as the no-authserv-id form ---

    #[test]
    fn parse_header_quoted_authserv_id_containing_equals_is_not_no_id_form() {
        // review_501.md MEDIUM: `"id=foo"` is a quote-atomic first token (RFC
        // 8601 SS2.2 permits a quoted-string authserv-id value), so the `=` it
        // contains is sealed inside quoting and must never be read as the
        // resinfo-shaped `=` that signals the no-authserv-id form. Before the
        // fix, `contains('=')` discarded the tokenizer's own quote state and
        // classified this as no-id, letting the quoted-first-segment fall
        // through as an unrecognized method while the later dmarc=pass
        // segment was retained -- exactly the shape `TopmostNoAuthservId`
        // must reject.
        let raw = r#""id=foo"; dmarc=pass header.from=example.com"#;
        let parsed = parse_header(raw).unwrap();
        assert!(
            parsed.authserv_id.is_some(),
            "a quoted first token containing '=' must not produce the no-authserv-id form; got authserv_id={:?}",
            parsed.authserv_id
        );

        let headers = vec![raw.to_string()];
        assert!(
            select_trusted(&headers, &TrustAnchor::TopmostNoAuthservId).is_none(),
            "a header whose topmost segment visibly carries a (quoted) authserv-id must never be \
             accepted under the no-authserv-id trust anchor"
        );
    }

    #[test]
    fn parse_header_exo_fixture_still_recognized_as_no_id_form_after_quote_aware_fix() {
        // Guards against the quote-aware classification regressing the
        // genuine, unquoted no-authserv-id shape the EXO trust anchor exists
        // for.
        let parsed = parse_header(EXO_FIXTURE_HEADER_VALUE).unwrap();
        assert!(parsed.authserv_id.is_none());
        assert!(parsed.dmarc_pass());
        assert!(parsed.spf_pass_aligned("gmail.com"));
    }

    // --- #501 remediation: reviewer finding (LOW) -- nested/escaped comment
    // combined with an internal `;`/`method=result` must not forge a method ---

    #[test]
    fn parse_header_nested_escaped_comment_with_semicolon_does_not_forge_dmarc() {
        // review_501.md LOW, per the inventory's required regression
        // (auth_results_inventory.md:236): a nested comment containing an
        // escaped `)` (which must not prematurely close the inner nesting)
        // and an internal `; dmarc=pass` (which must stay swallowed by the
        // comment, not leak out as a real segment boundary) must not
        // manufacture a forged dmarc entry -- while a genuine, separate
        // dmarc=pass segment later in the same header must still parse.
        let parsed = parse_header(
            "mx.example.com; spf=fail (outer (nested \\) paren) ; dmarc=pass) smtp.mailfrom=evil.com; dmarc=pass header.from=example.com",
        )
        .unwrap();
        assert_eq!(
            parsed.dmarc.len(),
            1,
            "the nested-comment-internal '; dmarc=pass' must not manufacture a dmarc entry; got {:?}",
            parsed.dmarc
        );
        assert!(
            parsed.dmarc_pass_aligned("example.com"),
            "the genuine, separate top-level dmarc=pass segment after the comment must still be recognized"
        );
        assert_eq!(parsed.spf.len(), 1);
        assert_eq!(parsed.spf[0].result, "fail");
        assert!(
            parsed.spf[0]
                .props
                .get("smtp.mailfrom")
                .is_some_and(|v| v == "evil.com"),
            "text after the closed outer comment must still be parsed as the real smtp.mailfrom property"
        );
    }

    // --- #501 remediation: coverage gap (T09/T10) -- close
    // at the parse_header/props/alignment level, not just the tokenizer helper ---

    #[test]
    fn parse_header_escaped_quote_inside_quoted_property_does_not_forge_alignment() {
        // T09: an escaped quote inside a quoted
        // property value, followed by injected-looking key=value text still
        // inside the same quoted value, must never let that inner text become
        // its own property. Previously only exercised at the tokenizer-helper
        // level (split_top_level_ws_escaped_quote_keeps_quote_open); this
        // proves the same invariant survives all the way through the props
        // map and the alignment check.
        let parsed = parse_header(
            r#"mx.example.com; spf=pass reason="said \"still quoted smtp.mailfrom=khive.ai" smtp.mailfrom=evil.com"#,
        )
        .unwrap();
        assert_eq!(parsed.spf.len(), 1);
        assert_eq!(
            parsed.spf[0].props.get("smtp.mailfrom").map(String::as_str),
            Some("evil.com")
        );
        assert!(parsed.spf_pass_aligned("evil.com"));
        assert!(!parsed.spf_pass_aligned("khive.ai"));
    }

    #[test]
    fn parse_header_escaped_backslash_before_quote_in_property_does_not_break_later_property() {
        // T10: an escaped backslash immediately
        // before the closing quote of a property value (pair-consumption
        // parity) must not create a premature token boundary that corrupts
        // the real, later property. Previously only exercised at the
        // tokenizer-helper level
        // (split_top_level_ws_escaped_backslash_allows_later_quote_to_close);
        // this proves the same invariant at the props/alignment level.
        let parsed =
            parse_header(r#"mx.example.com; spf=pass reason="path \\" smtp.mailfrom=evil.com"#)
                .unwrap();
        assert_eq!(parsed.spf.len(), 1);
        assert_eq!(
            parsed.spf[0].props.get("smtp.mailfrom").map(String::as_str),
            Some("evil.com")
        );
        assert!(parsed.spf_pass_aligned("evil.com"));
    }
}
