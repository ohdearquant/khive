use super::{extract_token, keep_leftmost};

// ─── Layer 1: known patterns ─────────────────────────────────────────────────

/// Each entry: (detector_name, needle, min_total_token_len).
///
/// The needle must appear as a word-boundary-adjacent prefix in the token.
/// `min_total_token_len` is the minimum length the token (needle + remainder)
/// must have — prevents the prefix alone triggering without a payload.
pub(super) const PREFIX_DETECTORS: &[(&str, &str, usize)] = &[
    // AWS
    ("aws-access-key-id", "AKIA", 20),
    ("aws-access-key-id", "ASIA", 20),
    // GitHub tokens: personal-access (ghp_), OAuth (gho_), GitHub App
    // user-to-server (ghu_), server-to-server (ghs_), refresh (ghr_), and the
    // fine-grained PAT (github_pat_). The gh*_ formats carry a 36-character
    // payload; fine-grained PATs carry an 82-character payload.
    ("github-token", "ghp_", 36),
    ("github-token", "gho_", 36),
    ("github-token", "ghu_", 36),
    ("github-token", "ghs_", 36),
    ("github-token", "ghr_", 36),
    ("github-token", "github_pat_", 93),
    // OpenAI project keys carry at least an 80-character payload.
    ("openai-api-key", "sk-proj-", 88),
    // NOTE: bare "sk-" also matches the more-specific prefixes below. Those
    // prefixes retain ownership of their candidates so the generic fallback
    // cannot bypass a vendor-specific minimum.
    // Anthropic
    ("anthropic-api-key", "sk-ant-", 108),
    // Stripe live keys
    ("stripe-secret-key", "sk_live_", 30),
    ("stripe-restricted-key", "rk_live_", 30),
    // Fly.io (fm2_ prefix only — FlyV1 handled separately because it embeds a space)
    ("fly-token", "fm2_", 20),
    // Vercel
    ("vercel-token", "vercel_", 20),
    // Slack
    ("slack-token", "xoxb-", 40),
    ("slack-token", "xoxa-", 40),
    ("slack-token", "xoxp-", 40),
    ("slack-token", "xoxr-", 40),
    ("slack-token", "xoxs-", 40),
    // Age secret key
    ("age-secret-key", "AGE-SECRET-KEY-", 60),
];

/// Known safe compound words that start with `sk-` but are not credentials.
/// E.g. scikit-learn slugs such as `sk-learn`, `sk-image`, `sk-lego`.
const SK_SAFE_PREFIXES: &[&str] = &["sk-learn", "sk-image", "sk-lego", "sk-base", "sk-misc"];

