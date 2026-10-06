#[cfg(doc)]
use super::is_pure_hex;
use super::{
    contains_bounded_word, extract_token, shannon_entropy, strip_delimiters,
    wrapper_strip_repeated, COMPOUND_TRIGGER_WORDS, ENTROPY_THRESHOLD, HEX_CREDENTIAL_LENGTHS,
    MIN_ENTROPY_LEN, TRIGGER_WORDS,
};

/// `true` when every byte of `run` is an ASCII hex digit. The
/// no-minimum-length, no-`0x`-prefix building block for
/// [`normalized_hex_credential_span`]'s intra-token run decomposition.
/// Unlike [`is_pure_hex`] this has no 8-char floor of its own — a legitimate
/// credential split across separators can leave a shorter individual run
/// that still must sum correctly with its neighbors.
fn is_hex_run(run: &str) -> bool {
    !run.is_empty() && run.bytes().all(|b| b.is_ascii_hexdigit())
}

const VCS_MARKERS: &[&str] = &["commit", "revision", "rev", "sha"];

/// Marker-word form of a VCS reference: `raw_token` is itself a bare marker
/// and the next token in `text` is a 40-hex value.
pub(super) fn is_vcs_marker_before_hex(text: &str, raw_token: &str) -> bool {
    let token = wrapper_strip_repeated(raw_token);
    let marker = strip_delimiters(token);
    if !VCS_MARKERS
        .iter()
        .any(|candidate| marker.eq_ignore_ascii_case(candidate))
    {
        return false;
    }
    let raw_offset = raw_token.as_ptr() as usize - text.as_ptr() as usize;
    let next = text[raw_offset + raw_token.len()..].trim_start();
    let next = wrapper_strip_repeated(extract_token(next));
    next.len() == 40 && next.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(super) fn is_git_revision_reference(text: &str, token_offset: usize, raw_token: &str) -> bool {
    const MARKERS: &[&str] = VCS_MARKERS;

    let token = wrapper_strip_repeated(raw_token);
    if is_vcs_marker_before_hex(text, raw_token) {
        return true;
    }

    if token.len() == 40 && token.bytes().all(|b| b.is_ascii_hexdigit()) {
        let marker = trailing_identifier(&text[..token_offset]);
        return MARKERS
            .iter()
            .any(|candidate| marker.eq_ignore_ascii_case(candidate));
    }

    let Some((label, value)) = token.rsplit_once(':') else {
        return false;
    };
    let label = wrapper_strip_repeated(label);
    let value = wrapper_strip_repeated(value);
    MARKERS
        .iter()
        .any(|candidate| label.eq_ignore_ascii_case(candidate))
        && value.len() == 40
        && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_exact_hex_revision(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Repository-native revision coordinates embedded in a URL/HTML token.
///
/// This intentionally recognizes only exact 40-hex revisions after a
/// canonical repository path marker, plus an exact `href=<40hex>` target.
/// Arbitrary query values, prefixes, and credential-length hex elsewhere in
/// markup remain subject to the normal detector.
pub(super) fn is_repository_revision_reference(raw_token: &str) -> bool {
    let token = wrapper_strip_repeated(raw_token);
    let low = token.to_ascii_lowercase();

    for marker in ["/blob/", "/tree/", "/commit/", "/commits/"] {
        let mut from = 0usize;
        while let Some(relative) = low[from..].find(marker) {
            let value_start = from + relative + marker.len();
            let remainder = &token[value_start..];
            let value_end = remainder
                .bytes()
                .position(|byte| !byte.is_ascii_hexdigit())
                .unwrap_or(remainder.len());
            if is_exact_hex_revision(&remainder[..value_end]) {
                return true;
            }
            from = value_start.min(low.len());
            if from == low.len() {
                break;
            }
        }
    }

    for (prefix, quote) in [("href=\"", '"'), ("href='", '\'')] {
        if let Some(start) = low.find(prefix) {
            let value = &token[start + prefix.len()..];
            if let Some(end) = value.find(quote) {
                if is_exact_hex_revision(&value[..end]) {
                    return true;
                }
            }
        }
    }

    false
}

fn trailing_identifier(text: &str) -> &str {
    let trimmed = text.trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
    trimmed
        .rsplit(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .next()
        .unwrap_or_default()
}

/// Return whether the identifier immediately preceding a candidate is a
/// credential label. This deliberately does not walk through narrative prose:
/// notation such as `key estimate uses \\operatorname{softmax}` contains a
/// trigger word, but does not assign the LaTeX fragment to that word.
pub(super) fn has_immediate_credential_label(text: &str, token_offset: usize) -> bool {
    let before = text[..token_offset].trim_end();
    let before = before
        .strip_suffix(':')
        .or_else(|| before.strip_suffix('='))
        .unwrap_or(before);
    let label = trailing_identifier(before).to_ascii_lowercase();

    label == "token"
        || COMPOUND_TRIGGER_WORDS
            .iter()
            .any(|trigger| label.contains(trigger))
        || TRIGGER_WORDS
            .iter()
            .any(|trigger| contains_bounded_word(&label, trigger))
}

/// A repository revision URL/href is exempt from broad trigger proximity,
/// but never from a direct credential label. When a value separator is
/// present immediately before the reference, only its actual field label is
/// authoritative: narrative shapes such as `key-scoped source citation:`
/// must not turn the earlier adjective into the citation value's label.
pub(super) fn has_direct_repository_credential_label_with_inline(
    text: &str,
    token_offset: usize,
    inline_trigger: bool,
) -> bool {
    if inline_trigger {
        return true;
    }
    let before = text[..token_offset].trim_end();
    if before.ends_with(':') || before.ends_with('=') {
        return has_immediate_credential_label(text, token_offset);
    }
    has_clause_credential_label_with_inline(
        text,
        token_offset,
        false,
        ClauseValueKind::VcsReference,
    )
}

/// Words the clause walk in [`has_clause_credential_label_with_inline`] steps over when
/// searching backwards for a credential label. Connectors are the words that
/// commonly sit between a label and its value in natural assignment prose
/// ("api key value is X", "the token was X"); the VCS coordinate markers are
/// included so the marker itself cannot shield an earlier label from the
/// walk ("api key value is commit <hex>"); prepositions, determiners, and
/// possessives are the glue of noun-compound label qualifiers ("api key for
/// our production deploy: X") and carry no content of their own.
const LABEL_CLAUSE_SKIP_WORDS: &[&str] = &[
    "commit",
    "revision",
    "rev",
    "sha",
    "is",
    "was",
    "are",
    "were",
    "be",
    "been",
    "being",
    "value",
    "values",
    "the",
    "a",
    "an",
    "this",
    "that",
    "it",
    "its",
    "as",
    "here",
    "now",
    "currently",
    "equals",
    "for",
    "of",
    "to",
    "in",
    "on",
    "at",
    "by",
    "with",
    "from",
    "per",
    "and",
    "or",
    "our",
    "my",
    "your",
    "their",
];

/// Maximum identifiers the clause walk examines. Bounds the scan cost to one
/// short assignment clause. Exhausting the budget is NOT evidence of absence:
/// when a value delimiter was crossed, running out of steps fails CLOSED
/// (treated as credential-labeled) — a truncated scan cannot prove the clause
/// is unlabeled, and any exhaustion-fails-open rule re-admits the labeled
/// value bypass one natural word past the budget. Sized so a label separated
/// from its value by connectors plus a dotted version qualifier ("api key
/// v1.2 value is commit <hex>" — the version costs two identifier steps)
/// stays in range without exhaustion.
const LABEL_CLAUSE_WALK_LIMIT: usize = 8;

/// File-path candidates may walk across at most this many content identifiers
/// without a `:`/`=` delimiter. Two reaches the direct-label shapes that need
/// protection (`auth scanner found <path>`) while keeping the walk local.
const FILE_PATH_NO_DELIMITER_CONTENT_LIMIT: usize = 2;

/// The two narrow trigger-context exemptions need different no-delimiter
/// behavior. VCS coordinates preserve the strict prose guard that keeps
/// `the key changes are in commit <sha>` readable; file paths admit the
/// bounded bridge above so a short label cannot disguise a credential value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ClauseValueKind {
    VcsReference,
    FilePath,
}

/// End offset of a sentence/paragraph boundary beginning at `index`.
///
/// A single newline remains intra-sentence so natural credential assignments
/// such as `api key:\n<value>` stay protected. Blank lines, clause-ending
/// punctuation, and a period followed by a non-alphanumeric byte end the
/// surrounding trigger context. The period rule deliberately keeps `v1.2`
/// intra-sentence.
fn sentence_boundary_end_at(bytes: &[u8], index: usize) -> Option<usize> {
    match bytes.get(index).copied()? {
        b';' | b'!' | b'?' => Some(index + 1),
        b'.' if bytes
            .get(index + 1)
            .is_some_and(|next| !next.is_ascii_alphanumeric()) =>
        {
            Some(index + 1)
        }
        b'\n' if bytes.get(index + 1) == Some(&b'\n') => Some(index + 2),
        b'\r'
            if bytes
                .get(index..index + 4)
                .is_some_and(|window| window == b"\r\n\r\n") =>
        {
            Some(index + 4)
        }
        _ => None,
    }
}

/// Context after the final sentence boundary in `text`.
pub(super) fn after_last_sentence_boundary(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut start = 0usize;
    for index in 0..bytes.len() {
        if let Some(end) = sentence_boundary_end_at(bytes, index) {
            start = end;
        }
    }
    &text[start..]
}

/// Context before the first sentence boundary in `text`.
pub(super) fn before_first_sentence_boundary(text: &str) -> &str {
    let bytes = text.as_bytes();
    for index in 0..bytes.len() {
        if sentence_boundary_end_at(bytes, index).is_some() {
            return &text[..index];
        }
    }
    text
}

/// Sentence/paragraph boundary inside a clause-walk gap. `;`, `!`, `?`, and
/// blank lines always end the clause. `.` ends it only when it is not
/// immediately followed by an alphanumeric character: a dot tight between
/// identifier fragments ("v1.2") is intra-token punctuation, while a dot at
/// the end of the gap abuts the next identifier (gaps end where the adjacent
/// identifier begins) and is likewise intra-token.
fn gap_has_sentence_boundary(gap: &str) -> bool {
    let bytes = gap.as_bytes();
    (0..bytes.len()).any(|index| sentence_boundary_end_at(bytes, index).is_some())
}

/// Version-shaped identifier fragment ("2", "v1", "12") — the pieces a dotted
/// version qualifier like `v1.2` splits into under identifier extraction.
/// Treated as connector material so a versioned label ("api key v1.2 value
/// is …") stays reachable.
fn is_version_fragment(word: &str) -> bool {
    let digits = word
        .strip_prefix('v')
        .or_else(|| word.strip_prefix('V'))
        .unwrap_or(word);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Hex-run identifier long enough to be credential material rather than a
/// word ("0123456789abcdef01234567"). Treated as connector material by the
/// clause walk: a separator-split payload fragment sitting between the
/// candidate value and its label is value material, not a label word that
/// ends the clause.
fn is_hex_fragment_word(word: &str) -> bool {
    word.len() >= 12 && word.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Allocation-free case-insensitive substring search for ASCII identifiers.
fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

/// A trailing identifier can contain only ASCII alphanumerics and `_`, so
/// splitting on `_` exactly preserves the bare-trigger boundary rule used by
/// [`contains_bounded_word`] without allocating a lowercase copy.
fn clause_label_has_credential_trigger(label: &str) -> bool {
    label.eq_ignore_ascii_case("token")
        || COMPOUND_TRIGGER_WORDS
            .iter()
            .any(|trigger| contains_ascii_case_insensitive(label, trigger))
        || label.split('_').any(|part| {
            TRIGGER_WORDS
                .iter()
                .any(|trigger| part.eq_ignore_ascii_case(trigger))
        })
}

fn is_label_clause_skip_word(label: &str) -> bool {
    LABEL_CLAUSE_SKIP_WORDS
        .iter()
        .any(|word| label.eq_ignore_ascii_case(word))
}

/// Words ending in the byte sequence `ed` are only a cheap proxy for a
/// regular English participle. Keep known lexical false matches explicit so
/// a noun such as `hundred` cannot become evidence that a credential label is
/// merely narrative prose. Irregular forms such as `found` deliberately do
/// not qualify: they must not shield `api key found <value>`.
pub(super) fn is_clause_narrative_participle(label: &str) -> bool {
    const NON_PARTICIPLE_ED_WORDS: &[&str] = &["hundred"];

    label.len() >= 5
        && label
            .get(label.len().saturating_sub(2)..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case("ed"))
        && !NON_PARTICIPLE_ED_WORDS
            .iter()
            .any(|word| label.eq_ignore_ascii_case(word))
}

/// Cheap regular-gerund proxy used only after the no-delimiter file-path walk
/// has crossed an explicit `in` on the value side (`api_key handling in
/// <path>`). Adjacency alone is never narrative evidence: `api key handling
/// <value>` must keep walking to the direct credential label.
pub(super) fn is_clause_narrative_gerund(label: &str) -> bool {
    label.len() >= 6
        && label
            .get(label.len().saturating_sub(3)..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case("ing"))
}

/// `true` when the candidate token sits in credential-value syntax: an inline
/// credential shape on the token itself, or a credential label reachable by
/// walking backwards through the current clause. The walk steps over
/// [`LABEL_CLAUSE_SKIP_WORDS`], version fragments, and long hex fragments,
/// and stops at a sentence/paragraph boundary (see
/// [`gap_has_sentence_boundary`]) — a label on the far side of a boundary is
/// prose context, not this value's label. Crossing a value delimiter (`:` or
/// `=`, including one attached to a VCS marker: "deploy sha: <hex>" is still
/// assignment syntax) additionally lets the walk step over content words
/// outside those sets, bounded only by [`LABEL_CLAUSE_WALK_LIMIT`], the
/// sentence boundary, and the past-participle stop: "label with qualifiers:
/// value" names the value regardless of how many qualifier nouns the label
/// carries ("api key for production deploy: X", "api key for shared
/// encrypted deploy: X"). A per-clause content-word cap was tried here and
/// removed — any cap re-admits the labeled-value bypass one natural
/// qualifier past the cap. For the same reason, exhausting the walk budget
/// after crossing a delimiter fails CLOSED: the clause is assignment-shaped
/// and its head was never scanned, so it is treated as credential-labeled.
/// A regular past-participle content word ("flagged", "introduced") ends the walk —
/// verb-phrase prose narrates an action on the value rather than labeling it
/// ("the auth scanner flagged this file: <path>", "one extra token was
/// introduced by sha: <hex>"). Coordinating conjunctions are transparent to
/// that position test — "shared and encrypted deploy" keeps both participles
/// in adjective position. Without a delimiter, VCS references step over only
/// the closed connector sets, preserving ordinary prose such as "the key
/// changes are in commit <hex>". File paths additionally step over at most
/// [`FILE_PATH_NO_DELIMITER_CONTENT_LIMIT`] content words, closing direct
/// label shapes such as "auth scanner found <path>" without broadening the
/// VCS tier.
pub(super) fn has_clause_credential_label_with_inline(
    text: &str,
    token_offset: usize,
    inline_trigger: bool,
    value_kind: ClauseValueKind,
) -> bool {
    if inline_trigger {
        return true;
    }

    let mut rest = &text[..token_offset];
    let mut crossed_value_delimiter = false;
    let mut no_delimiter_content_words = 0usize;
    // Whether the previously processed identifier (the one nearer the value)
    // was the literal preposition `in`. This starts false deliberately:
    // adjacency to the value must not make a gerund narrative evidence.
    let mut arrived_through_in_preposition = false;
    // Whether the previously processed identifier (the one nearer the value)
    // was connector material. Starts true: step 0 is adjacent to the value or
    // its delimiter.
    let mut arrived_through_connector = true;
    for step in 0..LABEL_CLAUSE_WALK_LIMIT {
        let label = trailing_identifier(rest);
        if label.is_empty() {
            return false;
        }
        let gap = &rest[label.as_ptr() as usize - rest.as_ptr() as usize + label.len()..];
        if step > 0 && gap_has_sentence_boundary(gap) {
            return false;
        }
        if gap.contains([':', '=']) {
            crossed_value_delimiter = true;
        }
        if clause_label_has_credential_trigger(label) {
            return true;
        }
        let skippable = is_label_clause_skip_word(label)
            || is_version_fragment(label)
            || is_hex_fragment_word(label);
        if !skippable {
            // This barrier applies to delimiter-bearing clauses and to the
            // bounded file-path bridge. It intentionally recognizes regular
            // `-ed` forms only: `found` is a bridge word, not a narrative
            // shield for a direct credential label.
            let no_delimiter_file_path_narrative = !crossed_value_delimiter
                && value_kind == ClauseValueKind::FilePath
                && arrived_through_in_preposition
                && is_clause_narrative_gerund(label);
            // Delimiter-bearing clauses and VCS coordinates preserve their
            // existing direct-participle semantics. A no-delimiter file path
            // must first process real value-side context: the synthetic
            // initial connector state alone cannot let `api key leaked
            // <value>` stop before reaching the trigger.
            let regular_participle_narrative = is_clause_narrative_participle(label)
                && (crossed_value_delimiter || value_kind != ClauseValueKind::FilePath || step > 0);
            if arrived_through_connector
                && (regular_participle_narrative || no_delimiter_file_path_narrative)
            {
                return false;
            }
            if !crossed_value_delimiter {
                if value_kind != ClauseValueKind::FilePath
                    || no_delimiter_content_words >= FILE_PATH_NO_DELIMITER_CONTENT_LIMIT
                {
                    return false;
                }
                no_delimiter_content_words += 1;
            }
        }
        // Coordinating conjunctions are transparent to participle-position
        // classification: in "shared and encrypted deploy" the coordination
        // as a whole is followed by a content noun, so "shared" is still an
        // adjective — the conjunction preserves the arrived state instead of
        // marking connector position.
        if !label.eq_ignore_ascii_case("and") && !label.eq_ignore_ascii_case("or") {
            arrived_through_connector = skippable;
        }
        arrived_through_in_preposition = label.eq_ignore_ascii_case("in");
        let start = label.as_ptr() as usize - rest.as_ptr() as usize;
        rest = &rest[..start];
    }
    // Budget exhausted. A clause that crossed a value delimiter and ran out
    // of steps without a sentence boundary or verb-position participle is
    // assignment-shaped with an unscanned head — fail closed rather than let
    // clause length launder a labeled credential into the exemptions.
    crossed_value_delimiter
}

pub(super) fn is_plausible_file_path(token: &str) -> bool {
    let token = token.trim_start_matches(|c: char| {
        matches!(
            c,
            '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'
        )
    });
    let token = token.trim_end_matches(|c: char| {
        matches!(
            c,
            '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '.'
        )
    });
    let path = if let Some(angle_path) = token.strip_prefix('<') {
        let Some(close) = angle_path.rfind('>') else {
            return false;
        };
        if !is_line_location_suffix(&angle_path[close + 1..]) {
            return false;
        }
        &angle_path[..close]
    } else if let Some((path, suffix)) = token.rsplit_once(':') {
        if is_line_location_suffix(suffix) {
            path
        } else {
            token
        }
    } else {
        token
    };

    if path.contains("://")
        || !path.contains('/')
        || !path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'~'))
    {
        return false;
    }

    let segment_count = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .count();
    segment_count >= 2 || (path.starts_with('/') && segment_count == 1)
}

/// Narrow high-entropy exemption for mathematical notation (#1988).
///
/// A fragment needs both a LaTeX control sequence and several structural
/// delimiters. Any embedded credential-length hex or independently
/// high-entropy alphanumeric run disables the exemption, so wrapping an
/// opaque value in `\texttt{...}` does not launder it.
pub(super) fn is_latex_fragment_without_credential_run(token: &str) -> bool {
    let bytes = token.as_bytes();
    let has_control_sequence = bytes
        .windows(2)
        .any(|pair| pair[0] == b'\\' && pair[1].is_ascii_alphabetic());
    let structural_count = bytes
        .iter()
        .filter(|byte| matches!(byte, b'\\' | b'{' | b'}' | b'^' | b'_'))
        .count();
    if !has_control_sequence || structural_count < 3 {
        return false;
    }

    if normalized_hex_credential_span(token).is_some() {
        return false;
    }

    !token
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|run| run.len() >= MIN_ENTROPY_LEN)
        .any(|run| shannon_entropy(run.as_bytes()) >= ENTROPY_THRESHOLD)
}

/// A SHA-256 value is admitted only with a nearby checksum designation, not
/// with a credential field that merely happens to describe its encoding.
pub(super) fn is_labeled_sha256_digest(text: &str, token_offset: usize, token: &str) -> bool {
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return false;
    }
    let before = text[..token_offset]
        .trim_end_matches(|ch: char| ch.is_ascii_whitespace() || matches!(ch, ':' | '='));
    let marker = ["sha256 key digest", "sha256 digest", "sha256"]
        .into_iter()
        .find(|marker| {
            before
                .get(before.len().saturating_sub(marker.len())..)
                .is_some_and(|tail| tail.eq_ignore_ascii_case(marker))
        });
    let Some(marker) = marker else {
        return false;
    };
    let marker_start = before.len() - marker.len();
    if marker_start > 0
        && text[..marker_start]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return false;
    }
    !has_clause_credential_label_with_inline(text, marker_start, false, ClauseValueKind::FilePath)
}

