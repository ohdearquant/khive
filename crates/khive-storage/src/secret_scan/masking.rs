use super::*;

/// Upper bound on how many characters of a diagnostic-boundary input the
/// canonical masker is ever asked to scan, regardless of a caller's own
/// output cap. Chosen comfortably larger than the largest output cap among
/// the callers of [`mask_bounded`] (1,024, `khive-mcp`'s
/// `MAX_BACKEND_ERROR_MESSAGE_CHARS`) so a credential's terminating span
/// remains inside the window for any message that is itself smaller than the
/// window. Masking cost scales with this constant, never with a caller's raw
/// input length. Shared by every diagnostic-boundary redaction site so the
/// window cannot drift between them independently.
pub const MASK_WINDOW_CHARS: usize = 4_096;

/// Result of [`mask_bounded`]: masked, window- and output-bounded text plus
/// the flags a caller needs to finish assembling a caller-visible
/// diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedMask {
    /// Masked text, never longer than `output_cap_chars` plus one trailing
    /// truncation-marker character. May be the bare truncation marker alone
    /// when the retained window held a single token longer than the window.
    pub text: String,
    /// True when `text` is not the complete masked input: the raw input
    /// exceeded `window_chars`, the masked window exceeded `output_cap_chars`,
    /// or the window held one token longer than the window that had to be
    /// dropped whole.
    pub truncated: bool,
    /// True when the canonical masker replaced a span inside the retained
    /// window, or the window's only content was dropped whole for being an
    /// oversized single token. A caller that decides whether raw content is
    /// safe to echo back verbatim on this flag must also treat `truncated`
    /// as reason enough on its own — content the window never retained is
    /// exactly as unverified as content the masker redacted.
    pub redacted: bool,
}

/// Bound a diagnostic-boundary masking call to at most `window_chars` of
/// `text` before the canonical masker ever runs, then cap the masked result
/// to `output_cap_chars`. Bounding happens BEFORE masking — not after, as a
/// truncate-then-mask policy would — so scan cost is bounded by
/// `window_chars` regardless of the caller's input length.
///
/// A window cut mid-token would let a masker that never saw the token's
/// terminating shape (e.g. the `@` closing `scheme://user:pass@host`) emit
/// the token's visible prefix unmasked — the exact split-secret hole a prior
/// mask-the-full-input-first policy existed to close, at the cost of
/// unbounded scan work. This function closes the hole without re-widening
/// the window to the input's full length: any token straddling the window
/// boundary is dropped in its entirety (back to the last whitespace inside
/// the window) rather than masked, because a masker can only vouch for a
/// token it saw whole. A single token that is itself longer than the window
/// has no earlier whitespace to fall back to inside the window and is
/// replaced by the truncation marker alone.
///
/// The same drop applies to a chain of bridged fragments straddling the
/// boundary, not just the one partial token touching it: see
/// `trailing_bridge_fragment_cut` (private to this module) for why a
/// forward lookahead cannot bound this instead, and for the backward walk
/// that closes it purely from data already inside the window.
///
/// `window_chars` must be at least `output_cap_chars`; debug builds assert
/// this so a misconfigured call site fails loudly instead of silently
/// capping tighter than it windows, and the cap is additionally clamped to
/// `window_chars` in every build so a misconfigured call site cannot cap
/// tighter than it windows even outside debug assertions.
pub fn mask_bounded(
    surface: RedactionSurface,
    text: &str,
    window_chars: usize,
    output_cap_chars: usize,
) -> BoundedMask {
    debug_assert!(
        window_chars >= output_cap_chars,
        "the input window must stay at least as large as the output cap"
    );
    let output_cap_chars = output_cap_chars.min(window_chars);

    let input_truncated = text.chars().nth(window_chars).is_some();
    let mut window: String = text.chars().take(window_chars).collect();

    if input_truncated {
        match window
            .char_indices()
            .rev()
            .find(|(_, ch)| ch.is_whitespace())
        {
            // Keep the whitespace itself; drop only the partial token after it.
            Some((idx, ch)) => window.truncate(idx + ch.len_utf8()),
            // A single token spans the whole window: nothing can be shown
            // whole, so show nothing at all.
            None => window.clear(),
        }
    }

    if input_truncated && !window.is_empty() {
        if let Some(cut) = trailing_bridge_fragment_cut(&window) {
            window.truncate(cut);
        }
    }

    if input_truncated && window.is_empty() {
        return BoundedMask {
            text: TRUNCATION_MARKER.to_string(),
            truncated: true,
            redacted: true,
        };
    }

    debug_assert!(window.chars().count() <= window_chars);
    let masked = mask_for_redaction_surface(surface, &window);
    let redacted = masked.as_ref() != window.as_str();

    let mut chars = masked.chars();
    let mut bounded: String = chars.by_ref().take(output_cap_chars).collect();
    let output_capped = chars.next().is_some();
    if output_capped || input_truncated {
        bounded.push_str(TRUNCATION_MARKER);
    }

    BoundedMask {
        text: bounded,
        truncated: input_truncated || output_capped,
        redacted,
    }
}

