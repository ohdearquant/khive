use std::any::Any;
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use khive_runtime::{
    KhiveRuntime, NamespaceToken, PackRuntime, RequestIdentity, RuntimeError, VerbRegistry,
};
use khive_storage::{
    AtomicUnitOp, SqlRow, SqlStatement, SqlValue, SqlWriter, StorageCapability, StorageError,
    StorageResult,
};
use khive_types::{
    HandlerDef, IdResolutionMode, Pack, PackSchemaPlan, ParamDef, VerbCategory, Visibility,
};

use crate::definition::{
    canonical_json, compile_document, digest, fold, Compiled, OperationType, Refusal, StepType,
};
use crate::frontier::{evaluate, GroupBy, Identity, Payload};
use crate::resolve::{resolve, Decision, Subject, Terms};

const fn param(
    name: &'static str,
    param_type: &'static str,
    required: bool,
    description: &'static str,
) -> ParamDef {
    ParamDef {
        name,
        param_type,
        required,
        description,
        resolution_mode: IdResolutionMode::NotApplicable,
    }
}

const OPERATION: ParamDef = param(
    "operation",
    "string",
    true,
    "The operation id that operation.open returned.",
);
const OBSERVED_AT: ParamDef = param(
    "observed_at",
    "string",
    false,
    "RFC 3339 time the thing happened; default the time it is recorded.",
);

pub(crate) static OPERATION_HANDLERS: [HandlerDef; 7] = [
    HandlerDef {
        name: "operation.define",
        description: "Publish operation types in the caller's namespace: slots, steps (who acts, after which steps, how many of a repeating slot), and rules that ask or skip a step on facts and on other steps. Each step is published as its own step type and pinned by digest. A slot is given by a kind and a role with an optional qualifier, by a relation to another slot of the type, or by neither; a kind, role or relation a slot names must be a term of the namespace (forms.terms) by name or alias. Publishing the same type again is a no-op; a different type under a published version is refused.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[param(
            "document",
            "object",
            true,
            "{operation_types: [{id, version, slots: [{name, repeats?, required?, kind?, role?, qualifier?, defined_by?: {relation, slot}}], steps: [{name, by, act?, after?, quantifier? all_of|at_least_one, activated_by?, unless?, outcome?}], rules?: [{step, action activate|skip, when, unless?, status?, owner?, citation?}]}]}. A condition list is alternatives of terms, each {fact: {field, slot?, present|equals|one_of}} or {step: {name, done}}.",
        )],
    },
    HandlerDef {
        name: "operation.open",
        description: "Open an operation of a published type in the caller's namespace, pinned to one type version, with any slot bindings known now. The type must be visible to the caller and published in another namespace.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            param("type", "string", true, "Operation type id."),
            param("version", "integer", false, "Type version; default the latest published."),
            param("type_namespace", "string", false, "Namespace holding the type, when more than one visible namespace publishes the id."),
            param("bindings", "object", false, "Slot name to an identity {kind, key, label?, entity_id?}, or to a list of them for a repeating slot."),
            param("parent", "string", false, "An operation in the caller's namespace this one is part of."),
        ],
    },
    HandlerDef {
        name: "operation.bind",
        description: "Record that an identity fills a slot of an operation. A slot of one identity takes one; binding the same identity again is a replay.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            OPERATION,
            param("slot", "string", true, "Slot name the type declares."),
            param("identity", "object", true, "{kind, key, label?, entity_id?}; two identities are the same when kind and key are."),
            param("key", "string", false, "Caller key for a retry; default derived from the binding."),
            OBSERVED_AT,
        ],
    },
    HandlerDef {
        name: "operation.record",
        description: "Append bindings, facts and acts to an operation in the caller's namespace. Each item carries a caller key: the same key with the same item is recorded once, the same key with a different item is refused. Records are never changed or removed.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            OPERATION,
            param(
                "items",
                "array",
                true,
                "Items, each {kind, key, observed_at?} with: binding {slot, identity}; fact {field, value, identity?}; act {step, identity?}, where identity is required when the step's slot repeats, and a named identity must be bound to that slot whenever the slot holds one.",
            ),
        ],
    },
    HandlerDef {
        name: "operation.frontier",
        description: "What an operation owes as of a time: each act owed now and who supplies it, each act blocked and the steps it waits on, each required slot nobody has bound, grouped by supplier, and the steps not asked with the rule that skipped them. Writes nothing.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            OPERATION,
            param("as_of", "string", false, "RFC 3339 time; default now."),
            param("basis", "string", false, "observed (default): records whose observed time is at or before as_of, in observed order; recorded: records stored at or before as_of, in stored order."),
            param("group_by", "string", false, "supplier (default): one group per identity or opener; by: one group per slot."),
        ],
    },
    HandlerDef {
        name: "operation.resolve",
        description: "Say which slot of a published operation type each subject of a loaded form fills, and how it was found: by a recorded decision (decision), by the subject's kind, role and qualifier (role), by a relation to a subject that fills the slot another slot is defined by (relation), or by a carrying relation to a subject that has a slot (carried, with the slot of that subject). A subject offered more than one slot, or reaching a slot that does not repeat along with another subject, is undecided with its candidates; a subject nothing reaches is no_slot. A decision whose slot the type version no longer has, or whose subject the form revision no longer declares, is named as lapsed. Reads the type (the latest version unless one is given), the form's subjects (the revision loaded last unless one is given), the namespace's terms and the decisions kept beside the type. Writes nothing.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            param("type", "string", true, "Operation type id."),
            param("version", "integer", false, "Type version; default the latest published."),
            param("type_namespace", "string", false, "Namespace holding the type, when more than one visible namespace publishes the id."),
            param("form", "string", true, "A form_id, or form_id@revision."),
            param("revision", "string", false, "Form revision; default the one loaded last."),
        ],
    },
    HandlerDef {
        name: "operation.decide",
        description: "Record that subjects of a form fill one slot of an operation type, or that they are not of this type, with the reason. The type must be published in the caller's namespace, and the decision is kept beside it. A subject the form's revision loaded last does not declare, or a slot the type's latest version does not have, is refused with nothing written. For each subject it names, a decision supersedes the earlier decisions naming that subject for this type and form; it keeps applying to later type versions and form revisions while its subject and its slot still exist. Decisions are never changed or removed.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            param("type", "string", true, "Operation type id, published in the caller's namespace."),
            param("form", "string", true, "The form_id whose subjects are decided."),
            param("subjects", "array of string", true, "Subject ids the form declares."),
            param("slot", "string", false, "The slot they fill; give this or not_this_type."),
            param("not_this_type", "boolean", false, "true: the subjects fill no slot of this type."),
            param("reason", "string", true, "Why, in a sentence a later reader can check."),
        ],
    },
];

