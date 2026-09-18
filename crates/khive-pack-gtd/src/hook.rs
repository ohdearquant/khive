//! `TaskHook` — gtd's per-kind specialization for the `task` note kind.
//!
//! Implements the `KindHook` extension point for the pack standard. Normalises
//! user-facing GTD fields into the kg storage shape on `prepare_create`, keeps
//! task body mirrors aligned on `prepare_note_update`, and creates `depends_on`
//! graph edges on `after_create` (best-effort). GTD lifecycle semantics are
//! documented in `docs/design.md`.

use async_trait::async_trait;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, KindHook, LinkSpec, Namespace, NamespaceToken, RuntimeError};
use khive_storage::Note;

use crate::handlers::parse_due;
use crate::schema::{is_valid_priority, priority_to_salience};
use crate::task_create::{link_depends_on_edges, prepare_task_create, TaskCreateInput};

#[derive(Debug, Default)]
/// KindHook implementation for the `task` note kind; normalises GTD fields on create.
pub struct TaskHook;

fn stored_content_is_title_fallback(note: &Note) -> bool {
    let effective_title = note.name.as_deref().map(str::trim).unwrap_or_default();
    note.content.trim().is_empty() || note.content.trim() == effective_title
}

fn synchronize_description(note: &Note, args: &mut Value) -> Result<(), RuntimeError> {
    let root = args
        .as_object_mut()
        .ok_or_else(|| RuntimeError::InvalidInput("update args must be an object".into()))?;

    if let Some(Value::Object(properties)) = root.get("properties") {
        for field in ["status", "completed_at", "transition_history"] {
            if properties.contains_key(field) {
                return Err(RuntimeError::InvalidInput(format!(
                    "properties.{field} is lifecycle-owned and cannot be patched on a task; use \
                     gtd.transition for lifecycle changes or gtd.complete for terminal completion"
                )));
            }
        }
        for field in ["blocked_by", "dependency_state", "actionable"] {
            if properties.contains_key(field) {
                return Err(RuntimeError::InvalidInput(format!(
                    "properties.{field} is derived from task dependencies and cannot be patched on a task; update properties.depends_on to change blockers"
                )));
            }
        }
    }

    let content_patch = root
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_string);
    let name_patch = match root.get("name") {
        None => None,
        Some(Value::String(name)) if name.trim().is_empty() => {
            return Err(RuntimeError::InvalidInput(
                "task title must not be empty".into(),
            ));
        }
        Some(Value::String(name)) => Some(name.clone()),
        Some(Value::Null) => {
            return Err(RuntimeError::InvalidInput(
                "task title cannot be cleared; `name` must be a non-empty string".into(),
            ));
        }
        Some(other) => {
            return Err(RuntimeError::InvalidInput(format!(
                "task update field `name` must be a string; got {other}"
            )));
        }
    };
    let description_patch = match root.get("properties") {
        None | Some(Value::Null) => None,
        Some(Value::Object(properties)) => match properties.get("description") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::String(description)) => Some(Some(description.clone())),
            Some(other) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "properties.description must be a string or null; got {other}"
                )))
            }
        },
        Some(other) => {
            return Err(RuntimeError::InvalidInput(format!(
                "properties on a `task` note must be patched with an object; got {other}"
            )))
        }
    };

    match (content_patch, description_patch) {
        (Some(content), Some(Some(description))) if content != description => {
            return Err(RuntimeError::InvalidInput(
                "task update fields `content` and `properties.description` must match when both are supplied"
                    .into(),
            ));
        }
        (Some(_), Some(None)) => {
            return Err(RuntimeError::InvalidInput(
                "task update cannot set `content` while clearing `properties.description`".into(),
            ));
        }
        (Some(content), _) => {
            if root.get("properties").is_none_or(Value::is_null) {
                root.insert("properties".into(), json!({}));
            }
            let properties = root
                .get_mut("properties")
                .expect("properties was inserted")
                .as_object_mut()
                .expect("properties was validated as object or inserted as object");
            properties.insert("description".into(), json!(content));
        }
        (None, Some(Some(description))) => {
            root.insert("content".into(), json!(description));
        }
        (None, Some(None)) => {
            let stored_description_exists = note
                .properties
                .as_ref()
                .and_then(|properties| properties.get("description"))
                .is_some_and(|description| !description.is_null());
            let should_write_title_fallback =
                stored_description_exists || stored_content_is_title_fallback(note);
            if should_write_title_fallback {
                let effective_title = name_patch
                    .as_deref()
                    .or(note.name.as_deref())
                    .filter(|title| !title.trim().is_empty())
                    .ok_or_else(|| {
                        RuntimeError::InvalidInput("task title must not be empty".into())
                    })?;
                root.insert("content".into(), json!(effective_title));
            }
        }
        (None, None)
            if name_patch.is_some()
                && note
                    .properties
                    .as_ref()
                    .and_then(|properties| properties.get("description"))
                    .and_then(Value::as_str)
                    .is_none()
                && stored_content_is_title_fallback(note) =>
        {
            root.insert(
                "content".into(),
                json!(name_patch.as_deref().expect("name patch is present")),
            );
        }
        (None, None) => {}
    }

    Ok(())
}

