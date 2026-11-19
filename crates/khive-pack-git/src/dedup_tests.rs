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

include!("dedup_tests/survivors_and_titles_tests.rs");

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

include!("dedup_tests/url_and_anchor_tests.rs");

include!("dedup_tests/candidate_proof_and_content_tests.rs");

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
async fn a_committed_reindex_failure_fails_the_report_without_stopping_merges() {
    let mut f = Fixture::embedding().await;
    let namespace = format!("git-dedup-reindex-{}", Uuid::new_v4().as_simple());
    f.token = f
        .runtime
        .authorize(Namespace::parse(&namespace).unwrap())
        .unwrap();
    f.canonical = f.project().await;
    let old = f.anchor().await;
    let other = f.anchor().await;
    let survivor = f
        .pr(f.canonical, Some("#1 t"), 1, "t", "survivor body")
        .await;
    let first = f.pr(old, Some("#1 t"), 1, "t", "first donor body").await;
    let second = f.pr(other, Some("#1 t"), 1, "t", "second donor body").await;
    f.retire(old).await;
    f.retire(other).await;
    let plan = f.plan().await;
    assert_eq!(plan.report().planned.len(), 1);
    assert_eq!(plan.report().planned[0].survivor.id, survivor.id);
    assert_eq!(plan.report().planned[0].donors.len(), 2);
    let events_before = f.merge_events().await;

    let _arm = khive_runtime::arm_fts_fail_scoped(&namespace);
    let report = apply_dedup(&f.runtime, &f.token, plan).await.unwrap();

    assert_eq!(report.applied.len(), 2);
    assert!(report.refused_merges.is_empty());
    let error = report.applied[0]
        .summary
        .post_commit_reindex_error
        .as_deref()
        .expect("the first committed merge must report the injected failure");
    assert!(error.contains("injected FTS failure"), "{error}");
    assert_eq!(report.applied[1].summary.post_commit_reindex_error, None);
    assert!(!report.success, "a later success must not mask the failure");
    assert_eq!(serde_json::to_value(&report).unwrap()["success"], false);
    assert!(report.summary_lines().contains(&format!(
        "note {}: merge committed; post-commit reindex failed: {}",
        survivor.id, error
    )));
    assert!(f.is_deleted(first.id).await);
    assert!(f.is_deleted(second.id).await);
    let kept = f.current(survivor.id).await;
    for body in ["survivor body", "first donor body", "second donor body"] {
        assert!(kept.content.contains(body), "missing {body}");
    }
    assert_eq!(kept.version, report.applied[1].kept_version);
    assert_eq!(f.merge_events().await, events_before + 2);
}

include!("dedup_tests/reporting_and_preview_tests.rs");

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
