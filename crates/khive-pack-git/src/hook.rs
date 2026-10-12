//! `KindHook` implementations for the three note kinds this pack contributes.
//! Validation only — provenance edges are supplied by the caller and linked
//! by the runtime's `create_note` path itself. See
//! crates/khive-pack-git/docs/api/hooks.md for why no `after_create` edge work
//! is needed here.
//!
//! `prepare_create` and `validate_note_update` share one predicate per field
//! (`validate_*_shape` below) so a value refused on create is refused on
//! update too — see `crates/khive-pack-git/docs/api/hooks.md` for the defect
//! this closed (a `properties.sha` rejected on create was storable verbatim
//! through a generic `update`, because only `prepare_create` validated it).

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, KindHook, NamespaceToken, RuntimeError};
use khive_storage::Note;

/// The canonical `changed_paths` element shape. `pub(crate)` so the ingester
/// filters the raw `git log -z --name-only` stream against exactly the rule
/// this hook enforces, instead of handing the hook paths it must reject
/// (a Unix filename may legitimately contain `\` or start `X:`; those can
/// never round-trip through `changed_paths`).
pub(crate) fn is_repo_relative_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    // Any `X:` prefix is a Windows drive reference — absolute (`C:/...`) or
    // drive-relative (`C:foo`). The canonical shape is `/`-separated
    // repo-relative, so reject the prefix regardless of what follows it.
    let windows_drive_prefix =
        bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !windows_drive_prefix
        && !path.contains('\0')
        && path
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

/// The refusal shared by every entry point that requires `properties` to be
/// present and an object — create requires the key outright; update refuses
/// only when the key is present with a non-object value (absence/null is a
/// no-op the runtime already filtered out before calling a hook).
fn properties_object_required_error() -> RuntimeError {
    RuntimeError::InvalidInput(
        "kind=commit|issue|pull_request requires a `properties` object".into(),
    )
}

fn properties_obj(args: &Value) -> Result<&serde_json::Map<String, Value>, RuntimeError> {
    args.get("properties")
        .and_then(Value::as_object)
        .ok_or_else(properties_object_required_error)
}

fn properties_obj_mut(
    args: &mut Value,
) -> Result<&mut serde_json::Map<String, Value>, RuntimeError> {
    args.get_mut("properties")
        .and_then(Value::as_object_mut)
        .ok_or_else(properties_object_required_error)
}

/// Shared shape predicate for `commit properties.sha`, applied to a string
/// already known to be present. Extracted so `prepare_create` and
/// `validate_note_update` reject the same value the same way instead of
/// carrying two copies of the check.
fn validate_sha_shape(sha: &str) -> Result<(), RuntimeError> {
    if crate::object_id::is_40_hex(sha.as_bytes()) {
        Ok(())
    } else {
        Err(RuntimeError::InvalidInput(format!(
            "commit properties.sha {sha:?} must be a 40-character hex string"
        )))
    }
}

fn validate_commit_key_sha(key: Option<&str>, sha: &str) -> Result<(), RuntimeError> {
    if let Some(key) = key.filter(|key| key.starts_with("git.commit:")) {
        let expected = format!("git.commit:{sha}");
        if key != expected {
            return Err(RuntimeError::InvalidInput(format!(
                "commit key {key:?} must equal {expected:?} for properties.sha {sha:?}"
            )));
        }
    }
    Ok(())
}

/// Shared shape predicate for `commit properties.parents`, given whatever
/// raw JSON value the key held (including an explicit `null`, which is not
/// an array and so is refused the same way on create and on update).
fn validate_parents_shape(value: &Value) -> Result<(), RuntimeError> {
    let arr = value.as_array().ok_or_else(|| {
        RuntimeError::InvalidInput("commit properties.parents must be an array".into())
    })?;
    for (idx, p) in arr.iter().enumerate() {
        let s = p.as_str().ok_or_else(|| {
            RuntimeError::InvalidInput(format!("commit properties.parents[{idx}] must be a string"))
        })?;
        if !crate::object_id::is_40_hex(s.as_bytes()) {
            return Err(RuntimeError::InvalidInput(format!(
                "commit properties.parents[{idx}] {s:?} must be a 40-character hex string"
            )));
        }
    }
    Ok(())
}

