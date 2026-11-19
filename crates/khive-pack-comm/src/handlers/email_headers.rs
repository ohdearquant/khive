use super::{EmailMessageIdDomains, HashSet, Note, RuntimeError, Uuid, Value};

/// Candidate `$.external_id` values (as received, plus bracket-toggled) to
/// match an inbound correlation key against. See
/// crates/khive-pack-comm/docs/api/message-lifecycle.md#message-id--references-header-helpers-403
pub(super) fn message_id_match_candidates(corr: &str) -> Vec<String> {
    let bare = corr
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(corr);
    if bare == corr {
        vec![corr.to_string(), format!("<{corr}>")]
    } else {
        vec![corr.to_string(), bare.to_string()]
    }
}

pub(super) fn outbound_email_message(note: &Note) -> bool {
    let props = note.properties.as_ref();
    props
        .and_then(|p| p.get("direction"))
        .and_then(Value::as_str)
        == Some("outbound")
        && (props
            .and_then(|p| p.get("channel_kind"))
            .and_then(Value::as_str)
            == Some("email")
            || props
                .and_then(|p| p.get("channel_slug"))
                .and_then(Value::as_str)
                .is_some_and(|slug| slug.contains('@'))
            || ["to_actor", "to", "from_actor", "from"].iter().any(|key| {
                props
                    .and_then(|p| p.get(*key))
                    .and_then(Value::as_str)
                    .is_some_and(|actor| actor.starts_with("email:"))
            }))
}

pub(super) fn verified_outbound_email_external_id(
    note: &Note,
    domains: &Result<Option<EmailMessageIdDomains>, String>,
) -> bool {
    let Some(domains) = domains.as_ref().ok().and_then(Option::as_ref) else {
        return false;
    };
    let props = note.properties.as_ref();
    domains.verifies_channel_slug(
        props
            .and_then(|p| p.get("channel_slug"))
            .and_then(Value::as_str),
    ) && props
        .and_then(|p| p.get("external_id"))
        .and_then(Value::as_str)
        .is_some_and(|external_id| domains.verify(note.id, external_id))
}

pub(super) fn external_id_unverifiable(note_id: Uuid, reason: &str) -> RuntimeError {
    khive_types::KhiveError::invalid_input(format!(
        "external_id_unverifiable: outbound message {note_id}: {reason}"
    ))
    .with_details(khive_types::Details::new_owned([
        ("reason", "external_id_unverifiable".to_string()),
        ("note_id", note_id.to_string()),
    ]))
    .into()
}

/// Normalize a stored Message-ID into RFC 5322 wire form (angle-bracketed);
/// the single place that does so for `In-Reply-To`/`References` headers.
pub(super) fn wrap_message_id(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with('<') && trimmed.ends_with('>') {
        trimmed.to_string()
    } else {
        format!("<{trimmed}>")
    }
}

/// Resolve the parent message's wire Message-ID (issue #403), direction-aware:
/// outbound parents read `external_id`, inbound parents read `wire_message_id`
/// (never the reverse — `external_id` on an inbound note is the IMAP dedup
/// key, not a Message-ID). `None` when the parent has no wire Message-ID.
pub(super) fn parent_wire_message_id(orig_props: &Value) -> Option<String> {
    let direction = orig_props.get("direction").and_then(Value::as_str);
    let raw = if direction == Some("outbound") {
        orig_props.get("external_id").and_then(Value::as_str)
    } else {
        orig_props.get("wire_message_id").and_then(Value::as_str)
    }?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(wrap_message_id(trimmed))
    }
}

/// Resolve the parent message's own `References` chain, direction-aware
/// (inbound: `wire_references`; outbound: `references_chain`). `None` when
/// the parent has no chain to extend (RFC 5322: caller then falls back to the
/// parent's Message-ID alone). See
/// crates/khive-pack-comm/docs/api/message-lifecycle.md#message-id--references-header-helpers-403
pub(super) fn parent_references_chain(orig_props: &Value) -> Option<&str> {
    let direction = orig_props.get("direction").and_then(Value::as_str);
    let raw = if direction == Some("outbound") {
        orig_props.get("references_chain").and_then(Value::as_str)
    } else {
        orig_props.get("wire_references").and_then(Value::as_str)
    }?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Sanitize a single References/In-Reply-To token: reject anything containing
/// CR or LF (header injection guard) or without an `@` (not a plausible
/// message id), then normalize to wire form via [`wrap_message_id`].
///
/// Returns `None` for a malformed token so the caller can skip it rather than
/// emit a corrupt header.
pub(super) fn sanitize_reference_token(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains(['\r', '\n']) {
        return None;
    }
    let bare = trimmed
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(trimmed);
    if bare.is_empty() || !bare.contains('@') || bare.contains(['<', '>']) {
        return None;
    }
    Some(wrap_message_id(trimmed))
}

/// Strip angle brackets and surrounding whitespace from a wire-form message id,
/// for use as a de-duplication comparison key only -- callers keep pushing each
/// token's original serialization into the emitted header, never this bare form.
fn bare_reference_id(token: &str) -> String {
    let trimmed = token.trim();
    trimmed
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(trimmed)
        .to_string()
}

/// Build the full `References` header value for a reply: the parent's
/// existing chain (sanitized, malformed tokens skipped) followed by the
/// parent's own Message-ID, de-duplicated by bracket-stripped form
/// (first-seen order). `parent_message_id` is expected already wire-wrapped.
pub(super) fn build_references_header(
    parent_chain: Option<&str>,
    parent_message_id: &str,
) -> String {
    let chain_tokens = parent_chain
        .map(|chain| {
            chain
                .split_whitespace()
                .filter_map(sanitize_reference_token)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let mut tokens: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for token in chain_tokens
        .into_iter()
        .chain(std::iter::once(parent_message_id.to_string()))
    {
        if seen.insert(bare_reference_id(&token)) {
            tokens.push(token);
        }
    }
    tokens.join(" ")
}