/// Shape-based patterns checked with custom logic.
///
/// Returns the LEFTMOST match across every detector (see [`keep_leftmost`]). The
/// detectors are still offered in priority order, so two detectors that match at
/// the SAME offset (e.g. bare `sk-` and the more-specific `sk-ant-`) resolve to
/// the first-offered one.
pub(super) fn check_known_patterns(text: &str) -> Option<(&str, &'static str)> {
    let base = text.as_ptr() as usize;
    let mut best: Option<(&str, &'static str)> = None;

    // --- Prefix patterns ---
    for &(name, needle, min_len) in PREFIX_DETECTORS {
        keep_leftmost(
            &mut best,
            find_prefix_token(text, needle, min_len).map(|m| (m, name)),
            base,
        );
    }

    // --- Bare `sk-` (after all more-specific sk- detectors above) ---
    // Require length ≥ 30 AND exclude known safe scikit/library compound words.
    if let Some(token) = find_bare_sk_token(text) {
        keep_leftmost(&mut best, Some((token, "openai-api-key")), base);
    }

    // --- Fly.io FlyV1 token: "FlyV1 <base64-payload>" ---
    // The format embeds a space, so the generic prefix extractor (which stops at
    // whitespace) cannot measure the combined length.  Check for `FlyV1 ` followed
    // by ≥ 4 non-whitespace characters as the payload.
    let mut from = 0;
    while let Some(rel) = text[from..].find("FlyV1 ") {
        let pos = from + rel;
        let at_boundary = pos == 0 || {
            text[..pos]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_ascii_alphanumeric())
        };
        if at_boundary {
            let payload_start = pos + 6; // skip "FlyV1 "
            let payload = extract_token(&text[payload_start..]);
            if payload.len() >= 4 {
                let candidate = &text[pos..payload_start + payload.len()];
                keep_leftmost(&mut best, Some((candidate, "fly-token")), base);
                break;
            }
        }
        from = pos + "FlyV1 ".len();
    }

    // --- PEM private key block ---
    // "-----BEGIN <TYPE> PRIVATE KEY-----" followed by a body.
    keep_leftmost(
        &mut best,
        find_pem_private_key_block(text).map(|m| (m, "pem-private-key")),
        base,
    );

    // --- JWT triple: eyJ...eyJ...eyJ (header.payload.signature) ---
    // A JWT starts with "eyJ" (base64url of `{"`) and has exactly two dots.
    keep_leftmost(&mut best, find_jwt(text).map(|m| (m, "jwt")), base);

    // --- URL userinfo: scheme://user:pass@host ---
    keep_leftmost(
        &mut best,
        find_url_userinfo(text).map(|m| (m, "url-userinfo")),
        base,
    );

    best
}

/// Locate the first token in `text` that starts with `needle` and has a
/// total length >= `min_len`.  Returns a slice of the full token on match.
pub(super) fn find_prefix_token<'a>(
    text: &'a str,
    needle: &str,
    min_len: usize,
) -> Option<&'a str> {
    let mut start = 0;
    while let Some(rel) = text[start..].find(needle) {
        let abs = start + rel;
        // Require that the needle starts at a token boundary (start-of-string
        // or preceded by a non-ASCII-alphanumeric char).  The needles are ASCII,
        // so only an ASCII alphanumeric can be a real continuation of the same
        // token; CJK/accented text (which Rust counts as `is_alphanumeric`) must
        // act as a delimiter, else a secret glued to non-Latin prose (`数据AKIA…`)
        // is missed.
        let at_boundary = abs == 0 || {
            let prev = text[..abs].chars().next_back().unwrap_or(' ');
            !prev.is_ascii_alphanumeric()
        };
        if at_boundary {
            let token = extract_token(&text[abs..]);
            if token.len() >= min_len && !is_filename_shaped_prefix_match(token, needle) {
                return Some(token);
            }
        }
        start = abs + needle.len().max(1);
    }
    None
}

/// Locate a generic `sk-` token without reclassifying a registered vendor
/// prefix that did not meet its own minimum length.
fn find_bare_sk_token(text: &str) -> Option<&str> {
    let base = text.as_ptr() as usize;
    let mut from = 0;
    while from < text.len() {
        let token = find_prefix_token(&text[from..], "sk-", 30)?;
        let belongs_to_specific_detector = PREFIX_DETECTORS
            .iter()
            .any(|&(_, needle, _)| needle.starts_with("sk-") && token.starts_with(needle));
        let is_safe_compound = SK_SAFE_PREFIXES.iter().any(|safe| token.starts_with(safe));
        if !belongs_to_specific_detector && !is_safe_compound {
            return Some(token);
        }

        let token_start = token.as_ptr() as usize - base;
        // Suppress only this `sk-` occurrence. The same whitespace-delimited
        // token may contain a later generic key glued after punctuation, and
        // advancing past the whole token would hide it from the fallback.
        from = token_start + "sk-".len();
    }
    None
}

/// Known source-file extensions that can terminate an ordinary provider-
/// prefixed filename. This is deliberately a closed set: an unknown suffix
/// is not evidence strong enough to suppress a context-free prefix detector.
const SOURCE_FILE_EXTENSIONS: &[&str] =
    &[".py", ".rs", ".ts", ".js", ".sh", ".md", ".toml", ".json"];