/// Inline Rust call references have syntax that an opaque credential does not:
/// backticks, a qualified symbol, and an empty argument list. Arguments are
/// excluded because their values would otherwise become part of the exemption.
pub(super) fn is_prose_code_reference(member: &str, token: &str) -> bool {
    let Some(code) = member.strip_prefix('`').and_then(|s| s.strip_suffix('`')) else {
        return false;
    };
    if code != token || !code.ends_with("()") {
        return false;
    }
    let Some((owner, method)) = code[..code.len() - 2].split_once("::") else {
        return false;
    };
    owner
        .as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_alphabetic())
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && method
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic())
        && method
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'<' | b'>'))
        && !method.contains("::")
}

/// Uppercase environment references are names, not values, when their final
/// component names a conventional non-secret field. The run checks above
/// still refuse any credential-shaped component inside the name.
pub(super) fn is_environment_name(token: &str) -> bool {
    let mut components = token.split('_');
    let Some(first) = components.next() else {
        return false;
    };
    if first.is_empty() || !first.bytes().all(|b| b.is_ascii_uppercase()) {
        return false;
    }
    let mut count = 0;
    let mut last = "";
    for part in components {
        if part.is_empty()
            || part.len() > 16
            || !part
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        {
            return false;
        }
        count += 1;
        last = part;
    }
    count >= 3 && matches!(last, "PATH" | "FILE" | "NAME" | "TTL")
}

