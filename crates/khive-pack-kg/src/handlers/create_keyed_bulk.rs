//! Keyed items of a bulk `create` (ADR-221 section 1).
//!
//! Items are judged in order, against the store and against the earlier
//! items of the same request: a key an earlier item created or reloaded is
//! judged against that item's payload, exactly as if it had been stored.
//! The judged request then runs as one atomic unit.

use std::collections::HashMap;

use khive_runtime::atomic_prepare::apply_post_commit_effects_with_failures;
use khive_runtime::{
    entity_carries_create_payload, entity_key_conflict, keyed_replay_assertion_plan,
    run_atomic_unit, same_create_payload, AtomicOpPlan, AtomicRunOutcome, DomainDisposition,
    EntityCreateSpec, NamespaceToken, PendingEntityId, PostCommitEffect, RuntimeError,
    VerbRegistry,
};
use khive_storage::Entity;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{BulkWrite, PreparedBulkEntity, PreparedBulkItem, PreparedBulkNote};
use crate::KgPack;

/// Times a request is judged and run before it gives up on holders that
/// keep moving between its judgment and its commit, as a singleton keyed
/// create gives up after the same number of lost claims.
const KEYED_BULK_ATTEMPTS: usize = 3;

/// The id and version of each stored holder a judgment read, in reading
/// order; `None` where the key was free.
type HolderReads = Vec<Option<(Uuid, i64)>>;

// A scoped test-only rendezvous between a request's first judgment and its
// unit, so a test can move a holder the judgment read. Production builds have
// neither the hook nor a branch for it.
#[cfg(test)]
tokio::task_local! {
    pub(in crate::handlers) static KEYED_BULK_UNIT_TEST_BARRIER: std::cell::RefCell<Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>>;
}

#[cfg(test)]
async fn wait_for_keyed_bulk_unit_test_barrier() {
    let barrier = KEYED_BULK_UNIT_TEST_BARRIER
        .try_with(|slot| slot.borrow_mut().take())
        .ok()
        .flatten();
    if let Some((reached, proceed)) = barrier {
        reached.send(()).expect("keyed bulk test barrier observer");
        proceed.await.expect("keyed bulk test barrier release");
    }
}

/// Where an item's key points while the request is judged.
#[derive(Clone)]
enum KeyHolder {
    /// The live entity that held the key before the request, unchanged by it.
    Stored(Box<Entity>),
    /// The row item `slot` writes: a new row, or the stored holder it reloads.
    Slot(usize),
}

/// One judged item, planned with the payload it carries now. Each plan is
/// built when its item is judged, so the runtime admits every item's own
/// payload in item order, as the same items sent one by one would be
/// admitted, and a payload a later item replaces is admitted all the same.
enum Slot {
    /// A new entity row; its key, when it has one, is claimed for it.
    Create {
        prepared: PreparedBulkEntity,
        normalized: Option<Value>,
        planned: Box<(Entity, AtomicOpPlan)>,
    },
    Note {
        write: Box<BulkWrite>,
        plan: AtomicOpPlan,
    },
    /// The stored holder of the item's key, rewritten to the slot's payload.
    Reload {
        holder: Box<Entity>,
        prepared: PreparedBulkEntity,
        normalized: Option<Value>,
        plan: AtomicOpPlan,
    },
    /// Writes nothing and answers as the row it names.
    Same {
        target: KeyHolder,
        updated: bool,
        normalized: Option<Value>,
    },
}

/// One judged item once its plan is built.
enum Row {
    Created(Box<BulkWrite>),
    Updated {
        id: Uuid,
        kind: String,
        normalized: Option<Value>,
    },
    Same {
        target: KeyHolder,
        updated: bool,
        normalized: Option<Value>,
    },
}