pub(super) const TRUNCATION_MARKER: &str = "…";

/// Redact every detected secret span in `text`, replacing each with
/// `***MASKED***`.
///
/// This is the masking counterpart to [`check`]: where `check` hard-blocks a
/// write on the first match, `mask_secrets` is for content that must be emitted
/// or stored with credentials stripped. Named Git/session/MCP callers enter it
/// through [`mask_for_redaction_surface`]. It reuses the SAME canonical detector
/// set as `check`/`scan`, so callers must never maintain a second, weaker masker.
///
/// Returns `Cow::Borrowed` when no secret is present (the common case), avoiding
/// an allocation. Spans are discovered left to right against the ORIGINAL text,
/// always evaluating trigger context over the full input — a high-entropy value
/// whose only trigger word sits to the left of an earlier-redacted secret is
/// still detected. Cumulative suffix-scan work is capped; when dense input
/// exhausts the cap after a match, that match is extended through the remaining
/// text so unscanned credentials cannot survive. See
/// `docs/api/secret_gate.md#mask_secrets` for the scan-cursor mechanics.
pub fn mask_secrets(text: &str) -> std::borrow::Cow<'_, str> {
    let (spans, _scan_work_bytes) = collect_mask_spans(text);
    if spans.is_empty() {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end) in spans {
        // Spans are non-overlapping and ascending (each starts at/after the prior
        // `end`); `max(cursor)` is a defensive guard, never load-bearing.
        let start = start.max(cursor);
        out.push_str(&text[cursor..start]);
        out.push_str(REDACTION_MARKER);
        cursor = end.max(cursor);
    }
    out.push_str(&text[cursor..]);
    std::borrow::Cow::Owned(out)
}

/// Collect absolute byte spans to redact and report cumulative scan bytes revisited.
/// Exhausting the work budget extends the last confirmed secret span through the input tail.
pub(super) fn collect_mask_spans(text: &str) -> (Vec<(usize, usize)>, usize) {
    let base = text.as_ptr() as usize;
    let context = EntropyScanContext::new(text);
    let tokens = &context.tokens;
    // Collect every secret span (absolute byte offsets into `text`) before
    // writing any output, so trigger-context detection always sees the original
    // string rather than the suffix after the previous redaction.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut from = 0;
    let mut scan_work_bytes = 0usize;
    while from < text.len() {
        // Candidate enumeration may revisit the prefix of the original token
        // containing `from`. Charge it as well as the remaining suffix; in a
        // whitespace gap the scan still begins at `from`.
        let token_index = tokens.partition_point(|&(offset, raw)| offset + raw.len() <= from);
        let scan_start = tokens
            .get(token_index)
            .map_or(from, |&(offset, _)| offset.min(from));
        let scan_len = text.len() - scan_start;
        let next_scan_work = scan_work_bytes.saturating_add(scan_len);
        if scan_work_bytes > 0 && next_scan_work > MAX_MASK_SCAN_WORK_BYTES {
            // Every previous sweep ended at a confirmed match; extending that
            // redaction through the remaining tail is fail-closed.
            spans.last_mut().expect("a prior scan found a span").1 = text.len();
            break;
        }
        scan_work_bytes = next_scan_work;
        match scan_from(text, from, &context) {
            Some((sub, _detector)) => {
                let start = sub.as_ptr() as usize - base;
                // The prefix detectors return whitespace-delimited tokens, so a
                // credential glued to structural punctuation (JSON quotes/braces,
                // sentence commas) carries that trailing punctuation into the
                // match. Trim a conservative trailing set that can never be part
                // of a credential, so redacting does not consume surrounding JSON
                // or prose structure. `=` `/` `+` `.` `-` `_` are intentionally
                // NOT trimmed — they are valid base64/JWT/key characters.
                let core_len = sub
                    .trim_end_matches(['"', '\'', '`', '}', ']', ')', ',', ';'])
                    .len();
                let end = extend_across_invisible_bridge(text, start + core_len.max(1));
                push_mask_spans(text, start, end, &mut spans);
                // `scan_from` only returns matches with start >= from, and `end`
                // is strictly greater than `start`, so `from` strictly advances.
                from = end;
            }
            None => break,
        }
    }
    (spans, scan_work_bytes)
}

