use super::{
    is_valid_mailbox_actor_label, DateTime, FilterOp, HashSet, InboxParams, KhiveRuntime,
    NamespaceToken, Note, NoteFilter, PageRequest, PropertyFilter, RuntimeError, SqlValue, Utc,
    Uuid, Value,
};

/// Validate an actor label: non-empty, no control characters, ≤255 bytes (ADR-057 Q1 loose).
pub(super) fn validate_actor_label(
    verb: &str,
    label: &str,
    field: &str,
) -> Result<(), RuntimeError> {
    if label.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}: `{field}` must not be empty"
        )));
    }
    if label.len() > 255 {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}: `{field}` must not exceed 255 bytes"
        )));
    }
    if label.chars().any(|c| c.is_control()) {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}: `{field}` must not contain control characters"
        )));
    }
    Ok(())
}

pub(super) fn parse_inbox_timestamp(field: &str, raw: &str) -> Result<i64, RuntimeError> {
    khive_runtime::rfc3339_to_utc_micros(raw).map_err(|e| {
        RuntimeError::InvalidInput(format!(
            "inbox: `{field}` must be a valid RFC 3339 timestamp, got {raw:?}: {e}"
        ))
    })
}

/// Parse a caller- or transport-supplied thread root and return the one wire
/// spelling accepted by message-properties v1. `Uuid` deliberately accepts
/// compact, braced, URN, and upper-hex input forms; normalizing here keeps
/// those convenient inputs from leaking into the stored contract or splitting
/// SQL thread lookups, which compare the JSON string exactly.
pub(super) fn canonicalize_thread_id(verb: &str, raw: &str) -> Result<String, RuntimeError> {
    raw.trim()
        .parse::<Uuid>()
        .map(|id| id.as_hyphenated().to_string())
        .map_err(|_| {
            RuntimeError::InvalidInput(format!(
                "{verb}: `thread_id` must be a full UUID because a short prefix would require \
                 scoped resolution and a thread root is an explicit stable reference; got \
                 {raw:?}"
            ))
        })
}

/// Fail-closed resolution for a caller-supplied thread root (issue #1673):
/// shape validation alone accepts any UUID-shaped value, and an unresolvable
/// one strands the new message — `comm.thread` cannot reconstruct a thread
/// whose root row no live note points at, so the phantom send would succeed
/// silently while no reader could ever see the conversation whole. A supplied
/// root therefore has to resolve to at least one live `message` note in the
/// caller's namespace carrying that `thread_id` (probing the alternate
/// spellings a pre-v1 handler could have stored, exactly as thread lookup
/// does), or the send is rejected and nothing is persisted.
pub(super) async fn require_existing_thread_root(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    verb: &str,
    canonical_thread_id: &str,
) -> Result<(), RuntimeError> {
    let root_uuid = canonical_thread_id
        .parse::<Uuid>()
        .expect("canonicalize_thread_id produced this value from a parsed UUID");
    let spellings = thread_id_query_spellings(root_uuid, None)
        .into_iter()
        .map(SqlValue::Text)
        .collect();
    let filter = NoteFilter {
        kind: Some("message".to_string()),
        property_filters: vec![PropertyFilter {
            json_path: "$.thread_id".to_string(),
            op: FilterOp::In(spellings),
            value: SqlValue::Null,
        }],
        ..Default::default()
    };
    let store = runtime.notes(token)?;
    let page = store
        .query_notes_filtered_count_free(
            token.namespace().as_str(),
            &filter,
            PageRequest {
                limit: 1,
                offset: 0,
            },
        )
        .await?;
    if page.items.is_empty() {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}: `thread_id` {canonical_thread_id:?} does not resolve to an existing \
             thread: no live message in this namespace carries that thread_id. Refusing to \
             strand the message on a phantom thread -- omit `thread_id` to \
             start a new thread, or pass the `full_id` of an existing message (see comm.thread)."
        )));
    }
    Ok(())
}

pub(super) fn validate_inbox_substring(
    field: &str,
    value: Option<&str>,
) -> Result<(), RuntimeError> {
    if value.is_some_and(|raw| raw.trim().is_empty()) {
        return Err(RuntimeError::InvalidInput(format!(
            "inbox: `{field}` must not be empty"
        )));
    }
    Ok(())
}

/// Derive the `thread_id` a `comm.send` response reports from the persisted
/// outbound note. A present, non-empty stored value is authoritative. An
/// empty stored value is treated exactly like a missing one: it is only
/// honest to fall back to the note's own UUID when the caller did NOT supply
/// a thread root (the note genuinely IS the new root). When the caller
/// supplied one, a missing or empty stored value means the write did not
/// persist the requested root, and silently reporting the note UUID would
/// route any continuation send into a NEW thread instead of the caller's.
pub(super) fn send_response_thread_id(
    supplied_thread_id: Option<&str>,
    outbound_note: &Note,
) -> Result<String, RuntimeError> {
    let stored_thread_id = outbound_note
        .properties
        .as_ref()
        .and_then(|properties| properties.get("thread_id"))
        .and_then(Value::as_str)
        .filter(|raw| !raw.is_empty())
        .map(str::to_owned);
    match stored_thread_id {
        Some(value) => Ok(value),
        None if supplied_thread_id.is_some() => Err(RuntimeError::Internal(format!(
            "send: outbound note {} was persisted without the caller-supplied thread_id \
             {supplied_thread_id:?}; refusing to report the note's own UUID as the thread \
             root because a continuation send would silently root a new thread",
            outbound_note.id
        ))),
        None => Ok(outbound_note.id.as_hyphenated().to_string()),
    }
}

