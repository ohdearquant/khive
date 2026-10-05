use super::*;
use khive_runtime::curation::{ContentMergeStrategy, EntityDedupMergePolicy, NotePatch};
use khive_runtime::{Namespace, VerbRegistryBuilder};
use khive_storage::note::Note;
use khive_storage::EdgeRelation;
use serde_json::json;

struct Fixture {
    runtime: KhiveRuntime,
    token: NamespaceToken,
    project: Uuid,
}

impl Fixture {
    async fn new() -> Self {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = runtime.authorize(Namespace::local()).unwrap();
        let mut builder = VerbRegistryBuilder::new();
        builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
        builder.register(crate::GitPack::new(runtime.clone()));
        builder.with_runtime_event_store(&runtime).unwrap();
        let registry = builder.build().unwrap();
        runtime.install_edge_rules(registry.all_edge_rules());
        runtime.install_kind_registry(
            registry
                .all_entity_kinds()
                .into_iter()
                .map(str::to_owned)
                .collect(),
            registry
                .all_note_kinds()
                .into_iter()
                .map(str::to_owned)
                .collect(),
        );
        runtime.install_pack_owned_note_kinds(
            registry
                .pack_owned_note_kinds()
                .into_iter()
                .map(str::to_owned)
                .collect(),
        );
        registry.apply_schema_plans(runtime.backend());
        let project = runtime
            .create_entity(&token, "project", None, "repository", None, None, vec![])
            .await
            .unwrap()
            .id;
        Self {
            runtime,
            token,
            project,
        }
    }

    async fn project(&self, token: &NamespaceToken) -> Uuid {
        self.runtime
            .create_entity(token, "project", None, "repository", None, None, vec![])
            .await
            .unwrap()
            .id
    }

    async fn note(&self, project: Uuid, name: &str, content: &str, properties: Value) -> Note {
        self.note_in(
            &self.token,
            "pull_request",
            project,
            name,
            content,
            properties,
        )
        .await
    }

    async fn note_in(
        &self,
        token: &NamespaceToken,
        kind: &str,
        project: Uuid,
        name: &str,
        content: &str,
        mut properties: Value,
    ) -> Note {
        properties["project_id"] = json!(project.to_string());
        let note = self
            .runtime
            .create_note(
                token,
                kind,
                Some(name),
                content,
                None,
                Some(properties),
                vec![project],
            )
            .await
            .unwrap();
        self.current_in(token, note.id).await
    }

