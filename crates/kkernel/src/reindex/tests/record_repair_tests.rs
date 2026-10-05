//! Exercise the real CLI route against an owned SQLite database.

use super::*;

fn repair_args(dir: &std::path::Path, id: Uuid) -> ReindexArgs {
    let mut args = snapshot_reindex_args(dir, false);
    args.id = Some(id);
    args.batch_size = 128;
    args
}

async fn script(runtime: &KhiveRuntime, sql: String) {
    runtime
        .sql()
        .writer()
        .await
        .expect("writer")
        .execute_script(sql)
        .await
        .expect("install owned fixture");
}

#[test]
fn record_selector_rejects_bulk_options_and_non_uuid_ids() {
    // Default batch size does not conflict; an explicit batch option does.
    let id = Uuid::new_v4().to_string();
    let parsed = ReindexArgs::try_parse_from(["reindex", "--id", &id])
        .expect("single record with ordinary defaults");
    assert_eq!(parsed.id.unwrap().to_string(), id);
    for tail in [
        vec!["--model", "model"],
        vec!["--batch-size", "128"],
        vec!["--knowledge-only"],
        vec!["--sections-only"],
        vec!["--no-sections"],
        vec!["--rebuild-fts"],
    ] {
        let mut argv = vec!["reindex", "--id", id.as_str()];
        argv.extend(tail);
        assert!(ReindexArgs::try_parse_from(argv).is_err());
    }
    for invalid in ["deadbeef", "a note name"] {
        assert!(ReindexArgs::try_parse_from(["reindex", "--id", invalid]).is_err());
    }
}

