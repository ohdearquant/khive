//! `TaskHook` — gtd's per-kind specialization for the `task` note kind.
//!
//! Implements the `KindHook` extension point for the pack standard. Normalises
//! user-facing GTD fields into the kg storage shape on `prepare_create`, keeps
//! task body mirrors aligned on `normalize_note_update`, and creates `depends_on`
//! graph edges on `after_create` (best-effort). GTD lifecycle semantics are
//! documented in `docs/design.md`.

use async_trait::async_trait;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::secret_gate::{mask_for_redaction_surface, RedactionSurface};
use khive_runtime::{
    KhiveRuntime, KindHook, LinkSpec, Namespace, NamespaceToken, NotePatch, NoteUpdateEffect,
    RuntimeError,
};
use khive_storage::{EdgeRelation, Note};

use crate::handlers::{parse_due, resolve_context_entity_id};
use crate::schema::{is_valid_priority, priority_to_salience};
use crate::task_create::{link_depends_on_edges, prepare_task_create, TaskCreateInput};

#[derive(Debug, Default)]
/// KindHook implementation for the `task` note kind; normalises GTD fields on create.
pub struct TaskHook;

/// Reuse creation's primary-namespace entity resolver for a supplied context.
/// Absence preserves the stored reference; explicit null retains patch clearing.
async fn normalize_context_entity_update(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &mut Value,
) -> Result<(), RuntimeError> {
    let Some(properties) = args.get_mut("properties").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    let Some(value) = properties
        .get("context_entity_id")
        .filter(|value| !value.is_null())
    else {
        return Ok(());
    };
    let raw = value.as_str().ok_or_else(|| {
        RuntimeError::InvalidInput(
            "properties.context_entity_id must be a full UUID string or null".into(),
        )
    })?;
    let id = resolve_context_entity_id(raw, runtime, token).await?;
    properties.insert("context_entity_id".into(), json!(id.to_string()));
    Ok(())
}

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
        // A duplicate judgment is only valid beside the cancellation it justifies, and
        // the lifecycle verbs validate and write the pair together.
        if properties.contains_key("duplicate_of") {
            return Err(RuntimeError::InvalidInput(
                "properties.duplicate_of is lifecycle-owned and cannot be patched on a task; \
                 record it with gtd.transition(status=\"cancelled\", duplicate_of=...) or \
                 gtd.complete(status=\"cancelled\", duplicate_of=...)"
                    .into(),
            ));
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
                "task update field `name` must be a string; got {}",
                mask_for_redaction_surface(RedactionSurface::GateProbe, &other.to_string())
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
                    "properties.description must be a string or null; got {}",
                    mask_for_redaction_surface(RedactionSurface::GateProbe, &other.to_string())
                )))
            }
        },
        Some(other) => {
            return Err(RuntimeError::InvalidInput(format!(
                "properties on a `task` note must be patched with an object; got {}",
                mask_for_redaction_surface(RedactionSurface::GateProbe, &other.to_string())
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
    let mut supplied_zone = None;
    for key in ["due_timezone", "timezone"] {
        let Some(value) = properties.get(key).filter(|value| !value.is_null()) else {
            continue;
        };
        let name = value.as_str().ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "{key} must be an IANA zone name string (e.g. \"America/New_York\"); got {}",
                mask_for_redaction_surface(RedactionSurface::GateProbe, &value.to_string())
            ))
        })?;
        let zone = name.parse::<chrono_tz::Tz>().map_err(|_| {
            RuntimeError::InvalidInput(format!(
                "timezone must be an IANA zone name (e.g. \"America/New_York\"); got {:?}",
                mask_for_redaction_surface(RedactionSurface::GateProbe, name)
            ))
        })?;
        supplied_zone.get_or_insert(zone);
    }
    if let Some(zone) = supplied_zone {
        return Ok(zone);
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
            RuntimeError::InvalidInput(format!(
                "due must be an ISO-8601 string or null; got {}",
                mask_for_redaction_surface(RedactionSurface::GateProbe, &due.to_string())
            ))
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
                "priority must be one of p0, p1, p2, p3, or null; got {}",
                mask_for_redaction_surface(RedactionSurface::GateProbe, &priority.to_string())
            ))
        })?;
        let canonical = named.to_ascii_lowercase();
        if !is_valid_priority(&canonical) {
            return Err(RuntimeError::InvalidInput(format!(
                "invalid priority {:?} — valid: p0, p1, p2, p3",
                mask_for_redaction_surface(RedactionSurface::GateProbe, named)
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

    async fn normalize_note_update(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        note: &Note,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        synchronize_description(note, args)?;
        normalize_due_update(runtime, note, args)?;
        normalize_priority_update(args)?;
        normalize_context_entity_update(runtime, token, args).await?;
        Ok(())
    }

    async fn note_update_effects(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        note: &Note,
        patch: &NotePatch,
    ) -> Result<Vec<NoteUpdateEffect>, RuntimeError> {
        let Some(value) = patch
            .properties
            .as_ref()
            .and_then(|p| p.get("context_entity_id"))
        else {
            return Ok(Vec::new());
        };
        // The normalizer has already validated/canonicalized the new reference.
        // Parse stored legacy UUID spellings too, so a spelling-only patch is a
        // graph no-op. A malformed historical value cannot identify an old edge.
        let previous = note
            .properties
            .as_ref()
            .and_then(|p| p.get("context_entity_id"))
            .and_then(Value::as_str)
            .and_then(|raw| Uuid::parse_str(raw).ok());
        let next = if value.is_null() {
            None
        } else {
            Some(
                value
                    .as_str()
                    .and_then(|raw| Uuid::parse_str(raw).ok())
                    .ok_or_else(|| {
                        RuntimeError::InvalidInput(
                            "context_entity_id update was not normalized".into(),
                        )
                    })?,
            )
        };
        if previous == next {
            return Ok(Vec::new());
        }
        let mut effects = Vec::new();
        if let Some(previous) = previous {
            if let Some(edge) = runtime
                .get_edge_by_natural_key_including_deleted(
                    token,
                    &note.namespace,
                    note.id,
                    previous,
                    EdgeRelation::Annotates,
                )
                .await?
                .filter(|edge| edge.deleted_at.is_none())
            {
                effects.push(NoteUpdateEffect::DeleteEdge(edge));
            }
        }
        if let Some(next) = next {
            let existing = runtime
                .get_edge_by_natural_key_including_deleted(
                    token,
                    &note.namespace,
                    note.id,
                    next,
                    EdgeRelation::Annotates,
                )
                .await?;
            // A separately authored live annotation already satisfies the new
            // context. Preserve its ID, timestamps, weight, and metadata.
            if let Some(edge) = existing.filter(|edge| edge.deleted_at.is_none()) {
                effects.push(NoteUpdateEffect::AssertLink(edge));
            } else {
                effects.push(NoteUpdateEffect::Link(LinkSpec {
                    namespace: Some(note.namespace.clone()),
                    source_id: note.id,
                    target_id: next,
                    relation: EdgeRelation::Annotates,
                    weight: 1.0,
                    metadata: None,
                    resurrect: true,
                }));
            }
        }
        Ok(effects)
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
#[path = "hook_tests.rs"]
mod tests;
