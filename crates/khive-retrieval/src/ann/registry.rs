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
use khive_storage::{AtomicUnitOp, SqlAccess, StorageError, StorageResult};

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

fn stmt(sql: impl Into<String>, params: Vec<SqlValue>, label: &str) -> SqlStatement {
    SqlStatement {
        sql: sql.into(),
        params,
        label: Some(label.to_owned()),
    }
}

fn integer_range_error(value: u64) -> StorageError {
    StorageError::Internal(format!(
        "ANN watermark {value} exceeds SQLite INTEGER range"
    ))
}

// Statement text which both a transactional registry operation and its
// single-statement (`pathless_*`) form run.  Each lives here once, so a change
// to the registry rule reaches every runtime.
const REGISTER_PENDING_SQL: &str = "INSERT OR IGNORE INTO ann_consumer_watermark \
     (consumer, namespace, embedding_model, watermark) \
     VALUES (?1, ?2, ?3, ?4)";

const MARK_RECOVERING_SQL: &str = "INSERT INTO ann_consumer_watermark \
     (consumer, namespace, embedding_model, watermark) \
     VALUES (?1, ?2, ?3, ?4) \
     ON CONFLICT(consumer, namespace, embedding_model) \
     DO UPDATE SET watermark = excluded.watermark";

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
    stmt(
        REGISTER_PENDING_SQL,
        vec![
            SqlValue::Text(consumer.to_owned()),
            SqlValue::Text(namespace.to_owned()),
            SqlValue::Text(model.to_owned()),
            SqlValue::Integer(PENDING_WATERMARK),
        ],
        &format!("{label_prefix}ann_register_pathless_consumer"),
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
    stmt(
        MARK_RECOVERING_SQL,
        vec![
            SqlValue::Text(consumer.to_owned()),
            SqlValue::Text(namespace.to_owned()),
            SqlValue::Text(model.to_owned()),
            SqlValue::Integer(RECOVERING_WATERMARK),
        ],
        &format!("{label_prefix}ann_mark_pathless_recovering"),
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
    stmt(
        raise_sql(authority),
        vec![
            SqlValue::Text(consumer.to_owned()),
            SqlValue::Text(namespace.to_owned()),
            SqlValue::Text(model.to_owned()),
            SqlValue::Integer(watermark),
        ],
        &format!("{label_prefix}ann_raise_pathless_watermark"),
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
        CompactionScope::Namespace(namespace) => stmt(
            "DELETE FROM ann_write_log \
             WHERE namespace = ?1 AND embedding_model = ?2 \
               AND seq <= (SELECT MIN(watermark.watermark) \
                           FROM ann_consumer_watermark watermark \
                           WHERE (watermark.namespace = ?1 \
                                  OR watermark.namespace = '*') \
                             AND watermark.embedding_model = ?2)",
            vec![SqlValue::Text(namespace), SqlValue::Text(model)],
            &label,
        ),
        CompactionScope::Model => stmt(
            "DELETE FROM ann_write_log \
             WHERE embedding_model = ?1 \
               AND seq <= (SELECT MIN(watermark.watermark) \
                           FROM ann_consumer_watermark watermark \
                           WHERE (watermark.namespace = ann_write_log.namespace \
                                  OR watermark.namespace = '*') \
                             AND watermark.embedding_model = ?1)",
            vec![SqlValue::Text(model)],
            &label,
        ),
    }
}

/// Statement which reads one consumer's registered watermark.
pub fn read_watermark_statement(
    label_prefix: &str,
    consumer: &str,
    namespace: &str,
    model: &str,
) -> SqlStatement {
    stmt(
        "SELECT watermark FROM ann_consumer_watermark \
         WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3",
        vec![
            SqlValue::Text(consumer.to_owned()),
            SqlValue::Text(namespace.to_owned()),
            SqlValue::Text(model.to_owned()),
        ],
        &format!("{label_prefix}ann_read_own_watermark"),
    )
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
    let rows = reader.query_all(statement).await?;
    Ok(rows
        .into_iter()
        .next()
        .and_then(|row| match row.get("watermark") {
            Some(SqlValue::Integer(n)) => Some(*n),
            _ => None,
        }))
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
                .execute(stmt(
                    REGISTER_PENDING_SQL,
                    vec![
                        SqlValue::Text(consumer.clone()),
                        SqlValue::Text(namespace.clone()),
                        SqlValue::Text(model.clone()),
                        SqlValue::Integer(PENDING_WATERMARK),
                    ],
                    "ann_registry_register_pending",
                ))
                .await?;
            let pending_insert = if inserted == 1 {
                // An orphan metadata row can exist only after manual surgery,
                // but it must not make a genuinely new registration inherit
                // an already-expired timestamp.
                "INSERT INTO ann_consumer_pending \
                 (consumer, namespace, embedding_model, registered_at_us) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(consumer, namespace, embedding_model) \
                 DO UPDATE SET registered_at_us = excluded.registered_at_us"
            } else {
                // Repeated calls while the same first build is in flight must
                // not refresh its grace window indefinitely.
                "INSERT OR IGNORE INTO ann_consumer_pending \
                 (consumer, namespace, embedding_model, registered_at_us) \
                 SELECT ?1, ?2, ?3, ?4 \
                 WHERE EXISTS (SELECT 1 FROM ann_consumer_watermark \
                               WHERE consumer = ?1 AND namespace = ?2 \
                                 AND embedding_model = ?3 AND watermark = ?5)"
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
                .execute(stmt(
                    pending_insert,
                    pending_params,
                    "ann_registry_stamp_pending",
                ))
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
                .execute(stmt(
                    MARK_RECOVERING_SQL,
                    vec![
                        SqlValue::Text(consumer.clone()),
                        SqlValue::Text(namespace.clone()),
                        SqlValue::Text(model.clone()),
                        SqlValue::Integer(RECOVERING_WATERMARK),
                    ],
                    "ann_registry_mark_recovering",
                ))
                .await?;
            writer
                .execute(stmt(
                    "DELETE FROM ann_consumer_pending \
                     WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3",
                    vec![
                        SqlValue::Text(consumer),
                        SqlValue::Text(namespace),
                        SqlValue::Text(model),
                    ],
                    "ann_registry_clear_pending_for_recovery",
                ))
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
                .execute(stmt(
                    raise_sql(authority),
                    vec![
                        SqlValue::Text(consumer.clone()),
                        SqlValue::Text(namespace.clone()),
                        SqlValue::Text(model.clone()),
                        SqlValue::Integer(watermark),
                    ],
                    "ann_registry_raise_watermark",
                ))
                .await?;
            if affected == 1 {
                writer
                    .execute(stmt(
                        "DELETE FROM ann_consumer_pending \
                         WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3",
                        vec![
                            SqlValue::Text(consumer),
                            SqlValue::Text(namespace),
                            SqlValue::Text(model),
                        ],
                        "ann_registry_activate_consumer",
                    ))
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
                .execute(stmt(
                    backfill_sql,
                    backfill_params,
                    "ann_registry_backfill_pending_timestamp",
                ))
                .await?;

            let retired_rows = writer
                .query_all(stmt(
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
                    "ann_registry_find_expired_pending",
                ))
                .await?;

            writer
                .execute(stmt(
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
                    "ann_registry_retire_expired_pending",
                ))
                .await?;

            // Drop metadata for retired rows and for consumers activated by a
            // legacy/manual checkpoint writer.  The watermark row is the
            // authoritative state.
            let pending_scope_filter = scope_filter.replace("watermark.", "pending.");
            writer
                .execute(stmt(
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
                    "ann_registry_prune_pending_metadata",
                ))
                .await?;

            let deleted_log_rows = match &scope {
                CompactionScope::Namespace(namespace) => {
                    writer
                        .execute(stmt(
                            "DELETE FROM ann_write_log \
                             WHERE namespace = ?1 AND embedding_model = ?2 \
                               AND seq <= (SELECT MIN(watermark) \
                                           FROM ann_consumer_watermark \
                                           WHERE (namespace = ?1 OR namespace = '*') \
                                             AND embedding_model = ?2)",
                            vec![
                                SqlValue::Text(namespace.clone()),
                                SqlValue::Text(model.clone()),
                            ],
                            "ann_registry_compact_namespace",
                        ))
                        .await?
                }
                CompactionScope::Model => {
                    writer
                        .execute(stmt(
                            "DELETE FROM ann_write_log \
                             WHERE embedding_model = ?1 \
                               AND seq <= (SELECT MIN(watermark.watermark) \
                                           FROM ann_consumer_watermark watermark \
                                           WHERE (watermark.namespace = ann_write_log.namespace \
                                                  OR watermark.namespace = '*') \
                                             AND watermark.embedding_model = ?1)",
                            vec![SqlValue::Text(model.clone())],
                            "ann_registry_compact_model",
                        ))
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
mod tests {
    use super::*;
    use khive_db::StorageBackend;

    fn memory_backend() -> StorageBackend {
        let backend = StorageBackend::memory().expect("memory backend");
        backend.prepare_core_schema().expect("core schema");
        backend
    }

    async fn execute(sql: &dyn SqlAccess, sql_text: &str, params: Vec<SqlValue>) {
        sql.writer()
            .await
            .expect("writer")
            .execute(stmt(sql_text, params, "ann_registry_test"))
            .await
            .expect("execute");
    }

    async fn scalar_i64(sql: &dyn SqlAccess, sql_text: &str, params: Vec<SqlValue>) -> i64 {
        match sql
            .reader()
            .await
            .expect("reader")
            .query_scalar(stmt(sql_text, params, "ann_registry_test_scalar"))
            .await
            .expect("query scalar")
        {
            Some(SqlValue::Integer(value)) => value,
            other => panic!("unexpected scalar: {other:?}"),
        }
    }

    #[tokio::test]
    async fn expired_pending_consumer_retires_without_weakening_active_minimum() {
        let backend = memory_backend();
        let sql = backend.sql();
        let model = "ann-registry-expiry";
        register_pending_at(sql.as_ref(), "dormant", "local", model, 1)
            .await
            .expect("register dormant");
        register_pending_at(sql.as_ref(), "active", "local", model, 1)
            .await
            .expect("register active");
        assert!(raise_watermark(
            sql.as_ref(),
            "active",
            "local",
            model,
            2,
            WatermarkAuthority::PendingOrActive,
        )
        .await
        .expect("activate"));
        for seq in 1..=3 {
            execute(
                sql.as_ref(),
                "INSERT INTO ann_write_log \
                 (seq, namespace, embedding_model, kind, field, subject_id, op) \
                 VALUES (?1, 'local', ?2, 'note', 'note.content', ?3, 'upsert')",
                vec![
                    SqlValue::Integer(seq),
                    SqlValue::Text(model.into()),
                    SqlValue::Text(format!("subject-{seq}")),
                ],
            )
            .await;
        }

        let outcome = compact_write_log_at(
            sql.as_ref(),
            CompactionScope::Namespace("local".into()),
            model,
            PENDING_GRACE_US + 2,
        )
        .await
        .expect("compact");
        assert_eq!(outcome.retired_consumers.len(), 1);
        assert_eq!(outcome.retired_consumers[0].consumer, "dormant");
        assert_eq!(outcome.deleted_log_rows, 2);
        assert_eq!(
            scalar_i64(
                sql.as_ref(),
                "SELECT COUNT(*) FROM ann_write_log WHERE embedding_model = ?1",
                vec![SqlValue::Text(model.into())],
            )
            .await,
            1
        );
        assert_eq!(
            scalar_i64(
                sql.as_ref(),
                "SELECT COUNT(*) FROM ann_consumer_watermark \
                 WHERE consumer = 'active' AND embedding_model = ?1 AND watermark = 2",
                vec![SqlValue::Text(model.into())],
            )
            .await,
            1
        );
    }

    #[tokio::test]
    async fn retirement_wins_before_delayed_checkpoint_publication() {
        let backend = memory_backend();
        let sql = backend.sql();
        let model = "ann-registry-retire-race";
        register_pending_at(sql.as_ref(), "dormant", "local", model, 1)
            .await
            .expect("register");
        let outcome = compact_write_log_at(
            sql.as_ref(),
            CompactionScope::Namespace("local".into()),
            model,
            PENDING_GRACE_US + 2,
        )
        .await
        .expect("compact");
        assert_eq!(outcome.retired_consumers.len(), 1);
        assert!(!raise_watermark(
            sql.as_ref(),
            "dormant",
            "local",
            model,
            7,
            WatermarkAuthority::PendingOrActive,
        )
        .await
        .expect("delayed raise"));
    }

    #[tokio::test]
    async fn pending_row_just_inside_grace_survives_and_still_pins_the_log() {
        let backend = memory_backend();
        let sql = backend.sql();
        let model = "ann-registry-grace-inside";
        // cutoff = now - PENDING_GRACE_US = 2; registered_at 3 is one
        // microsecond inside the grace window.
        register_pending_at(sql.as_ref(), "fresh", "local", model, 3)
            .await
            .expect("register");
        register_pending_at(sql.as_ref(), "peer", "local", model, 3)
            .await
            .expect("register peer");
        assert!(raise_watermark(
            sql.as_ref(),
            "peer",
            "local",
            model,
            5,
            WatermarkAuthority::PendingOrActive,
        )
        .await
        .expect("seed active peer"));
        execute(
            sql.as_ref(),
            "INSERT INTO ann_write_log \
             (seq, namespace, embedding_model, kind, field, subject_id, op) \
             VALUES (1, 'local', ?1, 'note', 'note.content', 'subject-1', 'upsert')",
            vec![SqlValue::Text(model.into())],
        )
        .await;

        let outcome = compact_write_log_at(
            sql.as_ref(),
            CompactionScope::Namespace("local".into()),
            model,
            PENDING_GRACE_US + 2,
        )
        .await
        .expect("compact");
        assert!(
            outcome.retired_consumers.is_empty(),
            "a row inside the grace window must not retire: {:?}",
            outcome.retired_consumers
        );
        // The surviving pending row (-2) keeps the registry minimum below
        // every log seq, so nothing compacts away underneath the live build.
        assert_eq!(outcome.deleted_log_rows, 0);
        assert!(
            raise_watermark(
                sql.as_ref(),
                "fresh",
                "local",
                model,
                1,
                WatermarkAuthority::PendingOrActive,
            )
            .await
            .expect("in-grace consumer must still be able to publish"),
            "the surviving registration must accept its checkpoint"
        );
    }

    #[tokio::test]
    async fn pending_row_at_exact_grace_boundary_retires() {
        let backend = memory_backend();
        let sql = backend.sql();
        let model = "ann-registry-grace-boundary";
        // cutoff = now - PENDING_GRACE_US = 2; registered_at 2 sits exactly
        // on the boundary, which the <= comparator retires.
        register_pending_at(sql.as_ref(), "boundary", "local", model, 2)
            .await
            .expect("register");
        let outcome = compact_write_log_at(
            sql.as_ref(),
            CompactionScope::Namespace("local".into()),
            model,
            PENDING_GRACE_US + 2,
        )
        .await
        .expect("compact");
        assert_eq!(outcome.retired_consumers.len(), 1);
        assert_eq!(outcome.retired_consumers[0].consumer, "boundary");
        assert_eq!(outcome.retired_consumers[0].registered_at_us, 2);
        assert!(!raise_watermark(
            sql.as_ref(),
            "boundary",
            "local",
            model,
            4,
            WatermarkAuthority::PendingOrActive,
        )
        .await
        .expect("raise after boundary retirement"));
    }

    #[tokio::test]
    async fn checkpoint_publication_before_compaction_defeats_age_retirement() {
        // Writer serialization makes the retirement/raise decision atomic in
        // whichever order the two transactions commit. The retire-first
        // ordering is pinned by `retirement_wins_before_delayed_checkpoint_
        // publication`; this is the raise-first ordering: an activation that
        // commits before compaction must protect the row from age
        // retirement, however far past its grace the registration is.
        let backend = memory_backend();
        let sql = backend.sql();
        let model = "ann-registry-activate-race";
        register_pending_at(sql.as_ref(), "late-build", "local", model, 1)
            .await
            .expect("register");
        assert!(raise_watermark(
            sql.as_ref(),
            "late-build",
            "local",
            model,
            9,
            WatermarkAuthority::PendingOrActive,
        )
        .await
        .expect("activation commits first"));
        let outcome = compact_write_log_at(
            sql.as_ref(),
            CompactionScope::Namespace("local".into()),
            model,
            10 * PENDING_GRACE_US,
        )
        .await
        .expect("compact");
        assert!(
            outcome.retired_consumers.is_empty(),
            "an activated consumer must never age-retire: {:?}",
            outcome.retired_consumers
        );
        assert_eq!(
            scalar_i64(
                sql.as_ref(),
                "SELECT watermark FROM ann_consumer_watermark \
                 WHERE consumer = 'late-build' AND embedding_model = ?1",
                vec![SqlValue::Text(model.into())],
            )
            .await,
            9
        );
    }

    #[tokio::test]
    async fn repeated_pending_registration_does_not_refresh_grace() {
        let backend = memory_backend();
        let sql = backend.sql();
        let model = "ann-registry-stable-grace";
        register_pending_at(sql.as_ref(), "pending", "local", model, 11)
            .await
            .expect("first registration");
        register_pending_at(sql.as_ref(), "pending", "local", model, 99)
            .await
            .expect("repeat registration");
        assert_eq!(
            scalar_i64(
                sql.as_ref(),
                "SELECT registered_at_us FROM ann_consumer_pending \
                 WHERE consumer = 'pending' AND embedding_model = ?1",
                vec![SqlValue::Text(model.into())],
            )
            .await,
            11
        );
    }

    #[tokio::test]
    async fn recovering_and_active_zero_consumers_never_age_retire() {
        let backend = memory_backend();
        let sql = backend.sql();
        let model = "ann-registry-protected-states";
        register_pending_at(sql.as_ref(), "active-zero", "local", model, 1)
            .await
            .expect("register active zero");
        assert!(raise_watermark(
            sql.as_ref(),
            "active-zero",
            "local",
            model,
            0,
            WatermarkAuthority::PendingOrActive,
        )
        .await
        .expect("activate at zero"));
        mark_recovering(sql.as_ref(), "recovering", "local", model)
            .await
            .expect("recovering");

        let outcome = compact_write_log_at(
            sql.as_ref(),
            CompactionScope::Namespace("local".into()),
            model,
            PENDING_GRACE_US * 10,
        )
        .await
        .expect("compact");
        assert!(outcome.retired_consumers.is_empty());
        assert_eq!(
            scalar_i64(
                sql.as_ref(),
                "SELECT COUNT(*) FROM ann_consumer_watermark \
                 WHERE embedding_model = ?1",
                vec![SqlValue::Text(model.into())],
            )
            .await,
            2
        );
    }

    // The statement tests below keep a copy of each literal the packs used to
    // inline as the oracle: the builder must reproduce its text, its parameter
    // order and its label exactly.

    fn assert_statement(got: &SqlStatement, sql: &str, params: Vec<SqlValue>, label: &str) {
        assert_eq!(got.sql, sql, "statement text");
        assert_eq!(
            format!("{:?}", got.params),
            format!("{params:?}"),
            "parameter order"
        );
        assert_eq!(got.label.as_deref(), Some(label), "statement label");
    }

    fn text(value: &str) -> SqlValue {
        SqlValue::Text(value.to_owned())
    }

    #[test]
    fn pathless_register_pending_matches_the_inlined_literal() {
        assert_statement(
            &pathless_register_pending("memory_", "memory:ann", "*", "model"),
            "INSERT OR IGNORE INTO ann_consumer_watermark \
             (consumer, namespace, embedding_model, watermark) \
             VALUES (?1, ?2, ?3, ?4)",
            vec![
                text("memory:ann"),
                text("*"),
                text("model"),
                SqlValue::Integer(-2),
            ],
            "memory_ann_register_pathless_consumer",
        );
        assert_statement(
            &pathless_register_pending("knowledge_", "knowledge:atom", "local", "model"),
            "INSERT OR IGNORE INTO ann_consumer_watermark \
             (consumer, namespace, embedding_model, watermark) \
             VALUES (?1, ?2, ?3, ?4)",
            vec![
                text("knowledge:atom"),
                text("local"),
                text("model"),
                SqlValue::Integer(-2),
            ],
            "knowledge_ann_register_pathless_consumer",
        );
    }

    #[test]
    fn pathless_mark_recovering_matches_the_inlined_literal() {
        assert_statement(
            &pathless_mark_recovering("knowledge_", "knowledge:atom", "local", "model"),
            "INSERT INTO ann_consumer_watermark \
             (consumer, namespace, embedding_model, watermark) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(consumer, namespace, embedding_model) \
             DO UPDATE SET watermark = excluded.watermark",
            vec![
                text("knowledge:atom"),
                text("local"),
                text("model"),
                SqlValue::Integer(-1),
            ],
            "knowledge_ann_mark_pathless_recovering",
        );
    }

    #[test]
    fn pathless_raise_watermark_matches_the_inlined_literal_for_every_authority() {
        let cases = [
            (
                WatermarkAuthority::PendingOrActive,
                "(watermark = -2 OR (watermark >= 0 AND watermark <= ?4))",
            ),
            (
                WatermarkAuthority::Active,
                "watermark >= 0 AND watermark <= ?4",
            ),
            (WatermarkAuthority::Recovering, "watermark = -1"),
        ];
        for (authority, predicate) in cases {
            let oracle = format!(
                "UPDATE ann_consumer_watermark SET watermark = ?4 \
                 WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3 \
                   AND {predicate}"
            );
            assert_statement(
                &pathless_raise_watermark("memory_", "memory:ann", "*", "model", 7, authority),
                &oracle,
                vec![
                    text("memory:ann"),
                    text("*"),
                    text("model"),
                    SqlValue::Integer(7),
                ],
                "memory_ann_raise_pathless_watermark",
            );
        }
        let knowledge = pathless_raise_watermark(
            "knowledge_",
            "knowledge:atom",
            "local",
            "model",
            0,
            WatermarkAuthority::Active,
        );
        assert_eq!(
            knowledge.label.as_deref(),
            Some("knowledge_ann_raise_pathless_watermark")
        );
    }

    #[test]
    fn pathless_compact_log_matches_the_inlined_literal_for_a_namespace() {
        assert_statement(
            &pathless_compact_log(
                "knowledge_",
                CompactionScope::Namespace("local".into()),
                "model",
            ),
            "DELETE FROM ann_write_log \
             WHERE namespace = ?1 AND embedding_model = ?2 \
               AND seq <= (SELECT MIN(watermark.watermark) \
                           FROM ann_consumer_watermark watermark \
                           WHERE (watermark.namespace = ?1 \
                                  OR watermark.namespace = '*') \
                             AND watermark.embedding_model = ?2)",
            vec![text("local"), text("model")],
            "knowledge_ann_compact_pathless_log",
        );
    }

    #[test]
    fn pathless_compact_log_matches_the_inlined_literal_for_a_model() {
        assert_statement(
            &pathless_compact_log("memory_", CompactionScope::Model, "model"),
            "DELETE FROM ann_write_log \
             WHERE embedding_model = ?1 \
               AND seq <= (SELECT MIN(watermark.watermark) \
                           FROM ann_consumer_watermark watermark \
                           WHERE (watermark.namespace = ann_write_log.namespace \
                                  OR watermark.namespace = '*') \
                             AND watermark.embedding_model = ?1)",
            vec![text("model")],
            "memory_ann_compact_pathless_log",
        );
    }

    #[test]
    fn read_watermark_statement_matches_the_inlined_literal() {
        let sql = "SELECT watermark FROM ann_consumer_watermark \
                   WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3";
        assert_statement(
            &read_watermark_statement("memory_", "memory:ann", "*", "model"),
            sql,
            vec![text("memory:ann"), text("*"), text("model")],
            "memory_ann_read_own_watermark",
        );
        // The knowledge pack has always labelled this statement without a pack
        // prefix; an empty prefix keeps that operator-visible name.
        assert_statement(
            &read_watermark_statement("", "knowledge:atom", "local", "model"),
            sql,
            vec![text("knowledge:atom"), text("local"), text("model")],
            "ann_read_own_watermark",
        );
    }

    async fn watermark_of(sql: &dyn SqlAccess, model: &str) -> Option<i64> {
        read_watermark(sql, "test_", "reader", "local", model)
            .await
            .expect("read watermark")
    }

    #[tokio::test]
    async fn read_watermark_reports_each_registry_state_and_none_when_absent() {
        let backend = memory_backend();
        let sql = backend.sql();
        let model = "ann-registry-read-watermark";
        assert_eq!(watermark_of(sql.as_ref(), model).await, None);
        register_pending_at(sql.as_ref(), "reader", "local", model, 1)
            .await
            .expect("register");
        assert_eq!(watermark_of(sql.as_ref(), model).await, Some(-2));
        assert!(raise_watermark(
            sql.as_ref(),
            "reader",
            "local",
            model,
            5,
            WatermarkAuthority::PendingOrActive,
        )
        .await
        .expect("activate"));
        assert_eq!(watermark_of(sql.as_ref(), model).await, Some(5));
        mark_recovering(sql.as_ref(), "reader", "local", model)
            .await
            .expect("recovering");
        assert_eq!(watermark_of(sql.as_ref(), model).await, Some(-1));
        let other = read_watermark(sql.as_ref(), "test_", "reader", "elsewhere", model)
            .await
            .expect("read other namespace");
        assert_eq!(other, None, "the namespace is part of the row identity");
    }

    async fn run(sql: &dyn SqlAccess, statement: SqlStatement) -> u64 {
        sql.writer()
            .await
            .expect("writer")
            .execute(statement)
            .await
            .expect("execute")
    }

    #[tokio::test]
    async fn pathless_statements_drive_the_registry_rows() {
        let backend = memory_backend();
        let sql = backend.sql();
        let model = "ann-registry-pathless-run";
        let register = pathless_register_pending("test_", "reader", "local", model);
        assert_eq!(run(sql.as_ref(), register.clone()).await, 1);
        assert_eq!(run(sql.as_ref(), register).await, 0, "insert ignored");
        assert_eq!(watermark_of(sql.as_ref(), model).await, Some(-2));

        let raise = |watermark, authority| {
            pathless_raise_watermark("test_", "reader", "local", model, watermark, authority)
        };
        let recovering = WatermarkAuthority::Recovering;
        assert_eq!(run(sql.as_ref(), raise(3, recovering)).await, 0);
        let first = raise(5, WatermarkAuthority::PendingOrActive);
        assert_eq!(run(sql.as_ref(), first).await, 1);
        assert_eq!(watermark_of(sql.as_ref(), model).await, Some(5));
        let behind = raise(4, WatermarkAuthority::Active);
        assert_eq!(run(sql.as_ref(), behind).await, 0, "no step back");
        let ahead = raise(9, WatermarkAuthority::Active);
        assert_eq!(run(sql.as_ref(), ahead).await, 1);
        assert_eq!(watermark_of(sql.as_ref(), model).await, Some(9));

        let fence = pathless_mark_recovering("test_", "reader", "local", model);
        assert_eq!(run(sql.as_ref(), fence).await, 1);
        assert_eq!(watermark_of(sql.as_ref(), model).await, Some(-1));
        assert_eq!(run(sql.as_ref(), raise(11, recovering)).await, 1);
        assert_eq!(watermark_of(sql.as_ref(), model).await, Some(11));

        for seq in 1..=6 {
            execute(
                sql.as_ref(),
                "INSERT INTO ann_write_log \
                 (seq, namespace, embedding_model, kind, field, subject_id, op) \
                 VALUES (?1, 'local', ?2, 'note', 'note.content', ?3, 'upsert')",
                vec![
                    SqlValue::Integer(seq),
                    text(model),
                    SqlValue::Text(format!("subject-{seq}")),
                ],
            )
            .await;
        }
        let by_namespace =
            pathless_compact_log("test_", CompactionScope::Namespace("local".into()), model);
        assert_eq!(run(sql.as_ref(), by_namespace).await, 6);
        execute(
            sql.as_ref(),
            "INSERT INTO ann_write_log \
             (seq, namespace, embedding_model, kind, field, subject_id, op) \
             VALUES (7, 'local', ?1, 'note', 'note.content', 'subject-7', 'upsert')",
            vec![text(model)],
        )
        .await;
        let by_model = pathless_compact_log("test_", CompactionScope::Model, model);
        assert_eq!(run(sql.as_ref(), by_model).await, 1);
    }
}
