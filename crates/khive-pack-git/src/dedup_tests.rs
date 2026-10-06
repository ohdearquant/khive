use super::*;
use async_trait::async_trait;
use khive_runtime::curation::NotePatch;
use khive_runtime::{EmbedderProvider, Namespace, RuntimeConfig, VerbRegistryBuilder};
use khive_storage::note::Note;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use std::sync::Arc;

const MODEL: EmbeddingModel = EmbeddingModel::BgeSmallEnV15;
const VECTOR_TABLE: &str = "vec_bge_small_en_v1_5";

struct LengthEmbedder;

#[async_trait]
impl EmbeddingService for LengthEmbedder {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|text| {
                let mut vector = vec![0.0_f32; MODEL.dimensions()];
                vector[0] = text.len() as f32;
                vector[1] = 1.0;
                vector
            })
            .collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "length-embedder"
    }
}

struct LengthProvider;

#[async_trait]
impl EmbedderProvider for LengthProvider {
    fn name(&self) -> &str {
        "bge-small-en-v1.5"
    }

    fn dimensions(&self) -> usize {
        MODEL.dimensions()
    }

    async fn build(&self) -> khive_runtime::RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(LengthEmbedder))
    }
}

struct Fixture {
    runtime: KhiveRuntime,
    token: NamespaceToken,
    canonical: Uuid,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_runtime(KhiveRuntime::memory().unwrap()).await
    }

    async fn embedding() -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            packs: vec!["kg".to_string()],
            brain_profile: None,
            actor_id: None,
            embedding_model: Some(MODEL),
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        runtime.register_embedder(LengthProvider);
        Self::with_runtime(runtime).await
    }

    async fn with_runtime(runtime: KhiveRuntime) -> Self {
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
        let mut fixture = Self {
            runtime,
            token,
            canonical: Uuid::nil(),
        };
        fixture.canonical = fixture.project().await;
        fixture
    }

    async fn project(&self) -> Uuid {
        self.runtime
            .create_entity_with_embedding_report(
                &self.token,
                "project",
                None,
                "repository",
                None,
                None,
                vec![],
            )
            .await
            .unwrap()
            .0
            .id
    }

    /// A repository anchor that will be merged into the canonical project once
    /// its notes exist.
    async fn anchor(&self) -> Uuid {
        self.project().await
    }

    async fn retire(&self, anchor: Uuid) {
        self.runtime
            .merge_entity(
                &self.token,
                self.canonical,
                anchor,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
            )
            .await
            .unwrap();
    }

    async fn note_of(
        &self,
        kind: &str,
        project: Uuid,
        name: Option<&str>,
        content: &str,
        properties: Value,
    ) -> Note {
        let mut properties = properties;
        properties["project_id"] = json!(project.to_string());
        let note = self
            .runtime
            .create_note(
                &self.token,
                kind,
                name,
                content,
                None,
                Some(properties),
                vec![project],
            )
            .await
            .unwrap();
        self.current(note.id).await
    }

    async fn pr(
        &self,
        project: Uuid,
        name: Option<&str>,
        number: i64,
        title: &str,
        content: &str,
    ) -> Note {
        self.note_of(
            "pull_request",
            project,
            name,
            content,
            json!({"number": number, "title": title}),
        )
        .await
    }

    async fn current(&self, id: Uuid) -> Note {
        self.runtime
            .notes(&self.token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap()
    }

    async fn is_deleted(&self, id: Uuid) -> bool {
        self.query(
            "SELECT deleted_at FROM notes WHERE id = ?1",
            vec![SqlValue::Text(id.to_string())],
        )
        .await[0]
            .get("deleted_at")
            .is_some_and(|value| !matches!(value, SqlValue::Null))
    }

    fn options(&self) -> DedupOptions {
        DedupOptions {
            project_id: self.canonical,
            refused_anchors: BTreeSet::new(),
            apply: false,
        }
    }

    async fn plan(&self) -> DedupPlan {
        plan_dedup(&self.runtime, &self.token, self.options())
            .await
            .unwrap()
    }

    async fn apply(&self) -> DedupReport {
        let plan = self.plan().await;
        apply_dedup(&self.runtime, &self.token, plan).await.unwrap()
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

    async fn execute(&self, sql: &str, params: Vec<SqlValue>) {
        self.runtime
            .sql()
            .writer()
            .await
            .unwrap()
            .execute(SqlStatement {
                sql: sql.into(),
                params,
                label: Some("dedup_fixture_write".into()),
            })
            .await
            .unwrap();
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

    async fn merge_events(&self) -> i64 {
        self.query(
            "SELECT COUNT(*) AS count FROM events WHERE kind='note_merged'",
            vec![],
        )
        .await[0]
            .i64("count")
            .unwrap()
    }

    async fn edit_content(&self, id: Uuid, content: &str) {
        self.runtime
            .update_note_with_embedding_report(
                &self.token,
                id,
                NotePatch::new(None, Some(content.into()), None, None, None),
            )
            .await
            .unwrap();
    }
}

fn property(note: &Note, key: &str) -> Value {
    note.properties
        .as_ref()
        .and_then(|properties| properties.get(key))
        .cloned()
        .unwrap_or(Value::Null)
}

fn unchanged_count(report: &DedupReport, reason: &str) -> usize {
    report
        .unchanged
        .iter()
        .filter(|row| row.reason == reason)
        .map(|row| row.notes)
        .sum()
}

#[tokio::test]
async fn stranded_duplicates_merge_into_one_note_that_keeps_the_twins_name() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    let other = f.anchor().await;
    let unnamed = f.pr(old, None, 7, "Fix the parser", "same body").await;
    let named = f
        .pr(
            other,
            Some("#7 Fix the parser"),
            7,
            "Fix the parser",
            "same body",
        )
        .await;
    f.retire(old).await;
    f.retire(other).await;

    let report = f.apply().await;

    assert!(report.success, "{:?}", report.refused_merges);
    assert_eq!(report.planned.len(), 1);
    assert_eq!(report.applied.len(), 1);
    let survivor = f.current(report.planned[0].survivor.id).await;
    let donor_id = if survivor.id == unnamed.id {
        named.id
    } else {
        unnamed.id
    };
    assert_eq!(survivor.name.as_deref(), Some("#7 Fix the parser"));
    assert_eq!(
        property(&survivor, "project_id"),
        json!(f.canonical.to_string())
    );
    assert!(f.is_deleted(donor_id).await);
}

#[tokio::test]
async fn a_survivor_without_a_name_takes_its_twins_name_and_keeps_the_canonical_home() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    let unnamed = f
        .pr(f.canonical, None, 9, "Add the cache", "cache body")
        .await;
    let named = f
        .pr(
            old,
            Some("#9 Add the cache"),
            9,
            "Add the cache",
            "cache body",
        )
        .await;
    f.retire(old).await;

    let plan = f.plan().await;
    let group = &plan.report().planned[0];
    assert_eq!(group.survivor.id, unnamed.id, "the canonical home wins");
    assert_eq!(group.renamed_to.as_deref(), Some("#9 Add the cache"));
    assert_eq!(group.rehomed_from, None);
    assert_eq!(plan.report().totals["pull_request"].survivors_renamed, 1);
    assert_eq!(plan.report().totals["pull_request"].notes_rehomed, 0);

    let report = f.apply().await;

    assert!(report.success);
    let survivor = f.current(unnamed.id).await;
    assert_eq!(survivor.name.as_deref(), Some("#9 Add the cache"));
    assert!(f.is_deleted(named.id).await);
}