/// A character that splits a payload without showing anything: non-ASCII and not
/// a letter or digit, so U+200B and its neighbours qualify while the letters of a
/// non-ASCII password do not. The second half of that predicate is load-bearing:
/// `redis://:密码@host` is ONE credential whose characters are non-ASCII, and a
/// rule keyed on non-ASCII alone splits it and prints the password between two
/// redaction markers.
fn is_invisible_bridge_separator(c: char) -> bool {
    !c.is_ascii() && !c.is_alphanumeric()
}

/// Byte offset a redaction must reach when the payload continues past `end`
/// behind an INVISIBLE separator.
///
/// A gap made only of [`is_invisible_bridge_separator`] characters is not something
/// a person types between a credential and the next word; it is how one payload is
/// split so each half falls under a detector's length floor. Detection already
/// reconstructs those chains ([`bridge_fragment_chain`]), but the masker redacted
/// only the token the scan returned, so the rest of the same payload survived into
/// stored text. Gaps holding any ASCII character — the ordinary spaces and newlines
/// between a commit sha and the prose after it — are never walked, so this cannot
/// eat surrounding text.
pub(super) fn extend_across_invisible_bridge(text: &str, end: usize) -> usize {
    let mut end = end;
    for _ in 1..MAX_BRIDGE_FRAGMENTS {
        let rest = &text[end..];
        let Some(gap_len) = rest.find(|c: char| c.is_ascii_alphanumeric()) else {
            break;
        };
        let gap = &rest[..gap_len];
        if gap.is_empty() || !gap.chars().all(is_invisible_bridge_separator) {
            break;
        }
        let fragment = &rest[gap_len..];
        let fragment_len = fragment
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(fragment.len());
        if fragment_len < MIN_BRIDGE_FRAGMENT_LEN {
            break;
        }
        end += gap_len + fragment_len;
    }
    end
}

/// Push the redaction spans for `text[start..end]`, breaking at
/// [`is_invisible_bridge_separator`] characters so a separator that joined two
/// fragments of one payload stays visible instead of being swallowed into a single
/// marker.
///
/// A span holding no such character — every ordinary credential, base64 and JWT
/// forms included, whose `.` `+` `/` `=` are ASCII, and non-ASCII passwords, whose
/// letters are alphanumeric — is pushed whole, so this changes nothing for them. A
/// span that yields no run at all is pushed whole as well: redacting more than
/// necessary is the safe direction.
fn push_mask_spans(text: &str, start: usize, end: usize, spans: &mut Vec<(usize, usize)>) {
    let span = &text[start..end];
    if !span.chars().any(is_invisible_bridge_separator) {
        spans.push((start, end));
        return;
    }
    let before = spans.len();
    let mut run_start: Option<usize> = None;
    for (offset, ch) in span.char_indices() {
        if is_invisible_bridge_separator(ch) {
            if let Some(run) = run_start.take() {
                spans.push((start + run, start + offset));
            }
        } else {
            run_start.get_or_insert(offset);
        }
    }
    if let Some(run) = run_start {
        spans.push((start + run, end));
    }
    if spans.len() == before {
        spans.push((start, end));
    }
}

