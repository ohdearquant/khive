//! Write-time secret detection gate (issue #76).
//!
//! Scans caller-supplied content strings before any storage write.  A match
//! causes a hard `RuntimeError::SecretDetected` that names the detector and
//! carries a masked excerpt — it never echoes the full candidate back.
//!
//! Scope: **credentials only** — API keys, tokens, private keys, passwords,
//! and connection strings with embedded credentials.  General PII such as
//! email addresses, phone numbers, and company names is intentionally NOT
//! blocked; those are normal knowledge-graph content.
//!
//! Detection is layered, cheap-first:
//!
//! 1. **Known-prefix / known-shape patterns** — AWS AKIA/ASIA, GitHub tokens,
//!    OpenAI `sk-proj-`, Anthropic `sk-ant-`, Stripe live keys, Fly.io tokens,
//!    Vercel secrets, Slack `xox*`, JWT triples, PEM private-key headers,
//!    Age secret keys, URL userinfo (`scheme://user:pass@`).
//!    Bare `sk-` is also checked but only when NOT followed by a known safe
//!    word boundary (e.g. `sk-learn`, `sk-image`).
//! 2. **High-entropy token heuristic** — base64/hex/base64url runs ≥ 24 chars
//!    near a trigger word (key, secret, password, credential, bearer, auth,
//!    apikey, api_key, access_key, private_key).  The word `token` alone is NOT
//!    a trigger to avoid blocking `tokenizer_*`, `token_count`, etc.
//!
//! Allowlist (false-positive suppression):
//! - Pure hex strings (sha256, git SHA) — passed unconditionally.
//! - UUID canonical form (`xxxxxxxx-xxxx-…`) — passed.
//! - Base64/base64url content hashes with an explicit `sha<N>-` prefix (SRI
//!   hashes, npm lockfile integrity) — passed when not preceded by a known-vendor
//!   prefix.  Bare base64 tokens without the `sha<N>-` prefix are NOT passed.
//! - Strings that are entirely ASCII punctuation/whitespace (e.g. code) — not
//!   subject to the entropy heuristic, only the literal-prefix checks apply.
//! - Non-ASCII characters (CJK prose, accented text, emoji) act as token
//!   delimiters for the entropy heuristic: only maximal ASCII runs are
//!   entropy-checked.  Real base64/hex/base64url credentials are ASCII, and
//!   `shannon_entropy` runs over UTF-8 bytes — multibyte codepoints inflate the
//!   byte-wise entropy and false-positive on natural-language non-Latin content.
//!   Treating non-ASCII as a delimiter (rather than skipping any whitespace
//!   token that merely contains it) keeps CJK prose unflagged while still
//!   catching an ASCII credential glued to CJK text/punctuation/fullwidth
//!   whitespace.  The literal-prefix checks (Layer 1) run independently and
//!   catch a prefixed secret regardless of adjacent non-ASCII characters.

use crate::error::{RuntimeError, RuntimeResult};

// ─── Public API ──────────────────────────────────────────────────────────────

/// Returned when a write would store credential-looking content.
///
/// Carries the detector name and a masked excerpt (`first6...Nchars`).  The
/// full candidate is never stored in the error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretMatch {
    /// Human-readable name of the detector that fired.
    pub detector: &'static str,
    /// `first6...N` — the first 6 chars of the match followed by the total length.
    pub masked: String,
}

impl std::fmt::Display for SecretMatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "content matches secret pattern {} at masked excerpt {}",
            self.detector, self.masked
        )
    }
}

/// Hard-block content from being written.
///
/// Returns `Err(RuntimeError::SecretDetected)` on the first match found, or
/// `Ok(())` if no secret pattern fires.
pub fn check(content: &str) -> RuntimeResult<()> {
    if let Some(m) = scan(content) {
        return Err(RuntimeError::SecretDetected(m));
    }
    Ok(())
}

/// Recursively scan a JSON value for credential-shaped strings.
///
/// Walks every string leaf (object values, array elements, nested objects).
/// Returns `Err(RuntimeError::SecretDetected)` on the first match found.
/// `None` / null / numeric / boolean JSON values are skipped.
pub fn check_json(value: &serde_json::Value) -> RuntimeResult<()> {
    scan_json_value(value)
}

/// Scan a string-tagged slice (entity/note tags).
///
/// Each tag string is scanned individually.
pub fn check_tags(tags: &[String]) -> RuntimeResult<()> {
    for tag in tags {
        check(tag)?;
    }
    Ok(())
}

