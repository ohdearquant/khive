#[cfg(doc)]
use super::{check_entropy_heuristic, mask_bounded, normalized_hex_credential_span};
use super::{
    is_assignment_label_gap, is_lookup_key_label, strip_delimiters, tokenize_entropy_tokens,
    MAX_BRIDGE_FRAGMENTS, MAX_BRIDGE_GLUE_TOKENS, MIN_BRIDGE_FRAGMENT_LEN,
};

/// `true` when the gap `text[gap_start..gap_end]` between two adjacent
/// tokenizer tokens holds no ASCII alphanumeric character — the shape a
/// tokenizer-delimiting separator (ASCII whitespace, or a non-ASCII
/// character such as U+200B; see the tokenizer comment in
/// [`check_entropy_heuristic`]) leaves behind when it splits one credential
/// payload into two tokens. Deliberately UNBOUNDED on gap byte length
/// (#1062: a byte-length bound here is defeated outright by
/// repeating the delimiter character) — [`bridge_fragment_chain`] is what
/// keeps the overall reconstruction bounded, via [`MAX_BRIDGE_FRAGMENTS`],
/// not this check. This still never bridges tokens separated by a genuine
/// word or sentence: any real word in the gap contains an ASCII alphanumeric
/// character and fails the check immediately.
fn adjacent_gap_is_bridgeable(text: &str, gap_start: usize, gap_end: usize) -> bool {
    gap_end >= gap_start && !text[gap_start..gap_end].contains(|c: char| c.is_ascii_alphanumeric())
}