#[tokio::test]
async fn a_survivor_under_a_retired_anchor_is_rehomed_to_the_canonical_project() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    let other = f.anchor().await;
    f.pr(old, Some("#3 t"), 3, "t", "body").await;
    f.pr(other, Some("#3 t"), 3, "t", "body").await;
    f.retire(old).await;
    f.retire(other).await;

    let plan = f.plan().await;
    let group = plan.report().planned[0].clone();
    assert!(group.rehomed_from.is_some());
    assert_eq!(plan.report().totals["pull_request"].notes_rehomed, 1);

    f.apply().await;

    let survivor = f.current(group.survivor.id).await;
    assert_eq!(
        property(&survivor, "project_id"),
        json!(f.canonical.to_string())
    );
    let history = property(&survivor, "_merge_history");
    assert_eq!(
        history[0]["annotation"]["from_project_id"],
        json!(group.donors[0].note.project_id.to_string())
    );
    assert_eq!(
        history[0]["annotation"]["into_project_id"],
        json!(group.survivor.project_id.to_string())
    );
}

#[tokio::test]
async fn titles_that_differ_refuse_the_group_and_name_both_titles() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    f.pr(old, Some("#4 First"), 4, "First", "a").await;
    f.pr(old, Some("#4 Second"), 4, "Second", "b").await;
    f.retire(old).await;
    let before = f.snapshot().await;

    let report = f.apply().await;

    assert!(report.planned.is_empty());
    assert_eq!(report.refused_groups.len(), 1);
    let reason = &report.refused_groups[0].reason;
    assert!(
        reason.contains("\"First\"") && reason.contains("\"Second\""),
        "{reason}"
    );
    let titles: BTreeSet<_> = report.refused_groups[0]
        .notes
        .iter()
        .filter_map(|note| note.title.clone())
        .collect();
    assert_eq!(titles, BTreeSet::from(["First".into(), "Second".into()]));
    assert_eq!(
        f.snapshot().await,
        before,
        "a refused group changes nothing"
    );
}

