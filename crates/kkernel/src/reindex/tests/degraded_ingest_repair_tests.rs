//! A conditional insert whose FTS write fails after commit is not repaired by
//! repeating the write: with the FTS store healthy again, the replay
//! deduplicates and indexes nothing. The record's FTS document comes back only
//! through the namespace reindex.

use super::*;
use khive_runtime::{ChannelIngestCapability, NamespaceToken, RuntimeError};
use khive_storage::types::TextFilter;

const TRIGGER: &str = "refuse_degraded_ingest_fts";

fn open(db_path: &str, config: &std::path::Path) -> (KhiveRuntime, NamespaceToken) {
    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(db_path),
        config: Some(config),
        namespace: Namespace::local(),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: Some(vec!["kg".into(), "comm".into()]),
        brain_profile: None,
    })
    .expect("resolve runtime config");
    let rt = KhiveRuntime::new(cfg).expect("runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize");
    (rt, token)
}

fn execute(rt: &KhiveRuntime, sql: &str) {
    rt.backend()
        .pool()
        .writer()
        .expect("writer")
        .conn()
        .execute_batch(sql)
        .expect("fixture sql");
}

/// Note FTS documents in the namespace; an empty `ids` counts all of them.
async fn fts_documents(rt: &KhiveRuntime, token: &NamespaceToken, ids: Vec<Uuid>) -> u64 {
    rt.text_for_notes(token)
        .expect("FTS store")
        .count(TextFilter {
            kinds: vec![SubstrateKind::Note],
            record_kinds: vec![],
            namespaces: vec!["local".to_string()],
            ids,
        })
        .await
        .expect("fts count")
}

fn degraded_record_id(error: &RuntimeError) -> Uuid {
    let RuntimeError::Khive(domain) = error.refusal_source() else {
        panic!("expected a committed degradation, got {error:?}");
    };
    let details = domain.details().expect("committed degradation details");
    assert_eq!(details.get("reason"), Some("post_commit_degraded"));
    assert_eq!(details.get("committed"), Some("true"));
    assert_eq!(details.get("retryable"), Some("false"));
    assert!(details
        .get("post_commit_degradations")
        .expect("diagnostics")
        .contains("fts_upsert"));
    details
        .get("record_id")
        .expect("record_id")
        .parse()
        .expect("record_id is a UUID")
}

#[tokio::test]
async fn reindex_restores_the_fts_document_a_degraded_conditional_insert_lacks() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("fixture dir");
    let db_path = dir.path().join("degraded.db");
    let db_path = db_path.to_str().expect("utf8 path").to_string();
    let config = write_empty_test_config(dir.path());
    let capability = ChannelIngestCapability::grant_for_direct_composition();
    let properties = serde_json::json!({
        "external_id": "imap:degraded-repair:1",
        "channel_kind": "email",
        "channel_slug": "mailbox@example.com",
    });

    let id = {
        let (rt, token) = open(&db_path, &config);
        rt.text_for_notes(&token).expect("FTS store");
        execute(
            &rt,
            &format!(
                "CREATE TRIGGER {TRIGGER} BEFORE INSERT ON fts_notes_rowids \
                 BEGIN SELECT RAISE(ABORT, 'degraded ingest FTS refusal'); END;"
            ),
        );
        let write = || {
            rt.try_create_note_as_trusted_ingest(
                &capability,
                &token,
                "message",
                None,
                "degraded ingest repair sentinel",
                Some(properties.clone()),
                None,
            )
        };
        let error = write()
            .await
            .expect_err("a committed insert whose FTS write fails reports the degradation");
        let id = degraded_record_id(&error);
        assert!(rt
            .notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .is_some());

        assert_eq!(
            fts_documents(&rt, &token, vec![]).await,
            0,
            "the refused FTS write leaves the namespace without note documents"
        );

        // The store accepts writes again, so any indexing by the replay would land.
        execute(&rt, &format!("DROP TRIGGER {TRIGGER};"));
        let replay = write()
            .await
            .expect("the replay is a clean deduplicated write");
        assert!(replay.is_none(), "the replay must deduplicate: {replay:?}");
        assert_eq!(
            fts_documents(&rt, &token, vec![]).await,
            0,
            "a deduplicated replay indexes nothing, so the document stays missing"
        );
        id
    };

    run_reindex_without_embeddings(ReindexArgs {
        db: Some(db_path.clone()),
        config: Some(config.clone()),
        model: None,
        batch_size: 100,
        keep_existing: true,
        namespace: Some("local".to_string()),
        knowledge_only: false,
        no_knowledge: true,
        best_effort: false,
        no_sections: false,
        sections_only: false,
        rebuild_fts: false,
        human: false,
    })
    .await
    .expect("namespace reindex succeeds");

    let (rt, token) = open(&db_path, &config);
    assert_eq!(
        fts_documents(&rt, &token, vec![id]).await,
        1,
        "the namespace reindex restores the degraded record's FTS document"
    );
}
