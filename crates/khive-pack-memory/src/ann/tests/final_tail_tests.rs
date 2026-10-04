use super::super::final_tail::final_tail_statement;
use super::*;

const MODEL: &str = "memory-final-tail-coalescing-test";

#[tokio::test]
async fn fresh_tail_joins_once_per_subject_and_matches_raw_replay() {
    let rt = test_runtime_with_hash_embedder(MODEL, 8);
    let token = rt.authorize(Namespace::local()).expect("local token");
    let mut ids = Vec::new();
    for i in 0..3 {
        ids.push(
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("repeated tail subject {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create note with embedding")
            .id,
        );
    }
    let sql = rt.sql();
    let mut writer = sql.writer().await.expect("writer");
    for (index, op) in [
        (2, "delete"),
        (0, "delete"),
        (2, "upsert"),
        (1, "delete"),
        (0, "upsert"),
        (0, "upsert"),
    ] {
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO ann_write_log \
                      (namespace, embedding_model, kind, field, subject_id, op) \
                      VALUES ('local', ?1, 'note', 'note.content', ?2, ?3)"
                    .into(),
                params: vec![
                    SqlValue::Text(MODEL.into()),
                    SqlValue::Text(ids[index].to_string()),
                    SqlValue::Text(op.into()),
                ],
                label: Some("test_repeated_tail_write".into()),
            })
            .await
            .expect("append repeated write");
    }
    writer
        .execute(SqlStatement {
            sql: format!(
                "DELETE FROM vec_{} WHERE subject_id = ?1",
                sanitize_model_key(MODEL)
            ),
            params: vec![SqlValue::Text(ids[1].to_string())],
            label: Some("test_final_tail_deleted_vector".into()),
        })
        .await
        .expect("remove the final deleted vector");
    drop(writer);

    let table = format!("vec_{}", sanitize_model_key(MODEL));
    let mut reader = sql.reader().await.expect("reader");
    // Preserve the pre-coalescing statement as the raw-row replay reference.
    let raw = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT log.seq, log.subject_id, log.op, \
                        vectors.embedding_model AS vector_model, \
                        vectors.kind AS vector_kind, vectors.field AS vector_field, \
                        vectors.embedding, live_note.id AS live_note_id \
                 FROM ann_write_log log \
                 LEFT JOIN {table} vectors ON vectors.subject_id = log.subject_id \
                 LEFT JOIN notes live_note ON live_note.id = log.subject_id \
                   AND live_note.deleted_at IS NULL \
                 WHERE log.embedding_model = ?1 AND log.kind = 'note' \
                   AND log.field = 'note.content' ORDER BY log.seq"
            ),
            params: vec![SqlValue::Text(MODEL.into())],
            label: Some("test_raw_tail_reference".into()),
        })
        .await
        .expect("raw suffix reference");
    assert_eq!(raw.len(), 9, "three initial and six repeated writes");

    let statement = final_tail_statement(MODEL, 0, None);
    let rows = reader
        .query_all(statement.clone())
        .await
        .expect("final rows");
    assert_eq!(rows.len(), 3, "join one embedding per distinct subject");
    let expected = parse_final_tail_rows(&raw, MODEL, 0).expect("raw replay");
    let actual = parse_final_tail_rows(&rows, MODEL, 0).expect("reduced replay");
    assert_eq!(
        actual, expected,
        "ops, first appearance order, and watermark"
    );
    assert_eq!(actual.0.iter().map(|(id, _)| *id).collect::<Vec<_>>(), ids);
    assert!(actual.0[0].1.is_some());
    assert!(actual.0[1].1.is_none(), "highest-seq delete wins");
    assert!(actual.0[2].1.is_some(), "upsert after delete wins");
    assert_eq!(
        actual.1,
        raw.last().expect("last raw row").i64("seq").unwrap() as u64
    );
    assert!(rows.last().unwrap().i64("seq").unwrap() < actual.1 as i64);
    assert_eq!(
        fetch_final_tail_on(reader.as_mut(), MODEL, 0, None)
            .await
            .expect("serving seam"),
        expected
    );

    let plans = reader.explain(statement).await.expect("query plan");
    let vector_plans: Vec<_> = plans
        .iter()
        .filter_map(|row| row.text("detail").ok())
        .filter(|detail| detail.contains("VIRTUAL TABLE INDEX "))
        .collect();
    assert_eq!(vector_plans.len(), 1, "one vec0 join plan: {plans:?}");
    // sqlite-vec's POINT index starts with 2!; a full scan uses index 1.
    assert!(
        vector_plans[0]
            .rsplit_once(':')
            .is_some_and(|(_, index)| index.starts_with("2!")),
        "vector join must use the subject point lookup: {vector_plans:?}"
    );

    // ceil(1.0 * 2 live vectors) selects two raw writes, both for subject 0.
    // Limiting after coalescing would incorrectly include other subjects.
    let capped = reader
        .query_all(final_tail_statement(MODEL, 0, Some(1.0)))
        .await
        .expect("capped suffix");
    assert_eq!(capped.len(), 1);
    assert_eq!(
        parse_final_tail_rows(&capped, MODEL, 0).expect("capped replay"),
        parse_final_tail_rows(&raw[raw.len() - 2..], MODEL, 0).expect("raw capped reference")
    );
    assert_eq!(capped[0].text("subject_id").unwrap(), ids[0].to_string());

    let empty = reader
        .query_all(final_tail_statement(MODEL, actual.1, None))
        .await
        .expect("empty suffix");
    assert!(empty.is_empty());
    assert_eq!(
        parse_final_tail_rows(&empty, MODEL, actual.1).unwrap(),
        (vec![], actual.1)
    );
}