/// Operation graphs: published operation types, operations recorded against them, and the
/// frontier of what each operation owes.
pub struct OperationPack {
    runtime: KhiveRuntime,
}

impl Pack for OperationPack {
    const NAME: &'static str = "operation";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &OPERATION_HANDLERS;
    /// Slots name their kinds, roles and relations from the forms pack's term lists, and
    /// resolve reads a form's subjects there.
    const REQUIRES: &'static [&'static str] = &["forms"];
    const SCHEMA_PLAN: Option<PackSchemaPlan> = Some(PackSchemaPlan {
        pack: Self::NAME,
        statements: crate::schema::OPERATION_SCHEMA_STATEMENTS,
    });
}

impl OperationPack {
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }
}

struct OperationPackFactory;

impl khive_runtime::PackFactory for OperationPackFactory {
    khive_runtime::pack_factory_metadata!(OperationPack);

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        Box::new(OperationPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&OperationPackFactory) }

#[async_trait]
impl PackRuntime for OperationPack {
    khive_runtime::pack_runtime_metadata!();

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        let params = Params::new(verb, params)?;
        match verb {
            "operation.define" => self.define(params, registry, token).await,
            "operation.open" => self.open(params, token).await,
            "operation.bind" => self.bind(params, token).await,
            "operation.record" => self.record(params, token).await,
            "operation.frontier" => self.frontier(params, token).await,
            "operation.resolve" => self.resolve(params, registry, token).await,
            "operation.decide" => self.decide(params, registry, token).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "operation pack does not handle verb {verb:?}"
            ))),
        }
    }
}

struct Params {
    verb: String,
    map: Map<String, Value>,
}

impl Params {
    fn new(verb: &str, params: Value) -> Result<Self, RuntimeError> {
        let map = match params {
            Value::Object(map) => map,
            Value::Null => Map::new(),
            _ => {
                return Err(RuntimeError::InvalidInput(format!(
                    "{verb}: parameters must be an object"
                )))
            }
        };
        if let Some(handler) = OPERATION_HANDLERS
            .iter()
            .find(|handler| handler.name == verb)
        {
            if let Some(name) = map.keys().find(|name| {
                !handler
                    .params
                    .iter()
                    .any(|param| param.name == name.as_str())
            }) {
                return Err(RuntimeError::InvalidInput(format!(
                    "{verb}: unknown parameter {name:?}"
                )));
            }
        }
        Ok(Self {
            verb: verb.to_owned(),
            map,
        })
    }

    fn invalid(&self, message: impl std::fmt::Display) -> RuntimeError {
        RuntimeError::InvalidInput(format!("{}: {message}", self.verb))
    }

    fn refused(&self, refusal: Refusal) -> RuntimeError {
        self.invalid(refusal)
    }

    fn opt_str(&self, name: &str) -> Result<Option<String>, RuntimeError> {
        match self.map.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
            Some(Value::String(_)) => Err(self.invalid(format!("{name} must not be empty"))),
            Some(_) => Err(self.invalid(format!("{name} must be a string"))),
        }
    }

    fn str(&self, name: &str) -> Result<String, RuntimeError> {
        self.opt_str(name)?
            .ok_or_else(|| self.invalid(format!("{name} is required")))
    }

    fn opt_i64(&self, name: &str) -> Result<Option<i64>, RuntimeError> {
        match self.map.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_i64()
                .map(Some)
                .ok_or_else(|| self.invalid(format!("{name} must be an integer"))),
        }
    }

    fn opt_value(&self, name: &str) -> Option<&Value> {
        match self.map.get(name) {
            None | Some(Value::Null) => None,
            Some(value) => Some(value),
        }
    }

    fn object(&self, name: &str) -> Result<Map<String, Value>, RuntimeError> {
        match self.opt_value(name) {
            Some(Value::Object(map)) => Ok(map.clone()),
            Some(_) => Err(self.invalid(format!("{name} must be an object"))),
            None => Err(self.invalid(format!("{name} is required"))),
        }
    }

    fn time(&self, name: &str) -> Result<Option<i64>, RuntimeError> {
        self.opt_str(name)?
            .map(|raw| {
                parse_time(&raw).map_err(|message| self.invalid(format!("{name}: {message}")))
            })
            .transpose()
    }
}

fn parse_time(raw: &str) -> Result<i64, String> {
    let time = chrono::DateTime::parse_from_rfc3339(raw)
        .map_err(|e| format!("not an RFC 3339 time ({e})"))?;
    let micros = time.timestamp_micros();
    if micros < 0 {
        return Err("times before 1970 are not recorded".to_owned());
    }
    Ok(micros)
}

fn format_time(micros: i64) -> Value {
    chrono::DateTime::<chrono::Utc>::from_timestamp_micros(micros)
        .map(|time| json!(time.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)))
        .unwrap_or(Value::Null)
}

fn now_us() -> i64 {
    chrono::Utc::now().timestamp_micros()
}

fn stmt(sql: &str, params: Vec<SqlValue>) -> SqlStatement {
    SqlStatement::new(sql, params)
}

fn text(value: &str) -> SqlValue {
    SqlValue::Text(value.to_owned())
}

fn corrupt(message: impl Into<String>) -> StorageError {
    StorageError::Internal(format!("operation: {}", message.into()))
}

fn col_text(row: &SqlRow, name: &str) -> StorageResult<String> {
    row.text(name)
        .map(str::to_owned)
        .map_err(|e| corrupt(format!("column {name}: {e}")))
}

fn col_opt_text(row: &SqlRow, name: &str) -> StorageResult<Option<String>> {
    row.opt_text(name)
        .map(|value| value.map(str::to_owned))
        .map_err(|e| corrupt(format!("column {name}: {e}")))
}

fn col_i64(row: &SqlRow, name: &str) -> StorageResult<i64> {
    row.i64(name)
        .map_err(|e| corrupt(format!("column {name}: {e}")))
}

fn col_opt_i64(row: &SqlRow, name: &str) -> StorageResult<Option<i64>> {
    row.opt_i64(name)
        .map_err(|e| corrupt(format!("column {name}: {e}")))
}

fn col_bytes(row: &SqlRow, name: &str) -> StorageResult<Vec<u8>> {
    match row.get(name) {
        Some(SqlValue::Blob(bytes)) => Ok(bytes.clone()),
        _ => Err(corrupt(format!("column {name} is not bytes"))),
    }
}

/// A refusal raised inside a unit rolls the unit back; it surfaces as the storage refusal.
fn refuse(verb: &str, refusal: Refusal) -> StorageError {
    StorageError::InvalidInput {
        capability: StorageCapability::Sql,
        operation: Cow::Owned(verb.to_owned()),
        message: format!("{verb}: {refusal}"),
    }
}

