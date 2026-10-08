use super::{pack_registry_tag, RuntimeError, RuntimeResult, Uuid, Value};

// ---------------------------------------------------------------------------
// arg extraction helpers
// ---------------------------------------------------------------------------

pub(super) fn obj(args: &Value) -> RuntimeResult<&serde_json::Map<String, Value>> {
    args.as_object()
        .ok_or_else(|| RuntimeError::InvalidInput("op args must be a JSON object".into()))
}

pub(super) fn refuse_pack_registry_tags(tags: &[String], verb: &str) -> RuntimeResult<()> {
    let Some(tag) = tags.iter().find_map(|tag| pack_registry_tag(tag)) else {
        return Ok(());
    };
    Err(RuntimeError::InvalidInput(format!(
        "{verb} refuses registry tag {tag:?}: registry rows are written only by the owning pack"
    )))
}

pub(super) fn require_str<'a>(args: &'a Value, key: &str) -> RuntimeResult<&'a str> {
    obj(args)?
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| RuntimeError::InvalidInput(format!("missing required field {key:?}")))
}

/// Parse `key` as a bare UUID — never a short hex prefix.
///
/// A short prefix is a *resolution*: an unfiltered search that applies no
/// namespace predicate, so it can match nothing, match exactly one record
/// across every namespace, or match ambiguously. That search already
/// happened upstream, at the `kkernel` CLI boundary that has namespace
/// context and calls `resolve_uuid_unfiltered` before handing args down to
/// this module
/// (`crates/kkernel/src/atomic_apply.rs::resolve_kg_ids_in_args`). By the
/// time an id reaches this plan-preparation stage it must already name one
/// specific, already-identified record — which is exactly what a full UUID
/// demonstrates and a prefix does not.
pub(super) fn require_uuid(args: &Value, key: &str) -> RuntimeResult<Uuid> {
    let raw = require_str(args, key)?;
    Uuid::parse_str(raw).map_err(|_| {
        RuntimeError::InvalidInput(format!(
            "{key} must be a full UUID; got {raw:?}. This atomic-plan stage consumes an \
             already-resolved record and performs no namespace-scoped search of its own, so a \
             short hex prefix cannot be resolved here — resolve it to a full UUID first (e.g. \
             via `get`) and pass that."
        ))
    })
}

pub(super) fn optional_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    obj(args).ok()?.get(key).and_then(|v| v.as_str())
}

pub(super) fn optional_create_string(args: &Value, key: &str) -> RuntimeResult<Option<String>> {
    match obj(args)?.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "{key} must be a string or null, got: {other}"
        ))),
    }
}

/// ADR-014 tri-state patch for entity `entity_type` and `description`.
/// Both canonical fields preserve explicit null before building the patch.
/// Read the same distinction from raw atomic arguments: absent -> `None`
/// (unchanged), key
/// present as `null` -> `Some(None)` (explicit clear), key present as a
/// string -> `Some(Some(s))` (set); any other JSON type -> a hard error.
pub(super) fn optional_entity_type_patch(
    args: &Value,
    key: &str,
) -> RuntimeResult<Option<Option<String>>> {
    match obj(args)?.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(value)) => Ok(Some(Some(value.clone()))),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "{key} must be a string or null, got: {other}"
        ))),
    }
}

/// Nullable note-name patch semantics: absent or JSON `null` leaves the name
/// unchanged; a string sets it; every other JSON type is an error.
/// Entity descriptions use the tri-state helper above so null clears them.
pub(super) fn optional_string_patch(
    args: &Value,
    key: &str,
) -> RuntimeResult<Option<Option<String>>> {
    match obj(args)?.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(Some(s.clone()))),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "{key} must be a string or null, got: {other}"
        ))),
    }
}

/// Strict string-or-absent-or-null patch for entity `name`. Unlike
/// `optional_str`'s `.as_str()`, this does not silently drop a non-string,
/// non-null value like `name: 123` as absent: it rejects it instead of
/// reporting success for an invalid update. Canonical validates entity
/// `name` via `string_value` on `UpdateParams.name: Option<Value>`: null
/// collapses to absent at the struct-deserialize boundary (see
/// `optional_string_patch` doc above), so the reachable behavior is:
/// absent/null -> unchanged; non-null string -> set; any other JSON type ->
/// hard error. This mirrors that exactly, reading raw JSON instead of a
/// deserialized struct.
pub(super) fn entity_name_patch(args: &Value) -> RuntimeResult<Option<String>> {
    match obj(args)?.get("name") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "name must be a string, got: {other}"
        ))),
    }
}

/// Nullable-JSON-value patch for `properties`: canonical
/// `properties: Option<Value>` on `UpdateParams` collapses a literal JSON
/// `null` to Rust `None` at the struct-deserialize boundary (same collapse
/// as `optional_string_patch` above), so `properties=null` is canonically a
/// no-op (leave existing properties unchanged): not a stored JSON `null`.
/// This module reads raw JSON, so it must replicate that collapse: key
/// absent OR JSON `null` -> `None` (no merge); any other JSON value ->
/// `Some(value)` (merge), with no further shape validation at this layer.
pub(super) fn optional_properties(args: &Value, key: &str) -> RuntimeResult<Option<Value>> {
    match obj(args)?.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => Ok(Some(v.clone())),
    }
}

/// `tags` patch: canonical `tags: Option<Vec<String>>` on `UpdateParams`
/// collapses a literal JSON `null` to Rust `None` at the struct-deserialize
/// boundary (same collapse as above), so `tags=null` is canonically a no-op
/// (leave existing tags unchanged). A non-array, non-null value is still a
/// hard error (mirrors the type failure `UpdateParams` deserialization would
/// itself produce for a malformed `tags`).
pub(super) fn optional_tags(args: &Value) -> RuntimeResult<Option<Vec<String>>> {
    match obj(args)?.get("tags") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => {
            let mut tags = Vec::with_capacity(items.len());
            for item in items {
                let s = item.as_str().ok_or_else(|| {
                    RuntimeError::InvalidInput("tags must be an array of strings".into())
                })?;
                tags.push(s.to_string());
            }
            Ok(Some(tags))
        }
        Some(_) => Err(RuntimeError::InvalidInput(
            "tags must be an array of strings".into(),
        )),
    }
}

pub(super) fn optional_f64(args: &Value, key: &str) -> RuntimeResult<Option<f64>> {
    match obj(args)?.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| RuntimeError::InvalidInput(format!("{key} must be a number"))),
    }
}

/// Tri-state patch extraction for `Option<Option<f64>>`-shaped fields
/// (`NotePatch::salience` / `NotePatch::decay_factor`): key absent -> `None`
/// (untouched), key present as JSON `null` -> `Some(None)` (clear), key
/// present as a number -> `Some(Some(v))` (set). Range validation lives in
/// curation.rs's `prepare_update_note_from_snapshot`, not here.
pub(super) fn optional_f64_patch(args: &Value, key: &str) -> RuntimeResult<Option<Option<f64>>> {
    match obj(args)?.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(v) => v
            .as_f64()
            .map(|f| Some(Some(f)))
            .ok_or_else(|| RuntimeError::InvalidInput(format!("{key} must be a number"))),
    }
}
