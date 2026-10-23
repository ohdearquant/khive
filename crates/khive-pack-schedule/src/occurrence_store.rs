//! Pack-owned transactions for durable occurrence accounting.
//!
//! Recurrence search, canonical encoding and identifier allocation happen before
//! the writer is entered. The atomic callback performs SQL and row checks only.

use std::any::Any;
use std::sync::Arc;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::note::Note;
use khive_storage::types::{SqlRow, SqlStatement, SqlValue, StorageResult};
use khive_storage::{AtomicUnitOp, SqlAccess, SqlWriter, StorageError};
use serde::Serialize;
use uuid::Uuid;

use crate::misfire::MisfirePolicy;
use crate::occurrence::{
    next_nominal, Admission, AuxiliaryWork, ClaimHandle, DefinitionActor, DefinitionV3,
    LaunchIntent, ObservedResult, OccurrenceInstant, OccurrenceKey, OccurrenceObservation,
    OccurrencePage, ScheduledPayload, SeriesSnapshot, TerminalOutcome, TerminalReceipt, WorkPage,
    WorkPurpose, PASS_MATERIALIZATION_LIMIT,
};

const DEFINITION_INSERT: &str = include_str!("../sql/occurrence-definition-insert.sql");
const DEFINITION_GET: &str = include_str!("../sql/occurrence-definition-get.sql");
const SERIES_INSERT: &str = include_str!("../sql/occurrence-series-insert.sql");
const SERIES_GET: &str = include_str!("../sql/occurrence-series-get.sql");
const SERIES_PAGE: &str = include_str!("../sql/occurrence-series-page.sql");
const SERIES_CAS: &str = include_str!("../sql/occurrence-series-cas.sql");
const OCCURRENCE_INSERT: &str = include_str!("../sql/occurrence-insert.sql");
const OCCURRENCE_PAGE: &str = include_str!("../sql/occurrence-page.sql");
const ATTEMPT_CLAIM: &str = include_str!("../sql/occurrence-attempt-claim.sql");
const ATTEMPT_RENEW: &str = include_str!("../sql/occurrence-attempt-renew.sql");
const LAUNCH_INTENT: &str = include_str!("../sql/occurrence-launch-intent.sql");
const RECEIPT_INSERT: &str = include_str!("../sql/occurrence-receipt-insert.sql");
const RECEIPT_PAGE: &str = include_str!("../sql/occurrence-receipt-page.sql");
const OUTBOX_INSERT: &str = include_str!("../sql/occurrence-outbox-insert.sql");
const OUTBOX_PAGE: &str = include_str!("../sql/occurrence-outbox-page.sql");
const SCHEMA_PRESENCE: &str = include_str!("../sql/occurrence-schema-presence.sql");
const CLOCK: &str = include_str!("../sql/occurrence-clock.sql");
const LEASE_US: i64 = 300_000_000;

fn internal(message: impl ToString) -> StorageError {
    StorageError::Internal(message.to_string())
}
fn text(value: impl ToString) -> SqlValue {
    SqlValue::Text(value.to_string())
}
fn integer(value: i64) -> SqlValue {
    SqlValue::Integer(value)
}
fn opt_id(value: Option<Uuid>) -> SqlValue {
    value.map(text).unwrap_or(SqlValue::Null)
}
fn boxed<T: Send + 'static>(value: T) -> Box<dyn Any + Send> {
    Box::new(value)
}
fn limit(value: usize) -> Result<i64, RuntimeError> {
    if !(1..=PASS_MATERIALIZATION_LIMIT).contains(&value) {
        return Err(RuntimeError::InvalidInput(
            "schedule page limit must be 1..200".into(),
        ));
    }
    Ok(value as i64)
}
fn instant(row: &SqlRow, seconds: &str, nanos: &str) -> StorageResult<Option<OccurrenceInstant>> {
    match (
        row.opt_i64(seconds).map_err(internal)?,
        row.opt_i64(nanos).map_err(internal)?,
    ) {
        (None, None) => Ok(None),
        (Some(seconds), Some(nanos)) if (0..1_000_000_000).contains(&nanos) => {
            Ok(Some(OccurrenceInstant {
                seconds,
                nanos: nanos as u32,
            }))
        }
        _ => Err(internal("invalid stored schedule instant")),
    }
}
fn decode_series(row: &SqlRow) -> StorageResult<SeriesSnapshot> {
    Ok(SeriesSnapshot {
        store_id: row.uuid("store_id").map_err(internal)?,
        schedule_id: row.uuid("schedule_id").map_err(internal)?,
        definition_revision: row.i64("definition_revision").map_err(internal)?,
        namespace: row.text("namespace").map_err(internal)?.to_owned(),
        revision: row.i64("revision").map_err(internal)?,
        state: row.text("state").map_err(internal)?.to_owned(),
        next: instant(row, "next_seconds", "next_nanos")?,
        window_source: instant(row, "window_source_seconds", "window_source_nanos")?,
        horizon: instant(row, "horizon_seconds", "horizon_nanos")?,
        selected: instant(row, "selected_seconds", "selected_nanos")?,
        selection_at_us: row.opt_i64("selection_at_us").map_err(internal)?,
        effective_max_lateness_s: row.opt_i64("effective_max_lateness_s").map_err(internal)?,
        active_occurrence_id: row.opt_uuid("active_occurrence_id").map_err(internal)?,
        active_invocation_id: row.opt_uuid("active_invocation_id").map_err(internal)?,
        suspended_invocation_id: row.opt_uuid("suspended_invocation_id").map_err(internal)?,
        cancellation_cut_us: row.opt_i64("cancellation_cut_us").map_err(internal)?,
        accounting_blocked_reason: row
            .opt_text("accounting_blocked_reason")
            .map_err(internal)?
            .map(str::to_owned),
    })
}
fn series_get(store: Uuid, schedule: Uuid) -> SqlStatement {
    SqlStatement::new(SERIES_GET, vec![text(store), text(schedule)])
}
fn series_cas(series: &SeriesSnapshot, expected_revision: i64) -> SqlStatement {
    let pair = |value: Option<OccurrenceInstant>| {
        [
            SqlValue::from_opt_i64(value.map(|v| v.seconds)),
            SqlValue::from_opt_i64(value.map(|v| i64::from(v.nanos))),
        ]
    };
    let mut values = vec![
        text(series.store_id),
        text(series.schedule_id),
        integer(expected_revision),
        text(&series.state),
    ];
    values.extend(pair(series.next));
    values.extend(pair(series.window_source));
    values.extend(pair(series.horizon));
    values.extend(pair(series.selected));
    values.extend([
        SqlValue::from_opt_i64(series.selection_at_us),
        SqlValue::from_opt_i64(series.effective_max_lateness_s),
        opt_id(series.active_occurrence_id),
        opt_id(series.active_invocation_id),
        opt_id(series.suspended_invocation_id),
        SqlValue::from_opt_i64(series.cancellation_cut_us),
        SqlValue::from_opt_text(series.accounting_blocked_reason.as_deref()),
    ]);
    SqlStatement::new(SERIES_CAS, values)
}
async fn writer_now(writer: &mut dyn SqlWriter, _store_id: Uuid) -> StorageResult<i64> {
    #[cfg(test)]
    if let Some(value) = test_clock_read(_store_id)? {
        return value;
    }
    let row = writer
        .query_row(SqlStatement::new(CLOCK, vec![]))
        .await?
        .ok_or_else(|| internal("schedule clock returned no row"))?;
    row.i64("now_us").map_err(internal)
}
async fn require_one(writer: &mut dyn SqlWriter, statement: SqlStatement) -> StorageResult<()> {
    if writer.execute(statement).await? != 1 {
        return Err(internal("schedule write affected an unexpected row count"));
    }
    Ok(())
}
fn bytes(row: &SqlRow, name: &str) -> StorageResult<Vec<u8>> {
    match row.get(name) {
        Some(SqlValue::Blob(value)) => Ok(value.clone()),
        _ => Err(internal(format!("invalid stored {name}"))),
    }
}

