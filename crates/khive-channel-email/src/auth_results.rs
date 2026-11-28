//! RFC 8601 `Authentication-Results` header parsing (structural subset).
//!
//! Hand-parsed because `mail-parser` has no structured support for this header.
//! Extracts only what the attribution gate needs: `authserv-id`, and the
//! `dmarc`/`spf`/`dkim` verdicts plus their `header.d` / `smtp.mailfrom` /
//! `header.from` alignment properties. CFWS-aware (quoted strings, nested
//! comments) so a `;` or `=` inside a quoted pvalue or comment can never forge
//! an extra segment. See crates/khive-channel-email/docs/api/auth-results.md
//! for the full parsing walkthrough and the two-shapes (RFC 8601 /
//! no-authserv-id) detection rationale.

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
/// matchable authserv_id, let alone accidentally match one. The type enforces
/// this invariant independently of defensive guards.
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

/// Split a raw header value into top-level `resinfo` segments on `;`,
/// quote/comment-aware. See crates/khive-channel-email/docs/api/auth-results.md#segment-splitting-split_top_level_segments.
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

/// Split one top-level `resinfo` segment (from [`split_top_level_segments`])
/// into whitespace-delimited tokens, quote-aware. Malformed quoted input is
/// retained as one atomic token through EOF rather than resuming whitespace
/// splitting. See crates/khive-channel-email/docs/api/auth-results.md#whitespace-tokenizing-split_top_level_ws.
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

/// RFC 8601 permits CFWS on either side of `=` and of the dot in a
/// `ptype.property` name. Comments were removed by the segment scanner;
/// remove only adjacent whitespace here, retaining quoted pvalues byte for
/// byte and leaving other whitespace for `split_top_level_ws` to tokenize.
/// Dot CFWS is valid only in property names, never an authserv-id or pvalue.
fn normalize_delimiter_cfws(segment: &str, property_names: bool) -> String {
    let chars: Vec<char> = segment.chars().collect();
    let mut normalized = String::with_capacity(segment.len());
    let mut in_quotes = false;
    let mut in_property_name = false;
    let mut in_value = false;
    let mut index = 0;
    while index < chars.len() {
        let current = chars[index];
        if in_quotes {
            normalized.push(current);
            if current == '\\' {
                if let Some(escaped) = chars.get(index + 1) {
                    normalized.push(*escaped);
                    index += 1;
                }
            } else if current == '"' {
                in_quotes = false;
            }
        } else if current == '"' {
            in_quotes = true;
            normalized.push(current);
        } else if current == '=' {
            in_property_name = false;
            in_value = true;
            normalized.push(current);
        } else if current.is_whitespace() {
            let start = index;
            while index + 1 < chars.len() && chars[index + 1].is_whitespace() {
                index += 1;
            }
            let next = chars.get(index + 1).copied();
            let previous = normalized.as_bytes().last().copied();
            let adjacent_to_equal = matches!(previous, Some(b'=')) || next == Some('=');
            let adjacent_dot = matches!(previous, Some(b'.')) || next == Some('.');
            let collapse_property_dot = property_names && in_property_name && adjacent_dot;
            if !adjacent_to_equal && !collapse_property_dot {
                normalized.extend(chars[start..=index].iter().copied());
                // A pvalue's malformed spaced dot is still part of that
                // value, not the start of a new property name.
                if !in_value || !adjacent_dot {
                    in_property_name = true;
                    in_value = false;
                }
            }
        } else {
            normalized.push(current);
        }
        index += 1;
    }
    normalized
}