/// One item's answer in a committed bulk create.
pub(super) struct ItemAnswer {
    /// The item's `results[i].result` value.
    pub(super) result: Value,
    /// The item's `entity_type` alias substitution, when one applied.
    pub(super) normalized: Option<Value>,
    /// The committed record for the `entities` list under `verbose`: the
    /// rows the item created or updated, never a row it only named.
    pub(super) record: Option<Value>,
    pub(super) created: bool,
    pub(super) updated: bool,
}

impl ItemAnswer {
    /// The answer of an item that wrote a new entity or note row.
    pub(super) fn of_write(write: &BulkWrite, verbose: bool) -> Result<Self, RuntimeError> {
        let result = write.ok_result(verbose)?;
        let (normalized, record) = match write {
            BulkWrite::Entity {
                entity, normalized, ..
            } => (
                normalized.clone(),
                verbose
                    .then(|| serde_json::to_value(entity))
                    .transpose()
                    .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?,
            ),
            BulkWrite::Note { .. } => (None, None),
        };
        Ok(Self {
            result,
            normalized,
            record,
            created: true,
            updated: false,
        })
    }
}

/// Why a judged request did not commit.
pub(super) enum KeyedBulkFailure {
    /// Refused while the request was judged: a key the secret gate or the
    /// length rule refuses, `key_conflict`, or a holder the caller may not
    /// update. Nothing was written.
    Judged { index: usize, error: RuntimeError },
    /// Refused while item `index`'s plan was built. Nothing was written.
    Plan { index: usize, error: RuntimeError },
    /// The unit rolled back at item `index`. Nothing was written.
    RolledBack { index: usize, failure: String },
    /// The transaction seam failed; whether the unit landed is unknown.
    Seam(String),
    /// The unit committed, and an item's answer could not be built.
    Answer(RuntimeError),
}

impl From<RuntimeError> for KeyedBulkFailure {
    fn from(error: RuntimeError) -> Self {
        Self::Answer(error)
    }
}

impl KeyedBulkFailure {
    /// The whole request's error under `atomic: true`.
    pub(super) fn into_atomic_error(self) -> RuntimeError {
        match self {
            // A structured refusal (`key_conflict`) names its key; a plain
            // invalid input names its item.
            Self::Judged {
                index,
                error: RuntimeError::InvalidInput(message),
            } => RuntimeError::InvalidInput(format!("items[{index}]: {message}")),
            Self::Judged { error, .. } => error,
            Self::Plan { index, error } => {
                RuntimeError::InvalidInput(format!("items[{index}]: {error}"))
            }
            Self::RolledBack { index, failure } => RuntimeError::Internal(format!(
                "create: atomic bulk batch rolled back at item index {index}: {failure}"
            )),
            Self::Seam(message) => {
                RuntimeError::Internal(format!("create: atomic bulk batch failed: {message}"))
            }
            Self::Answer(error) => error,
        }
    }

    /// One item's error under `atomic: false`.
    pub(super) fn into_item_error(self) -> (RuntimeError, DomainDisposition) {
        match self {
            Self::Judged { error, .. } | Self::Plan { error, .. } => {
                (error, DomainDisposition::NotCommitted)
            }
            Self::RolledBack { failure, .. } => (
                RuntimeError::Internal(format!("create: item write rolled back: {failure}")),
                DomainDisposition::NotCommitted,
            ),
            Self::Seam(message) => (
                RuntimeError::Internal(format!("create: item write seam failure: {message}")),
                DomainDisposition::Unknown,
            ),
            Self::Answer(error) => (error, DomainDisposition::Committed),
        }
    }
}

/// The payload and row id item `slot` gives its key, and whether that row is
/// stored already. A row the request would create has no id a refusal may
/// name: the request that refuses never writes it.
fn slot_payload(slots: &[Slot], slot: usize) -> Option<(&EntityCreateSpec, Uuid, bool)> {
    match slots.get(slot)? {
        Slot::Create {
            prepared, planned, ..
        } if prepared.key.is_some() => Some((&prepared.spec, planned.0.id, false)),
        Slot::Reload {
            holder, prepared, ..
        } => Some((&prepared.spec, holder.id, true)),
        _ => None,
    }
}