/// Publish only derived schedule fields. Existing runtime/web provenance keys
/// refuse this generic projection; a refusal rolls back the entire owned unit.
async fn project_note(
    writer: &mut dyn SqlWriter,
    series: &SeriesSnapshot,
    next_text: Option<&str>,
    now: i64,
) -> StorageResult<()> {
    let changed = writer
        .execute(SqlStatement::new(
            include_str!("../sql/occurrence-note-projection.sql"),
            vec![
                text(series.store_id),
                text(series.schedule_id),
                text(&series.namespace),
                SqlValue::from_opt_text(next_text),
                integer(now),
            ],
        ))
        .await?;
    if changed == 1 {
        return Ok(());
    }
    if changed != 0 || series.cancellation_cut_us.is_none() {
        return Err(internal(
            "schedule display projection could not advance the live source note",
        ));
    }
    let row = writer
        .query_row(series_get(series.store_id, series.schedule_id))
        .await?
        .ok_or_else(|| internal("schedule projection lost its series"))?;
    if row.opt_uuid("source_note_id").map_err(internal)?.is_none()
        || row.opt_text("source_note_namespace").map_err(internal)?
            != Some(series.namespace.as_str())
        || row
            .opt_i64("source_note_deleted_at")
            .map_err(internal)?
            .is_some()
    {
        return Ok(());
    }
    Err(internal(
        "schedule display projection refused reserved properties or an unadvanceable revision",
    ))
}

/// Infrastructure facade over the effective schedule store, not the replay server's main database.
#[derive(Clone)]
pub struct OccurrenceStore {
    sql: Arc<dyn SqlAccess>,
    store_id: Uuid,
}
impl std::fmt::Debug for OccurrenceStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OccurrenceStore")
            .field("store_id", &self.store_id)
            .finish_non_exhaustive()
    }
}

pub(crate) struct PreparedDefinition {
    definition: DefinitionV3,
    canonical: Vec<u8>,
    digest: [u8; 32],
    properties: String,
    content: String,
    updated_at: i64,
}

#[derive(Clone, Debug)]
struct PreparedTerminal {
    receipt_id: Uuid,
    intent_id: Uuid,
    result: ObservedResult,
    evidence: Vec<u8>,
    intent: Vec<u8>,
    intent_digest: [u8; 32],
    recipient: DefinitionActor,
}
impl PreparedTerminal {
    fn new(
        definition: &DefinitionV3,
        occurrence: &OccurrenceObservation,
        result: ObservedResult,
    ) -> Result<Self, RuntimeError> {
        let receipt_id = Uuid::new_v5(&occurrence.id, b"terminal.v3");
        let intent_id = Uuid::new_v5(&occurrence.id, b"outcome-notice.v3");
        let evidence = serde_json::to_vec(&result.evidence)
            .map_err(|e| RuntimeError::Internal(e.to_string()))?;
        #[derive(Serialize)]
        struct Intent<'a> {
            schema_version: u8,
            store_id: Uuid,
            occurrence_id: Uuid,
            terminal_receipt_id: Uuid,
            recipient: &'a DefinitionActor,
            notice_kind: &'static str,
        }
        let intent = serde_json::to_vec(&Intent {
            schema_version: 3,
            store_id: occurrence.key.store_id,
            occurrence_id: occurrence.id,
            terminal_receipt_id: receipt_id,
            recipient: &definition.recipient,
            notice_kind: "outcome_notice",
        })
        .map_err(|e| RuntimeError::Internal(e.to_string()))?;
        let intent_digest = *blake3::hash(&intent).as_bytes();
        Ok(Self {
            receipt_id,
            intent_id,
            result,
            evidence,
            intent,
            intent_digest,
            recipient: definition.recipient.clone(),
        })
    }
}

#[derive(Clone, Debug)]
struct PreparedOccurrence {
    observation: OccurrenceObservation,
    terminal: Option<PreparedTerminal>,
}

/// A bounded recurrence page, fully prepared before entry to the writer.
#[derive(Clone, Debug)]
pub struct PreparedPage {
    before: SeriesSnapshot,
    after: SeriesSnapshot,
    rows: Vec<PreparedOccurrence>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountingPage {
    pub created: usize,
    pub next: Option<OccurrenceInstant>,
    pub has_more: bool,
    pub selected_occurrence_id: Option<Uuid>,
}

impl OccurrenceStore {
    pub fn open(runtime: &KhiveRuntime) -> Result<Self, RuntimeError> {
        let owner = runtime.backend().database_owner_identity().map_err(|e| {
            RuntimeError::Unconfigured(format!(
                "version-3 schedules require a durable file-backed schedule store: {e}"
            ))
        })?;
        Ok(Self {
            sql: runtime.sql(),
            store_id: owner.durable_id(),
        })
    }
    /// Optional legacy-compatible availability, without hiding durable-store or partial-schema failures.
    pub async fn for_runtime(runtime: &KhiveRuntime) -> Result<Option<Self>, RuntimeError> {
        if runtime.sql().database_path().is_none() {
            return Ok(None);
        }
        let store = Self::open(runtime)?;
        if store.is_installed().await? {
            Ok(Some(store))
        } else {
            Ok(None)
        }
    }

    pub fn store_id(&self) -> Uuid {
        self.store_id
    }

    // One owned transaction boundary. All callbacks below contain SQL and bounded bookkeeping.
    async fn commit_prepared<T: Send + 'static>(
        &self,
        operation: AtomicUnitOp,
    ) -> Result<T, RuntimeError> {
        let value = self.sql.atomic_unit(operation).await?;
        value
            .downcast::<T>()
            .map(|value| *value)
            .map_err(|_| RuntimeError::Internal("schedule transaction result type mismatch".into()))
    }

    /// Presence only: this is not a proof of complete schema integrity.
    pub async fn is_installed(&self) -> Result<bool, RuntimeError> {
        let mut reader = self.sql.reader().await?;
        let row = reader
            .query_row(SqlStatement::new(SCHEMA_PRESENCE, vec![]))
            .await?
            .ok_or_else(|| {
                RuntimeError::Internal("schedule schema presence returned no row".into())
            })?;
        match row.i64("present_tables").map_err(internal)? {
            0 => Ok(false),
            6 => Ok(true),
            _ => Err(RuntimeError::Unconfigured(
                "version-3 schedule schema is only partially installed".into(),
            )),
        }
    }

    pub async fn clock(&self) -> Result<i64, RuntimeError> {
        #[cfg(test)]
        if let Some(value) = test_clock_read(self.store_id)? {
            return value.map_err(Into::into);
        }

        // The SQL clock is registered on the dedicated writer connection.
        // Queue-backed writer handles route standalone SELECTs through readers,
        // so observe it inside the existing owned transaction instead.
        let store_id = self.store_id;
        self.commit_prepared(Box::new(move |writer| {
            Box::pin(async move {
                let now = writer_now(writer, store_id).await?;
                Ok(boxed(now))
            })
        }))
        .await
    }

    pub(crate) fn prepare_definition(
        definition: DefinitionV3,
        staged: &Note,
    ) -> Result<PreparedDefinition, RuntimeError> {
        if staged.id != definition.schedule_id
            || staged.namespace != definition.namespace
            || staged.deleted_at.is_some()
            || staged.kind != "scheduled_event"
        {
            return Err(RuntimeError::InvalidInput(
                "schedule activation note binding mismatch".into(),
            ));
        }
        let properties = staged.properties.as_ref().ok_or_else(|| {
            RuntimeError::InvalidInput("schedule activation lacks properties".into())
        })?;
        if properties
            .get("receipt_version")
            .and_then(serde_json::Value::as_i64)
            != Some(3)
            || properties.get("status").and_then(serde_json::Value::as_str) != Some("pending")
        {
            return Err(RuntimeError::InvalidInput(
                "schedule activation is not a staged version-3 declaration".into(),
            ));
        }
        let canonical = definition.canonical_bytes()?;
        let digest = definition.digest()?;
        Ok(PreparedDefinition {
            definition,
            canonical,
            digest,
            properties: properties.to_string(),
            content: staged.content.clone(),
            updated_at: staged.updated_at,
        })
    }

