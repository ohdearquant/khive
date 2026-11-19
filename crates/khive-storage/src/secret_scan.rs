//! Pure credential detection, decoding, masking and safe match evidence.

mod bridge_fragments;
mod credential_triggers;
mod entropy;
mod escaped_json;
mod false_positive_filters;
mod known_patterns;
mod masking;
mod submitted_atom_digest;

use bridge_fragments::{
    bridge_fragment_chain, contains_bounded_word, contains_word, is_bridge_fragment_shape,
    trailing_bridge_fragment_cut,
};
use credential_triggers::{
    assignment_credential_trigger, build_match, extract_token, find_trigger,
    inline_credential_trigger, is_assignment_label_gap, is_base64_content_hash,
    is_credential_token_label, is_lookup_key_label, is_pure_hex, is_structured_identifier,
    is_uuid_canonical, shannon_entropy, strip_delimiters, value_candidates, wrapper_strip_repeated,
};
#[cfg(test)]
use credential_triggers::{LOOKUP_KEY_LABELS, POSITION_TOKEN_LABELS, RECEIPT_TOKEN_LABELS};
#[cfg(test)]
use entropy::TRIGGER_WINDOW;
use entropy::{
    check_entropy_heuristic, tokenize_entropy_tokens, EntropyScanContext, COMPOUND_TRIGGER_WORDS,
    ENTROPY_THRESHOLD, HEX_CREDENTIAL_LENGTHS, MAX_BRIDGE_FRAGMENTS, MAX_BRIDGE_GLUE_TOKENS,
    MIN_BRIDGE_FRAGMENT_LEN, MIN_ENTROPY_LEN, TRIGGER_WORDS,
};
use false_positive_filters::{
    after_last_sentence_boundary, before_first_sentence_boundary,
    has_clause_credential_label_with_inline, has_direct_repository_credential_label_with_inline,
    has_immediate_credential_label, is_aws_resource_name, is_environment_name,
    is_git_revision_reference, is_labeled_sha256_digest, is_latex_fragment_without_credential_run,
    is_latex_prose_macro, is_plausible_file_path, is_prose_code_reference,
    is_repository_revision_reference, is_vcs_marker_before_hex, normalized_hex_credential_span,
    ClauseValueKind,
};
#[cfg(test)]
use false_positive_filters::{is_clause_narrative_gerund, is_clause_narrative_participle};
use known_patterns::check_known_patterns;
#[cfg(doc)]
use known_patterns::find_url_userinfo;
#[cfg(test)]
use known_patterns::{find_prefix_token, is_filename_shaped_prefix_match, PREFIX_DETECTORS};
pub use masking::{
    bounded_masked_log_text, mask_bounded, mask_secrets, BoundedMask, MASK_WINDOW_CHARS,
};
#[cfg(test)]
use masking::{collect_mask_spans, MAX_LOG_TEXT_OUTPUT_CHARS, TRUNCATION_MARKER};
use masking::{extend_across_invisible_bridge, MAX_LOG_TEXT_MASK_INPUT_CHARS};

pub use submitted_atom_digest::masked_submitted_atom_digest_v1;

// ─── Public API ──────────────────────────────────────────────────────────────

/// Returned when a write would store credential-looking content.
///
/// Carries the detector name and a masked excerpt (`first6...Nchars`).  The
/// full candidate is never stored in the error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretMatch {
    /// Human-readable name of the detector that fired.
    pub detector: &'static str,
    /// Canonical trigger from the matched context; known-prefix rules need none.
    pub trigger: Option<&'static str>,
    /// `first6...N` — the first 6 chars of the match followed by the total length.
    pub masked: String,
    /// Which record and field the match came from. `None` only where the
    /// caller scanned exactly one string and nothing else, so there is one
    /// candidate. A single-record verb that scans several fields still needs
    /// this: the writer sees one refusal and cannot tell whether it was the
    /// name, the content, a tag or a property that matched.
    pub location: Option<String>,
}

