use std::any::Any;
use std::borrow::Cow;

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use khive_runtime::{KhiveRuntime, NamespaceToken, PackRuntime, RuntimeError, VerbRegistry};
use khive_storage::{
    AtomicUnitOp, SqlReader, SqlStatement, SqlValue, StorageCapability, StorageError, StorageResult,
};
use khive_types::{
    HandlerDef, IdResolutionMode, Pack, PackSchemaPlan, ParamDef, VerbCategory, Visibility,
};

use crate::engine::{
    col_i64, col_json, col_opt_i64, col_opt_text, col_text, decode_definition, waiting_actors,
    Engine, Refusal, Scope,
};
use crate::lang::ast::EntryKind;
use crate::lang::definition::LANGUAGE;
use crate::lang::{canonical_json, compile, digest, parse_vocabulary_source, Diagnostic, Registry};

const DEFAULT_DOMAIN: &str = "default";
const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

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

pub(crate) static CHARTER_HANDLERS: [HandlerDef; 10] = [
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
        description: "Every open run's current phase and what it waits on: the pending observations, the holding requirements and the role or actor who owes them.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            param("role", "string", false, "Only phases owed by this role."),
            param("actor", "string", false, "Only phases owed by this actor, directly or through a role binding."),
            param("charter", "string", false, "Only runs of this charter."),
            param("subject_kind", "string", false, "Only runs on this subject kind."),
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
}

