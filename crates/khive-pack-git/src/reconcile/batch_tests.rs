use super::*;
use khive_storage::types::SqlRow;
use khive_storage::{SqlReader, StorageResult};
use serde_json::json;

struct CountingReader {
    inner: Box<dyn SqlReader>,
    statements: Vec<SqlStatement>,
    returned: Vec<usize>,
}

#[async_trait::async_trait]
impl SqlReader for CountingReader {
    async fn query_all(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        self.statements.push(statement.clone());
        let rows = self.inner.query_all(statement).await?;
        self.returned.push(rows.len());
        Ok(rows)
    }
    async fn query_row(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlRow>> {
        self.statements.push(statement.clone());
        self.inner.query_row(statement).await
    }
    async fn query_scalar(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlValue>> {
        self.statements.push(statement.clone());
        self.inner.query_scalar(statement).await
    }
    async fn explain(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        self.statements.push(statement.clone());
        self.inner.explain(statement).await
    }
}

fn note_id(index: usize) -> Uuid {
    Uuid::from_u128(0xaaaaaaaa_aaaa_aaaa_aaaa_000000000000 + index as u128)
}

fn note(id: String, namespace: &str, kind: &str, sha: &str, deleted: bool) -> SqlStatement {
    SqlStatement::new("INSERT INTO notes(id,namespace,kind,properties,created_at,updated_at,deleted_at) VALUES(?1,?2,?3,?4,0,0,?5)", vec![SqlValue::Text(id), SqlValue::Text(namespace.into()), SqlValue::Text(kind.into()), SqlValue::Text(json!({"sha":sha}).to_string()), if deleted { SqlValue::Integer(42) } else { SqlValue::Null }])
}

fn edge(
    index: usize,
    source: String,
    project: Uuid,
    namespace: &str,
    relation: &str,
    deleted: bool,
) -> SqlStatement {
    SqlStatement::new("INSERT INTO graph_edges(namespace,id,source_id,target_id,relation,weight,created_at,updated_at,deleted_at,metadata) VALUES(?1,?2,?3,?4,?5,0.42,0,0,?6,'{\"curated\":true}')", vec![SqlValue::Text(namespace.into()), SqlValue::Text(note_id(10_000 + index).to_string()), SqlValue::Text(source), SqlValue::Text(project.to_string()), SqlValue::Text(relation.into()), if deleted { SqlValue::Integer(42) } else { SqlValue::Null }])
}

fn report(project: Uuid) -> ReconcileReport {
    ReconcileReport {
        project_id: project,
        namespace: "local".into(),
        source: "fixture".into(),
        frozen_tip: String::new(),
        preview_id: String::new(),
        complete_coverage: true,
        counts: ReconcileCounts::default(),
        diagnostics: Vec::new(),
        apply: None,
        after_apply: None,
        cursors_unchanged: true,
        success: false,
    }
}

async fn fixture(mixed: bool) -> (khive_db::StorageBackend, Vec<String>, Uuid) {
    let backend = khive_db::StorageBackend::memory().unwrap();
    assert!(backend.sql().database_path().is_none());
    backend.notes().unwrap();
    backend.graph().unwrap();
    let shas: Vec<_> = (0..2_000)
        .map(|i| format!("{:040x}", 0xabc000 + i))
        .collect();
    let project = Uuid::from_u128(0xbbbbbbbb_bbbb_bbbb_bbbb_000000000000);
    let mut statements = Vec::new();
    for (i, sha) in shas.iter().enumerate() {
        if mixed && i == 0 {
            continue;
        }
        let id = if mixed && i == 1 {
            note_id(i).to_string().to_uppercase()
        } else {
            note_id(i).to_string()
        };
        statements.push(note(id, "local", "commit", sha, mixed && i == 899));
    }
    if mixed {
        // More than three matches must remain ambiguous, bounded per SHA.
        for i in 0..3 {
            statements.push(note(
                note_id(3_000 + i).to_string(),
                "local",
                "commit",
                &shas[900],
                false,
            ));
        }
        statements.push(note(
            note_id(4_000).to_string(),
            "other",
            "commit",
            &shas[0],
            false,
        ));
        statements.push(note(
            note_id(4_001).to_string(),
            "local",
            "observation",
            &shas[0],
            false,
        ));
        statements.push(note(
            note_id(4_002).to_string(),
            "local",
            "commit",
            &shas[0].to_uppercase(),
            false,
        ));
        statements.extend([
            edge(
                1,
                note_id(1).to_string(),
                project,
                "local",
                "annotates",
                false,
            ),
            edge(
                2,
                note_id(2).to_string().to_uppercase(),
                project,
                "local",
                "annotates",
                false,
            ),
            edge(
                3,
                note_id(3).to_string(),
                Uuid::nil(),
                "local",
                "annotates",
                false,
            ),
            edge(4, note_id(4).to_string(), project, "local", "cites", false),
            edge(
                5,
                note_id(5).to_string(),
                project,
                "other",
                "annotates",
                false,
            ),
            edge(
                6,
                note_id(1799).to_string(),
                project,
                "local",
                "annotates",
                false,
            ),
            edge(
                7,
                note_id(1800).to_string(),
                project,
                "local",
                "annotates",
                true,
            ),
        ]);
    }
    backend
        .sql()
        .writer()
        .await
        .unwrap()
        .execute_batch(statements)
        .await
        .unwrap();
    (backend, shas, project)
}

async fn snapshot(reader: &mut dyn SqlReader) -> serde_json::Value {
    let notes = reader
        .query_all(SqlStatement::new("SELECT * FROM notes ORDER BY id", vec![]))
        .await
        .unwrap();
    let edges = reader
        .query_all(SqlStatement::new(
            "SELECT * FROM graph_edges ORDER BY namespace,id",
            vec![],
        ))
        .await
        .unwrap();
    json!({"notes":notes,"edges":edges})
}

#[tokio::test]
async fn two_thousand_acknowledged_shas_use_six_lookup_reads() {
    let (backend, shas, project) = fixture(false).await;
    let mut reader = CountingReader {
        inner: backend.sql().reader().await.unwrap(),
        statements: Vec::new(),
        returned: Vec::new(),
    };
    let mut report = report(project);
    let mut hasher = blake3::Hasher::new();
    let candidates = inspect_acknowledged(
        &mut reader,
        "local",
        project,
        &shas,
        &mut report,
        &mut hasher,
    )
    .await
    .unwrap();
    assert_eq!(
        reader.statements.len(),
        6,
        "lookup reads only; full preview adds three fixed reads"
    );
    assert_eq!(reader.returned, [900, 0, 900, 0, 200, 0]);
    for (chunk, pair) in shas.chunks(900).zip(reader.statements.chunks_exact(2)) {
        assert_eq!(pair[0].label.as_deref(), Some("git_annotation_repair_note"));
        assert_eq!(pair[1].label.as_deref(), Some("git_annotation_repair_edge"));
        assert!(
            matches!(&pair[0].params[1], SqlValue::Text(value) if value == &serde_json::to_string(chunk).unwrap())
        );
    }
    assert_eq!(
        candidates,
        shas.iter()
            .enumerate()
            .map(|(i, sha)| (sha.clone(), note_id(i)))
            .collect::<Vec<_>>()
    );
    assert_eq!(report.counts.acknowledged_shas_examined, 2_000);
    assert_eq!(report.counts.live_note_hits, 2_000);
    assert_eq!(report.counts.repairable_missing_links, 2_000);
    assert_eq!(
        report.counts.missing_notes
            + report.counts.deleted_notes
            + report.counts.ambiguous_notes
            + report.counts.live_project_edges
            + report.counts.tombstones_skipped,
        0
    );
}

#[tokio::test]
async fn mixed_chunk_boundaries_preserve_classes_hash_and_exact_identity() {
    let (backend, shas, project) = fixture(true).await;
    let mut reader = CountingReader {
        inner: backend.sql().reader().await.unwrap(),
        statements: Vec::new(),
        returned: Vec::new(),
    };
    let before = snapshot(reader.inner.as_mut()).await;
    let mut report = report(project);
    let mut hasher = blake3::Hasher::new();
    let candidates = inspect_acknowledged(
        &mut reader,
        "local",
        project,
        &shas,
        &mut report,
        &mut hasher,
    )
    .await
    .unwrap();
    assert_eq!(reader.statements.len(), 6);
    assert_eq!(reader.returned, [899, 1, 902, 1, 200, 1]);
    assert_eq!(report.counts.acknowledged_shas_examined, 2_000);
    assert_eq!(
        (
            report.counts.missing_notes,
            report.counts.deleted_notes,
            report.counts.ambiguous_notes
        ),
        (1, 1, 1)
    );
    assert_eq!(
        (
            report.counts.live_note_hits,
            report.counts.live_project_edges,
            report.counts.tombstones_skipped,
            report.counts.repairable_missing_links
        ),
        (1_997, 2, 1, 1_994)
    );
    let mut expected_hash = blake3::Hasher::new();
    let mut expected_candidates = Vec::new();
    for (i, sha) in shas.iter().enumerate() {
        let class = match i {
            0 => "missing_note",
            899 => "deleted_note",
            900 => "ambiguous_note",
            1 | 1799 => "live_edge",
            1800 => "tombstone",
            _ => "missing_link",
        };
        if !matches!(i, 0 | 899 | 900) {
            expected_hash.update(note_id(i).as_bytes());
        }
        expected_hash.update(&serde_json::to_vec(&(sha, class)).unwrap());
        if class == "missing_link" {
            expected_candidates.push((sha.clone(), note_id(i)));
        }
    }
    assert_eq!(candidates, expected_candidates);
    assert_eq!(hasher.finalize(), expected_hash.finalize());
    assert_eq!(snapshot(reader.inner.as_mut()).await, before);
}

#[tokio::test]
async fn unique_live_invalid_id_still_refuses_before_edge_lookup() {
    let backend = khive_db::StorageBackend::memory().unwrap();
    backend.notes().unwrap();
    backend.graph().unwrap();
    let sha = "a".repeat(40);
    backend
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(note("broken-id".into(), "local", "commit", &sha, false))
        .await
        .unwrap();
    let mut reader = CountingReader {
        inner: backend.sql().reader().await.unwrap(),
        statements: Vec::new(),
        returned: Vec::new(),
    };
    let mut report = report(Uuid::nil());
    let error = inspect_acknowledged(
        &mut reader,
        "local",
        Uuid::nil(),
        &[sha],
        &mut report,
        &mut blake3::Hasher::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "stored commit note has an invalid id");
    assert_eq!(reader.statements.len(), 1);
}