    pub(crate) async fn activate_definition(
        &self,
        prepared: PreparedDefinition,
    ) -> Result<(), RuntimeError> {
        let store_id = self.store_id;
        self.commit_prepared(Box::new(move |writer| {
            Box::pin(async move {
                let now = writer_now(writer, store_id).await?;
                let d = prepared.definition;
                let event_type = match &d.payload {
                    ScheduledPayload::Reminder { .. } => "remind",
                    ScheduledPayload::Action { .. } => "schedule",
                };
                require_one(
                    writer,
                    SqlStatement::new(
                        DEFINITION_INSERT,
                        vec![
                            text(store_id),
                            text(d.schedule_id),
                            integer(d.definition_revision),
                            text(&d.namespace),
                            SqlValue::Blob(prepared.digest.to_vec()),
                            SqlValue::Blob(prepared.canonical),
                            integer(now),
                            text(prepared.properties),
                            text(prepared.content),
                            integer(prepared.updated_at),
                            text(d.creator.label()),
                            text(event_type),
                        ],
                    ),
                )
                .await?;
                require_one(
                    writer,
                    SqlStatement::new(
                        SERIES_INSERT,
                        vec![
                            text(store_id),
                            text(d.schedule_id),
                            integer(d.definition_revision),
                            text(d.namespace),
                            integer(d.first_trigger.seconds),
                            integer(i64::from(d.first_trigger.nanos)),
                        ],
                    ),
                )
                .await?;
                Ok(boxed(()))
            })
        }))
        .await
    }

    pub async fn series(&self, schedule_id: Uuid) -> Result<Option<SeriesSnapshot>, RuntimeError> {
        let mut reader = self.sql.reader().await?;
        reader
            .query_row(series_get(self.store_id, schedule_id))
            .await?
            .as_ref()
            .map(decode_series)
            .transpose()
            .map_err(Into::into)
    }

    pub async fn definition(&self, series: &SeriesSnapshot) -> Result<DefinitionV3, RuntimeError> {
        if series.store_id != self.store_id {
            return Err(RuntimeError::InvalidInput(
                "schedule store binding mismatch".into(),
            ));
        }
        let mut reader = self.sql.reader().await?;
        let row = reader
            .query_row(SqlStatement::new(
                DEFINITION_GET,
                vec![
                    text(self.store_id),
                    text(series.schedule_id),
                    integer(series.definition_revision),
                ],
            ))
            .await?
            .ok_or_else(|| RuntimeError::Internal("schedule definition missing".into()))?;
        let d = DefinitionV3::authenticate(
            &bytes(&row, "canonical_definition")?,
            &bytes(&row, "definition_digest")?,
        )?;
        if d.schedule_id != series.schedule_id
            || d.definition_revision != series.definition_revision
            || d.namespace != series.namespace
        {
            return Err(RuntimeError::Internal(
                "schedule definition ownership mismatch".into(),
            ));
        }
        Ok(d)
    }

    /// Discover auxiliary work even if every display note has moved or disappeared.
    pub async fn series_work_page(
        &self,
        purpose: WorkPurpose,
        after: Option<Uuid>,
        page_limit: usize,
        materialization_allowance: usize,
    ) -> Result<WorkPage, RuntimeError> {
        let page_limit = limit(page_limit)?;
        let now = self.clock().await?;
        let mut reader = self.sql.reader().await?;
        let mut rows = reader
            .query_all(SqlStatement::new(
                SERIES_PAGE,
                vec![
                    text(self.store_id),
                    text(purpose.as_str()),
                    integer(now),
                    integer(materialization_allowance.min(PASS_MATERIALIZATION_LIMIT) as i64),
                    opt_id(after),
                    integer(page_limit + 1),
                ],
            ))
            .await?;
        let has_more = rows.len() > page_limit as usize;
        rows.truncate(page_limit as usize);
        let items = rows
            .iter()
            .map(|row| {
                Ok(AuxiliaryWork {
                    series: decode_series(row)?,
                    ready_unclaimed: row.opt_uuid("ready_unclaimed").map_err(internal)?,
                    expired_attempt: row.i64("expired_attempt").map_err(internal)? != 0,
                })
            })
            .collect::<StorageResult<Vec<_>>>()?;
        let next = items.last().map(|item| item.series.schedule_id);
        Ok(WorkPage {
            items,
            next,
            has_more,
        })
    }

    pub fn prepare_page(
        &self,
        snapshot: &SeriesSnapshot,
        definition: &DefinitionV3,
        captured_horizon_us: i64,
        global_grace_seconds: i64,
        allowance: usize,
    ) -> Result<PreparedPage, RuntimeError> {
        if snapshot.store_id != self.store_id
            || definition.schedule_id != snapshot.schedule_id
            || definition.namespace != snapshot.namespace
            || definition.definition_revision != snapshot.definition_revision
        {
            return Err(RuntimeError::InvalidInput(
                "schedule page binding mismatch".into(),
            ));
        }
        if global_grace_seconds < 0 {
            return Err(RuntimeError::InvalidInput(
                "schedule grace must be nonnegative".into(),
            ));
        }
        let mut after = snapshot.clone();
        let mut rows = Vec::new();
        let Some(mut current) = snapshot.next else {
            return Ok(PreparedPage {
                before: snapshot.clone(),
                after,
                rows,
            });
        };
        let horizon = OccurrenceInstant::from_micros(captured_horizon_us)?;
        let repeat = definition
            .repeat
            .as_deref()
            .map(crate::repeat::parse_repeat)
            .transpose()
            .map_err(RuntimeError::InvalidInput)?;
        if after.horizon.is_none() {
            // An old selected identity must settle before a new reconciliation window can replace it.
            if after.selected.is_some() {
                return Ok(PreparedPage {
                    before: snapshot.clone(),
                    after,
                    rows,
                });
            }
            after.window_source = Some(current);
            after.horizon = Some(horizon);
            after.selection_at_us = Some(captured_horizon_us);
            after.effective_max_lateness_s = Some(match definition.policy()? {
                MisfirePolicy::Skip => global_grace_seconds,
                MisfirePolicy::CatchUpOnce { max_lateness } => max_lateness,
            });
            after.selected = Some(match definition.policy()? {
                MisfirePolicy::Skip => current,
                MisfirePolicy::CatchUpOnce { .. } => match &repeat {
                    Some(r) => OccurrenceInstant::from_datetime(
                        r.latest_at_or_before(
                            current.datetime()?,
                            horizon.datetime()?,
                            Some(definition.monthly_anchor.datetime()?),
                        )
                        .ok_or_else(|| {
                            RuntimeError::InvalidInput(
                                "schedule recurrence has no selected occurrence".into(),
                            )
                        })?,
                    ),
                    None => current,
                },
            });
        }
        let horizon = after
            .horizon
            .ok_or_else(|| RuntimeError::Internal("schedule horizon missing".into()))?;
        let selection_at_us = after
            .selection_at_us
            .ok_or_else(|| RuntimeError::Internal("schedule selection time missing".into()))?;
        let bound = after
            .effective_max_lateness_s
            .ok_or_else(|| RuntimeError::Internal("schedule bound missing".into()))?;
        if after
            .cancellation_cut_us
            .is_some_and(|cut| current.nanos_since_epoch() >= i128::from(cut) * 1000)
        {
            after.next = None;
            after.selected = None;
            clear_completed_window(&mut after);
            return Ok(PreparedPage {
                before: snapshot.clone(),
                after,
                rows,
            });
        }
        for _ in 0..allowance.min(PASS_MATERIALIZATION_LIMIT) {
            if current > horizon {
                break;
            }
            if after
                .cancellation_cut_us
                .is_some_and(|cut| current.nanos_since_epoch() >= i128::from(cut) * 1000)
            {
                after.next = None;
                break;
            }
            let key = OccurrenceKey {
                store_id: self.store_id,
                schedule_id: snapshot.schedule_id,
                definition_revision: snapshot.definition_revision,
                scheduled_for: current,
            };
            let observation = OccurrenceObservation {
                id: key.id(),
                key,
                namespace: snapshot.namespace.clone(),
                materialized_at_us: captured_horizon_us,
                selection_at_us,
                effective_max_lateness_s: bound,
                terminal: None,
            };
            let result = if after.cancellation_cut_us.is_some() {
                Some(ObservedResult::non_invoked(
                    TerminalOutcome::Cancelled,
                    "schedule_cancelled",
                ))
            } else if after.suspended_invocation_id.is_some() {
                Some(ObservedResult::non_invoked(
                    TerminalOutcome::Refused,
                    "unresolved_prior_launch",
                ))
            } else if after.active_invocation_id.is_some() {
                Some(ObservedResult::non_invoked(
                    TerminalOutcome::Refused,
                    "overlap",
                ))
            } else if Some(current) != after.selected {
                Some(ObservedResult::non_invoked(
                    TerminalOutcome::Missed,
                    if matches!(definition.policy()?, MisfirePolicy::CatchUpOnce { .. }) {
                        "superseded_by_catch_up"
                    } else {
                        "not_selected_by_skip"
                    },
                ))
            } else if !current.within_bound(selection_at_us, bound) {
                Some(ObservedResult::non_invoked(
                    TerminalOutcome::Missed,
                    "selection_deadline_exceeded",
                ))
            } else {
                None
            };
            let terminal = result
                .map(|result| PreparedTerminal::new(definition, &observation, result))
                .transpose()?;
            rows.push(PreparedOccurrence {
                observation,
                terminal,
            });
            let next = match &repeat {
                Some(r) => next_nominal(
                    r,
                    definition.monthly_anchor.datetime()?,
                    current.datetime()?,
                )
                .map(OccurrenceInstant::from_datetime),
                None => None,
            };
            let next = next.filter(|next| {
                after
                    .cancellation_cut_us
                    .is_none_or(|cut| next.nanos_since_epoch() < i128::from(cut) * 1000)
            });
            after.next = next;
            let Some(next) = next else {
                break;
            };
            if next <= current {
                return Err(RuntimeError::InvalidInput(
                    "schedule recurrence did not advance".into(),
                ));
            }
            current = next;
        }
        let selected_can_launch = after.cancellation_cut_us.is_none()
            && after.active_invocation_id.is_none()
            && after.suspended_invocation_id.is_none()
            && after
                .selected
                .is_some_and(|selected| selected.within_bound(selection_at_us, bound));
        if after
            .next
            .zip(after.horizon)
            .is_none_or(|(next, horizon)| next > horizon)
            && !selected_can_launch
        {
            after.selected = None;
            clear_completed_window(&mut after);
        }
        if after.next.is_none()
            && after.cancellation_cut_us.is_none()
            && after.selected.is_none()
            && after.active_invocation_id.is_none()
            && after.suspended_invocation_id.is_none()
        {
            after.state = "exhausted".into();
        }
        Ok(PreparedPage {
            before: snapshot.clone(),
            after,
            rows,
        })
    }