fn unjudged_slot(index: usize) -> KeyedBulkFailure {
    KeyedBulkFailure::Judged {
        index,
        error: RuntimeError::Internal(format!(
            "keyed bulk create: items[{index}] names a key no earlier item holds"
        )),
    }
}

/// The update gate for a key's holder, asked only once the payloads differ.
/// A caller who may not update the holder gets `key_conflict`, naming the
/// holder only where it is `stored` and the caller may list the kind.
pub(super) async fn authorize_key_reload(
    registry: &VerbRegistry,
    token: &NamespaceToken,
    kind: &str,
    key: &str,
    (holder_id, stored): (Uuid, bool),
    update_args: Value,
) -> Result<(), RuntimeError> {
    match registry
        .authorize_effective_kg_read(token, "update", update_args, holder_id)
        .await
    {
        Ok(()) => Ok(()),
        Err(RuntimeError::PermissionDenied { .. }) => {
            let disclosed =
                (stored && registry.allows_entity_key_disclosure(token, kind)).then_some(holder_id);
            Err(entity_key_conflict(key, disclosed))
        }
        Err(error) => Err(error),
    }
}

impl KgPack {
    /// Judge `items` in order. Also returns, in reading order, the id and
    /// version of each stored holder the judgment read (`None` for a free
    /// key), so a rolled-back unit can tell whether a holder moved.
    async fn judge_keyed_bulk(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        items: &[PreparedBulkItem],
    ) -> Result<(Vec<Slot>, HolderReads), KeyedBulkFailure> {
        let mut slots: Vec<Slot> = Vec::with_capacity(items.len());
        let mut holders: HashMap<(String, String), KeyHolder> = HashMap::new();
        let mut read: HolderReads = Vec::new();
        for (index, item) in items.iter().enumerate() {
            let judged = |error| KeyedBulkFailure::Judged { index, error };
            let planned = |error| KeyedBulkFailure::Plan { index, error };
            let (prepared, normalized) = match item {
                PreparedBulkItem::Note(note) => {
                    let PreparedBulkNote { spec, args, hook } = note.clone();
                    let (note, plan) = self
                        .runtime
                        .prepare_bulk_note_plan(token, spec)
                        .await
                        .map_err(planned)?;
                    slots.push(Slot::Note {
                        write: Box::new(BulkWrite::Note { hook, args, note }),
                        plan,
                    });
                    continue;
                }
                PreparedBulkItem::Entity {
                    prepared,
                    normalized,
                } => (prepared.clone(), normalized.clone()),
            };
            let Some(key) = prepared.key.clone() else {
                let entity_plan = self
                    .runtime
                    .prepare_bulk_entity_plan(token, prepared.spec.clone())
                    .await
                    .map_err(planned)?;
                slots.push(Slot::Create {
                    prepared,
                    normalized,
                    planned: Box::new(entity_plan),
                });
                continue;
            };
            let kind = prepared.spec.kind.clone();
            let group = (kind.clone(), key.clone());
            let holder = match holders.get(&group) {
                Some(holder) => Some(holder.clone()),
                None => {
                    let stored = self
                        .runtime
                        .get_entity_by_key(token, &kind, &key)
                        .await
                        .map_err(judged)?;
                    read.push(stored.as_ref().map(|stored| (stored.id, stored.version)));
                    stored.map(|stored| KeyHolder::Stored(Box::new(stored)))
                }
            };
            let Some(holder) = holder else {
                let entity_plan = self
                    .runtime
                    .prepare_keyed_bulk_entity_plan(
                        token,
                        prepared.spec.clone(),
                        &key,
                        PendingEntityId::mint(),
                    )
                    .await
                    .map_err(planned)?;
                holders.insert(group, KeyHolder::Slot(index));
                slots.push(Slot::Create {
                    prepared,
                    normalized,
                    planned: Box::new(entity_plan),
                });
                continue;
            };
            let disclose = registry.allows_entity_key_disclosure(token, &kind);
            let (same, holder_id, stored) = match &holder {
                KeyHolder::Stored(stored) => (
                    entity_carries_create_payload(stored, &prepared.spec),
                    stored.id,
                    true,
                ),
                KeyHolder::Slot(slot) => {
                    let (spec, id, stored) =
                        slot_payload(&slots, *slot).ok_or_else(|| unjudged_slot(index))?;
                    (same_create_payload(spec, &prepared.spec), id, stored)
                }
            };
            if same {
                if !disclose {
                    return Err(judged(entity_key_conflict(&key, None)));
                }
                slots.push(Slot::Same {
                    target: holder,
                    updated: false,
                    normalized,
                });
                continue;
            }
            if !prepared.on_key {
                return Err(judged(entity_key_conflict(
                    &key,
                    (disclose && stored).then_some(holder_id),
                )));
            }
            if let KeyHolder::Stored(stored) = &holder {
                super::super::update::refuse_pack_registry_row(stored, "create").map_err(judged)?;
            }
            let spec = &prepared.spec;
            let update_args = json!({
                "id": holder_id.to_string(),
                "kind": kind,
                "name": spec.name,
                "description": spec.description,
                "entity_type": spec.entity_type,
                "properties": spec.properties,
                "tags": spec.tags,
            });
            authorize_key_reload(
                registry,
                token,
                &kind,
                &key,
                (holder_id, stored),
                update_args,
            )
            .await
            .map_err(judged)?;
            match holder {
                KeyHolder::Stored(stored) => {
                    let (_, plan) = self
                        .runtime
                        .prepare_keyed_entity_reload_plan(token, &stored, &prepared.spec)
                        .await
                        .map_err(planned)?;
                    holders.insert(group, KeyHolder::Slot(index));
                    slots.push(Slot::Reload {
                        holder: stored,
                        prepared,
                        normalized,
                        plan,
                    });
                }
                KeyHolder::Slot(slot) => {
                    // ADR-221 section 1: the earlier item's row takes this
                    // payload, and both items answer for that one row. The
                    // earlier payload was admitted when its item was
                    // judged; the row is planned again with this one.
                    match slots.get_mut(slot) {
                        Some(Slot::Create {
                            prepared: earlier,
                            planned: entity_plan,
                            ..
                        }) => {
                            self.runtime
                                .replan_keyed_bulk_entity(
                                    token,
                                    prepared.spec.clone(),
                                    &key,
                                    entity_plan,
                                )
                                .await
                                .map_err(planned)?;
                            earlier.spec = prepared.spec;
                            earlier.args = prepared.args;
                        }
                        Some(Slot::Reload {
                            holder: reloaded,
                            prepared: earlier,
                            plan,
                            ..
                        }) => {
                            let (_, replanned) = self
                                .runtime
                                .prepare_keyed_entity_reload_plan(token, reloaded, &prepared.spec)
                                .await
                                .map_err(planned)?;
                            *plan = replanned;
                            earlier.spec = prepared.spec;
                            earlier.args = prepared.args;
                        }
                        _ => return Err(unjudged_slot(index)),
                    }
                    slots.push(Slot::Same {
                        target: KeyHolder::Slot(slot),
                        updated: true,
                        normalized,
                    });
                }
            }
        }
        Ok((slots, read))
    }