/// Shared shape predicate for `commit properties.short_sha` against the
/// effective `sha` it must prefix. Only called with a `short` that is
/// already known to be a string — a non-string `short_sha` (including an
/// explicit JSON `null`) is silently left unvalidated by `prepare_create`,
/// and `validate_note_update` preserves that same bypass rather than
/// inventing a check the create path does not make.
fn validate_short_sha_shape(short: &str, sha: &str) -> Result<(), RuntimeError> {
    if short.is_empty() || !sha.starts_with(short) {
        Err(RuntimeError::InvalidInput(format!(
            "commit properties.short_sha {short:?} must be a non-empty prefix of sha {sha:?}"
        )))
    } else {
        Ok(())
    }
}

/// Shared shape predicate for `commit properties.changed_paths`, given
/// whatever raw JSON value the key held once an explicit `null` has already
/// been filtered out by the caller (both `prepare_create` and
/// `validate_note_update` treat `null` the same as absent for this field).
fn validate_changed_paths_shape(value: &Value) -> Result<(), RuntimeError> {
    let arr = value.as_array().ok_or_else(|| {
        RuntimeError::InvalidInput("commit properties.changed_paths must be an array".into())
    })?;
    let mut previous: Option<&str> = None;
    for (idx, path) in arr.iter().enumerate() {
        let Some(path) = path.as_str() else {
            return Err(RuntimeError::InvalidInput(format!(
                "commit properties.changed_paths[{idx}] must be a string"
            )));
        };
        if !is_repo_relative_path(path) {
            return Err(RuntimeError::InvalidInput(format!(
                "commit properties.changed_paths[{idx}] must be a non-empty \
                 repository-relative path using '/' separators, with no empty, '.' or \
                 '..' components, leading '/', drive prefix, backslash, or NUL byte"
            )));
        }
        if previous.is_some_and(|prior| path <= prior) {
            return Err(RuntimeError::InvalidInput(
                "commit properties.changed_paths must be sorted and deduplicated".into(),
            ));
        }
        previous = Some(path);
    }
    Ok(())
}

/// Read the retained half of the SHA/short-SHA pair from the stored note.
fn note_property_str<'a>(note: &'a Note, key: &str) -> Option<&'a str> {
    note.properties.as_ref()?.get(key)?.as_str()
}

/// `KindHook` for the immutable `commit` note kind.
///
/// Validates `properties.sha` (required, 40-hex) and, when present,
/// `properties.parents` (array of 40-hex strings) and `properties.changed_paths`
/// (array of repository-relative path strings; an explicit JSON `null` is
/// treated the same as an absent property). Commits have no lifecycle and no
/// `after_create` edge work.
///
/// `validate_note_update` enforces the identical shape on a generic property
/// patch, through the same `validate_*_shape` predicates `prepare_create`
/// calls: a key absent from the patch leaves the stored value alone; a key
/// present as JSON `null` clears it when the create path treats that field as
/// optional-and-nullable (`short_sha`, `changed_paths`) and is refused when
/// the create path requires the field (`sha`) or would itself reject a
/// literal `null` there (`parents`).
#[derive(Debug, Default)]
pub struct CommitHook;

#[async_trait]
impl KindHook for CommitHook {
    async fn prepare_create(
        &self,
        _runtime: &KhiveRuntime,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        let props = properties_obj(args)?;

        let sha = props
            .get("sha")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::InvalidInput("commit requires properties.sha".into()))?;
        validate_sha_shape(sha)?;
        validate_commit_key_sha(args.get("key").and_then(Value::as_str), sha)?;

        if let Some(parents) = props.get("parents") {
            validate_parents_shape(parents)?;
        }

        if let Some(short) = props.get("short_sha").and_then(Value::as_str) {
            validate_short_sha_shape(short, sha)?;
        }

        // An explicit JSON `null` carries no path facts and is treated the
        // same as an absent property; anything else must be the canonical
        // sorted, deduplicated array.
        if let Some(paths) = props.get("changed_paths").filter(|value| !value.is_null()) {
            validate_changed_paths_shape(paths)?;
        }