/// Closed, familiar math commands keep this exemption on notation rather
/// than arbitrary backslash-prefixed strings.
pub(super) fn is_latex_prose_macro(token: &str) -> bool {
    [
        "\\mathsf{",
        "\\mathbf{",
        "\\mathcal{",
        "\\mathrm{",
        "\\operatorname{",
    ]
    .iter()
    .any(|prefix| token.starts_with(prefix))
        && is_latex_fragment_without_credential_run(token)
}

/// Recognize two unambiguous AWS resource address forms. The resource path
/// is restricted to short name segments; an embedded long credential run is
/// refused before this predicate is reached.
pub(super) fn is_aws_resource_name(token: &str) -> bool {
    let mut parts = token.splitn(6, ':');
    let (Some("arn"), Some("aws"), Some(service), Some(region), Some(account), Some(resource)) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return false;
    };
    let valid_resource = !resource.is_empty()
        && resource.split('/').all(|segment| {
            !segment.is_empty()
                && segment.len() <= 24
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        });
    valid_resource
        && ((service == "s3" && region.is_empty() && account.is_empty())
            || (service == "iam"
                && region.is_empty()
                && account.len() == 12
                && account.bytes().all(|b| b.is_ascii_digit())
                && (resource.starts_with("role/") || resource.starts_with("policy/"))))
}