/// Returns `true` for a lowercase source filename after a known provider
/// prefix, such as `vercel_deployment_monitor.py`, optionally followed by a
/// source citation's line reference: `vercel_deployment_monitor.py:412` or
/// `vercel_deployment_monitor.py:412-418`.
///
/// A source extension alone is not enough. The payload before it must contain
/// only lowercase ASCII letters and filename/path punctuation; any uppercase
/// letter or digit is credential-value evidence and keeps the prefix match
/// fail-closed. Outer Markdown/prose punctuation is ignored, but never any
/// payload byte inside the filename itself.
///
/// The line reference is a POSITIONALLY DISTINCT suffix, never part of the
/// stem the checks below apply: it is parsed and stripped first, and only
/// then is the extension matched against what remains, so a digit inside the
/// stem itself (`some2module.py`) still fails the all-lowercase check exactly
/// as before.
///
/// A trailing colon with nothing after it stays where it has always been: in
/// the generic prose-punctuation trim just below, which removes it before any
/// of this runs. `<prefix>some_module.py:` in a sentence was admitted before
/// line references were understood here and is still admitted, because it is
/// the same filename it always was with sentence punctuation after it. Adding
/// a line-reference grammar is a widening of what this function accepts; it
/// narrows nothing.
pub(super) fn is_filename_shaped_prefix_match(token: &str, needle: &str) -> bool {
    let token = token.trim_end_matches(|c: char| {
        matches!(
            c,
            '`' | '"' | '\'' | ')' | ']' | '}' | '>' | ',' | ';' | ':' | '!' | '?'
        )
    });
    let token = token.strip_suffix('.').unwrap_or(token);
    let Some(payload) = token.strip_prefix(needle) else {
        return false;
    };
    let payload = strip_source_citation_line_reference(payload);
    let Some(stem) = SOURCE_FILE_EXTENSIONS
        .iter()
        .find_map(|extension| payload.strip_suffix(extension))
    else {
        return false;
    };

    stem.bytes().any(|byte| byte.is_ascii_lowercase())
        && stem
            .bytes()
            .any(|byte| matches!(byte, b'_' | b'-' | b'/' | b'.'))
        && stem
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || matches!(byte, b'_' | b'-' | b'/' | b'.'))
}

/// Strip a trailing source citation's line reference from `payload` and
/// return the remainder; returns `payload` unchanged when the tail is not
/// exactly one of these two shapes.
///
/// Grammar: `:<digits>` or `:<digits>-<digits>` — digits only, at least one
/// digit in each run, no leading `+`/`-` on either run, and a single `-`
/// separating the two runs in the range form. A second colon, a non-digit
/// byte, or more than one `-` all mean this is not a line reference, so the
/// caller's extension match declines exactly as it does for any other
/// unrecognized suffix.
///
/// This grammar is intentionally separate from the stem predicate the caller
/// applies after this returns: the reference is peeled off BEFORE the
/// extension is matched, so the digits admitted here never reach the stem,
/// and they do not loosen what that predicate accepts.
fn strip_source_citation_line_reference(payload: &str) -> &str {
    let Some(colon) = payload.rfind(':') else {
        return payload;
    };
    let (head, reference) = (&payload[..colon], &payload[colon + 1..]);

    let is_digit_run = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let is_line_reference = match reference.split_once('-') {
        None => is_digit_run(reference),
        Some((start, end)) => is_digit_run(start) && is_digit_run(end),
    };

    if is_line_reference {
        head
    } else {
        payload
    }
}

/// Shortest line of base64 that counts as PEM key material. Real key blocks
/// wrap at 64 columns; the last line of a block can be shorter, but a block
/// with no END marker is recognised by a full-width line, so a short tail on
/// its own is a mention rather than a key.
const PEM_BODY_LINE_MIN: usize = 40;

