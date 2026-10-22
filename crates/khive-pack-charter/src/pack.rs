use std::any::Any;
use std::borrow::Cow;
use std::str::FromStr;

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use khive_runtime::{KhiveRuntime, NamespaceToken, PackRuntime, RuntimeError, VerbRegistry};
use khive_storage::note::{FilterOp, PropertyFilter};
use khive_storage::{
    AtomicUnitOp, EdgeFilter, EdgeRelation, NoteFilter, PageRequest, SqlReader, SqlStatement,
    SqlValue, StorageCapability, StorageError, StorageResult,
};
use khive_types::{
    HandlerDef, IdResolutionMode, Pack, PackColumnAddition, PackColumnAffinity, PackSchemaPlan,
    ParamDef, VerbCategory, Visibility,
};

use crate::engine::{
    col_i64, col_json, col_opt_i64, col_opt_text, col_text, decode_definition, waiting_actors,
    Engine, Refusal, Scope,
};
use crate::lang::ast::EntryKind;
use crate::lang::definition::LANGUAGE;
use crate::lang::registry::{EntryDef, ParamType};
use crate::lang::{canonical_json, compile, digest, parse_vocabulary_source, Diagnostic, Registry};

const DEFAULT_DOMAIN: &str = "default";
const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

const CHARTER_COLUMN_ADDITIONS: &[PackColumnAddition] = &[
    PackColumnAddition {
        table: "charter_definitions",
        column: "source_bytes",
        affinity: PackColumnAffinity::Text,
    },
    PackColumnAddition {
        table: "charter_subjects",
        column: "subject_kind",
        affinity: PackColumnAffinity::Text,
    },
    PackColumnAddition {
        table: "charter_subjects",
        column: "subject_ref",
        affinity: PackColumnAffinity::Text,
    },
    PackColumnAddition {
        table: "charter_runs",
        column: "parent_run_id",
        affinity: PackColumnAffinity::Text,
    },
    PackColumnAddition {
        table: "charter_runs",
        column: "terminal_outcome",
        affinity: PackColumnAffinity::Text,
    },
    PackColumnAddition {
        table: "charter_phases",
        column: "round",
        affinity: PackColumnAffinity::Integer,
    },
    PackColumnAddition {
        table: "charter_phases",
        column: "entered_sequence",
        affinity: PackColumnAffinity::Integer,
    },
    PackColumnAddition {
        table: "charter_phases",
        column: "waiting_on",
        affinity: PackColumnAffinity::Text,
    },
];

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

const DOMAIN: ParamDef = param(
    "domain",
    "string",
    false,
    "Policy domain the charter and its runs live in. Default \"default\".",
);
const SOURCE: ParamDef = param(
    "source",
    "string",
    true,
    "Charter or vocabulary source text (ADR-208).",
);
const SUBJECT_KIND: ParamDef = param(
    "subject_kind",
    "string",
    true,
    "A registered subject kind, e.g. \"pull_request\".",
);
const SUBJECT_REF: ParamDef = param(
    "subject_ref",
    "string",
    true,
    "The subject's reference, e.g. \"owner/repo#123\".",
);
const GENERATION: ParamDef = param(
    "generation",
    "string",
    false,
    "The subject's current generation (a pull request head, a release candidate). charter.start, or an observation of a trigger the subject kind lists under new_generation, moves the subject to a new value and a new epoch; any other observation is recorded at the generation it names and counts only while the subject is there.",
);
const REQUEST_ID: ParamDef = param(
    "request_id",
    "string",
    false,
    "Caller-scoped key for a retry. The same key with the same parameters returns the stored result with replayed=true; the same key with different parameters is refused.",
);
const LIMIT: ParamDef = param(
    "limit",
    "integer",
    false,
    "Maximum rows, default 50, at most 500.",
);

pub(crate) static CHARTER_HANDLERS: [HandlerDef; 11] = [
    HandlerDef {
        name: "charter.compile",
        description: "Compile a charter source against the phrase registry and report its definition or every refusal, by line. Writes nothing.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[SOURCE, DOMAIN],
    },
    HandlerDef {
        name: "charter.publish",
        description: "Compile and store a charter version. Republishing identical source is a no-op; a different definition under a published version is refused. Every performed charter must already be published.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[SOURCE, DOMAIN, REQUEST_ID],
    },
    HandlerDef {
        name: "charter.vocabulary",
        description: "Publish a vocabulary of subjects, triggers, phrases and actions beside the built-in one. A name belongs to one vocabulary.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[SOURCE, DOMAIN, REQUEST_ID],
    },
    HandlerDef {
        name: "charter.start",
        description: "Start a run of a published charter on a subject and enter its first phase. An open run of the same charter on the same subject is returned instead of a second one.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            param("charter", "string", true, "Charter id."),
            SUBJECT_KIND,
            SUBJECT_REF,
            param("bindings", "object", false, "Role name to actor id, e.g. {\"author\": \"alice\"}. Only roles the charter declares."),
            param("version", "integer", false, "Charter version; default the latest published."),
            GENERATION,
            DOMAIN,
            REQUEST_ID,
        ],
    },
    HandlerDef {
        name: "charter.observe",
        description: "Record an observation of a registered trigger, phrase or action on a subject and advance every open run on it (or the named charter's run). The engine records; it performs nothing.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            SUBJECT_KIND,
            SUBJECT_REF,
            param("kind", "string", true, "Registry entry name, e.g. \"review.decision\"."),
            param("outcome", "string", false, "One of the entry's outcomes; may be omitted when the entry has exactly one."),
            param("args", "object", false, "Arguments the entry declares, e.g. {\"branch\": \"main\"}."),
            param("detail", "object", false, "Free-form detail stored with the observation."),
            param("producer", "string", false, "Who observed it; default the caller's actor."),
            param("event_id", "string", false, "Source event id; a repeat with the same producer, kind and event id is recorded once."),
            GENERATION,
            param("charter", "string", false, "Only advance this charter's open run on the subject."),
            DOMAIN,
            REQUEST_ID,
        ],
    },
    HandlerDef {
        name: "charter.sample",
        description: "Read a built-in store instrument and record its observation on a subject.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            SUBJECT_KIND,
            SUBJECT_REF,
            param(
                "kind",
                "string",
                true,
                "A phrase or trigger with a built-in store instrument.",
            ),
            param(
                "args",
                "object",
                false,
                "Arguments declared by the instrumented vocabulary entry.",
            ),
            REQUEST_ID,
            DOMAIN,
        ],
    },
    HandlerDef {
        name: "charter.cancel",
        description: "Cancel an open run; its open child runs are cancelled with it and a parent run records the outcome.",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            param("run_id", "string", true, "The run to cancel."),
            param("reason", "string", false, "Recorded with the cancellation."),
            DOMAIN,
            REQUEST_ID,
        ],
    },
    HandlerDef {
        name: "charter.waiting",
        description: "Phases owed to the caller, with a commit-order cursor and the last evaluation summary.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            param("role", "string", false, "Only phases owed by this role."),
            param("actor", "string", false, "Only phases owed by this actor, directly or through a role binding."),
            param("charter", "string", false, "Only runs of this charter."),
            param("subject_kind", "string", false, "Only runs on this subject kind."),
            param("changed_since", "string", false, "Opaque cursor returned by a prior waiting read."),
            DOMAIN,
        ],
    },
    HandlerDef {
        name: "charter.get",
        description: "One run with its phases and its most recent observations.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            param("run_id", "string", true, "The run."),
            param("evidence_limit", "integer", false, "Most recent observations to include, default 50, at most 500."),
            DOMAIN,
        ],
    },
    HandlerDef {
        name: "charter.runs",
        description: "Runs filtered by charter, subject and state, most recently updated first.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            param("charter", "string", false, "Charter id."),
            param("subject_kind", "string", false, "Subject kind."),
            param("subject_ref", "string", false, "Subject reference."),
            param("state", "string", false, "open, completed, terminated, cancelled, superseded, invalidated or failed."),
            LIMIT,
            DOMAIN,
        ],
    },
    HandlerDef {
        name: "charter.list",
        description: "Published charter versions and vocabularies in a domain.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[DOMAIN],
    },
];

/// Charter recording: compiles charter sources, publishes definitions and records runs.
pub struct CharterPack {
    runtime: KhiveRuntime,
}

impl Pack for CharterPack {
    const NAME: &'static str = "charter";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &CHARTER_HANDLERS;
    const SCHEMA_PLAN: Option<PackSchemaPlan> = Some(PackSchemaPlan {
        pack: Self::NAME,
        statements: crate::schema::CHARTER_SCHEMA_STATEMENTS,
    });
    const SCHEMA_COLUMN_ADDITIONS: &'static [PackColumnAddition] = CHARTER_COLUMN_ADDITIONS;
}

