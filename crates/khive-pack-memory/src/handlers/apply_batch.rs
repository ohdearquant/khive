//! `memory.apply_batch`: apply a chunk of memory rewrites in one call.
//!
//! A call carries a batch name and 1 to 200 rows. A row supersedes an existing
//! memory, creates a new one, skips a row, or lists a row as a deletion
//! candidate. Every row is checked before anything is written, the checks are the
//! same in a dry run and in a real run, and a failing row is refused without
//! stopping the others. Rows that write go through the `memory.remember` path
//! under an idempotency key derived from the batch name, so re-running a chunk
//! writes only what is missing. See the batch-apply amendment in
//! `docs/adr/ADR-021-memory-pack.md`.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use khive_runtime::keyed_memory::validate_memory_key;
use khive_runtime::secret_gate;
use khive_runtime::{Namespace, NamespaceToken, Resolved, RuntimeError};
use khive_storage::EdgeRelation;

use crate::MemoryPack;

use super::common::{decode_params, deser, DEFAULT_SALIENCE_EPISODIC, DEFAULT_SALIENCE_SEMANTIC};
use super::remember::{actor_namespace, check_remember_fields};

/// Longest accepted batch name, in characters.
const MAX_BATCH_NAME_CHARS: usize = 128;
/// Most rows one call accepts.
const MAX_ROWS: usize = 200;
/// Salience above this value needs `properties.verified`.
const UNVERIFIED_SALIENCE_CEILING: f64 = 0.4;
/// Properties the verb writes itself; a row may not supply them.
const ADDED_PROPERTY_KEYS: [&str; 3] = ["batch", "supersedes", "batch_row"];
/// Every outcome a result entry can carry, in the order the counts list them.
const OUTCOMES: [&str; 7] = [
    "planned", "written", "deduped", "skipped", "listed", "refused", "failed",
];

/// Parameters of `memory.apply_batch`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyBatchParams {
    batch: String,
    rows: Value,
    dry_run: Option<bool>,
}

/// What a row asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Action {
    Supersede,
    Create,
    Skip,
    DeleteCandidate,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Supersede => "supersede",
            Action::Create => "create",
            Action::Skip => "skip",
            Action::DeleteCandidate => "delete-candidate",
        }
    }
}

fn default_part() -> u32 {
    1
}

/// One row as supplied. Unknown fields are refused.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RowInput {
    id: Option<String>,
    #[serde(default = "default_part")]
    part: u32,
    action: Action,
    content: Option<String>,
    memory_type: Option<String>,
    salience: Option<f64>,
    properties: Option<Value>,
    tags: Option<Vec<String>>,
}

/// A stable refusal code and a human-readable message.
#[derive(Debug, Clone)]
struct Refusal {
    code: &'static str,
    message: String,
}

fn refusal(code: &'static str, message: impl Into<String>) -> Refusal {
    Refusal {
        code,
        message: message.into(),
    }
}

/// The row fields echoed in its result entry, as the caller supplied them.
struct Echo {
    id: Value,
    part: Value,
    action: Value,
}

fn echo_of(raw: &Value) -> Echo {
    Echo {
        id: raw.get("id").cloned().unwrap_or(Value::Null),
        part: raw.get("part").cloned().unwrap_or_else(|| json!(1)),
        action: raw.get("action").cloned().unwrap_or(Value::Null),
    }
}

/// A row that passed its checks and will be written.
struct PlannedWrite {
    /// Parameters for `memory.remember`: the content, the idempotency key and
    /// the properties with the three the verb adds.
    params: Value,
    /// The old row a `supersede` row replaces.
    supersedes: Option<Uuid>,
}

/// What the call will do for one row.
enum Plan {
    Refused(Refusal),
    Skipped,
    Listed,
    New(PlannedWrite),
    Deduped {
        existing: Uuid,
        supersedes: Option<Uuid>,
    },
}

/// One row after its checks.
struct Checked {
    echo: Echo,
    old_namespace: Option<String>,
    plan: Plan,
}

/// One entry of the result.
struct Entry {
    echo: Echo,
    outcome: &'static str,
    old_namespace: Option<String>,
    new_id: Option<Uuid>,
    reason: Option<Refusal>,
}

impl Entry {
    fn to_value(&self) -> Value {
        let reason = self
            .reason
            .as_ref()
            .map(|r| json!({ "code": r.code, "message": r.message }));
        json!({
            "id": self.echo.id,
            "part": self.echo.part,
            "action": self.echo.action,
            "outcome": self.outcome,
            "old_namespace": self.old_namespace,
            "new_id": self.new_id.map(|id| id.to_string()),
            "reason": reason,
        })
    }
}