impl std::fmt::Display for SecretMatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "content matches secret pattern {}", self.detector)?;
        if let Some(trigger) = self.trigger {
            write!(f, " near '{trigger}'")?;
        }
        if let Some(location) = &self.location {
            write!(f, " in {location}")?;
        }
        write!(f, ". {}", block_guidance(self.detector))
    }
}

/// Actionable, caller-visible guidance for a hard block, keyed by detector
/// name. If the content genuinely is a credential, remove it. If it
/// is not — the common case for the detectors below, which key off SHAPE
/// near a trigger word rather than a known credential prefix — the fix is to
/// break up the flagged token so it no longer reads as one contiguous
/// high-entropy value: separate it from words like key/secret/auth/token with
/// a sentence or paragraph boundary, or use an explicit repository revision
/// reference for a source hash.
fn block_guidance(detector: &'static str) -> &'static str {
    match detector {
        "url-userinfo" => {
            "Placeholders in URL credential positions still match this pattern. \
             Replace the whole URL with an environment-variable name or config key, \
             or remove the entire user/password segment before writing."
        }
        "high-entropy-token"
        | "uuid-near-trigger"
        | "content-hash-near-trigger"
        | "hex-credential-token" => {
            "If this is a real credential, remove it before writing. If it is not \
             (e.g. a file path, UUID, or hash that happens to sit near a word like \
             key/secret/auth/token), put the candidate in a separate sentence or \
             paragraph from those words, or express a source hash as an explicit \
             commit/revision reference."
        }
        "nested-escaped-json" => {
            "This value holds JSON text with escaped strings nested deeper than the \
             gate decodes. Store the inner document unescaped, or flatten the nesting, \
             before writing."
        }
        _ => {
            "If this is a real credential, remove it before writing; store secrets \
              in an env var or secrets manager instead."
        }
    }
}
/// Hard-block content from being written.
///
/// Returns `Err(SecretMatch)` on the first match found, or
/// `Ok(())` if no secret pattern fires.
pub fn check(content: &str) -> Result<(), SecretMatch> {
    check_decoded(content, 0)
}

/// JSON documents nested inside escaped string values deeper than this are
/// refused rather than passed unread.
const MAX_ESCAPED_JSON_DEPTH: usize = 8;

fn check_decoded(content: &str, depth: usize) -> Result<(), SecretMatch> {
    if let Some(m) = scan(content) {
        return Err(m);
    }
    for decoded in escaped_json::escaped_strings(content) {
        if depth == MAX_ESCAPED_JSON_DEPTH {
            return Err(build_match("nested-escaped-json", &decoded));
        }
        check_decoded(&decoded, depth + 1)?;
    }
    Ok(())
}
// ─── Scanner ─────────────────────────────────────────────────────────────────

/// Marker substituted for a detected secret span by [`mask_secrets`].
const REDACTION_MARKER: &str = "***MASKED***";

/// Maximum cumulative bytes revisited by the per-pass detector sweeps while masking
/// one input. Entropy tokens are materialized once, but resuming inside a token
/// rebuilds its member candidates and revisits their context, so that token's
/// full prefix is also charged. This permits two full-size passes over the 1 MiB
/// ASCII log-input case; the first pass is always allowed for larger or multibyte
/// callers. Once exhausted, the remainder is redacted wholesale.
const MAX_MASK_SCAN_WORK_BYTES: usize = MAX_LOG_TEXT_MASK_INPUT_CHARS * 2;

#[cfg(test)]
thread_local! {
    static ENTROPY_TOKENIZATION_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Return the LEFTMOST secret in `text` as `(matched_slice, detector)`.
///
/// The matched slice borrows from `text`, so the caller can recover its byte
/// span via pointer arithmetic — this is what lets [`mask_secrets`] redact in
/// place while [`scan`] only needs the masked excerpt.
///
/// "Leftmost" (smallest start offset), NOT first-by-detector-priority, is the
/// load-bearing contract: [`mask_secrets`] copies the text *before* each match
/// verbatim, so a non-leftmost match would leak an earlier secret detected by a
/// lower-priority detector (e.g. an `sk-ant-` key sitting to the left of a
/// `ghp_` token). Both detector layers are folded through [`keep_leftmost`].
#[cfg(test)]
fn scan_match(text: &str) -> Option<(&str, &'static str)> {
    let context = EntropyScanContext::new(text);
    scan_from(text, 0, &context)
}

/// Like [`scan_match`], but only returns secrets whose span starts at or after
/// `from`, while still evaluating Layer-2 trigger context against the FULL
/// `text`. [`mask_secrets`] calls this with an advancing `from` so that an
/// entropy token is detected even when its only trigger word sits to the left of
/// an already-redacted earlier secret. Layer-1 known patterns are context-free,
/// so scanning the `&text[from..]` suffix is equivalent; offsets recovered via
/// pointer arithmetic against the original `text` base stay absolute. The
/// pre-tokenized entropy view is shared by all passes.
fn scan_from<'a>(
    text: &'a str,
    from: usize,
    context: &EntropyScanContext<'a>,
) -> Option<(&'a str, &'static str)> {
    scan_from_with_trigger(text, from, context).map(|(slice, detector, _)| (slice, detector))
}

fn scan_from_with_trigger<'a>(
    text: &'a str,
    from: usize,
    context: &EntropyScanContext<'a>,
) -> Option<(&'a str, &'static str, Option<&'static str>)> {
    let mut best =
        check_known_patterns(&text[from..]).map(|(slice, detector)| (slice, detector, None));
    if let Some(candidate) = check_entropy_heuristic(text, from, context) {
        if best
            .as_ref()
            .is_none_or(|current| candidate.0.as_ptr() < current.0.as_ptr())
        {
            best = Some(candidate);
        }
    }
    best
}