#[tokio::test]
async fn a_note_without_a_title_refuses_the_group() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    f.pr(old, Some("#5 T"), 5, "T", "a").await;
    f.note_of("pull_request", old, Some("#5"), "b", json!({"number": 5}))
        .await;
    f.retire(old).await;

    let report = f.plan().await.report().clone();

    assert_eq!(report.refused_groups[0].reason, "a note has no title");
}

const URL_REASON: &str = "urls differ or are missing on some notes";

/// Two same-numbered notes whose stored urls are `urls`, planned in one project.
async fn plan_with_urls(urls: [Option<&str>; 2]) -> DedupReport {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    for url in urls {
        let mut properties = json!({"number": 1, "title": "t"});
        if let Some(url) = url {
            properties["url"] = json!(url);
        }
        f.note_of("pull_request", old, Some("t"), "body", properties)
            .await;
    }
    f.retire(old).await;
    f.plan().await.report().clone()
}

#[tokio::test]
async fn two_different_non_empty_urls_refuse_the_group() {
    let report = plan_with_urls([
        Some("https://github.com/a/b/pull/1"),
        Some("https://git.example.org/b/pull/1"),
    ])
    .await;
    assert!(report.planned.is_empty());
    assert_eq!(report.refused_groups.len(), 1);
    assert_eq!(report.refused_groups[0].reason, URL_REASON);
}

#[tokio::test]
async fn an_empty_url_beside_a_non_empty_one_is_not_proof_and_refuses_the_group() {
    for empty in [None, Some(""), Some("  ")] {
        let report = plan_with_urls([Some("https://github.com/a/b/pull/1"), empty]).await;
        assert!(report.planned.is_empty(), "{empty:?}");
        assert_eq!(report.refused_groups.len(), 1, "{empty:?}");
        assert_eq!(report.refused_groups[0].reason, URL_REASON, "{empty:?}");
    }
}

#[tokio::test]
async fn equal_urls_and_two_missing_urls_do_not_block_the_merge() {
    let same = "https://github.com/a/b/pull/1";
    for urls in [[Some(same), Some(same)], [None, Some("")]] {
        let report = plan_with_urls(urls).await;
        assert!(report.refused_groups.is_empty(), "{urls:?}");
        assert_eq!(report.planned.len(), 1, "{urls:?}");
    }
}

