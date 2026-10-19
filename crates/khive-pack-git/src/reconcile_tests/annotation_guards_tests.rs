use super::*;
use khive_runtime::RuntimeError;
use khive_storage::graph::{
    EdgeInsertIfAbsentOutcome, GraphGuardValueKind, GraphStatementGuardFailure,
    GraphStatementGuardFailureReason,
};
use std::sync::{Arc, Mutex};

async fn guard(f: &Fixture) -> AnnotationGuard {
    annotation_guard(
        &cursor_snapshot(&f.runtime, f.project_b).await.unwrap(),
        &f.first_sha,
        &f.source_identity,
    )
    .unwrap()
}

async fn link(f: &Fixture, guard: AnnotationGuard) -> Result<AnnotationOutcome, RuntimeError> {
    link_annotation_if_absent(&f.runtime, &f.token, f.first_note, f.project_b, guard).await
}

async fn rows(f: &Fixture, sql: &str) -> Value {
    serde_json::to_value(
        f.runtime
            .sql()
            .reader()
            .await
            .unwrap()
            .query_all(SqlStatement::new(sql, vec![]))
            .await
            .unwrap(),
    )
    .unwrap()
}

async fn images(f: &Fixture) -> Value {
    json!({
        "edges": rows(f, "SELECT * FROM graph_edges ORDER BY namespace,id").await,
        "ledger": rows(f, "SELECT * FROM graph_edges_seq ORDER BY seq").await,
        "sequence": rows(f, "SELECT * FROM sqlite_sequence WHERE name='graph_edges_seq'").await,
        "events": rows(f, "SELECT * FROM events ORDER BY id").await,
        "observations": rows(f, "SELECT * FROM event_observations ORDER BY event_id,role,position").await,
    })
}

fn script(f: &Fixture, sql: &str) {
    f.runtime
        .backend()
        .pool()
        .try_writer()
        .unwrap()
        .autocommit(|conn| Ok(conn.execute_batch(sql)?))
        .unwrap();
}