/// True if `target` occurs in `token` outside a quoted-string span (same
/// quoted-pair semantics as [`split_top_level_ws`]) -- unlike `str::contains`,
/// distinguishes a quoted `=` from an unquoted one. See
/// crates/khive-channel-email/docs/api/auth-results.md#contains_unquoted.
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
/// Detects the RFC 8601 form (leading `authserv-id` token) vs. the
/// no-authserv-id form (Exchange Online's internal-hop stamp, where the first
/// token is itself a `resinfo`). Returns `None` only when no signal can be
/// extracted at all -- see crates/khive-channel-email/docs/api/auth-results.md#parse_header
/// for the full shape-detection contract and the empty-vs-zero-signal distinction.
pub(crate) fn parse_header(raw: &str) -> Option<AuthResults> {
    let mut all_segments = split_top_level_segments(raw).into_iter();
    let first_raw = all_segments.next()?;
    let first_segment = normalize_delimiter_cfws(&first_raw, false);
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
                Box::new(
                    std::iter::once(normalize_delimiter_cfws(&first_raw, true)).chain(
                        all_segments.map(|segment| normalize_delimiter_cfws(&segment, true)),
                    ),
                ),
            )
        } else {
            (
                Some(first_token),
                Box::new(all_segments.map(|segment| normalize_delimiter_cfws(&segment, true))),
            )
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
/// `TrustAnchor::AuthservId`: topmost header (document order) whose
/// `authserv-id` matches the configured id, case-insensitive.
/// `TrustAnchor::TopmostNoAuthservId`: ONLY `raw_headers[0]` is ever
/// considered (position is the sole discriminator when no authserv-id
/// exists); it is trusted only if it parses AND is itself in the
/// no-authserv-id form -- any authserv-id on it, or a parse failure, fails
/// closed to `None` rather than falling through to a lower header.
/// `TrustAnchor::TopmostWithAuthservId` (ADR-056 Amendment 2026-10-15): the
/// same positional rule, except that the topmost header may also carry the
/// configured `authserv-id`; any other id on it, or a parse failure, fails
/// closed the same way. See
/// crates/khive-channel-email/docs/api/auth-results.md#select_trusted for
/// the topmost-wins rationale and its receiving-MTA precondition.
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
        TrustAnchor::TopmostWithAuthservId(configured_id) => {
            let topmost = parse_header(raw_headers.first()?)?;
            match topmost.authserv_id.as_deref() {
                None => Some(topmost),
                Some(id) if id.eq_ignore_ascii_case(configured_id) => Some(topmost),
                Some(_) => None,
            }
        }
    }
}

/// Observe header presence without making either header family trusted.
/// Prefer the topmost plain header, whose family the attribution gate uses.
pub(crate) fn observed_auth_header<'a>(
    plain_headers: &'a [String],
    headers: &'a HashMap<String, String>,
    raw_bytes: &[u8],
) -> Option<(&'static str, Option<&'a str>)> {
    let plain = plain_headers
        .first()
        .map(String::as_str)
        .or_else(|| headers.get("authentication-results").map(String::as_str));
    if plain.is_some() || raw_header_present(raw_bytes, b"authentication-results") {
        return Some(("authentication-results", plain));
    }
    let arc = headers
        .get("arc-authentication-results")
        .map(String::as_str);
    if arc.is_some() || raw_header_present(raw_bytes, b"arc-authentication-results") {
        return Some(("arc-authentication-results", arc));
    }
    None
}

/// The parser can omit an empty header value. This name-only fallback stops at
/// the header/body boundary and never supplies a value to the trust selector.
fn raw_header_present(raw_bytes: &[u8], expected: &[u8]) -> bool {
    for line in raw_bytes.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            break;
        }
        if let Some(colon) = line.iter().position(|byte| *byte == b':') {
            if line[..colon].eq_ignore_ascii_case(expected) {
                return true;
            }
        }
    }
    false
}