#[tokio::test]
async fn a_refused_anchor_keeps_its_notes_out_of_every_merge() {
    let f = Fixture::new().await;
    let kept = f.anchor().await;
    let old = f.anchor().await;
    let other = f.anchor().await;
    let guarded = f.pr(kept, Some("#8 t"), 8, "t", "body").await;
    f.pr(old, Some("#8 t"), 8, "t", "body").await;
    f.pr(other, Some("#8 t"), 8, "t", "body").await;
    f.pr(kept, Some("#9 t"), 9, "t", "body").await;
    f.pr(old, Some("#9 t"), 9, "t", "body").await;
    for anchor in [kept, old, other] {
        f.retire(anchor).await;
    }
    let mut options = f.options();
    options.refused_anchors.insert(kept);

    let plan = plan_dedup(&f.runtime, &f.token, options).await.unwrap();
    let report = apply_dedup(&f.runtime, &f.token, plan).await.unwrap();

    assert!(report.success);
    assert_eq!(report.refused_anchors, vec![kept]);
    assert_eq!(report.planned.len(), 1, "only #8 keeps two eligible notes");
    assert_eq!(report.planned[0].number, 8);
    assert_eq!(f.current(guarded.id).await, guarded);
    assert_eq!(
        unchanged_count(&report, "note belongs to a refused anchor"),
        2
    );
    assert_eq!(
        unchanged_count(&report, "no duplicate in the group"),
        1,
        "#9 has one eligible note left"
    );
}

#[tokio::test]
async fn refusing_the_canonical_anchor_prevents_preview_and_apply() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    let other = f.anchor().await;
    let canonical_note = f.pr(f.canonical, Some("#8 t"), 8, "t", "body").await;
    let old_note = f.pr(old, Some("#8 t"), 8, "t", "body").await;
    let other_note = f.pr(other, Some("#8 t"), 8, "t", "body").await;
    f.retire(old).await;
    f.retire(other).await;
    let ordinary = f.plan().await;
    assert_eq!(ordinary.report().planned.len(), 1);
    assert_eq!(ordinary.report().planned[0].survivor.id, canonical_note.id);
    assert_eq!(ordinary.report().planned[0].donors.len(), 2);
    let before = f.snapshot().await;

    for apply in [false, true] {
        let mut options = f.options();
        options.refused_anchors.insert(f.canonical);
        options.apply = apply;
        let error = match run_dedup(&f.runtime, &f.token, options).await {
            Ok(_) => panic!("canonical anchor must be refused before planning: apply={apply}"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            format!("cannot refuse canonical project anchor {}", f.canonical)
        );
        assert_eq!(f.snapshot().await, before, "apply={apply}");
        let mut canonical_live = 0;
        for id in [canonical_note.id, old_note.id, other_note.id] {
            assert!(!f.is_deleted(id).await);
            let current = f.current(id).await;
            canonical_live +=
                usize::from(property(&current, "project_id") == json!(f.canonical.to_string()));
        }
        assert_eq!(canonical_live, 1, "apply={apply}");
    }
}

