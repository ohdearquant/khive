//! Recording-milestone run engine (ADR-193 M1, ADR-208).
//!
//! Every write happens inside one `atomic_unit` whose op reads its own state through the
//! writer, so a command and the transitions it causes commit together or not at all. The
//! engine records what happened and what each open phase waits on; it performs no external
//! effect and admits no action.
//!
//! Phase semantics, evaluated over the observations recorded since the phase was entered:
//! a phase is settled once every `await` line, action, perform, holding `require` and `when`
//! name has an observation. A hard or soft `require`, an action or a perform without a passing
//! outcome then holds the phase, unless the first `when` whose label matches the newest
//! observation routes on that same name; a `when` on another name releases nothing. With
//! nothing held, the matched `when` decides the route, or the phase takes its `next` route.
//! Advisory requirements are recorded and never hold. Entering a phase past its `max_rounds`
//! bound enters the bound's `then` phase instead.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde_json::{json, Map, Value};

use khive_storage::{SqlRow, SqlStatement, SqlValue, SqlWriter, StorageError, StorageResult};

use crate::lang::definition::{Definition, PhaseDef, Route};
use crate::lang::{canonical_json, digest};

/// The producer recorded on observations the engine writes itself.
pub const ENGINE_PRODUCER: &str = "charter";
const STEP_LIMIT: usize = 4_096;

#[derive(Clone, Debug)]
pub struct Scope {
    pub domain: String,
    pub namespace: String,
    pub actor: String,
    pub now_us: i64,
}

/// A command refused before it wrote anything.
#[derive(Clone, Debug)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

pub type Outcome = Result<Value, Refusal>;

fn corrupt(message: impl Into<String>) -> StorageError {
    StorageError::Internal(format!("charter: {}", message.into()))
}

fn stmt(sql: &str, params: Vec<SqlValue>) -> SqlStatement {
    SqlStatement::new(sql, params)
}

fn text(value: &str) -> SqlValue {
    SqlValue::Text(value.to_owned())
}

pub fn col_text(row: &SqlRow, name: &str) -> StorageResult<String> {
    row.text(name)
        .map(str::to_owned)
        .map_err(|e| corrupt(format!("column {name}: {e:?}")))
}

pub fn col_opt_text(row: &SqlRow, name: &str) -> StorageResult<Option<String>> {
    row.opt_text(name)
        .map(|v| v.map(str::to_owned))
        .map_err(|e| corrupt(format!("column {name}: {e:?}")))
}

pub fn col_i64(row: &SqlRow, name: &str) -> StorageResult<i64> {
    row.i64(name)
        .map_err(|e| corrupt(format!("column {name}: {e:?}")))
}

pub fn col_opt_i64(row: &SqlRow, name: &str) -> StorageResult<Option<i64>> {
    row.opt_i64(name)
        .map_err(|e| corrupt(format!("column {name}: {e:?}")))
}

pub fn col_json(row: &SqlRow, name: &str) -> StorageResult<Value> {
    let bytes: &[u8] = match row.get(name) {
        Some(SqlValue::Blob(b)) => b,
        Some(SqlValue::Text(t)) => t.as_bytes(),
        Some(SqlValue::Null) | None => return Ok(Value::Null),
        Some(_) => return Err(corrupt(format!("column {name} is not JSON bytes"))),
    };
    serde_json::from_slice(bytes).map_err(|e| corrupt(format!("column {name}: {e}")))
}

pub fn decode_definition(row: &SqlRow) -> StorageResult<Definition> {
    serde_json::from_value(col_json(row, "definition_bytes")?)
        .map_err(|e| corrupt(format!("definition: {e}")))
}

/// One recorded observation, decoded from its payload.
#[derive(Clone, Debug)]
struct Obs {
    kind: String,
    outcome: Option<String>,
    args: Map<String, Value>,
    generation: Option<String>,
    epoch: Option<i64>,
}

#[derive(Clone, Debug)]
struct RunRow {
    charter_id: String,
    definition_version: i64,
    subject_id: String,
    subject_kind: String,
    subject_ref: String,
    candidate: Value,
    state: String,
    current_phase_id: Option<String>,
    parent_run_id: Option<String>,
}

#[derive(Clone, Debug)]
struct PhaseRow {
    phase_id: String,
    round: i64,
}