    async fn current_in(&self, token: &NamespaceToken, id: Uuid) -> Note {
        self.runtime
            .notes(token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap()
    }

    async fn current(&self, id: Uuid) -> Note {
        self.current_in(&self.token, id).await
    }

    async fn plan(&self) -> DedupPlan {
        plan_dedup(&self.runtime, &self.token, self.project)
            .await
            .unwrap()
    }

    async fn query(&self, sql: &str, params: Vec<SqlValue>) -> Vec<khive_storage::types::SqlRow> {
        self.runtime
            .sql()
            .reader()
            .await
            .unwrap()
            .query_all(SqlStatement {
                sql: sql.into(),
                params,
                label: Some("dedup_fixture_read".into()),
            })
            .await
            .unwrap()
    }

    async fn snapshot(&self) -> Value {
        let mut snapshot = serde_json::Map::new();
        for table in ["notes", "graph_edges", "events"] {
            snapshot.insert(
                table.into(),
                serde_json::to_value(
                    self.query(&format!("SELECT * FROM {table} ORDER BY id"), vec![])
                        .await,
                )
                .unwrap(),
            );
        }
        Value::Object(snapshot)
    }

    async fn merge_count(&self) -> i64 {
        self.query(
            "SELECT COUNT(*) AS count FROM events WHERE kind='note_merged'",
            vec![],
        )
        .await[0]
            .i64("count")
            .unwrap()
    }
}

#[tokio::test]
async fn real_anchor_merge_preview_apply_preserves_bodies_edges_history_and_lookup() {
    let f = Fixture::new().await;
    let old = f.project(&f.token).await;
    let url = "https://github.com/example/repository/pull/17";
    let into = f
        .note(
            old,
            "Detailed title",
            "Detailed body with the original explanation",
            json!({"number":17,"extra":"retained"}),
        )
        .await;
    let donor = f
        .note(
            f.project,
            "[pull_request]",
            "donor body",
            json!({"number":17,"url":url}),
        )
        .await;
    let third = f
        .note(
            f.project,
            "Second title",
            "third body",
            json!({"number":17}),
        )
        .await;
    let placeholder = f
        .note(
            old,
            "[pull_request]",
            "placeholder body",
            json!({"url":url}),
        )
        .await;
    let annotation = f
        .runtime
        .create_note(
            &f.token,
            "observation",
            None,
            "edge provenance",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let target = f
        .runtime
        .create_note(
            &f.token,
            "insight",
            None,
            "linked claim",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let outgoing = f
        .runtime
        .link(
            &f.token,
            donor.id,
            target.id,
            EdgeRelation::Supports,
            0.7,
            Some(json!({"retained":"metadata"})),
        )
        .await
        .unwrap();
    let incoming = f
        .runtime
        .link(
            &f.token,
            target.id,
            donor.id,
            EdgeRelation::Refutes,
            0.8,
            None,
        )
        .await
        .unwrap();
    let edge_annotation = f
        .runtime
        .link(
            &f.token,
            annotation.id,
            outgoing.id,
            EdgeRelation::Annotates,
            1.0,
            None,
        )
        .await
        .unwrap();
    f.runtime
        .merge_entity(
            &f.token,
            f.project,
            old,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();
    let before = f.snapshot().await;
    let plan = f.plan().await;
    let preview = serde_json::to_value(plan.report()).unwrap();
    assert_eq!(
        preview,
        serde_json::to_value(f.plan().await.report()).unwrap()
    );
    assert_eq!(
        f.snapshot().await,
        before,
        "preview must not mutate notes, edges or events"
    );
    assert_eq!(plan.report().planned.len(), 3);
    assert!(plan
        .report()
        .planned
        .iter()
        .all(|pair| pair.into_id == into.id));
    assert_eq!(
        plan.report().planned[0].from_id,
        donor.id,
        "URL donor must precede placeholder"
    );
    assert_eq!(plan.report().planned[2].from_id, placeholder.id);
    let preview_id = plan.report().preview_id.clone();
    let report = apply_dedup(&f.runtime, &f.token, plan).await.unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.preview_id, preview_id);
    assert_eq!(report.applied.len(), 3);
    assert!(report
        .applied
        .windows(2)
        .all(|pair| pair[1].kept_version > pair[0].kept_version));
    assert_eq!(f.merge_count().await, 3);
    let survivor = f.current(into.id).await;
    assert_eq!(survivor.version, report.applied[2].kept_version);
    for content in [
        &into.content,
        &donor.content,
        &third.content,
        &placeholder.content,
    ] {
        assert!(survivor.content.contains(content), "missing distinct body");
    }
    let properties = survivor.properties.unwrap();
    assert_eq!(properties["project_id"], f.project.to_string());
    assert_eq!(properties["number"], 17);
    assert_eq!(properties["url"], url);
    let history = properties["_merge_history"].as_array().unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(
        history[0]["git_note_repair"]["into_project_id"],
        old.to_string()
    );
    for note in [&donor, &third, &placeholder] {
        assert!(history
            .iter()
            .any(|entry| entry["merged_from"] == note.id.to_string()));
        let row = f
            .query(
                "SELECT content, properties, deleted_at FROM notes WHERE id=?1",
                vec![SqlValue::Text(note.id.to_string())],
            )
            .await;
        assert_eq!(row[0].text("content").unwrap(), note.content);
        assert_eq!(
            serde_json::from_str::<Value>(row[0].text("properties").unwrap()).unwrap(),
            note.properties.clone().unwrap()
        );
        assert!(row[0].i64("deleted_at").is_ok());
    }
    for (edge, source, target) in [
        (outgoing.id, into.id, target.id),
        (incoming.id, target.id, into.id),
        (edge_annotation.id, annotation.id, outgoing.id),
    ] {
        let row = f
            .query(
                "SELECT source_id, target_id FROM graph_edges WHERE id=?1 AND deleted_at IS NULL",
                vec![SqlValue::Text(edge.to_string())],
            )
            .await;
        assert_eq!(row.len(), 1);
        assert_eq!(row[0].text("source_id").unwrap(), source.to_string());
        assert_eq!(row[0].text("target_id").unwrap(), target.to_string());
    }
    let lookup = f
        .query(
            crate::sql::sql!("notes_by_number_select"),
            vec![
                SqlValue::Text("pull_request".into()),
                SqlValue::Text("local".into()),
                SqlValue::Integer(17),
                SqlValue::Text(f.project.to_string()),
            ],
        )
        .await;
    assert_eq!(lookup.len(), 1);
    assert_eq!(lookup[0].text("id").unwrap(), into.id.to_string());
    let after = f.snapshot().await;
    let repeat = run_dedup(
        &f.runtime,
        &f.token,
        DedupOptions {
            project_id: f.project,
            apply: true,
        },
    )
    .await
    .unwrap();
    assert!(repeat.success);
    assert!(repeat.planned.is_empty());
    assert!(repeat.applied.is_empty());
    assert_eq!(
        f.snapshot().await,
        after,
        "second apply must emit no events or writes"
    );
}

#[tokio::test]
async fn census_keeps_other_repositories_kinds_namespaces_and_unproven_notes_separate() {
    let f = Fixture::new().await;
    let into = f
        .note(
            f.project,
            "real title",
            "long canonical description",
            json!({"number":17}),
        )
        .await;
    let duplicate = f
        .note(f.project, "[pull_request]", "short", json!({"number":17}))
        .await;
    let other = f.project(&f.token).await;
    let foreign_token = f
        .runtime
        .authorize(Namespace::parse("actor:foreign").unwrap())
        .unwrap();
    let foreign_project = f.project(&foreign_token).await;
    let separate_repo = f
        .note(
            other,
            "same number",
            "other repository",
            json!({"number":17}),
        )
        .await;
    let separate_kind = f
        .note_in(
            &f.token,
            "issue",
            f.project,
            "same number",
            "other kind",
            json!({"number":17}),
        )
        .await;
    let separate_ns = f
        .note_in(
            &foreign_token,
            "pull_request",
            foreign_project,
            "same number",
            "other namespace",
            json!({"number":17}),
        )
        .await;
    let missing = f
        .runtime
        .create_note(
            &f.token,
            "pull_request",
            Some("missing project"),
            "unchanged",
            None,
            Some(json!({"number":17})),
            vec![f.project],
        )
        .await
        .unwrap();
    let malformed = f
        .note(
            f.project,
            "[pull_request]",
            "unchanged malformed",
            json!({"number":"17", "url":"https://github.com/example/repository/pull/17"}),
        )
        .await;
    let ambiguous = f
        .note(
            f.project,
            "multiple projects",
            "unchanged ambiguity",
            json!({"number":17}),
        )
        .await;
    f.runtime
        .link(
            &f.token,
            ambiguous.id,
            foreign_project,
            EdgeRelation::Annotates,
            1.0,
            None,
        )
        .await
        .unwrap();
    let unrelated = f
        .note(
            other,
            "same displayed project name",
            "unmerged anchor",
            json!({"number":17}),
        )
        .await;
    f.runtime
        .link(
            &f.token,
            unrelated.id,
            f.project,
            EdgeRelation::Annotates,
            1.0,
            None,
        )
        .await
        .unwrap();
    let unannotated = f
        .runtime
        .create_note(
            &f.token,
            "pull_request",
            Some("missing edge"),
            "unchanged missing edge",
            None,
            Some(json!({"number":17,"project_id":f.project.to_string()})),
            vec![],
        )
        .await
        .unwrap();
    let untouched = vec![
        separate_repo,
        separate_kind,
        separate_ns,
        f.current(missing.id).await,
        malformed,
        ambiguous,
        unrelated,
        f.current(unannotated.id).await,
    ];
    let plan = f.plan().await;
    assert_eq!(
        plan.report().planned.len(),
        1,
        "canonical project, kind and namespace must not widen"
    );
    assert_eq!(plan.report().planned[0].into_id, into.id);
    assert_eq!(plan.report().planned[0].from_id, duplicate.id);
    for id in [untouched[0].id, untouched[2].id] {
        assert!(
            !plan
                .report()
                .planned
                .iter()
                .any(|pair| pair.into_id == id || pair.from_id == id),
            "unrelated repository or namespace entered planned census"
        );
        assert!(
            !plan.report().unchanged.iter().any(|row| row.id == id),
            "unrelated repository or namespace entered reported census"
        );
    }
    for id in [
        missing.id,
        untouched[4].id,
        untouched[5].id,
        untouched[6].id,
        unannotated.id,
    ] {
        assert!(plan.report().unchanged.iter().any(|note| note.id == id));
    }
    let report = apply_dedup(&f.runtime, &f.token, plan).await.unwrap();
    assert!(report.success);
    assert_eq!(report.applied.len(), 1);
    for note in untouched {
        let token = f
            .runtime
            .authorize(Namespace::parse(&note.namespace).unwrap())
            .unwrap();
        assert_eq!(f.current_in(&token, note.id).await, note);
    }
}

#[tokio::test]
async fn placeholder_urls_must_be_unique_exact_and_preservable() {
    let f = Fixture::new().await;
    let url = "https://github.com/example/repository/pull/17";
    let into = f
        .note(
            f.project,
            "real title",
            "number seventeen",
            json!({"number":17,"url":url}),
        )
        .await;
    f.note(
        f.project,
        "other number",
        "number eighteen",
        json!({"number":18,"url":url}),
    )
    .await;
    let ambiguous = f
        .note(
            f.project,
            "[pull_request]",
            "ambiguous URL",
            json!({"url":url}),
        )
        .await;
    let not_placeholder = f
        .note(
            f.project,
            "arbitrary title",
            "not a placeholder",
            json!({"url":url}),
        )
        .await;
    let null_url = "https://github.com/example/repository/pull/19";
    let null_survivor = f
        .note(
            f.project,
            "rich title",
            "longest nineteen description survives intact",
            json!({"number":19,"url":null}),
        )
        .await;
    f.note(
        f.project,
        "[pull_request]",
        "donor",
        json!({"number":19,"url":null_url}),
    )
    .await;
    let cannot_retain = f
        .note(
            f.project,
            "[pull_request]",
            "not overwriting null",
            json!({"url":null_url}),
        )
        .await;
    let mixed_a = "https://github.com/example/repository/pull/20";
    let mixed_b = "https://git.example.org/repository/pull/20";
    let mixed_survivor = f
        .note(
            f.project,
            "rich title",
            "longest twenty description",
            json!({"number":20,"url":mixed_a}),
        )
        .await;
    f.note(
        f.project,
        "[pull_request]",
        "mixed donor",
        json!({"number":20,"url":mixed_b}),
    )
    .await;
    let mixed_placeholder = f
        .note(
            f.project,
            "[pull_request]",
            "do not overwrite URL",
            json!({"url":mixed_b}),
        )
        .await;
    let mut unchanged = vec![ambiguous, not_placeholder, cannot_retain, mixed_placeholder];
    for (index, bad_url) in [
        "https://github.com/example/repository/pull/17?x=1",
        "https://github.com/example/repository/pull/17#f",
        "https://user@github.com/example/repository/pull/17",
        "https://github.com/example/repository/pull/017",
        "HTTPS://github.com/example/repository/pull/17",
    ]
    .iter()
    .enumerate()
    {
        unchanged.push(
            f.note(
                f.project,
                "[pull_request]",
                &format!("unproven spelling {index}"),
                json!({"url":bad_url}),
            )
            .await,
        );
    }
    let plan = f.plan().await;
    assert_eq!(plan.report().planned.len(), 2);
    assert!(plan
        .report()
        .planned
        .iter()
        .all(|pair| pair.placeholder_url.is_none()));
    for note in &unchanged {
        assert!(plan.report().unchanged.iter().any(|row| row.id == note.id));
    }
    let report = apply_dedup(&f.runtime, &f.token, plan).await.unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(f.current(into.id).await, into);
    assert!(f.current(null_survivor.id).await.properties.unwrap()["url"].is_null());
    assert_eq!(
        f.current(mixed_survivor.id).await.properties.unwrap()["url"],
        mixed_a
    );
    for note in unchanged {
        assert_eq!(f.current(note.id).await, note);
    }
}

#[tokio::test]
async fn stale_versions_stop_the_group_without_retry_and_other_groups_can_apply() {
    let f = Fixture::new().await;
    let into = f
        .note(
            f.project,
            "real title",
            "long survivor description",
            json!({"number":17}),
        )
        .await;
    let first = f
        .note(f.project, "[pull_request]", "first", json!({"number":17}))
        .await;
    let third = f
        .note(f.project, "[pull_request]", "third", json!({"number":17}))
        .await;
    f.note(
        f.project,
        "other real title",
        "long independent group",
        json!({"number":18}),
    )
    .await;
    f.note(f.project, "[pull_request]", "short", json!({"number":18}))
        .await;
    let plan = f.plan().await;
    let changed = f
        .runtime
        .update_note_with_embedding_report(
            &f.token,
            into.id,
            NotePatch::new(
                None,
                Some("concurrent edit must survive".into()),
                None,
                None,
                None,
            ),
        )
        .await
        .unwrap()
        .0;
    let report = apply_dedup(&f.runtime, &f.token, plan).await.unwrap();
    assert!(!report.success);
    assert_eq!(
        report.refused.len(),
        2,
        "stale group must stop without retrying its next member"
    );
    assert_eq!(report.applied.len(), 1);
    assert_eq!(f.current(into.id).await, changed);
    assert_eq!(f.current(first.id).await, first);
    assert_eq!(f.current(third.id).await, third);
    assert_eq!(f.merge_count().await, 1);
}

#[tokio::test]
async fn oversized_candidate_refuses_entire_apply_without_mutation() {
    let f = Fixture::new().await;
    f.note(
        f.project,
        "real title",
        "long normal body",
        json!({"number":17}),
    )
    .await;
    f.note(f.project, "[pull_request]", "short", json!({"number":17}))
        .await;
    let oversized = f
        .note(
            f.project,
            "large note",
            &"word ".repeat(MAX_ROW_BYTES as usize / 5 + 1),
            json!({"number":18}),
        )
        .await;
    let before = f.snapshot().await;
    let plan = f.plan().await;
    assert!(!plan.report().complete_census);
    assert!(plan
        .report()
        .unchanged
        .iter()
        .any(|row| row.id == oversized.id));
    let report = apply_dedup(&f.runtime, &f.token, plan).await.unwrap();
    assert!(!report.success);
    assert!(report.applied.is_empty());
    assert_eq!(f.snapshot().await, before);
}

#[tokio::test]
async fn malformed_history_is_unchanged_but_keeps_contradictory_url_evidence() {
    let f = Fixture::new().await;
    let url = "https://github.com/example/repository/pull/17";
    let into = f
        .note(
            f.project,
            "real title",
            "number seventeen",
            json!({"number":17,"url":url}),
        )
        .await;
    let malformed = f
        .note(
            f.project,
            "other number",
            "retain damaged history",
            json!({"number":18,"url":url,"_merge_history":"broken"}),
        )
        .await;
    let placeholder = f
        .note(
            f.project,
            "[pull_request]",
            "ambiguous URL",
            json!({"url":url}),
        )
        .await;
    let before = f.snapshot().await;
    let plan = f.plan().await;
    assert!(plan.report().planned.is_empty());
    assert!(plan
        .report()
        .unchanged
        .iter()
        .any(|row| row.id == malformed.id && row.reason == "malformed merge history"));
    assert!(plan
        .report()
        .unchanged
        .iter()
        .any(|row| row.id == placeholder.id));
    let report = apply_dedup(&f.runtime, &f.token, plan).await.unwrap();
    assert!(report.success);
    assert!(report.applied.is_empty());
    assert_eq!(f.current(into.id).await, into);
    assert_eq!(f.snapshot().await, before);
}

#[tokio::test]
async fn candidate_query_bounds_rows_and_suppresses_over_total_payloads() {
    let f = Fixture::new().await;
    let mut notes = Vec::new();
    for number in 1..=3 {
        notes.push(
            f.note(
                f.project,
                "real title",
                "bounded body",
                json!({"number":number}),
            )
            .await,
        );
    }
    notes.sort_by_key(|note| note.id);
    let parameters = |limit, total| {
        vec![
            SqlValue::Text("local".into()),
            SqlValue::Text(f.project.to_string()),
            SqlValue::Text(serde_json::to_string(&[f.project.simple().to_string()]).unwrap()),
            SqlValue::Integer(limit),
            SqlValue::Integer(MAX_ROW_BYTES),
            SqlValue::Integer(total),
        ]
    };
    let query = crate::sql::sql!("dedup_candidates_select");
    let page = f.query(query, parameters(2, MAX_TOTAL_BYTES)).await;
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].text("id").unwrap(), notes[0].id.to_string());
    assert_eq!(page[1].text("id").unwrap(), notes[1].id.to_string());
    let first = &notes[0];
    let first_bytes = first.name.as_ref().unwrap().len()
        + first.content.len()
        + serde_json::to_string(first.properties.as_ref().unwrap())
            .unwrap()
            .len();
    let bounded = f.query(query, parameters(4, first_bytes as i64)).await;
    assert_eq!(bounded.len(), 3);
    assert_eq!(bounded[0].i64("within_budget").unwrap(), 1);
    for row in &bounded[1..] {
        assert_eq!(row.i64("within_budget").unwrap(), 0);
        for column in ["name", "content", "properties"] {
            assert!(
                matches!(row.get(column), Some(SqlValue::Null)),
                "oversized payload reached Rust"
            );
        }
    }
}