        Ok(())
    }

    async fn validate_note_update(
        &self,
        _runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        note: &Note,
        properties: Option<&Value>,
    ) -> Result<(), RuntimeError> {
        let Some(properties) = properties else {
            return Ok(());
        };
        let patch = properties
            .as_object()
            .ok_or_else(properties_object_required_error)?;

        // sha is required at create, so a patch cannot represent clearing it:
        // no create call could ever produce a commit row without one.
        if let Some(sha_value) = patch.get("sha") {
            if sha_value.is_null() {
                return Err(RuntimeError::InvalidInput(
                    "commit properties.sha cannot be cleared: every commit note requires a sha"
                        .into(),
                ));
            }
            let sha = sha_value.as_str().ok_or_else(|| {
                RuntimeError::InvalidInput("commit requires properties.sha".into())
            })?;
            validate_sha_shape(sha)?;
        }

        let effective_sha = patch
            .get("sha")
            .and_then(Value::as_str)
            .or_else(|| note_property_str(note, "sha"));
        if let Some(sha) = effective_sha {
            validate_commit_key_sha(note.key.as_deref(), sha)?;
        }

        // parents is optional at create (the key may be omitted), but create
        // itself refuses an explicit `null` there (it is not an array) —
        // `validate_parents_shape` reproduces that refusal unchanged, so a
        // patch cannot clear parents to null either.
        if let Some(parents_value) = patch.get("parents") {
            validate_parents_shape(parents_value)?;
        }

        // A patch to either coordinate must preserve the effective pair. An
        // explicit short_sha value, including null, overrides the stored value
        // before the create path's string-only validation is applied.
        if patch.contains_key("sha") || patch.contains_key("short_sha") {
            let effective_short = match patch.get("short_sha") {
                Some(value) => value.as_str(),
                None => note_property_str(note, "short_sha"),
            };
            let effective_sha = patch
                .get("sha")
                .and_then(Value::as_str)
                .or_else(|| note_property_str(note, "sha"));
            if let (Some(short), Some(sha)) = (effective_short, effective_sha) {
                validate_short_sha_shape(short, sha)?;
            }
        }

        // changed_paths: an explicit `null` is a no-op at create and stays
        // one here, so it clears the field rather than being refused.
        if let Some(paths) = patch.get("changed_paths").filter(|value| !value.is_null()) {
            validate_changed_paths_shape(paths)?;
        }

        Ok(())
    }
}

/// The governed `state_reason` value set for `issue` (ADR-088 §3). See
/// crates/khive-pack-git/docs/api/hooks.md#issuelikehook for why this is
/// `pub(crate)`.
pub(crate) const ISSUE_STATE_REASONS: &[&str] =
    &["completed", "not_planned", "reopened", "duplicate"];

/// Shared shape predicate for `{kind} properties.number`, applied to a value
/// already known to be present (`Some`).
fn validate_number_shape(kind: &str, value: &Value) -> Result<(), RuntimeError> {
    if value.is_u64() || value.is_i64() {
        Ok(())
    } else {
        Err(RuntimeError::InvalidInput(format!(
            "{kind} properties.number must be an integer"
        )))
    }
}

/// Shared shape predicate for `{kind} properties.project_id`: must be a
/// string parseable as a full UUID. The mutable create/update normalization
/// seams store the canonical hyphenated form; this predicate also serves the
/// immutable update validator.
fn validate_project_id_shape(kind: &str, value: &Value) -> Result<Uuid, RuntimeError> {
    let project_id = value.as_str().ok_or_else(|| {
        RuntimeError::InvalidInput(format!("{kind} requires properties.project_id"))
    })?;
    Uuid::parse_str(project_id).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "{kind} properties.project_id must be a full UUID because short-prefix resolution \
             can miss or be ambiguous, while provenance must identify one exact project; got \
             {project_id:?}: {error}"
        ))
    })
}

/// Shared shape predicate for `{kind} properties.state_reason`, applied to a
/// value already known to be a string.
fn validate_state_reason_shape(kind: &str, reason: &str) -> Result<(), RuntimeError> {
    // The raw value is never interpolated into this error: it is
    // caller-controlled (for `issue`, sourced from GitHub) and may be
    // credential-shaped. Only the static governed set is echoed.
    if kind == "issue" && !ISSUE_STATE_REASONS.contains(&reason) {
        return Err(RuntimeError::InvalidInput(format!(
            "issue properties.state_reason is not one of the governed values — valid: {}",
            ISSUE_STATE_REASONS.join(", ")
        )));
    }
    if reason.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(format!(
            "{kind} properties.state_reason must not be empty when present"
        )));
    }
    Ok(())
}

