//! Durable ANN-consumer registration lifecycle and compaction.
//!
//! A registration starts at the closed `PENDING_WATERMARK` value and blocks
//! compaction while its first checkpoint is being built.  Publishing that
//! checkpoint atomically moves it to an active watermark `S >= 0` and removes
//! the pending timestamp.  A pending registration which never activates is
//! retired after [`PENDING_GRACE_US`] by the same write transaction which
//! compacts the log; returning consumers therefore observe registry loss and
//! must rebuild before serving.

use std::any::Any;

use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{AtomicUnitOp, SqlAccess, SqlReader, StorageError, StorageResult};

/// Closed watermark for a registered consumer which has not yet published its
/// first durable checkpoint.  It sorts below every active watermark and below
/// the `-1` authoritative-rebuild fence, so it always blocks compaction.
pub const PENDING_WATERMARK: i64 = -2;

/// Closed watermark used while an authoritative registry-loss rebuild is in
/// flight.  Unlike a pending registration, this state never expires.
pub const RECOVERING_WATERMARK: i64 = -1;

/// A first checkpoint gets one day to finish before its never-activated
/// registration is retired.  Active (`S >= 0`) and recovering (`-1`) rows are
/// never age-retired.
pub const PENDING_GRACE_US: i64 = 24 * 60 * 60 * 1_000_000;

/// Registry state authorized to publish a checkpoint watermark.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatermarkAuthority {
    /// A first full checkpoint may activate a pending row; a rebuild of an
    /// already-active consumer may also advance it.
    PendingOrActive,
    /// An incremental or ordinary registered checkpoint requires `S >= 0`.
    Active,
    /// An authoritative recovery checkpoint requires exactly `-1`.
    Recovering,
}

/// Scope of one compaction operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactionScope {
    /// Compact one namespace/model pair, including wildcard consumers in its
    /// protection minimum.
    Namespace(String),
    /// Compact every namespace for a model, with a correlated per-namespace
    /// minimum.
    Model,
}

/// Identity of one pending consumer retired by compaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetiredConsumer {
    /// Stable consumer identity.
    pub consumer: String,
    /// Registry namespace (`*` for a global consumer).
    pub namespace: String,
    /// Embedding model whose write log was protected.
    pub embedding_model: String,
    /// Original pending-registration timestamp in microseconds since epoch.
    pub registered_at_us: i64,
}

/// Observable result of one lifecycle-aware compaction transaction.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompactionOutcome {
    /// Raw write-log rows deleted after evaluating the post-retirement minimum.
    pub deleted_log_rows: u64,
    /// Pending consumers retired and warned during this transaction.
    pub retired_consumers: Vec<RetiredConsumer>,
}

fn consumer_stmt(
    sql: impl Into<String>,
    label: &str,
    consumer: &str,
    namespace: &str,
    model: &str,
    watermark: i64,
) -> SqlStatement {
    SqlStatement::new(
        sql,
        vec![
            SqlValue::Text(consumer.to_owned()),
            SqlValue::Text(namespace.to_owned()),
            SqlValue::Text(model.to_owned()),
            SqlValue::Integer(watermark),
        ],
    )
    .labelled(label)
}

fn integer_range_error(value: u64) -> StorageError {
    StorageError::Internal(format!(
        "ANN watermark {value} exceeds SQLite INTEGER range"
    ))
}

// Statement text which both a transactional registry operation and its
// single-statement (`pathless_*`) form run.  Each lives here once, so a change
// to the registry rule reaches every runtime.
const REGISTER_PENDING_SQL: &str =
    include_str!("../../sql/ann_consumer_watermark_register_pending.sql");

const MARK_RECOVERING_SQL: &str =
    include_str!("../../sql/ann_consumer_watermark_mark_recovering.sql");

/// The `UPDATE` which publishes watermark `?4` for the registry row named by
/// `?1`, `?2` and `?3`, guarded by the predicate `authority` grants.
fn raise_sql(authority: WatermarkAuthority) -> String {
    let predicate = match authority {
        WatermarkAuthority::PendingOrActive => {
            "(watermark = -2 OR (watermark >= 0 AND watermark <= ?4))"
        }
        WatermarkAuthority::Active => "watermark >= 0 AND watermark <= ?4",
        WatermarkAuthority::Recovering => "watermark = -1",
    };
    format!(
        "UPDATE ann_consumer_watermark SET watermark = ?4 \
         WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3 \
           AND {predicate}"
    )
}