/// Replace `best` with `cand` when `cand` starts earlier in the original text
/// (`base` is the start address of that text). On a tie the incumbent wins, so
/// callers offer more-specific detectors first. This is what makes
/// [`check_known_patterns`] and [`scan_match`] return the leftmost secret span
/// rather than the first detector that happens to match anywhere.
fn keep_leftmost<'a>(
    best: &mut Option<(&'a str, &'static str)>,
    cand: Option<(&'a str, &'static str)>,
    base: usize,
) {
    if let Some((slice, name)) = cand {
        let start = slice.as_ptr() as usize - base;
        let replace = match *best {
            Some((incumbent, _)) => start < (incumbent.as_ptr() as usize - base),
            None => true,
        };
        if replace {
            *best = Some((slice, name));
        }
    }
}

/// Return the first `SecretMatch` found in `text`, or `None`.
pub fn scan(text: &str) -> Option<SecretMatch> {
    let context = EntropyScanContext::new(text);
    scan_from_with_trigger(text, 0, &context).map(|(slice, detector, trigger)| {
        let mut matched = build_match(detector, slice);
        matched.trigger = trigger;
        matched
    })
}

#[cfg(test)]
#[path = "secret_scan/escaped_json_tests.rs"]
mod escaped_json_tests;
#[cfg(test)]
#[path = "secret_scan/issue_2655_tests.rs"]
mod issue_2655_tests;
#[cfg(test)]
#[path = "secret_scan/tests.rs"]
mod tests;

/// A named redact-not-block surface whose contract is intentionally separate
/// from manifest-backed write admission (ADR-115 Amendment 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionSurface {
    /// Git ingestion stores only masked commit, issue, and pull-request text.
    GitIngest,
    /// Session mirroring stores only masked `text` and `raw` projections.
    SessionMirror,
    /// Workspace mirroring stores masked document descriptions.
    WorkspaceMirror,
    /// MCP diagnostics are bounded caller-visible transport data, not records.
    McpDiagnostic,
    /// The kg `scan` verb's masked preview: caller-visible, never stored.
    GateProbe,
}

/// Whether a redaction surface can consume a secret-gate exemption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionSurfaceMode {
    /// Always apply the canonical masker; never synthesize a stamp or event.
    PermanentMaskOnly,
}

/// Machine-readable contract for a named redaction surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedactionSurfaceContract {
    pub mode: RedactionSurfaceMode,
    /// Durable target containing the masked result, if this is a write surface.
    pub final_stored_target: Option<&'static str>,
    /// Reserved stamp location; absent for permanent mask-only surfaces.
    pub stamp_property: Option<&'static str>,
    /// Atomic exemption-success event; absent when admission cannot occur.
    pub atomic_success_event: Option<&'static str>,
}