fn unit_value(result: Box<dyn Any + Send>, verb: &str) -> Result<Value, RuntimeError> {
    result
        .downcast::<Value>()
        .map(|value| *value)
        .map_err(|_| RuntimeError::Internal(format!("{verb}: invalid transaction outcome")))
}

fn boxed(value: Value) -> Box<dyn Any + Send> {
    Box::new(value)
}

fn visible_namespaces_json(token: &NamespaceToken) -> String {
    json!(token.visible_namespace_strs()).to_string()
}

fn parse_identity(params: &Params, what: &str, value: &Value) -> Result<Identity, RuntimeError> {
    let identity: Identity = serde_json::from_value(value.clone()).map_err(|e| {
        params.invalid(format!(
            "{what}: identity must be {{kind, key, label?, entity_id?}} ({e})"
        ))
    })?;
    if identity.kind.trim().is_empty() || identity.key.trim().is_empty() {
        return Err(params.invalid(format!("{what}: identity kind and key must not be empty")));
    }
    Ok(identity)
}

/// A definition row read back and checked against the digest it is pinned by.
fn checked_definition(row: &SqlRow, kind: &str, pinned: &str) -> StorageResult<Vec<u8>> {
    let bytes = col_bytes(row, "definition_bytes")?;
    let held_kind = col_text(row, "kind")?;
    let held_digest = col_text(row, "digest")?;
    if held_kind != kind || held_digest != pinned || digest(&bytes) != pinned {
        return Err(corrupt(format!(
            "definition_digest_mismatch: {} v{} does not match the digest it is pinned by",
            col_text(row, "definition_id")?,
            col_i64(row, "version")?
        )));
    }
    Ok(bytes)
}

struct Loaded {
    operation_id: String,
    namespace: String,
    opened_by: String,
    opened_at_us: i64,
    parent: Option<String>,
    type_namespace: String,
    type_digest: String,
    definition: OperationType,
    step_types: HashMap<String, StepType>,
}

impl Loaded {
    fn type_view(&self) -> Value {
        json!({
            "namespace": self.type_namespace,
            "id": self.definition.id,
            "version": self.definition.version,
            "digest": self.type_digest,
        })
    }
}

struct Prepared {
    key: String,
    payload: Payload,
    digest: String,
    bytes: Vec<u8>,
    observed_at_us: Option<i64>,
}

const ITEM_FIELDS_BINDING: &[&str] = &["kind", "key", "observed_at", "slot", "identity"];
const ITEM_FIELDS_FACT: &[&str] = &["kind", "key", "observed_at", "field", "value", "identity"];
const ITEM_FIELDS_ACT: &[&str] = &["kind", "key", "observed_at", "step", "identity"];
const LATER_KINDS: &[&str] = &["document", "observation", "variant", "typing"];

fn prepare(payload: Payload, key: String, observed_at_us: Option<i64>) -> Prepared {
    let payload_value = serde_json::to_value(&payload).unwrap_or(Value::Null);
    let bytes = canonical_json(&payload_value);
    let digest = digest(&canonical_json(&json!({
        "payload": payload_value,
        "observed_at_us": observed_at_us,
    })));
    Prepared {
        key,
        payload,
        digest,
        bytes,
        observed_at_us,
    }
}

fn parse_item(params: &Params, index: usize, value: &Value) -> Result<Prepared, RuntimeError> {
    let Some(object) = value.as_object() else {
        return Err(params.invalid(format!("items[{index}] must be an object")));
    };
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| params.invalid(format!("items[{index}].kind must be a string")))?;
    let allowed = match kind {
        "binding" => ITEM_FIELDS_BINDING,
        "fact" => ITEM_FIELDS_FACT,
        "act" => ITEM_FIELDS_ACT,
        later if LATER_KINDS.contains(&later) => {
            return Err(params.refused(Refusal::new(
                "kind_not_available",
                format!("items[{index}]: {later} records are not taken yet"),
            )))
        }
        other => {
            return Err(params.invalid(format!(
                "items[{index}].kind must be binding, fact or act, got {other:?}"
            )))
        }
    };
    if let Some(name) = object.keys().find(|name| !allowed.contains(&name.as_str())) {
        return Err(params.invalid(format!("items[{index}]: {kind} takes no field {name:?}")));
    }
    let key = match object.get("key") {
        Some(Value::String(key)) if !key.trim().is_empty() => key.clone(),
        _ => return Err(params.invalid(format!("items[{index}].key must be a non-empty string"))),
    };
    let observed_at_us = match object.get("observed_at") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) => Some(
            parse_time(raw)
                .map_err(|m| params.invalid(format!("items[{index}].observed_at: {m}")))?,
        ),
        Some(_) => {
            return Err(params.invalid(format!("items[{index}].observed_at must be a string")))
        }
    };
    let identity = match object.get("identity") {
        None | Some(Value::Null) => None,
        Some(value) => Some(parse_identity(params, &format!("items[{index}]"), value)?),
    };
    let name = |field: &str| -> Result<String, RuntimeError> {
        match object.get(field) {
            Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
            _ => Err(params.invalid(format!("items[{index}].{field} must be a non-empty string"))),
        }
    };
    let payload = match kind {
        "binding" => Payload::Binding {
            slot: name("slot")?,
            identity: identity.ok_or_else(|| {
                params.invalid(format!("items[{index}]: a binding needs an identity"))
            })?,
        },
        "fact" => Payload::Fact {
            field: name("field")?,
            value: object
                .get("value")
                .cloned()
                .ok_or_else(|| params.invalid(format!("items[{index}]: a fact needs a value")))?,
            identity,
        },
        _ => Payload::Act {
            step: name("step")?,
            identity,
        },
    };
    Ok(prepare(payload, key, observed_at_us))
}

/// Checks an item against the pinned type before the unit opens.
fn check_item(params: &Params, loaded: &Loaded, item: &Prepared) -> Result<(), RuntimeError> {
    match &item.payload {
        Payload::Binding { slot, .. } => {
            if loaded.definition.slot(slot).is_none() {
                return Err(params.refused(Refusal::new(
                    "unknown_slot",
                    format!(
                        "{}: {} declares no slot {slot}",
                        item.key, loaded.definition.id
                    ),
                )));
            }
        }
        Payload::Fact { .. } => {}
        Payload::Act { step, identity } => {
            let Some(step_type) = loaded.step_types.get(step) else {
                return Err(params.refused(Refusal::new(
                    "unknown_step",
                    format!(
                        "{}: {} declares no step {step}",
                        item.key, loaded.definition.id
                    ),
                )));
            };
            let repeats = loaded
                .definition
                .slot(&step_type.by)
                .is_some_and(|slot| slot.repeats);
            if repeats && identity.is_none() {
                return Err(params.refused(Refusal::new(
                    "act_identity_required",
                    format!(
                        "{}: step {step} is asked of each identity in {}, so its act names which",
                        item.key, step_type.by
                    ),
                )));
            }
        }
    }
    Ok(())
}