/// Statement form of [`register_pending`] for a runtime with no durable
/// consumer lifecycle: one closed-fence insert, with no pending-age metadata.
///
/// `label_prefix` is prepended to the statement label, so each pack keeps its
/// own operator-visible statement names.
pub fn pathless_register_pending(
    label_prefix: &str,
    consumer: &str,
    namespace: &str,
    model: &str,
) -> SqlStatement {
    consumer_stmt(
        REGISTER_PENDING_SQL,
        &format!("{label_prefix}ann_register_pathless_consumer"),
        consumer,
        namespace,
        model,
        PENDING_WATERMARK,
    )
}

/// Statement form of [`mark_recovering`] for a runtime with no durable
/// consumer lifecycle: the recovery-fence upsert alone, with no pending
/// metadata to clear.
pub fn pathless_mark_recovering(
    label_prefix: &str,
    consumer: &str,
    namespace: &str,
    model: &str,
) -> SqlStatement {
    consumer_stmt(
        MARK_RECOVERING_SQL,
        &format!("{label_prefix}ann_mark_pathless_recovering"),
        consumer,
        namespace,
        model,
        RECOVERING_WATERMARK,
    )
}

/// Statement form of [`raise_watermark`] for a runtime with no durable
/// consumer lifecycle: the guarded `UPDATE` alone, with no pending metadata to
/// clear.  `watermark` is already a SQLite integer; the caller owns the range
/// check and its error text.  A run which affects no row means the fence
/// rejected `authority`.
pub fn pathless_raise_watermark(
    label_prefix: &str,
    consumer: &str,
    namespace: &str,
    model: &str,
    watermark: i64,
    authority: WatermarkAuthority,
) -> SqlStatement {
    consumer_stmt(
        raise_sql(authority),
        &format!("{label_prefix}ann_raise_pathless_watermark"),
        consumer,
        namespace,
        model,
        watermark,
    )
}

/// Statement form of [`compact_write_log`] for a runtime with no durable
/// consumer lifecycle: the log delete bounded by the registry minimum, with no
/// dormant registrations to retire.
pub fn pathless_compact_log(
    label_prefix: &str,
    scope: CompactionScope,
    model: &str,
) -> SqlStatement {
    let label = format!("{label_prefix}ann_compact_pathless_log");
    let model = model.to_owned();
    match scope {
        CompactionScope::Namespace(namespace) => SqlStatement::new(
            include_str!("../../sql/ann_write_log_compact_namespace_pathless.sql"),
            vec![SqlValue::Text(namespace), SqlValue::Text(model)],
        )
        .labelled(label),
        CompactionScope::Model => SqlStatement::new(
            include_str!("../../sql/ann_write_log_compact_model.sql"),
            vec![SqlValue::Text(model)],
        )
        .labelled(label),
    }
}

/// Register with one statement when pathless, otherwise retain the durable
/// pending-registration transaction and timestamp.
#[doc(hidden)]
pub async fn register_pending_dispatch(
    sql: &dyn SqlAccess,
    label_prefix: &str,
    consumer: &str,
    namespace: &str,
    model: &str,
    pathless: bool,
) -> StorageResult<()> {
    if pathless {
        let mut writer = sql.writer().await?;
        writer
            .execute(pathless_register_pending(
                label_prefix,
                consumer,
                namespace,
                model,
            ))
            .await?;
        Ok(())
    } else {
        register_pending(sql, consumer, namespace, model).await
    }
}

/// Publish through the pathless statement or the durable activation transaction.
/// Callers retain any pack-specific range diagnostic before entering this helper.
#[doc(hidden)]
// The existing registry identity and statement label stay explicit at both consumers.
#[allow(clippy::too_many_arguments)]
pub async fn raise_watermark_dispatch(
    sql: &dyn SqlAccess,
    label_prefix: &str,
    consumer: &str,
    namespace: &str,
    model: &str,
    watermark: u64,
    authority: WatermarkAuthority,
    pathless: bool,
) -> StorageResult<bool> {
    if pathless {
        let watermark = i64::try_from(watermark).map_err(|_| integer_range_error(watermark))?;
        let mut writer = sql.writer().await?;
        Ok(writer
            .execute(pathless_raise_watermark(
                label_prefix,
                consumer,
                namespace,
                model,
                watermark,
                authority,
            ))
            .await?
            == 1)
    } else {
        raise_watermark(sql, consumer, namespace, model, watermark, authority).await
    }
}