fn is_line_location_suffix(suffix: &str) -> bool {
    if suffix.is_empty() {
        return true;
    }
    let suffix = suffix.strip_prefix(':').unwrap_or(suffix);
    let suffix = suffix.strip_prefix('~').unwrap_or(suffix);
    let mut parts = suffix.split('-');
    let Some(start) = parts.next() else {
        return false;
    };
    !start.is_empty()
        && start.bytes().all(|b| b.is_ascii_digit())
        && parts
            .next()
            .is_none_or(|end| !end.is_empty() && end.bytes().all(|b| b.is_ascii_digit()))
        && parts.next().is_none()
}

/// The raw span whose consecutive pure-hex runs — splitting on every
/// non-alphanumeric character and dropping the separators for length
/// accounting — reach one of [`HEX_CREDENTIAL_LENGTHS`].
///
/// Closes the separator-dilution bypass (#1062) where a credential-length
/// hex payload is spread across multiple runs each individually below
/// [`MIN_ENTROPY_LEN`]: `0123456789abcdef0123/456789abcdef01234567` is two
/// 20-char hex runs that never individually reach 24 chars or a
/// credential-length boundary, but normalize to one 40-char hex sequence.
/// A non-hex, non-empty run resets the running sum — this only bridges
/// ADJACENT hex runs, not hex fragments scattered across unrelated filler.
pub(super) fn normalized_hex_credential_span(token: &str) -> Option<&str> {
    let mut concatenated_len = 0usize;
    let mut span_start = 0usize;
    for run in token.split(|c: char| !c.is_ascii_alphanumeric()) {
        if run.is_empty() {
            continue;
        }
        if is_hex_run(run) {
            let run_start = run.as_ptr() as usize - token.as_ptr() as usize;
            if concatenated_len == 0 {
                span_start = run_start;
            }
            concatenated_len += run.len();
            if HEX_CREDENTIAL_LENGTHS.contains(&concatenated_len) {
                return Some(&token[span_start..run_start + run.len()]);
            }
        } else {
            concatenated_len = 0;
        }
    }
    None
}
