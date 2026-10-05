#[cfg(doc)]
use super::contains_bounded_word;
#[cfg(test)]
use super::ENTROPY_TOKENIZATION_COUNT;
use super::{
    after_last_sentence_boundary, assignment_credential_trigger, before_first_sentence_boundary,
    bridge_fragment_chain, find_trigger, has_clause_credential_label_with_inline,
    has_direct_repository_credential_label_with_inline, has_immediate_credential_label,
    inline_credential_trigger, is_aws_resource_name, is_base64_content_hash,
    is_bridge_fragment_shape, is_environment_name, is_git_revision_reference,
    is_labeled_sha256_digest, is_latex_fragment_without_credential_run, is_latex_prose_macro,
    is_plausible_file_path, is_prose_code_reference, is_pure_hex, is_repository_revision_reference,
    is_structured_identifier, is_uuid_canonical, is_vcs_marker_before_hex,
    normalized_hex_credential_span, shannon_entropy, strip_delimiters, value_candidates,
    wrapper_strip_repeated, ClauseValueKind,
};

// ─── Layer 2: entropy heuristic ─────────────────────────────────────────────

/// Trigger words checked as a bounded standalone word (see
/// [`contains_bounded_word`]). `token` is deliberately excluded — see
/// `has_standalone_token`/`has_token_assignment` instead.
/// See `docs/api/secret_gate.md#trigger_words` for the substring-collision
/// rationale (issues #577 / #632).
pub(super) const TRIGGER_WORDS: &[&str] = &[
    "key",
    "secret",
    "password",
    "passwd",
    "credential",
    "bearer",
    "auth",
    "apikey",
];

/// Compound triggers that retain suffix matching inside credential labels.
/// Their underscore separator disambiguates them from ordinary prose, and
/// suffixes are common in versioned credential names such as `api_keyv2`.
pub(super) const COMPOUND_TRIGGER_WORDS: &[&str] = &["api_key", "access_key", "private_key"];

/// Minimum token length to apply the entropy check.
pub(super) const MIN_ENTROPY_LEN: usize = 24;

/// Shannon entropy threshold (bits per character) above which a token is
/// considered high-entropy.  7.0 corresponds to ~99% utilisation of a
/// 128-symbol alphabet — typical for random base64/hex.
pub(super) const ENTROPY_THRESHOLD: f64 = 4.5;

/// Window around a trigger word in which a high-entropy token must appear.
pub(super) const TRIGGER_WINDOW: usize = 120;

/// Credential-shaped exact hex lengths (AWS secret key, SHA-256/git SHA
/// doubled, SHA-512 hex, etc.) — checked against a whole token, a single
/// separator-delimited run, and a normalized (separator-stripped)
/// concatenation of adjacent hex runs/tokens; see
/// [`normalized_hex_credential_span`].
pub(super) const HEX_CREDENTIAL_LENGTHS: &[usize] = &[32, 40, 64, 128];

/// Max fragments [`bridge_fragment_chain`] concatenates per credential (fragment-count bound,
/// not gap byte length — see docs/api/secret_gate.md#bridge-fragment-reconstruction).
pub(super) const MAX_BRIDGE_FRAGMENTS: usize = 6;

/// Max delimiter-only glue tokens absorbed per direction while bridging fragments; see
/// docs/api/secret_gate.md#bridge-fragment-reconstruction.
pub(super) const MAX_BRIDGE_GLUE_TOKENS: usize = 6;

/// Shortest bare token treated as a plausible bridge fragment; see
/// docs/api/secret_gate.md#bridge-fragment-reconstruction.
pub(super) const MIN_BRIDGE_FRAGMENT_LEN: usize = 8;

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

/// Tokenize the full input once for the entropy detector. The returned offsets
/// are absolute offsets into `text` and remain valid for every scan cursor.
fn tokenize_entropy_tokens(text: &str) -> Vec<(usize, &str)> {
    #[cfg(test)]
    ENTROPY_TOKENIZATION_COUNT.with(|count| count.set(count.get() + 1));

    // Tokenize into maximal ASCII non-whitespace runs; non-ASCII chars are also
    // delimiters (see docs/api/secret_gate.md#module-level-detection-algorithm,
    // "non-ASCII token delimiting"). Identical to `split_ascii_whitespace` on
    // pure-ASCII input.
    text.split(|c: char| c.is_ascii_whitespace() || !c.is_ascii())
        .filter(|t| !t.is_empty())
        .map(|t| {
            let offset = t.as_ptr() as usize - text.as_ptr() as usize;
            (offset, t)
        })
        .collect()
}