/// A line consisting only of base64 alphabet characters, long enough to be a
/// wrapped line of a key block.
fn is_pem_body_line(line: &str) -> bool {
    line.len() >= PEM_BODY_LINE_MIN
        && line
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
}

/// Byte offset of the first line break at or after `from`: a real newline,
/// or the two-character escape `\\n` that a newline becomes once the text
/// is serialized JSON (a note's properties, a stream payload). Returns the
/// break's start and the offset just past it, or `None` when the rest of
/// the text is one line.
fn next_line_break(text: &str, from: usize) -> Option<(usize, usize)> {
    let rest = &text[from..];
    let real = rest.find('\n').map(|i| (from + i, from + i + 1));
    let escaped = rest.find("\\n").map(|i| (from + i, from + i + 2));
    match (real, escaped) {
        (Some(r), Some(e)) => Some(if r.0 <= e.0 { r } else { e }),
        (r, e) => r.or(e),
    }
}

/// End of the line starting at `from` (exclusive of its break) and the start
/// of the following line.
fn line_bounds(text: &str, from: usize) -> (usize, usize) {
    match next_line_break(text, from) {
        Some((end, next)) => (end, next),
        None => (text.len(), text.len()),
    }
}

/// Find the leftmost PEM private key block: a `-----BEGIN <TYPE> PRIVATE
/// KEY-----` header line followed by a body. The body is either a matching
/// `-----END ... PRIVATE KEY-----` marker before the next BEGIN, or at
/// least one line of base64 of key-block width directly under it. A header
/// with neither is a mention of the format (a documentation page, code that
/// prints the label) and carries no key, so it is not a candidate. Lines
/// break on a newline or on its JSON escape, so a key inside a serialized
/// document is read the same way as one in plain text. The returned slice
/// is bounded to the block: through the END line when one is present,
/// otherwise through the last base64 line under the header.
fn find_pem_private_key_block(text: &str) -> Option<&str> {
    let mut search = 0;
    while let Some(rel) = text[search..].find("-----BEGIN") {
        let pos = search + rel;
        let (header_end, body_start) = line_bounds(text, pos);
        let header = &text[pos..header_end];
        // Resume after this header on the next pass whatever it turns out to be.
        search = header_end;
        if !header.contains("PRIVATE KEY-----") {
            continue;
        }
        // The END marker must belong to this header: stop looking at the
        // next BEGIN so a mention above a real block does not claim it.
        let next_begin = text[body_start..]
            .find("-----BEGIN")
            .map(|r| body_start + r)
            .unwrap_or(text.len());
        if let Some(end_rel) = text[body_start..next_begin].find("-----END") {
            let end_pos = body_start + end_rel;
            let (end_line, end_next) = line_bounds(text, end_pos);
            if text[end_pos..end_line].contains("PRIVATE KEY-----") {
                return Some(&text[pos..end_next]);
            }
        }
        let mut body_end = None;
        let mut cursor = body_start;
        while cursor < text.len() {
            let (line_end, next) = line_bounds(text, cursor);
            let line = text[cursor..line_end]
                .trim_end_matches('\r')
                .trim_end_matches("\\r");
            if is_pem_body_line(line) {
                body_end = Some(next);
                cursor = next;
                continue;
            }
            // The last line of a wrapped block is usually shorter than the
            // others. Once at least one full-width line has been seen, a
            // trailing base64-only run of any length belongs to the block,
            // so a masked surface never keeps the tail of the key. Inside a
            // serialized JSON string that run ends at the closing quote
            // instead of a line break; the block ends where the run ends.
            let run = line
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/' || *b == b'=')
                .count();
            let whole_line = run == line.len();
            let json_end = line[run..].starts_with('"');
            if run > 0 && (whole_line || json_end) {
                if body_end.is_some() {
                    body_end = Some(if whole_line { next } else { cursor + run });
                } else if run >= PEM_BODY_LINE_MIN && json_end {
                    body_end = Some(cursor + run);
                }
            }
            break;
        }
        if let Some(end) = body_end {
            return Some(&text[pos..end]);
        }
    }
    None
}