    /// Run every judged item's plan as one unit and answer each item.
    /// `Ok(Err(..))` names the item at which the unit rolled back.
    async fn run_keyed_bulk_unit(
        &self,
        token: &NamespaceToken,
        verbose: bool,
        slots: Vec<Slot>,
    ) -> Result<Result<Vec<ItemAnswer>, (usize, String)>, KeyedBulkFailure> {
        let mut plans: Vec<AtomicOpPlan> = Vec::new();
        let mut plan_items: Vec<usize> = Vec::new();
        let mut rows: Vec<Row> = Vec::with_capacity(slots.len());
        for (index, slot) in slots.into_iter().enumerate() {
            match slot {
                Slot::Create {
                    prepared,
                    normalized,
                    planned,
                } => {
                    let PreparedBulkEntity { args, hook, .. } = prepared;
                    let (entity, plan) = *planned;
                    plans.push(plan);
                    plan_items.push(index);
                    rows.push(Row::Created(Box::new(BulkWrite::Entity {
                        hook,
                        args,
                        entity,
                        normalized,
                    })));
                }
                Slot::Note { write, plan } => {
                    plans.push(plan);
                    plan_items.push(index);
                    rows.push(Row::Created(write));
                }
                Slot::Reload {
                    holder,
                    normalized,
                    plan,
                    ..
                } => {
                    plans.push(plan);
                    plan_items.push(index);
                    rows.push(Row::Updated {
                        id: holder.id,
                        kind: holder.kind,
                        normalized,
                    });
                }
                Slot::Same {
                    target,
                    updated,
                    normalized,
                } => {
                    // An identical replay of a stored holder writes
                    // nothing; the unit asserts the holder is still the
                    // row the item was judged against.
                    if let KeyHolder::Stored(stored) = &target {
                        plans.push(keyed_replay_assertion_plan(stored));
                        plan_items.push(index);
                    }
                    rows.push(Row::Same {
                        target,
                        updated,
                        normalized,
                    });
                }
            }
        }

        // A reloaded row's post-commit reindex error, by row. A failed effect
        // that names no entity belongs to every reloaded row.
        let mut reindex_errors: HashMap<Uuid, String> = HashMap::new();
        let mut unattributed_error: Option<String> = None;
        if !plans.is_empty() {
            match run_atomic_unit(self.runtime.sql().as_ref(), plans).await {
                Ok(AtomicRunOutcome::Committed { post_commit }) => {
                    // A reload's reindex runs after the commit, as the
                    // ordinary update's does; a new row has none.
                    let report =
                        apply_post_commit_effects_with_failures(&self.runtime, token, post_commit)
                            .await;
                    for (effect, error) in report.failed_effects {
                        let error = format!("post-commit effects failed after commit: {error}");
                        match effect {
                            PostCommitEffect::ReindexEntity { entity_id } => {
                                reindex_errors.insert(entity_id, error);
                            }
                            _ => unattributed_error = Some(error),
                        }
                    }
                }
                Ok(AtomicRunOutcome::RolledBack {
                    failed_op_index,
                    failure,
                }) => {
                    let index = plan_items
                        .get(failed_op_index)
                        .copied()
                        .unwrap_or(failed_op_index);
                    return Ok(Err((index, format!("{failure:?}"))));
                }
                Err(error) => return Err(KeyedBulkFailure::Seam(error.0.to_string())),
            }
        }

        for row in &rows {
            if let Row::Created(write) = row {
                write.after_create(&self.runtime).await;
            }
        }
        // The row a target names, and whether the unit reloaded it: a
        // reloaded row's post-commit reindex error belongs to every answer
        // that names it.
        let committed_row = |target: &KeyHolder| -> Option<(Uuid, String, bool)> {
            match target {
                KeyHolder::Stored(stored) => Some((stored.id, stored.kind.clone(), false)),
                KeyHolder::Slot(slot) => match rows.get(*slot)? {
                    Row::Created(write) => match write.as_ref() {
                        BulkWrite::Entity { entity, .. } => {
                            Some((entity.id, entity.kind.clone(), false))
                        }
                        BulkWrite::Note { .. } => None,
                    },
                    Row::Updated { id, kind, .. } => Some((*id, kind.clone(), true)),
                    _ => None,
                },
            }
        };
        let mut answers = Vec::with_capacity(rows.len());
        for (index, row) in rows.iter().enumerate() {
            let answer = match row {
                Row::Created(write) => ItemAnswer::of_write(write, verbose)?,
                Row::Updated {
                    id,
                    kind,
                    normalized,
                } => {
                    let (mut result, record) = self
                        .keyed_row_answer(token, *id, kind, true, verbose)
                        .await?;
                    if let Some(error) = reindex_errors.get(id).or(unattributed_error.as_ref()) {
                        result["reindex_error"] = json!(error);
                    }
                    ItemAnswer {
                        result,
                        normalized: normalized.clone(),
                        record,
                        created: false,
                        updated: true,
                    }
                }
                Row::Same {
                    target,
                    updated,
                    normalized,
                } => {
                    let (id, kind, reloaded) = committed_row(target).ok_or_else(|| {
                        RuntimeError::Internal(format!(
                            "keyed bulk create: items[{index}] names no committed row"
                        ))
                    })?;
                    let (mut result, _) = self
                        .keyed_row_answer(token, id, &kind, *updated, verbose)
                        .await?;
                    let error = reindex_errors.get(&id).or(unattributed_error.as_ref());
                    if let (true, Some(error)) = (reloaded, error) {
                        result["reindex_error"] = json!(error);
                    }
                    ItemAnswer {
                        result,
                        normalized: normalized.clone(),
                        record: None,
                        created: false,
                        updated: *updated,
                    }
                }
            };
            answers.push(answer);
        }
        Ok(Ok(answers))
    }