#[derive(Clone, Copy)]
struct JsonScalarContext {
    value_start: usize,
    end: usize,
    label: Option<(usize, &'static str)>,
}

pub(super) struct EntropyScanContext<'a> {
    pub(super) tokens: Vec<(usize, &'a str)>,
    scalars: Vec<JsonScalarContext>,
}

impl<'a> EntropyScanContext<'a> {
    pub(super) fn new(text: &'a str) -> Self {
        Self {
            tokens: tokenize_entropy_tokens(text),
            scalars: json_scalar_contexts(text),
        }
    }

    fn scalar_for(&self, text: &str, value: &str) -> Option<JsonScalarContext> {
        let value = wrapper_strip_repeated(value);
        if value.is_empty() {
            return None;
        }
        let start = value.as_ptr() as usize - text.as_ptr() as usize;
        let end = start + value.len();
        let index = self.scalars.partition_point(|scalar| scalar.end <= start);
        self.scalars
            .get(index)
            .copied()
            .filter(|scalar| end > scalar.value_start && end <= scalar.end)
    }
}

// Validate once before interpreting punctuation as field boundaries. The
// iterative source walk retains offsets for masking; decoding keys prevents
// JSON escapes from hiding an owning credential label.
fn json_scalar_contexts(text: &str) -> Vec<JsonScalarContext> {
    if !text.trim_start().starts_with(['{', '['])
        || serde_json::from_str::<serde::de::IgnoredAny>(text).is_err()
    {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let mut scalars = Vec::new();
    let mut containers: Vec<Option<(usize, &'static str)>> = Vec::new();
    let mut pending_label = None;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'{' | b'[' => {
                let label = pending_label
                    .take()
                    .or_else(|| containers.last().copied().flatten());
                containers.push(label);
                index += 1;
            }
            b'}' | b']' => {
                containers.pop();
                pending_label = None;
                index += 1;
            }
            b',' | b':' | b' ' | b'\t' | b'\r' | b'\n' => index += 1,
            _ => {
                let start = index;
                let quoted = bytes[index] == b'"';
                let value_start = start + usize::from(quoted);
                let end = if quoted {
                    index += 1;
                    while index < bytes.len() && bytes[index] != b'"' {
                        index += if bytes[index] == b'\\' { 2 } else { 1 };
                    }
                    let end = index;
                    index += 1;
                    end
                } else {
                    while index < bytes.len()
                        && !bytes[index].is_ascii_whitespace()
                        && !matches!(bytes[index], b',' | b'}' | b']')
                    {
                        index += 1;
                    }
                    index
                };
                if index > bytes.len() {
                    return Vec::new();
                }
                if quoted && text[index..].trim_start().starts_with(':') {
                    let Ok(key) = serde_json::from_str::<String>(&text[start..index]) else {
                        return Vec::new();
                    };
                    let label =
                        find_trigger(&format!("{key}:"), true).map(|trigger| (end, trigger));
                    pending_label = label;
                    continue;
                }
                let label = pending_label
                    .take()
                    .or_else(|| containers.last().copied().flatten());
                scalars.push(JsonScalarContext {
                    value_start,
                    end,
                    label,
                });
            }
        }
    }
    scalars
}

// A bare Git-length value uses line-local context. The preceding label line
// remains authoritative when it explicitly ends in an assignment delimiter.
// Bridge anchors retain full-window context so masking cannot leave a fragment behind.
fn entropy_trigger(
    text: &str,
    tokens: &[(usize, &str)],
    index: usize,
    credential_label_only: bool,
    candidate: EntropyCandidate<'_>,
) -> Option<&'static str> {
    let (mut offset, raw) = tokens[index];
    let mut token_end = offset + raw.len();
    if let Some(scalar) = candidate.scalar {
        offset =
            (candidate.value.as_ptr() as usize - text.as_ptr() as usize).max(scalar.value_start);
        token_end = (candidate.value.as_ptr() as usize - text.as_ptr() as usize
            + candidate.value.len())
        .min(scalar.end);
    }
    let mut window_start = floor_char_boundary(text, offset.saturating_sub(TRIGGER_WINDOW));
    let mut window_end = floor_char_boundary(text, token_end + TRIGGER_WINDOW);
    if let Some(scalar) = candidate.scalar {
        window_start = window_start.max(scalar.value_start);
        window_end = window_end.min(scalar.end);
    }
    let token = strip_delimiters(raw);
    let standalone_revision = token.len() == 40
        && token.bytes().all(|b| b.is_ascii_hexdigit())
        && bridge_fragment_chain(tokens, text, index).len() == 1;
    let (start, end, preceding_label) = if standalone_revision {
        let line_start = text[window_start..offset]
            .rfind(['\r', '\n'])
            .map_or(window_start, |i| window_start + i + 1);
        let line_end = text[token_end..window_end]
            .find(['\r', '\n'])
            .map_or(window_end, |i| token_end + i);
        let before_line = &text[window_start..line_start];
        let previous = before_line
            .strip_suffix("\r\n")
            .or_else(|| before_line.strip_suffix(['\r', '\n']))
            .unwrap_or("");
        let previous_start = previous.rfind(['\r', '\n']).map_or(0, |i| i + 1);
        let previous = previous[previous_start..].trim_end();
        let label = if previous.ends_with([':', '=']) {
            find_trigger(previous, credential_label_only)
        } else {
            None
        };
        (
            line_start.max(window_start),
            line_end.min(window_end),
            label,
        )
    } else {
        (window_start, window_end, None)
    };
    // Trigger context stops at a sentence boundary: a detector name or an
    // unrelated clause in the preceding sentence is not context for this token
    // (issue #2056). The preceding-label fallback is exempt because that line
    // was already required to end in an assignment delimiter.
    find_trigger(
        after_last_sentence_boundary(&text[start..offset]),
        credential_label_only,
    )
    .or_else(|| {
        find_trigger(
            before_first_sentence_boundary(&text[token_end..end]),
            credential_label_only,
        )
    })
    .or(candidate.inline_trigger)
    .or(preceding_label)
}