    pub async fn commit_page(
        &self,
        page: PreparedPage,
    ) -> Result<Option<AccountingPage>, RuntimeError> {
        if page.rows.is_empty() && page.before == page.after {
            return Ok(Some(AccountingPage {
                created: 0,
                next: page.before.next,
                has_more: page
                    .before
                    .next
                    .zip(page.before.horizon)
                    .is_some_and(|(n, h)| n <= h),
                selected_occurrence_id: None,
            }));
        }
        let projection_text = page
            .after
            .next
            .or(page.after.selected)
            .map(|instant| instant.datetime().map(|instant| instant.to_rfc3339()))
            .transpose()?;
        self.commit_prepared(Box::new(move |writer| {
            Box::pin(async move {
                let current = writer
                    .query_row(series_get(page.before.store_id, page.before.schedule_id))
                    .await?
                    .as_ref()
                    .map(decode_series)
                    .transpose()?;
                if current.as_ref().map(|s| s.revision) != Some(page.before.revision) {
                    return Ok(boxed(None::<AccountingPage>));
                }
                let now = writer_now(writer, page.before.store_id).await?;
                require_one(writer, series_cas(&page.after, page.before.revision)).await?;
                project_note(writer, &page.after, projection_text.as_deref(), now).await?;
                let mut selected = None;
                for row in &page.rows {
                    let o = &row.observation;
                    require_one(
                        writer,
                        SqlStatement::new(
                            OCCURRENCE_INSERT,
                            vec![
                                text(o.id),
                                text(o.key.store_id),
                                text(o.key.schedule_id),
                                integer(o.key.definition_revision),
                                text(&o.namespace),
                                integer(o.key.scheduled_for.seconds),
                                integer(i64::from(o.key.scheduled_for.nanos)),
                                integer(now),
                                integer(o.selection_at_us),
                                integer(o.effective_max_lateness_s),
                            ],
                        ),
                    )
                    .await?;
                    if let Some(terminal) = &row.terminal {
                        insert_terminal(writer, o, terminal, None, None, now).await?;
                    } else {
                        selected = Some(o.id);
                    }
                }
                Ok(boxed(Some(AccountingPage {
                    created: page.rows.len(),
                    next: page.after.next,
                    has_more: page
                        .after
                        .next
                        .zip(page.after.horizon)
                        .is_some_and(|(n, h)| n <= h),
                    selected_occurrence_id: selected,
                })))
            })
        }))
        .await
    }
}

async fn insert_terminal(
    writer: &mut dyn SqlWriter,
    occurrence: &OccurrenceObservation,
    prepared: &PreparedTerminal,
    invocation: Option<Uuid>,
    final_decision: Option<i64>,
    now: i64,
) -> StorageResult<()> {
    let result = &prepared.result;
    require_one(
        writer,
        SqlStatement::new(
            RECEIPT_INSERT,
            vec![
                text(prepared.receipt_id),
                text(occurrence.id),
                text(&occurrence.namespace),
                text("terminal"),
                text(result.outcome.as_str()),
                text(&result.reason_code),
                opt_id(invocation),
                opt_id(result.exec_receipt_id),
                SqlValue::Null,
                integer(now),
                SqlValue::from_opt_i64(final_decision),
                SqlValue::from_opt_i64(result.started_at_us),
                SqlValue::from_opt_i64(result.completed_at_us),
                SqlValue::from_opt_i64(result.exit_code),
                SqlValue::from_opt_i64(result.exit_signal),
                SqlValue::Blob(prepared.evidence.clone()),
            ],
        ),
    )
    .await?;
    require_one(
        writer,
        SqlStatement::new(
            OUTBOX_INSERT,
            vec![
                text(prepared.intent_id),
                text(occurrence.key.store_id),
                text(occurrence.id),
                text(prepared.receipt_id),
                text(&occurrence.namespace),
                text(&prepared.recipient.kind),
                text(&prepared.recipient.id),
                SqlValue::Blob(prepared.intent_digest.to_vec()),
                SqlValue::Blob(prepared.intent.clone()),
                integer(now),
            ],
        ),
    )
    .await
}

fn occurrence_query(
    store: Uuid,
    namespace: Option<&str>,
    schedule: Uuid,
    revision: Option<i64>,
    after: Option<(OccurrenceInstant, Uuid)>,
    page_limit: i64,
    id: Option<Uuid>,
) -> SqlStatement {
    SqlStatement::new(
        OCCURRENCE_PAGE,
        vec![
            text(store),
            SqlValue::from_opt_text(namespace),
            text(schedule),
            SqlValue::from_opt_i64(revision),
            SqlValue::from_opt_i64(after.map(|(instant, _)| instant.seconds)),
            SqlValue::from_opt_i64(after.map(|(instant, _)| i64::from(instant.nanos))),
            opt_id(after.map(|(_, id)| id)),
            integer(page_limit),
            opt_id(id),
        ],
    )
}
fn decode_occurrence(row: &SqlRow) -> StorageResult<OccurrenceObservation> {
    let scheduled_for = OccurrenceInstant {
        seconds: row.i64("scheduled_seconds").map_err(internal)?,
        nanos: u32::try_from(row.i64("scheduled_nanos").map_err(internal)?).map_err(internal)?,
    };
    let key = OccurrenceKey {
        store_id: row.uuid("store_id").map_err(internal)?,
        schedule_id: row.uuid("schedule_id").map_err(internal)?,
        definition_revision: row.i64("definition_revision").map_err(internal)?,
        scheduled_for,
    };
    let id = row.uuid("id").map_err(internal)?;
    if id != key.id() {
        return Err(internal("stored occurrence identity mismatch"));
    }
    Ok(OccurrenceObservation {
        id,
        key,
        namespace: row.text("namespace").map_err(internal)?.to_owned(),
        materialized_at_us: row.i64("materialized_at_us").map_err(internal)?,
        selection_at_us: row.i64("selection_at_us").map_err(internal)?,
        effective_max_lateness_s: row.i64("effective_max_lateness_s").map_err(internal)?,
        terminal: None,
    })
}
fn same_claim(row: &SqlRow, claim: &ClaimHandle) -> StorageResult<bool> {
    Ok(
        row.opt_uuid("invocation_id").map_err(internal)? == Some(claim.invocation_id)
            && row.opt_i64("claim_generation").map_err(internal)? == Some(claim.generation)
            && row.opt_uuid("owner_nonce").map_err(internal)? == Some(claim.owner_nonce),
    )
}
fn clear_completed_window(series: &mut SeriesSnapshot) {
    if series
        .next
        .zip(series.horizon)
        .is_none_or(|(next, horizon)| next > horizon)
    {
        series.window_source = None;
        series.horizon = None;
        series.selection_at_us = None;
        series.effective_max_lateness_s = None;
    }
}
fn prepared_receipt(
    occurrence: &OccurrenceObservation,
    prepared: PreparedTerminal,
    invocation: Option<Uuid>,
    decision: Option<i64>,
    now: i64,
) -> TerminalReceipt {
    TerminalReceipt {
        id: prepared.receipt_id,
        occurrence_id: occurrence.id,
        outcome: prepared.result.outcome.as_str().into(),
        reason_code: prepared.result.reason_code,
        invocation_id: invocation,
        recorded_at_us: now,
        final_decision_at_us: decision,
        evidence: prepared.result.evidence,
    }
}