impl CharterPack {
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
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
        match verb {
            "charter.compile" => self.compile(params).await,
            "charter.publish" => self.publish(params, token).await,
            "charter.vocabulary" => self.vocabulary(params, token).await,
            "charter.start" => self.start(params, token).await,
            "charter.observe" => self.observe(params, token).await,
            "charter.cancel" => self.cancel(params, token).await,
            "charter.waiting" => self.waiting(params).await,
            "charter.get" => self.get(params).await,
            "charter.runs" => self.runs(params).await,
            "charter.list" => self.list(params).await,
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

fn stmt(sql: &str, params: Vec<SqlValue>) -> SqlStatement {
    SqlStatement::new(sql, params)
}

fn text(value: &str) -> SqlValue {
    SqlValue::Text(value.to_owned())
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
) -> StorageResult<Registry> {
    let rows = reader
        .query_all(stmt(
            include_str!("../sql/engine/vocabularies-list.sql"),
            vec![text(domain)],
        ))
        .await?;
    let mut registry = Registry::core();
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

/// A caller-scoped request key (ADR-193 D8).
struct Command {
    scope: Scope,
    request_id: String,
    verb: String,
    request_digest: String,
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
                    vec![text(&command.scope.actor), text(&command.request_id)],
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
            let result = out.downcast_ref::<Value>().ok_or_else(|| {
                refuse(&command.verb, Refusal::new("invalid_outcome", "not JSON"))
            })?;
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
                        SqlValue::Blob(canonical_json(result)),
                        SqlValue::from_opt_text(run_id),
                        SqlValue::Integer(command.scope.now_us),
                    ],
                ))
                .await?;
            Ok(out)
        })
    })
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
    async fn registry(&self, domain: &str) -> Result<Registry, RuntimeError> {
        let mut reader = self.runtime.sql().reader().await?;
        Ok(registry_from(reader.as_mut(), domain).await?)
    }

    async fn compile(&self, params: Params) -> Result<Value, RuntimeError> {
        let source = params.str("source")?;
        let registry = self.registry(&params.domain()?).await?;
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
        let registry = self.registry(&scope.domain).await?;
        let compiled = compile(&source, &registry)
            .map_err(|d| params.invalid(format!("charter source refused: {}", refusal_text(&d))))?;
        let verb = params.verb.clone();
        let result = self
            .runtime
            .sql()
            .atomic_unit(with_receipt(command, Box::new(move |writer| {
                Box::pin(async move {
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
                    let held = engine
                        .writer()
                        .query_row(stmt(
                            include_str!("../sql/engine/definition-get.sql"),
                            vec![
                                text(&scope.domain),
                                text(&definition.id),
                                SqlValue::Integer(definition.version),
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
                                SqlValue::Blob(source.into_bytes()),
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
        let parsed = parse_vocabulary_source(&source).map_err(|d| {
            params.invalid(format!("vocabulary source refused: {}", refusal_text(&d)))
        })?;
        let source_digest = digest(source.as_bytes());
        let verb = params.verb.clone();
        let result = self
            .runtime
            .sql()
            .atomic_unit(with_receipt(command, Box::new(move |writer| {
                Box::pin(async move {
                    let rows = writer
                        .query_all(stmt(
                            include_str!("../sql/engine/vocabularies-list.sql"),
                            vec![text(&scope.domain)],
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
                    let mut registry = registry_from(&mut *writer, &scope.domain).await?;
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
                            .subject(&subject_kind, &subject_ref, generation.as_deref(), true)
                            .await?;
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
            out["run"] = self.run_view(&params.domain()?, &run_id, 0).await?;
        }
        Ok(out)
    }

    async fn observe(&self, params: Params, token: &NamespaceToken) -> Result<Value, RuntimeError> {
        let subject_kind = params.str("subject_kind")?;
        let subject_ref = params.str("subject_ref")?;
        let kind = params.str("kind")?;
        let args = params.opt_object("args")?;
        let detail = params.opt_object("detail")?;
        let generation = params.opt_str("generation")?;
        let only_charter = params.opt_str("charter")?;
        let scope = params.scope(token)?;
        let command = params.command(&scope)?;
        let producer = params
            .opt_str("producer")?
            .unwrap_or_else(|| scope.actor.clone());
        if producer == crate::engine::ENGINE_PRODUCER {
            return Err(params.invalid(format!(
                "producer {producer:?} is reserved for the engine's own observations"
            )));
        }
        let event_id = params
            .opt_str("event_id")?
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let registry = self.registry(&scope.domain).await?;
        let entry = registry.get(&kind).ok_or_else(|| {
            params.invalid(format!(
                "unknown_kind: {kind} is not a registered trigger, phrase or action"
            ))
        })?;
        if entry.kind == EntryKind::Subject {
            return Err(params.invalid(format!(
                "wrong_entry_kind: {kind} is a subject, not an observation"
            )));
        }
        let establishes = registry
            .get(&subject_kind)
            .is_some_and(|s| s.new_generation.contains(&kind));
        let outcome = match params.opt_str("outcome")? {
            Some(o) if entry.outcomes.contains(&o) => o,
            Some(o) => {
                return Err(params.invalid(format!(
                    "unknown_outcome: {kind} has outcomes {}, not {o}",
                    entry.outcomes.join(" ")
                )))
            }
            None if entry.outcomes.len() == 1 => entry.outcomes[0].clone(),
            None => {
                return Err(params.invalid(format!(
                    "missing_outcome: {kind} needs one of {}",
                    entry.outcomes.join(" ")
                )))
            }
        };
        if let Some(name) = args
            .keys()
            .find(|name| !entry.params.iter().any(|p| &p.name == *name))
        {
            return Err(params.invalid(format!(
                "unknown_argument: {kind} declares no argument {name}"
            )));
        }

        let result = self
            .runtime
            .sql()
            .atomic_unit(with_receipt(
                command,
                Box::new(move |writer| {
                    Box::pin(async move {
                        let mut engine = Engine::new(writer, scope.clone());
                        let exists = engine
                            .writer()
                            .query_row(stmt(
                                include_str!("../sql/engine/subject-get.sql"),
                                vec![text(&subject_kind), text(&subject_ref)],
                            ))
                            .await?
                            .is_some();
                        if !exists {
                            return Ok(boxed(json!({"recorded": [], "events": []})));
                        }
                        let (subject_id, epoch, current) = engine
                            .subject(
                                &subject_kind,
                                &subject_ref,
                                generation.as_deref(),
                                establishes,
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
                        for (run_id, charter_id) in
                            engine.open_runs_for_subject(&subject_id).await?
                        {
                            if only_charter.as_ref().is_some_and(|c| c != &charter_id) {
                                continue;
                            }
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
                        Ok(boxed(json!({
                            "recorded": recorded,
                            "events": engine.journal.events,
                        })))
                    })
                }),
            ))
            .await?;
        unit_value(result, &params.verb)
    }

    async fn cancel(&self, params: Params, token: &NamespaceToken) -> Result<Value, RuntimeError> {
        let run_id = params.str("run_id")?;
        let reason = params.opt_str("reason")?;
        let scope = params.scope(token)?;
        let command = params.command(&scope)?;
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
                        if state != "open" || col_text(&row, "policy_domain")? != scope.domain {
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

    async fn waiting(&self, params: Params) -> Result<Value, RuntimeError> {
        let role = params.opt_str("role")?;
        let actor = params.opt_str("actor")?;
        let rows = self
            .runtime
            .sql()
            .reader()
            .await?
            .query_all(stmt(
                include_str!("../sql/engine/waiting.sql"),
                vec![
                    text(&params.domain()?),
                    opt_text(&params.opt_str("charter")?),
                    opt_text(&params.opt_str("subject_kind")?),
                ],
            ))
            .await?;
        let mut items = Vec::new();
        for row in rows {
            let candidate = col_json(&row, "candidate_bytes")?;
            let bindings = candidate["bindings"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            let waiting_on: Value = match col_opt_text(&row, "waiting_on")? {
                Some(raw) => serde_json::from_str(&raw).map_err(|e| {
                    RuntimeError::Internal(format!("charter.waiting: waiting_on: {e}"))
                })?,
                None => Value::Null,
            };
            let assignee_kind = col_text(&row, "assignee_kind")?;
            let assignee = col_text(&row, "assignee")?;
            let owner = (assignee_kind == "role").then_some(assignee.as_str());
            let mut owed_by = waiting_actors(&waiting_on, owner, &bindings);
            if assignee_kind == "actor" {
                owed_by.push(format!("actor:{assignee}"));
            }
            if role
                .as_ref()
                .is_some_and(|r| !owed_by.contains(&format!("role:{r}")))
                || actor
                    .as_ref()
                    .is_some_and(|a| !owed_by.contains(&format!("actor:{a}")))
            {
                continue;
            }
            items.push(json!({
                "run_id": col_text(&row, "run_id")?,
                "charter": col_text(&row, "charter_id")?,
                "version": col_i64(&row, "definition_version")?,
                "parent_run_id": col_opt_text(&row, "parent_run_id")?,
                "subject": {"kind": col_text(&row, "subject_kind")?, "ref": col_text(&row, "subject_ref")?},
                "phase": col_text(&row, "phase_id")?,
                "state": col_text(&row, "state")?,
                "round": col_i64(&row, "round")?,
                "assignee": {"kind": assignee_kind, "id": assignee},
                "owed_by": owed_by,
                "waiting_on": waiting_on,
                "since_us": col_i64(&row, "updated_at_us")?,
            }));
        }
        Ok(json!({"count": items.len(), "waiting": items}))
    }

    async fn run_view(
        &self,
        domain: &str,
        run_id: &str,
        evidence_limit: i64,
    ) -> Result<Value, RuntimeError> {
        let mut reader = self.runtime.sql().reader().await?;
        let Some(run) = reader
            .query_row(stmt(
                include_str!("../sql/engine/run-get.sql"),
                vec![text(run_id)],
            ))
            .await?
        else {
            return Err(RuntimeError::NotFound(format!("charter run {run_id}")));
        };
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

    async fn get(&self, params: Params) -> Result<Value, RuntimeError> {
        let run_id = params.str("run_id")?;
        let limit = params.limit("evidence_limit")?;
        self.run_view(&params.domain()?, &run_id, limit).await
    }

    async fn runs(&self, params: Params) -> Result<Value, RuntimeError> {
        let rows = self
            .runtime
            .sql()
            .reader()
            .await?
            .query_all(stmt(
                include_str!("../sql/engine/runs-list.sql"),
                vec![
                    text(&params.domain()?),
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
                "state": col_text(r, "state")?,
                "current_phase": col_opt_text(r, "current_phase_id")?,
                "parent_run_id": col_opt_text(r, "parent_run_id")?,
                "subject": {"kind": col_text(r, "subject_kind")?, "ref": col_text(r, "subject_ref")?},
                "updated_at_us": col_i64(r, "updated_at_us")?,
            }));
        }
        Ok(json!({"count": runs.len(), "runs": runs}))
    }

    async fn list(&self, params: Params) -> Result<Value, RuntimeError> {
        let domain = params.domain()?;
        let mut reader = self.runtime.sql().reader().await?;
        let charters = reader
            .query_all(stmt(
                include_str!("../sql/engine/definitions-list.sql"),
                vec![text(&domain)],
            ))
            .await?;
        let vocabularies = reader
            .query_all(stmt(
                include_str!("../sql/engine/vocabularies-list.sql"),
                vec![text(&domain)],
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
