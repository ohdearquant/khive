use super::{
    contains_bounded_word, contains_word, SecretMatch, COMPOUND_TRIGGER_WORDS, TRIGGER_WORDS,
};

pub(super) fn is_assignment_label_gap(c: char) -> bool {
    matches!(c, '"' | '\'' | '`')
}

/// Closed vocabulary of member names whose `_key` suffix names a lookup
/// identifier rather than a credential (issue #2654). The exception is an
/// allowlist on purpose: any other `*_key` label keeps `key` as a credential
/// trigger, so unknown compounds such as `hmac_key`, `master_key`, `ssh_key`,
/// `jwt_key`, `webhook_key` or `license_key` stay refused. The match is the
/// whole label: a qualified spelling such as `left_association_key` or
/// `hmac_cache_key` is not in the vocabulary and keeps its trigger, because a
/// prefix rule would re-open every compound the list closes (`hmac_` is not a
/// trigger word, so `hmac_cache_key` would strip to a listed suffix).
pub(super) const LOOKUP_KEY_LABELS: &[&str] = &[
    "association_key",
    "cache_key",
    "composite_key",
    "dedup_key",
    "foreign_key",
    "idempotency_key",
    "index_key",
    "lookup_key",
    "map_key",
    "natural_key",
    "partition_key",
    "primary_key",
    "range_key",
    "routing_key",
    "row_key",
    "search_key",
    "shard_key",
    "sort_key",
    "surrogate_key",
    "unique_key",
];

pub(super) fn is_lookup_key_label(label: &str) -> bool {
    LOOKUP_KEY_LABELS.contains(&label)
}

/// Finds a canonical compound credential label beginning at an identifier
/// boundary. The trailing edge is deliberately unbounded so version suffixes
/// and larger underscore-composed labels remain protected.
fn compound_trigger(low_text: &str) -> Option<&'static str> {
    COMPOUND_TRIGGER_WORDS.iter().copied().find(|needle| {
        let mut start = 0;
        while let Some(rel) = low_text[start..].find(needle) {
            let abs = start + rel;
            let before_ok = abs == 0
                || low_text[..abs]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !c.is_ascii_alphanumeric());
            if before_ok {
                return true;
            }
            start = abs + needle.len();
        }
        false
    })
}

pub(super) fn find_trigger(text: &str, credential_label_only: bool) -> Option<&'static str> {
    let low = text.to_ascii_lowercase();
    TRIGGER_WORDS
        .iter()
        .copied()
        .find(|tw| contains_bounded_word(&low, tw))
        .or_else(|| compound_trigger(&low))
        .or_else(|| {
            ((!credential_label_only && has_standalone_token(&low)) || has_token_assignment(&low))
                .then_some("token")
        })
        .or_else(|| assignment_credential_trigger(&low))
}

/// Detect a credential-bearing assignment label before an `=` or `:`.
///
/// The separator may be preceded by whitespace or a JSON quote. Compound
/// triggers deliberately retain substring matching inside the label so common
/// version suffixes such as `api_keyv2` remain protected.
pub(super) fn assignment_credential_trigger(low_text: &str) -> Option<&'static str> {
    low_text.char_indices().find_map(|(index, ch)| {
        if !matches!(ch, '=' | ':') {
            return None;
        }
        let before =
            low_text[..index].trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
        let label = before
            .rsplit(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .next()
            .unwrap_or_default();
        COMPOUND_TRIGGER_WORDS
            .iter()
            .copied()
            .find(|needle| label.contains(needle))
            .or_else(|| {
                TRIGGER_WORDS.iter().copied().find(|tw| {
                    (*tw != "key"
                        || !is_lookup_key_label(label)
                        || !low_text[before.len()..index]
                            .chars()
                            .all(is_assignment_label_gap))
                        && contains_bounded_word(label, tw)
                })
            })
            .or_else(|| (label == "token").then_some("token"))
    })
}

