//! Write-time secret detection gate (issue #76).
//!
//! Scans caller-supplied content strings before any storage write.  A match
//! causes a hard `RuntimeError::SecretDetected` that names the detector and
//! carries a masked excerpt — it never echoes the full candidate back.
//!
//! Detection is layered, cheap-first:
//!
//! 1. **Known-prefix / known-shape patterns** — AWS AKIA/ASIA, GitHub tokens,
//!    OpenAI `sk-`, Anthropic `sk-ant-`, Stripe live keys, Fly.io tokens,
//!    Vercel secrets, Slack `xox*`, JWT triples, PEM private-key headers,
//!    Age secret keys, URL userinfo (`scheme://user:pass@`).
//! 2. **High-entropy token heuristic** — base64/hex runs ≥ 24 chars near
//!    a trigger word (key, token, secret, password, credential, bearer).
//!
//! Allowlist (false-positive suppression):
//! - Pure hex strings (sha256, git SHA, UUID-hex) — passed unconditionally.
//! - UUID canonical form (`xxxxxxxx-xxxx-…`) — passed.
//! - Strings that are entirely ASCII punctuation/whitespace (e.g. code) — not
//!   subject to the entropy heuristic, only the literal-prefix checks apply.

use crate::error::{RuntimeError, RuntimeResult};

// ─── Error variant ───────────────────────────────────────────────────────────

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
    // OpenAI bare (after more-specific)
    ("openai-api-key", "sk-", 30),
    // Age secret key
    ("age-secret-key", "AGE-SECRET-KEY-", 60),
];

/// Shape-based patterns checked with custom logic.
fn check_known_patterns(text: &str) -> Option<SecretMatch> {
    // --- Prefix patterns ---
    for &(name, needle, min_len) in PREFIX_DETECTORS {
        if let Some(m) = find_prefix_token(text, needle, min_len) {
            return Some(build_match(name, m));
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
        // Find the start of the header to build a masked excerpt.
        if let Some(pos) = text.find("-----BEGIN") {
            let excerpt = &text[pos..];
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
const TRIGGER_WORDS: &[&str] = &[
    "key",
    "token",
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

fn check_entropy_heuristic(text: &str) -> Option<SecretMatch> {
    // Tokenize once: collect all whitespace-delimited tokens with their byte offsets.
    let tokens: Vec<(usize, &str)> = text
        .split_ascii_whitespace()
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
        // Allowlist: skip pure hex (sha256, git SHA), UUID canonical form, and
        // things that look like base64 content hashes (all hex or UUID-shaped).
        if is_allowlisted(token) {
            continue;
        }
        let entropy = shannon_entropy(token.as_bytes());
        if entropy < ENTROPY_THRESHOLD {
            continue;
        }
        // High-entropy token found — check whether a trigger word appears within
        // TRIGGER_WINDOW bytes of this token (before or after).
        let window_start = tok_offset.saturating_sub(TRIGGER_WINDOW);
        let window_end = (tok_offset + raw_token.len() + TRIGGER_WINDOW).min(text.len());
        let window = &text[window_start..window_end];
        let low_window = window.to_ascii_lowercase();
        if TRIGGER_WORDS.iter().any(|tw| low_window.contains(tw)) {
            return Some(build_match("high-entropy-token", token));
        }
    }
    None
}

// ─── Allowlist helpers ───────────────────────────────────────────────────────

/// Returns `true` for tokens that should NOT trigger the entropy heuristic.
///
/// Allowlisted forms:
/// - Pure lowercase hex of any length ≥ 8 (git SHA, sha256 digest, uuid-hex).
/// - UUID canonical form `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`.
/// - Strings shorter than MIN_ENTROPY_LEN after delimiter stripping.
fn is_allowlisted(token: &str) -> bool {
    // UUID canonical form.
    if is_uuid_canonical(token) {
        return true;
    }
    // Pure hex (lowercase or uppercase, possibly with 0x prefix).
    let hex_part = token
        .strip_prefix("0x")
        .or(token.strip_prefix("0X"))
        .unwrap_or(token);
    if hex_part.len() >= 8 && hex_part.bytes().all(|b| b.is_ascii_hexdigit()) {
        return true;
    }
    false
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
    fn blocks_high_entropy_near_token_word() {
        // 32 random-looking base64 chars adjacent to the word "token".
        let fake = "Bearer token: Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM"; // gitleaks:allow
        assert!(
            scan(fake).is_some(),
            "high-entropy token near 'token' must be caught"
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
    fn allowlist_passes_sha256() {
        let sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert!(is_allowlisted(sha));
    }

    #[test]
    fn allowlist_passes_uuid_canonical() {
        assert!(is_allowlisted("550e8400-e29b-41d4-a716-446655440000"));
    }

    #[test]
    fn allowlist_does_not_pass_mixed_token() {
        // A token that starts with letters but mixes in non-hex chars.
        assert!(!is_allowlisted("sk-aaaaaabbbbbbccccccddddddeeeeeeffffgg"));
    }
}