impl OperationPack {
    async fn define(
        &self,
        params: Params,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        let document = Value::Object(params.object("document")?);
        let compiled = compile_document(&document).map_err(|refusal| params.refused(refusal))?;
        check_terms(&params, &compiled, registry, token).await?;
        let namespace = token.namespace().as_str().to_owned();
        let now = now_us();
        let verb = params.verb.clone();
        let unit_verb = verb.clone();
        let op: AtomicUnitOp = Box::new(move |writer| {
            Box::pin(async move {
                let mut published = Vec::new();
                let mut already = Vec::new();
                for type_ in &compiled {
                    if held_same(&mut *writer, &unit_verb, &namespace, &type_.operation_type)
                        .await?
                    {
                        already.push(summary(type_));
                        continue;
                    }
                    for stored in type_
                        .step_types
                        .iter()
                        .chain(std::iter::once(&type_.operation_type))
                    {
                        if stored.kind == "step_type"
                            && held_same(&mut *writer, &unit_verb, &namespace, stored).await?
                        {
                            continue;
                        }
                        writer
                            .execute(stmt(
                                include_str!("../sql/definition-insert.sql"),
                                vec![
                                    text(&namespace),
                                    text(&stored.id),
                                    SqlValue::Integer(stored.version),
                                    text(stored.kind),
                                    text(&stored.digest),
                                    SqlValue::Blob(stored.bytes.clone()),
                                    text(&stored.source),
                                    SqlValue::Integer(now),
                                ],
                            ))
                            .await?;
                    }
                    published.push(summary(type_));
                }
                Ok(boxed(json!({
                    "namespace": namespace,
                    "published": published,
                    "already_published": already,
                })))
            })
        });
        let out = self.runtime.sql().atomic_unit(op).await?;
        unit_value(out, &verb)
    }

    async fn open(&self, params: Params, token: &NamespaceToken) -> Result<Value, RuntimeError> {
        let type_id = params.str("type")?;
        let version = params.opt_i64("version")?;
        if version.is_some_and(|v| v < 1) {
            return Err(params.invalid("version must be 1 or more"));
        }
        let type_namespace = params.opt_str("type_namespace")?;
        let parent = params.opt_str("parent")?;
        let namespace = token.namespace().as_str().to_owned();
        let mut reader = self.runtime.sql().reader().await?;
        let rows = reader
            .query_all(stmt(
                include_str!("../sql/definition-find.sql"),
                vec![
                    text(&type_id),
                    text(&visible_namespaces_json(token)),
                    SqlValue::from_opt_text(type_namespace.as_deref()),
                    SqlValue::from_opt_i64(version),
                ],
            ))
            .await?;
        let Some(row) = rows.first() else {
            return Err(RuntimeError::NotFound(format!("operation type {type_id}")));
        };
        let namespaces: BTreeSet<String> = rows
            .iter()
            .map(|row| col_text(row, "namespace"))
            .collect::<StorageResult<_>>()?;
        if namespaces.len() > 1 {
            return Err(params.refused(Refusal::new(
                "ambiguous_type",
                format!(
                    "{type_id} is published in {}; name one as type_namespace",
                    namespaces.into_iter().collect::<Vec<_>>().join(", ")
                ),
            )));
        }
        let definition_namespace = col_text(row, "namespace")?;
        if definition_namespace == namespace {
            return Err(params.refused(Refusal::new(
                "instance_in_type_namespace",
                format!(
                    "{type_id} is published in {namespace}; open its operations from another namespace"
                ),
            )));
        }
        let type_digest = col_text(row, "digest")?;
        let bytes = checked_definition(row, "operation_type", &type_digest)?;
        let definition: OperationType = serde_json::from_slice(&bytes)
            .map_err(|e| RuntimeError::Internal(format!("operation type {type_id}: {e}")))?;
        if let Some(parent) = &parent {
            let held = reader
                .query_row(stmt(
                    include_str!("../sql/operation-get.sql"),
                    vec![text(parent)],
                ))
                .await?;
            let visible = token.visible_namespace_strs();
            match held {
                Some(row) if col_text(&row, "namespace")? == namespace => {}
                Some(row) if visible.contains(&col_text(&row, "namespace")?.as_str()) => {
                    return Err(params.refused(Refusal::new(
                        "parent_in_other_namespace",
                        format!("parent {parent} is not in {namespace}"),
                    )))
                }
                _ => return Err(RuntimeError::NotFound(format!("operation {parent}"))),
            }
        }
        drop(reader);

        let mut bindings: Vec<(String, Identity)> = Vec::new();
        if let Some(given) = params.opt_value("bindings") {
            let Some(given) = given.as_object() else {
                return Err(params.invalid("bindings must be an object"));
            };
            for (slot_name, value) in given {
                let Some(slot) = definition.slot(slot_name) else {
                    return Err(params.refused(Refusal::new(
                        "unknown_slot",
                        format!("{type_id} declares no slot {slot_name}"),
                    )));
                };
                let listed: Vec<&Value> = match value {
                    Value::Array(items) => items.iter().collect(),
                    one => vec![one],
                };
                if listed.len() > 1 && !slot.repeats {
                    return Err(params.refused(Refusal::new(
                        "slot_holds_one",
                        format!(
                            "slot {slot_name} holds one identity, {} were given",
                            listed.len()
                        ),
                    )));
                }
                let mut seen = BTreeSet::new();
                for item in listed {
                    let identity = parse_identity(&params, &format!("bindings.{slot_name}"), item)?;
                    if seen.insert(identity.reference()) {
                        bindings.push((slot_name.clone(), identity));
                    }
                }
            }
        }

        let operation_id = uuid::Uuid::now_v7().to_string();
        let opened_by = token.actor().id.clone();
        let now = now_us();
        let records: Vec<Prepared> = bindings
            .iter()
            .enumerate()
            .map(|(index, (slot, identity))| {
                prepare(
                    Payload::Binding {
                        slot: slot.clone(),
                        identity: identity.clone(),
                    },
                    format!("open:{slot}:{index}"),
                    None,
                )
            })
            .collect();
        let verb = params.verb.clone();
        let (unit_id, unit_ns, unit_def_ns, unit_type_id) = (
            operation_id.clone(),
            namespace.clone(),
            definition_namespace.clone(),
            type_id.clone(),
        );
        let (unit_version, unit_digest, unit_parent, unit_opened_by) = (
            definition.version,
            type_digest.clone(),
            parent.clone(),
            opened_by.clone(),
        );
        let op: AtomicUnitOp = Box::new(move |writer| {
            Box::pin(async move {
                writer
                    .execute(stmt(
                        include_str!("../sql/operation-insert.sql"),
                        vec![
                            text(&unit_id),
                            text(&unit_ns),
                            text(&unit_def_ns),
                            text(&unit_type_id),
                            SqlValue::Integer(unit_version),
                            text(&unit_digest),
                            SqlValue::from_opt_text(unit_parent.as_deref()),
                            text(&unit_opened_by),
                            SqlValue::Integer(now),
                        ],
                    ))
                    .await?;
                for (index, record) in records.iter().enumerate() {
                    insert_record(
                        &mut *writer,
                        &unit_id,
                        &unit_ns,
                        index as i64 + 1,
                        record,
                        now,
                    )
                    .await?;
                }
                Ok(boxed(Value::Null))
            })
        });
        let out = self.runtime.sql().atomic_unit(op).await?;
        unit_value(out, &verb)?;

        let slots: Vec<Value> = definition
            .slots
            .iter()
            .map(|slot| {
                let bound: Vec<&Identity> = bindings
                    .iter()
                    .filter(|(name, _)| *name == slot.name)
                    .map(|(_, identity)| identity)
                    .collect();
                json!({
                    "name": slot.name,
                    "repeats": slot.repeats,
                    "required": slot.required,
                    "bound": bound,
                })
            })
            .collect();
        Ok(json!({
            "operation_id": operation_id,
            "namespace": namespace,
            "type": {
                "namespace": definition_namespace,
                "id": definition.id,
                "version": definition.version,
                "digest": type_digest,
            },
            "parent": parent,
            "opened_by": opened_by,
            "opened_at": format_time(now),
            "slots": slots,
        }))
    }