async fn load_run(w: &mut dyn SqlWriter, run_id: &str) -> StorageResult<Option<RunRow>> {
    let Some(row) = w
        .query_row(stmt(
            include_str!("../sql/engine/run-get.sql"),
            vec![text(run_id)],
        ))
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(RunRow {
        charter_id: col_text(&row, "charter_id")?,
        definition_version: col_i64(&row, "definition_version")?,
        subject_id: col_text(&row, "subject_id")?,
        subject_kind: col_text(&row, "subject_kind")?,
        subject_ref: col_text(&row, "subject_ref")?,
        candidate: col_json(&row, "candidate_bytes")?,
        state: col_text(&row, "state")?,
        current_phase_id: col_opt_text(&row, "current_phase_id")?,
        parent_run_id: col_opt_text(&row, "parent_run_id")?,
    }))
}

async fn load_definition(
    w: &mut dyn SqlWriter,
    domain: &str,
    charter_id: &str,
    version: Option<i64>,
) -> StorageResult<Option<(i64, String, Definition)>> {
    let row = match version {
        Some(v) => {
            w.query_row(stmt(
                include_str!("../sql/engine/definition-get.sql"),
                vec![text(domain), text(charter_id), SqlValue::Integer(v)],
            ))
            .await?
        }
        None => {
            w.query_row(stmt(
                include_str!("../sql/engine/definition-latest.sql"),
                vec![text(domain), text(charter_id)],
            ))
            .await?
        }
    };
    let Some(row) = row else { return Ok(None) };
    Ok(Some((
        col_i64(&row, "version")?,
        col_text(&row, "definition_digest")?,
        decode_definition(&row)?,
    )))
}

async fn load_phases(
    w: &mut dyn SqlWriter,
    domain: &str,
    run_id: &str,
) -> StorageResult<BTreeMap<String, PhaseRow>> {
    let rows = w
        .query_all(stmt(
            include_str!("../sql/engine/phases-for-run.sql"),
            vec![text(domain), text(run_id)],
        ))
        .await?;
    let mut out = BTreeMap::new();
    for row in rows {
        let phase = PhaseRow {
            phase_id: col_text(&row, "phase_id")?,
            round: col_i64(&row, "round")?,
        };
        out.insert(phase.phase_id.clone(), phase);
    }
    Ok(out)
}

/// The sequence that ended the previous visit of a phase: the first phase entry recorded after
/// that visit's own entry.
async fn previous_visit_end(
    w: &mut dyn SqlWriter,
    domain: &str,
    run_id: &str,
    phase_id: &str,
    round: i64,
) -> StorageResult<i64> {
    let rows = w
        .query_all(stmt(
            include_str!("../sql/engine/entries-for-run.sql"),
            vec![text(domain), text(run_id)],
        ))
        .await?;
    let mut previous_found = false;
    for row in rows {
        if previous_found {
            return col_i64(&row, "sequence");
        }
        let payload = col_json(&row, "payload_bytes")?;
        previous_found = payload["phase"] == phase_id && payload["round"] == round - 1;
    }
    Err(corrupt(format!(
        "phase {phase_id} round {round} has no ended previous visit"
    )))
}

async fn observations_since(
    w: &mut dyn SqlWriter,
    domain: &str,
    run_id: &str,
    after: i64,
) -> StorageResult<Vec<Obs>> {
    let rows = w
        .query_all(stmt(
            include_str!("../sql/engine/evidence-since.sql"),
            vec![text(domain), text(run_id), SqlValue::Integer(after)],
        ))
        .await?;
    let mut out = Vec::new();
    for row in rows {
        let payload = col_json(&row, "payload_bytes")?;
        out.push(Obs {
            kind: col_text(&row, "kind")?,
            outcome: payload["outcome"].as_str().map(str::to_owned),
            args: payload["args"].as_object().cloned().unwrap_or_default(),
            generation: payload["generation"].as_str().map(str::to_owned),
            epoch: payload["epoch"].as_i64(),
        });
    }
    Ok(out)
}