impl CharterPack {
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }

    async fn check_subject_scope(
        &self,
        params: &Params,
        token: &NamespaceToken,
    ) -> Result<(), RuntimeError> {
        let subject_kind = params.str("subject_kind")?;
        let subject_ref = params.str("subject_ref")?;
        let scope = params.scope(token)?;
        let Some(subject) = self
            .runtime
            .sql()
            .reader()
            .await?
            .query_row(stmt(
                include_str!("../sql/engine/subject-get.sql"),
                vec![text(&subject_kind), text(&subject_ref)],
            ))
            .await?
        else {
            return Ok(());
        };
        let visible = token.visible_namespace_strs();
        let namespace = col_text(&subject, "namespace")?;
        if col_text(&subject, "policy_domain")? != scope.domain
            || !visible.contains(&namespace.as_str())
        {
            return Err(RuntimeError::NotFound(format!(
                "charter subject {subject_kind}:{subject_ref}"
            )));
        }
        let run_scopes = col_opt_text(&subject, "run_scopes")?.unwrap_or_else(|| "[]".to_owned());
        let run_scopes: Vec<Value> = serde_json::from_str(&run_scopes)
            .map_err(|e| RuntimeError::Internal(format!("charter subject run scopes: {e}")))?;
        if run_scopes.iter().any(|run| {
            run["policy_domain"].as_str() != Some(scope.domain.as_str())
                || run["namespace"]
                    .as_str()
                    .is_none_or(|namespace| !visible.contains(&namespace))
        }) {
            return Err(RuntimeError::NotFound(format!(
                "charter subject {subject_kind}:{subject_ref}"
            )));
        }
        Ok(())
    }

    async fn check_definition_scope(
        &self,
        params: &Params,
        token: &NamespaceToken,
    ) -> Result<(), RuntimeError> {
        let scope = params.scope(token)?;
        let charter = params.str("charter")?;
        let row = match params.opt_i64("version")? {
            Some(version) => {
                self.runtime
                    .sql()
                    .reader()
                    .await?
                    .query_row(stmt(
                        include_str!("../sql/engine/definition-get.sql"),
                        vec![
                            text(&scope.domain),
                            text(&charter),
                            SqlValue::Integer(version),
                        ],
                    ))
                    .await?
            }
            None => {
                self.runtime
                    .sql()
                    .reader()
                    .await?
                    .query_row(stmt(
                        include_str!("../sql/engine/definition-latest.sql"),
                        vec![text(&scope.domain), text(&charter)],
                    ))
                    .await?
            }
        };
        if let Some(row) = row {
            let namespace = col_text(&row, "namespace")?;
            if !token.visible_namespace_strs().contains(&namespace.as_str()) {
                return Err(RuntimeError::NotFound(format!("charter {charter}")));
            }
        }
        Ok(())
    }
}

struct CharterPackFactory;

impl khive_runtime::PackFactory for CharterPackFactory {
    khive_runtime::pack_factory_metadata!(CharterPack);

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        Box::new(CharterPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&CharterPackFactory) }

#[async_trait]
impl PackRuntime for CharterPack {
    khive_runtime::pack_runtime_metadata!();

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        let params = Params::new(verb, params)?;
        if matches!(verb, "charter.start" | "charter.observe" | "charter.sample") {
            self.check_subject_scope(&params, token).await?;
        }
        if verb == "charter.start" {
            self.check_definition_scope(&params, token).await?;
        }
        match verb {
            "charter.compile" => self.compile(params, token).await,
            "charter.publish" => self.publish(params, token).await,
            "charter.vocabulary" => self.vocabulary(params, token).await,
            "charter.start" => self.start(params, token).await,
            "charter.observe" => self.observe(params, token).await,
            "charter.sample" => self.sample(params, token).await,
            "charter.cancel" => self.cancel(params, token).await,
            "charter.waiting" => self.waiting(params, token).await,
            "charter.get" => self.get(params, token).await,
            "charter.runs" => self.runs(params, token).await,
            "charter.list" => self.list(params, token).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "charter pack does not handle verb {verb:?}"
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
            other => {
                return Err(RuntimeError::InvalidInput(format!(
                    "{verb}: parameters must be an object, got {other}"
                )))
            }
        };
        if let Some(handler) = CHARTER_HANDLERS.iter().find(|handler| handler.name == verb) {
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

    fn opt_str(&self, name: &str) -> Result<Option<String>, RuntimeError> {
        match self.map.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
            Some(Value::String(_)) => Err(self.invalid(format!("{name} must not be empty"))),
            Some(other) => Err(self.invalid(format!("{name} must be a string, got {other}"))),
        }
    }

    fn str(&self, name: &str) -> Result<String, RuntimeError> {
        self.opt_str(name)?
            .ok_or_else(|| self.invalid(format!("{name} is required")))
    }

    fn opt_object(&self, name: &str) -> Result<Map<String, Value>, RuntimeError> {
        match self.map.get(name) {
            None | Some(Value::Null) => Ok(Map::new()),
            Some(Value::Object(m)) => Ok(m.clone()),
            Some(other) => Err(self.invalid(format!("{name} must be an object, got {other}"))),
        }
    }

    fn opt_i64(&self, name: &str) -> Result<Option<i64>, RuntimeError> {
        match self.map.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_i64()
                .map(Some)
                .ok_or_else(|| self.invalid(format!("{name} must be an integer, got {v}"))),
        }
    }

    fn limit(&self, name: &str) -> Result<i64, RuntimeError> {
        match self.opt_i64(name)? {
            None => Ok(DEFAULT_LIMIT),
            Some(n) if (1..=MAX_LIMIT).contains(&n) => Ok(n),
            Some(n) => {
                Err(self.invalid(format!("{name} must be between 1 and {MAX_LIMIT}, got {n}")))
            }
        }
    }

    fn domain(&self) -> Result<String, RuntimeError> {
        Ok(self
            .opt_str("domain")?
            .unwrap_or_else(|| DEFAULT_DOMAIN.to_owned()))
    }

    /// The caller's request key and a digest of everything else the request asked, or `None`
    /// when the call carries no `request_id`.
    fn command(&self, scope: &Scope) -> Result<Option<Command>, RuntimeError> {
        let Some(request_id) = self.opt_str("request_id")? else {
            return Ok(None);
        };
        let mut asked = self.map.clone();
        asked.insert("domain".to_owned(), json!(scope.domain));
        asked.insert("namespace".to_owned(), json!(scope.namespace));
        Ok(Some(Command {
            scope: scope.clone(),
            request_id,
            verb: self.verb.clone(),
            request_digest: digest(&canonical_json(
                &json!({"verb": self.verb, "params": asked}),
            )),
        }))
    }

    fn scope(&self, token: &NamespaceToken) -> Result<Scope, RuntimeError> {
        Ok(Scope {
            domain: self.domain()?,
            namespace: token.namespace().as_str().to_owned(),
            actor: token.actor().id.clone(),
            now_us: chrono::Utc::now().timestamp_micros(),
        })
    }
}

fn refusal_text(diagnostics: &[Diagnostic]) -> String {
    diagnostics
        .iter()
        .map(|d| format!("line {} {}: {}", d.line, d.code, d.message))
        .collect::<Vec<_>>()
        .join("; ")
}

fn validate_instrument_args(
    params: &Params,
    entry: &EntryDef,
    args: &Map<String, Value>,
) -> Result<(), RuntimeError> {
    for (name, value) in args {
        let Some(parameter) = entry
            .params
            .iter()
            .find(|parameter| parameter.name == *name)
        else {
            return Err(params.invalid(format!(
                "unknown_argument: {} declares no argument {name}",
                entry.name
            )));
        };
        let valid = match parameter.ty {
            ParamType::String | ParamType::Word => value.is_string(),
            ParamType::Integer | ParamType::Duration => value.as_i64().is_some(),
            ParamType::Bool => value.is_boolean(),
        };
        if !valid {
            return Err(params.invalid(format!(
                "argument_type: {}.{} must be {}, got {value}",
                entry.name,
                parameter.name,
                parameter.ty.as_str()
            )));
        }
    }
    for parameter in &entry.params {
        if !parameter.optional && !args.contains_key(&parameter.name) {
            return Err(params.invalid(format!(
                "missing_argument: {} needs {}",
                entry.name, parameter.name
            )));
        }
    }
    Ok(())
}

fn require_subject_kind(params: &Params, actual: &str, expected: &str) -> Result<(), RuntimeError> {
    if actual == expected {
        Ok(())
    } else {
        Err(params.invalid(format!(
            "wrong_subject_kind: {} samples {expected}, got {actual}",
            params.str("kind")?
        )))
    }
}

fn sample_uuid(
    params: &Params,
    args: &Map<String, Value>,
    name: &str,
) -> Result<uuid::Uuid, RuntimeError> {
    let raw = args
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| params.invalid(format!("{name} must be a UUID string")))?;
    uuid::Uuid::parse_str(raw)
        .map_err(|error| params.invalid(format!("{name} must be a UUID string: {error}")))
}