    async fn bind(&self, params: Params, token: &NamespaceToken) -> Result<Value, RuntimeError> {
        let operation_id = params.str("operation")?;
        let slot = params.str("slot")?;
        let identity_value = params
            .opt_value("identity")
            .cloned()
            .ok_or_else(|| params.invalid("identity is required"))?;
        let identity = parse_identity(&params, "identity", &identity_value)?;
        let observed_at_us = params.time("observed_at")?;
        let payload = Payload::Binding { slot, identity };
        let mut item = prepare(payload, String::new(), observed_at_us);
        item.key = match params.opt_str("key")? {
            Some(key) => key,
            None => format!("bind:{}", item.digest),
        };
        let mut out = self
            .append(&params, &operation_id, vec![item], token)
            .await?;
        let recorded = out["recorded"][0].take();
        Ok(json!({
            "operation_id": out["operation_id"].take(),
            "key": recorded["key"],
            "sequence": recorded["sequence"],
            "replayed": recorded["replayed"],
        }))
    }

    async fn record(&self, params: Params, token: &NamespaceToken) -> Result<Value, RuntimeError> {
        let operation_id = params.str("operation")?;
        let items = match params.opt_value("items") {
            Some(Value::Array(items)) if !items.is_empty() => items.clone(),
            Some(Value::Array(_)) => return Err(params.invalid("items must not be empty")),
            Some(_) => return Err(params.invalid("items must be an array")),
            None => return Err(params.invalid("items is required")),
        };
        let prepared = items
            .iter()
            .enumerate()
            .map(|(index, value)| parse_item(&params, index, value))
            .collect::<Result<Vec<_>, _>>()?;
        self.append(&params, &operation_id, prepared, token).await
    }

    async fn load(
        &self,
        operation_id: &str,
        token: &NamespaceToken,
    ) -> Result<Loaded, RuntimeError> {
        let mut reader = self.runtime.sql().reader().await?;
        let not_found = || RuntimeError::NotFound(format!("operation {operation_id}"));
        let Some(row) = reader
            .query_row(stmt(
                include_str!("../sql/operation-get.sql"),
                vec![text(operation_id)],
            ))
            .await?
        else {
            return Err(not_found());
        };
        let namespace = col_text(&row, "namespace")?;
        if !token.visible_namespace_strs().contains(&namespace.as_str()) {
            return Err(not_found());
        }
        let (Some(type_namespace), Some(type_id), Some(type_version), Some(type_digest)) = (
            col_opt_text(&row, "definition_namespace")?,
            col_opt_text(&row, "definition_id")?,
            col_opt_i64(&row, "definition_version")?,
            col_opt_text(&row, "definition_digest")?,
        ) else {
            return Err(RuntimeError::InvalidInput(format!(
                "operation {operation_id} has no pinned type"
            )));
        };
        let get = |id: &str, version: i64| {
            stmt(
                include_str!("../sql/definition-get.sql"),
                vec![text(&type_namespace), text(id), SqlValue::Integer(version)],
            )
        };
        let Some(type_row) = reader.query_row(get(&type_id, type_version)).await? else {
            return Err(RuntimeError::Internal(format!(
                "operation {operation_id}: pinned type {type_id} v{type_version} is missing"
            )));
        };
        let bytes = checked_definition(&type_row, "operation_type", &type_digest)?;
        let definition: OperationType = serde_json::from_slice(&bytes)
            .map_err(|e| RuntimeError::Internal(format!("operation type {type_id}: {e}")))?;
        let mut step_types = HashMap::new();
        for step in &definition.steps {
            let pinned = &step.step_type;
            let Some(step_row) = reader.query_row(get(&pinned.id, pinned.version)).await? else {
                return Err(RuntimeError::Internal(format!(
                    "operation {operation_id}: pinned step type {} v{} is missing",
                    pinned.id, pinned.version
                )));
            };
            let bytes = checked_definition(&step_row, "step_type", &pinned.digest)?;
            let step_type: StepType = serde_json::from_slice(&bytes)
                .map_err(|e| RuntimeError::Internal(format!("step type {}: {e}", pinned.id)))?;
            step_types.insert(step.name.clone(), step_type);
        }
        Ok(Loaded {
            operation_id: col_text(&row, "operation_id")?,
            namespace,
            opened_by: col_text(&row, "opened_by")?,
            opened_at_us: col_i64(&row, "opened_at_us")?,
            parent: col_opt_text(&row, "parent_operation_id")?,
            type_namespace,
            type_digest,
            definition,
            step_types,
        })
    }