/// Append one observation to a run and return its sequence. An observation already recorded
/// under the same producer, kind, source and event id returns the held sequence.
#[allow(clippy::too_many_arguments)]
async fn append(
    w: &mut dyn SqlWriter,
    scope: &Scope,
    run_id: &str,
    kind: &str,
    producer: &str,
    source_identity: &str,
    event_id: &str,
    payload: &Value,
) -> StorageResult<(i64, bool)> {
    if let Some(row) = w
        .query_row(stmt(
            include_str!("../sql/engine/evidence-find.sql"),
            vec![
                text(&scope.domain),
                text(run_id),
                text(producer),
                text(kind),
                text(source_identity),
                text(event_id),
            ],
        ))
        .await?
    {
        return Ok((col_i64(&row, "sequence")?, false));
    }
    let next = w
        .query_row(stmt(
            include_str!("../sql/engine/evidence-next.sql"),
            vec![text(&scope.domain), text(run_id)],
        ))
        .await?
        .ok_or_else(|| corrupt("evidence sequence"))?;
    let sequence = col_i64(&next, "next")?;
    let bytes = canonical_json(payload);
    w.execute(stmt(
        include_str!("../sql/engine/evidence-insert.sql"),
        vec![
            text(&scope.domain),
            text(run_id),
            SqlValue::Integer(sequence),
            text(&scope.namespace),
            text(kind),
            text(producer),
            text(source_identity),
            text(event_id),
            text(&digest(&bytes)),
            SqlValue::Blob(bytes),
            SqlValue::Integer(scope.now_us),
        ],
    ))
    .await?;
    Ok((sequence, true))
}

fn args_match(wanted: &BTreeMap<String, Value>, got: &Map<String, Value>) -> bool {
    wanted.iter().all(|(k, v)| got.get(k) == Some(v))
}

fn newest<'a>(obs: &'a [Obs], kind: &str, args: &BTreeMap<String, Value>) -> Option<&'a Obs> {
    obs.iter()
        .rev()
        .find(|o| o.kind == kind && args_match(args, &o.args))
}

#[derive(Debug)]
enum Eval {
    Wait {
        state: &'static str,
        waiting_on: Value,
    },
    Route(Route),
}

fn evaluate(phase: &PhaseDef, obs: &[Obs]) -> Eval {
    let mut pending = Vec::new();
    let mut waiting_trigger = false;
    let mut waiting_actor = false;
    for line in &phase.awaits {
        if !obs.iter().any(|o| line.contains(&o.kind)) {
            waiting_trigger = true;
            pending.push(json!({"await": line}));
        }
    }
    for action in &phase.actions {
        if newest(obs, &action.name, &action.args).is_none() {
            waiting_actor = true;
            pending.push(json!({"action": action.name, "args": action.args, "by": action.by}));
        }
    }
    let none = BTreeMap::new();
    for child in &phase.performs {
        if newest(obs, child, &none).is_none() {
            waiting_actor = true;
            pending.push(json!({"perform": child}));
        }
    }
    for req in &phase.requires {
        if req.level.holds_phase() && newest(obs, &req.phrase, &req.args).is_none() {
            pending.push(json!({"require": req.phrase, "args": req.args, "level": req.level}));
        }
    }
    let mut seen_when = BTreeSet::new();
    for when in &phase.whens {
        if seen_when.insert((&when.name, canonical_json(&json!(when.args))))
            && newest(obs, &when.name, &when.args).is_none()
            && !pending.iter().any(|p| {
                p["require"] == json!(when.name)
                    || p["action"] == json!(when.name)
                    || p["perform"] == json!(when.name)
            })
        {
            pending.push(json!({"when": when.name, "args": when.args}));
        }
    }
    if !pending.is_empty() {
        let state = if waiting_trigger {
            "waiting_trigger"
        } else if waiting_actor {
            "waiting_actor"
        } else {
            "waiting_gate"
        };
        return Eval::Wait {
            state,
            waiting_on: json!({"pending": pending}),
        };
    }

    // A failing requirement, action or perform holds the phase unless the `when` that matched
    // routes on that same name; a `when` on another name does not release it.
    let matched = phase.whens.iter().find(|w| {
        newest(obs, &w.name, &w.args).and_then(|o| o.outcome.as_deref()) == Some(w.label.as_str())
    });
    let routes_on = |name: &str, args: &BTreeMap<String, Value>| {
        matched.is_some_and(|w| w.name == name && &w.args == args)
    };
    let mut blocked = Vec::new();
    for req in &phase.requires {
        if !req.level.holds_phase() || routes_on(&req.phrase, &req.args) {
            continue;
        }
        let outcome = newest(obs, &req.phrase, &req.args).and_then(|o| o.outcome.clone());
        if !outcome.as_ref().is_some_and(|o| req.passes.contains(o)) {
            blocked.push(json!({"require": req.phrase, "args": req.args, "level": req.level, "outcome": outcome}));
        }
    }
    if !blocked.is_empty() {
        return Eval::Wait {
            state: "waiting_gate",
            waiting_on: json!({"blocked": blocked}),
        };
    }
    for action in &phase.actions {
        if routes_on(&action.name, &action.args) {
            continue;
        }
        let outcome = newest(obs, &action.name, &action.args).and_then(|o| o.outcome.clone());
        if !outcome.as_ref().is_some_and(|o| action.passes.contains(o)) {
            blocked.push(json!({"action": action.name, "by": action.by, "outcome": outcome}));
        }
    }
    for child in &phase.performs {
        if routes_on(child, &none) {
            continue;
        }
        let outcome = newest(obs, child, &none).and_then(|o| o.outcome.clone());
        if outcome.as_deref() != Some("completed") {
            blocked.push(json!({"perform": child, "outcome": outcome}));
        }
    }
    if !blocked.is_empty() {
        return Eval::Wait {
            state: "waiting_actor",
            waiting_on: json!({"blocked": blocked}),
        };
    }
    if let Some(when) = matched {
        return Eval::Route(when.route.clone());
    }
    Eval::Route(phase.next.clone())
}