/// Compact with one statement when pathless, otherwise retain atomic pending
/// retirement, metadata cleanup and warning publication.
#[doc(hidden)]
pub async fn compact_dispatch(
    sql: &dyn SqlAccess,
    label_prefix: &str,
    scope: CompactionScope,
    model: &str,
    pathless: bool,
) -> StorageResult<()> {
    if pathless {
        let mut writer = sql.writer().await?;
        writer
            .execute(pathless_compact_log(label_prefix, scope, model))
            .await?;
        Ok(())
    } else {
        compact_write_log(sql, scope, model).await.map(|_| ())
    }
}

/// Statement which reads one consumer's registered watermark.
pub fn read_watermark_statement(
    label_prefix: &str,
    consumer: &str,
    namespace: &str,
    model: &str,
) -> SqlStatement {
    SqlStatement::new(
        include_str!("../../sql/ann_consumer_watermark_select.sql"),
        vec![
            SqlValue::Text(consumer.to_owned()),
            SqlValue::Text(namespace.to_owned()),
            SqlValue::Text(model.to_owned()),
        ],
    )
    .labelled(format!("{label_prefix}ann_read_own_watermark"))
}

/// SQL SELECT for a wildcard-inclusive minimum using caller-selected bind positions.
/// Callers supply trusted, positive numbered positions and bind namespace/model values.
#[doc(hidden)]
pub fn min_watermark_subquery(namespace_param: usize, model_param: usize) -> String {
    format!(
        "SELECT MIN(watermark) FROM ann_consumer_watermark \
         WHERE (namespace = ?{namespace_param} OR namespace = '*') \
           AND embedding_model = ?{model_param}"
    )
}

/// Statement reading the exact namespace/model minimum, including wildcard consumers.
pub fn min_watermark_statement(label_prefix: &str, namespace: &str, model: &str) -> SqlStatement {
    let minimum = min_watermark_subquery(1, 2);
    SqlStatement::new(
        format!("SELECT ({minimum}) AS m"),
        vec![
            SqlValue::Text(namespace.to_owned()),
            SqlValue::Text(model.to_owned()),
        ],
    )
    .labelled(format!("{label_prefix}ann_registry_min"))
}

async fn read_integer_on(
    reader: &mut dyn SqlReader,
    statement: SqlStatement,
    column: &str,
) -> StorageResult<Option<i64>> {
    let rows = reader.query_all(statement).await?;
    Ok(rows
        .into_iter()
        .next()
        .and_then(|row| row.i64_or_none(column)))
}

/// Read one consumer using the caller's existing reader and complete statement label.
/// No connection is acquired. Only an Integer in the first row is returned; negative
/// watermarks are preserved and absent, NULL or other-shaped values return `None`.
pub async fn read_watermark_on(
    reader: &mut dyn SqlReader,
    label: &str,
    consumer: &str,
    namespace: &str,
    model: &str,
) -> StorageResult<Option<i64>> {
    let statement = read_watermark_statement("", consumer, namespace, model).labelled(label);
    read_integer_on(reader, statement, "watermark").await
}

/// Read the wildcard-inclusive minimum using the caller's existing reader.
/// The label prefix is followed by `ann_registry_min`. Decoding is as permissive
/// as [`read_watermark_on`], including retention of negative closed watermarks.
pub async fn min_watermark_on(
    reader: &mut dyn SqlReader,
    label_prefix: &str,
    namespace: &str,
    model: &str,
) -> StorageResult<Option<i64>> {
    read_integer_on(
        reader,
        min_watermark_statement(label_prefix, namespace, model),
        "m",
    )
    .await
}