fn stmt(sql: &str, params: Vec<SqlValue>) -> SqlStatement {
    SqlStatement::new(sql, params)
}

fn text(value: &str) -> SqlValue {
    SqlValue::Text(value.to_owned())
}

fn visible_namespaces_json(token: &NamespaceToken) -> String {
    json!(token.visible_namespace_strs()).to_string()
}

fn parse_waiting_cursor(cursor: &str) -> Option<i64> {
    let hex = cursor.strip_prefix("cc1:")?;
    let sequence = i64::from_str_radix(hex, 16).ok()?;
    (sequence >= 0 && cursor == format!("cc1:{sequence:x}")).then_some(sequence)
}

fn opt_text(value: &Option<String>) -> SqlValue {
    SqlValue::from_opt_text(value.as_deref())
}

/// A refusal raised inside a unit rolls the unit back; it surfaces as the storage refusal.
fn refuse(verb: &str, refusal: Refusal) -> StorageError {
    StorageError::InvalidInput {
        capability: StorageCapability::Sql,
        operation: Cow::Owned(verb.to_owned()),
        message: format!("{}: {}", refusal.code, refusal.message),
    }
}

async fn registry_from<R: SqlReader + ?Sized>(
    reader: &mut R,
    domain: &str,
    namespaces: &str,
) -> StorageResult<Registry> {
    let sources = registry_sources(reader, domain, namespaces).await?;
    let mut registry = Registry::core();
    for source in sources {
        let parsed = parse_vocabulary_source(&source).map_err(|d| {
            StorageError::Internal(format!(
                "charter: stored vocabulary does not parse: {}",
                refusal_text(&d)
            ))
        })?;
        registry.add(&parsed).map_err(|d| {
            StorageError::Internal(format!(
                "charter: stored vocabulary does not load: {}",
                refusal_text(&d)
            ))
        })?;
    }
    Ok(registry)
}

async fn registry_sources<R: SqlReader + ?Sized>(
    reader: &mut R,
    domain: &str,
    namespaces: &str,
) -> StorageResult<Vec<String>> {
    let rows = reader
        .query_all(stmt(
            include_str!("../sql/engine/vocabularies-list.sql"),
            vec![text(domain), text(namespaces)],
        ))
        .await?;
    let mut sources = Vec::with_capacity(rows.len());
    for row in rows {
        let bytes = match row.get("source_bytes") {
            Some(SqlValue::Blob(b)) => b.clone(),
            _ => {
                return Err(StorageError::Internal(
                    "charter: vocabulary source bytes".into(),
                ))
            }
        };
        let source = String::from_utf8(bytes)
            .map_err(|e| StorageError::Internal(format!("charter: vocabulary source: {e}")))?;
        sources.push(source);
    }
    Ok(sources)
}

fn registry_from_snapshot(snapshot: Option<String>) -> StorageResult<Registry> {
    let mut registry = Registry::core();
    let Some(snapshot) = snapshot else {
        return Ok(registry);
    };
    let snapshot: Value = serde_json::from_str(&snapshot)
        .map_err(|e| StorageError::Internal(format!("charter: registry snapshot: {e}")))?;
    let sources = snapshot["vocabularies"]
        .as_array()
        .ok_or_else(|| StorageError::Internal("charter: registry snapshot vocabularies".into()))?;
    for source in sources {
        let source = source
            .as_str()
            .ok_or_else(|| StorageError::Internal("charter: registry snapshot source".into()))?;
        let parsed = parse_vocabulary_source(source).map_err(|d| {
            StorageError::Internal(format!(
                "charter: pinned vocabulary does not parse: {}",
                refusal_text(&d)
            ))
        })?;
        registry.add(&parsed).map_err(|d| {
            StorageError::Internal(format!(
                "charter: pinned vocabulary does not load: {}",
                refusal_text(&d)
            ))
        })?;
    }
    Ok(registry)
}

async fn registry_for_run<R: SqlReader + ?Sized>(
    reader: &mut R,
    run_id: &str,
) -> StorageResult<Registry> {
    let row = reader
        .query_row(stmt(
            include_str!("../sql/engine/definition-source-for-run.sql"),
            vec![text(run_id)],
        ))
        .await?;
    let source = row
        .as_ref()
        .map(|row| col_opt_text(row, "source_bytes"))
        .transpose()?
        .flatten();
    registry_from_snapshot(source)
}

fn validate_observation(
    verb: &str,
    kind: &str,
    entry: Option<&Value>,
    requested_outcome: Option<&str>,
    args: &Map<String, Value>,
) -> StorageResult<String> {
    let invalid = |code: &'static str, message: String| refuse(verb, Refusal::new(code, message));
    let entry = entry.ok_or_else(|| {
        invalid(
            "unknown_kind",
            format!("{kind} is not registered for the target run"),
        )
    })?;
    if entry["kind"] == "subject" {
        return Err(invalid(
            "wrong_entry_kind",
            format!("{kind} is a subject, not an observation"),
        ));
    }
    let outcomes = entry["outcomes"]
        .as_array()
        .ok_or_else(|| StorageError::Internal(format!("charter: {kind} has no outcomes")))?;
    let outcome = match requested_outcome {
        Some(outcome) if outcomes.iter().any(|v| v.as_str() == Some(outcome)) => outcome.to_owned(),
        Some(outcome) => {
            let choices = outcomes
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            return Err(invalid(
                "unknown_outcome",
                format!("{kind} has outcomes {choices}, not {outcome}"),
            ));
        }
        None if outcomes.len() == 1 => outcomes[0]
            .as_str()
            .ok_or_else(|| StorageError::Internal(format!("charter: {kind} outcome")))?
            .to_owned(),
        None => {
            let choices = outcomes
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            return Err(invalid(
                "missing_outcome",
                format!("{kind} needs one of {choices}"),
            ));
        }
    };
    let definitions = entry["params"]
        .as_array()
        .ok_or_else(|| StorageError::Internal(format!("charter: {kind} has no parameters")))?;
    for name in args.keys() {
        if !definitions.iter().any(|param| param["name"] == *name) {
            return Err(invalid(
                "unknown_argument",
                format!("{kind} declares no argument {name}"),
            ));
        }
    }
    for definition in definitions {
        let name = definition["name"]
            .as_str()
            .ok_or_else(|| StorageError::Internal(format!("charter: {kind} parameter name")))?;
        let Some(value) = args.get(name) else {
            if definition["optional"] == false {
                return Err(invalid(
                    "missing_argument",
                    format!("{kind} needs argument {name}"),
                ));
            }
            continue;
        };
        let valid = match definition["type"].as_str() {
            Some("string" | "word") => value.is_string(),
            Some("integer" | "duration") => value.as_i64().is_some() || value.as_u64().is_some(),
            Some("bool") => value.is_boolean(),
            _ => false,
        };
        if !valid {
            return Err(invalid(
                "argument_type",
                format!("{kind}.{name} has the wrong value type"),
            ));
        }
    }
    Ok(outcome)
}

/// A caller-scoped request key (ADR-193 D8).
struct Command {
    scope: Scope,
    request_id: String,
    verb: String,
    request_digest: String,
}

struct Observation {
    subject_kind: String,
    subject_ref: String,
    kind: String,
    requested_outcome: Option<String>,
    args: Map<String, Value>,
    detail: Map<String, Value>,
    generation: Option<String>,
    only_charter: Option<String>,
    producer: String,
    event_id: String,
    sampled: Option<Value>,
}

struct InstrumentContext<'a> {
    params: &'a Params,
    token: &'a NamespaceToken,
    scope: &'a Scope,
    subject_kind: &'a str,
    subject_ref: &'a str,
    kind: &'a str,
    args: &'a Map<String, Value>,
}