/// Resolve the zone a `due` arriving through the generic update path is anchored in.
///
/// Order: the zone named in this update, then the anchor the task already carries, then the
/// configured display zone. A zone named by the caller must parse — that is their input and a
/// silent fallback would store an anchor they did not ask for. A stored anchor that does not parse
/// is a row written before this normalization existed, so it falls through to the configured zone
/// rather than failing an update that is repairing it.
fn resolve_due_zone(
    runtime: &KhiveRuntime,
    note: &Note,
    properties: &serde_json::Map<String, Value>,
) -> Result<chrono_tz::Tz, RuntimeError> {
    // Select on the value, not on the key. Choosing the key first and calling `as_str` on the
    // result treats a present-but-malformed zone as absent: `{due_timezone: 7, timezone:
    // "Asia/Tokyo"}` would discard the spelling that parses and fall through to the stored or
    // configured zone, storing an anchor nobody asked for. A zone the caller wrote is either used
    // or refused.
    for key in ["due_timezone", "timezone"] {
        let Some(value) = properties.get(key).filter(|value| !value.is_null()) else {
            continue;
        };
        let name = value.as_str().ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "{key} must be an IANA zone name string (e.g. \"America/New_York\"); got {value}"
            ))
        })?;
        return name.parse::<chrono_tz::Tz>().map_err(|_| {
            RuntimeError::InvalidInput(format!(
                "timezone must be an IANA zone name (e.g. \"America/New_York\"); got {name:?}"
            ))
        });
    }
    let stored = note
        .properties
        .as_ref()
        .and_then(|value| value.get("due_timezone"))
        .and_then(Value::as_str)
        .and_then(|name| name.parse::<chrono_tz::Tz>().ok());
    Ok(stored.unwrap_or_else(|| runtime.config().display_timezone))
}

/// Normalize a `due` written through the generic property path into the shape `gtd.assign`
/// produces, and rewrite `due_timezone` beside it.
///
/// `due` has a validating writer (`gtd.assign`) and, before this, a silent one: `properties` is a
/// free-form map, so a reschedule through the generic update stored whatever string arrived, next
/// to whichever `due_timezone` the create had left. The row then claimed an anchor its value did
/// not carry, and a normalized row and an un-normalized one were indistinguishable by shape,
/// because the field beside the value still looked right. Both writers now run the same
/// normalization, so there is one stored shape rather than two.
fn normalize_due_update(
    runtime: &KhiveRuntime,
    note: &Note,
    args: &mut Value,
) -> Result<(), RuntimeError> {
    let Some(properties) = args.get_mut("properties").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    let Some(due) = properties.get("due") else {
        return Ok(());
    };
    // An explicit null clears the deadline. The anchor is derived state, so it goes with it:
    // leaving it standing is the stale-anchor case this function exists to end.
    if due.is_null() {
        properties.insert("due_timezone".into(), Value::Null);
        return Ok(());
    }
    let due = due
        .as_str()
        .ok_or_else(|| {
            RuntimeError::InvalidInput(format!("due must be an ISO-8601 string or null; got {due}"))
        })?
        .to_string();
    let zone = resolve_due_zone(runtime, note, properties)?;
    // `timezone` is the spelling `gtd.assign` takes, and a caller who learned it there will use it
    // here. Accepting it and then storing it would leave a junk property that no reader consumes
    // and that the create path never writes, so it is consumed rather than kept: the anchor lands
    // in `due_timezone`, which is the stored name.
    properties.remove("timezone");
    properties.insert("due".into(), json!(parse_due(&due, zone)?));
    properties.insert("due_timezone".into(), json!(zone.name()));
    Ok(())
}

