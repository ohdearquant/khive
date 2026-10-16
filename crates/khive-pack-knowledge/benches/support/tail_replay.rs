use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{
    EmbedderProvider, KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::types::{SqlStatement, SqlValue};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};
use uuid::Uuid;

const MODEL: EmbeddingModel = EmbeddingModel::AllMiniLmL6V2;
const CONSUMER: &str = "knowledge:knowledge.atom";
// This harness disables search reranking, so scoped vector reads belong to replay.
const REPLAY_POINT_SQL: &str = "SELECT embedding FROM vec_all_minilm_l6_v2 WHERE subject_id = ?1 AND namespace = ?2 AND field = ?3 AND embedding_model = ?4";
const FINAL_STATES_SQL: &str = "SELECT subject_id, op, MAX(seq) AS seq FROM ann_write_log WHERE namespace = ?1 AND embedding_model = ?2 AND field = 'knowledge.atom' AND seq > ?3 GROUP BY subject_id";

#[derive(Clone, Copy, Debug)]
pub enum Arm {
    OneIndex,
    VerbDelete,
    SameSubject,
    DistinctSubjects,
    ComposedVectorDelete,
}

impl Arm {
    pub fn label(self) -> &'static str {
        match self {
            Self::OneIndex => "one_index_write",
            Self::VerbDelete => "knowledge_delete_atoms_only",
            Self::SameSubject => "m_rewrites_one_subject",
            Self::DistinctSubjects => "m_distinct_subjects",
            Self::ComposedVectorDelete => "vector_delete_then_atom_delete_two_commits",
        }
    }
}

pub struct Sample {
    pub report: Value,
    pub raw_rows: u64,
    pub distinct_subjects: u64,
    pub replay_point_reads: usize,
    pub final_state_scans: usize,
}

struct SyntheticProvider;
struct SyntheticService;

#[async_trait]
impl EmbedderProvider for SyntheticProvider {
    fn name(&self) -> &str {
        "all-minilm-l6-v2"
    }
    fn dimensions(&self) -> usize {
        MODEL.native_dimensions()
    }
    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
        Ok(Arc::new(SyntheticService))
    }
}

#[async_trait]
impl EmbeddingService for SyntheticService {
    async fn embed(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        assert_eq!(model, MODEL);
        Ok(texts
            .iter()
            .map(|text| {
                let mut subject: usize = 0;
                let mut version: usize = 0;
                for word in text.split_whitespace() {
                    let word = word
                        .trim_matches(|c: char| !c.is_alphanumeric())
                        .to_ascii_lowercase();
                    if let Some(value) = word.strip_prefix("subject") {
                        subject = value.parse().unwrap_or(subject);
                    }
                    if let Some(value) = word.strip_prefix("visiblemarker") {
                        version = value.parse().unwrap_or(version);
                    }
                }
                let digest = blake3::hash(format!("subject{subject}:revision{version}").as_bytes());
                let mut state = u64::from_le_bytes(digest.as_bytes()[..8].try_into().unwrap()) | 1;
                let mut vector: Vec<f32> = (0..MODEL.native_dimensions())
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        (state as u32 as f32 / u32::MAX as f32) * 2.0 - 1.0
                    })
                    .collect();
                let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
                for value in &mut vector {
                    *value /= norm;
                }
                vector
            })
            .collect())
    }
    fn supports_model(&self, model: EmbeddingModel) -> bool {
        model == MODEL
    }
    fn name(&self) -> &'static str {
        "tail-replay-synthetic-384"
    }
}

fn atom(subject: usize, version: usize) -> Value {
    json!({
        "slug": format!("tail-subject-{subject}"),
        "name": format!("Subject {subject}"),
        "finalized": true,
        "content": format!("subject{subject} visiblemarker{version} knowledge retrieval vector embedding tail replay corpus measurement deterministic synthetic content graph search ranking state visibility evidence warm resident update write benchmark")
    })
}

fn integer(value: Option<&SqlValue>) -> u64 {
    match value {
        Some(SqlValue::Integer(value)) => {
            u64::try_from(*value).expect("nonnegative ledger integer")
        }
        other => panic!("expected a ledger integer, got {other:?}"),
    }
}

async fn watermark(runtime: &KhiveRuntime) -> u64 {
    let sql = runtime.sql();
    let mut reader = sql.reader().await.expect("watermark reader");
    let row = reader.query_row(SqlStatement {
        sql: "SELECT watermark FROM ann_consumer_watermark WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3".into(),
        params: vec![SqlValue::Text(CONSUMER.into()), SqlValue::Text("local".into()), SqlValue::Text(MODEL.to_string())],
        label: None,
    }).await.expect("watermark query").expect("published consumer row");
    integer(row.get("watermark"))
}