/// Run `op` at most once per caller request key. A repeat of the same request returns the stored
/// result; the same key on a different request is refused. The receipt commits with the effect.
fn with_receipt(command: Option<Command>, op: AtomicUnitOp) -> AtomicUnitOp {
    let Some(command) = command else {
        return op;
    };
    Box::new(move |writer| {
        Box::pin(async move {
            let held = writer
                .query_row(stmt(
                    include_str!("../sql/engine/command-get.sql"),
                    vec![
                        text(&command.scope.actor),
                        text(&command.request_id),
                        SqlValue::Integer(0),
                        SqlValue::Null,
                        SqlValue::Null,
                    ],
                ))
                .await?;
            if let Some(row) = held {
                if col_text(&row, "request_digest")? != command.request_digest {
                    return Err(refuse(
                        &command.verb,
                        Refusal::new(
                            "request_id_reused",
                            format!(
                                "request_id {} was used for a different request",
                                command.request_id
                            ),
                        ),
                    ));
                }
                let mut stored = col_json(&row, "result_bytes")?;
                stored["replayed"] = json!(true);
                return Ok(boxed(stored));
            }
            let out = op(&mut *writer).await?;
            let mut result = out.downcast_ref::<Value>().cloned().ok_or_else(|| {
                refuse(&command.verb, Refusal::new("invalid_outcome", "not JSON"))
            })?;
            if command.verb == "charter.start" {
                if let Some(run_id) = result["run_id"].as_str() {
                    result["run"] = run_snapshot(
                        &mut *writer,
                        &command.scope.domain,
                        &command.scope.namespace,
                        run_id,
                    )
                    .await?;
                }
            }
            let run_id = result["run_id"]
                .as_str()
                .or_else(|| result["cancelled"].as_str());
            writer
                .execute(stmt(
                    include_str!("../sql/engine/command-insert.sql"),
                    vec![
                        text(&command.scope.domain),
                        text(&command.scope.actor),
                        text(&command.request_id),
                        text(&command.scope.namespace),
                        text(&command.verb),
                        text(&command.request_digest),
                        SqlValue::Blob(canonical_json(&result)),
                        SqlValue::from_opt_text(run_id),
                        SqlValue::Integer(command.scope.now_us),
                    ],
                ))
                .await?;
            Ok(boxed(result))
        })
    })
}

async fn run_snapshot<R: SqlReader + ?Sized>(
    reader: &mut R,
    domain: &str,
    namespace: &str,
    run_id: &str,
) -> StorageResult<Value> {
    let run = reader
        .query_row(stmt(
            include_str!("../sql/engine/run-get.sql"),
            vec![text(run_id)],
        ))
        .await?
        .ok_or_else(|| StorageError::Internal(format!("charter run {run_id} disappeared")))?;
    if col_text(&run, "policy_domain")? != domain || col_text(&run, "namespace")? != namespace {
        return Err(StorageError::Internal(format!(
            "charter run {run_id} is outside its start scope"
        )));
    }
    let phases = reader
        .query_all(stmt(
            include_str!("../sql/engine/phases-for-run.sql"),
            vec![text(domain), text(run_id)],
        ))
        .await?;
    let mut phase_views = Vec::new();
    for phase in &phases {
        let waiting_on: Value = match col_opt_text(phase, "waiting_on")? {
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|error| StorageError::Internal(format!("charter: waiting_on: {error}")))?,
            None => Value::Null,
        };
        phase_views.push(json!({
            "phase": col_text(phase, "phase_id")?,
            "state": col_text(phase, "state")?,
            "round": col_i64(phase, "round")?,
            "assignee": {"kind": col_text(phase, "assignee_kind")?, "id": col_text(phase, "assignee")?},
            "entered_sequence": col_opt_i64(phase, "entered_sequence")?,
            "waiting_on": waiting_on,
        }));
    }
    Ok(json!({
        "run_id": col_text(&run, "run_id")?,
        "charter": col_text(&run, "charter_id")?,
        "version": col_i64(&run, "definition_version")?,
        "assurance": col_text(&run, "assurance")?,
        "state": col_text(&run, "state")?,
        "current_phase": col_opt_text(&run, "current_phase_id")?,
        "parent_run_id": col_opt_text(&run, "parent_run_id")?,
        "subject": {"kind": col_text(&run, "subject_kind")?, "ref": col_text(&run, "subject_ref")?},
        "subject_epoch": col_i64(&run, "subject_epoch")?,
        "candidate": col_json(&run, "candidate_bytes")?,
        "phases": phase_views,
        "created_at_us": col_i64(&run, "created_at_us")?,
        "updated_at_us": col_i64(&run, "updated_at_us")?,
    }))
}

fn unit_value(result: Box<dyn Any + Send>, verb: &str) -> Result<Value, RuntimeError> {
    result
        .downcast::<Value>()
        .map(|v| *v)
        .map_err(|_| RuntimeError::Internal(format!("{verb}: invalid transaction outcome")))
}

fn boxed(value: Value) -> Box<dyn Any + Send> {
    Box::new(value)
}

impl CharterPack {
    async fn replay_receipt(
        &self,
        params: &Params,
        command: Option<&Command>,
    ) -> Result<Option<Value>, RuntimeError> {
        let Some(command) = command else {
            return Ok(None);
        };
        let mut reader = self.runtime.sql().reader().await?;
        let held = reader
            .query_row(stmt(
                include_str!("../sql/engine/command-get.sql"),
                vec![
                    text(&command.scope.actor),
                    text(&command.request_id),
                    SqlValue::Integer(0),
                    SqlValue::Null,
                    SqlValue::Null,
                ],
            ))
            .await?;
        let Some(row) = held else {
            return Ok(None);
        };
        if col_text(&row, "request_digest")? != command.request_digest {
            return Err(params.invalid(format!(
                "request_id_reused: request_id {} was used for a different request",
                command.request_id
            )));
        }
        let mut result = col_json(&row, "result_bytes")?;
        result["replayed"] = json!(true);
        Ok(Some(result))
    }

    async fn commit_observation(
        &self,
        verb: String,
        scope: Scope,
        command: Option<Command>,
        namespaces: String,
        observation: Observation,
    ) -> Result<Value, RuntimeError> {
        let result_verb = verb.clone();
        let result = self
            .runtime
            .sql()
            .atomic_unit(with_receipt(
                command,
                Box::new(move |writer| {
                    Box::pin(async move {
                        let Observation {
                            subject_kind,
                            subject_ref,
                            kind,
                            requested_outcome,
                            args,
                            detail,
                            generation,
                            only_charter,
                            producer,
                            event_id,
                            sampled,
                        } = observation;
                        let mut engine = Engine::new(writer, scope.clone());
                        let subject_row = engine
                            .writer()
                            .query_row(stmt(
                                include_str!("../sql/engine/subject-get.sql"),
                                vec![text(&subject_kind), text(&subject_ref)],
                            ))
                            .await?;
                        let Some(subject_row) = subject_row else {
                            let registry =
                                registry_from(&mut *engine.writer(), &scope.domain, &namespaces)
                                    .await?;
                            let entry = registry.get(&kind).map(|entry| entry.to_json());
                            validate_observation(
                                &verb,
                                &kind,
                                entry.as_ref(),
                                requested_outcome.as_deref(),
                                &args,
                            )?;
                            let mut result = json!({"recorded": [], "events": []});
                            if let Some(sampled) = sampled {
                                result["sampled"] = json!(true);
                                result["kind"] = sampled["kind"].clone();
                                result["outcome"] = sampled["outcome"].clone();
                                result["producer"] = sampled["producer"].clone();
                            }
                            return Ok(boxed(result));
                        };
                        let subject_id = col_text(&subject_row, "subject_id")?;
                        let mut targets = Vec::new();
                        let mut resolved_outcome: Option<String> = None;
                        let mut establishes = false;
                        for (run_id, charter_id) in
                            engine.open_runs_for_subject(&subject_id).await?
                        {
                            if only_charter.as_ref().is_some_and(|c| c != &charter_id) {
                                continue;
                            }
                            let definition = engine.pinned_definition(&run_id).await?;
                            let pinned_registry =
                                registry_for_run(&mut *engine.writer(), &run_id).await?;
                            let pinned_entry =
                                definition.registry.get(&kind).cloned().or_else(|| {
                                    pinned_registry.get(&kind).map(|entry| entry.to_json())
                                });
                            let outcome = validate_observation(
                                &verb,
                                &kind,
                                pinned_entry.as_ref(),
                                requested_outcome.as_deref(),
                                &args,
                            )?;
                            if resolved_outcome
                                .as_ref()
                                .is_some_and(|held| held != &outcome)
                            {
                                return Err(refuse(
                                    &verb,
                                    Refusal::new(
                                        "ambiguous_outcome",
                                        format!("{kind} resolves differently for open runs"),
                                    ),
                                ));
                            }
                            resolved_outcome.get_or_insert(outcome);
                            establishes |= definition
                                .registry
                                .get(&subject_kind)
                                .and_then(|subject| subject["new_generation"].as_array())
                                .is_some_and(|names| names.iter().any(|name| name == &kind))
                                || pinned_registry
                                    .get(&subject_kind)
                                    .is_some_and(|subject| subject.new_generation.contains(&kind));
                            targets.push((run_id, charter_id));
                        }
                        if targets.is_empty() {
                            let registry =
                                registry_from(&mut *engine.writer(), &scope.domain, &namespaces)
                                    .await?;
                            let entry = registry.get(&kind).map(|entry| entry.to_json());
                            resolved_outcome = Some(validate_observation(
                                &verb,
                                &kind,
                                entry.as_ref(),
                                requested_outcome.as_deref(),
                                &args,
                            )?);
                            establishes = registry
                                .get(&subject_kind)
                                .is_some_and(|subject| subject.new_generation.contains(&kind));
                        }
                        let outcome = resolved_outcome.ok_or_else(|| {
                            StorageError::Internal("charter: missing outcome".into())
                        })?;
                        let replayed_trigger = if establishes {
                            let trigger_payload = json!({
                                "outcome": outcome,
                                "args": args,
                                "detail": detail,
                                "generation": generation,
                            });
                            engine
                                .record_generation_trigger(
                                    &subject_id,
                                    &producer,
                                    &kind,
                                    &event_id,
                                    &trigger_payload,
                                )
                                .await?
                        } else {
                            false
                        };
                        let (subject_id, epoch, current) = engine
                            .subject(
                                &subject_kind,
                                &subject_ref,
                                generation.as_deref(),
                                establishes && !replayed_trigger,
                            )
                            .await?;
                        let payload = json!({
                            "outcome": outcome,
                            "args": args,
                            "detail": detail,
                            "epoch": epoch,
                            "generation": generation.or(current),
                        });
                        let mut recorded = Vec::new();
                        for (run_id, charter_id) in targets {
                            let (sequence, fresh) = engine
                                .observe(
                                    &run_id,
                                    &subject_id,
                                    epoch,
                                    &kind,
                                    &producer,
                                    &event_id,
                                    &payload,
                                )
                                .await?;
                            recorded.push(json!({
                                "run_id": run_id, "charter": charter_id,
                                "sequence": sequence, "already_recorded": !fresh,
                            }));
                        }
                        engine.settle().await?;
                        let mut result = json!({
                            "recorded": recorded,
                            "events": engine.journal.events,
                        });
                        if let Some(sampled) = sampled {
                            result["sampled"] = json!(true);
                            result["kind"] = sampled["kind"].clone();
                            result["outcome"] = sampled["outcome"].clone();
                            result["producer"] = sampled["producer"].clone();
                        }
                        Ok(boxed(result))
                    })
                }),
            ))
            .await?;
        unit_value(result, &result_verb)
    }