enum Work {
    Advance(String),
    End {
        run_id: String,
        state: &'static str,
        reason: Option<String>,
    },
    StartChild {
        parent_run_id: String,
        charter_id: String,
    },
}

/// Collects the transitions one command caused, for the command's reply.
#[derive(Default)]
pub struct Journal {
    pub events: Vec<Value>,
    pub started: Vec<String>,
}

pub struct Engine<'w> {
    w: &'w mut dyn SqlWriter,
    scope: Scope,
    queue: VecDeque<Work>,
    pub journal: Journal,
    definitions: BTreeMap<(String, i64), Definition>,
}

impl<'w> Engine<'w> {
    pub fn new(w: &'w mut dyn SqlWriter, scope: Scope) -> Self {
        Self {
            w,
            scope,
            queue: VecDeque::new(),
            journal: Journal::default(),
            definitions: BTreeMap::new(),
        }
    }

    pub fn writer(&mut self) -> &mut dyn SqlWriter {
        &mut *self.w
    }

    async fn definition(&mut self, charter_id: &str, version: i64) -> StorageResult<Definition> {
        let key = (charter_id.to_owned(), version);
        if let Some(d) = self.definitions.get(&key) {
            return Ok(d.clone());
        }
        let (_, _, d) =
            load_definition(&mut *self.w, &self.scope.domain, charter_id, Some(version))
                .await?
                .ok_or_else(|| {
                    corrupt(format!(
                        "run pins missing definition {charter_id} v{version}"
                    ))
                })?;
        self.definitions.insert(key, d.clone());
        Ok(d)
    }

    pub async fn latest_definition(
        &mut self,
        charter_id: &str,
    ) -> StorageResult<Option<(i64, String, Definition)>> {
        load_definition(&mut *self.w, &self.scope.domain, charter_id, None).await
    }

    /// Find or create the subject row. A subject without a generation takes the first one it is
    /// given; after that a different `generation` starts a new subject epoch only when
    /// `establishes` is set, so a late observation for an older candidate never moves it back.
    pub async fn subject(
        &mut self,
        kind: &str,
        reference: &str,
        generation: Option<&str>,
        establishes: bool,
    ) -> StorageResult<(String, i64, Option<String>)> {
        let found = self
            .w
            .query_row(stmt(
                include_str!("../sql/engine/subject-get.sql"),
                vec![text(kind), text(reference)],
            ))
            .await?;
        let candidate = json!({"generation": generation});
        let bytes = canonical_json(&candidate);
        let Some(row) = found else {
            let subject_id = format!("{kind}:{reference}");
            self.w
                .execute(stmt(
                    include_str!("../sql/engine/subject-insert.sql"),
                    vec![
                        text(&self.scope.domain),
                        text(&subject_id),
                        text(&self.scope.namespace),
                        text(kind),
                        text(reference),
                        text(&digest(&bytes)),
                        SqlValue::Blob(bytes),
                        SqlValue::Integer(self.scope.now_us),
                    ],
                ))
                .await?;
            return Ok((subject_id, 0, generation.map(str::to_owned)));
        };
        let subject_id = col_text(&row, "subject_id")?;
        let epoch = col_i64(&row, "subject_epoch")?;
        let held = col_json(&row, "candidate_bytes")?["generation"]
            .as_str()
            .map(str::to_owned);
        match generation {
            Some(g) if held.as_deref() != Some(g) && (establishes || held.is_none()) => {
                let affected = self
                    .w
                    .execute(stmt(
                        include_str!("../sql/engine/subject-generation.sql"),
                        vec![
                            text(&subject_id),
                            text(&digest(&bytes)),
                            SqlValue::Blob(bytes),
                            SqlValue::Integer(self.scope.now_us),
                            SqlValue::Integer(col_i64(&row, "revision")?),
                        ],
                    ))
                    .await?;
                if affected != 1 {
                    return Err(corrupt("subject generation update lost its revision"));
                }
                Ok((subject_id, epoch + 1, Some(g.to_owned())))
            }
            _ => Ok((subject_id, epoch, held)),
        }
    }

