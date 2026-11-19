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
        .execute(SqlStatement::new(sql_text, params).labelled("ann_registry_test"))
        .await
        .expect("execute");
}

async fn scalar_i64(sql: &dyn SqlAccess, sql_text: &str, params: Vec<SqlValue>) -> i64 {
    match sql
        .reader()
        .await
        .expect("reader")
        .query_scalar(SqlStatement::new(sql_text, params).labelled("ann_registry_test_scalar"))
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
// inline as the oracle: the builder must reproduce its SQL up to whitespace,
// its parameter order and its label exactly.

fn assert_statement(got: &SqlStatement, sql: &str, params: Vec<SqlValue>, label: &str) {
    assert_eq!(
        got.sql.split_whitespace().collect::<Vec<_>>(),
        sql.split_whitespace().collect::<Vec<_>>(),
        "statement text"
    );
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