    async fn registry(
        &self,
        domain: &str,
        token: &NamespaceToken,
    ) -> Result<Registry, RuntimeError> {
        let mut reader = self.runtime.sql().reader().await?;
        let namespaces = visible_namespaces_json(token);
        Ok(registry_from(reader.as_mut(), domain, &namespaces).await?)
    }

    async fn compile(&self, params: Params, token: &NamespaceToken) -> Result<Value, RuntimeError> {
        let source = params.str("source")?;
        let registry = self.registry(&params.domain()?, token).await?;
        Ok(match compile(&source, &registry) {
            Ok(compiled) => json!({
                "valid": true,
                "charter": compiled.summary(),
                "definition": compiled.definition,
            }),
            Err(diagnostics) => json!({"valid": false, "diagnostics": diagnostics}),
        })
    }

    async fn publish(&self, params: Params, token: &NamespaceToken) -> Result<Value, RuntimeError> {
        let source = params.str("source")?;
        let scope = params.scope(token)?;
        let command = params.command(&scope)?;
        let namespaces = visible_namespaces_json(token);
        let verb = params.verb.clone();
        let result = self
            .runtime
            .sql()
            .atomic_unit(with_receipt(command, Box::new(move |writer| {
                Box::pin(async move {
                    let registry = registry_from(&mut *writer, &scope.domain, &namespaces).await?;
                    let compiled = compile(&source, &registry).map_err(|d| {
                        StorageError::InvalidInput {
                            capability: StorageCapability::Sql,
                            operation: Cow::Owned(verb.clone()),
                            message: format!(
                                "{verb}: charter source refused: {}",
                                refusal_text(&d)
                            ),
                        }
                    })?;
                    let definition = &compiled.definition;
                    let mut engine = Engine::new(writer, scope.clone());
                    for child in definition.phases.iter().flat_map(|p| p.performs.iter()) {
                        if engine.latest_definition(child).await?.is_none() {
                            return Err(refuse(
                                &verb,
                                Refusal::new(
                                    "unpublished_perform",
                                    format!("perform {child}: publish {child} first"),
                                ),
                            ));
                        }
                    }
                    if let Some(path) = engine.perform_cycle(definition).await? {
                        return Err(refuse(
                            &verb,
                            Refusal::new(
                                "perform_cycle",
                                format!("perform dependency cycle: {}", path.join(" -> ")),
                            ),
                        ));
                    }
                    let held = engine
                        .writer()
                        .query_row(stmt(
                            include_str!("../sql/engine/definition-get.sql"),
                            vec![
                                text(&scope.domain),
                                text(&definition.id),
                                SqlValue::Integer(definition.version),
                                text(&namespaces),
                            ],
                        ))
                        .await?;
                    if let Some(row) = held {
                        let held_digest = col_text(&row, "definition_digest")?;
                        if held_digest != compiled.definition_digest {
                            return Err(refuse(
                                &verb,
                                Refusal::new(
                                    "version_conflict",
                                    format!(
                                        "{} v{} is published with a different definition ({held_digest}); publish a new version",
                                        definition.id, definition.version
                                    ),
                                ),
                            ));
                        }
                        return Ok(boxed(json!({
                            "published": false,
                            "already_published": true,
                            "charter": compiled.summary(),
                        })));
                    }
                    let vocabularies =
                        registry_sources(&mut *engine.writer(), &scope.domain, &namespaces).await?;
                    let source_snapshot = String::from_utf8(canonical_json(&json!({
                        "charter_source": source,
                        "vocabularies": vocabularies,
                    })))
                    .map_err(|e| StorageError::Internal(format!("charter: source snapshot: {e}")))?;
                    engine
                        .writer()
                        .execute(stmt(
                            include_str!("../sql/engine/definition-insert.sql"),
                            vec![
                                text(&scope.domain),
                                text(&definition.id),
                                SqlValue::Integer(definition.version),
                                text(&scope.namespace),
                                text(LANGUAGE),
                                text(&compiled.definition_digest),
                                SqlValue::Blob(compiled.definition_bytes.clone()),
                                text(&source_snapshot),
                                text(&compiled.gate_registry_digest),
                                SqlValue::Integer(scope.now_us),
                            ],
                        ))
                        .await?;
                    Ok(boxed(json!({"published": true, "charter": compiled.summary()})))
                })
            })))
            .await?;
        unit_value(result, &params.verb)
    }

    async fn vocabulary(
        &self,
        params: Params,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        let source = params.str("source")?;
        let scope = params.scope(token)?;
        let command = params.command(&scope)?;
        let source_digest = digest(source.as_bytes());
        let namespaces = visible_namespaces_json(token);
        let verb = params.verb.clone();
        let result = self
            .runtime
            .sql()
            .atomic_unit(with_receipt(command, Box::new(move |writer| {
                Box::pin(async move {
                    let parsed = parse_vocabulary_source(&source).map_err(|d| {
                        StorageError::InvalidInput {
                            capability: StorageCapability::Sql,
                            operation: Cow::Owned(verb.clone()),
                            message: format!(
                                "{verb}: vocabulary source refused: {}",
                                refusal_text(&d)
                            ),
                        }
                    })?;
                    let rows = writer
                        .query_all(stmt(
                            include_str!("../sql/engine/vocabularies-list.sql"),
                            vec![text(&scope.domain), text(&namespaces)],
                        ))
                        .await?;
                    for row in &rows {
                        if col_text(row, "name")? == parsed.name
                            && col_i64(row, "version")? == parsed.version
                        {
                            let held = col_text(row, "source_digest")?;
                            if held != source_digest {
                                return Err(refuse(
                                    &verb,
                                    Refusal::new(
                                        "version_conflict",
                                        format!(
                                            "vocabulary {} v{} is published with different source; publish a new version",
                                            parsed.name, parsed.version
                                        ),
                                    ),
                                ));
                            }
                            return Ok(boxed(json!({
                                "published": false,
                                "already_published": true,
                                "vocabulary": parsed.name,
                                "version": parsed.version,
                            })));
                        }
                    }
                    let mut registry = registry_from(&mut *writer, &scope.domain, &namespaces).await?;
                    let added = registry.add(&parsed).map_err(|d| {
                        refuse(
                            &verb,
                            Refusal::new("vocabulary_refused", refusal_text(&d)),
                        )
                    })?;
                    writer
                        .execute(stmt(
                            include_str!("../sql/engine/vocabulary-insert.sql"),
                            vec![
                                text(&scope.domain),
                                text(&parsed.name),
                                SqlValue::Integer(parsed.version),
                                text(&scope.namespace),
                                text(&source_digest),
                                SqlValue::Blob(source.into_bytes()),
                                SqlValue::Integer(scope.now_us),
                            ],
                        ))
                        .await?;
                    Ok(boxed(json!({
                        "published": true,
                        "vocabulary": parsed.name,
                        "version": parsed.version,
                        "entries": added.iter().map(|e| e.to_json()).collect::<Vec<_>>(),
                    })))
                })
            })))
            .await?;
        unit_value(result, &params.verb)
    }