    pub async fn open_runs_for_subject(
        &mut self,
        subject_id: &str,
    ) -> StorageResult<Vec<(String, String)>> {
        let rows = self
            .w
            .query_all(stmt(
                include_str!("../sql/engine/runs-open-for-subject.sql"),
                vec![text(&self.scope.domain), text(subject_id)],
            ))
            .await?;
        rows.iter()
            .map(|r| Ok((col_text(r, "run_id")?, col_text(r, "charter_id")?)))
            .collect()
    }

    pub async fn open_run_for_charter(
        &mut self,
        charter_id: &str,
        subject_id: &str,
    ) -> StorageResult<Option<String>> {
        let row = self
            .w
            .query_row(stmt(
                include_str!("../sql/engine/run-open-for-charter.sql"),
                vec![text(&self.scope.domain), text(charter_id), text(subject_id)],
            ))
            .await?;
        row.map(|r| col_text(&r, "run_id")).transpose()
    }

    /// Insert a run and its dormant phases, then enter the first phase.
    pub async fn start_run(
        &mut self,
        version: i64,
        definition_digest: &str,
        definition: &Definition,
        subject: (&str, i64, Option<&str>),
        bindings: &Map<String, Value>,
        parent_run_id: Option<&str>,
    ) -> StorageResult<String> {
        let (subject_id, epoch, generation) = subject;
        let run_id = uuid::Uuid::new_v4().to_string();
        let candidate = json!({
            "bindings": bindings,
            "generation": generation,
            "started_by": self.scope.actor,
        });
        let bytes = canonical_json(&candidate);
        self.w
            .execute(stmt(
                include_str!("../sql/engine/run-insert.sql"),
                vec![
                    text(&self.scope.domain),
                    text(&run_id),
                    text(&self.scope.namespace),
                    text(&format!(
                        "{}:{}:{}:{}",
                        self.scope.domain, definition.id, subject_id, run_id
                    )),
                    text(subject_id),
                    SqlValue::Integer(epoch),
                    text(&definition.id),
                    SqlValue::Integer(version),
                    text(definition_digest),
                    text(&digest(&bytes)),
                    SqlValue::Blob(bytes),
                    SqlValue::from_opt_text(parent_run_id),
                    SqlValue::Integer(self.scope.now_us),
                ],
            ))
            .await?;
        for phase in &definition.phases {
            let (kind, assignee) = match &phase.owner {
                Some(role) => ("role", role.clone()),
                None => ("actor", self.scope.actor.clone()),
            };
            self.w
                .execute(stmt(
                    include_str!("../sql/engine/phase-insert.sql"),
                    vec![
                        text(&self.scope.domain),
                        text(&run_id),
                        text(&phase.id),
                        text(&self.scope.namespace),
                        SqlValue::Integer(phase.ordinal),
                        text(kind),
                        text(&assignee),
                        SqlValue::Integer(self.scope.now_us),
                    ],
                ))
                .await?;
        }
        self.definitions
            .insert((definition.id.clone(), version), definition.clone());
        self.journal.started.push(run_id.clone());
        self.journal.events.push(json!({
            "run_id": run_id, "charter": definition.id, "event": "started",
            "parent_run_id": parent_run_id,
        }));
        let first = definition
            .phases
            .first()
            .ok_or_else(|| corrupt("definition has no phases"))?
            .id
            .clone();
        self.enter(&run_id, definition, &first).await?;
        self.queue.push_back(Work::Advance(run_id.clone()));
        Ok(run_id)
    }