/// Maximum characters of raw error text admitted to the masking pass.
///
/// This is NOT a tight bound like [`MAX_LOG_TEXT_OUTPUT_CHARS`] below — it exists only to
/// stop a truly pathological input (gigabytes of attacker-controlled text funneled into one
/// error/log line) from making the masking scan unbounded. It is a pure compute bound, not a
/// safety bound: [`find_url_userinfo`] has no length limit on the password it recognizes, so
/// no finite value of this constant can guarantee a credential never straddles it — a
/// password longer than whatever this is set to always has a crossing case. The actual
/// safety invariant lives in [`redact_crossing_boundary_url_userinfo`], the fallback
/// [`bounded_masked_log_text`] runs after [`mask_secrets`]: it redacts any `scheme://user:`
/// opening whose password run reaches this cut point without a terminating `@`, regardless
/// of how long that password is. 1 MiB just keeps the scan itself cheap.
pub(super) const MAX_LOG_TEXT_MASK_INPUT_CHARS: usize = 1_048_576;
/// Maximum characters of masked error text emitted to a log record.
pub(super) const MAX_LOG_TEXT_OUTPUT_CHARS: usize = 1_024;

/// Bound and mask arbitrary error text for log emission.
///
/// Log records are a disclosure surface the same way wire errors are: they are
/// shipped, aggregated, and read by consumers outside the process. Backend
/// error text (gate backends included) can embed connection strings or
/// credentials, so the FULL text (up to `MAX_LOG_TEXT_MASK_INPUT_CHARS`, a
/// pure compute bound — see its doc comment) is masked with the canonical
/// detector set before any truncation happens. Masking after truncation would
/// let a secret whose tail sits past the bound lose the context (e.g. a URL's
/// terminating `@`) a detector needs to recognize it, leaving its head
/// unmasked in the log — truncate-then-mask must never replace
/// mask-then-truncate here. Because `MAX_LOG_TEXT_MASK_INPUT_CHARS` is
/// finite and `find_url_userinfo` has no bound on password length, a
/// password long enough still crosses the cut before its terminating `@`
/// ever appears; `redact_crossing_boundary_url_userinfo` closes that gap
/// by redacting the unterminated opening directly, so no credential prefix
/// survives regardless of secret length. Control (`Cc`), format (`Cf`), line
/// separator (`Zl`), and paragraph separator (`Zp`) Unicode codepoints in the
/// masked text are then escaped: a log line is plain text read by tooling
/// outside this process, and an embedded line break or bidi/format override
/// could forge or visually disguise part of the record. The result is bounded
/// again for the emitted record. A truncation in either the masking pass or the
/// output pass appends `…` so the record declares its own incompleteness.
///
/// This function, not [`mask_for_redaction_surface`], is the direct
/// `mask_secrets` caller for general log-text bounding: it is not one of the
/// three named redact-not-block surfaces (git ingest, session mirror, MCP
/// diagnostics) that surface owns, and its own mask-then-truncate contract —
/// with the additional crossing-boundary fallback above — is a strict
/// superset of what the surface wrapper provides. It lives in this module
/// specifically so it can stay a direct caller; the call-site census in
/// `crates/khive-runtime/tests/adr115_redaction_call_site_census.rs` only
/// requires callers *outside* this file to route through the wrapper.
pub fn bounded_masked_log_text(text: &str) -> String {
    let mask_input_truncated = text.chars().nth(MAX_LOG_TEXT_MASK_INPUT_CHARS).is_some();
    let bounded_input: std::borrow::Cow<'_, str> = if mask_input_truncated {
        std::borrow::Cow::Owned(text.chars().take(MAX_LOG_TEXT_MASK_INPUT_CHARS).collect())
    } else {
        std::borrow::Cow::Borrowed(text)
    };
    let masked = mask_secrets(&bounded_input);
    let masked = if mask_input_truncated {
        redact_crossing_boundary_url_userinfo(&masked)
    } else {
        masked
    };
    let neutralized = neutralize_log_unsafe_chars(&masked);

    let mut chars = neutralized.chars();
    let mut bounded: String = chars.by_ref().take(MAX_LOG_TEXT_OUTPUT_CHARS).collect();
    if chars.next().is_some() || mask_input_truncated {
        bounded.push('…');
    }
    bounded
}