    async fn start(&self, params: Params, token: &NamespaceToken) -> Result<Value, RuntimeError> {
        let charter_id = params.str("charter")?;
        let subject_kind = params.str("subject_kind")?;
        let subject_ref = params.str("subject_ref")?;
        let bindings = params.opt_object("bindings")?;
        for (role, actor) in &bindings {
            if actor.as_str().is_none_or(str::is_empty) {
                return Err(params.invalid(format!(
                    "binding for role {role} must be a non-empty actor id string"
                )));
            }
        }
        let version = params.opt_i64("version")?;
        let generation = params.opt_str("generation")?;
        let scope = params.scope(token)?;
        let command = params.command(&scope)?;
        let verb = params.verb.clone();
        let view_scope = scope.clone();
        let result = self
            .runtime
            .sql()
            .atomic_unit(with_receipt(
                command,
                Box::new(move |writer| {
                    Box::pin(async move {
                        let mut engine = Engine::new(writer, scope.clone());
                        let found = match version {
                            Some(v) => {
                                engine
                                    .writer()
                                    .query_row(stmt(
                                        include_str!("../sql/engine/definition-get.sql"),
                                        vec![
                                            text(&scope.domain),
                                            text(&charter_id),
                                            SqlValue::Integer(v),
                                        ],
                                    ))
                                    .await?
                            }
                            None => {
                                engine
                                    .writer()
                                    .query_row(stmt(
                                        include_str!("../sql/engine/definition-latest.sql"),
                                        vec![text(&scope.domain), text(&charter_id)],
                                    ))
                                    .await?
                            }
                        };
                        let Some(row) = found else {
                            return Err(refuse(
                                &verb,
                                Refusal::new(
                                    "not_published",
                                    format!(
                                    "charter {charter_id} has no published version in domain {}",
                                    scope.domain
                                ),
                                ),
                            ));
                        };
                        let definition = decode_definition(&row)?;
                        let definition_version = col_i64(&row, "version")?;
                        let definition_digest = col_text(&row, "definition_digest")?;
                        if definition.subject != subject_kind {
                            return Err(refuse(
                                &verb,
                                Refusal::new(
                                    "subject_mismatch",
                                    format!(
                                    "charter {charter_id} runs on {} subjects, not {subject_kind}",
                                    definition.subject
                                ),
                                ),
                            ));
                        }
                        if let Some(role) = bindings
                            .keys()
                            .find(|role| !definition.roles.iter().any(|r| &r.name == *role))
                        {
                            return Err(refuse(
                                &verb,
                                Refusal::new(
                                    "unknown_role",
                                    format!("charter {charter_id} declares no role {role}"),
                                ),
                            ));
                        }
                        let (subject_id, epoch, current) = engine
                            .subject(&subject_kind, &subject_ref, generation.as_deref(), false)
                            .await?;
                        if generation
                            .as_deref()
                            .is_some_and(|requested| current.as_deref() != Some(requested))
                        {
                            return Err(refuse(
                                &verb,
                                Refusal::new(
                                    "generation_mismatch",
                                    format!(
                                        "subject {subject_kind}:{subject_ref} is at generation {current:?}, not {:?}",
                                        generation.as_deref().unwrap_or_default()
                                    ),
                                ),
                            ));
                        }
                        if let Some(run_id) = engine
                            .open_run_for_charter(&charter_id, &subject_id)
                            .await?
                        {
                            return Ok(boxed(json!({"started": false, "run_id": run_id})));
                        }
                        let run_id = engine
                            .start_run(
                                definition_version,
                                &definition_digest,
                                &definition,
                                (&subject_id, epoch, current.as_deref()),
                                &bindings,
                                None,
                            )
                            .await?;
                        engine.settle().await?;
                        Ok(boxed(json!({
                            "started": true,
                            "run_id": run_id,
                            "events": engine.journal.events,
                        })))
                    })
                }),
            ))
            .await?;
        let mut out = unit_value(result, &params.verb)?;
        if let Some(run_id) = out["run_id"].as_str().map(str::to_owned) {
            out["run"] = self.run_view_for_start(&view_scope, &run_id, 0).await?;
        }
        Ok(out)
    }

    async fn observe(&self, params: Params, token: &NamespaceToken) -> Result<Value, RuntimeError> {
        let subject_kind = params.str("subject_kind")?;
        let subject_ref = params.str("subject_ref")?;
        let kind = params.str("kind")?;
        let args = params.opt_object("args")?;
        let detail = params.opt_object("detail")?;
        let requested_outcome = params.opt_str("outcome")?;
        let generation = params.opt_str("generation")?;
        let only_charter = params.opt_str("charter")?;
        let scope = params.scope(token)?;
        let namespaces = visible_namespaces_json(token);
        let command = params.command(&scope)?;
        let verb = params.verb.clone();
        let producer = params
            .opt_str("producer")?
            .unwrap_or_else(|| scope.actor.clone());
        if producer == crate::engine::ENGINE_PRODUCER || producer == crate::engine::STORE_PRODUCER {
            return Err(params.invalid(format!(
                "producer {producer:?} is reserved for observations the charter pack records itself"
            )));
        }
        let event_id = params
            .opt_str("event_id")?
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        self.commit_observation(
            verb,
            scope,
            command,
            namespaces,
            Observation {
                subject_kind,
                subject_ref,
                kind,
                requested_outcome,
                args,
                detail,
                generation,
                only_charter,
                producer,
                event_id,
                sampled: None,
            },
        )
        .await
    }

    async fn sample(&self, params: Params, token: &NamespaceToken) -> Result<Value, RuntimeError> {
        let subject_kind = params.str("subject_kind")?;
        let subject_ref = params.str("subject_ref")?;
        let kind = params.str("kind")?;
        let args = params.opt_object("args")?;
        let scope = params.scope(token)?;
        let namespaces = visible_namespaces_json(token);
        let command = params.command(&scope)?;
        if let Some(replay) = self.replay_receipt(&params, command.as_ref()).await? {
            return Ok(replay);
        }

        let registry = self.registry(&scope.domain, token).await?;
        let subject = registry.get(&subject_kind).ok_or_else(|| {
            params.invalid(format!("unknown_subject: {subject_kind} is not registered"))
        })?;
        if subject.kind != EntryKind::Subject {
            return Err(params.invalid(format!(
                "wrong_entry_kind: {subject_kind} is not a subject kind"
            )));
        }
        let entry = registry.get(&kind).ok_or_else(|| {
            params.invalid(format!(
                "unknown_kind: {kind} is not a registered trigger or phrase"
            ))
        })?;
        if !matches!(entry.kind, EntryKind::Trigger | EntryKind::Phrase) {
            return Err(params.invalid(format!(
                "no_store_instrument: {kind} has no built-in store instrument"
            )));
        }
        validate_instrument_args(&params, entry, &args)?;
        let outcome = self
            .instrument(InstrumentContext {
                params: &params,
                token,
                scope: &scope,
                subject_kind: &subject_kind,
                subject_ref: &subject_ref,
                kind: &kind,
                args: &args,
            })
            .await?;
        if !entry.outcomes.contains(&outcome) {
            return Err(params.invalid(format!(
                "invalid_instrument_outcome: {kind} produced {outcome}, expected {}",
                entry.outcomes.join(" ")
            )));
        }

        let sampled =
            json!({"kind": kind, "outcome": outcome, "producer": crate::engine::STORE_PRODUCER});
        self.commit_observation(
            params.verb.clone(),
            scope,
            command,
            namespaces,
            Observation {
                subject_kind,
                subject_ref,
                kind,
                requested_outcome: Some(outcome),
                args,
                detail: Map::new(),
                generation: None,
                only_charter: None,
                producer: crate::engine::STORE_PRODUCER.to_owned(),
                event_id: uuid::Uuid::new_v4().to_string(),
                sampled: Some(sampled),
            },
        )
        .await
    }