#[test]
fn annotation_classifies_only_known_predicate_false_labels() {
    for (label, expected) in [
        ("git.annotation.source", AnnotationOutcome::SourceChanged),
        ("git.annotation.target", AnnotationOutcome::TargetChanged),
        ("git.annotation.cursor", AnnotationOutcome::CursorChanged),
    ] {
        let failed = |reason| {
            EdgeInsertIfAbsentOutcome::GuardFailed(GraphStatementGuardFailure {
                label: label.into(),
                reason,
            })
        };
        assert_eq!(
            annotation::classify_outcome(failed(GraphStatementGuardFailureReason::PredicateFalse))
                .unwrap(),
            expected
        );
        for reason in [
            GraphStatementGuardFailureReason::NoRows,
            GraphStatementGuardFailureReason::MultipleRows,
            GraphStatementGuardFailureReason::ColumnCount { actual: 2 },
            GraphStatementGuardFailureReason::NonBoolean {
                kind: GraphGuardValueKind::Null,
            },
        ] {
            assert!(annotation::classify_outcome(failed(reason)).is_err());
        }
    }
    assert!(
        annotation::classify_outcome(EdgeInsertIfAbsentOutcome::GuardFailed(
            GraphStatementGuardFailure {
                label: "unrecognized".into(),
                reason: GraphStatementGuardFailureReason::PredicateFalse,
            }
        ))
        .is_err()
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn annotation_false_guards_keep_source_target_cursor_precedence() {
    for first_false in 0..3 {
        let f = Fixture::new().await;
        let mut expected = guard(&f).await;
        if first_false == 0 {
            expected.expected_sha = if f.first_sha == "a".repeat(40) {
                "b".repeat(40)
            } else {
                "a".repeat(40)
            };
        }
        if first_false <= 1 {
            expected.source_identity.push_str("/changed");
        }
        expected.commits.updated_at += 1;
        let before = images(&f).await;
        assert_eq!(
            link(&f, expected).await.unwrap(),
            [
                AnnotationOutcome::SourceChanged,
                AnnotationOutcome::TargetChanged,
                AnnotationOutcome::CursorChanged,
            ][first_false]
        );
        assert_eq!(images(&f).await, before);
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn annotation_cursor_native_error_is_not_a_false_guard_or_a_write() {
    let f = Fixture::new().await;
    let expected = guard(&f).await;
    script(&f, "DROP TABLE git_mirror_cursor");
    let before = images(&f).await;
    let error = link(&f, expected.clone())
        .await
        .expect_err("missing cursor table is operational");
    assert!(error.to_string().contains("no such table"), "{error}");
    assert_eq!(images(&f).await, before);

    // Ordered earlier false guards may win over a later native cursor error.
    let mut stale_source = expected.clone();
    stale_source.expected_sha = if f.first_sha == "a".repeat(40) {
        "b".repeat(40)
    } else {
        "a".repeat(40)
    };
    assert_eq!(
        link(&f, stale_source).await.unwrap(),
        AnnotationOutcome::SourceChanged
    );
    let mut stale_target = expected;
    stale_target.source_identity.push_str("/changed");
    assert_eq!(
        link(&f, stale_target).await.unwrap(),
        AnnotationOutcome::TargetChanged
    );
    assert_eq!(images(&f).await, before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn annotation_cursor_requires_both_exact_bytes_timestamps_and_text_types() {
    for changed in 0..6 {
        let f = Fixture::new().await;
        let mut expected = guard(&f).await;
        match changed {
            0 => expected.commits.value.push(0),
            1 => expected.checkpoint.value.push(0),
            2 => expected.commits.updated_at += 1,
            3 => expected.checkpoint.updated_at += 1,
            4 | 5 => {
                f.execute(
                    "UPDATE git_mirror_cursor SET cursor_value=CAST(cursor_value AS BLOB) \
                 WHERE project_id=?1 AND kind=?2",
                    vec![
                        SqlValue::Text(f.project_b.to_string()),
                        SqlValue::Text(
                            if changed == 4 {
                                "commits"
                            } else {
                                "commits_checkpoint"
                            }
                            .into(),
                        ),
                    ],
                )
                .await
            }
            _ => unreachable!(),
        }
        let before = images(&f).await;
        let cursor_before = rows(
            &f,
            "SELECT * FROM git_mirror_cursor ORDER BY project_id,kind",
        )
        .await;
        assert_eq!(
            link(&f, expected).await.unwrap(),
            AnnotationOutcome::CursorChanged
        );
        assert_eq!(images(&f).await, before);
        assert_eq!(
            rows(
                &f,
                "SELECT * FROM git_mirror_cursor ORDER BY project_id,kind"
            )
            .await,
            cursor_before
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn annotation_same_sha_uniqueness_includes_deleted_notes_with_and_without_indexes() {
    for indexed in [true, false] {
        for deleted in [false, true] {
            let f = Fixture::new().await;
            let expected = guard(&f).await;
            if !indexed {
                script(
                    &f,
                    "DROP INDEX idx_git_notes_history_canonical_sha; \
                            DROP INDEX idx_git_notes_history_noncanonical",
                );
            }
            let mut duplicate = khive_storage::Note::new("local", "commit", "duplicate SHA");
            duplicate.properties = Some(json!({"sha":f.first_sha}));
            if deleted {
                duplicate.deleted_at = Some(chrono::Utc::now().timestamp_micros());
            }
            f.runtime
                .notes(&f.token)
                .unwrap()
                .upsert_note(duplicate)
                .await
                .unwrap();
            let before = images(&f).await;
            assert_eq!(
                link(&f, expected).await.unwrap(),
                AnnotationOutcome::SourceChanged,
                "indexed={indexed}, deleted={deleted}"
            );
            assert_eq!(images(&f).await, before);
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn annotation_unexplained_zero_insert_remains_an_error() {
    let f = Fixture::new().await;
    let expected = guard(&f).await;
    script(
        &f,
        &format!(
            "CREATE TRIGGER annotation_ignore_insert BEFORE INSERT ON graph_edges \
         WHEN NEW.source_id='{}' AND NEW.target_id='{}' \
         BEGIN SELECT RAISE(IGNORE); END;",
            f.first_note, f.project_b
        ),
    );
    let before = images(&f).await;
    let error = link(&f, expected.clone())
        .await
        .expect_err("zero without a winner is not Created");
    assert!(
        error
            .to_string()
            .to_ascii_lowercase()
            .contains("query returned no rows"),
        "{error}"
    );
    assert_eq!(images(&f).await, before);
    script(&f, "DROP TRIGGER annotation_ignore_insert");
    assert_eq!(
        link(&f, expected).await.unwrap(),
        AnnotationOutcome::Created
    );
    assert_eq!(f.link_created_events(f.first_note).await, 1);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn annotation_event_failure_rolls_back_edge_sequence_and_projection_then_recovers() {
    let f = Fixture::new().await;
    let expected = guard(&f).await;
    script(
        &f,
        &format!(
            "CREATE TRIGGER annotation_event_fault BEFORE INSERT ON events \
         WHEN NEW.verb='link' AND EXISTS(SELECT 1 FROM graph_edges \
              WHERE source_id='{}' AND target_id='{}') \
         BEGIN SELECT RAISE(ABORT,'annotation-event-after-edge'); END;",
            f.first_note, f.project_b
        ),
    );
    let before = images(&f).await;
    let error = link(&f, expected.clone())
        .await
        .expect_err("event fault must abort owner unit");
    assert!(
        error.to_string().contains("annotation-event-after-edge"),
        "{error}"
    );
    assert_eq!(images(&f).await, before);
    script(&f, "DROP TRIGGER annotation_event_fault");
    assert_eq!(
        link(&f, expected).await.unwrap(),
        AnnotationOutcome::Created
    );
    assert_eq!(f.link_created_events(f.first_note).await, 1);
    let events = rows(
        &f,
        "SELECT payload FROM events WHERE verb='link' ORDER BY id",
    )
    .await;
    let encoded = serde_json::to_string(&events).unwrap();
    assert!(encoded.contains("source_kind"));
    let mut reader = f.runtime.sql().reader().await.unwrap();
    let row = reader
        .query_row(SqlStatement::new(
            "SELECT json_extract(payload,'$.source_kind') AS source_kind, \
                json_extract(payload,'$.target_kind') AS target_kind \
         FROM events WHERE verb='link' AND target_id IN \
             (SELECT id FROM graph_edges WHERE source_id=?1 AND target_id=?2)",
            vec![
                SqlValue::Text(f.first_note.to_string()),
                SqlValue::Text(f.project_b.to_string()),
            ],
        ))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(row.get("source_kind"), Some(SqlValue::Text(kind)) if kind == "note"));
    assert!(matches!(row.get("target_kind"), Some(SqlValue::Text(kind)) if kind == "entity"));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn annotation_actual_cursor_evaluation_observes_the_writer_transaction() {
    let f = Fixture::new().await;
    let expected = guard(&f).await;
    let observed = Arc::new(Mutex::new(Vec::new()));
    {
        let writer = f.runtime.backend().pool().try_writer().unwrap();
        let observed = Arc::clone(&observed);
        writer
            .autocommit(|conn| {
                conn.create_scalar_function(
                    "annotation_cursor_transaction",
                    1,
                    rusqlite::functions::FunctionFlags::SQLITE_UTF8,
                    move |context| {
                        // SAFETY: the non-owning reference stays inside this callback on
                        // the executing connection's thread. No handle escapes, no query
                        // or mutation is re-entered, and only SQLite's transaction state is read.
                        let connection = unsafe { context.get_connection()? };
                        observed.lock().expect("transaction observations").push(
                            connection.transaction_state(Some("main"))?
                                == rusqlite::TransactionState::Write,
                        );
                        context.get::<i64>(0)
                    },
                )
                .unwrap();
                conn.execute_batch(
                    "ALTER TABLE git_mirror_cursor RENAME TO annotation_cursor_rows; \
             CREATE VIEW git_mirror_cursor AS \
             SELECT project_id,kind,cursor_value, \
                    annotation_cursor_transaction(updated_at) AS updated_at \
             FROM annotation_cursor_rows;",
                )
                .unwrap();
                Ok(())
            })
            .unwrap();
    }
    // This is evaluated by the ACTUAL pack cursor SELECT, on its real connection.
    // A post-guard hook alone would not detect a cursor cached before BEGIN.
    assert_eq!(
        link(&f, expected).await.unwrap(),
        AnnotationOutcome::Created
    );
    let observed = observed.lock().unwrap();
    assert!(
        observed.len() >= 2,
        "both cursor rows must execute the witness: {observed:?}"
    );
    assert!(
        observed.iter().all(|write| *write),
        "every actual cursor evaluation must be inside the writer transaction: {observed:?}"
    );
}