    /// Record an external observation on one open run and queue its advance. The same event id
    /// is a new observation at each subject epoch, so a candidate that returns re-reads its gates.
    #[allow(clippy::too_many_arguments)]
    pub async fn observe(
        &mut self,
        run_id: &str,
        subject_id: &str,
        epoch: i64,
        kind: &str,
        producer: &str,
        event_id: &str,
        payload: &Value,
    ) -> StorageResult<(i64, bool)> {
        let (sequence, fresh) = append(
            &mut *self.w,
            &self.scope,
            run_id,
            kind,
            producer,
            &format!("{subject_id}@{epoch}"),
            event_id,
            payload,
        )
        .await?;
        if fresh {
            self.queue.push_back(Work::Advance(run_id.to_owned()));
        }
        Ok((sequence, fresh))
    }

    pub fn cancel(&mut self, run_id: &str, reason: Option<String>) {
        self.queue.push_back(Work::End {
            run_id: run_id.to_owned(),
            state: "cancelled",
            reason,
        });
    }

    pub fn advance_later(&mut self, run_id: &str) {
        self.queue.push_back(Work::Advance(run_id.to_owned()));
    }

    /// Drain queued work: advance runs, start children, end runs and report endings upward.
    pub async fn settle(&mut self) -> StorageResult<()> {
        let mut steps = 0usize;
        while let Some(work) = self.queue.pop_front() {
            steps += 1;
            if steps > STEP_LIMIT {
                return Err(corrupt("run settlement exceeded its step limit"));
            }
            match work {
                Work::Advance(run_id) => self.advance(&run_id).await?,
                Work::End {
                    run_id,
                    state,
                    reason,
                } => self.end(&run_id, state, reason).await?,
                Work::StartChild {
                    parent_run_id,
                    charter_id,
                } => self.start_child(&parent_run_id, &charter_id).await?,
            }
        }
        Ok(())
    }

    async fn enter(
        &mut self,
        run_id: &str,
        definition: &Definition,
        requested: &str,
    ) -> StorageResult<()> {
        let phases = load_phases(&mut *self.w, &self.scope.domain, run_id).await?;
        let mut target = requested.to_owned();
        let mut hops = 0;
        loop {
            let def = definition
                .phase(&target)
                .ok_or_else(|| corrupt(format!("unknown phase {target}")))?;
            let row = phases
                .get(&target)
                .ok_or_else(|| corrupt(format!("run has no phase row {target}")))?;
            match &def.max_rounds {
                Some(bound) if row.round >= i64::from(bound.rounds) => {
                    hops += 1;
                    if hops > definition.phases.len() {
                        return Err(corrupt("max_rounds bounds route in a circle"));
                    }
                    self.journal.events.push(json!({
                        "run_id": run_id, "event": "bounded", "phase": target,
                        "rounds": bound.rounds, "then": bound.then,
                    }));
                    target = bound.then.clone();
                }
                _ => break,
            }
        }
        let round = phases[&target].round + 1;
        let (sequence, _) = append(
            &mut *self.w,
            &self.scope,
            run_id,
            "charter.entered",
            ENGINE_PRODUCER,
            run_id,
            &format!("entered:{target}:{round}"),
            &json!({"phase": target, "round": round, "requested": requested}),
        )
        .await?;
        self.w
            .execute(stmt(
                include_str!("../sql/engine/phase-enter.sql"),
                vec![
                    text(&self.scope.domain),
                    text(run_id),
                    text(&target),
                    SqlValue::Integer(sequence),
                    SqlValue::Integer(self.scope.now_us),
                ],
            ))
            .await?;
        let affected = self
            .w
            .execute(stmt(
                include_str!("../sql/engine/run-set-phase.sql"),
                vec![
                    text(&self.scope.domain),
                    text(run_id),
                    text(&target),
                    SqlValue::Integer(self.scope.now_us),
                ],
            ))
            .await?;
        if affected != 1 {
            return Err(corrupt("entering a phase of a run that is not open"));
        }
        self.journal.events.push(json!({
            "run_id": run_id, "event": "entered", "phase": target, "round": round,
        }));
        let def = definition.phase(&target).expect("checked above");
        for child in &def.performs {
            self.queue.push_back(Work::StartChild {
                parent_run_id: run_id.to_owned(),
                charter_id: child.clone(),
            });
        }
        Ok(())
    }

