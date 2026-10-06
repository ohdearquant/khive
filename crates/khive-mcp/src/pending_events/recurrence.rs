use super::*;

/// Compute the next `trigger_at` for a repeating event, given the current
/// `trigger_at` and the `repeat` spec.
///
/// Returns `None` for an absent, malformed, or exhausted repeat. Callers that
/// finalize a row must use `next_trigger_at_for_event` to distinguish a
/// one-shot from a stored recurrence that cannot advance.
pub(super) fn next_trigger_at(
    repeat: &Option<String>,
    current: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let repeat = khive_pack_schedule::repeat::parse_repeat(repeat.as_deref()?).ok()?;
    repeat.next_after(current)
}

pub(super) const INVALID_MONTHLY_ANCHOR: &str =
    "monthly repeat_anchor must be a valid timestamp no later than trigger_at";
const INVALID_STORED_REPEAT: &str = "stored repeat must be a string";
pub(super) const UNADVANCEABLE_REPEAT: &str = "stored repeat has no representable next occurrence";

fn is_monthly_repeat(repeat: &Option<String>) -> bool {
    repeat
        .as_deref()
        .is_some_and(|value| value.trim() == "monthly")
}

/// Each monthly candidate is computed from the original anchor, never from
/// the preceding (possibly clamped) trigger. A legacy row adopts its current
/// trigger in the same finalization that first advances it.
fn monthly_next_after(
    properties: &mut Value,
    current: DateTime<Utc>,
    bound: DateTime<Utc>,
) -> std::result::Result<Option<DateTime<Utc>>, &'static str> {
    let (anchor_text, legacy) = match properties.get("repeat_anchor") {
        Some(value) => (
            value.as_str().ok_or(INVALID_MONTHLY_ANCHOR)?.to_string(),
            false,
        ),
        None => (
            properties
                .get("trigger_at")
                .and_then(Value::as_str)
                .ok_or(INVALID_MONTHLY_ANCHOR)?
                .to_string(),
            true,
        ),
    };
    let anchor = anchor_text
        .parse::<DateTime<Utc>>()
        .map_err(|_| INVALID_MONTHLY_ANCHOR)?;
    if anchor > current || (legacy && anchor != current) {
        return Err(INVALID_MONTHLY_ANCHOR);
    }
    let next = khive_pack_schedule::repeat::Repeat::Monthly.first_after(anchor, bound);
    if next.is_some() && legacy {
        properties["repeat_anchor"] = json!(anchor_text);
    }
    Ok(next)
}

pub(super) fn next_trigger_at_for_event(
    properties: &mut Value,
    repeat: &Option<String>,
    current: DateTime<Utc>,
) -> std::result::Result<Option<DateTime<Utc>>, &'static str> {
    if repeat.is_none() {
        return if properties
            .get("repeat")
            .is_some_and(|value| !value.is_null())
        {
            Err(INVALID_STORED_REPEAT)
        } else {
            Ok(None)
        };
    }
    if is_monthly_repeat(repeat) {
        monthly_next_after(properties, current, current)?
            .map(Some)
            .ok_or(UNADVANCEABLE_REPEAT)
    } else {
        next_trigger_at(repeat, current)
            .map(Some)
            .ok_or(UNADVANCEABLE_REPEAT)
    }
}

/// Advance a missed repeating event's `trigger_at` past every occurrence at
/// or before `now`, landing on the first occurrence strictly after `now`
/// (ADR-106 missed-event amendment) — avoids firing a catch-up burst.
/// Returns `None` for an absent, malformed, or exhausted repeat. Callers that
/// finalize a row must use `advance_repeat_past_missed_for_event` to distinguish
/// a one-shot from a stored recurrence that cannot advance.
/// See `crates/khive-mcp/docs/api/pending-events.md` for the termination
/// argument.
pub(super) fn advance_repeat_past_missed(
    repeat: &Option<String>,
    current: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let repeat = khive_pack_schedule::repeat::parse_repeat(repeat.as_deref()?).ok()?;
    repeat.first_after(current, now)
}

pub(super) fn advance_repeat_past_missed_for_event(
    properties: &mut Value,
    repeat: &Option<String>,
    current: DateTime<Utc>,
    now: DateTime<Utc>,
) -> std::result::Result<Option<DateTime<Utc>>, &'static str> {
    if repeat.is_none() {
        return if properties
            .get("repeat")
            .is_some_and(|value| !value.is_null())
        {
            Err(INVALID_STORED_REPEAT)
        } else {
            Ok(None)
        };
    }
    if is_monthly_repeat(repeat) {
        monthly_next_after(properties, current, now)?
            .map(Some)
            .ok_or(UNADVANCEABLE_REPEAT)
    } else {
        advance_repeat_past_missed(repeat, current, now)
            .map(Some)
            .ok_or(UNADVANCEABLE_REPEAT)
    }
}

pub(super) fn reminder_delivery_action(actor: &str, content: &str) -> String {
    let action = json!([{
        "tool": "comm.send",
        "args": {
            "to": actor,
            "subject": reminder_subject(content),
            "content": content,
            "self_send": true,
        }
    }]);
    serde_json::to_string(&action).expect("reminder delivery action is JSON-serializable")
}

pub(super) fn reminder_subject(content: &str) -> String {
    const MAX_HEAD_CHARS: usize = 80;
    let collapsed = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let head: String = chars.by_ref().take(MAX_HEAD_CHARS).collect();
    if chars.next().is_some() {
        format!("[Reminder] {head}…")
    } else if head.is_empty() {
        "[Reminder]".to_string()
    } else {
        format!("[Reminder] {head}")
    }
}

pub(super) async fn append_reminder_delivery_failure_event(
    server: &KhiveMcpServer,
    namespace: &str,
    scheduled_event_id: uuid::Uuid,
    audit_actor: &str,
    recipient_actor: &str,
    error: &str,
) {
    let Some(store) = server.event_store() else {
        return;
    };
    let event = khive_storage::Event::new(
        namespace,
        "schedule.remind.fire",
        EventKind::Audit,
        SubstrateKind::Note,
        audit_actor,
    )
    .with_outcome(EventOutcome::Error)
    .with_target(scheduled_event_id)
    .with_payload(json!({
        "scheduled_event_id": scheduled_event_id,
        "recipient_actor": recipient_actor,
        "error": khive_runtime::secret_gate::bounded_masked_log_text(error),
    }));
    if let Err(trace_error) = store.append_event(event).await {
        tracing::error!(
            scheduled_event_id = %scheduled_event_id,
            error = %trace_error,
            "pending-events: reminder delivery failure event append failed"
        );
    }
}