/// An entropy view with an assignment context bounded to one inline member.
#[derive(Clone, Copy)]
struct EntropyCandidate<'a> {
    value: &'a str,
    member: &'a str,
    inline_trigger: Option<&'static str>,
    bridge_anchor: bool,
    scalar: Option<JsonScalarContext>,
}

fn entropy_candidates(raw: &str) -> Vec<EntropyCandidate<'_>> {
    let mut candidates = Vec::new();
    // Preserve external-window reconstruction across punctuation. This view
    // deliberately supplies no inline label from anywhere in the token.
    if raw.contains([',', ';', '&']) {
        candidates.push(EntropyCandidate {
            value: raw,
            member: raw,
            inline_trigger: None,
            bridge_anchor: true,
            scalar: None,
        });
    }
    for member in raw
        .split([',', ';', '&'])
        .filter(|member| !member.is_empty())
    {
        let has_assignment = member.contains([':', '=']);
        candidates.push(EntropyCandidate {
            value: member,
            member,
            inline_trigger: (!has_assignment)
                .then(|| inline_credential_trigger(member))
                .flatten(),
            bridge_anchor: member.len() == raw.len(),
            scalar: None,
        });
        if !has_assignment {
            continue;
        }
        // An underscore carrier is still a value when an enclosing benign
        // assignment or trailing base64 padding introduces delimiters. Bound
        // this fallback to its own segment so a later label cannot govern an
        // earlier unrelated value.
        for segment in member.split([':', '=']) {
            if let Some(inline_trigger) = inline_credential_trigger(segment) {
                candidates.push(EntropyCandidate {
                    value: wrapper_strip_repeated(segment),
                    member,
                    inline_trigger: Some(inline_trigger),
                    bridge_anchor: false,
                    scalar: None,
                });
            }
        }
        let low = member.to_ascii_lowercase();
        let mut assignment_start = 0;
        for (offset, separator) in member.char_indices() {
            if !matches!(separator, ':' | '=') {
                continue;
            }
            let end = offset + separator.len_utf8();
            let inline_trigger = assignment_credential_trigger(&low[assignment_start..end]);
            assignment_start = end;
            if inline_trigger.is_none() {
                continue;
            }
            let value = wrapper_strip_repeated(&member[end..]);
            if value.is_empty() {
                continue;
            }
            // The first credential assignment governs this member's remaining
            // value, including nested carriers. Exact UUID/hash extraction
            // still tries every suffix via `value_candidates`; the run and
            // reconstruction checks see the entire governed value as before.
            candidates.push(EntropyCandidate {
                value,
                member,
                inline_trigger,
                bridge_anchor: member.len() == raw.len(),
                scalar: None,
            });
            break;
        }
    }
    candidates
}