/// Read one consumer's registered watermark.  `None` means the registry holds
/// no row for that consumer, namespace and model.
pub async fn read_watermark(
    sql: &dyn SqlAccess,
    label_prefix: &str,
    consumer: &str,
    namespace: &str,
    model: &str,
) -> StorageResult<Option<i64>> {
    let statement = read_watermark_statement(label_prefix, consumer, namespace, model);
    let mut reader = sql.reader().await?;
    read_integer_on(reader.as_mut(), statement, "watermark").await
}

/// Register a consumer in the pending state without changing an existing
/// active or recovering registration.
pub async fn register_pending(
    sql: &dyn SqlAccess,
    consumer: &str,
    namespace: &str,
    model: &str,
) -> StorageResult<()> {
    register_pending_at(
        sql,
        consumer,
        namespace,
        model,
        chrono::Utc::now().timestamp_micros(),
    )
    .await
}

async fn register_pending_at(
    sql: &dyn SqlAccess,
    consumer: &str,
    namespace: &str,
    model: &str,
    registered_at_us: i64,
) -> StorageResult<()> {
    let consumer = consumer.to_owned();
    let namespace = namespace.to_owned();
    let model = model.to_owned();
    let op: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let inserted = writer
                .execute(consumer_stmt(
                    REGISTER_PENDING_SQL,
                    "ann_registry_register_pending",
                    &consumer,
                    &namespace,
                    &model,
                    PENDING_WATERMARK,
                ))
                .await?;
            let pending_insert = if inserted == 1 {
                // An orphan metadata row can exist only after manual surgery,
                // but it must not make a genuinely new registration inherit
                // an already-expired timestamp.
                include_str!("../../sql/ann_consumer_pending_upsert.sql")
            } else {
                // Repeated calls while the same first build is in flight must
                // not refresh its grace window indefinitely.
                include_str!("../../sql/ann_consumer_pending_insert_if_pending.sql")
            };
            let mut pending_params = vec![
                SqlValue::Text(consumer),
                SqlValue::Text(namespace),
                SqlValue::Text(model),
                SqlValue::Integer(registered_at_us),
            ];
            if inserted != 1 {
                pending_params.push(SqlValue::Integer(PENDING_WATERMARK));
            }
            writer
                .execute(
                    SqlStatement::new(pending_insert, pending_params)
                        .labelled("ann_registry_stamp_pending"),
                )
                .await?;
            Ok(Box::new(()) as Box<dyn Any + Send>)
        })
    });
    sql.atomic_unit(op).await?;
    Ok(())
}

/// Publish the non-expiring authoritative-rebuild fence and clear any pending
/// lifecycle timestamp in the same transaction.
pub async fn mark_recovering(
    sql: &dyn SqlAccess,
    consumer: &str,
    namespace: &str,
    model: &str,
) -> StorageResult<()> {
    let warning_consumer = consumer.to_owned();
    let warning_namespace = namespace.to_owned();
    let warning_model = model.to_owned();
    let consumer = consumer.to_owned();
    let namespace = namespace.to_owned();
    let model = model.to_owned();
    let op: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            writer
                .execute(consumer_stmt(
                    MARK_RECOVERING_SQL,
                    "ann_registry_mark_recovering",
                    &consumer,
                    &namespace,
                    &model,
                    RECOVERING_WATERMARK,
                ))
                .await?;
            writer
                .execute(
                    SqlStatement::new(
                        include_str!("../../sql/ann_consumer_pending_delete.sql"),
                        vec![
                            SqlValue::Text(consumer),
                            SqlValue::Text(namespace),
                            SqlValue::Text(model),
                        ],
                    )
                    .labelled("ann_registry_clear_pending_for_recovery"),
                )
                .await?;
            Ok(Box::new(()) as Box<dyn Any + Send>)
        })
    });
    sql.atomic_unit(op).await?;
    tracing::warn!(
        consumer = %warning_consumer,
        namespace = %warning_namespace,
        embedding_model = %warning_model,
        "ANN consumer entered the non-expiring recovery fence; compaction remains blocked \
         until an authoritative full checkpoint succeeds"
    );
    Ok(())
}