/// Fallback for a `scheme://user:<password>` credential whose password run
/// collided with [`bounded_masked_log_text`]'s truncation of the mask-scan
/// input at [`MAX_LOG_TEXT_MASK_INPUT_CHARS`]. [`find_url_userinfo`] only
/// recognizes a credential once it sees the terminating `@`; when that `@`
/// sits past the truncation point, [`mask_secrets`] never sees the shape at
/// all and the raw `scheme://user:<password prefix>` reaches the log. No
/// finite value of [`MAX_LOG_TEXT_MASK_INPUT_CHARS`] can rule this out — the
/// detector is unbounded, so any cap has a crossing case — so the invariant
/// has to come from this fallback, not from the cap's size.
///
/// Only called when `bounded_masked_log_text` actually truncated the input.
/// It scans `://` occurrences left to right and redacts at the EARLIEST
/// unterminated `user:<password-run>` opening: a colon splits the tail into
/// two non-empty pieces and no `@`, space, or newline appears anywhere from
/// that occurrence to the end of the truncated text. An occurrence whose
/// tail does contain one of those terminators is a complete URL or ordinary
/// prose that ends inside the text (e.g. two URLs logged side by side) and
/// is skipped, not redacted. The anchor must be the earliest such opening,
/// never the last: a later `://` can sit INSIDE the crossing password
/// itself (passwords may contain `://`), and anchoring there would leave
/// the real `user:<password prefix>` before it in the emitted log. From the
/// earliest unterminated opening's colon onward everything is redacted —
/// zero password characters survive, no matter how long the password
/// actually is.
fn redact_crossing_boundary_url_userinfo(text: &str) -> std::borrow::Cow<'_, str> {
    let mut search_from = 0usize;
    while let Some(rel) = text[search_from..].find("://") {
        let scheme_pos = search_from + rel;
        let rest = &text[scheme_pos + 3..];
        let terminated =
            rest.contains('@') || rest.contains(' ') || rest.contains('\n') || rest.contains('\r');
        if !terminated {
            // Same rules as `find_url_userinfo`: the userinfo colon must sit
            // in the authority component (before any `/`, `?`, or `#` — a
            // later colon is path/query text), and only the password must be
            // non-empty (an empty username, `redis://:pass`, is a standard
            // connection-string form and no less a credential). The password
            // run AFTER the colon is unrestricted — a crossing password may
            // itself contain any of those delimiters.
            let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            if let Some(colon) = rest[..authority_end].find(':') {
                let pass = &rest[colon + 1..];
                if !pass.is_empty() {
                    let redact_from = scheme_pos + 3 + colon;
                    let mut out = String::with_capacity(redact_from + REDACTION_MARKER.len());
                    out.push_str(&text[..redact_from]);
                    out.push_str(REDACTION_MARKER);
                    return std::borrow::Cow::Owned(out);
                }
            }
        }
        search_from = scheme_pos + 3;
    }
    std::borrow::Cow::Borrowed(text)
}

/// `true` for a Unicode control (`Cc`), format (`Cf`), line separator (`Zl`), or
/// paragraph separator (`Zp`) codepoint, tab excepted.
///
/// Classification is by Unicode general category rather than an ASCII byte range so that
/// multi-byte control/format characters (bidi overrides, zero-width joiners, line/paragraph
/// separators encoded as UTF-8) are caught the same way as single-byte C0 controls like
/// CR/LF — a byte-range check would only ever see the latter. Tab is excepted: it is
/// visually inert in a log line and common in legitimately reformatted prose.
fn is_log_unsafe_char(c: char) -> bool {
    if c == '\t' {
        return false;
    }
    matches!(
        unicode_general_category::get_general_category(c),
        unicode_general_category::GeneralCategory::Control
            | unicode_general_category::GeneralCategory::Format
            | unicode_general_category::GeneralCategory::LineSeparator
            | unicode_general_category::GeneralCategory::ParagraphSeparator
    )
}

/// Escape every [`is_log_unsafe_char`] codepoint in `text` as `\u{XXXX}`.
///
/// Returns `Cow::Borrowed` when nothing needs escaping (the common case), avoiding an
/// allocation. This runs on already-masked text: it must never be skipped for text that
/// bypassed [`mask_secrets`], since a control character can sit inside a would-be secret
/// span and is a distinct disclosure vector from the credential detectors (log injection /
/// forgery, not credential leakage).
fn neutralize_log_unsafe_chars(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.chars().any(is_log_unsafe_char) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if is_log_unsafe_char(c) {
            out.push_str(&format!("\\u{{{:04x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    std::borrow::Cow::Owned(out)
}