/// Final stored target for [`RedactionSurface::GitIngest`] — see
/// [`redaction_surface_contract`].
pub const GIT_INGEST_STORED_TARGET: &str = "final git-ingest entity/note fields";

/// Final stored target for [`RedactionSurface::SessionMirror`] — see
/// [`redaction_surface_contract`]. Names every column the session mirror
/// writes a masked provider-export projection into, not just the message
/// body columns. `cwd`/`git_branch` live only on `sessions`, keyed per
/// session — `session_messages` carries no such columns of its own (see the
/// `session_messages` DDL in `khive-pack-session`).
pub const SESSION_MIRROR_STORED_TARGET: &str =
    "session_messages.text, session_messages.raw, sessions.cwd, sessions.git_branch, \
     and sessions.slug";

/// Final stored target for [`RedactionSurface::WorkspaceMirror`] — see
/// [`redaction_surface_contract`]. The mirror passes masked text into the
/// document entity's `description`; source URI, type, and checksum are separate.
pub const WORKSPACE_MIRROR_STORED_TARGET: &str = "workspace mirror document.description";

/// Return the closed contract for a named redact-not-block surface.
pub const fn redaction_surface_contract(surface: RedactionSurface) -> RedactionSurfaceContract {
    let final_stored_target = match surface {
        RedactionSurface::GitIngest => Some(GIT_INGEST_STORED_TARGET),
        RedactionSurface::SessionMirror => Some(SESSION_MIRROR_STORED_TARGET),
        RedactionSurface::WorkspaceMirror => Some(WORKSPACE_MIRROR_STORED_TARGET),
        RedactionSurface::McpDiagnostic | RedactionSurface::GateProbe => None,
    };

    RedactionSurfaceContract {
        mode: RedactionSurfaceMode::PermanentMaskOnly,
        final_stored_target,
        stamp_property: None,
        atomic_success_event: None,
    }
}

/// Apply the canonical detector to a named permanent mask-only surface.
///
/// This wrapper makes the non-admission decision executable at each call site:
/// it has no manifest input, cannot return an exemption outcome, and cannot
/// synthesize the runtime-owned `khive:secret_gate` property or success event.
pub fn mask_for_redaction_surface(
    surface: RedactionSurface,
    text: &str,
) -> std::borrow::Cow<'_, str> {
    match redaction_surface_contract(surface).mode {
        RedactionSurfaceMode::PermanentMaskOnly => mask_secrets(text),
    }
}

/// Invalid or ambiguous coordinates in contextual detector evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OriginMappingError;

impl std::fmt::Display for OriginMappingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("secret scan origin mapping failed")
    }
}

impl std::error::Error for OriginMappingError {}

/// Read-only scalar attribution for one canonical JSON property document.
pub struct PropertyScanContext<'a> {
    canonical: &'a str,
    scalars: Vec<entropy::JsonScalarContext>,
}

impl<'a> PropertyScanContext<'a> {
    pub fn new(canonical: &'a str) -> Result<Self, OriginMappingError> {
        serde_json::from_str::<serde::de::IgnoredAny>(canonical).map_err(|_| OriginMappingError)?;
        Ok(Self {
            canonical,
            scalars: entropy::json_scalar_contexts(canonical),
        })
    }

    /// Scan one complete quoted key or value, retaining all of its view refusals.
    pub fn scan_atom(
        &self,
        quoted_span: std::ops::Range<usize>,
    ) -> Result<Vec<SecretMatch>, OriginMappingError> {
        let quoted = self
            .canonical
            .get(quoted_span.clone())
            .ok_or(OriginMappingError)?;
        if !quoted.starts_with('"') || !quoted.ends_with('"') || quoted.len() < 2 {
            return Err(OriginMappingError);
        }
        let original: String = serde_json::from_str(quoted).map_err(|_| OriginMappingError)?;
        let view = map_decoded_span(
            self.canonical,
            None,
            quoted_span.start + 1..quoted_span.end - 1,
            &original,
        )?;
        let label = self
            .scalars
            .binary_search_by_key(&(quoted_span.start + 1), |scalar| scalar.value_start)
            .ok()
            .map(|index| self.scalars[index])
            .filter(|scalar| scalar.end == quoted_span.end - 1)
            .and_then(|scalar| scalar.label);
        let mut matches = Vec::new();
        scan_origin_views(view, label, 0, &mut matches)?;
        Ok(matches)
    }
}

