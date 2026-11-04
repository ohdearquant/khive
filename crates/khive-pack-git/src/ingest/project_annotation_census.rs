use super::*;

const NOTE_LIMIT: usize = 1000;
const NOTE_ID_LIMIT: usize = 20;

#[derive(Debug, Default)]
struct ProjectAnnotationCensus {
    missing_count: u64,
    note_ids: Vec<String>,
    complete: bool,
    notes_scanned: u64,
}

pub(super) async fn append_report(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    project_id: Uuid,
    report: &mut IngestReport,
) {
    match read_census(runtime, token, project_id, NOTE_LIMIT, NOTE_ID_LIMIT).await {
        Ok(census) => {
            report.missing_project_annotation_count = Some(census.missing_count);
            report.missing_project_annotation_note_ids = census.note_ids;
            report.project_annotation_census_complete = census.complete;
            report.project_annotation_notes_scanned = census.notes_scanned;
        }
        Err(error) => {
            report.missing_project_annotation_count = None;
            report.missing_project_annotation_note_ids.clear();
            report.project_annotation_census_complete = false;
            report.project_annotation_notes_scanned = 0;
            let detail = error.to_string();
            report.warnings.push(format!(
                "project annotation census unavailable: {}",
                mask_git_ingest(&detail)
            ));
        }
    }
}

async fn read_census(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    project_id: Uuid,
    note_limit: usize,
    note_id_limit: usize,
) -> Result<ProjectAnnotationCensus> {
    let fetch_limit = i64::try_from(note_limit)
        .ok()
        .and_then(|limit| limit.checked_add(1))
        .ok_or_else(|| anyhow!("project annotation census limit is out of range"))?;
    let sql = runtime.sql();
    let mut reader = sql.reader().await.map_err(anyhow::Error::new)?;
    // One statement keeps the note prefix and edge-existence checks in one
    // snapshot. The extra row is a coverage sentinel, never part of the count.
    let rows = reader
        .query_all(SqlStatement {
            sql: sql!("project_annotation_census").into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(project_id.to_string()),
                SqlValue::Integer(fetch_limit),
            ],
            label: Some("git_ingest_project_annotation_census".into()),
        })
        .await
        .map_err(anyhow::Error::new)?;
    let mut census = ProjectAnnotationCensus {
        complete: rows.len() <= note_limit,
        ..ProjectAnnotationCensus::default()
    };
    for row in rows.iter().take(note_limit) {
        let id = row
            .uuid("id")
            .ok()
            .ok_or_else(|| anyhow!("project annotation census returned an invalid note ID"))?;
        let missing = match row.get("missing_annotation") {
            Some(SqlValue::Integer(0)) => false,
            Some(SqlValue::Integer(1)) => true,
            _ => bail!("project annotation census returned an invalid edge-existence value"),
        };
        census.notes_scanned += 1;
        if missing {
            census.missing_count += 1;
            if census.note_ids.len() < note_id_limit {
                census.note_ids.push(id.to_string());
            }
        }
    }
    // Sorting the bounded display sample needs no SQL sort of the corpus.
    census.note_ids.sort_unstable();
    Ok(census)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GitPack;
    use khive_pack_kg::KgPack;

    async fn fixture() -> (KhiveRuntime, NamespaceToken, VerbRegistry) {
        let runtime = KhiveRuntime::memory().expect("private memory runtime");
        let token = runtime
            .authorize(khive_runtime::Namespace::local())
            .expect("authorize local");
        let mut builder = khive_runtime::VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(GitPack::new(runtime.clone()));
        builder
            .with_runtime_event_store(&runtime)
            .expect("configure trusted runtime audit store");
        let registry = builder.build().expect("registry builds");
        runtime.install_edge_rules(registry.all_edge_rules());
        registry.call_register_entity_type_validators(&runtime);
        registry.apply_schema_plans(runtime.backend());
        (runtime, token, registry)
    }

    async fn create(registry: &VerbRegistry, body: Value) -> Uuid {
        let response = registry.dispatch("create", body).await.expect("create ok");
        Uuid::parse_str(response["id"].as_str().expect("receipt ID"))
            .expect("canonical note or entity ID")
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn project_annotation_census_empty_report_is_complete_zero() {
        let (runtime, token, registry) = fixture().await;
        let project_id = create(&registry, json!({"kind": "project", "name": "census-empty"})).await;
        let mut report = IngestReport {
            done: true,
            ..IngestReport::default()
        };
        assert_eq!(report.missing_project_annotation_count, None);
        append_report(&runtime, &token, project_id, &mut report).await;
        assert_eq!(report.missing_project_annotation_count, Some(0));
        assert!(report.project_annotation_census_complete);
        assert_eq!(report.project_annotation_notes_scanned, 0);
        assert!(report.missing_project_annotation_note_ids.is_empty());
        assert!(report.done, "the diagnostic must preserve ingest completion");
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn project_annotation_census_cap_and_id_cap_are_independent() {
        let (runtime, token, registry) = fixture().await;
        let project_id = create(&registry, json!({"kind": "project", "name": "census-cap"})).await;
        let empty = read_census(&runtime, &token, project_id, 2, 1)
            .await
            .expect("empty census");
        assert!(empty.complete);
        assert_eq!(empty.notes_scanned, 0);
        assert_eq!(empty.missing_count, 0);

        for number in 1..=3 {
            create(
                &registry,
                json!({
                    "kind": "issue",
                    "content": "",
                    "properties": {
                        "number": number,
                        "title": format!("cap issue {number}"),
                        "project_id": project_id.to_string()
                    }
                }),
            )
            .await;
        }

        let partial = read_census(&runtime, &token, project_id, 2, 1)
            .await
            .expect("capped census");
        assert!(!partial.complete, "the third note is a coverage sentinel");
        assert_eq!(partial.notes_scanned, 2);
        assert_eq!(partial.missing_count, 2, "the sentinel is not counted");
        assert_eq!(partial.note_ids.len(), 1, "the display list has its own cap");

        let complete = read_census(&runtime, &token, project_id, 3, 1)
            .await
            .expect("complete census");
        assert!(complete.complete);
        assert_eq!(complete.notes_scanned, 3);
        assert_eq!(complete.missing_count, 3);
        assert_eq!(complete.note_ids.len(), 1);
    }
}