/// `KindHook` shared by `issue` and `pull_request` — both require
/// `properties.number` (integer) and `properties.project_id` (UUID), and,
/// when present, validate `properties.state_reason` (governed to a fixed
/// set for `issue` per ADR-088 §3; only checked for non-emptiness for
/// `pull_request`). See crates/khive-pack-git/docs/api/hooks.md#issuelikehook
/// for why `project_id` is required here rather than left to caller
/// discipline.
///
/// `validate_note_update` enforces the identical shape on a generic property
/// patch, through the same `validate_*_shape` predicates `prepare_create`
/// calls: a key absent from the patch leaves the stored value alone; `number`
/// and `project_id` cannot be cleared to `null` (both are required at
/// create, so no create call could produce a row missing either); an
/// explicit `null` for `state_reason` clears it, mirroring `prepare_create`'s
/// own silent bypass for any non-string value there.
#[derive(Debug)]
pub struct IssueLikeHook {
    /// The note kind this instance validates: `"issue"` or `"pull_request"`.
    pub kind: &'static str,
}

#[async_trait]
impl KindHook for IssueLikeHook {
    async fn prepare_create(
        &self,
        _runtime: &KhiveRuntime,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        let props = properties_obj_mut(args)?;

        let number = props.get("number").ok_or_else(|| {
            RuntimeError::InvalidInput(format!("{} requires properties.number", self.kind))
        })?;
        validate_number_shape(self.kind, number)?;

        let project_id_value = props.get("project_id").ok_or_else(|| {
            RuntimeError::InvalidInput(format!("{} requires properties.project_id", self.kind))
        })?;
        let project_uuid = validate_project_id_shape(self.kind, project_id_value)?;

        if let Some(reason) = props.get("state_reason").and_then(Value::as_str) {
            validate_state_reason_shape(self.kind, reason)?;
        }

        props.insert(
            "project_id".to_string(),
            Value::String(project_uuid.as_hyphenated().to_string()),
        );

        Ok(())
    }

    async fn normalize_note_update(
        &self,
        _runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        _note: &Note,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        let Some(project_id) = args
            .get_mut("properties")
            .and_then(Value::as_object_mut)
            .and_then(|properties| properties.get_mut("project_id"))
        else {
            return Ok(());
        };
        // Leave invalid/null values to the existing validator and its refusals.
        if let Some(project_uuid) = project_id
            .as_str()
            .and_then(|value| Uuid::parse_str(value).ok())
        {
            *project_id = Value::String(project_uuid.as_hyphenated().to_string());
        }
        Ok(())
    }

    async fn validate_note_update(
        &self,
        _runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        _note: &Note,
        properties: Option<&Value>,
    ) -> Result<(), RuntimeError> {
        let Some(properties) = properties else {
            return Ok(());
        };
        let patch = properties
            .as_object()
            .ok_or_else(properties_object_required_error)?;

        // number is required at create, so a patch cannot represent clearing
        // it: no create call could ever produce a row without one.
        if let Some(number_value) = patch.get("number") {
            if number_value.is_null() {
                return Err(RuntimeError::InvalidInput(format!(
                    "{} properties.number cannot be cleared: every {} note requires a number",
                    self.kind, self.kind
                )));
            }
            validate_number_shape(self.kind, number_value)?;
        }

        // project_id is likewise required at create; clearing it would
        // strand provenance that must always identify one exact project.
        if let Some(project_id_value) = patch.get("project_id") {
            if project_id_value.is_null() {
                return Err(RuntimeError::InvalidInput(format!(
                    "{} properties.project_id cannot be cleared: every {} note requires a \
                     project_id",
                    self.kind, self.kind
                )));
            }
            validate_project_id_shape(self.kind, project_id_value)?;
        }

        // state_reason is only checked when present as a string, mirroring
        // `prepare_create`'s own silent bypass for any other JSON shape —
        // including an explicit `null`, which is how a caller clears it.
        if let Some(reason) = patch.get("state_reason").and_then(Value::as_str) {
            validate_state_reason_shape(self.kind, reason)?;
        }

        Ok(())
    }
}

#[cfg(test)]
#[path = "hook_tests.rs"]
mod tests;
