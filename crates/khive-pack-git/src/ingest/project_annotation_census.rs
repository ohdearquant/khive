use super::*;

const NOTE_LIMIT: usize = 1000;
const NOTE_ID_LIMIT: usize = 20;

pub(super) async fn append_existing_note_warning(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    project_id: Uuid,
    kind: &str,
    number: u64,
    note_id: Uuid,
    report: &mut IngestReport,
) {
    match read_existing_note_annotation_status(runtime, token, project_id, kind, number, note_id)
        .await
    {
        Ok(false) => {}
        Ok(true) => report.warnings.push(format!(
            "existing {kind} #{number} note {note_id} is missing a live project annotation to {project_id}"
        )),
        Err(error) => {
            let detail = error.to_string();
            report.warnings.push(format!(
                "existing {kind} #{number} note {note_id} project annotation check unavailable: {}",
                mask_git_ingest(&detail)
            ));
        }
    }
}

async fn read_existing_note_annotation_status(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    project_id: Uuid,
    kind: &str,
    number: u64,
    note_id: Uuid,
) -> Result<bool> {
    let sql = runtime.sql();
    let mut reader = sql.reader().await.map_err(anyhow::Error::new)?;
    // Revalidate the lookup identity and inspect its edge in one read snapshot.
    // A changed note is unavailable evidence, not a missing annotation.
    let row = reader
        .query_row(SqlStatement {
            sql: sql!("existing_project_annotation_status").into(),
            params: vec![
                SqlValue::Text(note_id.to_string()),
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(kind.to_string()),
                SqlValue::Integer(number as i64),
                SqlValue::Text(project_id.to_string()),
            ],
            label: Some("git_ingest_existing_project_annotation_status".into()),
        })
        .await
        .map_err(anyhow::Error::new)?
        .ok_or_else(|| anyhow!("existing note changed"))?;
    match row.get("missing_annotation") {
        Some(SqlValue::Integer(0)) => Ok(false),
        Some(SqlValue::Integer(1)) => Ok(true),
        _ => bail!("invalid edge-existence value"),
    }
}

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
    // The display sample is sorted here, after the bounded read.
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
        let project_id = create(
            &registry,
            json!({"kind": "project", "name": "census-empty"}),
        )
        .await;
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
        assert!(
            report.done,
            "the diagnostic must preserve ingest completion"
        );
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
        assert_eq!(
            partial.note_ids.len(),
            1,
            "the display list has its own cap"
        );

        let complete = read_census(&runtime, &token, project_id, 3, 1)
            .await
            .expect("complete census");
        assert!(complete.complete);
        assert_eq!(complete.notes_scanned, 3);
        assert_eq!(complete.missing_count, 3);
        assert_eq!(complete.note_ids.len(), 1);
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn project_annotation_census_partial_scan_inspects_lowest_note_ids() {
        let (runtime, token, registry) = fixture().await;
        let project_id = create(
            &registry,
            json!({"kind": "project", "name": "census-order"}),
        )
        .await;
        let mut ids = Vec::new();
        // The lowest ID is in the middle, independently of generated IDs.
        for (number, id) in [
            (1, "64140000-0000-4000-8000-000000000003"),
            (2, "64140000-0000-4000-8000-000000000001"),
            (3, "64140000-0000-4000-8000-000000000002"),
        ] {
            let title = format!("order issue {number}");
            let mut note = khive_storage::Note::new("local", "issue", title.as_str());
            note.id = Uuid::parse_str(id).expect("fixed fixture UUID");
            note.properties = Some(json!({
                "number": number,
                "title": title,
                "project_id": project_id.to_string()
            }));
            let id = note.id;
            runtime
                .notes(&token)
                .expect("local note store")
                .upsert_note(note)
                .await
                .expect("insert fixed-ID issue note");
            ids.push(id.to_string());
        }
        let lowest = ids.iter().min().cloned().expect("at least one note");
        assert!(
            Some(&lowest) != ids.first() && Some(&lowest) != ids.last(),
            "fixture needs the lowest ID to be neither the first nor the last created note"
        );

        let partial = read_census(&runtime, &token, project_id, 1, 20)
            .await
            .expect("capped census");
        assert!(!partial.complete);
        assert_eq!(partial.notes_scanned, 1);
        assert_eq!(
            partial.note_ids,
            vec![lowest],
            "a partial census must inspect the lowest note IDs"
        );
    }

    fn annotation_note(kind: &str, number: u64, project_id: Uuid) -> khive_storage::Note {
        let mut note = khive_storage::Note::new("local", kind, "untrusted body text");
        note.properties = Some(json!({
            "number": number,
            "title": "untrusted title text",
            "project_id": project_id.to_string()
        }));
        note
    }

    fn annotation_edge(note_id: Uuid, project_id: Uuid) -> khive_storage::Edge {
        let now = chrono::DateTime::from_timestamp_micros(khive_storage::now_micros())
            .expect("representable fixture timestamp");
        khive_storage::Edge {
            id: Uuid::new_v4().into(),
            namespace: "local".into(),
            source_id: note_id,
            target_id: project_id,
            relation: EdgeRelation::Annotates,
            weight: 1.0,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata: Some(json!({"fixture": "preserve metadata"})),
            target_backend: None,
        }
    }

    async fn annotation_rows(runtime: &KhiveRuntime) -> Value {
        let sql = runtime.sql();
        let mut reader = sql.reader().await.expect("snapshot reader");
        let mut snapshot = Vec::new();
        // Complete, ordered rows include tombstones and foreign namespaces.
        for statement in [
            "SELECT * FROM notes ORDER BY id",
            "SELECT * FROM graph_edges ORDER BY id",
        ] {
            let rows = reader
                .query_all(SqlStatement::new(statement, vec![]))
                .await
                .expect("read all domain rows");
            snapshot.push(serde_json::to_value(rows).expect("serialize tagged SQL cells"));
        }
        json!(snapshot)
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn existing_annotation_warning_requires_the_live_project_edge() {
        let (runtime, token, registry) = fixture().await;
        let project_id = create(&registry, json!({"kind": "project", "name": "edge-check"})).await;
        let other_project = create(
            &registry,
            json!({"kind": "project", "name": "other-edge-check"}),
        )
        .await;
        let notes = runtime.notes(&token).expect("note store");
        let graph = runtime.graph(&token).expect("graph store");
        for kind in ["issue", "pull_request"] {
            for (index, case) in [
                "linked",
                "absent",
                "deleted",
                "relation",
                "reverse",
                "target",
                "namespace",
            ]
            .into_iter()
            .enumerate()
            {
                let number = index as u64 + 1;
                let note = annotation_note(kind, number, project_id);
                let note_id = note.id;
                notes.upsert_note(note).await.expect("seed existing note");
                let mut edge = annotation_edge(note_id, project_id);
                match case {
                    "deleted" => edge.deleted_at = Some(edge.updated_at),
                    "relation" => edge.relation = EdgeRelation::Extends,
                    "reverse" => std::mem::swap(&mut edge.source_id, &mut edge.target_id),
                    "target" => edge.target_id = other_project,
                    "namespace" => edge.namespace = "foreign".into(),
                    _ => {}
                }
                if case != "absent" {
                    let edge_id = edge.id;
                    graph
                        .upsert_edge(edge.clone())
                        .await
                        .expect("seed candidate edge");
                    let stored = graph
                        .get_edge_including_deleted(edge_id)
                        .await
                        .expect("read candidate edge")
                        .expect("candidate edge persisted");
                    assert_eq!(
                        serde_json::to_value(stored).unwrap(),
                        serde_json::to_value(edge).unwrap()
                    );
                }
                // A linked foreign note with the same number cannot cover the local note.
                let mut foreign = annotation_note(kind, number, project_id);
                foreign.namespace = "foreign".into();
                let mut foreign_edge = annotation_edge(foreign.id, project_id);
                foreign_edge.namespace = "foreign".into();
                notes
                    .upsert_note(foreign)
                    .await
                    .expect("seed foreign same-number note");
                graph
                    .upsert_edge(foreign_edge)
                    .await
                    .expect("seed foreign annotation");
                assert_eq!(
                    find_by_number(&runtime, &token, kind, project_id, number)
                        .await
                        .unwrap(),
                    Some(note_id)
                );
                let before = annotation_rows(&runtime).await;
                let mut report = IngestReport {
                    done: true,
                    issues_skipped_existing: 9,
                    ..IngestReport::default()
                };
                let mut expected = serde_json::to_value(&report).unwrap();
                if case != "linked" {
                    expected["warnings"] = json!([format!(
                        "existing {kind} #{number} note {note_id} is missing a live project annotation to {project_id}"
                    )]);
                }
                append_existing_note_warning(
                    &runtime,
                    &token,
                    project_id,
                    kind,
                    number,
                    note_id,
                    &mut report,
                )
                .await;
                assert_eq!(
                    serde_json::to_value(&report).unwrap(),
                    expected,
                    "{kind}/{case}"
                );
                assert_eq!(
                    annotation_rows(&runtime).await,
                    before,
                    "{kind}/{case} must only read"
                );
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn existing_annotation_warning_revalidates_the_lookup_note() {
        let (runtime, token, registry) = fixture().await;
        let project_id = create(
            &registry,
            json!({"kind": "project", "name": "changed-note-check"}),
        )
        .await;
        let notes = runtime.notes(&token).expect("note store");
        for kind in ["issue", "pull_request"] {
            for (index, case) in [
                "id",
                "missing",
                "namespace",
                "kind",
                "number",
                "project",
                "deleted",
            ]
            .into_iter()
            .enumerate()
            {
                let number = index as u64 + 1;
                let mut note = annotation_note(kind, number, project_id);
                let mut note_id = note.id;
                notes
                    .upsert_note(note.clone())
                    .await
                    .expect("seed lookup note");
                assert_eq!(
                    find_by_number(&runtime, &token, kind, project_id, number)
                        .await
                        .unwrap(),
                    Some(note_id)
                );
                match case {
                    "id" => note_id = Uuid::new_v4(),
                    "missing" => assert!(notes
                        .delete_note(note_id, khive_storage::DeleteMode::Hard)
                        .await
                        .unwrap()),
                    "namespace" => note.namespace = "foreign".into(),
                    "kind" => note.kind = "observation".into(),
                    "number" => note.properties.as_mut().unwrap()["number"] = json!(number + 100),
                    "project" => {
                        note.properties.as_mut().unwrap()["project_id"] =
                            json!(Uuid::new_v4().to_string())
                    }
                    "deleted" => note.deleted_at = Some(khive_storage::now_micros()),
                    _ => unreachable!(),
                }
                if !matches!(case, "id" | "missing") {
                    notes
                        .upsert_note(note)
                        .await
                        .expect("change observed lookup identity");
                }
                let before = annotation_rows(&runtime).await;
                let mut report = IngestReport {
                    done: true,
                    ..IngestReport::default()
                };
                let mut expected = serde_json::to_value(&report).unwrap();
                expected["warnings"] = json!([format!(
                    "existing {kind} #{number} note {note_id} project annotation check unavailable: existing note changed"
                )]);
                append_existing_note_warning(
                    &runtime,
                    &token,
                    project_id,
                    kind,
                    number,
                    note_id,
                    &mut report,
                )
                .await;
                assert_eq!(
                    serde_json::to_value(&report).unwrap(),
                    expected,
                    "{kind}/{case}"
                );
                assert_eq!(annotation_rows(&runtime).await, before);
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn existing_annotation_warning_reports_unavailable_reads() {
        let (runtime, token, registry) = fixture().await;
        let project_id = create(
            &registry,
            json!({"kind": "project", "name": "read-error-check"}),
        )
        .await;
        let note = annotation_note("issue", 17, project_id);
        let note_id = note.id;
        runtime
            .notes(&token)
            .unwrap()
            .upsert_note(note)
            .await
            .expect("seed note");
        runtime
            .graph(&token)
            .unwrap()
            .upsert_edge(annotation_edge(note_id, project_id))
            .await
            .expect("seed live annotation");
        assert_eq!(
            find_by_number(&runtime, &token, "issue", project_id, 17)
                .await
                .unwrap(),
            Some(note_id)
        );
        let before = annotation_rows(&runtime).await;
        let mut linked = IngestReport::default();
        append_existing_note_warning(
            &runtime,
            &token,
            project_id,
            "issue",
            17,
            note_id,
            &mut linked,
        )
        .await;
        assert!(linked.warnings.is_empty(), "healthy linked positive");
        runtime
            .sql()
            .writer()
            .await
            .expect("fault writer")
            .execute(SqlStatement::new(
                "ALTER TABLE graph_edges RENAME TO annotation_edges_unavailable",
                vec![],
            ))
            .await
            .expect("hide graph table in private fixture");
        let mut unavailable = IngestReport {
            done: true,
            ..IngestReport::default()
        };
        append_existing_note_warning(
            &runtime,
            &token,
            project_id,
            "issue",
            17,
            note_id,
            &mut unavailable,
        )
        .await;
        assert_eq!(unavailable.warnings.len(), 1);
        assert!(unavailable.warnings[0].starts_with(&format!(
            "existing issue #17 note {note_id} project annotation check unavailable: "
        )));
        assert!(!unavailable.warnings[0].contains("is missing a live project annotation"));
        assert!(!unavailable.warnings[0].contains("untrusted"));
        let mut expected = serde_json::to_value(IngestReport {
            done: true,
            ..IngestReport::default()
        })
        .unwrap();
        expected["warnings"] = json!(unavailable.warnings);
        assert_eq!(serde_json::to_value(&unavailable).unwrap(), expected);
        runtime
            .sql()
            .writer()
            .await
            .expect("restore writer")
            .execute(SqlStatement::new(
                "ALTER TABLE annotation_edges_unavailable RENAME TO graph_edges",
                vec![],
            ))
            .await
            .expect("restore graph table");
        assert_eq!(annotation_rows(&runtime).await, before);
        let mut restored = IngestReport::default();
        append_existing_note_warning(
            &runtime,
            &token,
            project_id,
            "issue",
            17,
            note_id,
            &mut restored,
        )
        .await;
        assert!(restored.warnings.is_empty(), "restored linked positive");
        assert_eq!(annotation_rows(&runtime).await, before);
    }
}