#[tokio::test]
async fn notes_without_proof_or_a_number_are_left_alone() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    let second_project = f.project().await;
    f.pr(old, Some("#1 t"), 1, "t", "body").await;
    f.pr(old, Some("#1 t"), 1, "t", "body").await;
    // The note's project_id is in the lineage, but its annotation is gone.
    let unannotated = f.pr(old, Some("#2 t"), 2, "t", "body").await;
    // Annotates the canonical project and a second live project.
    let ambiguous = f.pr(old, Some("#3 t"), 3, "t", "body").await;
    f.runtime
        .link(
            &f.token,
            ambiguous.id,
            second_project,
            EdgeRelation::Annotates,
            1.0,
            None,
        )
        .await
        .unwrap();
    // No usable number.
    f.pr(old, Some("zero"), 0, "t", "body").await;
    f.note_of(
        "pull_request",
        old,
        Some("text"),
        "b",
        json!({"number": "12"}),
    )
    .await;
    f.retire(old).await;
    f.execute(
        "DELETE FROM graph_edges WHERE source_id = ?1 AND relation = 'annotates'",
        vec![SqlValue::Text(unannotated.id.to_string())],
    )
    .await;

    let report = f.plan().await.report().clone();

    assert_eq!(
        unchanged_count(&report, "no live annotation to the canonical project"),
        1
    );
    assert_eq!(
        unchanged_count(&report, "also annotates another live project"),
        1
    );
    assert_eq!(unchanged_count(&report, "no positive integer number"), 2);
    assert_eq!(
        report.planned.iter().map(|g| g.number).collect::<Vec<_>>(),
        vec![1]
    );
}

#[tokio::test]
async fn a_note_annotating_the_canonical_project_from_an_unrelated_project_is_left_alone() {
    let f = Fixture::new().await;
    let stray = f.project().await;
    let note = f
        .note_of(
            "pull_request",
            stray,
            Some("#6 t"),
            "body",
            json!({"number": 6, "title": "t"}),
        )
        .await;
    f.runtime
        .link(
            &f.token,
            note.id,
            f.canonical,
            EdgeRelation::Annotates,
            1.0,
            None,
        )
        .await
        .unwrap();

    let report = f.plan().await.report().clone();

    assert_eq!(
        unchanged_count(
            &report,
            "project_id is not in the canonical project's lineage"
        ),
        1
    );
    assert!(report.planned.is_empty());
}

#[tokio::test]
async fn issues_and_pull_requests_with_one_number_are_separate_groups() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    f.pr(old, Some("#5 t"), 5, "t", "body").await;
    f.note_of(
        "issue",
        old,
        Some("#5 t"),
        "body",
        json!({"number": 5, "title": "t"}),
    )
    .await;
    f.retire(old).await;

    let report = f.plan().await.report().clone();

    assert!(report.planned.is_empty());
    assert_eq!(report.totals["issue"].candidates, 1);
    assert_eq!(report.totals["pull_request"].candidates, 1);
}

#[tokio::test]
async fn equal_bodies_are_kept_once_and_distinct_bodies_are_all_retained() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    let a = f.pr(old, Some("#1 t"), 1, "t", "alpha").await;
    f.pr(old, Some("#1 t"), 1, "t", "alpha").await;
    f.pr(old, Some("#2 t"), 2, "t", "alpha").await;
    f.pr(old, Some("#2 t"), 2, "t", "beta").await;
    f.pr(old, Some("#2 t"), 2, "t", "alpha").await;
    f.retire(old).await;

    let report = f.apply().await;

    assert!(report.success, "{:?}", report.refused_merges);
    let by_number = |number: i64| {
        report
            .planned
            .iter()
            .find(|group| group.number == number)
            .unwrap()
    };
    assert_eq!(by_number(1).donors[0].body, BodyMerge::KeepSurvivor);
    assert_eq!(f.current(by_number(1).survivor.id).await.content, "alpha");
    let two = by_number(2);
    let bodies: Vec<_> = two.donors.iter().map(|d| d.body).collect();
    assert_eq!(
        bodies.iter().filter(|b| **b == BodyMerge::Append).count(),
        1
    );
    let merged = f.current(two.survivor.id).await.content;
    assert_eq!(merged.matches("alpha").count(), 1);
    assert_eq!(merged.matches("beta").count(), 1);
    assert!(merged.contains("\n\n---\n\n"));
    let _ = a;
}