struct OriginView {
    text: String,
    origins: Vec<std::ops::Range<usize>>,
}

fn map_decoded_span(
    input: &str,
    parent: Option<&[std::ops::Range<usize>]>,
    span: std::ops::Range<usize>,
    expected: &str,
) -> Result<OriginView, OriginMappingError> {
    let source = input.get(span.clone()).ok_or(OriginMappingError)?;
    if parent.is_some_and(|origins| origins.len() != input.len()) {
        return Err(OriginMappingError);
    }
    let mut text = String::new();
    let mut origins = Vec::new();
    let mut cursor = 0;
    while cursor < source.len() {
        let start = cursor;
        let first = source[cursor..].chars().next().ok_or(OriginMappingError)?;
        let ch = if first != '\\' {
            if first == '"' || first < ' ' {
                return Err(OriginMappingError);
            }
            cursor += first.len_utf8();
            first
        } else {
            let tail = &source[cursor..];
            let (len, valid) = escaped_json::origin_token(tail);
            if !valid {
                return Err(OriginMappingError);
            }
            cursor += len;
            let literal = format!("\"{}\"", &source[start..cursor]);
            let decoded: String = serde_json::from_str(&literal).map_err(|_| OriginMappingError)?;
            let mut chars = decoded.chars();
            let ch = chars.next().ok_or(OriginMappingError)?;
            if chars.next().is_some() {
                return Err(OriginMappingError);
            }
            ch
        };
        let start = span.start + start;
        let end = span.start + cursor;
        let origin = if let Some(parent) = parent {
            let slice = parent.get(start..end).ok_or(OriginMappingError)?;
            let first = slice.first().ok_or(OriginMappingError)?;
            let last = slice.last().ok_or(OriginMappingError)?;
            if first.start >= first.end
                || last.start >= last.end
                || slice
                    .windows(2)
                    .any(|pair| pair[0].start > pair[1].start || pair[0].end > pair[1].end)
            {
                return Err(OriginMappingError);
            }
            first.start..last.end
        } else {
            start..end
        };
        if origins
            .last()
            .is_some_and(|prior: &std::ops::Range<usize>| {
                prior.start > origin.start || prior.end > origin.end
            })
        {
            return Err(OriginMappingError);
        }
        text.push(ch);
        origins.extend(std::iter::repeat_n(origin, ch.len_utf8()));
    }
    if text != expected || origins.len() != text.len() {
        return Err(OriginMappingError);
    }
    Ok(OriginView { text, origins })
}

fn scan_origin_views(
    view: OriginView,
    label: Option<(usize, &'static str)>,
    depth: usize,
    matches: &mut Vec<SecretMatch>,
) -> Result<(), OriginMappingError> {
    if let Some(matched) = scan(&view.text) {
        matches.push(matched);
    }
    if let Some(label) = label {
        let context = EntropyScanContext::with_owning(&view.text, &view.origins, label);
        if let Some((value, detector, trigger)) = check_entropy_heuristic(&view.text, 0, &context) {
            let mut matched = build_match(detector, value);
            matched.trigger = trigger;
            matches.push(matched);
        }
    }
    for decoded in escaped_json::escaped_views(&view.text) {
        let child = map_decoded_span(
            &view.text,
            Some(&view.origins),
            decoded.source,
            &decoded.text,
        )?;
        if depth == MAX_ESCAPED_JSON_DEPTH {
            matches.push(build_match("nested-escaped-json", &child.text));
        } else {
            scan_origin_views(child, label, depth + 1, matches)?;
        }
    }
    Ok(())
}