/// Preserve the original bounded bridge walk while clipping inline context
/// at sibling-member boundaries and the governing value's start. A later
/// assignment cannot use an earlier value as its bridge anchor. External-window
/// candidates retain the original token and its unchanged bridge reconstruction.
fn entropy_bridge_fragments<'a>(
    tokens: &[(usize, &'a str)],
    text: &'a str,
    index: usize,
    candidate: EntropyCandidate<'a>,
) -> Vec<&'a str> {
    let fragments = bridge_fragment_chain(tokens, text, index);
    if candidate.inline_trigger.is_none() {
        return fragments;
    }
    let raw = tokens[index].1;
    let raw_start = raw.as_ptr() as usize;
    let raw_end = raw_start + raw.len();
    let value_start = candidate.value.as_ptr() as usize;
    let member_end = candidate.member.as_ptr() as usize + candidate.member.len();
    fragments
        .into_iter()
        .filter_map(|fragment| {
            let start = fragment.as_ptr() as usize;
            if (raw_start..raw_end).contains(&start) {
                Some(strip_delimiters(candidate.value))
            } else if (start < raw_start && value_start == raw_start)
                || (start >= raw_end && member_end == raw_end)
            {
                Some(fragment)
            } else {
                None
            }
        })
        .collect()
}

/// `from` limits returned spans; context remains relative to each original
/// whitespace token, including when masking resumes inside one member.
pub(super) fn check_entropy_heuristic<'a>(
    text: &'a str,
    from: usize,
    context: &EntropyScanContext<'a>,
) -> Option<(&'a str, &'static str, Option<&'static str>)> {
    let tokens = &context.tokens;
    let first_token = tokens.partition_point(|&(offset, raw)| offset + raw.len() <= from);
    for (idx, &(context_offset, raw)) in tokens.iter().enumerate().skip(first_token) {
        let token = strip_delimiters(raw);
        if token.len() < MIN_ENTROPY_LEN && !is_bridge_fragment_shape(token) {
            continue;
        }
        let mut best: Option<(&str, &'static str, Option<&'static str>)> = None;
        for mut candidate in entropy_candidates(raw) {
            candidate.scalar = context.scalar_for(text, candidate.value);
            if candidate.scalar.is_none() && !context.scalars.is_empty() {
                let core = wrapper_strip_repeated(candidate.value);
                let start = core.as_ptr() as usize - text.as_ptr() as usize;
                let end = start + core.len();
                let first = context
                    .scalars
                    .partition_point(|scalar| scalar.end <= start);
                if context
                    .scalars
                    .get(first)
                    .is_some_and(|scalar| scalar.value_start < end && scalar.end < end)
                {
                    // The member views below still scan each value; a raw
                    // bridge anchor spanning sibling scalars has no shared label.
                    continue;
                }
            }
            if let Some(scalar) = candidate.scalar {
                let offset = candidate.value.as_ptr() as usize - text.as_ptr() as usize;
                let label = scalar
                    .label
                    .filter(|(end, _)| offset.saturating_sub(*end) <= TRIGGER_WINDOW)
                    .map(|(_, trigger)| trigger);
                candidate.inline_trigger = candidate.inline_trigger.or(label);
            }
            let offset = candidate.value.as_ptr() as usize - text.as_ptr() as usize;
            if offset + candidate.value.len() <= from {
                continue;
            }
            if let Some(found) =
                check_entropy_candidate(text, from, tokens, idx, context_offset, candidate)
            {
                if best
                    .as_ref()
                    .is_none_or(|current| found.0.as_ptr() < current.0.as_ptr())
                {
                    best = Some(found);
                }
            }
        }
        if best.is_some() {
            return best;
        }
    }
    None
}