    async fn append(
        &self,
        params: &Params,
        operation_id: &str,
        items: Vec<Prepared>,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        let loaded = self.load(operation_id, token).await?;
        let namespace = token.namespace().as_str().to_owned();
        if loaded.namespace != namespace {
            return Err(params.refused(Refusal::new(
                "operation_not_in_write_namespace",
                format!(
                    "operation {operation_id} is recorded in {}, not in {namespace}",
                    loaded.namespace
                ),
            )));
        }
        let mut keys = BTreeSet::new();
        for item in &items {
            if !keys.insert(item.key.clone()) {
                return Err(params.refused(Refusal::new(
                    "duplicate_key",
                    format!("key {} appears twice in one request", item.key),
                )));
            }
            check_item(params, &loaded, item)?;
        }
        let slots: HashMap<String, bool> = loaded
            .definition
            .slots
            .iter()
            .map(|slot| (slot.name.clone(), slot.repeats))
            .collect();
        let act_slots: HashMap<String, String> = loaded
            .step_types
            .iter()
            .map(|(name, step_type)| (name.clone(), step_type.by.clone()))
            .collect();
        let verb = params.verb.clone();
        let unit_verb = verb.clone();
        let unit_id = loaded.operation_id.clone();
        let now = now_us();
        let op: AtomicUnitOp = Box::new(move |writer| {
            Box::pin(async move {
                let rows = writer
                    .query_all(stmt(
                        include_str!("../sql/records-for-operation.sql"),
                        vec![text(&unit_id)],
                    ))
                    .await?;
                let mut bound: HashMap<String, Vec<String>> = HashMap::new();
                for row in &rows {
                    if col_text(row, "kind")? != "binding" {
                        continue;
                    }
                    let payload: Payload =
                        serde_json::from_slice(&col_bytes(row, "payload_bytes")?)
                            .map_err(|e| corrupt(format!("record payload: {e}")))?;
                    if let Payload::Binding { slot, identity } = payload {
                        bound.entry(slot).or_default().push(identity.reference());
                    }
                }
                let mut next = rows.len() as i64 + 1;
                let mut recorded = Vec::with_capacity(items.len());
                for item in &items {
                    let held = writer
                        .query_row(stmt(
                            include_str!("../sql/record-by-key.sql"),
                            vec![text(&unit_id), text(&item.key)],
                        ))
                        .await?;
                    if let Some(row) = held {
                        if col_text(&row, "payload_digest")? != item.digest {
                            return Err(refuse(
                                &unit_verb,
                                Refusal::new(
                                    "caller_key_conflict",
                                    format!("key {} already records a different item", item.key),
                                ),
                            ));
                        }
                        recorded.push(json!({
                            "key": item.key,
                            "sequence": col_i64(&row, "sequence")?,
                            "replayed": true,
                        }));
                        continue;
                    }
                    match &item.payload {
                        Payload::Binding { slot, identity } => {
                            let reference = identity.reference();
                            let held = bound.entry(slot.clone()).or_default();
                            if held.contains(&reference) {
                                return Err(refuse(
                                    &unit_verb,
                                    Refusal::new(
                                        "identity_already_bound",
                                        format!("{reference} is already bound to slot {slot}"),
                                    ),
                                ));
                            }
                            if !slots.get(slot).copied().unwrap_or(false) && !held.is_empty() {
                                return Err(refuse(
                                    &unit_verb,
                                    Refusal::new(
                                        "slot_holds_one",
                                        format!("slot {slot} already holds {}", held[0]),
                                    ),
                                ));
                            }
                            held.push(reference);
                        }
                        Payload::Act {
                            step,
                            identity: Some(identity),
                        } => {
                            let by = act_slots.get(step).cloned().unwrap_or_default();
                            let reference = identity.reference();
                            let held = bound.get(&by).map_or(&[][..], Vec::as_slice);
                            // A repeating slot is asked of each identity bound there; a slot of
                            // one identity is asked of the identity bound there, once one is.
                            let repeats = slots.get(&by).copied().unwrap_or(false);
                            if (repeats || !held.is_empty()) && !held.contains(&reference) {
                                return Err(refuse(
                                    &unit_verb,
                                    Refusal::new(
                                        "act_identity_unbound",
                                        format!("{reference} is not bound to slot {by}, which step {step} is asked of"),
                                    ),
                                ));
                            }
                        }
                        Payload::Act { .. } | Payload::Fact { .. } => {}
                    }
                    insert_record(&mut *writer, &unit_id, &namespace, next, item, now).await?;
                    recorded.push(json!({"key": item.key, "sequence": next, "replayed": false}));
                    next += 1;
                }
                Ok(boxed(
                    json!({"operation_id": unit_id, "recorded": recorded}),
                ))
            })
        });
        let out = self.runtime.sql().atomic_unit(op).await?;
        unit_value(out, &verb)
    }

    async fn frontier(
        &self,
        params: Params,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        let operation_id = params.str("operation")?;
        let as_of = params.time("as_of")?;
        let basis = params
            .opt_str("basis")?
            .unwrap_or_else(|| "observed".to_owned());
        let by_recorded = match basis.as_str() {
            "observed" => false,
            "recorded" => true,
            other => {
                return Err(
                    params.invalid(format!("basis must be observed or recorded, got {other:?}"))
                )
            }
        };
        let group_by = match params.opt_str("group_by")?.as_deref() {
            None | Some("supplier") => GroupBy::Supplier,
            Some("by") => GroupBy::By,
            Some(other) => {
                return Err(
                    params.invalid(format!("group_by must be supplier or by, got {other:?}"))
                )
            }
        };
        let loaded = self.load(&operation_id, token).await?;
        let mut reader = self.runtime.sql().reader().await?;
        let rows = reader
            .query_all(stmt(
                include_str!("../sql/records-for-operation.sql"),
                vec![text(&loaded.operation_id)],
            ))
            .await?;
        drop(reader);
        let mut records: Vec<(i64, i64, Payload)> = Vec::with_capacity(rows.len());
        for row in &rows {
            let observed = col_i64(row, "observed_at_us")?;
            let recorded = col_i64(row, "recorded_at_us")?;
            let time = if by_recorded { recorded } else { observed };
            if as_of.is_some_and(|as_of| time > as_of) {
                continue;
            }
            let payload: Payload = serde_json::from_slice(&col_bytes(row, "payload_bytes")?)
                .map_err(|e| {
                    RuntimeError::Internal(format!("operation {operation_id}: record payload: {e}"))
                })?;
            records.push((time, col_i64(row, "sequence")?, payload));
        }
        records.sort_by_key(|(time, sequence, _)| {
            if by_recorded {
                (*sequence, 0)
            } else {
                (*time, *sequence)
            }
        });
        let applied: Vec<Payload> = records.into_iter().map(|(_, _, payload)| payload).collect();
        let mut view = evaluate(
            &loaded.definition,
            &loaded.step_types,
            &applied,
            &loaded.opened_by,
            group_by,
        )
        .map_err(|message| {
            RuntimeError::Internal(format!("operation {operation_id}: {message}"))
        })?;
        if let Value::Object(map) = &mut view {
            map.insert("operation_id".to_owned(), json!(loaded.operation_id));
            map.insert("namespace".to_owned(), json!(loaded.namespace));
            map.insert("type".to_owned(), loaded.type_view());
            map.insert("parent".to_owned(), json!(loaded.parent));
            map.insert("opened_by".to_owned(), json!(loaded.opened_by));
            map.insert("opened_at".to_owned(), format_time(loaded.opened_at_us));
            map.insert(
                "as_of".to_owned(),
                as_of.map(format_time).unwrap_or(Value::Null),
            );
            map.insert("basis".to_owned(), json!(basis));
            map.insert("records_applied".to_owned(), json!(applied.len()));
        }
        Ok(view)
    }