/// Detect credential labels embedded in the same whitespace token as a value.
///
/// The surrounding-context scan deliberately excludes the candidate token so
/// a trigger word inside a path cannot make that path self-trigger. Credential
/// assignments still need to fire when no whitespace separates label and
/// value, including JSON-like forms. Underscore-delimited config identifiers
/// without an assignment are retained for compatibility with shapes such as
/// `session_secret_<value>`.
pub(super) fn inline_credential_trigger(raw_token: &str) -> Option<&'static str> {
    let low = raw_token.to_ascii_lowercase();
    assignment_credential_trigger(&low).or_else(|| {
        if !low.contains(['/', '-', '.']) && low.contains('_') {
            COMPOUND_TRIGGER_WORDS
                .iter()
                .copied()
                .find(|needle| low.contains(needle))
                .or_else(|| {
                    TRIGGER_WORDS
                        .iter()
                        .copied()
                        .find(|tw| contains_bounded_word(&low, tw))
                })
        } else {
            None
        }
    })
}

/// Returns `true` when `low_window` contains the word `token` as a standalone
/// word, with underscore treated as a WORD CHARACTER / continuation (see
/// [`contains_word`]) — but NOT as part of compound identifiers such as
/// `tokenizer`, `token_count`, or `next_token`. This underscore-as-
/// continuation rule is deliberately different from
/// [`contains_bounded_word`]: `token` alone is not a
/// credential trigger (it fires on too many benign technical terms), so it
/// needs the narrower, underscore-inclusive standalone-word definition,
/// whereas the bare `TRIGGER_WORDS` need underscore-joined compounds like
/// `secret_key` to still register.
fn has_standalone_token(low_window: &str) -> bool {
    contains_word(low_window, "token", true)
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
                .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
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
pub(super) fn is_pure_hex(token: &str) -> bool {
    let hex_part = token
        .strip_prefix("0x")
        .or(token.strip_prefix("0X"))
        .unwrap_or(token);
    hex_part.len() >= 8 && hex_part.len() <= 128 && hex_part.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Returns `true` for tokens that are unambiguous base64/base64url content
/// hashes with an explicit `sha<N>-` prefix (SRI hash, npm lockfile integrity).
/// Bare base64 of the same length WITHOUT the prefix is NOT allowlisted — see
/// `docs/api/secret_gate.md#is_base64_content_hash` for the full criteria list and
/// why the explicit prefix is required.
pub(super) fn is_base64_content_hash(token: &str) -> bool {
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
        "ghu_",
        "ghs_",
        "ghr_",
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

/// Structural separators that gate entry into [`is_structured_identifier`]
/// (rule 1: the token must contain at least one of these). The actual run
/// decomposition (rule 2) splits on every non-alphanumeric character, not
/// just these four — see the doc comment on `is_structured_identifier`.
const STRUCTURAL_SEPARATORS: [char; 4] = ['/', '-', '_', '.'];

/// Largest length a single path/branch/identifier segment (a "run" between
/// separators) may have and still be considered word-shaped.
const MAX_RUN_LEN: usize = 24;

/// Runs whose letter portion is at or below this length skip the
/// case-transition-density check: density is not a meaningful signal on very
/// short runs (e.g. `R1`, `v2`, `ADR`).
const DENSITY_EXEMPT_LETTER_LEN: usize = 4;

/// Maximum case-transition density (transitions divided by letter_count - 1)
/// a run's letter portion may have and still be considered word-shaped.
const MAX_CASE_TRANSITION_DENSITY: f64 = 0.3;

/// Returns `true` when `token` is shaped like a file path, branch name, or
/// other structured identifier rather than a high-entropy secret (word-shaped
/// runs separated by `/`, `-`, `_`, `.`). Exempts from the entropy heuristic
/// ONLY outside trigger context — see the module doc and
/// `docs/api/secret_gate.md#is_structured_identifier` for the run-shape criteria.
pub(super) fn is_structured_identifier(token: &str) -> bool {
    if !token.contains(|c: char| STRUCTURAL_SEPARATORS.contains(&c)) {
        return false;
    }
    let runs: Vec<&str> = token
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|r| !r.is_empty())
        .collect();
    runs.len() >= 2 && runs.iter().all(|run| is_word_shaped_run(run))
}

/// A single run (segment between structural separators) is word-shaped when
/// it matches `[A-Za-z]+[0-9]*` or `[0-9]+`, is at most [`MAX_RUN_LEN`] chars,
/// and (for the letters-then-digits form) its letter portion has a low
/// case-transition density.
fn is_word_shaped_run(run: &str) -> bool {
    if run.is_empty() || run.len() > MAX_RUN_LEN {
        return false;
    }
    let bytes = run.as_bytes();
    if bytes.iter().all(|b| b.is_ascii_digit()) {
        return true;
    }
    let letter_end = bytes
        .iter()
        .position(|b| !b.is_ascii_alphabetic())
        .unwrap_or(bytes.len());
    // A run that does not start with a letter, and is not pure digits (ruled
    // out above), mixes digits and letters in a shape other than
    // letters-then-digits — not word-shaped.
    if letter_end == 0 {
        return false;
    }
    // Everything after the leading letters must be digits only (no further
    // letters), else the run is not the `[A-Za-z]+[0-9]*` shape.
    if !bytes[letter_end..].iter().all(|b| b.is_ascii_digit()) {
        return false;
    }
    case_transition_density_ok(&run[..letter_end])
}

/// `true` when the case-transition density of `letters` (an all-ASCII-letter
/// string) is at or below [`MAX_CASE_TRANSITION_DENSITY`]. A transition is an
/// adjacent letter pair where one side is uppercase and the other is not.
/// Runs with few enough letters pass automatically (see
/// [`DENSITY_EXEMPT_LETTER_LEN`]) since density is noisy on short strings.
fn case_transition_density_ok(letters: &str) -> bool {
    let chars: Vec<char> = letters.chars().collect();
    if chars.len() <= DENSITY_EXEMPT_LETTER_LEN {
        return true;
    }
    let transitions = chars
        .windows(2)
        .filter(|w| w[0].is_ascii_uppercase() != w[1].is_ascii_uppercase())
        .count();
    let density = transitions as f64 / (chars.len() - 1) as f64;
    density <= MAX_CASE_TRANSITION_DENSITY
}

/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`
pub(super) fn is_uuid_canonical(s: &str) -> bool {
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
pub(super) fn strip_delimiters(s: &str) -> &str {
    s.trim_matches(|c| matches!(c, '"' | '\'' | '`' | ':' | '=' | ',' | ';'))
}

/// Strip `{}()[]"'.,;` from both ends of `s`, repeatedly (JSON nests one
/// wrapper inside another).
fn strip_wrappers(s: &str) -> &str {
    s.trim_matches(|c: char| {
        matches!(
            c,
            '{' | '}' | '(' | ')' | '[' | ']' | '"' | '\'' | '`' | '.' | ',' | ';'
        )
    })
}

pub(super) fn wrapper_strip_repeated(token: &str) -> &str {
    let mut cur = token;
    loop {
        let next = strip_wrappers(cur);
        if next == cur {
            return cur;
        }
        cur = next;
    }
}

/// Yields every candidate value an assignment/wrapper-glued token could
/// contain, for the near-trigger UUID/content-hash exact-shape checks only.
/// See `docs/api/secret_gate.md#value_candidates` for why every `=`/`:` suffix
/// must be tried rather than just the first or last.
pub(super) fn value_candidates(token: &str) -> impl Iterator<Item = &str> {
    let cur = wrapper_strip_repeated(token);
    std::iter::once(cur).chain(cur.char_indices().filter_map(move |(i, c)| {
        if c == '=' || c == ':' {
            let after = strip_wrappers(&cur[i + c.len_utf8()..]);
            if !after.is_empty() {
                return Some(after);
            }
        }
        None
    }))
}

// ─── Utilities ───────────────────────────────────────────────────────────────

/// Extract a contiguous token (non-whitespace chars) starting at the beginning of `s`.
pub(super) fn extract_token(s: &str) -> &str {
    let end = s
        .find(|c: char| c.is_whitespace() || c == '\n' || c == '\r')
        .unwrap_or(s.len());
    &s[..end]
}

/// Shannon entropy in bits per character.
///
/// H = -∑ p_i log2(p_i)
pub(super) fn shannon_entropy(bytes: &[u8]) -> f64 {
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
pub(super) fn build_match(detector: &'static str, candidate: &str) -> SecretMatch {
    let chars: Vec<char> = candidate.chars().collect();
    let preview: String = chars.iter().take(6).collect();
    let masked = format!("{}...{}chars", preview, chars.len());
    SecretMatch {
        detector,
        trigger: None,
        masked,
        location: None,
    }
}