/// Validate a `priority` written through the generic property path and re-derive the note's
/// `salience` beside it.
///
/// `priority` is the same shape of defect as `due` above, with one difference that makes it worse:
/// no gtd verb can change a priority at all, so the free-form property path is not a second writer
/// but the only one. `gtd.assign` checks the value against `p0..p3`, stores it lowercase, and sets
/// `salience` from it; the property path did none of the three. A nonsense priority was therefore
/// storable but unreachable, since `gtd.tasks(priority=..)` validates its filter against the same
/// list, and an escalated task kept ranking at the salience its create had written.
///
/// A caller who names `salience` in the same update keeps their value. That is the same rule as
/// the zone above: what the caller states explicitly wins over what is derived for them.
fn normalize_priority_update(args: &mut Value) -> Result<(), RuntimeError> {
    let Some(root) = args.as_object_mut() else {
        return Ok(());
    };
    // Presence, not truthiness: `salience: null` is the tri-state contract's explicit clear
    // (key absent -> untouched, null -> clear, number -> set), so reading null as "the caller said
    // nothing" would overwrite a clear with the derived value and silently ignore the instruction.
    let caller_set_salience = root.contains_key("salience");
    let Some(priority) = root
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get("priority"))
    else {
        return Ok(());
    };

    // An explicit null clears the priority. `gtd.assign` gives a task with no priority a salience
    // of 0.5, so that is what a cleared one falls back to rather than keeping the old level's.
    let canonical = if priority.is_null() {
        None
    } else {
        let named = priority.as_str().ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "priority must be one of p0, p1, p2, p3, or null; got {priority}"
            ))
        })?;
        let canonical = named.to_ascii_lowercase();
        if !is_valid_priority(&canonical) {
            return Err(RuntimeError::InvalidInput(format!(
                "invalid priority {named:?} — valid: p0, p1, p2, p3"
            )));
        }
        Some(canonical)
    };

    let salience = canonical
        .as_deref()
        .map(priority_to_salience)
        .unwrap_or(0.5);
    if let Some(canonical) = canonical {
        if let Some(properties) = root.get_mut("properties").and_then(Value::as_object_mut) {
            properties.insert("priority".into(), json!(canonical));
        }
    }
    if !caller_set_salience {
        root.insert("salience".into(), json!(salience));
    }
    Ok(())
}

#[async_trait]
impl KindHook for TaskHook {
    async fn prepare_create(
        &self,
        runtime: &KhiveRuntime,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        let token = args
            .get("namespace")
            .and_then(Value::as_str)
            .and_then(|s| Namespace::parse(s).ok())
            .map(|ns| runtime.authorize(ns))
            .unwrap_or_else(|| runtime.authorize(Namespace::local()))?;

        // #625/#626: this generic `create(kind="note", note_kind="task")`
        // entry point and `gtd.assign` (`GtdPack::handle_assign` in
        // handlers.rs) both normalize/validate through
        // `task_create::prepare_task_create` so status/priority checks,
        // `depends_on` resolution, and `context_entity_id` handling can't
        // drift between the two paths again.
        let input = TaskCreateInput::from_create_args(args)?;
        let prepared = prepare_task_create(runtime, &token, input).await?;
        prepared.apply_to_create_args(args)?;

        Ok(())
    }

    async fn after_create(
        &self,
        runtime: &KhiveRuntime,
        id: Uuid,
        args: &Value,
    ) -> Result<(), RuntimeError> {
        let token = args
            .get("namespace")
            .and_then(Value::as_str)
            .and_then(|s| Namespace::parse(s).ok())
            .map(|ns| runtime.authorize(ns))
            .unwrap_or_else(|| runtime.authorize(Namespace::local()))?;

        if let Some(properties) = args.get("properties") {
            link_depends_on_edges(runtime, &token, id, properties, "task hook").await;
        }

        Ok(())
    }