#[tokio::test]
async fn a_stale_donor_stops_its_group_and_leaves_other_groups_to_apply() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    let survivor = f
        .note_of(
            "pull_request",
            old,
            Some("#1 t"),
            "body",
            json!({"number": 1, "title": "t", "a": 1, "b": 2, "c": 3}),
        )
        .await;
    let first = f
        .note_of(
            "pull_request",
            old,
            Some("#1 t"),
            "first",
            json!({"number": 1, "title": "t", "a": 1, "b": 2}),
        )
        .await;
    let second = f
        .note_of(
            "pull_request",
            old,
            Some("#1 t"),
            "second",
            json!({"number": 1, "title": "t", "a": 1}),
        )
        .await;
    f.pr(old, Some("#2 t"), 2, "t", "body").await;
    f.pr(old, Some("#2 t"), 2, "t", "body").await;
    f.retire(old).await;
    let plan = f.plan().await;
    assert_eq!(plan.report().planned[0].survivor.id, survivor.id);
    assert_eq!(plan.report().planned[0].donors[0].note.id, first.id);
    f.edit_content(first.id, "edited after planning").await;

    let report = apply_dedup(&f.runtime, &f.token, plan).await.unwrap();

    assert!(!report.success);
    assert_eq!(report.applied.len(), 1, "the independent group applies");
    assert_eq!(report.refused_merges.len(), 2);
    assert!(report.refused_merges[1]
        .reason
        .contains("group stopped after an earlier merge refusal"));
    assert_eq!(f.current(second.id).await, second);
    assert_eq!(f.current(survivor.id).await, survivor);
    assert_eq!(f.merge_events().await, 1);
}

#[tokio::test]
async fn a_survivor_edited_after_planning_refuses_every_merge_of_its_group() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    let survivor = f.pr(old, Some("#1 t"), 1, "t", "body long").await;
    f.pr(old, Some("#1 t"), 1, "t", "x").await;
    f.retire(old).await;
    let plan = f.plan().await;
    let survivor_id = plan.report().planned[0].survivor.id;
    assert_eq!(survivor_id, survivor.id);
    f.edit_content(survivor_id, "changed").await;

    let report = apply_dedup(&f.runtime, &f.token, plan).await.unwrap();

    assert!(report.applied.is_empty());
    assert_eq!(report.refused_merges.len(), 1);
    let reason = &report.refused_merges[0].reason;
    assert!(
        reason.contains(&format!("note {survivor_id} changed concurrently")),
        "{reason}"
    );
    assert_eq!(f.merge_events().await, 0);
}

async fn refused_after(f: &Fixture, change: impl AsyncFnOnce(&Fixture, &Note, &Note)) -> String {
    let old = f.anchor().await;
    let a = f.pr(old, Some("#1 t"), 1, "t", "alpha").await;
    let b = f.pr(old, Some("#1 t"), 1, "t", "beta").await;
    f.retire(old).await;
    let plan = f.plan().await;
    let group = plan.report().planned[0].clone();
    let (survivor, donor) = if group.survivor.id == a.id {
        (a, b)
    } else {
        (b, a)
    };
    change(f, &survivor, &donor).await;
    let before = f.merge_events().await;
    let report = apply_dedup(&f.runtime, &f.token, plan).await.unwrap();
    assert!(report.applied.is_empty(), "{:?}", report.applied.len());
    assert_eq!(f.merge_events().await, before);
    report.refused_merges[0].reason.clone()
}

#[tokio::test]
async fn losing_the_canonical_annotation_after_planning_refuses_the_merge() {
    let f = Fixture::new().await;
    let reason = refused_after(&f, async |f, _survivor, donor| {
        f.execute(
            "UPDATE graph_edges SET deleted_at = 1 WHERE source_id = ?1 AND relation = 'annotates'",
            vec![SqlValue::Text(donor.id.to_string())],
        )
        .await;
    })
    .await;
    assert!(reason.contains("NoteEdgeTo"), "{reason}");
}

