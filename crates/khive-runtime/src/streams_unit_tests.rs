use super::{allocate_sequence, SequenceRefusal};
use crate::atomic_prepare::{prepare_delete, prepare_update};
use crate::atomic_runner::{run_atomic_unit, AtomicRunOutcome};
use crate::{KhiveRuntime, Namespace, NotePatch, RuntimeError, VerbRegistryBuilder};
use serde_json::{json, Value};

#[test]
fn allocate_sequence_exhaustion_preserves_head_at_checked_add_boundary() {
    // Overflow is logically possible, but no bounded real-store fixture
    // reaches it: stream_gap requires consecutive inserts from 1, and
    // ledger UPDATE/DELETE are forbidden. Supply the head at this unit
    // boundary instead of fabricating an exhausted persisted stream.
    let mut head = i64::MAX - 1;
    assert!(matches!(
        allocate_sequence(&mut head, None),
        Ok(seq) if seq == i64::MAX
    ));
    assert_eq!(head, i64::MAX);
    assert!(matches!(
        allocate_sequence(&mut head, None),
        Err(SequenceRefusal::Exhausted)
    ));
    assert_eq!(
        head,
        i64::MAX,
        "refused allocation must not advance the head"
    );
}

#[tokio::test]
async fn stream_atomic_metadata_cas_preserves_record_and_refuses_stale_plan() {
    let rt = KhiveRuntime::memory().unwrap();
    let token = rt.authorize(Namespace::local()).unwrap();
    let registry = VerbRegistryBuilder::new().build().unwrap();
    let appended = rt
        .stream_append(
            &token,
            "cas",
            &json!({"n": 1}),
            None,
            "observation",
            None,
            None,
            None,
            None,
            &registry,
        )
        .await
        .unwrap();
    let id = uuid::Uuid::parse_str(appended["id"].as_str().unwrap()).unwrap();
    for args in [
        json!({"id": id, "content": "changed"}),
        json!({"id": id, "properties": {"x": 1}}),
    ] {
        let err = prepare_update(&rt, &token, &args, None).await.unwrap_err();
        assert!(matches!(err, RuntimeError::Khive(ref e) if e.details().is_some()));
        assert!(err.to_string().contains("immutable"));
    }
    for hard in [false, true] {
        let err = prepare_delete(&rt, &token, &json!({"id": id, "hard": hard}), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("immutable"));
    }
    let plan = prepare_update(&rt, &token, &json!({"id": id, "salience": 0.7}), None)
        .await
        .unwrap();
    assert!(matches!(
        run_atomic_unit(rt.sql().as_ref(), vec![plan])
            .await
            .unwrap(),
        AtomicRunOutcome::Committed { .. }
    ));
    let stale = prepare_update(&rt, &token, &json!({"id": id, "salience": 0.2}), None)
        .await
        .unwrap();
    rt.update_note(
        &token,
        id,
        NotePatch::new(None, None, Some(Some(0.9)), None, None),
    )
    .await
    .unwrap();
    assert!(matches!(
        run_atomic_unit(rt.sql().as_ref(), vec![stale])
            .await
            .unwrap(),
        AtomicRunOutcome::RolledBack { .. }
    ));
    let note = rt
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(note.salience, Some(0.9));
    assert_eq!(
        serde_json::from_str::<Value>(&note.content).unwrap(),
        json!({"n": 1})
    );
}

#[tokio::test]
async fn stream_namespace_isolation_and_real_stat_count() {
    let rt = KhiveRuntime::memory().unwrap();
    let a = rt.authorize(Namespace::parse("a").unwrap()).unwrap();
    let b = rt.authorize(Namespace::parse("b").unwrap()).unwrap();
    let registry = VerbRegistryBuilder::new().build().unwrap();
    for token in [&a, &b] {
        assert_eq!(
            rt.stream_append(
                token,
                "same-name",
                &json!(token.namespace().as_str()),
                Some(1),
                "observation",
                None,
                None,
                None,
                None,
                &registry
            )
            .await
            .unwrap()["seq"],
            1
        );
    }
    for token in [&a, &b] {
        let page = rt.stream_read(token, "same-name", 0, 10).await.unwrap();
        assert_eq!(page["entries"][0]["record"], token.namespace().as_str());
    }
    rt.sql().writer().await.unwrap().execute_script("DROP TRIGGER refuse_stream_ledger_update; UPDATE note_streams SET seq=5 WHERE namespace='a';".into()).await.unwrap();
    assert_eq!(
        rt.stream_stat(&a, "same-name").await.unwrap(),
        json!({"count": 1, "head_seq": 5, "floor_seq": 1})
    );
    assert_eq!(
        rt.stream_stat(&b, "same-name").await.unwrap(),
        json!({"count": 1, "head_seq": 1, "floor_seq": 1})
    );
}