async fn ledger(runtime: &KhiveRuntime, after: u64) -> (u64, u64, u64) {
    let sql = runtime.sql();
    let mut reader = sql.reader().await.expect("ledger reader");
    let row = reader.query_row(SqlStatement {
        sql: "SELECT COUNT(*) AS raw_rows, COUNT(DISTINCT subject_id) AS subjects, COALESCE(MAX(seq), ?3) AS last_seq FROM ann_write_log WHERE namespace = ?1 AND embedding_model = ?2 AND field = 'knowledge.atom' AND seq > ?3".into(),
        params: vec![SqlValue::Text("local".into()), SqlValue::Text(MODEL.to_string()), SqlValue::Integer(i64::try_from(after).expect("SQLite watermark"))],
        label: None,
    }).await.expect("ledger query").expect("ledger aggregate row");
    (
        integer(row.get("raw_rows")),
        integer(row.get("subjects")),
        integer(row.get("last_seq")),
    )
}

fn row_has_ann(row: &Value) -> bool {
    row["score_provenance"]["sources"]
        .as_array()
        .is_some_and(|sources| sources.iter().any(|source| source == "ann"))
}

fn has_ann(response: &Value) -> bool {
    response["results"]
        .as_array()
        .expect("results")
        .iter()
        .any(row_has_ann)
}

async fn search(registry: &VerbRegistry, version: usize) -> Value {
    registry
        .dispatch(
            "knowledge.search",
            json!({
                "query": format!("subject0 visiblemarker{version}"), "limit": 20, "rerank": false,
            }),
        )
        .await
        .expect("real knowledge search")
}

fn visible(response: &Value, version: usize, deleted: bool) -> bool {
    let target = response["results"]
        .as_array()
        .expect("results")
        .iter()
        .find(|row| row["slug"] == "tail-subject-0");
    if deleted {
        target.is_none()
    } else {
        target.is_some_and(|row| {
            row_has_ann(row)
                && row["content"]
                    .as_str()
                    .is_some_and(|body| body.contains(&format!("visiblemarker{version}")))
        })
    }
}