impl OccurrenceStore {
    pub async fn occurrences(
        &self,
        token: &NamespaceToken,
        schedule: Uuid,
        revision: Option<i64>,
        after: Option<(OccurrenceInstant, Uuid)>,
        page_limit: usize,
    ) -> Result<OccurrencePage, RuntimeError> {
        let page_limit = limit(page_limit)?;
        let mut reader = self.sql.reader().await?;
        let mut rows = reader
            .query_all(occurrence_query(
                self.store_id,
                Some(token.namespace().as_str()),
                schedule,
                revision,
                after,
                page_limit + 1,
                None,
            ))
            .await?;
        let has_more = rows.len() > page_limit as usize;
        rows.truncate(page_limit as usize);
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let mut item = decode_occurrence(&row)?;
            if row
                .opt_uuid("terminal_receipt_id")
                .map_err(internal)?
                .is_some()
            {
                let terminal_id = row.opt_uuid("terminal_receipt_id").map_err(internal)?;
                item.terminal = Some(serde_json::json!({"id":terminal_id,
                    "outcome":row.opt_text("terminal_outcome").map_err(internal)?,
                    "reason_code":row.opt_text("terminal_reason_code").map_err(internal)?,
                    "record_kind":"terminal",
                    "invocation_id":row.opt_uuid("invocation_id").map_err(internal)?,
                    "exec_receipt_id":row.opt_uuid("terminal_exec_receipt_id").map_err(internal)?,
                    "recorded_at_us":row.opt_i64("terminal_recorded_at_us").map_err(internal)?,
                    "final_decision_at_us":row.opt_i64("terminal_final_decision_at_us").map_err(internal)?,
                    "started_at_us":row.opt_i64("terminal_started_at_us").map_err(internal)?,
                    "completed_at_us":row.opt_i64("terminal_completed_at_us").map_err(internal)?,
                    "exit_code":row.opt_i64("terminal_exit_code").map_err(internal)?,
                    "exit_signal":row.opt_i64("terminal_exit_signal").map_err(internal)?,
                    "evidence":serde_json::from_slice::<serde_json::Value>(&bytes(&row,"terminal_evidence")?).map_err(internal)?}));
            }
            items.push(item);
        }
        Ok(OccurrencePage { items, has_more })
    }

    /// Token-scoped append-only receipt history; continuation is exclusive (time, ID).
    pub async fn receipt(
        &self,
        token: &NamespaceToken,
        occurrence_id: Uuid,
        after: Option<(i64, Uuid)>,
        page_limit: usize,
    ) -> Result<ReceiptPage, RuntimeError> {
        let page_limit = limit(page_limit)?;
        let mut reader = self.sql.reader().await?;
        let mut rows = reader
            .query_all(receipt_query(
                self.store_id,
                Some(token.namespace().as_str()),
                occurrence_id,
                after,
                page_limit + 1,
                None,
            ))
            .await?;
        let has_more = rows.len() > page_limit as usize;
        rows.truncate(page_limit as usize);
        let items = rows
            .iter()
            .map(receipt_json)
            .collect::<StorageResult<Vec<_>>>()?;
        Ok(ReceiptPage { items, has_more })
    }

    pub async fn notice_intent_work_page(
        &self,
        after: Option<(i64, Uuid)>,
        page_limit: usize,
    ) -> Result<NoticeIntentPage, RuntimeError> {
        self.notice_page(None, after, page_limit).await
    }
    pub async fn pending_notices(
        &self,
        token: &NamespaceToken,
        after: Option<(i64, Uuid)>,
        page_limit: usize,
    ) -> Result<NoticeIntentPage, RuntimeError> {
        self.notice_page(Some(token.namespace().as_str()), after, page_limit)
            .await
    }
    async fn notice_page(
        &self,
        namespace: Option<&str>,
        after: Option<(i64, Uuid)>,
        page_limit: usize,
    ) -> Result<NoticeIntentPage, RuntimeError> {
        let page_limit = limit(page_limit)?;
        let mut reader = self.sql.reader().await?;
        let mut rows = reader
            .query_all(SqlStatement::new(
                OUTBOX_PAGE,
                vec![
                    text(self.store_id),
                    SqlValue::from_opt_i64(after.map(|(at, _)| at)),
                    opt_id(after.map(|(_, id)| id)),
                    integer(page_limit + 1),
                    SqlValue::from_opt_text(namespace),
                ],
            ))
            .await?;
        let has_more = rows.len() > page_limit as usize;
        rows.truncate(page_limit as usize);
        let items = rows
            .iter()
            .map(|row| {
                Ok(NoticeIntent {
                    id: row.uuid("id").map_err(internal)?,
                    occurrence_id: row.uuid("occurrence_id").map_err(internal)?,
                    terminal_receipt_id: row.uuid("terminal_receipt_id").map_err(internal)?,
                    namespace: row.text("namespace").map_err(internal)?.into(),
                    created_at_us: row.i64("created_at_us").map_err(internal)?,
                    canonical_intent: bytes(row, "canonical_intent")?,
                    intent_digest: bytes(row, "intent_digest")?,
                })
            })
            .collect::<StorageResult<Vec<_>>>()?;
        Ok(NoticeIntentPage { items, has_more })
    }

    /// Claim a previously materialized candidate; never creates an occurrence identity.
    pub async fn claim_candidate(
        &self,
        schedule: Uuid,
        occurrence_id: Uuid,
    ) -> Result<Option<ClaimHandle>, RuntimeError> {
        let store = self.store_id;
        let attempt_id = Uuid::new_v4();
        let invocation_id = Uuid::new_v4();
        let owner_nonce = Uuid::new_v4();
        self.commit_prepared(Box::new(move |writer| {
            Box::pin(async move {
                let Some(row) = writer.query_row(series_get(store, schedule)).await? else {
                    return Ok(boxed(None::<ClaimHandle>));
                };
                let mut series = decode_series(&row)?;
                // Enumeration may pass a materialized identity, but its remaining window must finish before launch.
                if series
                    .next
                    .zip(series.horizon)
                    .is_some_and(|(next, horizon)| next <= horizon)
                {
                    return Ok(boxed(None::<ClaimHandle>));
                }
                let Some(row) = writer
                    .query_row(occurrence_query(
                        store,
                        None,
                        schedule,
                        Some(series.definition_revision),
                        None,
                        1,
                        Some(occurrence_id),
                    ))
                    .await?
                else {
                    return Ok(boxed(None::<ClaimHandle>));
                };
                let occurrence = decode_occurrence(&row)?;
                if occurrence.namespace != series.namespace
                    || row
                        .opt_uuid("terminal_receipt_id")
                        .map_err(internal)?
                        .is_some()
                    || row
                        .opt_uuid("launch_intent_id")
                        .map_err(internal)?
                        .is_some()
                    || (series.selected != Some(occurrence.key.scheduled_for)
                        && series.active_occurrence_id != Some(occurrence_id))
                    || series.suspended_invocation_id.is_some()
                    || series
                        .active_occurrence_id
                        .is_some_and(|id| id != occurrence_id)
                {
                    return Ok(boxed(None::<ClaimHandle>));
                }
                let now = writer_now(writer, store).await?;
                let expires = now
                    .checked_add(LEASE_US)
                    .ok_or_else(|| internal("schedule lease overflow"))?;
                if writer
                    .execute(SqlStatement::new(
                        ATTEMPT_CLAIM,
                        vec![
                            text(attempt_id),
                            text(occurrence_id),
                            text(&occurrence.namespace),
                            text(invocation_id),
                            text(owner_nonce),
                            integer(expires),
                            integer(now),
                        ],
                    ))
                    .await?
                    == 0
                {
                    return Ok(boxed(None::<ClaimHandle>));
                }
                let row = writer
                    .query_row(occurrence_query(
                        store,
                        None,
                        schedule,
                        Some(series.definition_revision),
                        None,
                        1,
                        Some(occurrence_id),
                    ))
                    .await?
                    .ok_or_else(|| internal("claimed occurrence missing"))?;
                let actual_invocation = row.uuid("invocation_id").map_err(internal)?;
                let generation = row.i64("claim_generation").map_err(internal)?;
                series.active_occurrence_id = Some(occurrence_id);
                series.active_invocation_id = Some(actual_invocation);
                series.selected = None;
                clear_completed_window(&mut series);
                require_one(writer, series_cas(&series, series.revision)).await?;
                project_note(writer, &series, None, now).await?;
                Ok(boxed(Some(ClaimHandle {
                    store_id: store,
                    occurrence,
                    invocation_id: actual_invocation,
                    generation,
                    owner_nonce,
                })))
            })
        }))
        .await
    }

    pub async fn renew_claim(&self, claim: &ClaimHandle) -> Result<bool, RuntimeError> {
        if claim.store_id != self.store_id {
            return Err(RuntimeError::InvalidInput(
                "schedule claim store mismatch".into(),
            ));
        }
        let claim = claim.clone();
        self.commit_prepared(Box::new(move |writer| {
            Box::pin(async move {
                let now = writer_now(writer, claim.store_id).await?;
                let expires = now
                    .checked_add(LEASE_US)
                    .ok_or_else(|| internal("schedule lease overflow"))?;
                let changed = writer
                    .execute(SqlStatement::new(
                        ATTEMPT_RENEW,
                        vec![
                            text(claim.occurrence.id),
                            text(claim.invocation_id),
                            integer(claim.generation),
                            text(claim.owner_nonce),
                            integer(expires),
                        ],
                    ))
                    .await?;
                if changed > 1 {
                    return Err(internal("schedule renewal affected multiple claims"));
                }
                Ok(boxed(changed == 1))
            })
        }))
        .await
    }

    pub async fn begin_invocation(&self, claim: &ClaimHandle) -> Result<Admission, RuntimeError> {
        let snapshot = self
            .series(claim.occurrence.key.schedule_id)
            .await?
            .ok_or_else(|| RuntimeError::Internal("schedule series missing".into()))?;
        let definition = self.definition(&snapshot).await?;
        let missed = PreparedTerminal::new(
            &definition,
            &claim.occurrence,
            ObservedResult::non_invoked(TerminalOutcome::Missed, "final_deadline_exceeded"),
        )?;
        let backwards = PreparedTerminal::new(
            &definition,
            &claim.occurrence,
            ObservedResult::non_invoked(TerminalOutcome::Refused, "clock_order_unavailable"),
        )?;
        let cancelled = PreparedTerminal::new(
            &definition,
            &claim.occurrence,
            ObservedResult::non_invoked(TerminalOutcome::Cancelled, "schedule_cancelled"),
        )?;
        let intent_id = Uuid::new_v4();
        let claim = claim.clone();
        if claim.store_id != self.store_id {
            return Err(RuntimeError::InvalidInput(
                "schedule claim store mismatch".into(),
            ));
        }
        self.commit_prepared(Box::new(move |writer| {
            Box::pin(async move {
                let Some(row) = writer
                    .query_row(series_get(claim.store_id, claim.occurrence.key.schedule_id))
                    .await?
                else {
                    return Ok(boxed(Admission::Lost));
                };
                let mut series = decode_series(&row)?;
                let Some(row) = writer
                    .query_row(occurrence_query(
                        claim.store_id,
                        None,
                        series.schedule_id,
                        Some(series.definition_revision),
                        None,
                        1,
                        Some(claim.occurrence.id),
                    ))
                    .await?
                else {
                    return Ok(boxed(Admission::Lost));
                };
                if !same_claim(&row, &claim)?
                    || row
                        .opt_uuid("terminal_receipt_id")
                        .map_err(internal)?
                        .is_some()
                    || row
                        .opt_uuid("launch_intent_id")
                        .map_err(internal)?
                        .is_some()
                    || series.active_invocation_id != Some(claim.invocation_id)
                    || series.suspended_invocation_id.is_some()
                {
                    return Ok(boxed(Admission::Lost));
                }
                let now = writer_now(writer, claim.store_id).await?;
                let terminal = if series.cancellation_cut_us.is_some() {
                    Some(cancelled)
                } else if now < claim.occurrence.selection_at_us {
                    Some(backwards)
                } else if !claim
                    .occurrence
                    .key
                    .scheduled_for
                    .within_bound(now, claim.occurrence.effective_max_lateness_s)
                {
                    Some(missed)
                } else {
                    None
                };
                if let Some(terminal) = terminal {
                    insert_terminal(
                        writer,
                        &claim.occurrence,
                        &terminal,
                        Some(claim.invocation_id),
                        Some(now),
                        now,
                    )
                    .await?;
                    series.active_occurrence_id = None;
                    series.active_invocation_id = None;
                    if series.next.is_none()
                        && series.cancellation_cut_us.is_none()
                        && series.suspended_invocation_id.is_none()
                    {
                        series.state = "exhausted".into();
                    }
                    require_one(writer, series_cas(&series, series.revision)).await?;
                    project_note(writer, &series, None, now).await?;
                    return Ok(boxed(Admission::Settled(prepared_receipt(
                        &claim.occurrence,
                        terminal,
                        Some(claim.invocation_id),
                        Some(now),
                        now,
                    ))));
                }
                if writer
                    .execute(SqlStatement::new(
                        LAUNCH_INTENT,
                        vec![
                            text(claim.occurrence.id),
                            text(claim.invocation_id),
                            integer(claim.generation),
                            text(claim.owner_nonce),
                            text(intent_id),
                            integer(now),
                        ],
                    ))
                    .await?
                    == 0
                {
                    return Ok(boxed(Admission::Lost));
                }
                Ok(boxed(Admission::Launch(LaunchIntent {
                    claim,
                    intent_id,
                    admission_at_us: now,
                })))
            })
        }))
        .await
    }

    pub async fn finish(
        &self,
        intent: &LaunchIntent,
        result: ObservedResult,
    ) -> Result<Option<TerminalReceipt>, RuntimeError> {
        if intent.claim.store_id != self.store_id {
            return Err(RuntimeError::InvalidInput(
                "schedule invocation store mismatch".into(),
            ));
        }
        let snapshot = self
            .series(intent.claim.occurrence.key.schedule_id)
            .await?
            .ok_or_else(|| RuntimeError::Internal("schedule series missing".into()))?;
        let definition = self.definition(&snapshot).await?;
        let terminal = PreparedTerminal::new(&definition, &intent.claim.occurrence, result)?;
        let intent = intent.clone();
        self.commit_prepared(Box::new(move |writer| {
            Box::pin(async move {
                let Some(row) = writer
                    .query_row(series_get(
                        intent.claim.store_id,
                        intent.claim.occurrence.key.schedule_id,
                    ))
                    .await?
                else {
                    return Ok(boxed(None::<TerminalReceipt>));
                };
                let mut series = decode_series(&row)?;
                let row = writer
                    .query_row(occurrence_query(
                        intent.claim.store_id,
                        None,
                        series.schedule_id,
                        Some(series.definition_revision),
                        None,
                        1,
                        Some(intent.claim.occurrence.id),
                    ))
                    .await?
                    .ok_or_else(|| internal("invoked occurrence missing"))?;
                if !same_claim(&row, &intent.claim)?
                    || row.opt_uuid("launch_intent_id").map_err(internal)? != Some(intent.intent_id)
                    || row
                        .opt_uuid("terminal_receipt_id")
                        .map_err(internal)?
                        .is_some()
                {
                    return Ok(boxed(None::<TerminalReceipt>));
                }
                let now = writer_now(writer, intent.claim.store_id).await?;
                insert_terminal(
                    writer,
                    &intent.claim.occurrence,
                    &terminal,
                    Some(intent.claim.invocation_id),
                    Some(intent.admission_at_us),
                    now,
                )
                .await?;
                if series.active_invocation_id == Some(intent.claim.invocation_id) {
                    series.active_occurrence_id = None;
                    series.active_invocation_id = None;
                }
                if terminal.result.outcome == TerminalOutcome::Indeterminate {
                    series.suspended_invocation_id = Some(intent.claim.invocation_id);
                    if series.state != "cancelled" {
                        series.state = "suspended".into();
                    }
                }
                if series.next.is_none()
                    && series.cancellation_cut_us.is_none()
                    && series.suspended_invocation_id.is_none()
                {
                    series.state = "exhausted".into();
                }
                require_one(writer, series_cas(&series, series.revision)).await?;
                project_note(writer, &series, None, now).await?;
                Ok(boxed(Some(prepared_receipt(
                    &intent.claim.occurrence,
                    terminal,
                    Some(intent.claim.invocation_id),
                    Some(intent.admission_at_us),
                    now,
                ))))
            })
        }))
        .await
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct NoticeIntent {
    pub id: Uuid,
    pub occurrence_id: Uuid,
    pub terminal_receipt_id: Uuid,
    pub namespace: String,
    pub created_at_us: i64,
    pub canonical_intent: Vec<u8>,
    pub intent_digest: Vec<u8>,
}
#[derive(Clone, Debug, Serialize)]
pub struct NoticeIntentPage {
    pub items: Vec<NoticeIntent>,
    pub has_more: bool,
}

fn receipt_json(row: &SqlRow) -> StorageResult<serde_json::Value> {
    let evidence: serde_json::Value =
        serde_json::from_slice(&bytes(row, "canonical_evidence")?).map_err(internal)?;
    Ok(
        serde_json::json!({"id":row.uuid("id").map_err(internal)?,"occurrence_id":row.uuid("occurrence_id").map_err(internal)?,
        "record_kind":row.text("record_kind").map_err(internal)?,"outcome":row.text("outcome").map_err(internal)?,
        "reason_code":row.text("reason_code").map_err(internal)?,"invocation_id":row.opt_uuid("invocation_id").map_err(internal)?,
        "exec_receipt_id":row.opt_uuid("exec_receipt_id").map_err(internal)?,"prior_receipt_id":row.opt_uuid("prior_receipt_id").map_err(internal)?,
        "recorded_at_us":row.i64("recorded_at_us").map_err(internal)?,"final_decision_at_us":row.opt_i64("final_decision_at_us").map_err(internal)?,
        "started_at_us":row.opt_i64("started_at_us").map_err(internal)?,"completed_at_us":row.opt_i64("completed_at_us").map_err(internal)?,
        "exit_code":row.opt_i64("exit_code").map_err(internal)?,"exit_signal":row.opt_i64("exit_signal").map_err(internal)?,"evidence":evidence}),
    )
}

#[derive(Clone, Debug, Serialize)]
pub struct ReceiptPage {
    pub items: Vec<serde_json::Value>,
    pub has_more: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct CancellationCut {
    pub changed: bool,
    pub cut_at_us: i64,
}
#[derive(Clone, Debug)]
pub enum Recovery {
    None,
    Reclaimable {
        schedule_id: Uuid,
        occurrence_id: Uuid,
    },
    Settled(TerminalReceipt),
}

fn receipt_query(
    store: Uuid,
    namespace: Option<&str>,
    occurrence: Uuid,
    after: Option<(i64, Uuid)>,
    page_limit: i64,
    kind: Option<&str>,
) -> SqlStatement {
    SqlStatement::new(
        RECEIPT_PAGE,
        vec![
            text(store),
            SqlValue::from_opt_text(namespace),
            text(occurrence),
            SqlValue::from_opt_i64(after.map(|(at, _)| at)),
            opt_id(after.map(|(_, id)| id)),
            integer(page_limit),
            SqlValue::from_opt_text(kind),
        ],
    )
}

impl OccurrenceStore {
    async fn observation(
        &self,
        schedule: Uuid,
        id: Uuid,
    ) -> Result<Option<OccurrenceObservation>, RuntimeError> {
        let mut reader = self.sql.reader().await?;
        reader
            .query_row(occurrence_query(
                self.store_id,
                None,
                schedule,
                None,
                None,
                1,
                Some(id),
            ))
            .await?
            .as_ref()
            .map(decode_occurrence)
            .transpose()
            .map_err(Into::into)
    }

    /// Cancel in the immutable original namespace. The cut never depends on a display note's location.
    pub async fn cancel_v3(
        &self,
        token: &NamespaceToken,
        schedule: Uuid,
    ) -> Result<Option<CancellationCut>, RuntimeError> {
        let Some(snapshot) = self.series(schedule).await? else {
            return Ok(None);
        };
        if snapshot.namespace != token.namespace().as_str() {
            return Ok(None);
        }
        let definition = self.definition(&snapshot).await?;
        let candidate = snapshot.active_occurrence_id.or_else(|| {
            snapshot.selected.map(|scheduled_for| {
                OccurrenceKey {
                    store_id: self.store_id,
                    schedule_id: schedule,
                    definition_revision: snapshot.definition_revision,
                    scheduled_for,
                }
                .id()
            })
        });
        let observation = match candidate {
            Some(id) => self.observation(schedule, id).await?,
            None => None,
        };
        let cancelled = observation
            .as_ref()
            .map(|o| {
                PreparedTerminal::new(
                    &definition,
                    o,
                    ObservedResult::non_invoked(TerminalOutcome::Cancelled, "schedule_cancelled"),
                )
            })
            .transpose()?;
        let uncertain = observation
            .as_ref()
            .map(|o| {
                PreparedTerminal::new(
                    &definition,
                    o,
                    ObservedResult::non_invoked(
                        TerminalOutcome::Indeterminate,
                        "cancelled_after_launch_intent",
                    ),
                )
            })
            .transpose()?;
        self.commit_prepared(Box::new(move |writer| {
            Box::pin(async move {
                let Some(row) = writer
                    .query_row(series_get(snapshot.store_id, schedule))
                    .await?
                else {
                    return Ok(boxed(None::<CancellationCut>));
                };
                let mut series = decode_series(&row)?;
                if let Some(cut_at_us) = series.cancellation_cut_us {
                    return Ok(boxed(Some(CancellationCut {
                        changed: false,
                        cut_at_us,
                    })));
                }
                if series.revision != snapshot.revision {
                    return Ok(boxed(None::<CancellationCut>));
                }
                let now = writer_now(writer, snapshot.store_id).await?;
                let cut = OccurrenceInstant::from_micros(now).map_err(internal)?;
                series.cancellation_cut_us = Some(now);
                series.state = "cancelled".into();
                // Cancellation extends a captured window only to its finite exclusive cut.
                series.horizon = Some(cut);
                if series.next.is_some_and(|next| next >= cut) {
                    series.next = None;
                }
                if series.window_source.is_none() {
                    series.window_source = series.next;
                }
                if series.selection_at_us.is_none() {
                    series.selection_at_us = Some(now);
                }
                if series.effective_max_lateness_s.is_none() {
                    series.effective_max_lateness_s = Some(0);
                }
                if let (Some(o), Some(cancelled), Some(uncertain)) =
                    (&observation, &cancelled, &uncertain)
                {
                    let row = writer
                        .query_row(occurrence_query(
                            snapshot.store_id,
                            None,
                            schedule,
                            Some(snapshot.definition_revision),
                            None,
                            1,
                            Some(o.id),
                        ))
                        .await?
                        .ok_or_else(|| internal("cancelled occurrence disappeared"))?;
                    if row
                        .opt_uuid("terminal_receipt_id")
                        .map_err(internal)?
                        .is_none()
                    {
                        let invocation = row.opt_uuid("invocation_id").map_err(internal)?;
                        let terminal = if row
                            .opt_uuid("launch_intent_id")
                            .map_err(internal)?
                            .is_some()
                        {
                            uncertain
                        } else {
                            cancelled
                        };
                        insert_terminal(
                            writer,
                            o,
                            terminal,
                            invocation,
                            row.opt_i64("admission_at_us").map_err(internal)?,
                            now,
                        )
                        .await?;
                        if terminal.result.outcome == TerminalOutcome::Indeterminate {
                            series.suspended_invocation_id = invocation;
                        }
                    }
                }
                series.active_occurrence_id = None;
                series.active_invocation_id = None;
                series.selected = None;
                require_one(writer, series_cas(&series, series.revision)).await?;
                project_note(writer, &series, None, now).await?;
                Ok(boxed(Some(CancellationCut {
                    changed: true,
                    cut_at_us: now,
                })))
            })
        }))
        .await
    }

    /// Expiry before intent permits reclaim; expiry after intent records uncertainty without replay.
    pub async fn recover_expired(&self, work: &AuxiliaryWork) -> Result<Recovery, RuntimeError> {
        let Some(id) = work.series.active_occurrence_id else {
            return Ok(Recovery::None);
        };
        let Some(observation) = self.observation(work.series.schedule_id, id).await? else {
            return Ok(Recovery::None);
        };
        let definition = self.definition(&work.series).await?;
        let uncertain = PreparedTerminal::new(
            &definition,
            &observation,
            ObservedResult::non_invoked(TerminalOutcome::Indeterminate, "launch_intent_unsettled"),
        )?;
        let store = self.store_id;
        let schedule = work.series.schedule_id;
        self.commit_prepared(Box::new(move |writer| {
            Box::pin(async move {
                let Some(row) = writer.query_row(series_get(store, schedule)).await? else {
                    return Ok(boxed(Recovery::None));
                };
                let mut series = decode_series(&row)?;
                let Some(row) = writer
                    .query_row(occurrence_query(
                        store,
                        None,
                        schedule,
                        None,
                        None,
                        1,
                        Some(id),
                    ))
                    .await?
                else {
                    return Ok(boxed(Recovery::None));
                };
                let now = writer_now(writer, store).await?;
                if row
                    .opt_uuid("terminal_receipt_id")
                    .map_err(internal)?
                    .is_some()
                    || row
                        .opt_i64("lease_expires_at_us")
                        .map_err(internal)?
                        .is_none_or(|expiry| expiry > now)
                    || series.active_occurrence_id != Some(id)
                {
                    return Ok(boxed(Recovery::None));
                }
                if row
                    .opt_uuid("launch_intent_id")
                    .map_err(internal)?
                    .is_none()
                {
                    return Ok(boxed(Recovery::Reclaimable {
                        schedule_id: schedule,
                        occurrence_id: id,
                    }));
                }
                let invocation = row.uuid("invocation_id").map_err(internal)?;
                let decision = row.opt_i64("admission_at_us").map_err(internal)?;
                insert_terminal(
                    writer,
                    &observation,
                    &uncertain,
                    Some(invocation),
                    decision,
                    now,
                )
                .await?;
                series.active_occurrence_id = None;
                series.active_invocation_id = None;
                series.suspended_invocation_id = Some(invocation);
                if series.state != "cancelled" {
                    series.state = "suspended".into();
                }
                require_one(writer, series_cas(&series, series.revision)).await?;
                project_note(writer, &series, None, now).await?;
                Ok(boxed(Recovery::Settled(prepared_receipt(
                    &observation,
                    uncertain,
                    Some(invocation),
                    decision,
                    now,
                ))))
            })
        }))
        .await
    }

    /// Append an actual late response held by the original invocation's host. No second notice is created.
    pub async fn append_verified_reconciliation(
        &self,
        intent: &LaunchIntent,
        result: ObservedResult,
    ) -> Result<bool, RuntimeError> {
        if intent.claim.store_id != self.store_id {
            return Err(RuntimeError::InvalidInput(
                "schedule invocation store mismatch".into(),
            ));
        }
        let reconciliation_id = Uuid::new_v4();
        let evidence = serde_json::to_vec(&result.evidence)
            .map_err(|e| RuntimeError::Internal(e.to_string()))?;
        let intent = intent.clone();
        self.commit_prepared(Box::new(move |writer| {
            Box::pin(async move {
                let Some(series_row) = writer
                    .query_row(series_get(
                        intent.claim.store_id,
                        intent.claim.occurrence.key.schedule_id,
                    ))
                    .await?
                else {
                    return Ok(boxed(false));
                };
                let mut series = decode_series(&series_row)?;
                let Some(row) = writer
                    .query_row(occurrence_query(
                        intent.claim.store_id,
                        None,
                        series.schedule_id,
                        None,
                        None,
                        1,
                        Some(intent.claim.occurrence.id),
                    ))
                    .await?
                else {
                    return Ok(boxed(false));
                };
                if !same_claim(&row, &intent.claim)?
                    || row.opt_uuid("launch_intent_id").map_err(internal)? != Some(intent.intent_id)
                {
                    return Ok(boxed(false));
                }
                let Some(terminal) = writer
                    .query_row(receipt_query(
                        intent.claim.store_id,
                        None,
                        intent.claim.occurrence.id,
                        None,
                        1,
                        Some("terminal"),
                    ))
                    .await?
                else {
                    return Ok(boxed(false));
                };
                if terminal.text("outcome").map_err(internal)? != "indeterminate" {
                    return Ok(boxed(false));
                }
                let now = writer_now(writer, intent.claim.store_id).await?;
                require_one(
                    writer,
                    SqlStatement::new(
                        RECEIPT_INSERT,
                        vec![
                            text(reconciliation_id),
                            text(intent.claim.occurrence.id),
                            text(&series.namespace),
                            text("reconciliation"),
                            text(result.outcome.as_str()),
                            text(result.reason_code),
                            text(intent.claim.invocation_id),
                            opt_id(result.exec_receipt_id),
                            text(terminal.uuid("id").map_err(internal)?),
                            integer(now),
                            integer(intent.admission_at_us),
                            SqlValue::from_opt_i64(result.started_at_us),
                            SqlValue::from_opt_i64(result.completed_at_us),
                            SqlValue::from_opt_i64(result.exit_code),
                            SqlValue::from_opt_i64(result.exit_signal),
                            SqlValue::Blob(evidence),
                        ],
                    ),
                )
                .await?;
                if result.outcome != TerminalOutcome::Indeterminate
                    && series.suspended_invocation_id == Some(intent.claim.invocation_id)
                {
                    series.suspended_invocation_id = None;
                    if series.state == "suspended" {
                        series.state = if series.next.is_some() {
                            "active"
                        } else {
                            "exhausted"
                        }
                        .into();
                    }
                }
                require_one(writer, series_cas(&series, series.revision)).await?;
                project_note(writer, &series, None, now).await?;
                Ok(boxed(true))
            })
        }))
        .await
    }
}

#[cfg(test)]
struct ClockFixture {
    readings: std::collections::VecDeque<Result<i64, &'static str>>,
    repeat: Option<Result<i64, &'static str>>,
    count: usize,
}
#[cfg(test)]
fn clocks() -> &'static std::sync::Mutex<std::collections::HashMap<Uuid, ClockFixture>> {
    static CLOCKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<Uuid, ClockFixture>>,
    > = std::sync::OnceLock::new();
    CLOCKS.get_or_init(Default::default)
}
#[cfg(test)]
fn test_clock_read(store: Uuid) -> StorageResult<Option<StorageResult<i64>>> {
    let mut fixtures = clocks()
        .lock()
        .map_err(|_| internal("schedule fixture clock lock poisoned"))?;
    let Some(fixture) = fixtures.get_mut(&store) else {
        return Ok(None);
    };
    fixture.count += 1;
    let reading = fixture
        .readings
        .pop_front()
        .or(fixture.repeat)
        .unwrap_or(Err("fixture sequence exhausted"));
    Ok(Some(reading.map_err(|reason| {
        internal(format!("schedule clock unavailable: {reason}"))
    })))
}
#[cfg(test)]
pub(crate) struct TestClock {
    store: Uuid,
}
#[cfg(test)]
impl TestClock {
    pub(crate) fn fixed(store: Uuid, now: i64) -> Self {
        let guard = Self { store };
        let old = clocks().lock().expect("fixture clock lock").insert(
            store,
            ClockFixture {
                readings: Default::default(),
                repeat: Some(Ok(now)),
                count: 0,
            },
        );
        assert!(old.is_none(), "one fixture clock per store");
        guard
    }
    pub(crate) fn sequence(&self, readings: Vec<Result<i64, &'static str>>) {
        let mut clocks = clocks().lock().expect("fixture clock lock");
        let fixture = clocks.get_mut(&self.store).expect("live fixture clock");
        fixture.readings = readings.into();
        fixture.repeat = None;
        fixture.count = 0;
    }
    pub(crate) fn set(&self, now: i64) {
        let mut clocks = clocks().lock().expect("fixture clock lock");
        let fixture = clocks.get_mut(&self.store).expect("live fixture clock");
        fixture.readings.clear();
        fixture.repeat = Some(Ok(now));
        fixture.count = 0;
    }
    pub(crate) fn count(&self) -> usize {
        clocks()
            .lock()
            .expect("fixture clock lock")
            .get(&self.store)
            .expect("live fixture clock")
            .count
    }
}
#[cfg(test)]
impl Drop for TestClock {
    fn drop(&mut self) {
        if let Ok(mut clocks) = clocks().lock() {
            clocks.remove(&self.store);
        }
    }
}