/// Scan for a JWT pattern: at least two "eyJ" segments separated by a `.`
/// character, with each segment at least 10 chars.
fn find_jwt(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 4 < bytes.len() {
        if bytes[i..].starts_with(b"eyJ") {
            // Find the end of this JWT (whitespace or string end).
            let end = bytes[i..]
                .iter()
                .position(|&b| b == b' ' || b == b'\n' || b == b'\r' || b == b'\t')
                .map(|p| i + p)
                .unwrap_or(bytes.len());
            let candidate = &text[i..end];
            // Must have at least 2 dots and 3 eyJ-prefixed segments.
            let dots = candidate.as_bytes().iter().filter(|&&b| b == b'.').count();
            if dots >= 2 {
                let parts: Vec<&str> = candidate.splitn(3, '.').collect();
                if parts.len() == 3
                    && parts[0].starts_with("eyJ")
                    && parts[1].starts_with("eyJ")
                    && parts[0].len() >= 10
                    && parts[1].len() >= 10
                {
                    return Some(candidate);
                }
            }
            i = end + 1;
        } else {
            i += 1;
        }
    }
    None
}

/// Detect `scheme://user:pass@host` patterns where the userinfo carries an
/// actual credential: a non-empty password. The username may be empty —
/// `redis://:secret@host` is a standard empty-user connection string and its
/// password is no less a credential for the missing username.
pub(super) fn find_url_userinfo(text: &str) -> Option<&str> {
    let mut search = text;
    let mut base = 0usize;
    while let Some(at_rel) = search.find("://") {
        let at_abs = base + at_rel;
        // After `://`, only the authority component may carry userinfo: it
        // ends at the first `/`, `?`, `#`, space, or newline. An `@` past
        // that boundary is path/query text (`https://host/a:x@next`), not a
        // credential.
        let rest_start = at_abs + 3;
        let rest = &text[rest_start..];
        let authority_end = rest
            .find(['/', '?', '#', ' ', '\n', '\r'])
            .unwrap_or(rest.len());
        if let Some(at_pos) = rest[..authority_end].rfind('@') {
            let userinfo = &rest[..at_pos];
            // Must contain a colon with a non-empty password after it.
            if let Some(colon) = userinfo.find(':') {
                let pass = &userinfo[colon + 1..];
                if !pass.is_empty() {
                    // Return a slice starting from the scheme.  Walk back from
                    // `at_abs` to the first non-scheme char and resume just past
                    // it.  Use `char_indices` and skip by the separator's full
                    // UTF-8 width: a multibyte separator (e.g. CJK prose before a
                    // credential URL) would otherwise leave `scheme_start` inside
                    // the codepoint and panic the slice below.
                    let scheme_start = text[..at_abs]
                        .char_indices()
                        .rev()
                        .find(|(_, c)| {
                            !c.is_ascii_alphanumeric() && *c != '+' && *c != '-' && *c != '.'
                        })
                        .map(|(idx, c)| idx + c.len_utf8())
                        .unwrap_or(0);
                    // Ensure there are no spaces in userinfo (not a code snippet).
                    if !userinfo.contains(' ') && !userinfo.contains('\n') {
                        let end = rest_start
                            + at_pos
                            + 1
                            + rest[at_pos + 1..]
                                .find([' ', '\n', '\r'])
                                .unwrap_or(rest[at_pos + 1..].len());
                        return Some(&text[scheme_start..end.min(text.len())]);
                    }
                }
            }
        }
        base = at_abs + 3;
        search = &text[base..];
    }
    None
}