fn error_message(error: &RuntimeError) -> String {
    match error {
        RuntimeError::InvalidInput(message) => message.clone(),
        other => other.to_string(),
    }
}

fn prop<'a>(extra: &'a Option<Map<String, Value>>, key: &str) -> Option<&'a Value> {
    extra.as_ref().and_then(|map| map.get(key))
}

/// A property names something when it is a non-blank string or an array that
/// holds at least one non-blank string.
fn names_something(value: Option<&Value>) -> bool {
    match value {
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Array(items)) => items
            .iter()
            .any(|item| item.as_str().is_some_and(|text| !text.trim().is_empty())),
        _ => false,
    }
}

/// `verified` is `true` or a non-empty string naming what was checked.
fn is_verified(extra: &Option<Map<String, Value>>) -> bool {
    match prop(extra, "verified") {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => !text.trim().is_empty(),
        _ => false,
    }
}

/// Compact JSON with object keys in sorted order, so the same row always
/// serializes to the same text whatever order its keys arrived in.
fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(item, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// Lowercase hex SHA-256 of `text`.
fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Lowercase hex SHA-256 of a row's canonical JSON. The eight fields are always
/// present; a field the row left out is null. `id` is the old row's id in its
/// canonical hyphenated form.
fn batch_row_hash(row: &RowInput, old_id: Option<Uuid>) -> String {
    let canonical = json!({
        "action": row.action.as_str(),
        "id": old_id.map(|id| id.to_string()),
        "part": row.part,
        "content": row.content,
        "memory_type": row.memory_type,
        "salience": row.salience,
        "properties": row.properties,
        "tags": row.tags,
    });
    let mut text = String::new();
    write_canonical(&canonical, &mut text);
    sha256_hex(&text)
}

/// A row after parsing: the fields its entry echoes, and the row itself or the
/// reason it did not parse.
struct Parsed {
    echo: Echo,
    row: Result<RowInput, Refusal>,
}

/// The idempotency key a writing row is stored under, when the row names one:
/// `<batch>:<old id>:<part>` for `supersede` and `<batch>:create:<part>` for
/// `create`. The old id is written in its canonical hyphenated form, so two
/// spellings of one id give one key.
fn row_key(batch: &str, row: &RowInput) -> Option<String> {
    match row.action {
        Action::Supersede => {
            let old_id = row.id.as_deref()?.parse::<Uuid>().ok()?;
            Some(format!("{batch}:{old_id}:{}", row.part))
        }
        Action::Create if row.id.is_none() => Some(format!("{batch}:create:{}", row.part)),
        _ => None,
    }
}

fn refused(
    echo: Echo,
    old_namespace: Option<String>,
    why: Refusal,
) -> Result<Checked, RuntimeError> {
    Ok(Checked {
        echo,
        old_namespace,
        plan: Plan::Refused(why),
    })
}

impl MemoryPack {
    /// Check one row and decide what the call will do for it, writing nothing.
    ///
    /// A dry run and a real run both go through this function, so they refuse
    /// the same rows for the same reasons. A row that fails a check becomes a
    /// refusal entry; only a store read that fails stops the call.
    async fn check_row(
        &self,
        token: &NamespaceToken,
        write_token: &NamespaceToken,
        batch: &str,
        parsed: Parsed,
        key_counts: &HashMap<String, usize>,
    ) -> Result<Checked, RuntimeError> {
        let Parsed { echo, row } = parsed;
        let row = match row {
            Ok(row) => row,
            Err(why) => return refused(echo, None, why),
        };
        let action = row.action;

        let old_id: Option<Uuid> = match (action != Action::Create, row.id.as_deref()) {
            (true, None) => {
                return refused(
                    echo,
                    None,
                    refusal(
                        "id-missing",
                        format!("{} needs the id of the old row", action.as_str()),
                    ),
                )
            }
            (false, Some(_)) => {
                return refused(echo, None, refusal("id-not-allowed", "create takes no id"))
            }
            (true, Some(text)) => match text.parse::<Uuid>() {
                Ok(id) => Some(id),
                Err(_) => {
                    return refused(
                        echo,
                        None,
                        refusal(
                            "old-unresolved",
                            "id is not a full UUID, so no row resolves from it",
                        ),
                    )
                }
            },
            (false, None) => None,
        };

        let mut old_namespace: Option<String> = None;
        let mut old_in_shared = false;
        if let Some(id) = old_id {
            match self.runtime.resolve_by_id(token, id).await? {
                None => {
                    return refused(
                        echo,
                        None,
                        refusal("old-unresolved", format!("no row resolves from id {id}")),
                    )
                }
                Some(Resolved::Note(note)) => {
                    if note.kind != "memory" {
                        return refused(
                            echo,
                            Some(note.namespace.clone()),
                            refusal(
                                "old-not-memory",
                                format!("old row is a {} note, not a memory", note.kind),
                            ),
                        );
                    }
                    // A namespace the caller declared it owns is checked like the
                    // caller's own: it is not shared, so the shared-row rule below
                    // does not apply to it (ADR-021 Delta 1).
                    if note.namespace == Namespace::LOCAL {
                        old_in_shared = true;
                    } else if note.namespace != write_token.namespace().as_str()
                        && !write_token.owns(&note.namespace)
                    {
                        return refused(
                            echo,
                            Some(note.namespace.clone()),
                            refusal(
                                "old-namespace",
                                format!("old row is in namespace {}", note.namespace),
                            ),
                        );
                    }
                    old_namespace = Some(note.namespace);
                }
                Some(_) => {
                    return refused(
                        echo,
                        None,
                        refusal("old-not-memory", "old row is not a memory note"),
                    )
                }
            }
        }

        match action {
            Action::Skip => {
                return Ok(Checked {
                    echo,
                    old_namespace,
                    plan: Plan::Skipped,
                })
            }
            Action::DeleteCandidate => {
                return Ok(Checked {
                    echo,
                    old_namespace,
                    plan: Plan::Listed,
                })
            }
            Action::Supersede | Action::Create => {}
        }

        let content = row.content.as_deref().unwrap_or("");
        let (memory_type, extra) = match check_remember_fields(
            content,
            row.memory_type.as_deref(),
            row.properties.as_ref(),
        ) {
            Ok(checked) => checked,
            Err(error) => {
                return refused(
                    echo,
                    old_namespace,
                    refusal("content-invalid", error_message(&error)),
                )
            }
        };

        // The salience the row would be stored with: the supplied value, or the
        // memory type's default when the row gives none, as `memory.remember`
        // resolves it. A semantic row without a salience is stored at 0.5, so the
        // ceiling applies to it as well.
        let stored_salience = row.salience.unwrap_or(match memory_type {
            "semantic" => DEFAULT_SALIENCE_SEMANTIC,
            _ => DEFAULT_SALIENCE_EPISODIC,
        });
        if !(0.0..=1.0).contains(&stored_salience) {
            return refused(
                echo,
                old_namespace,
                refusal(
                    "salience-range",
                    format!("salience must be in [0, 1], got {stored_salience}"),
                ),
            );
        }
        if stored_salience > UNVERIFIED_SALIENCE_CEILING && !is_verified(&extra) {
            let origin = if row.salience.is_some() {
                String::new()
            } else {
                format!(" (the {memory_type} default; the row gives none)")
            };
            return refused(
                echo,
                old_namespace,
                refusal(
                    "salience-unverified",
                    format!(
                        "salience {stored_salience}{origin} is above {UNVERIFIED_SALIENCE_CEILING} and properties.verified is absent, false or empty"
                    ),
                ),
            );
        }

        if !names_something(prop(&extra, "who")) {
            return refused(
                echo,
                old_namespace,
                refusal("who-missing", "properties.who must name who wrote the row"),
            );
        }

        if action == Action::Supersede
            && old_in_shared
            && !names_something(prop(&extra, "source_ids"))
        {
            return refused(
                echo,
                old_namespace,
                refusal(
                    "source-ids-missing",
                    "properties.source_ids must list the sources when the old row is in the shared namespace",
                ),
            );
        }

        for added in ADDED_PROPERTY_KEYS {
            if prop(&extra, added).is_some() {
                return refused(
                    echo,
                    old_namespace,
                    refusal(
                        "reserved-property",
                        format!(
                            "properties.{added} is written by this verb and may not be supplied"
                        ),
                    ),
                );
            }
        }

        let batch_row = batch_row_hash(&row, old_id);
        let mut properties = extra.unwrap_or_default();
        properties.insert("batch".to_owned(), json!(batch));
        if let Some(id) = old_id {
            properties.insert("supersedes".to_owned(), json!(id.to_string()));
        }
        properties.insert("batch_row".to_owned(), json!(batch_row));
        let properties = Value::Object(properties);
        // The stored properties carry the three added keys, so the size and key
        // limits are checked on the merged object.
        if let Err(error) =
            check_remember_fields(content, row.memory_type.as_deref(), Some(&properties))
        {
            return refused(
                echo,
                old_namespace,
                refusal("content-invalid", error_message(&error)),
            );
        }

        let key = match row_key(batch, &row) {
            Some(key) => key,
            None => {
                return refused(
                    echo,
                    old_namespace,
                    refusal("invalid-row", "the row names no idempotency key"),
                )
            }
        };
        if key_counts.get(&key).copied().unwrap_or(0) > 1 {
            return refused(
                echo,
                old_namespace,
                refusal(
                    "duplicate-key",
                    format!("another row of this call has the key {key}"),
                ),
            );
        }

        // The write path scans the key, the content, the properties and the tags
        // for credential-shaped text. The same scans run here, so a dry run refuses
        // what the write would refuse. The refusal names the detector and the
        // field, never the matched text.
        let scanned = secret_gate::check_at(&key, "note", "key")
            .and_then(|()| {
                secret_gate::check_note_fields_at(content, None, Some(&properties), "note")
            })
            .and_then(|()| {
                secret_gate::check_tags_at(row.tags.as_deref().unwrap_or(&[]), "note", "tags")
            });
        if let Err(error) = scanned {
            return refused(
                echo,
                old_namespace,
                refusal("secret-detected", error_message(&error)),
            );
        }

        let caller_namespace = write_token.namespace().as_str();
        let held = self
            .runtime
            .notes(write_token)?
            .get_live_notes_by_key(caller_namespace, &key, Some("memory"))
            .await?;
        if let Some(existing) = held.first() {
            let stored = existing
                .properties
                .as_ref()
                .and_then(|stored| stored.get("batch_row"))
                .and_then(Value::as_str);
            if stored == Some(batch_row.as_str()) {
                return Ok(Checked {
                    echo,
                    old_namespace,
                    plan: Plan::Deduped {
                        existing: existing.id,
                        supersedes: old_id,
                    },
                });
            }
            return refused(
                echo,
                old_namespace,
                refusal(
                    "dedupe-name-taken",
                    format!(
                        "key {key} is already taken in namespace {caller_namespace} by a row with different content; use a new part or a new batch name"
                    ),
                ),
            );
        }

        let mut params = json!({
            "content": content,
            "key": key,
            "properties": properties,
        });
        if let Some(memory_type) = &row.memory_type {
            params["memory_type"] = json!(memory_type);
        }
        if let Some(value) = row.salience {
            params["salience"] = json!(value);
        }
        if let Some(tags) = &row.tags {
            params["tags"] = json!(tags);
        }
        Ok(Checked {
            echo,
            old_namespace,
            plan: Plan::New(PlannedWrite {
                params,
                supersedes: old_id,
            }),
        })
    }
}

/// Why a row's write did not complete.
struct WriteFailure {
    code: &'static str,
    error: RuntimeError,
    /// The new row's id, when the row was stored before the failure.
    new_id: Option<Uuid>,
}

impl MemoryPack {
    /// Assert the `supersedes` edge from a new row to the row it replaces, in the
    /// caller's namespace. A row that replaces nothing has no edge. Linking is
    /// idempotent, so asserting an edge that already exists changes nothing.
    async fn assert_supersedes(
        &self,
        write_token: &NamespaceToken,
        new_id: Uuid,
        old_id: Option<Uuid>,
    ) -> Result<(), RuntimeError> {
        if let Some(old_id) = old_id {
            self.runtime
                .link(
                    write_token,
                    new_id,
                    old_id,
                    EdgeRelation::Supersedes,
                    1.0,
                    None,
                )
                .await?;
        }
        Ok(())
    }

    /// Write one planned row through the `memory.remember` path, then assert its
    /// `supersedes` edge. Returns the outcome (`written`, or `deduped` when the
    /// write found its key already taken) and the id of the stored row.
    async fn write_row(
        &self,
        token: &NamespaceToken,
        write_token: &NamespaceToken,
        planned: PlannedWrite,
    ) -> Result<(&'static str, Uuid), WriteFailure> {
        let stored = self
            .handle_remember(token, planned.params)
            .await
            .map_err(|error| WriteFailure {
                code: "write-failed",
                error,
                new_id: None,
            })?;
        let new_id = stored
            .get("id")
            .and_then(Value::as_str)
            .and_then(|text| text.parse::<Uuid>().ok())
            .ok_or_else(|| WriteFailure {
                code: "write-failed",
                error: RuntimeError::Internal("memory.remember returned no row id".into()),
                new_id: None,
            })?;
        let replayed = stored
            .get("replayed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.assert_supersedes(write_token, new_id, planned.supersedes)
            .await
            .map_err(|error| WriteFailure {
                code: "link-failed",
                error,
                new_id: Some(new_id),
            })?;
        Ok((if replayed { "deduped" } else { "written" }, new_id))
    }

    /// `memory.apply_batch`: check every row, then (unless `dry_run`) write the
    /// rows that passed, one result entry per row in input order.
    pub(crate) async fn handle_apply_batch(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: ApplyBatchParams = deser(params)?;
        if token.actor().is_anonymous() {
            return Err(RuntimeError::InvalidInput(
                "memory.apply_batch needs a named actor: an anonymous caller's writes would land in the shared namespace with no writer".into(),
            ));
        }

        let batch = p.batch.as_str();
        if batch.trim().is_empty() || batch.chars().count() > MAX_BATCH_NAME_CHARS {
            return Err(RuntimeError::InvalidInput(format!(
                "batch must be a name of 1 to {MAX_BATCH_NAME_CHARS} characters"
            )));
        }
        // The longest key a row of this call can need: a `supersede` row with a
        // hyphenated UUID and the largest part number. A batch name that cannot
        // form it cannot form any row's key.
        validate_memory_key(&format!("{batch}:{}:{}", Uuid::nil(), u32::MAX)).map_err(|error| {
            RuntimeError::InvalidInput(format!(
                "batch name cannot form a valid idempotency key: {}",
                error_message(&error)
            ))
        })?;

        let rows = p
            .rows
            .as_array()
            .ok_or_else(|| RuntimeError::InvalidInput("rows must be an array".into()))?;
        if rows.is_empty() || rows.len() > MAX_ROWS {
            return Err(RuntimeError::InvalidInput(format!(
                "rows must hold 1 to {MAX_ROWS} rows, got {}",
                rows.len()
            )));
        }
        let dry_run = p.dry_run.unwrap_or(true);
        let write_token = token.with_namespace(actor_namespace(token)?);

        let parsed: Vec<Parsed> = rows
            .iter()
            .map(|raw| Parsed {
                echo: echo_of(raw),
                row: decode_params::<RowInput>(raw.clone())
                    .map_err(|message| refusal("invalid-row", message)),
            })
            .collect();

        let mut key_counts: HashMap<String, usize> = HashMap::new();
        for parsed_row in &parsed {
            if let Ok(row) = &parsed_row.row {
                if let Some(key) = row_key(batch, row) {
                    *key_counts.entry(key).or_insert(0) += 1;
                }
            }
        }

        // Every row is checked before the first write.
        let mut checked: Vec<Checked> = Vec::with_capacity(parsed.len());
        for parsed_row in parsed {
            checked.push(
                self.check_row(token, &write_token, batch, parsed_row, &key_counts)
                    .await?,
            );
        }

        let mut entries: Vec<Entry> = Vec::with_capacity(checked.len());
        for item in checked {
            let Checked {
                echo,
                old_namespace,
                plan,
            } = item;
            let (outcome, new_id, reason): (&'static str, Option<Uuid>, Option<Refusal>) =
                match plan {
                    Plan::Refused(why) => ("refused", None, Some(why)),
                    Plan::Skipped => ("skipped", None, None),
                    Plan::Listed => ("listed", None, None),
                    Plan::Deduped {
                        existing,
                        supersedes,
                    } => {
                        if dry_run {
                            ("deduped", Some(existing), None)
                        } else {
                            match self
                                .assert_supersedes(&write_token, existing, supersedes)
                                .await
                            {
                                Ok(()) => ("deduped", Some(existing), None),
                                Err(error) => (
                                    "failed",
                                    Some(existing),
                                    Some(refusal("link-failed", error_message(&error))),
                                ),
                            }
                        }
                    }
                    Plan::New(planned) => {
                        if dry_run {
                            ("planned", None, None)
                        } else {
                            match self.write_row(token, &write_token, planned).await {
                                Ok((outcome, id)) => (outcome, Some(id), None),
                                Err(failure) => (
                                    "failed",
                                    failure.new_id,
                                    Some(refusal(failure.code, error_message(&failure.error))),
                                ),
                            }
                        }
                    }
                };
            entries.push(Entry {
                echo,
                outcome,
                old_namespace,
                new_id,
                reason,
            });
        }

        let mut counts = Map::new();
        for outcome in OUTCOMES {
            let total = entries
                .iter()
                .filter(|entry| entry.outcome == outcome)
                .count();
            counts.insert(outcome.to_owned(), json!(total));
        }
        Ok(json!({
            "batch": batch,
            "dry_run": dry_run,
            "entries": entries.iter().map(Entry::to_value).collect::<Vec<Value>>(),
            "counts": counts,
        }))
    }
}