    async fn resolve(
        &self,
        params: Params,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        let type_id = params.str("type")?;
        let version = params.opt_i64("version")?;
        if version.is_some_and(|v| v < 1) {
            return Err(params.invalid("version must be 1 or more"));
        }
        let type_namespace = params.opt_str("type_namespace")?;
        let form = params.str("form")?;
        let revision = params.opt_str("revision")?;
        let mut reader = self.runtime.sql().reader().await?;
        let rows = reader
            .query_all(stmt(
                include_str!("../sql/definition-find.sql"),
                vec![
                    text(&type_id),
                    text(&visible_namespaces_json(token)),
                    SqlValue::from_opt_text(type_namespace.as_deref()),
                    SqlValue::from_opt_i64(version),
                ],
            ))
            .await?;
        drop(reader);
        let Some(row) = rows.first() else {
            return Err(RuntimeError::NotFound(format!("operation type {type_id}")));
        };
        let namespaces: BTreeSet<String> = rows
            .iter()
            .map(|row| col_text(row, "namespace"))
            .collect::<StorageResult<_>>()?;
        if namespaces.len() > 1 {
            return Err(params.refused(Refusal::new(
                "ambiguous_type",
                format!(
                    "{type_id} is published in {}; name one as type_namespace",
                    namespaces.into_iter().collect::<Vec<_>>().join(", ")
                ),
            )));
        }
        let definition_namespace = col_text(row, "namespace")?;
        let type_digest = col_text(row, "digest")?;
        let bytes = checked_definition(row, "operation_type", &type_digest)?;
        let definition: OperationType = serde_json::from_slice(&bytes)
            .map_err(|e| RuntimeError::Internal(format!("operation type {type_id}: {e}")))?;

        let mut args = json!({ "form": form });
        if let Some(revision) = &revision {
            args["revision"] = json!(revision);
        }
        let declared = forms_read(registry, token, "forms.subjects", args).await?;
        let terms =
            Terms::from_reply(&forms_read(registry, token, "forms.terms", json!({})).await?);
        let form_id = declared["form"]["form_id"]
            .as_str()
            .ok_or_else(|| RuntimeError::Internal("forms.subjects named no form_id".to_owned()))?
            .to_owned();
        let subjects: Vec<Subject> = declared["subjects"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Subject::from_view)
            .collect();

        let mut reader = self.runtime.sql().reader().await?;
        let rows = reader
            .query_all(stmt(
                include_str!("../sql/decisions-for.sql"),
                vec![text(&definition_namespace), text(&type_id), text(&form_id)],
            ))
            .await?;
        drop(reader);
        let decisions: Vec<Decision> = rows
            .iter()
            .map(|row| {
                Ok(Decision {
                    id: col_text(row, "decision_id")?,
                    subjects: subject_ids(row)?,
                    slot: col_opt_text(row, "slot")?,
                })
            })
            .collect::<StorageResult<_>>()?;
        let resolution = resolve(&definition, &terms, &subjects, &decisions);
        Ok(json!({
            "type": {
                "namespace": definition_namespace,
                "id": definition.id,
                "version": definition.version,
                "digest": type_digest,
            },
            "form": declared["form"],
            "subjects": resolution.subjects,
            "lapsed": resolution.lapsed,
            "decisions_read": decisions.len(),
        }))
    }

    async fn decide(
        &self,
        params: Params,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        let type_id = params.str("type")?;
        let form = params.str("form")?;
        let reason = params.str("reason")?;
        let given: Vec<String> = match params.opt_value("subjects") {
            Some(Value::Array(items)) if !items.is_empty() => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .filter(|s| !s.trim().is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| params.invalid("subjects is a list of subject ids"))
                })
                .collect::<Result<_, _>>()?,
            _ => return Err(params.invalid("subjects is required: a list of subject ids")),
        };
        let slot = params.opt_str("slot")?;
        let not_this_type = match params.opt_value("not_this_type") {
            None => false,
            Some(Value::Bool(flag)) => *flag,
            Some(_) => return Err(params.invalid("not_this_type must be true or false")),
        };
        if slot.is_some() == not_this_type {
            return Err(params.invalid("give exactly one of slot and not_this_type"));
        }
        let namespace = token.namespace().as_str().to_owned();
        let mut reader = self.runtime.sql().reader().await?;
        let rows = reader
            .query_all(stmt(
                include_str!("../sql/definition-find.sql"),
                vec![
                    text(&type_id),
                    text(&json!([namespace]).to_string()),
                    text(&namespace),
                    SqlValue::Null,
                ],
            ))
            .await?;
        drop(reader);
        let Some(row) = rows.first() else {
            return Err(params.refused(Refusal::new(
                "unknown_type",
                format!(
                    "{type_id} is not published in {namespace}; a decision is kept beside its type"
                ),
            )));
        };
        let type_digest = col_text(row, "digest")?;
        let bytes = checked_definition(row, "operation_type", &type_digest)?;
        let definition: OperationType = serde_json::from_slice(&bytes)
            .map_err(|e| RuntimeError::Internal(format!("operation type {type_id}: {e}")))?;
        if let Some(slot) = &slot {
            if definition.slot(slot).is_none() {
                return Err(params.refused(Refusal::new(
                    "unknown_slot",
                    format!("{type_id} v{} has no slot {slot}", definition.version),
                )));
            }
        }

        let declared =
            forms_read(registry, token, "forms.subjects", json!({ "form": form })).await?;
        let form_id = declared["form"]["form_id"]
            .as_str()
            .ok_or_else(|| RuntimeError::Internal("forms.subjects named no form_id".to_owned()))?
            .to_owned();
        let form_revision = declared["form"]["revision"]
            .as_str()
            .ok_or_else(|| RuntimeError::Internal("forms.subjects named no revision".to_owned()))?
            .to_owned();
        let ids: Vec<String> = declared["subjects"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s["id"].as_str().map(str::to_owned))
            .collect();
        let mut subjects: Vec<String> = Vec::new();
        for wanted in &given {
            let Some(id) = ids.iter().find(|id| fold(id) == fold(wanted)) else {
                return Err(params.refused(Refusal::new(
                    "unknown_subject",
                    format!("{form_id}@{form_revision} declares no subject {wanted}"),
                )));
            };
            if !subjects.contains(id) {
                subjects.push(id.clone());
            }
        }

        let decision_id = uuid::Uuid::now_v7().to_string();
        let decided_by = token.actor().id.clone();
        let now = now_us();
        let verb = params.verb.clone();
        let (unit_ns, unit_id, unit_type, unit_form) = (
            namespace.clone(),
            decision_id.clone(),
            type_id.clone(),
            form_id.clone(),
        );
        let (unit_subjects, unit_slot, unit_reason, unit_by) = (
            subjects.clone(),
            slot.clone(),
            reason.clone(),
            decided_by.clone(),
        );
        let (unit_version, unit_revision) = (definition.version, form_revision.clone());
        let op: AtomicUnitOp = Box::new(move |writer| {
            Box::pin(async move {
                let earlier = writer
                    .query_all(stmt(
                        include_str!("../sql/decisions-for.sql"),
                        vec![text(&unit_ns), text(&unit_type), text(&unit_form)],
                    ))
                    .await?;
                let mut supersedes = Vec::new();
                for row in &earlier {
                    let named = subject_ids(row)?;
                    if named
                        .iter()
                        .any(|n| unit_subjects.iter().any(|s| fold(s) == fold(n)))
                    {
                        supersedes.push(col_text(row, "decision_id")?);
                    }
                }
                writer
                    .execute(stmt(
                        include_str!("../sql/decision-insert.sql"),
                        vec![
                            text(&unit_ns),
                            text(&unit_id),
                            text(&unit_type),
                            text(&unit_form),
                            text(&json!(unit_subjects).to_string()),
                            SqlValue::Integer(unit_version),
                            text(&unit_revision),
                            SqlValue::from_opt_text(unit_slot.as_deref()),
                            text(&unit_reason),
                            text(&unit_by),
                            SqlValue::Integer(now),
                            text(&json!(supersedes).to_string()),
                        ],
                    ))
                    .await?;
                Ok(boxed(json!(supersedes)))
            })
        });
        let out = self.runtime.sql().atomic_unit(op).await?;
        let supersedes = unit_value(out, &verb)?;
        Ok(json!({
            "decision": {
                "id": decision_id,
                "namespace": namespace,
                "type": {"id": type_id, "version": definition.version},
                "form": {"form_id": form_id, "revision": form_revision},
                "subjects": subjects,
                "slot": slot,
                "not_this_type": not_this_type,
                "reason": reason,
                "decided_by": decided_by,
                "decided_at": format_time(now),
                "supersedes": supersedes,
            }
        }))
    }
}