pub(super) fn inbox_note_matches(
    note: &Note,
    params: &InboxParams,
    before_micros: Option<i64>,
    subject_needle: Option<&str>,
    content_needle: Option<&str>,
) -> bool {
    let props = note.properties.as_ref();
    if params.kind.as_deref().is_some_and(|kind| note.kind != kind) {
        return false;
    }
    if params.tags.as_ref().is_some_and(|tags| {
        tags.iter().any(|tag| {
            !props
                .and_then(|properties| properties.get("tags"))
                .and_then(Value::as_array)
                .is_some_and(|stored| stored.iter().any(|value| value.as_str() == Some(tag)))
        })
    }) {
        return false;
    }
    let sender = props
        .and_then(|properties| properties.get("from_actor"))
        .and_then(Value::as_str);

    if params
        .from_prefix
        .as_deref()
        .is_some_and(|prefix| !sender.is_some_and(|value| value.starts_with(prefix)))
    {
        return false;
    }
    if params
        .exclude_from_actor
        .as_deref()
        .is_some_and(|excluded| sender == Some(excluded))
    {
        return false;
    }
    if before_micros.is_some_and(|before| note.created_at >= before) {
        return false;
    }
    if subject_needle.is_some_and(|needle| {
        !props
            .and_then(|properties| properties.get("subject"))
            .and_then(Value::as_str)
            .is_some_and(|subject| subject.to_lowercase().contains(needle))
    }) {
        return false;
    }
    if content_needle.is_some_and(|needle| !note.content.to_lowercase().contains(needle)) {
        return false;
    }

    true
}

/// Return the exact, indexable spellings a pre-v1 handler could have stored
/// for one UUID root. Before v1, valid caller input was persisted verbatim
/// after `Uuid` parsing, so compact, braced, URN, and upper-hex formatter
/// outputs may coexist with the canonical lower-case hyphenated value.
///
/// `selected_raw` retains an arbitrary mixed-case spelling from the row the
/// caller selected. The common lower/upper formatter outputs cover rows other
/// than that selected row without falling back to a namespace-wide scan.
pub(super) fn thread_id_query_spellings(root: Uuid, selected_raw: Option<&str>) -> Vec<String> {
    let mut spellings = vec![
        root.as_hyphenated().to_string(),
        root.simple().to_string(),
        root.braced().to_string(),
        root.urn().to_string(),
        format!("{:X}", root.as_hyphenated()),
        format!("{:X}", root.simple()),
        format!("{:X}", root.braced()),
        format!("{:X}", root.urn()),
    ];
    if let Some(raw) = selected_raw.map(str::trim).filter(|raw| !raw.is_empty()) {
        spellings.push(raw.to_string());
    }

    let mut seen = HashSet::new();
    spellings.retain(|spelling| seen.insert(spelling.clone()));
    spellings
}

/// Parse a caller-supplied timestamp, rejecting anything that does not
/// resolve to an instant, with the verb and field named in the error.
/// Callers own the serialization policy on the parsed value: ingest
/// re-serializes in UTC, heartbeat preserves the supplied spelling.
pub(super) fn parse_supplied_timestamp(
    verb: &str,
    field: &str,
    raw: &str,
) -> Result<DateTime<chrono::FixedOffset>, RuntimeError> {
    DateTime::parse_from_rfc3339(raw.trim()).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "{verb}: `{field}` must be a valid RFC 3339 timestamp, got {raw:?}: {error}"
        ))
    })
}

/// Validate an adapter timestamp before it can be certified as a v1 `sent_at`
/// value, then serialize the instant in one RFC 3339 representation (UTC).
pub(super) fn canonicalize_ingest_sent_at(raw: &str) -> Result<String, RuntimeError> {
    parse_supplied_timestamp("ingest", "sent_at", raw)
        .map(|timestamp| timestamp.with_timezone(&Utc).to_rfc3339())
}

pub(super) fn caller_inherits_legacy_pool(token: &NamespaceToken) -> bool {
    token.actor().is_anonymous() && token.actor().id == "local"
}

pub(super) fn legacy_recipient(properties: Option<&Value>) -> bool {
    properties
        .and_then(|properties| properties.get("to_actor"))
        .is_none_or(Value::is_null)
}

pub(super) fn addressed_recipient(properties: Option<&Value>) -> Option<&str> {
    properties
        .and_then(|properties| properties.get("to_actor"))
        .and_then(Value::as_str)
        .filter(|recipient| *recipient == "local" || is_valid_mailbox_actor_label(recipient))
}

pub(super) fn caller_is_addressee(token: &NamespaceToken, properties: Option<&Value>) -> bool {
    addressed_recipient(properties) == Some(token.actor().id.as_str())
        || (caller_inherits_legacy_pool(token) && legacy_recipient(properties))
}