    async fn validate_note_update(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        note: &Note,
        properties: Option<&Value>,
    ) -> Result<(), RuntimeError> {
        crate::dependency::validate_property_update(runtime, token, note, properties).await
    }

    async fn prepare_note_update(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        note: &Note,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        synchronize_description(note, args)?;
        normalize_due_update(runtime, note, args)?;
        normalize_priority_update(args)?;
        let properties = args.get("properties").filter(|value| !value.is_null());
        crate::dependency::validate_property_update(runtime, token, note, properties).await
    }

    async fn validate_links(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        links: &[LinkSpec],
    ) -> Result<(), RuntimeError> {
        crate::dependency::validate_dependency_links(runtime, token, links).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    fn task_note_with(properties: Value) -> Note {
        let mut task = Note::new("local", "task", "body");
        task.name = Some("a task".to_string());
        task.properties = Some(properties);
        task
    }

    /// A reschedule through the generic property path lands in the same shape `gtd.assign`
    /// writes, and keeps the anchor the task already had rather than the host's.
    #[tokio::test]
    async fn due_through_properties_is_anchored_in_the_tasks_existing_zone() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let note = task_note_with(json!({
            "due": "2026-10-01T00:00:00-04:00",
            "due_timezone": "America/New_York",
            "status": "inbox",
        }));
        let mut args = json!({"properties": {"due": "2026-12-25"}});

        normalize_due_update(&runtime, &note, &mut args).expect("normalize");

        assert_eq!(
            args["properties"]["due"], "2026-12-25T00:00:00-05:00",
            "a date-only due must be anchored to the earliest instant of that local date"
        );
        assert_eq!(
            args["properties"]["due_timezone"], "America/New_York",
            "the anchor must be rewritten beside the value, not left from the create"
        );
    }

    /// The zone named in the update wins over the one the task carries. Only the `due` assertion
    /// below discriminates: zone names parse case-sensitively, so a zone named in this spelling is
    /// stored back as the same string the caller sent, and the anchor assertion would hold even if
    /// nothing rewrote it. The rewrite itself is proved by the `timezone`-spelling test, where the
    /// stored anchor is a value no caller supplied.
    #[tokio::test]
    async fn a_zone_named_in_the_update_wins_over_the_stored_anchor() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let note = task_note_with(json!({"due_timezone": "America/New_York"}));
        let mut args = json!({
            "properties": {"due": "2026-12-25", "due_timezone": "Asia/Tokyo"}
        });

        normalize_due_update(&runtime, &note, &mut args).expect("normalize");