    async fn advance(&mut self, run_id: &str) -> StorageResult<()> {
        let Some(run) = load_run(&mut *self.w, run_id).await? else {
            return Err(corrupt(format!("unknown run {run_id}")));
        };
        if run.state != "open" {
            return Ok(());
        }
        let definition = self
            .definition(&run.charter_id, run.definition_version)
            .await?;
        let phase_id = run
            .current_phase_id
            .clone()
            .ok_or_else(|| corrupt("open run has no current phase"))?;
        let phases = load_phases(&mut *self.w, &self.scope.domain, run_id).await?;
        let row = &phases[&phase_id];
        let def = definition
            .phase(&phase_id)
            .ok_or_else(|| corrupt(format!("unknown phase {phase_id}")))?
            .clone();
        let (subject_generation, subject_epoch) = self.subject_generation(&run).await?;
        // A first visit reads every observation at the current generation and subject epoch
        // (ADR-208 D3, ADR-193 D8); a revisit reads only what arrived after its previous visit
        // ended, so a loop never reuses the evidence that sent it round.
        let window = if row.round > 1 {
            previous_visit_end(
                &mut *self.w,
                &self.scope.domain,
                run_id,
                &phase_id,
                row.round,
            )
            .await?
        } else {
            0
        };
        let obs: Vec<Obs> = observations_since(&mut *self.w, &self.scope.domain, run_id, window)
            .await?
            .into_iter()
            .filter(|o| match (&o.generation, &subject_generation) {
                (Some(g), Some(current)) => g == current && o.epoch == Some(subject_epoch),
                _ => true,
            })
            .collect();
        match evaluate(&def, &obs) {
            Eval::Wait {
                state,
                mut waiting_on,
            } => {
                waiting_on["round"] = json!(row.round);
                waiting_on["owner"] = json!(def.owner);
                self.w
                    .execute(stmt(
                        include_str!("../sql/engine/phase-state.sql"),
                        vec![
                            text(&self.scope.domain),
                            text(run_id),
                            text(&phase_id),
                            text(state),
                            SqlValue::Text(
                                String::from_utf8(canonical_json(&waiting_on))
                                    .map_err(|e| corrupt(format!("waiting_on: {e}")))?,
                            ),
                            SqlValue::Integer(self.scope.now_us),
                        ],
                    ))
                    .await?;
            }
            Eval::Route(route) => {
                self.leave(run_id, &phase_id).await?;
                self.journal.events.push(json!({
                    "run_id": run_id, "event": "routed", "from": phase_id, "route": route,
                }));
                match route {
                    Route::Goto { phase } => {
                        self.enter(run_id, &definition, &phase).await?;
                        self.queue.push_back(Work::Advance(run_id.to_owned()));
                    }
                    Route::Complete => self.queue.push_back(Work::End {
                        run_id: run_id.to_owned(),
                        state: "completed",
                        reason: None,
                    }),
                    Route::Terminate { reason } => self.queue.push_back(Work::End {
                        run_id: run_id.to_owned(),
                        state: "terminated",
                        reason,
                    }),
                }
            }
        }
        Ok(())
    }

    async fn subject_generation(&mut self, run: &RunRow) -> StorageResult<(Option<String>, i64)> {
        let row = self
            .w
            .query_row(stmt(
                include_str!("../sql/engine/subject-get.sql"),
                vec![text(&run.subject_kind), text(&run.subject_ref)],
            ))
            .await?
            .ok_or_else(|| corrupt("run subject missing"))?;
        let generation = col_json(&row, "candidate_bytes")?["generation"]
            .as_str()
            .map(str::to_owned);
        Ok((generation, col_i64(&row, "subject_epoch")?))
    }

    async fn leave(&mut self, run_id: &str, phase_id: &str) -> StorageResult<()> {
        self.w
            .execute(stmt(
                include_str!("../sql/engine/phase-state.sql"),
                vec![
                    text(&self.scope.domain),
                    text(run_id),
                    text(phase_id),
                    text("completed"),
                    SqlValue::Null,
                    SqlValue::Integer(self.scope.now_us),
                ],
            ))
            .await?;
        Ok(())
    }