/// `true` when `s` is shaped like a plausible FRAGMENT of a separator-split
/// credential: alphanumeric-only and at least [`MIN_BRIDGE_FRAGMENT_LEN`]
/// bytes. Shared by the short-token anchor-admission check in
/// [`check_entropy_heuristic`] and by [`bridge_fragment_chain`]'s outward
/// walk. A long anchor can reach reconstruction without satisfying this
/// shape, but every neighboring fragment merged into its chain must satisfy
/// it. This stops the walk at a short trigger/glue word (`key`, `api`, `for`)
/// sitting immediately beside the real fragments (#1062).
pub(super) fn is_bridge_fragment_shape(s: &str) -> bool {
    s.len() >= MIN_BRIDGE_FRAGMENT_LEN && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// `true` when `s` holds no ASCII alphanumeric character at all — the same
/// predicate [`adjacent_gap_is_bridgeable`] applies to the byte-range GAP
/// between two tokenizer tokens, applied here to a tokenizer TOKEN itself
/// (`s` is always non-empty: the tokenizer filters empty tokens). A
/// delimiter-only token such as `---` sitting between two Unicode-separator
/// gaps (#1062) carries none of a credential's own
/// characters — it is exactly as transparent to reconstruction as the
/// surrounding whitespace/Unicode gaps are, so [`bridge_fragment_chain`]
/// treats it as glue to walk across, not as a chain-terminating non-fragment
/// token. A token can never be both this and [`is_bridge_fragment_shape`]:
/// the latter requires only alphanumeric bytes, this requires none.
fn is_delimiter_only_token(s: &str) -> bool {
    !s.bytes().any(|b| b.is_ascii_alphanumeric())
}

/// Looks outward from `tokens[edge]` in `dir` (`-1` = toward index 0, `+1` =
/// toward the end) for the next [`is_bridge_fragment_shape`] token, walking
/// transparently across up to [`MAX_BRIDGE_GLUE_TOKENS`] consecutive
/// [`is_delimiter_only_token`] glue tokens along the way. Every gap crossed
/// — including the ones on either side of a glue token — must be
/// [`adjacent_gap_is_bridgeable`]. Returns the found fragment's index, or
/// `None` if the walk runs off the end of `tokens`, meets a token that is
/// neither a fragment nor glue, meets a non-bridgeable gap, or exhausts the
/// glue budget before finding a fragment.
fn probe_bridge_fragment(
    tokens: &[(usize, &str)],
    text: &str,
    edge: usize,
    dir: isize,
) -> Option<usize> {
    let mut i = edge;
    let mut glue_skipped = 0usize;
    loop {
        let next_i = i.checked_add_signed(dir)?;
        if next_i >= tokens.len() {
            return None;
        }
        let (lo, hi) = if dir < 0 { (next_i, i) } else { (i, next_i) };
        let (lo_offset, lo_raw) = tokens[lo];
        let (hi_offset, _) = tokens[hi];
        let gap_start = lo_offset + lo_raw.len();
        if !adjacent_gap_is_bridgeable(text, gap_start, hi_offset) {
            return None;
        }
        let candidate = strip_delimiters(tokens[next_i].1);
        if is_bridge_fragment_shape(candidate) {
            return Some(next_i);
        }
        if is_delimiter_only_token(candidate) && glue_skipped < MAX_BRIDGE_GLUE_TOKENS {
            glue_skipped += 1;
            i = next_i;
            continue;
        }
        return None;
    }
}

/// Reconstructs the bounded chain of tokenizer fragments containing
/// `tokens[anchor_idx]`, by walking outward in both directions via
/// [`probe_bridge_fragment`] until the chain has reached
/// [`MAX_BRIDGE_FRAGMENTS`] real fragments or neither direction can extend
/// further. Returns each REAL fragment's [`strip_delimiters`]-ed body, in
/// document order, for the caller to recombine — any delimiter-only glue
/// tokens absorbed along the way (#1062) are dropped from the
/// result entirely, so a caller joining fragments with a space
/// ([`normalized_hex_credential_span`]) or concatenating them directly
/// (the generic entropy check) sees only the genuine fragments, exactly as
/// if the glue were more gap. Extends both directions every iteration so a
/// credential split with fragments on both sides of the anchor (e.g. the
/// anchor is the MIDDLE fragment of a three-way split) is fully
/// reconstructed, not just one side of it. A length-1 result means no
/// extension was possible — callers should skip further work in that case.
/// The anchor itself is included without a fragment-shape check; only outward
/// extensions are admitted through [`probe_bridge_fragment`].
pub(super) fn bridge_fragment_chain<'a>(
    tokens: &[(usize, &'a str)],
    text: &str,
    anchor_idx: usize,
) -> Vec<&'a str> {
    let mut start = anchor_idx;
    let mut end = anchor_idx;
    let mut fragment_count = 1usize;

    loop {
        let mut extended = false;
        if fragment_count < MAX_BRIDGE_FRAGMENTS && start > 0 {
            if let Some(new_start) = probe_bridge_fragment(tokens, text, start, -1) {
                start = new_start;
                fragment_count += 1;
                extended = true;
            }
        }
        if fragment_count < MAX_BRIDGE_FRAGMENTS && end + 1 < tokens.len() {
            if let Some(new_end) = probe_bridge_fragment(tokens, text, end, 1) {
                end = new_end;
                fragment_count += 1;
                extended = true;
            }
        }
        if !extended {
            break;
        }
    }

    tokens[start..=end]
        .iter()
        .map(|&(_, raw)| strip_delimiters(raw))
        .filter(|stripped| !is_delimiter_only_token(stripped))
        .collect()
}

/// Extra backward truncation [`mask_bounded`] applies after it drops the
/// token straddling the window boundary.
///
/// `bridge_fragment_chain` can reconstruct a credential from up to
/// [`MAX_BRIDGE_FRAGMENTS`] whitespace-separated pieces, each individually
/// too short or low-entropy on its own to be recognized. The gap between two
/// fragments is deliberately unbounded in byte length
/// (`adjacent_gap_is_bridgeable`), so there is no finite forward lookahead
/// past the window boundary that could guarantee seeing every fragment of a
/// chain straddling the cut — a chain can be padded arbitrarily far past the
/// window by a single long glue token, which still counts as only one hop
/// against [`MAX_BRIDGE_GLUE_TOKENS`]. Rather than scan past the boundary,
/// this walks BACKWARD from it — data already read into the window, so the
/// extra work stays bounded by `window_chars` alone and needs no lookahead —
/// dropping every further bridge-fragment-shaped token chained to the
/// fragment [`mask_bounded`] already removed, spending the same
/// fragment-count and glue-token budgets [`bridge_fragment_chain`] would
/// spend walking outward from an anchor.
///
/// Runs unconditionally on every truncated window, regardless of whether the
/// window itself carries trigger-word context. `collect_mask_spans` step 6
/// admits trigger context from EITHER side of a fragment chain (a credential
/// like `<frag> <frag> <frag> is the api key for ...` is reconstructed by the
/// unbounded masker even though the trigger sits after the fragments), so a
/// trigger word that would justify keeping this window's tail may sit past
/// the window boundary — data this function, by construction, cannot see.
/// Gating the walk on an in-window trigger check would leave exactly that
/// case unprotected: the window carries no visible trigger, the walk would
/// never run, and any whole fragments already read into the window would
/// leak. The walk cannot distinguish a genuine chained fragment from an
/// unrelated fragment-shaped word sitting at the tail of an untriggered
/// window either; the trade this makes is dropping that word too rather than
/// risking a leaked credential fragment. The cost is bounded: at most
/// `MAX_BRIDGE_FRAGMENTS - 1` tokens of a tail that mask_bounded has already
/// decided to truncate.
///
/// Returns the byte offset to truncate `window` to, or `None` when nothing
/// beyond the already-trimmed token needs to go.
pub(super) fn trailing_bridge_fragment_cut(window: &str) -> Option<usize> {
    let tokens = tokenize_entropy_tokens(window);
    let mut idx = tokens.len().checked_sub(1)?;
    let mut fragment_budget = MAX_BRIDGE_FRAGMENTS - 1;
    let mut glue_budget = MAX_BRIDGE_GLUE_TOKENS;
    let mut cut_at = None;

    loop {
        let (offset, raw) = tokens[idx];
        let candidate = strip_delimiters(raw);
        if is_bridge_fragment_shape(candidate) {
            if fragment_budget == 0 {
                break;
            }
            fragment_budget -= 1;
            glue_budget = MAX_BRIDGE_GLUE_TOKENS;
            cut_at = Some(offset);
        } else if is_delimiter_only_token(candidate) {
            if glue_budget == 0 {
                break;
            }
            glue_budget -= 1;
        } else {
            break;
        }

        if idx == 0 {
            break;
        }
        let (prev_offset, prev_raw) = tokens[idx - 1];
        let gap_start = prev_offset + prev_raw.len();
        if !adjacent_gap_is_bridgeable(window, gap_start, offset) {
            break;
        }
        idx -= 1;
    }

    cut_at
}

/// Returns `true` when `low_window` contains `needle` as a standalone word —
/// bounded on both sides by a character outside the word-char set (or
/// start/end of string) — rather than merely as a substring.
/// `underscore_is_word_char` selects the boundary rule the caller needs; see
/// `docs/api/secret_gate.md#contains_word` for the two deliberately different
/// rules and why each caller needs its own.
pub(super) fn contains_word(low_window: &str, needle: &str, underscore_is_word_char: bool) -> bool {
    let is_word_char = |c: char| c.is_ascii_alphanumeric() || (underscore_is_word_char && c == '_');
    let mut start = 0;
    while let Some(rel) = low_window[start..].find(needle) {
        let abs = start + rel;
        let before_ok = abs == 0
            || low_window[..abs]
                .chars()
                .next_back()
                .is_none_or(|c| !is_word_char(c));
        let after_end = abs + needle.len();
        let after_ok = after_end >= low_window.len()
            || low_window[after_end..]
                .chars()
                .next()
                .is_none_or(|c| !is_word_char(c));
        if before_ok && after_ok {
            return true;
        }
        start = abs + needle.len().max(1);
    }
    false
}

/// Returns `true` when `low_window` contains the bare trigger word `needle`
/// as a standalone word, with underscore treated as a BOUNDARY (see
/// [`contains_word`]) — so `secret_key=…`/`auth_token=…`/`signing_key=…`
/// still match (on the `secret`/`auth`/`key` half), while pure letter-joined
/// collisions like `authorized`/`authentication`/`monkey`/`keyword` do not.
pub(super) fn contains_bounded_word(low_window: &str, needle: &str) -> bool {
    if needle != "key" {
        return contains_word(low_window, needle, false);
    }
    low_window
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|label| {
            if !contains_word(label, needle, false) {
                return false;
            }
            let end = label.as_ptr() as usize - low_window.as_ptr() as usize + label.len();
            let after = low_window[end..].trim_start_matches(is_assignment_label_gap);
            !(is_lookup_key_label(label) && after.starts_with([':', '=']))
        })
}