#[tokio::test]
async fn gaining_a_second_project_annotation_after_planning_refuses_the_merge() {
    let f = Fixture::new().await;
    let reason = refused_after(&f, async |f, survivor, _donor| {
        let other = f.project().await;
        f.runtime
            .link(
                &f.token,
                survivor.id,
                other,
                EdgeRelation::Annotates,
                1.0,
                None,
            )
            .await
            .unwrap();
    })
    .await;
    assert!(reason.contains("NoteEdgeTargetsWithin"), "{reason}");
}

#[tokio::test]
async fn a_broken_anchor_lineage_after_planning_refuses_the_merge() {
    let f = Fixture::new().await;
    let reason = refused_after(&f, async |f, _survivor, _donor| {
        f.execute(
            "UPDATE entities SET merged_into = NULL, version = version + 1 \
             WHERE deleted_at IS NOT NULL AND kind = 'project'",
            vec![],
        )
        .await;
    })
    .await;
    assert!(reason.contains("EntityLineageReaches"), "{reason}");
}

#[tokio::test]
async fn the_merge_embeds_the_survivor_and_drops_the_tombstoned_vector() {
    let f = Fixture::embedding().await;
    let old = f.anchor().await;
    let a = f.pr(old, Some("#1 t"), 1, "t", "short").await;
    let b = f
        .pr(old, Some("#1 t"), 1, "t", "a considerably longer body")
        .await;
    f.retire(old).await;
    let vector = |id: Uuid| {
        let f = &f;
        async move {
            f.query(
                &format!("SELECT embedding FROM {VECTOR_TABLE} WHERE subject_id = ?1"),
                vec![SqlValue::Text(id.to_string())],
            )
            .await
            .iter()
            .map(|row| format!("{:?}", row.get("embedding")))
            .collect::<Vec<_>>()
        }
    };
    let (before_a, before_b) = (vector(a.id).await, vector(b.id).await);
    assert_eq!((before_a.len(), before_b.len()), (1, 1));

    let report = f.apply().await;

    assert!(report.success, "{:?}", report.refused_merges);
    assert_eq!(report.applied[0].summary.post_commit_reindex_error, None);
    let group = &report.planned[0];
    let (kept, gone, before_kept) = if group.survivor.id == a.id {
        (a.id, b.id, before_a)
    } else {
        (b.id, a.id, before_b)
    };
    let after = vector(kept).await;
    assert_eq!(after.len(), 1);
    assert_ne!(
        after, before_kept,
        "the survivor's vector reflects the merged body"
    );
    assert!(vector(gone).await.is_empty());
}

#[tokio::test]
async fn preview_changes_nothing_and_a_second_apply_finds_nothing_to_do() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    f.pr(old, Some("#1 t"), 1, "t", "alpha").await;
    f.pr(old, Some("#1 t"), 1, "t", "beta").await;
    f.pr(old, Some("#2 t"), 2, "t", "only one").await;
    f.retire(old).await;
    let before = f.snapshot().await;

    let preview = run_dedup(&f.runtime, &f.token, f.options()).await.unwrap();
    assert!(!preview.apply_requested);
    assert_eq!(preview.planned.len(), 1);
    assert_eq!(f.snapshot().await, before, "a preview writes nothing");

    let mut options = f.options();
    options.apply = true;
    let first = run_dedup(&f.runtime, &f.token, options.clone())
        .await
        .unwrap();
    assert!(first.success);
    assert_eq!(first.applied.len(), 1);
    let events = f.merge_events().await;

    let second = run_dedup(&f.runtime, &f.token, options).await.unwrap();
    assert!(second.success);
    assert!(second.planned.is_empty() && second.applied.is_empty());
    assert_eq!(f.merge_events().await, events);
}