#[tokio::test]
async fn record_selector_repairs_degraded_insert_without_touching_other_indexes() {
    if crate::test_process::run_in_child() {
        return;
    }
    let dir = tempfile::tempdir().expect("owned directory");
    let mut args = repair_args(dir.path(), Uuid::nil());
    let runtime = snapshot_test_runtime(&args);
    let token = runtime.authorize(Namespace::local()).expect("namespace");
    let healthy = runtime
        .create_note(
            &token,
            "observation",
            None,
            "healthyindexsentinel",
            None,
            None,
            vec![],
        )
        .await
        .expect("healthy note");
    let fts = runtime.text_for_notes(&token).expect("FTS");
    let healthy_before = serde_json::to_value(
        fts.get_document("local", healthy.id)
            .await
            .expect("document")
            .expect("indexed"),
    )
    .unwrap();

    // Fail the actual index transaction after its note insert has committed.
    // FTS5 virtual tables cannot own triggers; their ordinary rowid map can.
    script(
        &runtime,
        "CREATE TRIGGER reject_repair_fixture_index
        BEFORE INSERT ON fts_notes_rowids
        BEGIN SELECT RAISE(ABORT, 'fixture index unavailable'); END;"
            .into(),
    )
    .await;
    let degraded_id = match runtime
        .try_create_note(&token, "observation", None, "degradedindexsentinel", None)
        .await
    {
        // The repair contract does not depend on whether the caller received
        // the older best-effort success or the explicit committed-degradation
        // refusal. Both must identify a committed note with no FTS document.
        Ok(Some(note)) => note.id,
        Ok(None) => panic!("unique fixture must create a note"),
        Err(error) => {
            let khive_runtime::RuntimeError::Khive(domain) = error.refusal_source() else {
                panic!("unexpected conditional-insert failure: {error:?}");
            };
            let details = domain.details().expect("committed degradation details");
            assert_eq!(details.get("reason"), Some("post_commit_degraded"));
            assert_eq!(details.get("committed"), Some("true"));
            assert!(details
                .get("post_commit_degradations")
                .unwrap()
                .contains("fts_upsert"));
            details
                .get("record_id")
                .unwrap()
                .parse()
                .expect("committed record UUID")
        }
    };
    assert!(runtime
        .notes(&token)
        .unwrap()
        .get_note(degraded_id)
        .await
        .unwrap()
        .is_some());
    assert!(fts
        .get_document("local", degraded_id)
        .await
        .expect("read")
        .is_none());
    script(&runtime, "DROP TRIGGER reject_repair_fixture_index;".into()).await;

    // Count real writes to the unrelated row; identical rewrites still count.
    script(
        &runtime,
        format!(
            "CREATE TABLE unrelated_index_writes (n INTEGER);
        INSERT INTO unrelated_index_writes VALUES (0);
        CREATE TRIGGER observe_unrelated_index_insert AFTER INSERT ON fts_notes_rowids
        WHEN NEW.subject_id = '{}' BEGIN UPDATE unrelated_index_writes SET n=n+1; END;
        CREATE TRIGGER observe_unrelated_index_delete AFTER DELETE ON fts_notes_rowids
        WHEN OLD.subject_id = '{}' BEGIN UPDATE unrelated_index_writes SET n=n+1; END;
        CREATE TRIGGER observe_unrelated_index_update AFTER UPDATE ON fts_notes_rowids
        WHEN NEW.subject_id = '{}' OR OLD.subject_id = '{}'
        BEGIN UPDATE unrelated_index_writes SET n=n+1; END;",
            healthy.id, healthy.id, healthy.id, healthy.id
        ),
    )
    .await;
    args.id = Some(degraded_id);
    run_reindex_without_embeddings(args)
        .await
        .expect("single-record CLI repair");
    let hits = fts
        .search(khive_storage::types::TextSearchRequest {
            query: "degradedindexsentinel".into(),
            mode: khive_storage::types::TextQueryMode::Plain,
            filter: None,
            top_k: 10,
            snippet_chars: 0,
        })
        .await
        .expect("real keyword search");
    assert!(hits.iter().any(|hit| hit.subject_id == degraded_id));
    assert_eq!(
        serde_json::to_value(
            fts.get_document("local", healthy.id)
                .await
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        healthy_before
    );
    assert_eq!(
        snapshot_test_count(&runtime, "SELECT n FROM unrelated_index_writes").await,
        0
    );
}

#[tokio::test]
async fn record_selector_refuses_unknown_id_without_namespace_backfill() {
    if crate::test_process::run_in_child() {
        return;
    }
    let dir = tempfile::tempdir().expect("owned directory");
    let args = repair_args(dir.path(), Uuid::new_v4());
    let runtime = snapshot_test_runtime(&args);
    let token = runtime.authorize(Namespace::local()).unwrap();
    let unrelated = Note::new("local", "observation", "unrelatedmissingindex");
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(unrelated.clone())
        .await
        .unwrap();
    let failure = run_reindex_without_embeddings(args).await;
    assert!(
        failure.is_err(),
        "unknown ID must not become a successful empty run"
    );
    assert!(runtime
        .text_for_notes(&token)
        .unwrap()
        .get_document("local", unrelated.id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn record_selector_stage_failure_exits_nonzero_unless_best_effort() {
    if crate::test_process::run_in_child() {
        return;
    }
    let dir = tempfile::tempdir().expect("owned directory");
    let mut args = repair_args(dir.path(), Uuid::nil());
    let runtime = snapshot_test_runtime(&args);
    let token = runtime.authorize(Namespace::local()).unwrap();
    let note = Note::new("local", "observation", "retryableindexfailure");
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    script(
        &runtime,
        "CREATE TRIGGER reject_repair_fixture_index
        BEFORE INSERT ON fts_notes_rowids
        BEGIN SELECT RAISE(ABORT, 'fixture index unavailable'); END;"
            .into(),
    )
    .await;
    args.id = Some(note.id);
    let error = run_reindex_without_embeddings(args)
        .await
        .expect_err("partial failure");
    assert!(error.to_string().contains("repair remains incomplete"));
    let mut retry = repair_args(dir.path(), note.id);
    retry.best_effort = true;
    run_reindex_without_embeddings(retry)
        .await
        .expect("explicit best effort");
    assert!(runtime
        .text_for_notes(&token)
        .unwrap()
        .get_document("local", note.id)
        .await
        .unwrap()
        .is_none());
}