fn scan_json_value(value: &serde_json::Value) -> RuntimeResult<()> {
    match value {
        serde_json::Value::String(s) => check(s),
        serde_json::Value::Array(arr) => {
            for v in arr {
                scan_json_value(v)?;
            }
            Ok(())
        }
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                // Scan both the key (a credential can appear as a JSON key name)
                // and the value recursively.
                check(k)?;
                scan_json_value(v)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

// ─── Scanner ─────────────────────────────────────────────────────────────────

/// Return the first `SecretMatch` found in `text`, or `None`.
fn scan(text: &str) -> Option<SecretMatch> {
    // Layer 1: known prefix / shape patterns (no allocation per check).
    if let Some(m) = check_known_patterns(text) {
        return Some(m);
    }
    // Layer 2: entropy heuristic on long tokens near trigger words.
    if let Some(m) = check_entropy_heuristic(text) {
        return Some(m);
    }
    None
}

// ─── Layer 1: known patterns ─────────────────────────────────────────────────

/// Each entry: (detector_name, needle, min_total_token_len).
///
/// The needle must appear as a word-boundary-adjacent prefix in the token.
/// `min_total_token_len` is the minimum length the token (needle + remainder)
/// must have — prevents the prefix alone triggering without a payload.
const PREFIX_DETECTORS: &[(&str, &str, usize)] = &[
    // AWS
    ("aws-access-key-id", "AKIA", 20),
    ("aws-access-key-id", "ASIA", 20),
    // GitHub personal-access tokens
    ("github-token", "ghp_", 36),
    ("github-token", "gho_", 36),
    ("github-token", "github_pat_", 20),
    // OpenAI
    ("openai-api-key", "sk-proj-", 40),
    // NOTE: bare "sk-" also matches Anthropic/Stripe below; put it last so
    // the more-specific detectors fire first when both would match.
    // Anthropic
    ("anthropic-api-key", "sk-ant-", 20),
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
fn check_known_patterns(text: &str) -> Option<SecretMatch> {
    // --- Prefix patterns ---
    for &(name, needle, min_len) in PREFIX_DETECTORS {
        if let Some(m) = find_prefix_token(text, needle, min_len) {
            return Some(build_match(name, m));
        }
    }

    // --- Bare `sk-` (after all more-specific sk- detectors above) ---
    // Require length ≥ 30 AND exclude known safe scikit/library compound words.
    if let Some(token) = find_prefix_token(text, "sk-", 30) {
        if !SK_SAFE_PREFIXES.iter().any(|safe| token.starts_with(safe)) {
            return Some(build_match("openai-api-key", token));
        }
    }

    // --- Fly.io FlyV1 token: "FlyV1 <base64-payload>" ---
    // The format embeds a space, so the generic prefix extractor (which stops at
    // whitespace) cannot measure the combined length.  Check for `FlyV1 ` followed
    // by ≥ 4 non-whitespace characters as the payload.
    if let Some(pos) = text.find("FlyV1 ") {
        let at_boundary = pos == 0 || {
            text[..pos]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric())
        };
        if at_boundary {
            let payload_start = pos + 6; // skip "FlyV1 "
            let payload = extract_token(&text[payload_start..]);
            if payload.len() >= 4 {
                let candidate = &text[pos..payload_start + payload.len()];
                return Some(build_match("fly-token", candidate));
            }
        }
    }

    // --- PEM private key block ---
    // "-----BEGIN <TYPE> PRIVATE KEY-----"
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----") {
        if let Some(pos) = text.find("-----BEGIN") {
            // Measure only the key block itself (up to END marker or end-of-string),
            // not the rest of the surrounding text, so build_match reports the
            // block length rather than the remaining string length.
            let block_end = text[pos..]
                .find("-----END")
                .map(|rel| {
                    text[pos + rel..]
                        .find('\n')
                        .map(|l| pos + rel + l + 1)
                        .unwrap_or(text.len())
                })
                .unwrap_or(text.len());
            let excerpt = &text[pos..block_end];
            return Some(build_match("pem-private-key", excerpt));
        }
    }

    // --- JWT triple: eyJ...eyJ...eyJ (header.payload.signature) ---
    // A JWT starts with "eyJ" (base64url of `{"`) and has exactly two dots.
    if let Some(m) = find_jwt(text) {
        return Some(build_match("jwt", m));
    }

    // --- URL userinfo: scheme://user:pass@host ---
    if let Some(m) = find_url_userinfo(text) {
        return Some(build_match("url-userinfo", m));
    }

    None
}

/// Locate the first token in `text` that starts with `needle` and has a
/// total length >= `min_len`.  Returns a slice of the full token on match.
fn find_prefix_token<'a>(text: &'a str, needle: &str, min_len: usize) -> Option<&'a str> {
    let mut start = 0;
    while let Some(rel) = text[start..].find(needle) {
        let abs = start + rel;
        // Require that the needle starts at a token boundary (start-of-string
        // or preceded by whitespace / punctuation that isn't alphanumeric).
        let at_boundary = abs == 0 || {
            let prev = text[..abs].chars().next_back().unwrap_or(' ');
            !prev.is_alphanumeric()
        };
        if at_boundary {
            let token = extract_token(&text[abs..]);
            if token.len() >= min_len {
                return Some(token);
            }
        }
        start = abs + needle.len().max(1);
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

/// Detect `scheme://user:pass@host` patterns where the `user:pass` portion
/// contains actual credentials (both user and pass non-empty).
fn find_url_userinfo(text: &str) -> Option<&str> {
    let mut search = text;
    let mut base = 0usize;
    while let Some(at_rel) = search.find("://") {
        let at_abs = base + at_rel;
        // After `://`, look for `@` before the next `/`, `?`, ` `, or newline.
        let rest_start = at_abs + 3;
        let rest = &text[rest_start..];
        if let Some(at_pos) = rest.find('@') {
            let userinfo = &rest[..at_pos];
            // Must contain a colon and both sides non-empty.
            if let Some(colon) = userinfo.find(':') {
                let user = &userinfo[..colon];
                let pass = &userinfo[colon + 1..];
                if !user.is_empty() && !pass.is_empty() && pass.len() >= 4 {
                    // Return a slice starting from the scheme.
                    // Walk back from at_abs to find the start of the scheme.
                    let scheme_start = text[..at_abs]
                        .rfind(|c: char| {
                            !c.is_ascii_alphanumeric() && c != '+' && c != '-' && c != '.'
                        })
                        .map(|p| p + 1)
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

// ─── Layer 2: entropy heuristic ─────────────────────────────────────────────

/// Trigger words near which high-entropy tokens are suspicious.
///
/// The bare substring `token` is NOT in this list because it fires on benign
/// terms like `tokenizer`, `token_count`, and `next_token`.  Instead we use
/// the dedicated boundary-aware helpers `has_standalone_token` (standalone word)
/// and `has_token_assignment` (`token=` / `token:` with word boundary before).
const TRIGGER_WORDS: &[&str] = &[
    "key",
    "secret",
    "password",
    "passwd",
    "credential",
    "bearer",
    "auth",
    "apikey",
    "api_key",
    "access_key",
    "private_key",
];

/// Minimum token length to apply the entropy check.
const MIN_ENTROPY_LEN: usize = 24;

/// Shannon entropy threshold (bits per character) above which a token is
/// considered high-entropy.  7.0 corresponds to ~99% utilisation of a
/// 128-symbol alphabet — typical for random base64/hex.
const ENTROPY_THRESHOLD: f64 = 4.5;

/// Window around a trigger word in which a high-entropy token must appear.
const TRIGGER_WINDOW: usize = 120;

/// Largest index `<= i` that lies on a UTF-8 char boundary of `s`. Stable
/// replacement for the unstable `str::floor_char_boundary`; used to snap
/// byte-offset windows that may land inside a multibyte char before slicing.
fn floor_char_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn check_entropy_heuristic(text: &str) -> Option<SecretMatch> {
    // Tokenize into maximal ASCII non-whitespace runs, recording each run's byte
    // offset.  Non-ASCII characters are delimiters (alongside ASCII whitespace):
    // real base64/hex/base64url credentials are ASCII, so splitting on non-ASCII
    // isolates an ASCII credential glued to CJK text/punctuation/fullwidth
    // whitespace, while a run of natural-language CJK yields no ASCII run long
    // enough to trip the length floor below.  On pure-ASCII input this is
    // identical to `split_ascii_whitespace`.
    let tokens: Vec<(usize, &str)> = text
        .split(|c: char| c.is_ascii_whitespace() || !c.is_ascii())
        .filter(|t| !t.is_empty())
        .map(|t| {
            let offset = t.as_ptr() as usize - text.as_ptr() as usize;
            (offset, t)
        })
        .collect();

    for &(tok_offset, raw_token) in &tokens {
        // Strip common delimiters that wrap the actual value.
        let token = strip_delimiters(raw_token);
        if token.len() < MIN_ENTROPY_LEN {
            continue;
        }

        // `token` is ASCII here (non-ASCII was split out at tokenization), so
        // `shannon_entropy` over its bytes is a true per-character entropy.

        // UUID and sha-prefixed base64 content hashes (SRI / npm lockfile) are
        // unconditionally allowlisted: their forms are unambiguous regardless of
        // surrounding context.
        if is_uuid_canonical(token) || is_base64_content_hash(token) {
            continue;
        }

        // Compute the trigger window before deciding whether to allowlist hex
        // tokens.  A pure-hex token near a credential trigger word cannot be
        // safely assumed to be a non-secret hash and must be entropy-checked.
        let window_start = floor_char_boundary(text, tok_offset.saturating_sub(TRIGGER_WINDOW));
        let window_end = floor_char_boundary(text, tok_offset + raw_token.len() + TRIGGER_WINDOW);
        let window = &text[window_start..window_end];
        let low_window = window.to_ascii_lowercase();

        let near_trigger = TRIGGER_WORDS.iter().any(|tw| low_window.contains(tw))
            || has_standalone_token(&low_window)
            || has_token_assignment(&low_window);

        // Pure hex tokens (git SHA, checksum digests) are allowlisted only when
        // they are NOT near a credential trigger.
        if !near_trigger && is_pure_hex(token) {
            continue;
        }

        // Hex API keys (AWS secret access key, Stripe test keys, random hex
        // tokens) are pure hex yet are real credentials.  The entropy heuristic
        // cannot catch them — hex alphabet maxes at log2(16) = 4.0 bits/char,
        // which is always below ENTROPY_THRESHOLD (4.5).  A credential-shaped
        // hex token (32 / 40 / 64 / 128 chars) near a trigger word is always
        // flagged.  Credential triggers dominate: adding "sha" or "hash" to
        // the window does not rescue the token — a caller controlling the prose
        // could trivially bypass the gate with one extra word.  Safe git SHAs
        // and content-hash digests do not appear near credential trigger words
        // and are already allowed via the `!near_trigger && is_pure_hex` path.
        const HEX_CREDENTIAL_LENGTHS: &[usize] = &[32, 40, 64, 128];
        if near_trigger && is_pure_hex(token) && HEX_CREDENTIAL_LENGTHS.contains(&token.len()) {
            return Some(build_match("hex-credential-token", token));
        }

        let entropy = shannon_entropy(token.as_bytes());
        if entropy < ENTROPY_THRESHOLD {
            continue;
        }

        // High-entropy token in trigger context — flag it.
        if near_trigger {
            return Some(build_match("high-entropy-token", token));
        }
    }
    None
}

/// Returns `true` when `low_window` contains the word `token` as a standalone
/// word — i.e. surrounded by non-alphanumeric boundaries — but NOT as part of
/// compound identifiers such as `tokenizer`, `token_count`, or `next_token`.
fn has_standalone_token(low_window: &str) -> bool {
    let needle = "token";
    let mut start = 0;
    while let Some(rel) = low_window[start..].find(needle) {
        let abs = start + rel;
        let before_ok = abs == 0
            || low_window[..abs]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        let after_end = abs + needle.len();
        let after_ok = after_end >= low_window.len()
            || low_window[after_end..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        if before_ok && after_ok {
            return true;
        }
        start = abs + needle.len().max(1);
    }
    false
}

/// Returns `true` when `low_window` contains the assignment form `token=` or
/// `token:` where the `token` identifier has a word boundary BEFORE it.
///
/// This is boundary-aware so that compound identifiers like `next_token:` or
/// `pagination_token=` do NOT trigger — only a standalone `token=`/`token:`
/// at the start of a field name does.
///
/// Examples that return `true`:  `token=<value>`, `token: <value>`,
///   `"token": "<value>"` (JSON key-value pairs).
/// Examples that return `false`: `next_token: <value>`,
///   `pagination_token=<value>`, `token_count: <value>`.
fn has_token_assignment(low_window: &str) -> bool {
    let needle = "token";
    let mut start = 0;
    while let Some(rel) = low_window[start..].find(needle) {
        let abs = start + rel;
        // Require a word boundary BEFORE `token`.
        let before_ok = abs == 0
            || low_window[..abs]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        let after_end = abs + needle.len();
        // Require `=` or `:` immediately after `token` (possibly with surrounding
        // whitespace or quotes stripped by the time we see the lowercased window).
        let after_char = low_window[after_end..].chars().next();
        let after_is_assign = matches!(after_char, Some('=') | Some(':'));
        if before_ok && after_is_assign {
            return true;
        }
        start = abs + needle.len().max(1);
    }
    false
}

// ─── Allowlist helpers ───────────────────────────────────────────────────────

/// Returns `true` for pure-hex tokens (case-insensitive, optional `0x`/`0X` prefix,
/// 8–128 chars) — git SHAs, checksum digests, uuid-hex without hyphens.
///
/// This helper is used with context: pure-hex tokens near credential trigger words
/// are NOT allowlisted (see `check_entropy_heuristic`).  Only call this function
/// when you have already confirmed no trigger context is nearby.
fn is_pure_hex(token: &str) -> bool {
    let hex_part = token
        .strip_prefix("0x")
        .or(token.strip_prefix("0X"))
        .unwrap_or(token);
    hex_part.len() >= 8 && hex_part.len() <= 128 && hex_part.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Returns `true` for tokens that are unambiguous base64/base64url content
/// hashes with an explicit `sha<N>-` prefix (SRI hash, npm lockfile integrity).
///
/// Criteria:
/// - Token starts with `sha<digits>-` (e.g. `sha256-`, `sha384-`, `sha512-`).
/// - The body after the prefix matches a SHA-family length (43, 64, or 86–88
///   unpadded chars).
/// - Every byte in the body is a standard-base64 or URL-safe-base64 character.
/// - Does NOT start with a known vendor-token prefix (those are credentials
///   regardless of alphabet).
///
/// Bare base64 tokens of those lengths WITHOUT the `sha<N>-` prefix are NOT
/// allowlisted here — a 43-char base64url API token near the word "key" is
/// indistinguishable from a sha256 hash body without the prefix, so we require
/// the explicit prefix to avoid false-negative credential escapes.
fn is_base64_content_hash(token: &str) -> bool {
    // Known vendor prefixes — never allowlist even if they look like base64.
    // Includes bare `sk-` to prevent OpenAI-shaped tokens from being allowlisted.
    const VENDOR_PREFIXES: &[&str] = &[
        "sk-",
        "rk_live_",
        "fm2_",
        "vercel_",
        "xoxb-",
        "xoxa-",
        "xoxp-",
        "xoxr-",
        "xoxs-",
        "ghp_",
        "gho_",
        "github_pat_",
        "AKIA",
        "ASIA",
        "AGE-SECRET-KEY-",
        "FlyV1",
    ];
    if VENDOR_PREFIXES.iter().any(|p| token.starts_with(p)) {
        return false;
    }
    // Require an explicit SRI `sha[0-9]+-` prefix.  Bare base64 at sha-length
    // is NOT allowlisted — it is indistinguishable from a real API token.
    let body = if let Some(rest) = token.strip_prefix("sha") {
        // rest starts with digits followed by '-'
        let dash = rest.find('-').unwrap_or(rest.len());
        let digits = &rest[..dash];
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && dash < rest.len() {
            &rest[dash + 1..] // everything after "sha<digits>-"
        } else {
            return false; // no valid sha<N>- prefix → not a known content hash
        }
    } else {
        return false; // no sha prefix → not allowlisted
    };
    // Strip optional padding (at most 2 `=`).
    let stripped = body.trim_end_matches('=');
    let pad_removed = body.len() - stripped.len();
    if pad_removed > 2 {
        return false;
    }
    // Accept only SHA-family content-hash lengths (43, 64, 86–88 chars unpadded).
    let n = stripped.len();
    if n != 43 && n != 64 && !(86..=88).contains(&n) {
        return false;
    }
    // Accept both standard-base64 and URL-safe-base64 alphabets.
    stripped
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'-' || b == b'_')
}

/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`
fn is_uuid_canonical(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    b[8] == b'-'
        && b[13] == b'-'
        && b[18] == b'-'
        && b[23] == b'-'
        && b[..8].iter().all(|c| c.is_ascii_hexdigit())
        && b[9..13].iter().all(|c| c.is_ascii_hexdigit())
        && b[14..18].iter().all(|c| c.is_ascii_hexdigit())
        && b[19..23].iter().all(|c| c.is_ascii_hexdigit())
        && b[24..].iter().all(|c| c.is_ascii_hexdigit())
}

/// Strip common wrapping characters (`"`, `'`, `` ` ``, `:`, `=`) from both ends.
fn strip_delimiters(s: &str) -> &str {
    s.trim_matches(|c| matches!(c, '"' | '\'' | '`' | ':' | '=' | ',' | ';'))
}

// ─── Utilities ───────────────────────────────────────────────────────────────

/// Extract a contiguous token (non-whitespace chars) starting at the beginning of `s`.
fn extract_token(s: &str) -> &str {
    let end = s
        .find(|c: char| c.is_whitespace() || c == '\n' || c == '\r')
        .unwrap_or(s.len());
    &s[..end]
}

/// Shannon entropy in bits per character.
///
/// H = -∑ p_i log2(p_i)
fn shannon_entropy(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let len = bytes.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// Build a `SecretMatch` from a detector name and the candidate string.
///
/// The masked excerpt is: first 6 chars + "..." + total length.
/// Never includes more than 6 chars of the actual value.
fn build_match(detector: &'static str, candidate: &str) -> SecretMatch {
    let chars: Vec<char> = candidate.chars().collect();
    let preview: String = chars.iter().take(6).collect();
    let masked = format!("{}...{}chars", preview, chars.len());
    SecretMatch { detector, masked }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Catch suite ──────────────────────────────────────────────────────────

    #[test]
    fn blocks_aws_akia() {
        // FAKE key: prefix is real shape, 16-char suffix invented.
        let fake = "AKIAFAKEKEY1234567890";
        assert!(scan(fake).is_some(), "AKIA must be caught");
        let m = scan(fake).unwrap();
        assert_eq!(m.detector, "aws-access-key-id");
        // Masked excerpt must not echo the full key.
        assert!(
            !m.masked.contains("FAKEKEY1234567890"),
            "must not echo the secret: {}",
            m.masked
        );
    }

    #[test]
    fn blocks_aws_asia() {
        let fake = "ASIAFAKEKEY00000000000";
        let m = scan(fake);
        assert!(m.is_some(), "ASIA must be caught");
        assert_eq!(m.unwrap().detector, "aws-access-key-id");
    }

    #[test]
    fn blocks_github_ghp() {
        // 36 chars total to pass min_len.
        let fake = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        assert!(scan(fake).is_some(), "ghp_ must be caught");
    }

    #[test]
    fn blocks_github_gho() {
        let fake = "gho_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        assert!(scan(fake).is_some(), "gho_ must be caught");
    }

    #[test]
    fn blocks_github_pat() {
        let fake = "github_pat_AAAAAABBBBBBCCCCCC";
        assert!(scan(fake).is_some(), "github_pat_ must be caught");
    }

    #[test]
    fn blocks_openai_sk() {
        let fake = "sk-aaaaaabbbbbbccccccddddddeeeeeeffffgg";
        assert!(scan(fake).is_some(), "sk- must be caught");
    }

    #[test]
    fn blocks_anthropic_sk_ant() {
        let fake = "sk-ant-api03-AAAAAAAAAAAAAAA";
        assert!(scan(fake).is_some(), "sk-ant- must be caught");
        assert_eq!(scan(fake).unwrap().detector, "anthropic-api-key");
    }

    #[test]
    fn blocks_stripe_live() {
        let fake = "sk_live_FAKESTRIPE0000000000000"; // gitleaks:allow
        assert!(scan(fake).is_some(), "sk_live_ must be caught");
        assert_eq!(scan(fake).unwrap().detector, "stripe-secret-key");
    }

    #[test]
    fn blocks_stripe_restricted() {
        let fake = "rk_live_FAKESTRIPE0000000000000"; // gitleaks:allow
        assert!(scan(fake).is_some(), "rk_live_ must be caught");
        assert_eq!(scan(fake).unwrap().detector, "stripe-restricted-key");
    }

    #[test]
    fn blocks_fly_flyv1() {
        let fake = "FlyV1 FAKEFLYTOKEN000000000000000000";
        assert!(scan(fake).is_some(), "FlyV1 must be caught");
        assert_eq!(scan(fake).unwrap().detector, "fly-token");
    }

    #[test]
    fn blocks_fly_fm2() {
        let fake = "fm2_FAKEFLYTOKEN00000000000000000";
        assert!(scan(fake).is_some(), "fm2_ must be caught");
        assert_eq!(scan(fake).unwrap().detector, "fly-token");
    }

    #[test]
    fn blocks_vercel_token() {
        let fake = "vercel_FAKETOKEN00000000000000000";
        assert!(scan(fake).is_some(), "vercel_ must be caught");
        assert_eq!(scan(fake).unwrap().detector, "vercel-token");
    }

    #[test]
    fn blocks_slack_xoxb() {
        let fake = "xoxb-FAKE-SLACKTOKEN-000000000000000000000000";
        assert!(scan(fake).is_some(), "xoxb- must be caught");
        assert_eq!(scan(fake).unwrap().detector, "slack-token");
    }

    #[test]
    fn blocks_pem_private_key() {
        // Split the header so the literal detector-trigger string is not present
        // verbatim in source — pre-commit's detect-private-key hook would fire.
        // The gate detects it at runtime because scan() sees the assembled string.
        let header = ["-----BEGIN RSA", " PRIVATE KEY-----"].concat(); // gitleaks:allow
        let fake = format!("{}\nMIIEo\u{2026}\n-----END RSA PRIVATE KEY-----", header);
        assert!(scan(&fake).is_some(), "PEM private key must be caught");
        assert_eq!(scan(&fake).unwrap().detector, "pem-private-key");
    }

    #[test]
    fn blocks_pem_ec_private_key() {
        let header = ["-----BEGIN EC", " PRIVATE KEY-----"].concat(); // gitleaks:allow
        let fake = format!("{}\nMHQCAQEE\u{2026}\n-----END EC PRIVATE KEY-----", header);
        assert!(scan(&fake).is_some(), "EC PEM must be caught");
    }

    #[test]
    fn blocks_age_secret_key() {
        // AGE-SECRET-KEY- followed by 59 base32 chars (Bech32m body).
        let fake = "AGE-SECRET-KEY-1QQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ";
        assert!(scan(fake).is_some(), "AGE-SECRET-KEY- must be caught");
        assert_eq!(scan(fake).unwrap().detector, "age-secret-key");
    }

    #[test]
    fn blocks_jwt_triple() {
        // Synthetic JWT structure: header.payload.signature (no real key).
        let fake = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.FAKE_SIG_XXXXXXXXXXXX"; // gitleaks:allow
        assert!(scan(fake).is_some(), "JWT triple must be caught");
        assert_eq!(scan(fake).unwrap().detector, "jwt");
    }

    #[test]
    fn blocks_url_userinfo() {
        let fake = "postgresql://dbuser:S3cr3tP4ss@db.example.com:5432/mydb";
        assert!(scan(fake).is_some(), "URL userinfo must be caught");
        assert_eq!(scan(fake).unwrap().detector, "url-userinfo");
    }

    #[test]
    fn blocks_high_entropy_near_bearer_word() {
        // 32 random-looking base64 chars adjacent to the word "bearer".
        let fake = "Bearer token: Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM"; // gitleaks:allow
        assert!(
            scan(fake).is_some(),
            "high-entropy value near 'bearer' must be caught"
        );
        assert_eq!(scan(fake).unwrap().detector, "high-entropy-token");
    }

    #[test]
    fn blocks_high_entropy_near_secret_word() {
        let fake = "secret=Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM"; // gitleaks:allow
        assert!(
            scan(fake).is_some(),
            "high-entropy value near 'secret' must be caught"
        );
    }

    #[test]
    fn error_message_masks_secret() {
        let fake = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let m = scan(fake).unwrap();
        // Masked form: first 6 chars + "...N chars".
        // Must NOT contain the full suffix.
        let masked = &m.masked;
        assert!(
            !masked.contains("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
            "mask must not echo the full secret value; got: {masked}"
        );
        // Must start with "ghp_AA" (first 6 chars of the token).
        assert!(
            masked.starts_with("ghp_AA"),
            "mask must show first 6 chars; got: {masked}"
        );
    }

    // ── False-positive suite ─────────────────────────────────────────────────

    #[test]
    fn allows_sha256_hex() {
        // 64-char lowercase hex — typical sha256 digest.
        let sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert!(
            scan(sha).is_none(),
            "sha256 hex must pass (allowlisted); fired: {:?}",
            scan(sha)
        );
    }

    #[test]
    fn allows_uuid() {
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        assert!(
            scan(uuid).is_none(),
            "UUID must pass; fired: {:?}",
            scan(uuid)
        );
    }

    #[test]
    fn allows_git_sha() {
        // 40-char lowercase git SHA.
        let sha = "d362950a3c9b1a4cb47d97f1623e38f1a1e6bcdf";
        assert!(
            scan(sha).is_none(),
            "git SHA must pass; fired: {:?}",
            scan(sha)
        );
    }

    #[test]
    fn allows_normal_prose() {
        let prose =
            "The FlashAttention paper introduces IO-aware tiling for transformer self-attention.";
        assert!(scan(prose).is_none(), "normal prose must pass");
    }

    #[test]
    fn allows_code_snippet() {
        let code = r#"fn create_entity(name: &str, kind: &str) -> RuntimeResult<Entity> {
    self.validate_entity_kind(kind)?;
    Ok(Entity::new("local", kind, name))
}"#;
        assert!(
            scan(code).is_none(),
            "code snippet must pass; fired: {:?}",
            scan(code)
        );
    }

    #[test]
    fn allows_long_url_without_credentials() {
        let url = "https://docs.example.com/api/v2/entities?kind=concept&limit=100";
        assert!(scan(url).is_none(), "URL without userinfo must pass");
    }

    #[test]
    fn allows_base64_image_stub() {
        // Realistic short base64 data URI stub — no trigger words, below threshold length.
        let b64 = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVQI12NgAAIABQ";
        assert!(
            scan(b64).is_none(),
            "base64 image stub without trigger word must pass; fired: {:?}",
            scan(b64)
        );
    }

    #[test]
    fn allows_long_plain_url() {
        let url = "https://api.github.com/repos/ohdearquant/khive/pulls/76/comments?per_page=100";
        assert!(
            scan(url).is_none(),
            "plain URL must pass; fired: {:?}",
            scan(url)
        );
    }

    #[test]
    fn allows_manifest_content_hash() {
        // A string like what appears in Cargo.lock or npm lockfiles.
        let line =
            "checksum = \"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\"";
        assert!(
            scan(line).is_none(),
            "manifest content hash line must pass; fired: {:?}",
            scan(line)
        );
    }

    #[test]
    fn masked_excerpt_format() {
        let fake = "AKIAFAKEKEY1234567890";
        let m = scan(fake).unwrap();
        // Format: first6...Nchars
        assert!(m.masked.contains("..."), "masked must contain '...'");
        assert!(m.masked.ends_with("chars"), "masked must end with 'chars'");
    }

    // ── Gate function ────────────────────────────────────────────────────────

    #[test]
    fn check_returns_ok_for_safe_content() {
        assert!(check("A normal memory note about LoRA.").is_ok());
    }

    #[test]
    fn check_returns_err_for_secret() {
        let fake = "AKIAFAKEKEY1234567890";
        let result = check(fake);
        assert!(result.is_err(), "check must fail for AKIA key");
        let err = result.unwrap_err();
        assert!(
            matches!(err, RuntimeError::SecretDetected(_)),
            "error variant must be SecretDetected"
        );
    }

    // ── Entropy helpers ──────────────────────────────────────────────────────

    #[test]
    fn entropy_of_uniform_string_is_zero() {
        let s = "aaaaaaaaaaaaaaaa";
        assert!(shannon_entropy(s.as_bytes()) < 0.01);
    }

    #[test]
    fn entropy_of_random_bytes_is_high() {
        // A truly random-looking string should exceed 4.5 bits/char.
        let s = b"X9kZ2vQpLrT8nJwYuAeHfBsDcGiONvM1"; // 32 mixed base64 chars
        assert!(shannon_entropy(s) > 4.5, "entropy={}", shannon_entropy(s));
    }

    #[test]
    fn cjk_prose_near_trigger_is_not_flagged() {
        // Regression: a multibyte CJK run (~19 chars = 57 bytes) clears the
        // byte-length floor, and `shannon_entropy` over UTF-8 bytes reads it as
        // high-entropy — so a Chinese title near the `auth` trigger word used to
        // false-positive as `high-entropy-token`.  Non-ASCII tokens are now
        // skipped by the entropy heuristic: real base64/hex credentials are
        // ASCII, so this cannot hide a secret.
        let content = "更新 auth 配置数据库连接管理系统核心模块设计文档";
        assert!(
            check(content).is_ok(),
            "CJK prose near a trigger word must not be flagged as a secret"
        );
    }

    #[test]
    fn ascii_secret_near_trigger_still_flagged() {
        // The non-ASCII skip must NOT weaken detection of genuine ASCII
        // high-entropy credentials near a trigger word.
        let content = "api_key X9kZ2vQpLrT8nJwYuAeHfBsDcGiONvM1";
        assert!(
            check(content).is_err(),
            "ASCII high-entropy token near a trigger word must still be blocked"
        );
    }

    #[test]
    fn ascii_secret_in_cjk_context_does_not_panic_and_is_flagged() {
        // The ±120-byte trigger window around an ASCII token can land in the
        // middle of a multibyte CJK character when the token is embedded in
        // non-Latin prose.  Slicing on a non-char-boundary would panic — the
        // window bounds are snapped via `floor_char_boundary`.  Detection of
        // the genuine ASCII secret must still fire.
        let cjk = "数据库连接管理系统核心模块设计文档".repeat(6); // 17 chars × 6 = 306 bytes
                                                                  // The leading single-byte `x` breaks 3-byte CJK alignment so the window
                                                                  // start (token_offset - 120) lands mid-character without the snap.
        let content = format!("{cjk}x api_key X9kZ2vQpLrT8nJwYuAeHfBsDcGiONvM1 {cjk}");
        assert!(
            check(&content).is_err(),
            "ASCII secret in CJK context must still be blocked (and must not panic)"
        );
    }

    #[test]
    fn ascii_secret_glued_to_cjk_is_still_flagged() {
        // Regression: a prefixless high-entropy credential glued (no ASCII
        // whitespace) to CJK text, CJK brackets/quotes, a fullwidth space, or a
        // fullwidth colon used to slip through, because the whole whitespace token
        // contained a non-ASCII byte and was skipped wholesale.  Non-ASCII is now
        // a token delimiter, so the ASCII credential run is isolated and
        // entropy-checked while the surrounding ±120-byte window still sees the
        // trigger word.
        let secret = "X9kZ2vQpLrT8nJwYuAeHfBsDcGiONvM1"; // gitleaks:allow
        let cases = [
            format!("api_key {secret}数据"),     // CJK suffix glued to the token
            format!("api_key 「{secret}」"),     // CJK brackets wrap the token
            format!("api_key　{secret}"),        // U+3000 ideographic space separator
            format!("api_key：{secret}"),        // U+FF1A fullwidth colon separator
            format!("数据{secret}更新 api_key"), // CJK-glued prefix, trigger after
        ];
        for content in &cases {
            assert!(
                check(content).is_err(),
                "ASCII secret glued to CJK must be blocked: {content:?}"
            );
        }
    }

    #[test]
    fn high_entropy_ascii_run_without_trigger_is_not_flagged() {
        // The non-ASCII-as-delimiter change must not weaken the trigger-context
        // discipline: a high-entropy ASCII run isolated from CJK prose but NOT
        // near a credential trigger word is still allowed (only the tokenizer
        // changed, not the `near_trigger` gate).
        let secret = "X9kZ2vQpLrT8nJwYuAeHfBsDcGiONvM1"; // gitleaks:allow
        let content = format!("数据库连接{secret}核心模块设计文档");
        assert!(
            check(&content).is_ok(),
            "high-entropy ASCII run with no trigger word must not be flagged"
        );
    }

    #[test]
    fn allowlist_passes_sha256() {
        // A plain sha256 hex digest passes via `is_pure_hex` (not `is_allowlisted`
        // because hex is now context-dependent; this tests the primitive directly).
        let sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert!(is_pure_hex(sha));
    }

    #[test]
    fn allowlist_passes_uuid_canonical() {
        assert!(is_uuid_canonical("550e8400-e29b-41d4-a716-446655440000"));
    }

    #[test]
    fn allowlist_does_not_pass_mixed_token() {
        // A token that starts with letters but mixes in non-hex chars.
        assert!(!is_pure_hex("sk-aaaaaabbbbbbccccccddddddeeeeeeffffgg"));
    }

    // ── Structured-field gate helpers ────────────────────────────────────────

    #[test]
    fn check_json_blocks_secret_in_object_value() {
        let props = serde_json::json!({ "api_key": "AKIAFAKEKEY1234567890" });
        assert!(
            check_json(&props).is_err(),
            "secret in properties object value must be blocked"
        );
    }

    #[test]
    fn check_json_blocks_secret_in_nested_object() {
        let props = serde_json::json!({ "credentials": { "token": "sk-proj-FAKEKEY00000000000000000000000000000000" } }); // gitleaks:allow
        assert!(
            check_json(&props).is_err(),
            "secret in nested properties object must be blocked"
        );
    }

    #[test]
    fn check_json_blocks_secret_in_array() {
        let props = serde_json::json!(["normal", "AKIAFAKEKEY1234567890"]);
        assert!(
            check_json(&props).is_err(),
            "secret in JSON array must be blocked"
        );
    }

    #[test]
    fn check_json_passes_safe_properties() {
        let props = serde_json::json!({
            "domain": "attention",
            "status": "researched",
            "year": 2024
        });
        assert!(
            check_json(&props).is_ok(),
            "normal properties must pass; fired: {:?}",
            check_json(&props).err()
        );
    }

    #[test]
    fn check_tags_blocks_credential_tag() {
        let tags = vec![
            "type:concept".to_string(),
            "AKIAFAKEKEY1234567890".to_string(),
        ];
        assert!(
            check_tags(&tags).is_err(),
            "credential-shaped tag must be blocked"
        );
    }

    #[test]
    fn check_tags_passes_normal_tags() {
        let tags = vec!["type:concept".to_string(), "domain:attention".to_string()];
        assert!(
            check_tags(&tags).is_ok(),
            "normal tags must pass; fired: {:?}",
            check_tags(&tags).err()
        );
    }

    // ── False-positive: sk-learn and scikit-learn slugs ──────────────────────

    #[test]
    fn allows_sk_learn_prose() {
        // scikit-learn slug used as an entity name or knowledge atom.
        let texts = &[
            "sk-learn is a Python machine learning library",
            "sk-learn-compatible transformer pipeline reference",
            "sk-learn scikit-learn estimator interface",
        ];
        for t in texts {
            assert!(
                scan(t).is_none(),
                "sk-learn prose must pass; fired: {:?} on {:?}",
                scan(t),
                t
            );
        }
    }

    #[test]
    fn blocks_openai_sk_proj_not_confused_with_sk_learn() {
        // Real OpenAI key shape must still be caught.
        let fake = "sk-proj-FAKEKEY00000000000000000000000000000000"; // gitleaks:allow
        assert!(
            scan(fake).is_some(),
            "sk-proj- key must still be caught after sk-learn exemption"
        );
    }

    // ── False-positive: SRI / tokenizer hash metadata ────────────────────────

    #[test]
    fn allows_sri_hash() {
        // SRI hash as used in HTML integrity attributes (sha384, base64-encoded).
        // Placed near the word "key" to test the entropy heuristic allowlist.
        let line = "integrity key: sha384-oqVuAfXRKap7fdgcCY5uykM6+R9GqQ8K/uxy9rx7HNQlGYl1kPzQho1wx4JwY8wC";
        assert!(
            scan(line).is_none(),
            "SRI hash must pass; fired: {:?}",
            scan(line)
        );
    }

    #[test]
    fn allows_base64_tokenizer_hash_metadata() {
        // Tokenizer metadata containing a base64 hash near technical keywords.
        let line = "tokenizer_vocab_hash: Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM"; // gitleaks:allow
        assert!(
            scan(line).is_none(),
            "tokenizer hash metadata must pass; fired: {:?}",
            scan(line)
        );
    }

    #[test]
    fn allows_npm_lockfile_integrity() {
        // npm lockfile integrity line with sha512 base64url hash (86 base64 chars + ==).
        // sha512 digest = 64 bytes → base64 = 88 chars (86 unpadded + ==).
        let body_86 = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM1234567890abcdefghijklmnopqrstuvwxABCDEFGHIJKLMNOPQRST";
        assert_eq!(body_86.len(), 86, "test body must be exactly 86 chars");
        let line = format!(
            "resolved: https://registry.npmjs.org/foo/-/foo-1.0.0.tgz\nintegrity: sha512-{body_86}=="
        );
        assert!(
            scan(&line).is_none(),
            "npm lockfile integrity must pass; fired: {:?}",
            scan(&line)
        );
    }

    // ── False-positive: tokenizer vs token trigger word ─────────────────────

    #[test]
    fn allows_tokenizer_vocab_hash_no_block() {
        // `tokenizer_vocab_hash` contains the substring "token" but NOT as a
        // standalone word (followed by 'i' which is alphanumeric), so the
        // standalone-token boundary check must not fire here.
        let line = "tokenizer_vocab_hash = Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM"; // gitleaks:allow
        assert!(
            scan(line).is_none(),
            "tokenizer_vocab_hash must pass; 'token' is only standalone-word matched; fired: {:?}",
            scan(line)
        );
    }

    // ── True-positives: bare base64 at sha-lengths near trigger words ────────

    #[test]
    fn blocks_bare_base64url_43chars_near_key() {
        // A 43-char base64url token (= sha256 body length) near the word "key".
        // Without a sha<N>- prefix this MUST be caught, not allowlisted.
        let token_43 = "wJalrXUtnFEMI-K7MDENGbPxRfiCYEXAMPLEKEYX123"; // gitleaks:allow
        assert_eq!(token_43.len(), 43, "test token must be exactly 43 chars");
        let line = format!("api key {token_43}");
        assert!(
            scan(&line).is_some(),
            "43-char base64url token near 'key' must be caught (no sha-prefix = not a hash); fired: {:?}",
            scan(&line)
        );
    }

    #[test]
    fn blocks_bare_base64url_64chars_near_secret() {
        // A 64-char base64url token (= sha384 body length) near "secret".
        // Must be caught without sha<N>- prefix.
        let token_64 = "wJalrXUtnFEMI-K7MDENGbPxRfiCYEXAMPLEKEYX123wJalrXUtnFEMI-K7MDENa"; // gitleaks:allow
        assert_eq!(token_64.len(), 64, "test token must be exactly 64 chars");
        let line = format!("secret: {token_64}");
        assert!(
            scan(&line).is_some(),
            "64-char base64url token near 'secret' must be caught; got: {:?}",
            scan(&line)
        );
    }

    #[test]
    fn blocks_bare_base64url_86chars_near_auth() {
        // An 86-char base64url token (= sha512 body length) near "auth".
        // Must be caught without sha<N>- prefix.
        let token_86 = "wJalrXUtnFEMI-K7MDENGbPxRfiCYEXAMPLEKEYX123wJalrXUtnFEMI-K7MDENwJalrXUtnFEMI-K7MDENabc"; // gitleaks:allow
        assert_eq!(token_86.len(), 86, "test token must be exactly 86 chars");
        let line = format!("auth header {token_86}");
        assert!(
            scan(&line).is_some(),
            "86-char base64url token near 'auth' must be caught; got: {:?}",
            scan(&line)
        );
    }

    // ── True-positives: standalone `token` trigger ───────────────────────────

    #[test]
    fn blocks_service_token_opaque_value() {
        // "service token <opaque-high-entropy>" — `token` as a standalone word
        // with a high-entropy value must be caught.
        let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
        assert!(
            opaque.len() >= 24,
            "opaque must be long enough for entropy check"
        );
        let line = format!("service token {opaque}");
        assert!(
            scan(&line).is_some(),
            "service token <opaque> must be caught by standalone 'token' check; got: {:?}",
            scan(&line)
        );
    }

    #[test]
    fn blocks_token_equals_credential() {
        // `token=<high-entropy>` (assignment form) must be caught via has_token_assignment.
        let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
        let line = format!("token={opaque}");
        assert!(
            scan(&line).is_some(),
            "token=<value> must be caught via token= trigger; got: {:?}",
            scan(&line)
        );
    }

    #[test]
    fn blocks_token_colon_credential() {
        // `token: <high-entropy>` (key-value form) must be caught via has_token_assignment.
        let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
        let line = format!("token: {opaque}");
        assert!(
            scan(&line).is_some(),
            "token: <value> must be caught via token: trigger; got: {:?}",
            scan(&line)
        );
    }

    #[test]
    fn allows_next_token_technical_context() {
        // `next_token` is a technical term; the high-entropy value here has low
        // entropy anyway, so it must pass.
        let line = "next_token: cursor-page-2-abcdef12345678";
        assert!(
            scan(line).is_none(),
            "next_token technical context must not be blocked; fired: {:?}",
            scan(line)
        );
    }

    // ── Finding 6: boundary-aware token= / token: — compound identifiers must pass ──

    #[test]
    fn allows_next_token_high_entropy_cursor() {
        // `next_token:` with a realistic high-entropy pagination cursor must NOT be
        // blocked.  `next_token` has `_token` suffix — not a standalone assignment form.
        let cursor = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
        let line = format!("next_token: {cursor}");
        assert!(
            scan(&line).is_none(),
            "next_token with high-entropy cursor must pass (compound identifier); fired: {:?}",
            scan(&line)
        );
    }

    #[test]
    fn allows_token_count_high_entropy() {
        // `token_count:` with a high-entropy value must NOT be blocked.
        // `token_count` has `token_` prefix — the word boundary after `token` is `_`,
        // which is excluded by has_token_assignment.
        let opaque = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvMabcdef"; // gitleaks:allow
        let line = format!("token_count: {opaque}");
        assert!(
            scan(&line).is_none(),
            "token_count with high-entropy value must pass; fired: {:?}",
            scan(&line)
        );
    }

    // ── Finding 5: hex allowlist is not applied when trigger context is present ─
    //
    // Pure hex strings have a theoretical maximum entropy of log2(16) = 4.0 bits/char,
    // which is below the ENTROPY_THRESHOLD of 4.5.  That means pure hex tokens cannot
    // reach the entropy threshold and will never be flagged by the heuristic alone.
    //
    // However, the hex allowlist was previously applied BEFORE the trigger window was
    // computed, meaning a future threshold reduction or edge case could silently
    // skip credential-context hex.  The fix: compute trigger context first; only
    // apply the hex allowlist when NOT near a trigger.  The tests below verify the
    // structural change is in place by confirming that non-pure-hex high-entropy
    // tokens near triggers are caught (showing the trigger path is live), and that
    // purely hex tokens near triggers still correctly pass (entropy too low to flag).

    #[test]
    fn hex_near_key_blocked_in_credential_context() {
        // A pure-hex 32-char token near "api key" is a credential-shaped hex
        // token in trigger context.  Entropy alone cannot flag it (hex max =
        // 4.0 < 4.5 threshold), but the explicit hex-credential-token path
        // must catch it.
        let hex32 = "4f9c2e8a1d3b5c7e9f0a2b4d6e8c0a2b";
        assert_eq!(hex32.len(), 32);
        let line = format!("api key {hex32}");
        assert!(
            scan(&line).is_some(),
            "32-char pure hex near 'api key' must be blocked; got None"
        );
    }

    #[test]
    fn hex_credential_lengths_blocked_near_trigger() {
        // Verify all four credential-shaped lengths are caught near a trigger.
        let hex40 = "a3f5c2e9d1b8047e63a1f4c2d5b6e8f1a9c3d2e4";
        let hex64 = "1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b";
        let hex128 = format!("{hex64}{hex64}");
        assert_eq!(hex40.len(), 40);
        assert_eq!(hex64.len(), 64);
        assert_eq!(hex128.len(), 128);

        for (label, hex) in &[
            ("hex40", hex40),
            ("hex64", hex64),
            ("hex128", hex128.as_str()),
        ] {
            let line = format!("secret key: {hex}");
            assert!(
                scan(&line).is_some(),
                "{label} near 'secret key' must be blocked; got None"
            );
        }
    }

    #[test]
    fn hex_blocked_when_trigger_and_hash_word_coexist() {
        // Credential trigger dominates: adding "hash" or "sha" to the window does
        // not rescue a pure-hex token when a credential trigger is also present.
        // An attacker controlling the prose could otherwise bypass the gate with
        // one extra word, so the hash-word exception must NOT apply in trigger context.
        let hex32 = "4f9c2e8a1d3b5c7e9f0a2b4d6e8c0a2b";
        let key_hash_line = format!("api key hash {hex32}");
        let secret_sha_line = format!("secret sha {hex32}");
        assert!(
            scan(&key_hash_line).is_some(),
            "'api key hash <hex32>' must be blocked; got None"
        );
        assert!(
            scan(&secret_sha_line).is_some(),
            "'secret sha <hex32>' must be blocked; got None"
        );
    }

    #[test]
    fn hex_near_sha_context_word_allowed() {
        // A 40-char hex with "sha" or "commit" in the window — but no credential
        // trigger — must be allowed (git SHA or content hash in normal prose).
        let hex40 = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
        let sha_line = format!("sha1: {hex40}");
        let commit_line = format!("commit sha {hex40}");
        assert!(
            scan(&sha_line).is_none(),
            "hex40 near 'sha1' context must be allowed; fired: {:?}",
            scan(&sha_line)
        );
        assert!(
            scan(&commit_line).is_none(),
            "hex40 near 'commit sha' context must be allowed; fired: {:?}",
            scan(&commit_line)
        );
    }

    #[test]
    fn hex64_near_hash_context_allowed() {
        // A 64-char hex near "sha256" or "hash" — with no credential trigger —
        // must be allowed (content digest in normal prose).
        let hex64 = "1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b";
        let sha_line = format!("sha256: {hex64}");
        let hash_line = format!("hash value {hex64}");
        assert!(
            scan(&sha_line).is_none(),
            "hex64 near 'sha256' must be allowed; fired: {:?}",
            scan(&sha_line)
        );
        assert!(
            scan(&hash_line).is_none(),
            "hex64 near 'hash' must be allowed; fired: {:?}",
            scan(&hash_line)
        );
    }

    #[test]
    fn blocks_high_entropy_hex_like_token_near_key() {
        // A token whose character set exceeds pure hex (contains mixed-case, digits,
        // and non-hex chars) that ALSO passes `is_pure_hex = false` AND has high
        // entropy AND appears near "key" MUST be caught.  This is the realistic
        // real-world case: hex-looking API tokens often mix case and non-hex chars.
        // Example: a 32-char mixed-charset token near "api key".
        let mixed = "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM"; // gitleaks:allow — not pure hex
        assert!(!is_pure_hex(mixed), "test token must not be pure hex");
        let line = format!("api key {mixed}");
        assert!(
            scan(&line).is_some(),
            "mixed-charset high-entropy token near 'api key' must be caught; got: {:?}",
            scan(&line)
        );
    }

    #[test]
    fn allows_hex40_without_trigger() {
        // 40-char hex string in a neutral context (no trigger word) must still pass —
        // it's likely a git commit SHA or content hash.
        let hex40 = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
        let line = format!("commit: {hex40}");
        assert!(
            scan(&line).is_none(),
            "40-char hex without trigger word must pass; fired: {:?}",
            scan(&line)
        );
    }

    // ── Finding 4: check_json scans object keys ───────────────────────────────

    #[test]
    fn check_json_blocks_secret_in_object_key() {
        // A credential used as a JSON object key (not a value) must be caught.
        let props = serde_json::json!({ "ghp_FakeGitHubToken0000000000000000000": "redacted" }); // gitleaks:allow
        assert!(
            check_json(&props).is_err(),
            "credential as JSON object key must be blocked"
        );
    }

    #[test]
    fn check_json_blocks_nested_secret_key() {
        // Nested credential key must be caught.
        let props = serde_json::json!({
            "metadata": {
                "AKIAFAKEKEY000000000": "value" // gitleaks:allow
            }
        });
        assert!(
            check_json(&props).is_err(),
            "nested credential as JSON object key must be blocked"
        );
    }

    // ── PEM masking format ───────────────────────────────────────────────────

    #[test]
    fn pem_masked_excerpt_reflects_block_length_not_rest_of_string() {
        let header = ["-----BEGIN RSA", " PRIVATE KEY-----"].concat(); // gitleaks:allow
        let fake = format!(
            "{}\nMIIEo\u{2026}\n-----END RSA PRIVATE KEY-----\nsome trailing text that is very long",
            header
        );
        let m = scan(&fake).unwrap();
        assert_eq!(m.detector, "pem-private-key");
        // The masked length should reflect only the key block, not the whole string.
        // "some trailing text that is very long" is ~37 chars; total string is much longer.
        // The block ends after "-----END RSA PRIVATE KEY-----\n".
        // We just verify it is shorter than the full string length.
        let full_len = fake.chars().count();
        let reported_len: usize = m
            .masked
            .trim_end_matches("chars")
            .rsplit("...")
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(full_len + 1);
        assert!(
            reported_len < full_len,
            "masked length ({reported_len}) should be less than full string length ({full_len})"
        );
    }
}