        assert_eq!(args["properties"]["due"], "2026-12-25T00:00:00+09:00");
        assert_eq!(args["properties"]["due_timezone"], "Asia/Tokyo");
    }

    /// Both halves of the pair are refused when they cannot be parsed. Before this, each was
    /// stored verbatim: `due` as "next tuesday-ish" and the zone as "Mars/Olympus".
    #[tokio::test]
    async fn an_unparseable_due_or_zone_is_refused() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let note = task_note_with(json!({"status": "inbox"}));

        let mut bad_due = json!({"properties": {"due": "next tuesday-ish"}});
        normalize_due_update(&runtime, &note, &mut bad_due)
            .expect_err("an unparseable due must not be stored verbatim");

        let mut bad_zone = json!({
            "properties": {"due": "2026-12-25", "due_timezone": "Mars/Olympus"}
        });
        let err = normalize_due_update(&runtime, &note, &mut bad_zone)
            .expect_err("an unparseable zone must not be stored verbatim");
        assert!(
            format!("{err}").contains("IANA"),
            "the refusal must name what a zone is; got: {err}"
        );

        let mut wrong_type = json!({"properties": {"due": 20261225}});
        normalize_due_update(&runtime, &note, &mut wrong_type)
            .expect_err("a non-string, non-null due must be refused");
    }

    /// Clearing the deadline clears its anchor, since the anchor is derived state. A surviving
    /// `due_timezone` beside an absent `due` is the same stale-pair defect in its other direction.
    #[tokio::test]
    async fn clearing_due_clears_the_anchor_with_it() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let note = task_note_with(json!({
            "due": "2026-10-01T00:00:00-04:00",
            "due_timezone": "America/New_York",
        }));
        let mut args = json!({"properties": {"due": null}});

        normalize_due_update(&runtime, &note, &mut args).expect("normalize");

        assert!(args["properties"]["due"].is_null());
        assert!(
            args["properties"]["due_timezone"].is_null(),
            "the anchor must not outlive the value it anchors"
        );
    }

    /// An update that does not mention `due` leaves the pair untouched, including on a task that
    /// has none: the normalizer must not mint fields nobody asked for.
    #[tokio::test]
    async fn an_update_without_due_touches_neither_field() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let note = task_note_with(json!({"due": "2026-10-01T00:00:00-04:00"}));
        let mut args = json!({"properties": {"priority": "p1"}});

        normalize_due_update(&runtime, &note, &mut args).expect("normalize");

        assert_eq!(args, json!({"properties": {"priority": "p1"}}));

        let mut no_properties = json!({"content": "body only"});
        normalize_due_update(&runtime, &note, &mut no_properties).expect("normalize");
        assert_eq!(no_properties, json!({"content": "body only"}));
    }

    /// A row written before this normalization can carry an anchor that is not a zone. That is not
    /// the caller's input, so the update repairs it instead of failing on it.
    #[tokio::test]
    async fn a_stored_anchor_that_is_not_a_zone_falls_back_instead_of_failing() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let note =
            task_note_with(json!({"due": "next tuesday-ish", "due_timezone": "Mars/Olympus"}));
        let mut args = json!({"properties": {"due": "2026-12-25"}});

        normalize_due_update(&runtime, &note, &mut args)
            .expect("a bad stored anchor must not fail the repair");

        let configured = runtime.config().display_timezone.name().to_string();
        assert_eq!(args["properties"]["due_timezone"], configured);
        assert_ne!(args["properties"]["due"], "2026-12-25");
    }

    /// The `timezone` spelling from `gtd.assign` is accepted here and consumed rather than stored,
    /// since nothing reads it and the create path never writes it.
    #[tokio::test]
    async fn the_assign_spelling_of_the_zone_is_consumed_not_stored() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let note = task_note_with(json!({"due_timezone": "America/New_York"}));
        let mut args = json!({
            "properties": {"due": "2026-12-25", "timezone": "Asia/Tokyo"}
        });

        normalize_due_update(&runtime, &note, &mut args).expect("normalize");

        assert_eq!(args["properties"]["due"], "2026-12-25T00:00:00+09:00");
        assert_eq!(args["properties"]["due_timezone"], "Asia/Tokyo");
        assert!(
            args["properties"].get("timezone").is_none(),
            "the assign spelling must not survive into the stored bag; got: {}",
            args["properties"]
        );
    }

    /// A zone the caller wrote is used or refused, never ignored. Selecting the key before
    /// checking that its value is a string made a malformed `due_timezone` read as absent, which
    /// discarded a `timezone` alias that did parse and stored an anchor nobody asked for.
    #[tokio::test]
    async fn a_malformed_zone_is_refused_rather_than_read_as_absent() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let note = task_note_with(json!({"due_timezone": "America/New_York"}));

        let mut both = json!({
            "properties": {"due": "2026-12-25", "due_timezone": 7, "timezone": "Asia/Tokyo"}
        });
        let err = normalize_due_update(&runtime, &note, &mut both)
            .expect_err("a non-string zone must be refused, not skipped for the next spelling");
        assert!(
            format!("{err}").contains("due_timezone"),
            "the refusal must name the key the caller got wrong; got: {err}"
        );
        assert_eq!(
            both["properties"]["due"], "2026-12-25",
            "a refused update must leave the caller's arguments untouched"
        );

        // An explicit null is absence, not a malformed value: it falls through to the stored
        // anchor the way an omitted key does.
        let mut nulled = json!({
            "properties": {"due": "2026-12-25", "due_timezone": null}
        });
        normalize_due_update(&runtime, &note, &mut nulled).expect("a null zone reads as absent");
        assert_eq!(nulled["properties"]["due_timezone"], "America/New_York");
    }

    /// The wiring, not just the function: the hook the generic update path calls must run it.
    #[tokio::test]
    async fn the_update_hook_runs_the_normalization() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let token = runtime
            .authorize(Namespace::local())
            .expect("authorize local");
        let note = task_note_with(json!({
            "description": "body",
            "status": "inbox",
            "due_timezone": "America/New_York",
        }));
        let mut args = json!({"properties": {"due": "2026-12-25"}});

        TaskHook
            .prepare_note_update(&runtime, &token, &note, &mut args)
            .await
            .expect("hook");

        assert_eq!(args["properties"]["due"], "2026-12-25T00:00:00-05:00");
        assert_eq!(args["properties"]["due_timezone"], "America/New_York");
    }

    /// The core of #2925: `salience` is derived from `priority` at create and feeds ranking, and
    /// the property path used to change one without the other. An escalated task kept the level
    /// its create had written.
    #[tokio::test]
    async fn an_escalated_priority_rewrites_the_salience_beside_it() {
        // No note fixture: the level a task currently holds does not enter this decision. The
        // salience follows the level named in THIS update, which is what makes the escalation
        // land rather than being merged against a stored value.
        let mut args = json!({"properties": {"priority": "p0"}});

        normalize_priority_update(&mut args).expect("normalize");

        assert_eq!(args["properties"]["priority"], "p0");
        assert_eq!(
            args["salience"], 1.0,
            "the ranking value must follow the level, not the create"
        );
    }

    /// `gtd.assign` stores the level lowercase. The property path now agrees, so one task cannot
    /// hold a spelling that its own create path would never have written.
    #[tokio::test]
    async fn a_priority_is_stored_in_the_spelling_the_create_path_uses() {
        let mut args = json!({"properties": {"priority": "P1"}});

        normalize_priority_update(&mut args).expect("normalize");

        assert_eq!(args["properties"]["priority"], "p1");
        assert_eq!(args["salience"], 0.75);
    }

    /// A value outside p0..p3 was storable and then unreachable, because `gtd.tasks(priority=..)`
    /// validates its filter against the same list the create path validates writes against.
    #[tokio::test]
    async fn a_priority_outside_the_levels_is_refused_rather_than_stored() {
        let mut nonsense = json!({"properties": {"priority": "URGENT!!"}});
        let err = normalize_priority_update(&mut nonsense)
            .expect_err("a level outside p0..p3 must not be stored verbatim");
        assert!(
            format!("{err}").contains("p0, p1, p2, p3"),
            "the refusal must name the levels; got: {err}"
        );

        let mut wrong_type = json!({"properties": {"priority": 0}});
        normalize_priority_update(&mut wrong_type)
            .expect_err("a non-string, non-null priority must be refused");
    }

    /// Clearing the level lands on the salience `gtd.assign` gives a task created without one,
    /// rather than leaving the cleared level's value behind as derived state with nothing to
    /// derive from.
    #[tokio::test]
    async fn clearing_the_priority_falls_back_to_the_unprioritized_salience() {
        let mut args = json!({"properties": {"priority": null}});

        normalize_priority_update(&mut args).expect("normalize");

        assert_eq!(args["salience"], 0.5);
    }

    /// The caller's own value wins over the derived one, which is the same rule the zone above
    /// follows: what a caller states explicitly beats what is computed for them.
    #[tokio::test]
    async fn a_caller_named_salience_survives_the_derivation() {
        let mut args = json!({"properties": {"priority": "p0"}, "salience": 0.1});

        normalize_priority_update(&mut args).expect("normalize");

        assert_eq!(args["properties"]["priority"], "p0");
        assert_eq!(args["salience"], 0.1);
    }

    /// `salience: null` is the tri-state contract's explicit clear, so a caller who sends it
    /// beside a priority is asking for the level without the derived ranking. Reading null as
    /// silence would overwrite the clear with the derived number and ignore the instruction.
    #[tokio::test]
    async fn an_explicit_null_salience_is_a_clear_and_survives_the_derivation() {
        let mut args = json!({"properties": {"priority": "p0"}, "salience": null});

        normalize_priority_update(&mut args).expect("normalize");

        assert_eq!(args["properties"]["priority"], "p0");
        assert!(
            args["salience"].is_null(),
            "an explicit clear must reach the patch; got: {}",
            args["salience"]
        );
    }

    /// An update that does not mention the level must not mint a salience nobody asked for: doing
    /// so would rewrite the ranking of every task touched for an unrelated reason.
    #[tokio::test]
    async fn an_update_without_a_priority_does_not_touch_the_salience() {
        let mut args = json!({"properties": {"planning_label": "reviewed"}});
        normalize_priority_update(&mut args).expect("normalize");
        assert_eq!(args, json!({"properties": {"planning_label": "reviewed"}}));

        let mut no_properties = json!({"content": "body only"});
        normalize_priority_update(&mut no_properties).expect("normalize");
        assert_eq!(no_properties, json!({"content": "body only"}));
    }

    /// The wiring: the hook every generic update path calls must run it, or the function is
    /// correct and unreachable.
    #[tokio::test]
    async fn the_update_hook_runs_the_priority_normalization() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let token = runtime
            .authorize(Namespace::local())
            .expect("authorize local");
        let note = task_note_with(json!({"description": "body", "priority": "p3"}));
        let mut args = json!({"properties": {"priority": "p0"}});

        TaskHook
            .prepare_note_update(&runtime, &token, &note, &mut args)
            .await
            .expect("hook");

        assert_eq!(args["properties"]["priority"], "p0");
        assert_eq!(args["salience"], 1.0);
    }

    #[tokio::test]
    async fn normalized_task_update_refuses_a_stale_note_snapshot() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let token = runtime
            .authorize(Namespace::local())
            .expect("authorize local");
        let mut task = Note::new("local", "task", "original body");
        task.name = Some("original title".to_string());
        task.properties = Some(json!({"description": "original body", "status": "inbox"}));
        let task_id = task.id;
        runtime
            .notes(&token)
            .expect("note store")
            .upsert_note(task)
            .await
            .expect("seed task");

        let snapshot = runtime
            .notes(&token)
            .expect("note store")
            .get_note(task_id)
            .await
            .expect("read task")
            .expect("task exists");
        let mut args = json!({"content": "hook-derived body"});
        synchronize_description(&snapshot, &mut args).expect("normalize mirrors");

        // Deterministically land another writer after hook normalization but
        // before persistence from the original snapshot.
        let mut concurrent = snapshot.clone();
        concurrent.name = Some("concurrent title".to_string());
        concurrent.content = "concurrent body".to_string();
        concurrent.properties = Some(json!({"description": "concurrent body", "status": "inbox"}));
        concurrent.updated_at = snapshot.updated_at.saturating_add(10);
        runtime
            .notes(&token)
            .expect("note store")
            .upsert_note(concurrent)
            .await
            .expect("concurrent write");

        let patch = khive_runtime::NotePatch::new(
            None,
            args.get("content")
                .and_then(Value::as_str)
                .map(str::to_string),
            None,
            None,
            args.get("properties").cloned(),
        );
        let err = runtime
            .update_note_from_snapshot_with_embedding_report(&token, snapshot, patch)
            .await
            .expect_err("stale hook snapshot must not overwrite the concurrent task");
        let RuntimeError::Khive(conflict) = &err else {
            panic!("expected structured conflict, got: {err:?}");
        };
        assert_eq!(conflict.kind(), khive_types::ErrorKind::Conflict);
        assert!(conflict.message().contains("retry with fresh state"));

        let persisted = runtime
            .notes(&token)
            .expect("note store")
            .get_note(task_id)
            .await
            .expect("read persisted task")
            .expect("task exists");
        assert_eq!(persisted.name.as_deref(), Some("concurrent title"));
        assert_eq!(persisted.content, "concurrent body");
        assert_eq!(
            persisted
                .properties
                .as_ref()
                .and_then(|props| props.get("description"))
                .and_then(Value::as_str),
            Some("concurrent body")
        );
    }
}