/// Return values of matching raw headers after RFC 5322 continuation unfolding.
pub(crate) fn raw_header_values(raw_bytes: &[u8], expected: &[u8]) -> Vec<String> {
    let mut values = Vec::new();
    let mut current: Option<Vec<u8>> = None;
    for line in raw_bytes.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            break;
        }
        if line.first().is_some_and(u8::is_ascii_whitespace) {
            if let Some(value) = current.as_mut() {
                value.push(b' ');
                value.extend_from_slice(line.trim_ascii());
            }
            continue;
        }
        if let Some(value) = current.take() {
            values.push(String::from_utf8_lossy(&value).into_owned());
        }
        if let Some(colon) = line.iter().position(|byte| *byte == b':') {
            if line[..colon].eq_ignore_ascii_case(expected) {
                current = Some(line[colon + 1..].trim_ascii().to_vec());
            }
        }
    }
    if let Some(value) = current {
        values.push(String::from_utf8_lossy(&value).into_owned());
    }
    values
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ObservedAuthservId {
    Present { value: String, truncated: bool },
    NoAuthservId,
    Unparseable,
}

/// Extract only an observed identifier; method verdicts never leave this helper.
/// ARC's instance prefix is removed for disclosure, never for attribution.
pub(crate) fn observed_authserv_id(raw: Option<&str>, arc: bool) -> ObservedAuthservId {
    let Some(raw) = raw else {
        return ObservedAuthservId::Unparseable;
    };
    let segments = split_top_level_segments(raw);
    let segment = if arc {
        let Some(instance) = segments.first() else {
            return ObservedAuthservId::Unparseable;
        };
        let instance = normalize_delimiter_cfws(instance, false);
        let valid_instance = instance
            .trim()
            .split_once('=')
            .is_some_and(|(name, value)| {
                name.trim().eq_ignore_ascii_case("i")
                    && !value.is_empty()
                    && value.bytes().all(|byte| byte.is_ascii_digit())
                    && value.parse::<u32>().is_ok_and(|instance| instance > 0)
            });
        if !valid_instance {
            return ObservedAuthservId::Unparseable;
        }
        segments.get(1)
    } else {
        segments.first()
    };
    let Some(segment) = segment else {
        return ObservedAuthservId::Unparseable;
    };
    let segment = normalize_delimiter_cfws(segment, false);
    let Some(token) = split_top_level_ws(&segment).into_iter().next() else {
        return ObservedAuthservId::Unparseable;
    };
    if contains_unquoted(&token, '=') {
        return ObservedAuthservId::NoAuthservId;
    }
    let mut value = String::new();
    for character in token.chars() {
        let printable = character.escape_default().to_string();
        if value.len() + printable.len() > 128 {
            return ObservedAuthservId::Present {
                value,
                truncated: true,
            };
        }
        value.push_str(&printable);
    }
    ObservedAuthservId::Present {
        value,
        truncated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observed_authserv_ids_are_bounded_printable_and_never_select_arc() {
        let long = format!("{}; dmarc=pass", "x".repeat(300));
        assert_eq!(
            observed_authserv_id(Some(&long), false),
            ObservedAuthservId::Present {
                value: "x".repeat(128),
                truncated: true
            }
        );
        let quoted = "\"m\u{e9}x\u{1}id\" 1; dmarc=pass";
        let ObservedAuthservId::Present { value, truncated } =
            observed_authserv_id(Some(quoted), false)
        else {
            panic!("quoted identifier should be observed")
        };
        assert!(value.is_ascii() && value.bytes().all(|byte| byte.is_ascii_graphic()));
        assert!(value.contains("\\u{e9}") && value.contains("\\u{1}"));
        assert!(!truncated && value.len() <= 128);
        assert_eq!(
            observed_authserv_id(Some("i=2; mx.microsoft.com 1; dmarc=pass"), true),
            ObservedAuthservId::Present {
                value: "mx.microsoft.com".into(),
                truncated: false
            }
        );
        assert_eq!(
            observed_authserv_id(Some("i=bad; mx.example.com"), true),
            ObservedAuthservId::Unparseable
        );
        assert_eq!(
            observed_authserv_id(Some("spf=pass smtp.mailfrom=example.com"), false),
            ObservedAuthservId::NoAuthservId
        );
        assert_eq!(
            observed_authserv_id(Some(""), false),
            ObservedAuthservId::Unparseable
        );
        assert!(select_trusted(&[], &TrustAnchor::AuthservId("mx.microsoft.com".into())).is_none());
    }

    #[test]
    fn observed_header_presence_stops_at_body_and_ignores_continuation_names() {
        let empty = HashMap::new();
        assert_eq!(
            observed_auth_header(
                &[],
                &empty,
                b"Authentication-Results:\r\nFrom: a@example.com\r\n\r\nbody"
            ),
            Some(("authentication-results", None))
        );
        assert_eq!(
            observed_auth_header(&[], &empty, b"ARC-Authentication-Results:\r\n\r\nbody"),
            Some(("arc-authentication-results", None))
        );
        assert_eq!(observed_auth_header(&[], &empty, b"Subject: text\r\n Authentication-Results: forged\r\n\r\nARC-Authentication-Results: body"), None);
    }

    #[test]
    fn parse_header_extracts_authserv_id_and_dmarc_pass() {
        let parsed = parse_header("mx.example.com; dmarc=pass header.from=example.com").unwrap();
        assert_eq!(parsed.authserv_id.as_deref(), Some("mx.example.com"));
        assert!(parsed.dmarc_pass());
    }

    #[test]
    fn spaced_method_and_property_delimiters_preserve_alignment() {
        let parsed = parse_header(
            r#"mx.example.net; dmarc = pass header . from = example.com reason = "a = b""#,
        )
        .unwrap();
        assert_eq!(parsed.authserv_id.as_deref(), Some("mx.example.net"));
        assert_eq!(parsed.dmarc.len(), 1);
        assert_eq!(parsed.dmarc[0].result, "pass");
        assert_eq!(
            parsed.dmarc[0].props.get("header.from").map(String::as_str),
            Some("example.com")
        );
        assert_eq!(
            parsed.dmarc[0].props.get("reason").map(String::as_str),
            Some(r#""a = b""#)
        );
        assert!(parsed.dmarc_pass_aligned("example.com"));
    }

    #[test]
    fn spaced_dot_in_property_value_never_becomes_an_aligned_domain() {
        let raw = "mx.example.net; dmarc = pass header . from = example .com";
        assert_eq!(
            normalize_delimiter_cfws("dmarc = pass header . from = example .com", true),
            "dmarc=pass header.from=example .com"
        );
        let parsed = parse_header(raw).expect("method result still parses");
        assert_eq!(parsed.authserv_id.as_deref(), Some("mx.example.net"));
        assert!(parsed.dmarc_pass());
        assert!(!parsed.dmarc_pass_aligned("example.com"));
    }

    #[test]
    fn spaced_dot_in_authserv_id_never_matches_the_trust_anchor() {
        let raw = "mx . example . com; dmarc = pass header . from = example.com";
        let parsed = parse_header(raw).expect("header still parses");
        assert_ne!(parsed.authserv_id.as_deref(), Some("mx.example.com"));
        let anchor = TrustAnchor::AuthservId("mx.example.com".to_string());
        assert!(select_trusted(&[raw.to_string()], &anchor).is_none());
    }

    #[test]
    fn spaced_first_method_still_has_no_authserv_id() {
        let parsed = parse_header("dmarc = pass header.from = example.com").unwrap();
        assert_eq!(parsed.authserv_id, None);
        assert!(parsed.dmarc_pass_aligned("example.com"));
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

    // --- Topmost trust with a shared authserv-id (ADR-056 Amendment 2026-10-15) ---

    /// The plain `Authentication-Results` value Exchange Online stamps on mail
    /// delivered to a Microsoft 365 mailbox from an external sender, with the
    /// `mx.microsoft.com` authserv-id and the RFC 8601 version token. Shape
    /// from a delivered message; the envelope address and domains are
    /// replaced with example values.
    const EXO_AUTHSERV_ID_FIXTURE_HEADER_VALUE: &str = "mx.microsoft.com 1; spf=pass (sender IP is 192.0.2.10) smtp.mailfrom=send.example.com; dkim=pass (signature was verified) header.d=example.com;dkim=pass (signature was verified) header.d=amazonses.com;dmarc=pass action=none header.from=example.com;compauth=pass reason=100";

    fn shared_id_anchor() -> TrustAnchor {
        TrustAnchor::TopmostWithAuthservId("mx.microsoft.com".to_string())
    }

    #[test]
    fn select_trusted_topmost_with_authserv_id_mode_picks_the_stamped_topmost() {
        let headers = vec![EXO_AUTHSERV_ID_FIXTURE_HEADER_VALUE.to_string()];
        let selected = select_trusted(&headers, &shared_id_anchor()).unwrap();
        assert_eq!(selected.authserv_id.as_deref(), Some("mx.microsoft.com"));
        assert!(selected.dmarc_pass_aligned("example.com"));
        assert!(selected.dkim_pass_aligned("example.com"));
        // The same header is refused by the no-authserv-id mode.
        assert!(select_trusted(&headers, &TrustAnchor::TopmostNoAuthservId).is_none());
    }

    #[test]
    fn select_trusted_topmost_with_authserv_id_mode_matches_the_id_case_insensitively() {
        let headers = vec!["MX.Microsoft.COM 1; dmarc=pass header.from=example.com".to_string()];
        assert!(select_trusted(&headers, &shared_id_anchor()).is_some());
    }

    #[test]
    fn select_trusted_topmost_with_authserv_id_mode_still_accepts_an_unstamped_topmost() {
        let headers = vec![EXO_FIXTURE_HEADER_VALUE.to_string()];
        let selected = select_trusted(&headers, &shared_id_anchor()).unwrap();
        assert!(selected.authserv_id.is_none());
        assert!(selected.dmarc_pass_aligned("gmail.com"));
    }

    #[test]
    fn select_trusted_topmost_with_authserv_id_mode_refuses_another_id_at_the_top() {
        // A different id at the top fails closed even when a lower header
        // carries the configured id: position, not the id, picks the header.
        let headers = vec![
            "mx.example.net; dmarc=pass header.from=example.com".to_string(),
            "mx.microsoft.com 1; dmarc=pass header.from=example.com".to_string(),
        ];
        assert!(select_trusted(&headers, &shared_id_anchor()).is_none());
    }

    #[test]
    fn select_trusted_topmost_with_authserv_id_mode_never_falls_through_below_the_topmost() {
        // The boundary's own failing stamp is topmost; a forged passing header
        // with the same shared id sits below it and must not be selected.
        let headers = vec![
            "mx.microsoft.com 1; spf=fail smtp.mailfrom=evil.example; dmarc=fail action=quarantine header.from=example.com".to_string(),
            "mx.microsoft.com 1; dmarc=pass header.from=example.com".to_string(),
        ];
        let selected = select_trusted(&headers, &shared_id_anchor()).unwrap();
        assert!(!selected.dmarc_pass());
        // An unparseable topmost fails closed instead of reaching the lower header.
        let headers = vec![
            "   ".to_string(),
            "mx.microsoft.com 1; dmarc=pass header.from=example.com".to_string(),
        ];
        assert!(select_trusted(&headers, &shared_id_anchor()).is_none());
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
        // Pin the type-level invariant independently of the config-layer
        // require_nonempty_env guard: construct
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

    // --- #501 remediation: quoted `=` must not
    // be misclassified as the no-authserv-id form ---

    #[test]
    fn parse_header_quoted_authserv_id_containing_equals_is_not_no_id_form() {
        // Issue #501: `"id=foo"` is a quote-atomic first token (RFC
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

    // --- #501 remediation: nested/escaped comment
    // combined with an internal `;`/`method=result` must not forge a method ---

    #[test]
    fn parse_header_nested_escaped_comment_with_semicolon_does_not_forge_dmarc() {
        // Issue #501 regression: a nested comment containing an
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