/// Conditionally publish a durable checkpoint and activate the consumer.
///
/// Returns `false` when retirement or a competing checkpoint changed the
/// registry state first.  The conditional update and pending-metadata cleanup
/// share one writer transaction, so compaction cannot observe an active
/// watermark with stale pending metadata or retire between those steps.
pub async fn raise_watermark(
    sql: &dyn SqlAccess,
    consumer: &str,
    namespace: &str,
    model: &str,
    watermark: u64,
    authority: WatermarkAuthority,
) -> StorageResult<bool> {
    let watermark = i64::try_from(watermark).map_err(|_| integer_range_error(watermark))?;
    let consumer = consumer.to_owned();
    let namespace = namespace.to_owned();
    let model = model.to_owned();
    let op: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let affected = writer
                .execute(consumer_stmt(
                    raise_sql(authority),
                    "ann_registry_raise_watermark",
                    &consumer,
                    &namespace,
                    &model,
                    watermark,
                ))
                .await?;
            if affected == 1 {
                writer
                    .execute(
                        SqlStatement::new(
                            include_str!("../../sql/ann_consumer_pending_delete.sql"),
                            vec![
                                SqlValue::Text(consumer),
                                SqlValue::Text(namespace),
                                SqlValue::Text(model),
                            ],
                        )
                        .labelled("ann_registry_activate_consumer"),
                    )
                    .await?;
            }
            Ok(Box::new(affected == 1) as Box<dyn Any + Send>)
        })
    });
    let result = sql.atomic_unit(op).await?;
    Ok(*result
        .downcast::<bool>()
        .expect("ANN watermark atomic unit returns bool"))
}

/// Retire expired never-activated consumers and compact the write log in one
/// writer-serialized transaction.
pub async fn compact_write_log(
    sql: &dyn SqlAccess,
    scope: CompactionScope,
    model: &str,
) -> StorageResult<CompactionOutcome> {
    compact_write_log_at(sql, scope, model, chrono::Utc::now().timestamp_micros()).await
}

