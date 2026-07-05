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
    let first_token = first_segment.split_whitespace().next()?;

    // A valid RFC 8601 authserv-id can never contain `=`; a resinfo always
    // begins `method[/version]=result`. An unquoted `=` in the first
    // whitespace token is therefore unambiguous evidence that this boundary
    // emits no authserv-id at all, regardless of whether an optional
    // method-version suffix (`method/version=result`) is also present --
    // the `=` survives that suffix either way.
    let is_no_authserv_id_form = first_token.contains('=');

    let (authserv_id, method_segments): (Option<String>, Box<dyn Iterator<Item = String>>) =
        if is_no_authserv_id_form {
            (
                None,
                Box::new(std::iter::once(first_segment).chain(all_segments)),
            )
        } else {
            (Some(first_token.to_string()), Box::new(all_segments))
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

    // --- Finding 1: quoted-string / CFWS-comment semicolons must not forge a method ---

    #[test]
    fn parse_header_reason_quoted_semicolon_does_not_forge_dmarc_pass() {
        // review #496 repro: a `;` inside a quoted reason= pvalue must never be
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
        // internal review round 2 evidence: a non-numeric "version" must never be silently
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
    /// Online stamps on its internal hop, taken verbatim from the real fixture
    /// `/Users/lion/projects/local workspace artifact`
    /// (unfolded), minus the leading `Authentication-Results: ` field name.
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
}