/// A read of the forms pack, made as the caller.
async fn forms_read(
    registry: &VerbRegistry,
    token: &NamespaceToken,
    verb: &str,
    args: Value,
) -> Result<Value, RuntimeError> {
    Box::pin(registry.dispatch_with_identity(verb, args, Some(RequestIdentity::from_token(token))))
        .await
}

/// Refuse a type whose slot names a kind, role or relation that is not a term of the
/// namespace by name or alias. The terms are read only when a slot names one.
async fn check_terms(
    params: &Params,
    compiled: &[Compiled],
    registry: &VerbRegistry,
    token: &NamespaceToken,
) -> Result<(), RuntimeError> {
    let mut named: Vec<(&str, &str, &str, &str)> = Vec::new();
    for type_ in compiled {
        for slot in &type_.definition.slots {
            for (list, term) in [
                ("kind", slot.kind.as_deref()),
                ("role", slot.role.as_deref()),
                (
                    "relation",
                    slot.defined_by.as_ref().map(|d| d.relation.as_str()),
                ),
            ] {
                if let Some(term) = term {
                    named.push((&type_.definition.id, &slot.name, list, term));
                }
            }
        }
    }
    if named.is_empty() {
        return Ok(());
    }
    let terms = Terms::from_reply(&forms_read(registry, token, "forms.terms", json!({})).await?);
    match named.iter().find(|(_, _, list, term)| !terms.knows(list, term)) {
        Some((type_id, slot, list, term)) => Err(params.refused(Refusal::new(
            "unknown_term",
            format!(
                "{type_id}: slot {slot} names the {list} \"{term}\", which is not a {list} of this namespace by name or alias; load it with forms.load"
            ),
        ))),
        None => Ok(()),
    }
}

fn subject_ids(row: &SqlRow) -> StorageResult<Vec<String>> {
    serde_json::from_str(&col_text(row, "subject_ids")?)
        .map_err(|e| corrupt(format!("column subject_ids: {e}")))
}

fn summary(type_: &Compiled) -> Value {
    json!({
        "id": type_.operation_type.id,
        "version": type_.operation_type.version,
        "digest": type_.operation_type.digest,
        "steps": type_
            .step_types
            .iter()
            .map(|step| json!({"id": step.id, "version": step.version, "digest": step.digest}))
            .collect::<Vec<_>>(),
    })
}

/// True when the definition is already stored with the same digest; a different digest under
/// the same id and version is refused.
async fn held_same(
    writer: &mut dyn SqlWriter,
    verb: &str,
    namespace: &str,
    stored: &crate::definition::Stored,
) -> StorageResult<bool> {
    let held = writer
        .query_row(stmt(
            include_str!("../sql/definition-get.sql"),
            vec![
                text(namespace),
                text(&stored.id),
                SqlValue::Integer(stored.version),
            ],
        ))
        .await?;
    let Some(row) = held else {
        return Ok(false);
    };
    if col_text(&row, "kind")? != stored.kind {
        return Err(refuse(
            verb,
            Refusal::new(
                "kind_conflict",
                format!("{} is published in {namespace} as another kind", stored.id),
            ),
        ));
    }
    let held_digest = col_text(&row, "digest")?;
    if held_digest != stored.digest {
        return Err(refuse(
            verb,
            Refusal::new(
                "version_conflict",
                format!(
                    "{} v{} is published with a different definition ({held_digest}); publish a new version",
                    stored.id, stored.version
                ),
            ),
        ));
    }
    Ok(true)
}

async fn insert_record(
    writer: &mut dyn SqlWriter,
    operation_id: &str,
    namespace: &str,
    sequence: i64,
    record: &Prepared,
    now: i64,
) -> StorageResult<()> {
    writer
        .execute(stmt(
            include_str!("../sql/record-insert.sql"),
            vec![
                text(operation_id),
                SqlValue::Integer(sequence),
                text(namespace),
                text(record.payload.kind()),
                text(&record.key),
                text(&record.digest),
                SqlValue::Blob(record.bytes.clone()),
                SqlValue::Integer(record.observed_at_us.unwrap_or(now)),
                SqlValue::Integer(now),
            ],
        ))
        .await?;
    Ok(())
}