pub async fn measure(n: usize, arm: Arm, m: usize, offered_hz: f64, explicit_warm: bool) -> Sample {
    assert!(n >= 32 && m > 0 && m < n);
    assert!(offered_hz.is_finite() && offered_hz > 0.0);
    let dir = tempfile::tempdir().expect("private file-backed measurement root");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(dir.path().join("tail-replay.db")),
        embedding_model: Some(MODEL),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("file-backed runtime");
    runtime.register_embedder(SyntheticProvider);
    assert_eq!(runtime.embedder_dimensions(&MODEL.to_string()), Some(384));
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(KnowledgePack::new_with_index_role(runtime.clone(), true));
    let registry = builder.build().expect("registry");
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());
    for begin in (0..n).step_by(500) {
        let atoms: Vec<_> = (begin..(begin + 500).min(n)).map(|i| atom(i, 0)).collect();
        let seeded = registry
            .dispatch("knowledge.upsert_atoms", json!({"atoms": atoms}))
            .await
            .expect("seed real atoms");
        assert_eq!(
            seeded["created"].as_u64(),
            Some((begin + 500).min(n).saturating_sub(begin) as u64)
        );
    }
    let initial = registry
        .dispatch(
            "knowledge.index",
            json!({"rebuild_ann": true, "batch_size": 500}),
        )
        .await
        .expect("build and persist initial ANN");
    assert_eq!(initial["indexed"].as_u64(), Some(n as u64));
    assert_eq!(initial["ann_vectors"].as_u64(), Some(n as u64));
    assert_eq!(initial["ann_failed"], false);
    registry.call_warm_all().await;
    for _ in 0..3 {
        assert!(
            has_ann(&search(&registry, 0).await),
            "warm boundary requires positive public ANN provenance"
        );
    }
    let before = watermark(&runtime).await;
    assert_eq!(
        ledger(&runtime, before).await.0,
        0,
        "positive ANN provenance with an empty durable tail proves a loaded baseline slot"
    );
    let fraction = khive_runtime::config::ann_rebuild_threshold_from_env();
    let write_count = if matches!(arm, Arm::SameSubject | Arm::DistinctSubjects) {
        m
    } else {
        1
    };
    let write_start = Instant::now();
    let mut max_lateness = Duration::ZERO;
    let mut final_version = 0;
    for i in 0..write_count {
        // The offered schedule is fixed before writes; late completions never move it.
        let offered_at = write_start + Duration::from_secs_f64(i as f64 / offered_hz);
        if let Some(wait) = offered_at.checked_duration_since(Instant::now()) {
            tokio::time::sleep(wait).await;
        }
        max_lateness = max_lateness.max(Instant::now().saturating_duration_since(offered_at));
        if matches!(arm, Arm::VerbDelete | Arm::ComposedVectorDelete) {
            if matches!(arm, Arm::ComposedVectorDelete) {
                let sql = runtime.sql();
                let mut reader = sql.reader().await.expect("target identity reader");
                let row = reader.query_row(SqlStatement {
                    sql: "SELECT id FROM knowledge_atoms WHERE namespace = ?1 AND slug = ?2 AND deleted_at IS NULL".into(),
                    params: vec![SqlValue::Text("local".into()), SqlValue::Text("tail-subject-0".into())], label: None,
                }).await.expect("target identity").expect("live target");
                let id = match row.get("id") {
                    Some(SqlValue::Text(id)) => Uuid::parse_str(id).expect("target UUID"),
                    other => panic!("target id: {other:?}"),
                };
                drop(reader);
                let vectors = runtime
                    .vectors_for_model(&token, &MODEL.to_string())
                    .expect("vector store");
                assert_eq!(
                    vectors
                        .delete_subjects(&[id])
                        .await
                        .expect("real vector delete"),
                    1
                );
            }
            let removed = registry
                .dispatch("knowledge.delete_atoms", json!({"ids": ["tail-subject-0"]}))
                .await
                .expect("real atom delete");
            assert_eq!(removed["deleted"], 1);
        } else {
            let subject = if matches!(arm, Arm::DistinctSubjects) {
                i
            } else {
                0
            };
            let version = i + 1;
            registry
                .dispatch(
                    "knowledge.upsert_atoms",
                    json!({"atoms": [atom(subject, version)]}),
                )
                .await
                .expect("prepare actual atom rewrite");
            let indexed = registry
                .dispatch(
                    "knowledge.index",
                    json!({"ids": [format!("tail-subject-{subject}")]}),
                )
                .await
                .expect("real index vector write");
            assert_eq!(indexed["indexed"], 1);
            assert_eq!(indexed["failed"], 0);
            if subject == 0 {
                final_version = version;
            }
        }
    }
    let committed = Instant::now();
    let write_elapsed = committed.duration_since(write_start);
    let (raw_rows, distinct_subjects, last_seq) = ledger(&runtime, before).await;
    let observation = runtime
        .backend()
        .pool()
        .observe_test_statement_starts(100_000)
        .expect("actual statement-start observation");
    let mut explicit_warm_ns = None;
    if explicit_warm {
        let started = Instant::now();
        registry.call_warm_all().await;
        explicit_warm_ns = Some(started.elapsed().as_nanos());
    }
    let first_start = Instant::now();
    let mut response = search(&registry, final_version).await;
    let first_search_ns = first_start.elapsed().as_nanos();
    let deleted = matches!(arm, Arm::VerbDelete | Arm::ComposedVectorDelete);
    let watchdog = Instant::now() + Duration::from_secs(60);
    while !(has_ann(&response) && visible(&response, final_version, deleted)) {
        assert!(
            Instant::now() < watchdog,
            "visibility watchdog expired; this is not a performance result"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
        response = search(&registry, final_version).await;
    }
    let visible_ns = committed.elapsed().as_nanos();
    for _ in 0..2 {
        let settled = search(&registry, final_version).await;
        assert!(
            has_ann(&settled),
            "post-write guard requires public ANN provenance"
        );
        assert!(
            visible(&settled, final_version, deleted),
            "post-write visibility must remain stable"
        );
    }
    let starts = observation
        .started_statements()
        .expect("complete statement observation");
    drop(observation);
    let replay_point_reads = starts
        .iter()
        .filter(|s| s.readonly && s.sql == REPLAY_POINT_SQL)
        .count();
    let final_state_scans = starts
        .iter()
        .filter(|s| s.readonly && s.sql == FINAL_STATES_SQL)
        .count();
    let fresh_tail_snapshot_starts = starts
        .iter()
        .filter(|s| {
            s.readonly
                && s.sql.starts_with("WITH registry AS (")
                && s.sql.contains("vectors.field AS vector_field")
                && s.sql.contains("LEFT JOIN finals ON 1 = 1")
        })
        .count();
    let after = watermark(&runtime).await;
    let mut report = serde_json::Map::new();
    for section in [
        json!({
            "schema_version": 1,
            "issue": 3780,
            "arm": arm.label(),
            "n": n,
            "m": write_count,
            "model": MODEL.to_string(),
            "provider": "deterministic synthetic; no lattice model inference",
            "dimensions": 384,
            "cache_state": {"process": "resident", "application_ann": "published file-backed segment, three positive ANN warm searches and empty baseline tail", "model": "synthetic provider/service resident", "sqlite": "same warm pool; page contents uncontrolled after writes", "os_page_cache": "uncontrolled"},
            "warm_boundary": "initial full index publication, call_warm_all, three positive ANN provenance searches, empty durable tail",
            "after_write_boundary": if explicit_warm { "explicit public call_warm_all before first search (smoke/control)" } else { "first real search after commit, then positive stable ANN/visibility guards (measurement)" },
        }),
        json!({
            "instrumentation": "existing test-support statement-start observer enabled; instrumentation overhead is included",
            "sqlite_statement_starts": starts.len(),
            "offered_write_rate_ops_s": offered_hz,
            "offered_schedule": "fixed monotonic deadlines; one writer, late arrivals are retained, never dropped",
            "achieved_write_rate_ops_s": write_count as f64 / write_elapsed.as_secs_f64(),
            "max_arrival_lateness_ns": max_lateness.as_nanos(),
            "completed_workload_ops": write_count,
            "failed_or_refused_ops": 0,
            "overflow_policy": "sequential waiting with positive rate; no dropped offers; runtime errors refuse the sample",
            "residency_mode": "file-backed mmap publication path; actual backing is not publicly introspectable",
            "ann_rebuild_threshold": fraction,
        }),
        json!({
            "tail_caps": {"restart_raw_row_fraction": fraction, "fresh_tail_no_serving_index_fraction": fraction, "replay_batch_subjects": 500, "fresh_tail_reresolve_rounds": 3, "ann_readiness_cap_ms": 5000},
            "ann_fresh_tail_enabled": runtime.ann_fresh_tail_enabled(),
            "raw_tail_rows_before_search": raw_rows,
            "distinct_tail_subjects_before_search": distinct_subjects,
            "baseline_watermark": before,
            "last_workload_seq": last_seq,
            "post_search_watermark": after,
            "ann_replay_point_read_starts": replay_point_reads,
            "ann_final_state_scan_starts": final_state_scans,
            "fresh_tail_snapshot_statement_starts": fresh_tail_snapshot_starts,
            "first_search_ns": first_search_ns,
        }),
        json!({
            "commit_to_visible_ns": visible_ns,
            "explicit_warm_call_ns": explicit_warm_ns,
            "commit_to_visible_interval_includes": "post-commit ledger read, observer setup, optional explicit warm call, and public search/visibility checks",
            "search_interval_per_raw_row_ns_proxy": if raw_rows == 0 { None } else { Some(first_search_ns as f64 / raw_rows as f64) },
            "search_interval_per_distinct_subject_ns_proxy": if distinct_subjects == 0 { None } else { Some(first_search_ns as f64 / distinct_subjects as f64) },
            "isolated_replay_duration": "UNMEASURED",
            "warm_wait_duration": "UNMEASURED",
            "fresh_tail_returned_rows": "UNMEASURED",
            "route_evidence": "positive public ANN provenance and empty baseline tail; updated target has ANN provenance and final body marker; actual observed replay SQL starts after writes",
            "visibility_scope": "public ANN-tagged target visibility, including eligible fresh-tail candidates; not proof that background replay completed",
            "delete_semantics": if matches!(arm, Arm::ComposedVectorDelete) { "informational two-commit vector+atom deletion; no atomicity claim" } else if matches!(arm, Arm::VerbDelete) { "actual knowledge.delete_atoms soft-deletes atom only; no vector delete tail" } else { "knowledge.index after a content rewrite; vector commit acknowledged before visibility interval" },
        }),
    ] {
        let Value::Object(fields) = section else {
            unreachable!("report sections are JSON objects");
        };
        report.extend(fields);
    }
    let report = Value::Object(report);
    Sample {
        report,
        raw_rows,
        distinct_subjects,
        replay_point_reads,
        final_state_scans,
    }
}