fn check_entropy_candidate<'a>(
    text: &'a str,
    from: usize,
    tokens: &[(usize, &'a str)],
    idx: usize,
    context_offset: usize,
    candidate: EntropyCandidate<'a>,
) -> Option<(&'a str, &'static str, Option<&'static str>)> {
    let raw_token = candidate.value;
    // Strip common delimiters that wrap the actual value.
    let original_offset = raw_token.as_ptr() as usize - text.as_ptr() as usize;
    let remaining = &raw_token[from.saturating_sub(original_offset).min(raw_token.len())..];
    let token = strip_delimiters(remaining);
    // Only RETURN tokens at or after `from` (already-redacted spans lie
    // before it); the trigger window below still spans the full text.
    let token_offset = token.as_ptr() as usize - text.as_ptr() as usize;
    if token_offset < from {
        return None;
    }
    // A token below MIN_ENTROPY_LEN still passes through when it's a plausible
    // bridge FRAGMENT (see docs/api/secret_gate.md#bridge-fragment-reconstruction);
    // gating on alphanumeric runs (not hex-only) covers base64/base64url halves too.
    let is_bridge_candidate = is_bridge_fragment_shape(token);
    if token.len() < MIN_ENTROPY_LEN && !is_bridge_candidate {
        return None;
    }

    // `token` is ASCII here (non-ASCII was split out at tokenization), so
    // `shannon_entropy` over its bytes is a true per-character entropy.

    // Compute the trigger window before any shape-based allowlist decision.
    // UUIDs require credential-label context rather than a generic mention
    // of `token`; base64 content-hash exemptions remain trigger-sensitive.
    // VCS revisions and file paths use narrower syntactic context below.
    let trigger = entropy_trigger(text, tokens, idx, false, candidate);
    let near_trigger = trigger.is_some();
    let uuid_trigger = entropy_trigger(text, tokens, idx, true, candidate);
    let uuid_near_credential_label = uuid_trigger.is_some();

    // Step 1 (see doc: per-token flagging sequence). UUIDs fall through only
    // beside an explicit credential label; the generic word `token` remains
    // trigger context for opaque values but is common in design prose. Content
    // hashes retain the broader trigger rule. Hex-shaped entropy alone (<=4.0
    // bits/char) can never reach ENTROPY_THRESHOLD.
    let has_uuid_candidate = value_candidates(token).any(is_uuid_canonical);
    if uuid_near_credential_label && has_uuid_candidate {
        return Some((token, "uuid-near-trigger", uuid_trigger));
    }
    if near_trigger && value_candidates(token).any(is_base64_content_hash) {
        return Some((token, "content-hash-near-trigger", trigger));
    }
    if !uuid_near_credential_label && is_uuid_canonical(token) {
        return None;
    }
    if !near_trigger && is_base64_content_hash(token) {
        return None;
    }

    // Step 2. Pure hex off-trigger is allowlisted; trigger-adjacent hex needs an
    // explicit VCS coordinate marker (see doc).
    if !near_trigger && is_pure_hex(token) {
        return None;
    }

    // A fixed-width SHA-256 digest has an independent, explicit source label.
    // Defer its admission until after bridge reconstruction: a digest-shaped
    // fragment must still be checked as part of a larger candidate.
    let labeled_sha256_digest = near_trigger
        && candidate.inline_trigger.is_none()
        && is_labeled_sha256_digest(text, token_offset, token);

    // VCS-marker exemption is a flag over the hex-credential-shape checks only,
    // never an early skip of fragment reconstruction below (see doc).
    if is_vcs_marker_before_hex(text, candidate.member)
        && !has_clause_credential_label_with_inline(
            text,
            context_offset,
            candidate.inline_trigger.is_some(),
            ClauseValueKind::VcsReference,
        )
    {
        return None;
    }

    let repository_revision_reference = is_repository_revision_reference(candidate.member);
    let vcs_reference_exempt = if repository_revision_reference {
        !has_direct_repository_credential_label_with_inline(
            text,
            context_offset,
            candidate.inline_trigger.is_some(),
        )
    } else {
        is_git_revision_reference(text, context_offset, candidate.member)
            && !has_clause_credential_label_with_inline(
                text,
                context_offset,
                candidate.inline_trigger.is_some(),
                ClauseValueKind::VcsReference,
            )
    };

    // Dense mathematical notation has the same mixed-character entropy
    // profile as an opaque token. Exempt a syntactically recognizable
    // LaTeX fragment only when it contains no credential-shaped run and
    // the immediately preceding field does not label it as a credential
    // (#1988). Known-prefix detectors have already run before this layer.
    if near_trigger
        && is_latex_fragment_without_credential_run(token)
        && !(candidate.inline_trigger.is_some()
            || has_immediate_credential_label(text, context_offset))
    {
        return None;
    }

    // Step 3. Hex API keys aren't caught by the entropy heuristic (hex tops out at
    // 4.0 bits/char, below ENTROPY_THRESHOLD 4.5); flag credential-shaped hex directly.
    if !vcs_reference_exempt
        && near_trigger
        && !labeled_sha256_digest
        && is_pure_hex(token)
        && HEX_CREDENTIAL_LENGTHS.contains(&token.len())
    {
        return Some((token, "hex-credential-token", trigger));
    }

    // Step 4 (issue #1044): a credential can dilute below the whole-token-average
    // checks above via low-entropy filler sharing its whitespace token
    // (`vault/<payload>/rotate.md`); re-check each `/`-split run independently.
    // See doc for the #1040 corpus rationale behind the MIN_ENTROPY_LEN floor.
    if near_trigger {
        // vcs_reference_exempt also covers single-token forms below (`rev:<hex>`);
        // it does not cover fragment reconstruction.
        for run in token.split(|c: char| !c.is_ascii_alphanumeric()) {
            if run.len() < MIN_ENTROPY_LEN {
                continue;
            }
            if !vcs_reference_exempt
                && !labeled_sha256_digest
                && is_pure_hex(run)
                && HEX_CREDENTIAL_LENGTHS.contains(&run.len())
            {
                return Some((run, "hex-credential-token", trigger));
            }
            if shannon_entropy(run.as_bytes()) >= ENTROPY_THRESHOLD {
                return Some((token, "high-entropy-token", trigger));
            }
        }

        // Step 5 (#1062): concatenate consecutive pure-hex runs (dropping
        // separators) and re-check against HEX_CREDENTIAL_LENGTHS — catches a
        // hex payload split into multiple sub-floor runs. See doc.
        if !vcs_reference_exempt && !labeled_sha256_digest {
            if let Some(candidate) = normalized_hex_credential_span(token) {
                return Some((candidate, "hex-credential-token", trigger));
            }
        }

        // Step 6 (#1062, Unicode variant): bridge fragments split across non-ASCII
        // tokenizer delimiters (e.g. U+200B) via `bridge_fragment_chain`, which walks
        // both directions across a bounded chain (MAX_BRIDGE_FRAGMENTS,
        // MAX_BRIDGE_GLUE_TOKENS) rather than one adjacent pair — see
        // docs/api/secret_gate.md#check_entropy_heuristic--per-token-flagging-sequence
        // for the exact guarantee and its accepted residual (same-uid-host) limits.
        if !vcs_reference_exempt
            && (candidate.bridge_anchor || candidate.inline_trigger.is_some())
            && tokens.len() > 1
        {
            let fragments = entropy_bridge_fragments(tokens, text, idx, candidate);
            if fragments.len() > 1 {
                let first = fragments[0];
                let last = fragments[fragments.len() - 1];
                let chain_start = first.as_ptr() as usize - text.as_ptr() as usize;
                let chain_end = last.as_ptr() as usize - text.as_ptr() as usize + last.len();
                let search_start = chain_start.max(from);
                if let Some(candidate) =
                    normalized_hex_credential_span(&text[search_start..chain_end])
                {
                    return Some((candidate, "hex-credential-token", trigger));
                }
                let concatenated: String = fragments.concat();
                if concatenated.len() >= MIN_ENTROPY_LEN
                    && concatenated.bytes().all(|b| b.is_ascii_alphanumeric())
                    && shannon_entropy(concatenated.as_bytes()) >= ENTROPY_THRESHOLD
                {
                    return Some((token, "high-entropy-token", trigger));
                }
            }
        }

        if is_plausible_file_path(token)
            && !has_clause_credential_label_with_inline(
                text,
                context_offset,
                candidate.inline_trigger.is_some(),
                ClauseValueKind::FilePath,
            )
        {
            return None;
        }

        // Step 8. These shapes are references in technical prose. The prefix, per-run,
        // normalized-hex, and bridge detectors above retain priority over
        // them; an opaque value inside any of those carriers still refuses.
        if labeled_sha256_digest
            || is_prose_code_reference(candidate.member, token)
            || is_environment_name(token)
            || is_latex_prose_macro(token)
            || is_aws_resource_name(token)
        {
            return None;
        }
    }

    // Canonical repository links and href commit targets are source
    // coordinates, not standalone values. The direct-label guard above
    // keeps `api key: <revision URL>` fail-closed; technical prose such as
    // `key-scoped source` can safely retain the citation (#2076).
    if vcs_reference_exempt {
        return None;
    }

    // Step 9: structured-identifier exemption, off-trigger only. Must run after the
    // UUID/hex checks and before the entropy computation (an identifier can exceed
    // ENTROPY_THRESHOLD on Shannon entropy alone).
    if !near_trigger && is_structured_identifier(token) {
        return None;
    }

    let entropy = shannon_entropy(token.as_bytes());
    if entropy < ENTROPY_THRESHOLD {
        return None;
    }

    // High-entropy token in trigger context — flag it.
    if near_trigger {
        return Some((token, "high-entropy-token", trigger));
    }
    None
}