    /// The answer of an item that created no row: the row it names, with
    /// `created: false`, and `updated: true` when the request rewrote it.
    /// Under `verbose` the record is read again after the commit.
    async fn keyed_row_answer(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        kind: &str,
        updated: bool,
        verbose: bool,
    ) -> Result<(Value, Option<Value>), RuntimeError> {
        let record = if verbose {
            let entity = self.runtime.get_entity(token, id).await?;
            Some(
                serde_json::to_value(&entity)
                    .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?,
            )
        } else {
            None
        };
        let mut result = match &record {
            Some(Value::Object(record)) => {
                let mut record = record.clone();
                record.insert("full_id".to_string(), json!(id.to_string()));
                Value::Object(record)
            }
            _ => json!({"id": id, "full_id": id.to_string(), "kind": kind}),
        };
        result["created"] = json!(false);
        if updated {
            result["updated"] = json!(true);
        }
        Ok((result, record))
    }

    /// Judge `items` and run them as one unit (ADR-221 section 1). A unit
    /// that rolls back is judged again from a fresh read while the holders
    /// it read keep moving, up to [`KEYED_BULK_ATTEMPTS`] times; a rollback
    /// with the same holders read is not a race and fails at once. After the
    /// last attempt the request is judged once more, so a key another writer
    /// now holds answers as it would have had that writer been first.
    pub(super) async fn commit_keyed_bulk(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        verbose: bool,
        items: &[PreparedBulkItem],
    ) -> Result<Vec<ItemAnswer>, KeyedBulkFailure> {
        let mut last: Option<(HolderReads, usize, String)> = None;
        for _ in 0..KEYED_BULK_ATTEMPTS {
            let (slots, read) = self.judge_keyed_bulk(token, registry, items).await?;
            if let Some((previous, index, failure)) = last.take() {
                if previous == read {
                    return Err(KeyedBulkFailure::RolledBack { index, failure });
                }
            }
            #[cfg(test)]
            wait_for_keyed_bulk_unit_test_barrier().await;
            match self.run_keyed_bulk_unit(token, verbose, slots).await? {
                Ok(answers) => return Ok(answers),
                Err((index, failure)) => last = Some((read, index, failure)),
            }
        }
        self.judge_keyed_bulk(token, registry, items).await?;
        let (_, index, failure) = last.unwrap_or_default();
        Err(KeyedBulkFailure::RolledBack {
            index,
            failure: format!(
                "{failure}; the holders of the request's keys changed during \
                 {KEYED_BULK_ATTEMPTS} attempts"
            ),
        })
    }
}
