//! Validation/defaulting for the shared finding-note create and update paths.

use async_trait::async_trait;
use serde_json::{json, Value};

use khive_runtime::{KhiveRuntime, KindHook, NamespaceToken, RuntimeError};
use khive_storage::Note;

use crate::vocab::{is_valid_confidence, is_valid_finding_status, is_valid_severity};

/// One closed-set field: the property name, its predicate, and the values its
/// error message names.
///
/// Create and update validate through this one table. The defect it exists to
/// prevent is the two writers drifting apart: every one of these fields is
/// consumed by comparing it against its valid set, so a value outside the set
/// is not reachable by a filter on that axis while the row still appears in
/// unfiltered listings.
type FindingEnum = (&'static str, fn(&str) -> bool, &'static str);

const FINDING_ENUMS: [FindingEnum; 3] = [
    (
        "kind_status",
        is_valid_finding_status as fn(&str) -> bool,
        "open, resolved, wontfix, invalid",
    ),
    (
        "severity",
        is_valid_severity as fn(&str) -> bool,
        "critical, high, medium, low, info",
    ),
    (
        "confidence",
        is_valid_confidence as fn(&str) -> bool,
        "high, medium, low",
    ),
];

/// Validate one non-null closed-set value. A key outside the table is not this
/// hook's business and is accepted.
fn validate_finding_enum(key: &str, value: &Value) -> Result<(), RuntimeError> {
    let Some((_, is_valid, valid_values)) = FINDING_ENUMS.iter().find(|(name, _, _)| *name == key)
    else {
        return Ok(());
    };
    let text = value
        .as_str()
        .ok_or_else(|| RuntimeError::InvalidInput(format!("{key} must be a string")))?;
    if !is_valid(text) {
        return Err(RuntimeError::InvalidInput(format!(
            "invalid {key} {text:?}; valid: {valid_values}"
        )));
    }
    Ok(())
}

#[derive(Debug, Default)]
pub(crate) struct FindingHook;

#[async_trait]
impl KindHook for FindingHook {
    async fn prepare_create(
        &self,
        _runtime: &KhiveRuntime,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        let title = args
            .get("title")
            .or_else(|| args.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "kind=note + note_kind=finding requires 'title' or 'name'".into(),
                )
            })?;
        if title.trim().is_empty() {
            return Err(RuntimeError::InvalidInput("title must not be empty".into()));
        }

        let mut props = match args.get("properties") {
            Some(Value::Object(map)) => Value::Object(map.clone()),
            Some(Value::Null) | None => json!({}),
            Some(_) => {
                return Err(RuntimeError::InvalidInput(
                    "properties must be an object".into(),
                ))
            }
        };
        let obj = props
            .as_object_mut()
            .expect("props is object by construction");

        for key in [
            "severity",
            "confidence",
            "categories",
            "source_run",
            "standard",
            "evidence",
            "refs",
        ] {
            if let Some(v) = args.get(key) {
                obj.insert(key.to_string(), v.clone());
            }
        }

        if let Some(v) = args.get("kind_status") {
            obj.insert("kind_status".into(), v.clone());
        } else if !obj.contains_key("kind_status") {
            obj.insert("kind_status".into(), json!("open"));
        }

        // kind_status is defaulted just above, so it is present here; severity
        // and confidence stay optional.
        for (key, _, _) in FINDING_ENUMS {
            if let Some(value) = obj.get(key) {
                validate_finding_enum(key, value)?;
            }
        }

        if let Some(v) = obj.get("evidence") {
            if !v.is_array() {
                return Err(RuntimeError::InvalidInput(
                    "evidence must be an array".into(),
                ));
            }
        }

        let content = args
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| title.clone());

        let root = args
            .as_object_mut()
            .ok_or_else(|| RuntimeError::Internal("create args must be a JSON object".into()))?;
        root.insert("name".into(), json!(title));
        root.insert("content".into(), json!(content));
        root.insert("properties".into(), props);

        Ok(())
    }

    async fn after_create(
        &self,
        _runtime: &KhiveRuntime,
        _id: uuid::Uuid,
        _args: &Value,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    /// Hold the update path to the predicates the create path uses.
    ///
    /// `code.ingest` is the pack's only verb, so the generic property update is
    /// the only way a finding's status can ever change after it is written, and
    /// resolving a finding is the one lifecycle action it has. That makes this
    /// the path that matters, not a secondary one.
    ///
    /// Patch semantics follow the shared tri-state contract: a key that is
    /// absent leaves the stored value alone, and a key present as `null` clears
    /// it. `severity` and `confidence` are optional on create, so clearing them
    /// lands in a state the create path can also produce. `kind_status` is not:
    /// create defaults it to `open` and every finding carries one, so clearing
    /// it is refused rather than allowed to produce a row no create could write.
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
        let Some(map) = properties.as_object() else {
            return Err(RuntimeError::InvalidInput(
                "properties must be an object".into(),
            ));
        };

        for (key, _, _) in FINDING_ENUMS {
            let Some(value) = map.get(key) else {
                continue;
            };
            if value.is_null() {
                if key == "kind_status" {
                    return Err(RuntimeError::InvalidInput(
                        "kind_status cannot be cleared; a finding always carries one of: open, \
                         resolved, wontfix, invalid"
                            .into(),
                    ));
                }
                continue;
            }
            validate_finding_enum(key, value)?;
        }
        Ok(())
    }
}