    async fn end(
        &mut self,
        run_id: &str,
        state: &'static str,
        reason: Option<String>,
    ) -> StorageResult<()> {
        let Some(run) = load_run(&mut *self.w, run_id).await? else {
            return Err(corrupt(format!("unknown run {run_id}")));
        };
        if run.state != "open" {
            return Ok(());
        }
        if let Some(phase) = &run.current_phase_id {
            self.leave(run_id, phase).await?;
        }
        append(
            &mut *self.w,
            &self.scope,
            run_id,
            "charter.ended",
            ENGINE_PRODUCER,
            run_id,
            "ended",
            &json!({"outcome": state, "reason": reason}),
        )
        .await?;
        let affected = self
            .w
            .execute(stmt(
                include_str!("../sql/engine/run-end.sql"),
                vec![
                    text(&self.scope.domain),
                    text(run_id),
                    text(state),
                    SqlValue::Integer(self.scope.now_us),
                ],
            ))
            .await?;
        if affected != 1 {
            return Err(corrupt("ending a run that is not open"));
        }
        self.journal.events.push(json!({
            "run_id": run_id, "charter": run.charter_id, "event": state, "reason": reason,
        }));
        let children = self
            .w
            .query_all(stmt(
                include_str!("../sql/engine/runs-open-children.sql"),
                vec![text(&self.scope.domain), text(run_id)],
            ))
            .await?;
        for child in children {
            self.queue.push_front(Work::End {
                run_id: col_text(&child, "run_id")?,
                state: "cancelled",
                reason: Some(format!("parent run {run_id} ended {state}")),
            });
        }
        if let Some(parent) = &run.parent_run_id {
            let parent_open = load_run(&mut *self.w, parent)
                .await?
                .is_some_and(|p| p.state == "open");
            if parent_open {
                append(
                    &mut *self.w,
                    &self.scope,
                    parent,
                    &run.charter_id,
                    ENGINE_PRODUCER,
                    &run.subject_id,
                    run_id,
                    &json!({"outcome": state, "child_run_id": run_id, "reason": reason}),
                )
                .await?;
                self.queue.push_back(Work::Advance(parent.clone()));
            }
        }
        Ok(())
    }

    async fn start_child(&mut self, parent_run_id: &str, charter_id: &str) -> StorageResult<()> {
        let Some(parent) = load_run(&mut *self.w, parent_run_id).await? else {
            return Err(corrupt(format!("unknown run {parent_run_id}")));
        };
        if parent.state != "open" {
            return Ok(());
        }
        let Some((version, definition_digest, definition)) =
            self.latest_definition(charter_id).await?
        else {
            // Publication refuses a perform of an unpublished charter, so this is an
            // inconsistent store, recorded on the parent as a cancelled child.
            append(
                &mut *self.w,
                &self.scope,
                parent_run_id,
                charter_id,
                ENGINE_PRODUCER,
                &parent.subject_id,
                &uuid::Uuid::new_v4().to_string(),
                &json!({"outcome": "cancelled", "reason": "the performed charter is not published"}),
            )
            .await?;
            self.queue
                .push_back(Work::Advance(parent_run_id.to_owned()));
            return Ok(());
        };
        let bindings = parent.candidate["bindings"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        let generation = parent.candidate["generation"].as_str().map(str::to_owned);
        let subject_epoch = self
            .w
            .query_row(stmt(
                include_str!("../sql/engine/subject-get.sql"),
                vec![text(&parent.subject_kind), text(&parent.subject_ref)],
            ))
            .await?
            .map(|r| col_i64(&r, "subject_epoch"))
            .transpose()?
            .unwrap_or(0);
        let child_bindings: Map<String, Value> = bindings
            .into_iter()
            .filter(|(role, _)| definition.roles.iter().any(|r| &r.name == role))
            .collect();
        self.start_run(
            version,
            &definition_digest,
            &definition,
            (&parent.subject_id, subject_epoch, generation.as_deref()),
            &child_bindings,
            Some(parent_run_id),
        )
        .await?;
        Ok(())
    }
}

/// The waiting view's per-phase reading: who the phase waits on, resolved through bindings.
pub fn waiting_actors(
    waiting_on: &Value,
    owner: Option<&str>,
    bindings: &Map<String, Value>,
) -> Vec<String> {
    let mut roles = BTreeSet::new();
    if let Some(owner) = owner {
        roles.insert(owner.to_owned());
    }
    for key in ["pending", "blocked"] {
        for item in waiting_on[key].as_array().into_iter().flatten() {
            if let Some(by) = item["by"].as_str() {
                roles.insert(by.to_owned());
            }
        }
    }
    roles
        .into_iter()
        .flat_map(|role| {
            let actor = bindings
                .get(&role)
                .and_then(Value::as_str)
                .map(str::to_owned);
            [
                Some(format!("role:{role}")),
                actor.map(|a| format!("actor:{a}")),
            ]
        })
        .flatten()
        .collect()
}