    async fn instrument(&self, context: InstrumentContext<'_>) -> Result<String, RuntimeError> {
        let InstrumentContext {
            params,
            token,
            scope,
            subject_kind,
            subject_ref,
            kind,
            args,
        } = context;
        match kind {
            "inbox.unread_older_than" => {
                let age = args
                    .get("age")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| params.invalid("age must be a duration in seconds"))?;
                if age < 0 {
                    return Err(params.invalid("age must be non-negative"));
                }
                let cutoff = scope.now_us.saturating_sub(age.saturating_mul(1_000_000));
                let filter = NoteFilter {
                    kind: Some("message".to_owned()),
                    max_created_at: Some(cutoff),
                    property_filters: vec![
                        PropertyFilter {
                            json_path: "$.direction".to_owned(),
                            op: FilterOp::Eq,
                            value: SqlValue::Text("inbound".to_owned()),
                        },
                        PropertyFilter {
                            json_path: "$.read".to_owned(),
                            op: FilterOp::JsonTypeNeMissing,
                            value: SqlValue::Text("true".to_owned()),
                        },
                        PropertyFilter {
                            json_path: "$.to_actor".to_owned(),
                            op: FilterOp::Eq,
                            value: SqlValue::Text(scope.actor.clone()),
                        },
                    ],
                    ..NoteFilter::default()
                };
                let notes = self.runtime.notes(token)?;
                let counts = notes
                    .count_notes_filtered_bounded_in_snapshot(
                        token.namespace().as_str(),
                        &[filter],
                        1,
                    )
                    .await?;
                Ok(if counts.first().is_some_and(|count| count.count > 0) {
                    "some"
                } else {
                    "none"
                }
                .to_owned())
            }
            "task.status" => {
                require_subject_kind(params, subject_kind, "task")?;
                let note = self
                    .note_for_sample(params, token, subject_ref, "task")
                    .await?;
                let status = note
                    .properties
                    .as_ref()
                    .and_then(|properties| properties.get("status"))
                    .and_then(Value::as_str)
                    .unwrap_or("inbox");
                if ![
                    "inbox",
                    "next",
                    "waiting",
                    "someday",
                    "active",
                    "done",
                    "cancelled",
                ]
                .contains(&status)
                {
                    return Err(params.invalid(format!(
                        "unknown_store_outcome: task status {status:?} is not registered"
                    )));
                }
                Ok(status.to_owned())
            }
            "graph.edge_present" => {
                require_subject_kind(params, subject_kind, "graph_edge")?;
                let source = sample_uuid(params, args, "source_id")?;
                let target = sample_uuid(params, args, "target_id")?;
                let relation = args
                    .get("relation")
                    .and_then(Value::as_str)
                    .ok_or_else(|| params.invalid("relation must be a graph relation"))?;
                let relation = EdgeRelation::from_str(relation)
                    .map_err(|error| params.invalid(format!("invalid relation: {error}")))?;
                let edges = self
                    .runtime
                    .graph(token)?
                    .query_edges(
                        EdgeFilter {
                            source_ids: vec![source],
                            target_ids: vec![target],
                            relations: vec![relation],
                            ..EdgeFilter::default()
                        },
                        Vec::new(),
                        PageRequest {
                            offset: 0,
                            limit: 1,
                        },
                    )
                    .await?;
                Ok(if edges.items.is_empty() {
                    "absent"
                } else {
                    "present"
                }
                .to_owned())
            }
            "message.arrived" => {
                require_subject_kind(params, subject_kind, "message")?;
                self.note_for_sample(params, token, subject_ref, "message")
                    .await?;
                Ok("observed".to_owned())
            }
            "schedule.fired" => {
                require_subject_kind(params, subject_kind, "scheduled_event")?;
                let note = self
                    .note_for_sample(params, token, subject_ref, "scheduled_event")
                    .await?;
                let fired = note
                    .properties
                    .as_ref()
                    .and_then(|properties| properties.get("status"))
                    .and_then(Value::as_str)
                    == Some("fired");
                if !fired {
                    return Err(params.invalid(format!(
                        "store_state_not_observed: scheduled event {subject_ref} has not fired"
                    )));
                }
                Ok("observed".to_owned())
            }
            "memory.recorded" => {
                require_subject_kind(params, subject_kind, "memory")?;
                Ok(if self
                    .note_for_sample_optional(params, token, subject_ref, "memory")
                    .await?
                    .is_some()
                {
                    "pass"
                } else {
                    "fail"
                }
                .to_owned())
            }
            "loop.log_written" => {
                require_subject_kind(params, subject_kind, "session")?;
                Ok(if self
                    .note_for_sample_optional(params, token, subject_ref, "session")
                    .await?
                    .is_some()
                {
                    "pass"
                } else {
                    "fail"
                }
                .to_owned())
            }
            _ => Err(params.invalid(format!(
                "no_store_instrument: {kind} has no built-in store instrument"
            ))),
        }
    }

    async fn note_for_sample(
        &self,
        params: &Params,
        token: &NamespaceToken,
        subject_ref: &str,
        expected_kind: &str,
    ) -> Result<khive_storage::Note, RuntimeError> {
        self.note_for_sample_optional(params, token, subject_ref, expected_kind)
            .await?
            .ok_or_else(|| {
                params.invalid(format!(
                    "no_store_state: {expected_kind} record {subject_ref} was not found"
                ))
            })
    }

    async fn note_for_sample_optional(
        &self,
        params: &Params,
        token: &NamespaceToken,
        subject_ref: &str,
        expected_kind: &str,
    ) -> Result<Option<khive_storage::Note>, RuntimeError> {
        let id = uuid::Uuid::parse_str(subject_ref).map_err(|error| {
            params.invalid(format!("subject_ref must be a record UUID: {error}"))
        })?;
        let note = self.runtime.notes(token)?.get_note(id).await?;
        Ok(note.filter(|note| note.kind == expected_kind))
    }

    async fn cancel(&self, params: Params, token: &NamespaceToken) -> Result<Value, RuntimeError> {
        let run_id = params.str("run_id")?;
        let reason = params.opt_str("reason")?;
        let scope = params.scope(token)?;
        let command = params.command(&scope)?;
        let visible_namespaces = token
            .visible_namespace_strs()
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let verb = params.verb.clone();
        let result = self
            .runtime
            .sql()
            .atomic_unit(with_receipt(
                command,
                Box::new(move |writer| {
                    Box::pin(async move {
                        let mut engine = Engine::new(writer, scope.clone());
                        let row = engine
                            .writer()
                            .query_row(stmt(
                                include_str!("../sql/engine/run-get.sql"),
                                vec![text(&run_id)],
                            ))
                            .await?;
                        let Some(row) = row else {
                            return Err(refuse(
                                &verb,
                                Refusal::new("unknown_run", format!("no run {run_id}")),
                            ));
                        };
                        let state = col_text(&row, "state")?;
                        let namespace = col_text(&row, "namespace")?;
                        if col_text(&row, "policy_domain")? != scope.domain
                            || !visible_namespaces
                                .iter()
                                .any(|visible| visible == &namespace)
                        {
                            return Err(refuse(
                                &verb,
                                Refusal::new("unknown_run", format!("no visible run {run_id}")),
                            ));
                        }
                        if state != "open" {
                            return Err(refuse(
                                &verb,
                                Refusal::new(
                                    "not_open",
                                    format!(
                                        "run {run_id} is {state} in domain {}",
                                        col_text(&row, "policy_domain")?
                                    ),
                                ),
                            ));
                        }
                        engine.cancel(&run_id, reason);
                        engine.settle().await?;
                        Ok(boxed(
                            json!({"cancelled": run_id, "events": engine.journal.events}),
                        ))
                    })
                }),
            ))
            .await?;
        unit_value(result, &params.verb)
    }

    async fn waiting(&self, params: Params, token: &NamespaceToken) -> Result<Value, RuntimeError> {
        let role = params.opt_str("role")?;
        let actor = params.opt_str("actor")?;
        let changed_since = params.opt_str("changed_since")?;
        let sequence = match changed_since.as_deref() {
            Some(cursor) => Some(parse_waiting_cursor(cursor).ok_or_else(|| {
                params.invalid("changed_since must be a cursor returned by charter.waiting")
            })?),
            None => None,
        };
        let rows = self
            .runtime
            .sql()
            .reader()
            .await?
            .query_all(stmt(
                include_str!("../sql/engine/waiting.sql"),
                vec![
                    text(&params.domain()?),
                    text(&visible_namespaces_json(token)),
                    opt_text(&params.opt_str("charter")?),
                    opt_text(&params.opt_str("subject_kind")?),
                    SqlValue::from_opt_i64(sequence),
                ],
            ))
            .await?;
        let mut items = Vec::new();
        let mut cursor_order = 0;
        for row in rows {
            cursor_order = col_i64(&row, "cursor_order")?;
            if col_i64(&row, "cursor_only")? == 1 {
                continue;
            }
            let view = col_json(&row, "view_bytes")?;
            let candidate = view["candidate"].clone();
            let bindings = candidate["bindings"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            let waiting_on = view["waiting_on"].clone();
            let assignee_kind = view["assignee_kind"].as_str().unwrap_or("");
            let assignee = view["assignee"].as_str().unwrap_or("");
            let owner = (assignee_kind == "role").then_some(assignee);
            let mut owed_by = waiting_actors(&waiting_on, owner, &bindings);
            if assignee_kind == "actor" {
                owed_by.push(format!("actor:{assignee}"));
            }
            let Some(caller) = token.actor().binding_id() else {
                continue;
            };
            if !owed_by.contains(&format!("actor:{caller}"))
                || role
                    .as_ref()
                    .is_some_and(|r| !owed_by.contains(&format!("role:{r}")))
                || actor
                    .as_ref()
                    .is_some_and(|a| !owed_by.contains(&format!("actor:{a}")))
            {
                continue;
            }
            items.push(json!({
                "run_id": view["run_id"],
                "charter": view["charter"],
                "version": view["version"],
                "assurance": view["assurance"],
                "parent_run_id": view["parent_run_id"],
                "subject": view["subject"],
                "phase": view["phase"],
                "state": view["state"],
                "round": view["round"],
                "assignee": {"kind": assignee_kind, "id": assignee},
                "owed_by": owed_by,
                "waiting_on": waiting_on,
                "since_us": view["since_us"],
                "revision": view["revision"],
                "evaluation": view["evaluation"],
            }));
        }
        if sequence.is_some_and(|sequence| sequence > cursor_order) {
            return Err(params.invalid("changed_since is not a cursor issued by this store"));
        }
        Ok(json!({
            "count": items.len(),
            "waiting": items,
            "cursor": format!("cc1:{cursor_order:x}"),
        }))
    }

    async fn run_view_for_start(
        &self,
        scope: &Scope,
        run_id: &str,
        evidence_limit: i64,
    ) -> Result<Value, RuntimeError> {
        let mut reader = self.runtime.sql().reader().await?;
        if let Some(receipt) = reader
            .query_row(stmt(
                include_str!("../sql/engine/command-get.sql"),
                vec![
                    text(&scope.actor),
                    text(run_id),
                    SqlValue::Integer(1),
                    text(&scope.domain),
                    text(&json!([scope.namespace]).to_string()),
                ],
            ))
            .await?
        {
            if col_text(&receipt, "verb")? == "charter.start" {
                let stored = col_json(&receipt, "result_bytes")?;
                if let Some(snapshot) = stored.get("run") {
                    return Ok(snapshot.clone());
                }
            }
        }
        let visible = vec![scope.namespace.clone()];
        self.run_view_with_visible(&scope.domain, run_id, evidence_limit, Some(&visible))
            .await
    }

    async fn run_view_scoped(
        &self,
        domain: &str,
        run_id: &str,
        evidence_limit: i64,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        let visible = token
            .visible_namespace_strs()
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        self.run_view_with_visible(domain, run_id, evidence_limit, Some(&visible))
            .await
    }

    async fn run_view_with_visible(
        &self,
        domain: &str,
        run_id: &str,
        evidence_limit: i64,
        visible_namespaces: Option<&[String]>,
    ) -> Result<Value, RuntimeError> {
        let mut reader = self.runtime.sql().reader().await?;
        let run = reader
            .query_row(stmt(
                include_str!("../sql/engine/run-get.sql"),
                vec![text(run_id)],
            ))
            .await?;
        let Some(run) = run else {
            return Err(RuntimeError::NotFound(format!("charter run {run_id}")));
        };
        let row_domain = col_text(&run, "policy_domain")?;
        let run_namespace = col_text(&run, "namespace")?;
        if row_domain != domain
            || visible_namespaces
                .is_some_and(|visible| !visible.iter().any(|namespace| namespace == &run_namespace))
        {
            return Err(RuntimeError::NotFound(format!("charter run {run_id}")));
        }
        let phases = reader
            .query_all(stmt(
                include_str!("../sql/engine/phases-for-run.sql"),
                vec![text(domain), text(run_id)],
            ))
            .await?;
        let mut phase_views = Vec::new();
        for p in &phases {
            let waiting_on: Value = match col_opt_text(p, "waiting_on")? {
                Some(raw) => serde_json::from_str(&raw)
                    .map_err(|e| RuntimeError::Internal(format!("charter: waiting_on: {e}")))?,
                None => Value::Null,
            };
            phase_views.push(json!({
                "phase": col_text(p, "phase_id")?,
                "state": col_text(p, "state")?,
                "round": col_i64(p, "round")?,
                "assignee": {"kind": col_text(p, "assignee_kind")?, "id": col_text(p, "assignee")?},
                "entered_sequence": col_opt_i64(p, "entered_sequence")?,
                "waiting_on": waiting_on,
            }));
        }
        let mut view = json!({
            "run_id": col_text(&run, "run_id")?,
            "charter": col_text(&run, "charter_id")?,
            "version": col_i64(&run, "definition_version")?,
                "assurance": col_text(&run, "assurance")?,
            "state": col_text(&run, "state")?,
            "current_phase": col_opt_text(&run, "current_phase_id")?,
            "parent_run_id": col_opt_text(&run, "parent_run_id")?,
            "subject": {"kind": col_text(&run, "subject_kind")?, "ref": col_text(&run, "subject_ref")?},
            "subject_epoch": col_i64(&run, "subject_epoch")?,
            "candidate": col_json(&run, "candidate_bytes")?,
            "phases": phase_views,
            "created_at_us": col_i64(&run, "created_at_us")?,
            "updated_at_us": col_i64(&run, "updated_at_us")?,
        });
        if evidence_limit > 0 {
            let rows = reader
                .query_all(stmt(
                    include_str!("../sql/engine/evidence-recent.sql"),
                    vec![
                        text(domain),
                        text(run_id),
                        SqlValue::Integer(evidence_limit),
                    ],
                ))
                .await?;
            let mut evidence = Vec::new();
            for r in rows.iter().rev() {
                evidence.push(json!({
                    "sequence": col_i64(r, "sequence")?,
                    "kind": col_text(r, "kind")?,
                    "producer": col_text(r, "producer")?,
                    "event_id": col_text(r, "source_event_id")?,
                    "payload": col_json(r, "payload_bytes")?,
                    "received_at_us": col_i64(r, "received_at_us")?,
                }));
            }
            view["evidence"] = Value::Array(evidence);
        }
        Ok(view)
    }

    async fn get(&self, params: Params, token: &NamespaceToken) -> Result<Value, RuntimeError> {
        let run_id = params.str("run_id")?;
        let limit = params.limit("evidence_limit")?;
        self.run_view_scoped(&params.domain()?, &run_id, limit, token)
            .await
    }

    async fn runs(&self, params: Params, token: &NamespaceToken) -> Result<Value, RuntimeError> {
        let rows = self
            .runtime
            .sql()
            .reader()
            .await?
            .query_all(stmt(
                include_str!("../sql/engine/runs-list.sql"),
                vec![
                    text(&params.domain()?),
                    text(&visible_namespaces_json(token)),
                    opt_text(&params.opt_str("charter")?),
                    opt_text(&params.opt_str("subject_kind")?),
                    opt_text(&params.opt_str("subject_ref")?),
                    opt_text(&params.opt_str("state")?),
                    SqlValue::Integer(params.limit("limit")?),
                ],
            ))
            .await?;
        let mut runs = Vec::new();
        for r in &rows {
            runs.push(json!({
                "run_id": col_text(r, "run_id")?,
                "charter": col_text(r, "charter_id")?,
                "version": col_i64(r, "definition_version")?,
                "assurance": col_text(r, "assurance")?,
                "state": col_text(r, "state")?,
                "current_phase": col_opt_text(r, "current_phase_id")?,
                "parent_run_id": col_opt_text(r, "parent_run_id")?,
                "subject": {"kind": col_text(r, "subject_kind")?, "ref": col_text(r, "subject_ref")?},
                "updated_at_us": col_i64(r, "updated_at_us")?,
            }));
        }
        Ok(json!({"count": runs.len(), "runs": runs}))
    }

    async fn list(&self, params: Params, token: &NamespaceToken) -> Result<Value, RuntimeError> {
        let domain = params.domain()?;
        let namespaces = visible_namespaces_json(token);
        let mut reader = self.runtime.sql().reader().await?;
        let charters = reader
            .query_all(stmt(
                include_str!("../sql/engine/definitions-list.sql"),
                vec![text(&domain), text(&namespaces)],
            ))
            .await?;
        let vocabularies = reader
            .query_all(stmt(
                include_str!("../sql/engine/vocabularies-list.sql"),
                vec![text(&domain), text(&namespaces)],
            ))
            .await?;
        let mut charter_rows = Vec::new();
        for r in &charters {
            charter_rows.push(json!({
                "charter": col_text(r, "charter_id")?,
                "version": col_i64(r, "version")?,
                "definition_digest": col_text(r, "definition_digest")?,
                "gate_registry_digest": col_text(r, "gate_registry_digest")?,
                "created_at_us": col_i64(r, "created_at_us")?,
            }));
        }
        let mut vocabulary_rows =
            vec![json!({"vocabulary": "core", "version": 1, "built_in": true})];
        for r in &vocabularies {
            vocabulary_rows.push(json!({
                "vocabulary": col_text(r, "name")?,
                "version": col_i64(r, "version")?,
                "source_digest": col_text(r, "source_digest")?,
            }));
        }
        Ok(json!({"domain": domain, "charters": charter_rows, "vocabularies": vocabulary_rows}))
    }
}