async fn compact_write_log_at(
    sql: &dyn SqlAccess,
    scope: CompactionScope,
    model: &str,
    now_us: i64,
) -> StorageResult<CompactionOutcome> {
    let cutoff_us = now_us.saturating_sub(PENDING_GRACE_US);
    let model = model.to_owned();
    let op: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let (scope_filter, scope_params) = match &scope {
                CompactionScope::Namespace(namespace) => (
                    " AND (watermark.namespace = ?3 OR watermark.namespace = '*')",
                    vec![
                        SqlValue::Integer(cutoff_us),
                        SqlValue::Text(model.clone()),
                        SqlValue::Text(namespace.clone()),
                    ],
                ),
                CompactionScope::Model => (
                    "",
                    vec![SqlValue::Integer(cutoff_us), SqlValue::Text(model.clone())],
                ),
            };

            // Repair a pending row created by an interrupted/manual writer
            // without lifecycle metadata.  It starts a fresh grace interval
            // instead of pinning silently forever or being retired without an
            // age proof.
            let backfill_sql = format!(
                "INSERT OR IGNORE INTO ann_consumer_pending \
                 (consumer, namespace, embedding_model, registered_at_us) \
                 SELECT watermark.consumer, watermark.namespace, \
                        watermark.embedding_model, ?1 \
                 FROM ann_consumer_watermark watermark \
                 WHERE watermark.watermark = -2 AND watermark.embedding_model = ?2{scope_filter}"
            );
            let mut backfill_params = scope_params.clone();
            backfill_params[0] = SqlValue::Integer(now_us);
            writer
                .execute(
                    SqlStatement::new(backfill_sql, backfill_params)
                        .labelled("ann_registry_backfill_pending_timestamp"),
                )
                .await?;

            let retired_rows = writer
                .query_all(
                    SqlStatement::new(
                        format!(
                            "SELECT watermark.consumer, watermark.namespace, \
                                watermark.embedding_model, pending.registered_at_us \
                         FROM ann_consumer_watermark watermark \
                         JOIN ann_consumer_pending pending \
                           ON pending.consumer = watermark.consumer \
                          AND pending.namespace = watermark.namespace \
                          AND pending.embedding_model = watermark.embedding_model \
                         WHERE pending.registered_at_us <= ?1 \
                           AND watermark.watermark = -2 \
                           AND watermark.embedding_model = ?2{scope_filter} \
                         ORDER BY watermark.consumer, watermark.namespace"
                        ),
                        scope_params.clone(),
                    )
                    .labelled("ann_registry_find_expired_pending"),
                )
                .await?;

            writer
                .execute(
                    SqlStatement::new(
                        format!(
                            "DELETE FROM ann_consumer_watermark AS watermark \
                         WHERE watermark.watermark = -2 \
                           AND watermark.embedding_model = ?2{scope_filter} \
                           AND EXISTS (SELECT 1 FROM ann_consumer_pending pending \
                                       WHERE pending.consumer = watermark.consumer \
                                         AND pending.namespace = watermark.namespace \
                                         AND pending.embedding_model = watermark.embedding_model \
                                         AND pending.registered_at_us <= ?1)"
                        ),
                        scope_params.clone(),
                    )
                    .labelled("ann_registry_retire_expired_pending"),
                )
                .await?;

            // Drop metadata for retired rows and for consumers activated by a
            // legacy/manual checkpoint writer.  The watermark row is the
            // authoritative state.
            let pending_scope_filter = scope_filter.replace("watermark.", "pending.");
            writer
                .execute(SqlStatement::new(
                    format!(
                        "DELETE FROM ann_consumer_pending AS pending \
                         WHERE pending.embedding_model = ?2{pending_scope_filter} \
                           AND NOT EXISTS (SELECT 1 FROM ann_consumer_watermark watermark \
                                           WHERE watermark.consumer = pending.consumer \
                                             AND watermark.namespace = pending.namespace \
                                             AND watermark.embedding_model = pending.embedding_model \
                                             AND watermark.watermark = -2)"
                    ),
                    scope_params,
                ).labelled("ann_registry_prune_pending_metadata"))
                .await?;

            let deleted_log_rows = match &scope {
                CompactionScope::Namespace(namespace) => {
                    writer
                        .execute(
                            SqlStatement::new(
                                include_str!("../../sql/ann_write_log_compact_namespace.sql"),
                                vec![
                                    SqlValue::Text(namespace.clone()),
                                    SqlValue::Text(model.clone()),
                                ],
                            )
                            .labelled("ann_registry_compact_namespace"),
                        )
                        .await?
                }
                CompactionScope::Model => {
                    writer
                        .execute(
                            SqlStatement::new(
                                include_str!("../../sql/ann_write_log_compact_model.sql"),
                                vec![SqlValue::Text(model.clone())],
                            )
                            .labelled("ann_registry_compact_model"),
                        )
                        .await?
                }
            };

            let mut retired_consumers = Vec::with_capacity(retired_rows.len());
            for row in retired_rows {
                let text = |column: &str| match row.get(column) {
                    Some(SqlValue::Text(value)) => Ok(value.clone()),
                    other => Err(StorageError::Internal(format!(
                        "ANN retired-consumer {column}: unexpected {other:?}"
                    ))),
                };
                let registered_at_us = match row.get("registered_at_us") {
                    Some(SqlValue::Integer(value)) => *value,
                    other => {
                        return Err(StorageError::Internal(format!(
                            "ANN retired-consumer registered_at_us: unexpected {other:?}"
                        )))
                    }
                };
                retired_consumers.push(RetiredConsumer {
                    consumer: text("consumer")?,
                    namespace: text("namespace")?,
                    embedding_model: text("embedding_model")?,
                    registered_at_us,
                });
            }
            Ok(Box::new(CompactionOutcome {
                deleted_log_rows,
                retired_consumers,
            }) as Box<dyn Any + Send>)
        })
    });

    let result = sql.atomic_unit(op).await?;
    let outcome = *result
        .downcast::<CompactionOutcome>()
        .expect("ANN compaction atomic unit returns CompactionOutcome");
    for retired in &outcome.retired_consumers {
        tracing::warn!(
            consumer = %retired.consumer,
            namespace = %retired.namespace,
            embedding_model = %retired.embedding_model,
            registered_at_us = retired.registered_at_us,
            grace_hours = PENDING_GRACE_US / (60 * 60 * 1_000_000),
            "retired dormant ANN consumer which never published a checkpoint; \
             a returning consumer must rebuild before serving"
        );
    }
    Ok(outcome)
}

#[cfg(test)]
#[path = "registry/tests.rs"]
mod tests;
