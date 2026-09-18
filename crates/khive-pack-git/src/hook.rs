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

/// A 40-character lowercase-hex string, the shape of a full git commit SHA-1.
fn is_40_hex(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

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
    if is_40_hex(sha) {
        Ok(())
    } else {
        Err(RuntimeError::InvalidInput(format!(
            "commit properties.sha {sha:?} must be a 40-character hex string"
        )))
    }
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
        if !is_40_hex(s) {
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

/// Read a string-valued property off a stored note, for the one hook check
/// that needs the CURRENT record rather than only the patch (`short_sha`
/// must prefix the effective `sha`, which on an update that does not itself
/// patch `sha` is the value already on record).
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

    async fn after_create(
        &self,
        _runtime: &KhiveRuntime,
        _id: Uuid,
        _args: &Value,
    ) -> Result<(), RuntimeError> {
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

        // parents is optional at create (the key may be omitted), but create
        // itself refuses an explicit `null` there (it is not an array) —
        // `validate_parents_shape` reproduces that refusal unchanged, so a
        // patch cannot clear parents to null either.
        if let Some(parents_value) = patch.get("parents") {
            validate_parents_shape(parents_value)?;
        }

        // short_sha is only checked when it is present as a string, mirroring
        // `prepare_create`'s own silent bypass for any other JSON shape —
        // including an explicit `null`, which is how a caller clears it.
        if let Some(short) = patch.get("short_sha").and_then(Value::as_str) {
            let effective_sha = patch
                .get("sha")
                .and_then(Value::as_str)
                .or_else(|| note_property_str(note, "sha"));
            if let Some(sha) = effective_sha {
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
/// string parseable as a full UUID. Does not normalize/insert the canonical
/// hyphenated form — that mutation is `prepare_create`'s job alone, since
/// `validate_note_update` is handed an immutable patch.
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

    async fn after_create(
        &self,
        _runtime: &KhiveRuntime,
        _id: Uuid,
        _args: &Value,
    ) -> Result<(), RuntimeError> {
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
mod tests {
    use super::*;
    use khive_runtime::{Namespace, VerbRegistry, VerbRegistryBuilder};
    use serde_json::json;

    async fn fixture() -> (NamespaceToken, VerbRegistry) {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(khive_pack_kg::KgPack::new(rt.clone()));
        builder.register(crate::GitPack::new(rt.clone()));
        builder
            .with_runtime_event_store(&rt)
            .expect("configure trusted runtime audit store");
        let registry = builder.build().expect("registry builds");
        rt.install_edge_rules(registry.all_edge_rules());
        registry.apply_schema_plans(rt.backend());
        (token, registry)
    }

    async fn create_commit(registry: &VerbRegistry, sha: &str) -> Value {
        registry
            .dispatch(
                "create",
                json!({
                    "kind": "commit",
                    "name": sha,
                    "content": "a commit",
                    "properties": {"sha": sha},
                }),
            )
            .await
            .expect("create commit ok")
    }

    async fn create_issue(registry: &VerbRegistry, project_id: Uuid, number: i64) -> Value {
        registry
            .dispatch(
                "create",
                json!({
                    "kind": "issue",
                    "name": format!("issue {number}"),
                    "content": "an issue",
                    "properties": {"number": number, "project_id": project_id.to_string()},
                }),
            )
            .await
            .expect("create issue ok")
    }

    fn valid_sha() -> String {
        "a".repeat(40)
    }

    fn other_valid_sha() -> String {
        "b".repeat(40)
    }

    // ---------------------------------------------------------------------
    // CommitHook: create rejects an invalid sha (the reported defect's
    // create-side half — pinned so the mutation table below has a create
    // arm to compare against).
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn create_commit_refuses_invalid_sha() {
        let (_token, registry) = fixture().await;
        let err = registry
            .dispatch(
                "create",
                json!({
                    "kind": "commit",
                    "name": "bad",
                    "content": "a commit",
                    "properties": {"sha": "not-a-sha"},
                }),
            )
            .await
            .expect_err("a non-40-hex sha must be refused on create");
        assert!(
            err.to_string()
                .contains("must be a 40-character hex string"),
            "unexpected error: {err}"
        );
    }

    // ---------------------------------------------------------------------
    // CommitHook: update refuses the same values create refuses, with the
    // same message.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn update_commit_refuses_invalid_sha_with_create_message() {
        let (_token, registry) = fixture().await;
        let created = create_commit(&registry, &valid_sha()).await;
        let id = created["id"].as_str().unwrap().to_string();

        let err = registry
            .dispatch(
                "update",
                json!({"id": id, "properties": {"sha": "not-a-sha"}}),
            )
            .await
            .expect_err("update must refuse the same invalid sha create refuses");
        assert_eq!(
            err.to_string(),
            format!(
                "invalid input: commit properties.sha {:?} must be a 40-character hex string",
                "not-a-sha"
            ),
            "update's message must match create's exactly; got: {err}"
        );

        let after = registry
            .dispatch("get", json!({"id": id}))
            .await
            .expect("get after refused update");
        assert_eq!(
            after["properties"]["sha"],
            json!(valid_sha()),
            "a refused update must leave the stored sha untouched"
        );
    }

    #[tokio::test]
    async fn update_commit_refuses_invalid_parent_element() {
        let (_token, registry) = fixture().await;
        let created = create_commit(&registry, &valid_sha()).await;
        let id = created["id"].as_str().unwrap().to_string();

        let err = registry
            .dispatch(
                "update",
                json!({"id": id, "properties": {"parents": ["short"]}}),
            )
            .await
            .expect_err("update must refuse a non-40-hex parent element");
        assert!(
            err.to_string().contains("commit properties.parents[0]"),
            "error must name the offending index: {err}"
        );
    }

    #[tokio::test]
    async fn update_commit_refuses_invalid_changed_path() {
        let (_token, registry) = fixture().await;
        let created = create_commit(&registry, &valid_sha()).await;
        let id = created["id"].as_str().unwrap().to_string();

        let err = registry
            .dispatch(
                "update",
                json!({"id": id, "properties": {"changed_paths": ["/abs/path"]}}),
            )
            .await
            .expect_err("update must refuse an absolute changed_paths entry");
        assert!(
            err.to_string()
                .contains("commit properties.changed_paths[0]"),
            "error must name the offending index: {err}"
        );
    }

    #[tokio::test]
    async fn update_commit_refuses_short_sha_not_a_prefix_of_current_sha() {
        let (_token, registry) = fixture().await;
        let sha = valid_sha();
        let created = create_commit(&registry, &sha).await;
        let id = created["id"].as_str().unwrap().to_string();

        // short_sha is patched alone; the effective sha to check against is
        // the one already on record, read off the stored note snapshot.
        let err = registry
            .dispatch(
                "update",
                json!({"id": id, "properties": {"short_sha": "bbbbbbb"}}),
            )
            .await
            .expect_err("short_sha must still be validated against the ON-RECORD sha");
        assert!(
            err.to_string().contains("must be a non-empty prefix"),
            "unexpected error: {err}"
        );
    }

    // ---------------------------------------------------------------------
    // CommitHook: update still accepts a VALID patch — the easy-to-forget
    // arm that would catch an over-broad refusal.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn update_commit_accepts_valid_sha_and_short_sha_together() {
        let (_token, registry) = fixture().await;
        let created = create_commit(&registry, &valid_sha()).await;
        let id = created["id"].as_str().unwrap().to_string();
        let new_sha = other_valid_sha();
        let short = new_sha[..8].to_string();

        registry
            .dispatch(
                "update",
                json!({"id": id, "properties": {"sha": new_sha, "short_sha": short}}),
            )
            .await
            .expect("a valid sha + matching short_sha update must be accepted");

        let after = registry
            .dispatch("get", json!({"id": id}))
            .await
            .expect("get after accepted update");
        assert_eq!(after["properties"]["sha"], json!(other_valid_sha()));
    }

    #[tokio::test]
    async fn update_commit_accepts_valid_changed_paths() {
        let (_token, registry) = fixture().await;
        let created = create_commit(&registry, &valid_sha()).await;
        let id = created["id"].as_str().unwrap().to_string();

        registry
            .dispatch(
                "update",
                json!({"id": id, "properties": {"changed_paths": ["a.rs", "b/c.rs"]}}),
            )
            .await
            .expect("a valid changed_paths update must be accepted");

        let after = registry
            .dispatch("get", json!({"id": id}))
            .await
            .expect("get after accepted update");
        assert_eq!(
            after["properties"]["changed_paths"],
            json!(["a.rs", "b/c.rs"])
        );
    }

    // ---------------------------------------------------------------------
    // CommitHook: clear semantics.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn update_commit_refuses_to_clear_required_sha() {
        let (_token, registry) = fixture().await;
        let created = create_commit(&registry, &valid_sha()).await;
        let id = created["id"].as_str().unwrap().to_string();

        let err = registry
            .dispatch("update", json!({"id": id, "properties": {"sha": null}}))
            .await
            .expect_err("sha is required and must refuse an explicit clear");
        assert!(
            err.to_string().contains("cannot be cleared"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn update_commit_refuses_to_clear_parents_because_create_itself_would() {
        let (_token, registry) = fixture().await;
        let created = create_commit(&registry, &valid_sha()).await;
        let id = created["id"].as_str().unwrap().to_string();

        let err = registry
            .dispatch("update", json!({"id": id, "properties": {"parents": null}}))
            .await
            .expect_err("parents=null is not an array, same as create's own refusal");
        assert!(
            err.to_string()
                .contains("commit properties.parents must be an array"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn update_commit_clears_optional_short_sha_and_changed_paths() {
        let (_token, registry) = fixture().await;
        let sha = valid_sha();
        let created = registry
            .dispatch(
                "create",
                json!({
                    "kind": "commit",
                    "name": "clear-me",
                    "content": "a commit",
                    "properties": {
                        "sha": sha,
                        "short_sha": sha[..8].to_string(),
                        "changed_paths": ["a.rs"],
                    },
                }),
            )
            .await
            .expect("create commit with optional fields ok");
        let id = created["id"].as_str().unwrap().to_string();

        registry
            .dispatch(
                "update",
                json!({"id": id, "properties": {"short_sha": null, "changed_paths": null}}),
            )
            .await
            .expect("clearing optional fields must be accepted");

        let after = registry
            .dispatch("get", json!({"id": id}))
            .await
            .expect("get after clear");
        assert_eq!(after["properties"]["short_sha"], Value::Null);
        assert_eq!(after["properties"]["changed_paths"], Value::Null);
    }

    // ---------------------------------------------------------------------
    // CommitHook: an update naming none of these fields is left alone.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn update_commit_unrelated_patch_leaves_validated_fields_untouched() {
        let (_token, registry) = fixture().await;
        let sha = valid_sha();
        let created = create_commit(&registry, &sha).await;
        let id = created["id"].as_str().unwrap().to_string();

        registry
            .dispatch(
                "update",
                json!({"id": id, "properties": {"unrelated": "note"}}),
            )
            .await
            .expect("a patch naming only an unrelated key must be accepted");

        let after = registry
            .dispatch("get", json!({"id": id}))
            .await
            .expect("get after unrelated update");
        assert_eq!(after["properties"]["sha"], json!(sha));
        assert_eq!(after["properties"]["unrelated"], json!("note"));
    }

    // ---------------------------------------------------------------------
    // IssueLikeHook: create rejects an invalid number/project_id (pinned for
    // the mutation table's create arm).
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn create_issue_refuses_non_integer_number() {
        let (_token, registry) = fixture().await;
        let project = Uuid::new_v4();
        let err = registry
            .dispatch(
                "create",
                json!({
                    "kind": "issue",
                    "name": "bad number",
                    "content": "an issue",
                    "properties": {"number": "five", "project_id": project.to_string()},
                }),
            )
            .await
            .expect_err("a non-integer number must be refused on create");
        assert!(
            err.to_string().contains("must be an integer"),
            "unexpected error: {err}"
        );
    }

    // ---------------------------------------------------------------------
    // IssueLikeHook: update refuses the same values create refuses.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn update_issue_refuses_non_integer_number_with_create_message() {
        let (_token, registry) = fixture().await;
        let project = Uuid::new_v4();
        let created = create_issue(&registry, project, 1).await;
        let id = created["id"].as_str().unwrap().to_string();

        let err = registry
            .dispatch(
                "update",
                json!({"id": id, "properties": {"number": "five"}}),
            )
            .await
            .expect_err("update must refuse the same non-integer number create refuses");
        assert_eq!(
            err.to_string(),
            "invalid input: issue properties.number must be an integer",
            "update's message must match create's exactly; got: {err}"
        );
    }

    #[tokio::test]
    async fn update_issue_refuses_invalid_project_id() {
        let (_token, registry) = fixture().await;
        let project = Uuid::new_v4();
        let created = create_issue(&registry, project, 1).await;
        let id = created["id"].as_str().unwrap().to_string();

        let err = registry
            .dispatch(
                "update",
                json!({"id": id, "properties": {"project_id": "not-a-uuid"}}),
            )
            .await
            .expect_err("update must refuse a non-UUID project_id");
        assert!(
            err.to_string().contains("must be a full UUID"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn update_pull_request_refuses_ungoverned_state_reason_only_for_issue() {
        let (_token, registry) = fixture().await;
        let project = Uuid::new_v4();
        let created = registry
            .dispatch(
                "create",
                json!({
                    "kind": "pull_request",
                    "name": "pr",
                    "content": "a pull request",
                    "properties": {"number": 1, "project_id": project.to_string()},
                }),
            )
            .await
            .expect("create pull_request ok");
        let id = created["id"].as_str().unwrap().to_string();

        // pull_request has no governed set — any non-empty string passes.
        registry
            .dispatch(
                "update",
                json!({"id": id, "properties": {"state_reason": "closed-by-whoever"}}),
            )
            .await
            .expect("pull_request state_reason only needs non-emptiness");

        let err = registry
            .dispatch(
                "update",
                json!({"id": id, "properties": {"state_reason": "   "}}),
            )
            .await
            .expect_err("a whitespace-only state_reason must still be refused");
        assert!(
            err.to_string().contains("must not be empty"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn update_issue_refuses_ungoverned_state_reason() {
        let (_token, registry) = fixture().await;
        let project = Uuid::new_v4();
        let created = create_issue(&registry, project, 1).await;
        let id = created["id"].as_str().unwrap().to_string();

        let err = registry
            .dispatch(
                "update",
                json!({"id": id, "properties": {"state_reason": "closed-by-whoever"}}),
            )
            .await
            .expect_err("an ungoverned issue state_reason must be refused");
        assert!(
            err.to_string().contains("governed values"),
            "unexpected error: {err}"
        );
    }

    // ---------------------------------------------------------------------
    // IssueLikeHook: update still accepts a VALID patch.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn update_issue_accepts_valid_state_reason_and_number() {
        let (_token, registry) = fixture().await;
        let project = Uuid::new_v4();
        let created = create_issue(&registry, project, 1).await;
        let id = created["id"].as_str().unwrap().to_string();

        registry
            .dispatch(
                "update",
                json!({"id": id, "properties": {"number": 2, "state_reason": "completed"}}),
            )
            .await
            .expect("a valid number + governed state_reason update must be accepted");

        let after = registry
            .dispatch("get", json!({"id": id}))
            .await
            .expect("get after accepted update");
        assert_eq!(after["properties"]["number"], json!(2));
        assert_eq!(after["properties"]["state_reason"], json!("completed"));
    }

    // ---------------------------------------------------------------------
    // IssueLikeHook: clear semantics.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn update_issue_refuses_to_clear_required_number_and_project_id() {
        let (_token, registry) = fixture().await;
        let project = Uuid::new_v4();
        let created = create_issue(&registry, project, 1).await;
        let id = created["id"].as_str().unwrap().to_string();

        let err = registry
            .dispatch("update", json!({"id": id, "properties": {"number": null}}))
            .await
            .expect_err("number is required and must refuse an explicit clear");
        assert!(err.to_string().contains("cannot be cleared"));

        let err = registry
            .dispatch(
                "update",
                json!({"id": id, "properties": {"project_id": null}}),
            )
            .await
            .expect_err("project_id is required and must refuse an explicit clear");
        assert!(err.to_string().contains("cannot be cleared"));
    }

    #[tokio::test]
    async fn update_issue_clears_optional_state_reason() {
        let (_token, registry) = fixture().await;
        let project = Uuid::new_v4();
        let created = registry
            .dispatch(
                "create",
                json!({
                    "kind": "issue",
                    "name": "with reason",
                    "content": "an issue",
                    "properties": {
                        "number": 1,
                        "project_id": project.to_string(),
                        "state_reason": "completed",
                    },
                }),
            )
            .await
            .expect("create issue with state_reason ok");
        let id = created["id"].as_str().unwrap().to_string();

        registry
            .dispatch(
                "update",
                json!({"id": id, "properties": {"state_reason": null}}),
            )
            .await
            .expect("clearing the optional state_reason must be accepted");

        let after = registry
            .dispatch("get", json!({"id": id}))
            .await
            .expect("get after clear");
        assert_eq!(after["properties"]["state_reason"], Value::Null);
    }

    // ---------------------------------------------------------------------
    // IssueLikeHook: an update naming none of these fields is left alone.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn update_issue_unrelated_patch_leaves_validated_fields_untouched() {
        let (_token, registry) = fixture().await;
        let project = Uuid::new_v4();
        let created = create_issue(&registry, project, 7).await;
        let id = created["id"].as_str().unwrap().to_string();

        registry
            .dispatch(
                "update",
                json!({"id": id, "properties": {"unrelated": "note"}}),
            )
            .await
            .expect("a patch naming only an unrelated key must be accepted");

        let after = registry
            .dispatch("get", json!({"id": id}))
            .await
            .expect("get after unrelated update");
        assert_eq!(after["properties"]["number"], json!(7));
        assert_eq!(
            after["properties"]["project_id"],
            json!(project.to_string())
        );
        assert_eq!(after["properties"]["unrelated"], json!("note"));
    }
}