#[tokio::test]
async fn totals_account_for_every_unnamed_note_and_group_them_by_anchor() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    let other = f.anchor().await;
    let kept = f.anchor().await;
    f.pr(old, None, 1, "t", "a").await;
    f.pr(other, Some("#1 t"), 1, "t", "a").await;
    f.pr(old, None, 2, "u", "a").await;
    f.pr(other, None, 2, "v", "a").await;
    f.pr(old, None, 3, "t", "a").await;
    f.pr(kept, None, 4, "t", "a").await;
    f.pr(old, None, 4, "t", "a").await;
    f.note_of("pull_request", old, None, "a", json!({"title": "t"}))
        .await;
    f.note_of(
        "issue",
        other,
        None,
        "a",
        json!({"number": 1, "title": "t"}),
    )
    .await;
    for anchor in [old, other, kept] {
        f.retire(anchor).await;
    }
    let mut options = f.options();
    options.refused_anchors.insert(kept);

    let report = plan_dedup(&f.runtime, &f.token, options)
        .await
        .unwrap()
        .report()
        .clone();

    let prs = &report.totals["pull_request"];
    assert_eq!(prs.candidates, 8);
    assert_eq!(prs.unnamed, 7);
    assert_eq!(prs.groups_found, 2);
    assert_eq!((prs.groups_planned, prs.groups_refused), (1, 1));
    assert_eq!(prs.unnamed_merged_away, 1);
    assert_eq!(prs.unnamed_survivors, 0);
    assert_eq!(prs.unnamed_in_refused_groups, 2);
    assert_eq!(prs.unnamed_unchanged, 4);
    assert_eq!(
        prs.unnamed,
        prs.unnamed_merged_away
            + prs.unnamed_survivors
            + prs.unnamed_in_refused_groups
            + prs.unnamed_unchanged
    );
    let anchors: BTreeMap<(String, String), usize> = report
        .unnamed_by_anchor
        .iter()
        .map(|row| {
            (
                (row.kind.clone(), row.project_id.clone()),
                row.unnamed_notes,
            )
        })
        .collect();
    assert_eq!(anchors[&("pull_request".into(), old.to_string())], 5);
    assert_eq!(anchors[&("pull_request".into(), other.to_string())], 1);
    assert_eq!(anchors[&("pull_request".into(), kept.to_string())], 1);
    assert_eq!(anchors[&("issue".into(), other.to_string())], 1);
    let lines = report.summary_lines().join("\n");
    assert!(
        lines.contains("pull_request: unnamed 7 = merged away"),
        "{lines}"
    );
    assert!(lines.contains("titles differ"), "{lines}");
}

#[tokio::test]
async fn notes_in_another_namespace_are_not_candidates() {
    let f = Fixture::new().await;
    let foreign = f
        .runtime
        .authorize(Namespace::parse("other-namespace").unwrap())
        .unwrap();
    let old = f.anchor().await;
    f.pr(old, Some("#1 t"), 1, "t", "a").await;
    let outsider = f
        .runtime
        .create_note(
            &foreign,
            "pull_request",
            Some("#1 t"),
            "a",
            None,
            Some(json!({"number": 1, "title": "t", "project_id": old.to_string()})),
            vec![old],
        )
        .await
        .unwrap();
    f.retire(old).await;

    let report = f.plan().await.report().clone();

    assert_eq!(report.totals["pull_request"].candidates, 1);
    assert!(report.planned.is_empty());
    assert!(f
        .current_in_foreign(&foreign, outsider.id)
        .await
        .deleted_at
        .is_none());
}

impl Fixture {
    async fn current_in_foreign(&self, token: &NamespaceToken, id: Uuid) -> Note {
        self.runtime
            .notes(token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap()
    }
}

#[tokio::test]
async fn the_project_must_be_a_live_canonical_project() {
    let f = Fixture::new().await;
    let old = f.anchor().await;
    f.retire(old).await;
    for project in [old, Uuid::new_v4()] {
        let options = DedupOptions {
            project_id: project,
            ..f.options()
        };
        let error = match plan_dedup(&f.runtime, &f.token, options).await {
            Ok(_) => panic!("project {project} must be refused"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("live canonical project"),
            "{error}"
        );
    }
}
