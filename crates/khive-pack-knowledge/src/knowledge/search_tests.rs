use super::*;
use khive_storage::types::{SqlRow, StorageResult};
use std::sync::{Arc, Mutex};

#[cfg(feature = "namespace-trigram-proto")]
pub(super) type PrototypePhaseTimes = Arc<Mutex<HashMap<&'static str, (u128, u64)>>>;

#[cfg(feature = "namespace-trigram-proto")]
tokio::task_local! {
    static PROTOTYPE_PHASE_TIMES: PrototypePhaseTimes;
}

#[cfg(feature = "namespace-trigram-proto")]
pub(super) async fn with_prototype_phase_times<F: std::future::Future>(
    timings: PrototypePhaseTimes,
    future: F,
) -> F::Output {
    PROTOTYPE_PHASE_TIMES.scope(timings, future).await
}

#[cfg(feature = "namespace-trigram-proto")]
fn prototype_phase(sql: &str) -> &'static str {
    let prefixed = sql.contains("fts_knowledge_namespace_proto");
    if sql.starts_with("SELECT namespace_key FROM knowledge_fts_namespace_tokens") {
        "namespace_key"
    } else if sql.starts_with("SELECT count(*) AS frequency") {
        if prefixed {
            "term_frequency_prefixed"
        } else {
            "term_frequency_slot_table"
        }
    } else if sql.starts_with("SELECT rowid FROM") {
        if prefixed {
            "phase_a_rowids_prefixed"
        } else {
            "phase_a_rowids_slot_table"
        }
    } else if sql.starts_with("SELECT a.* FROM fts_") {
        if prefixed {
            "eligibility_fallback_prefixed"
        } else {
            "eligibility_fallback_slot_table"
        }
    } else if sql.starts_with("SELECT a.*, a.rowid AS rowid") {
        "phase_b_hydration"
    } else if sql.starts_with("SELECT 1 AS present FROM fts_") {
        if prefixed {
            "namespace_existence_prefixed"
        } else {
            "namespace_existence_slot_table"
        }
    } else if sql.starts_with("SELECT 1 AS present FROM knowledge_atoms") {
        "namespace_membership"
    } else {
        "other_lexical_read"
    }
}

#[cfg(feature = "namespace-trigram-proto")]
struct PrototypeTimingReader {
    inner: Box<dyn khive_storage::SqlReader>,
    timings: PrototypePhaseTimes,
}

#[cfg(feature = "namespace-trigram-proto")]
impl PrototypeTimingReader {
    fn record(&self, sql: &str, elapsed: std::time::Duration) {
        let mut timings = self.timings.lock().expect("prototype phase timings");
        let sample = timings.entry(prototype_phase(sql)).or_default();
        sample.0 += elapsed.as_nanos();
        sample.1 += 1;
    }
}

#[cfg(feature = "namespace-trigram-proto")]
#[async_trait::async_trait]
impl khive_storage::SqlReader for PrototypeTimingReader {
    async fn query_row(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlRow>> {
        let started = std::time::Instant::now();
        let result = self.inner.query_row(statement.clone()).await;
        self.record(&statement.sql, started.elapsed());
        result
    }

    async fn query_all(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        let started = std::time::Instant::now();
        let result = self.inner.query_all(statement.clone()).await;
        self.record(&statement.sql, started.elapsed());
        result
    }

    async fn query_scalar(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlValue>> {
        let started = std::time::Instant::now();
        let result = self.inner.query_scalar(statement.clone()).await;
        self.record(&statement.sql, started.elapsed());
        result
    }

    async fn explain(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        self.inner.explain(statement).await
    }
}

tokio::task_local! {
    static TERM_PROBES: Arc<Mutex<Vec<SqlStatement>>>;
}

struct TermRecordingReader {
    inner: Box<dyn khive_storage::SqlReader>,
    probes: Arc<Mutex<Vec<SqlStatement>>>,
}

impl TermRecordingReader {
    fn record(&self, statement: &SqlStatement) {
        if statement.sql.contains("fts_knowledge MATCH") {
            self.probes
                .lock()
                .expect("term probes")
                .push(statement.clone());
        }
    }
}

#[async_trait::async_trait]
impl khive_storage::SqlReader for TermRecordingReader {
    async fn query_row(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlRow>> {
        self.record(&statement);
        self.inner.query_row(statement).await
    }

    async fn query_all(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        self.record(&statement);
        self.inner.query_all(statement).await
    }

    async fn query_scalar(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlValue>> {
        self.record(&statement);
        self.inner.query_scalar(statement).await
    }

    async fn explain(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        self.inner.explain(statement).await
    }
}

pub(super) fn record_term_probes(
    inner: Box<dyn khive_storage::SqlReader>,
) -> Box<dyn khive_storage::SqlReader> {
    let inner: Box<dyn khive_storage::SqlReader> = match TERM_PROBES.try_with(Arc::clone) {
        Ok(probes) => Box::new(TermRecordingReader { inner, probes }),
        Err(_) => inner,
    };
    #[cfg(feature = "namespace-trigram-proto")]
    if let Ok(timings) = PROTOTYPE_PHASE_TIMES.try_with(Arc::clone) {
        return Box::new(PrototypeTimingReader { inner, timings });
    }
    inner
}

fn distinct_term_query(count: usize) -> String {
    (0..count)
        .map(|index| format!("distinctterm{index}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn probed_term(statement: &SqlStatement) -> &str {
    match &statement.params[0] {
        SqlValue::Text(term) => term,
        other => panic!("expected bound FTS term, got {other:?}"),
    }
}

#[tokio::test]
async fn distinct_term_fan_out_is_bounded_and_reports_truncation() {
    let runtime = KhiveRuntime::memory().expect("runtime");
    let token = runtime.authorize(Namespace::local()).expect("token");
    for query in [
        distinct_term_query(FTS_TERM_COUNT_LIMIT * 2),
        "alpha beta gamma".into(),
    ] {
        let terms = fts5_candidate_terms(&query);
        let probes = Arc::new(Mutex::new(Vec::new()));
        let response = TERM_PROBES
            .scope(
                probes.clone(),
                KnowledgeHandlers::search(
                    &runtime,
                    &token,
                    json!({"query": query, "rerank": false}),
                    &vamana::new_shared(),
                ),
            )
            .await
            .expect("bounded search");
        let probes = probes.lock().expect("term probes");
        assert_eq!(probes.len(), terms.len().min(FTS_TERM_COUNT_LIMIT));
        assert_eq!(
            probes.iter().map(probed_term).collect::<Vec<_>>(),
            terms
                .iter()
                .take(FTS_TERM_COUNT_LIMIT)
                .map(String::as_str)
                .collect::<Vec<_>>(),
            "admission must precede even the rarity-frequency probes"
        );
        assert_eq!(
            response["candidate_provenance"]["terms_truncated"],
            terms.len() > FTS_TERM_COUNT_LIMIT
        );
        assert_eq!(response["candidate_provenance"]["lexical"], "no_match");
    }
}

#[tokio::test]
async fn term_budget_is_shared_across_decomposed_sub_queries() {
    let runtime = KhiveRuntime::memory().expect("runtime");
    let token = runtime.authorize(Namespace::local()).expect("token");
    // Ten raw terms expand to twenty: the full query fits, but the
    // combined work of all three passes must still consume one budget.
    for count in [10, FTS_TERM_COUNT_LIMIT + 18] {
        let probes = Arc::new(Mutex::new(Vec::new()));
        let response = TERM_PROBES.scope(probes.clone(), KnowledgeHandlers::search(
                &runtime, &token,
                json!({"query": distinct_term_query(count), "decompose": true, "rerank": false}),
                &vamana::new_shared(),
            )).await.expect("decomposed bounded search");
        assert_eq!(
            probes.lock().expect("term probes").len(),
            FTS_TERM_COUNT_LIMIT
        );
        assert_eq!(response["candidate_provenance"]["terms_truncated"], true);
    }
}

#[tokio::test]
async fn term_bound_covers_staged_fallback_and_namespace_probes() {
    let runtime = KhiveRuntime::memory().expect("runtime");
    let query = distinct_term_query(FTS_TERM_COUNT_LIMIT * 2);
    let terms = fts5_candidate_terms(&query);
    let content = terms
        .iter()
        .map(|term| term.trim_matches('"'))
        .collect::<Vec<_>>()
        .join(" ");
    let sql = runtime.sql();
    let mut writer = sql.writer().await.expect("writer");
    for index in 0..4 {
        let (namespace, content) = if index < 3 {
            ("foreign", content.as_str())
        } else {
            ("local", terms.last().unwrap().trim_matches('"'))
        };
        writer.execute(SqlStatement {
                sql: "INSERT INTO knowledge_atoms (id, namespace, slug, name, content, tags, finalized, status, created_at, updated_at) VALUES (?1, ?2, ?3, ?3, ?4, '[]', 1, 'reviewed', 0, 0)".into(),
                params: vec![
                    SqlValue::Text(Uuid::new_v4().to_string()),
                    SqlValue::Text(namespace.into()),
                    SqlValue::Text(format!("term-bound-{index}")),
                    SqlValue::Text(content.into()),
                ],
                label: None,
            }).await.expect("seed bounded-stage fixture");
    }
    drop(writer);
    let budget = FtsTermBudget::new();
    let probes = Arc::new(Mutex::new(Vec::new()));
    let outcome = TERM_PROBES
        .scope(
            probes.clone(),
            with_phase_a_widen_ceiling_override(
                2,
                super::fetch_fts_candidates(
                    &runtime,
                    "local",
                    &query,
                    None,
                    &[],
                    &[],
                    5,
                    &budget,
                    LexicalStage::new(
                        LexicalPass::Full,
                        tokio::time::Instant::now(),
                        lexical_stage_budget(),
                    ),
                ),
            ),
        )
        .await
        .expect("bounded staged fetch");
    assert_eq!(outcome.state, LexicalCandidateState::NoMatch);
    assert!(outcome.atoms.is_empty() && outcome.timeout.is_none());
    assert!(budget.truncated());
    let probes = probes.lock().expect("term probes");
    let allowed: HashSet<_> = terms
        .iter()
        .take(FTS_TERM_COUNT_LIMIT)
        .map(String::as_str)
        .collect();
    assert!(probes
        .iter()
        .all(|statement| allowed.contains(probed_term(statement))));
    assert_eq!(
        probes
            .iter()
            .filter(|statement| statement.sql.starts_with("SELECT rowid"))
            .count(),
        FTS_TERM_COUNT_LIMIT
    );
    assert_eq!(
        probes
            .iter()
            .filter(|statement| statement.sql.starts_with("SELECT count(*) AS frequency"))
            .count(),
        FTS_TERM_COUNT_LIMIT
    );
    assert_eq!(
        probes
            .iter()
            .filter(|statement| statement.sql.starts_with("SELECT a.*"))
            .count(),
        FTS_TERM_COUNT_LIMIT
    );
    assert_eq!(
        probes
            .iter()
            .filter(|statement| statement
                .sql
                .starts_with("SELECT 1 AS present FROM fts_knowledge"))
            .count(),
        FTS_TERM_COUNT_LIMIT
    );
}

#[test]
fn lexical_candidate_state_merge_preserves_completed_and_timed_out_passes() {
    use LexicalCandidateState::*;
    for (states, expected) in [
        (vec![], NoMatch),
        (vec![NoMatch, NoMatch], NoMatch),
        (vec![NoMatch, Filtered], Filtered),
        (vec![Filtered, Matched], Matched),
        (vec![TimedOut, TimedOut], TimedOut),
        (vec![TimedOut, NoMatch], PartialTimeout),
        (vec![Matched, TimedOut], PartialTimeout),
        (vec![PartialTimeout, Matched], PartialTimeout),
    ] {
        assert_eq!(
            LexicalCandidateState::merge(&states),
            expected,
            "{states:?}"
        );
        let reversed: Vec<_> = states.into_iter().rev().collect();
        assert_eq!(LexicalCandidateState::merge(&reversed), expected);
    }
}

async fn seed_candidate_state_fixture(runtime: &KhiveRuntime) {
    let access = runtime.sql();
    let mut writer = access.writer().await.expect("writer");
    writer.execute(SqlStatement {
            sql: "INSERT INTO knowledge_atoms \
                  (id, namespace, slug, name, content, tags, finalized, status, created_at, updated_at) \
                  VALUES \
                  ('92700000-0000-0000-0000-000000000001', 'local', 'newest-unrelated', \
                   'Newest Unrelated', 'ordinary background content', '[]', 1, 'reviewed', 20, 20), \
                  ('92700000-0000-0000-0000-000000000002', 'local', 'filtered-match', \
                   'Filtered Match', 'zzfilteredzz', '[]', 0, 'draft', 10, 10)".into(),
            params: Vec::new(),
            label: None,
        }).await.expect("seed candidate state rows");
}

#[tokio::test]
async fn true_lexical_miss_does_not_return_newest_rows() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    seed_candidate_state_fixture(&runtime).await;
    let query = "zzgenuinelyabsentzz";
    let outcome = fetch_fts_candidates(&runtime, "local", query, None, &[], &[], 5)
        .await
        .expect("true lexical miss");
    assert!(
        outcome.atoms.is_empty(),
        "a true FTS miss must not return recent rows"
    );
    assert_eq!(outcome.state, LexicalCandidateState::NoMatch);
    assert!(outcome.timeout.is_none());
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let response = KnowledgeHandlers::search(
        &runtime,
        &token,
        json!({"query": query, "rerank": false}),
        &vamana::new_shared(),
    )
    .await
    .expect("public search");
    assert_eq!(
        response,
        json!({
            "results": [], "total": 0,
            "candidate_provenance": {"lexical": "no_match", "fallback": "none", "terms_truncated": false},
        })
    );
    let lexical = KnowledgeHandlers::search(
        &runtime,
        &token,
        json!({"query": "ordinary", "rerank": false}),
        &vamana::new_shared(),
    )
    .await
    .expect("matching lexical control");
    assert_eq!(
        lexical["candidate_provenance"],
        json!({"lexical": "matched", "fallback": "none", "terms_truncated": false})
    );
    assert_eq!(lexical["results"][0]["slug"], "newest-unrelated");
    assert_eq!(
        lexical["results"][0]["score_provenance"],
        json!({
            "sources": ["lexical"], "embedding_rerank": false,
            "normalization": "s_over_s_plus_1", "calibrated": false,
        })
    );
}

#[tokio::test]
async fn lexical_candidate_state_distinguishes_filtered_match() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    seed_candidate_state_fixture(&runtime).await;
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let ann = vamana::new_shared();
    for (include_drafts, state, total) in [(false, "filtered", 0), (true, "matched", 1)] {
        let response = KnowledgeHandlers::search(
            &runtime,
            &token,
            json!({"query": "zzfilteredzz", "rerank": false, "include_drafts": include_drafts}),
            &ann,
        )
        .await
        .expect("filtered search");
        assert_eq!(
            response["candidate_provenance"],
            json!({"lexical": state, "fallback": "none", "terms_truncated": false})
        );
        assert_eq!(response["total"], total);
        if include_drafts {
            assert_eq!(response["results"][0]["slug"], "filtered-match");
        }
    }
}

async fn fetch_fts_candidates(
    runtime: &KhiveRuntime,
    ns: &str,
    raw_query: &str,
    type_filter: Option<&str>,
    statuses: &[String],
    exclude_statuses: &[&str],
    fetch_limit: usize,
) -> Result<FtsFetchOutcome, RuntimeError> {
    super::fetch_fts_candidates(
        runtime,
        ns,
        raw_query,
        type_filter,
        statuses,
        exclude_statuses,
        fetch_limit,
        &FtsTermBudget::new(),
        LexicalStage::new(
            LexicalPass::Full,
            tokio::time::Instant::now(),
            lexical_stage_budget(),
        ),
    )
    .await
}

struct ProbeRecordingReader {
    inner: Box<dyn khive_storage::SqlReader>,
    probes: Vec<(SqlStatement, Vec<SqlRow>)>,
}

#[async_trait::async_trait]
impl khive_storage::SqlReader for ProbeRecordingReader {
    async fn query_row(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlRow>> {
        self.inner.query_row(statement).await
    }

    async fn query_all(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        let rows = self.inner.query_all(statement.clone()).await?;
        self.probes.push((statement, rows.clone()));
        Ok(rows)
    }

    async fn query_scalar(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlValue>> {
        self.inner.query_scalar(statement).await
    }

    async fn explain(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        self.inner.explain(statement).await
    }
}

#[tokio::test]
async fn compose_direct_handler_rejects_namespace_token_mismatch() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let ann = vamana::new_shared();

    let err = KnowledgeHandlers::compose(
        &runtime,
        &token,
        json!({
            "namespace": "bench-arm-a",
            "query": "must reject before reading",
        }),
        &ann,
        HashMap::new(),
    )
    .await
    .expect_err("a local token must not elevate into a measurement arm");

    assert!(
        matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("does not match authorized token namespace")),
        "unexpected error: {err:?}"
    );
}

#[test]
fn fts_candidate_terms_recalls_non_contiguous_terms() {
    assert_eq!(
        fts5_candidate_terms("alpha beta alpha and").join(" OR "),
        "\"alpha\" OR \"alphas\" OR \"beta\" OR \"betas\""
    );
    assert_eq!(
        fts5_candidate_terms("RAG").join(" OR "),
        "\"rag\" OR \"rags\""
    );
    assert_eq!(
        fts5_candidate_terms("the and").join(" OR "),
        "\"the and\"",
        "stop-only queries retain the exact-phrase fallback"
    );
}

#[tokio::test]
async fn rarity_probes_are_capped_and_sort_rare_terms_before_common_terms() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    seed_low_overlap_corpus(&runtime, 1_100, 20).await;
    let mut reader = ProbeRecordingReader {
        inner: runtime.sql().reader().await.expect("reader"),
        probes: Vec::new(),
    };
    let terms = [
        "\"term1\"",
        "\"term18\"",
        "\"missing\"",
        "\"term11\"",
        "\"synthetic\"",
    ];
    let ordered = rarest_fts_terms_first(
        &mut reader,
        terms.iter().map(|term| (*term).to_string()).collect(),
        &mut LexicalStage::new(
            LexicalPass::Full,
            tokio::time::Instant::now(),
            lexical_stage_budget(),
        ),
    )
    .await
    .expect("frequency probes");
    assert_eq!(
        ordered,
        ["\"term11\"", "\"term18\"", "\"synthetic\"", "\"term1\""]
    );
    assert_eq!(reader.probes.len(), terms.len());
    assert_eq!(
        reader
            .probes
            .iter()
            .map(|(_, rows)| rows.len())
            .collect::<Vec<_>>(),
        [1; 5],
        "each probe must materialize one aggregate row, including zero matches"
    );
    assert_eq!(
        reader
            .probes
            .iter()
            .map(|(_, rows)| row_i64(&rows[0], "frequency").expect("frequency"))
            .collect::<Vec<_>>(),
        [501, 55, 0, 55, 501],
        "rare counts stay exact and both common terms tie at the cap"
    );
    for (statement, rows) in &reader.probes {
        assert!(matches!(statement.params[1], SqlValue::Integer(501)));
        assert_eq!(rows[0].columns.len(), 1);
    }
}

#[tokio::test]
async fn fts_candidates_pin_the_rowid_prefix_boundary() {
    for (regular_count, best_is_admitted) in [
        (FTS_TERM_LIMIT * PHASE_A_OVERFETCH_FACTOR, false),
        (FTS_TERM_LIMIT - 1, true),
    ] {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let access = runtime.sql();
        let mut writer = access.writer().await.expect("writer");
        writer
                .execute(SqlStatement {
                    sql: "WITH RECURSIVE entries(n) AS ( \
                              VALUES(1) UNION ALL SELECT n + 1 FROM entries WHERE n < ?1 \
                          ) \
                          INSERT INTO knowledge_atoms ( \
                              rowid, id, namespace, slug, name, content, tags, finalized, \
                              status, created_at, updated_at \
                          ) \
                          SELECT n, printf('92700000-0000-0000-0000-%012d', n), \
                              'local', printf('prefix-%06d', n), 'Prefix Document', \
                              'zzprefixzz padding padding padding padding padding padding padding', \
                              '[]', 1, 'reviewed', 0, 0 FROM entries"
                        .into(),
                    params: vec![SqlValue::Integer(regular_count as i64)],
                    label: None,
                })
                .await
                .expect("seed early matches");
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO knowledge_atoms ( \
                              rowid, id, namespace, slug, name, content, tags, finalized, \
                              status, created_at, updated_at \
                          ) VALUES ( \
                              ?1, '92700000-0000-0000-0001-000000000000', 'local', \
                              'zzprefix-best', 'Prefix Document', ?2, '[]', 1, 'reviewed', 0, 0)"
                    .into(),
                params: vec![
                    SqlValue::Integer((regular_count + 1) as i64),
                    SqlValue::Text("zzprefixzz ".repeat(8)),
                ],
                label: None,
            })
            .await
            .expect("seed late repeated match");
        drop(writer);

        let mut reader = access.reader().await.expect("reader");
        let best = reader
            .query_row(SqlStatement {
                sql: "SELECT a.rowid, a.slug FROM fts_knowledge \
                          JOIN knowledge_atoms AS a ON a.rowid = fts_knowledge.rowid \
                          WHERE fts_knowledge MATCH ?1 AND a.namespace = 'local' \
                            AND a.deleted_at IS NULL \
                          ORDER BY bm25(fts_knowledge), a.slug LIMIT 1"
                    .into(),
                params: vec![SqlValue::Text("\"zzprefixzz\"".into())],
                label: None,
            })
            .await
            .expect("BM25 control query")
            .expect("BM25 match");
        assert_eq!(row_str(&best, "slug").as_deref(), Some("zzprefix-best"));
        assert_eq!(row_i64(&best, "rowid"), Some((regular_count + 1) as i64));
        drop(reader);

        let outcome = fetch_fts_candidates(
            &runtime,
            "local",
            "zzprefixzz",
            None,
            &[],
            &[],
            FTS_TERM_LIMIT,
        )
        .await
        .expect("bounded candidate fetch");
        assert!(outcome.timeout.is_none());
        assert_eq!(outcome.atoms.len(), FTS_TERM_LIMIT);
        assert_eq!(
            outcome
                .atoms
                .iter()
                .any(|atom| atom.slug == "zzprefix-best"),
            best_is_admitted,
            "BM25-best row admission must follow the rowid window; regular_count={regular_count}"
        );
    }
}

#[tokio::test]
async fn phase_a_limit_uses_index_order_without_sorting_matches() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    seed_low_overlap_corpus(&runtime, 1_000, 20).await;
    let access = runtime.sql();
    let mut reader = access.reader().await.expect("reader");
    let statement = phase_a_rowids_statement("\"term1\"", 3);
    for bounded in [statement.clone(), term_frequency_statement("\"term1\"")] {
        for (query, should_sort) in [
            (bounded.sql.clone(), false),
            (
                bounded
                    .sql
                    .replace("ORDER BY rowid", "ORDER BY bm25(fts_knowledge), rowid"),
                true,
            ),
        ] {
            let plan = reader
                .query_all(SqlStatement {
                    sql: format!("EXPLAIN QUERY PLAN {query}"),
                    params: bounded.params.clone(),
                    label: None,
                })
                .await
                .expect("query plan");
            assert!(!plan.is_empty());
            assert!(plan.iter().any(|row| {
                row_str(row, "detail")
                    .is_some_and(|detail| detail.contains("fts_knowledge VIRTUAL TABLE INDEX"))
            }));
            let sorts = plan.iter().any(|row| {
                row_str(row, "detail").is_some_and(|detail| detail.contains("TEMP B-TREE"))
            });
            assert_eq!(sorts, should_sort, "plan: {plan:?}");
        }
    }
    let rows = reader.query_all(statement).await.expect("bounded rowids");
    let ids: Vec<_> = rows
        .iter()
        .map(|row| row_i64(row, "rowid").unwrap())
        .collect();
    assert_eq!(ids.len(), 3);
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
}

/// Issue #2766. `rarest_fts_terms_first` fetches no candidates — it sorts
/// the terms — and before this bound it ran against the whole lexical
/// stage budget, after which the caller returned an EMPTY candidate list
/// for a stage that had not yet asked for a candidate. The probe now has
/// its own quarter-budget and its expiry falls back to the arrival order.
///
/// This arm fails on the unpatched code, where `atoms` is empty and the
/// state is `TimedOut`.
#[tokio::test(start_paused = true)]
async fn rarity_probe_expiry_keeps_the_lexical_arm() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    seed_low_overlap_corpus(&runtime, 1_000, 20).await;
    let stage_budget = std::time::Duration::from_secs(1);

    let outcome = with_lexical_stage_budget_override_ms(
        1_000,
        khive_storage::scope_request_read_deadline(
            stage_budget,
            // Past the quarter-budget (250 ms) after the first probe, and
            // short of the stage budget, so only the probe is cut off.
            with_fts_probe_deadline_advance_after_term(
                1,
                std::time::Duration::from_millis(300),
                fetch_fts_candidates(
                    &runtime,
                    "local",
                    "term1 term18",
                    None,
                    &[],
                    &[],
                    CANDIDATE_POOL,
                ),
            ),
        ),
    )
    .await
    .expect("a cut-off ordering probe must not fail the fetch");

    assert!(
        !outcome.atoms.is_empty(),
        "the ordering probe timing out must not discard the lexical arm"
    );
    assert!(
        outcome.timeout.is_some(),
        "the degradation must still be reported, not swallowed"
    );
    assert_eq!(
        outcome.state,
        LexicalCandidateState::PartialTimeout,
        "candidates in hand with a recorded timeout is partial, never clean"
    );
    let detail = outcome.timeout.expect("recorded timeout");
    assert_eq!(
        detail.phase,
        LexicalPhase::TermFrequency,
        "the reported phase must name the probe, not the fetch"
    );
}

/// The pairing that shows the fallback IS the candidate-list order and
/// that the probe is what produced the rarest-first ordering. One arm
/// cannot show both, so both run here over the same corpus and the same
/// per-term expiry, differing only in whether the probe was allowed to
/// finish.
///
/// The fallback order is NOT the caller's word order, and this arm is
/// written the way it is because the first version of it assumed that and
/// failed: `fts5_candidate_terms` ends in `expand_terms`, which sorts
/// (`scoring.rs`), so the candidate list is lexicographic whatever the
/// caller wrote. Both spellings therefore fall back to `term1` first, and
/// running both is the evidence for that rather than a repetition.
///
/// The term pair is chosen, not incidental. The corpus writes
/// `discusses topic termN` for N in 0..20, so every token matches 50 rows
/// EXCEPT `term1`, whose FTS prefix family also covers `term10`..`term19`
/// and therefore matches about 550. `term1` is the only common token
/// available, and it has to be paired with a token OUTSIDE its own prefix
/// family or the two match sets are nested and no assertion can separate
/// them. `term7` qualifies; `term18` does not, which is what an earlier
/// version of this arm got wrong.
///
/// The reading instrument is the FIRST atom. The merge loop walks the
/// per-term row vectors in order, so `atoms[0]` is the first row of the
/// first term the fetch actually queried, whatever the candidate cap does
/// to the rest.
#[tokio::test(start_paused = true)]
async fn rarity_probe_decides_the_order_and_its_expiry_falls_back_to_arrival() {
    let stage_budget = std::time::Duration::from_secs(1);

    // Anchored on the corpus's own surrounding words: a bare `term1` needle
    // is a prefix of `term18` and of `term10`, and would match rows the
    // assertion means to exclude.
    let topic = |term: &str| format!("topic {term} ");
    // Lexicographically first in the candidate list, hence first in the
    // fallback; `term7` is the rarer one, hence first when the probe runs.
    let fallback_first_term = "term1";
    for query in ["term1 term7", "term7 term1"] {
        // Probe allowed to finish: the RARE term is queried first whatever
        // the caller wrote.
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        seed_low_overlap_corpus(&runtime, 1_000, 20).await;
        let ordered = with_lexical_stage_budget_override_ms(
            1_000,
            khive_storage::scope_request_read_deadline(
                stage_budget,
                with_fts_deadline_advance_after_term(
                    1,
                    stage_budget,
                    fetch_fts_candidates(&runtime, "local", query, None, &[], &[], CANDIDATE_POOL),
                ),
            ),
        )
        .await
        .expect("partial fetch");
        let ordered_first = ordered
            .atoms
            .first()
            .map(|atom| atom.content.clone())
            .expect("the probe-intact fetch must produce candidates");
        assert!(
            ordered_first.contains(&topic("term7")),
            "with the probe intact the rarer term is queried first whatever the \
                 caller wrote ({query}); got {} atoms, first content {ordered_first:?}",
            ordered.atoms.len()
        );

        // Probe cut off: the order is the one the caller wrote.
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        seed_low_overlap_corpus(&runtime, 1_000, 20).await;
        let fallback = with_lexical_stage_budget_override_ms(
            1_000,
            khive_storage::scope_request_read_deadline(
                stage_budget,
                with_fts_probe_deadline_advance_after_term(
                    1,
                    std::time::Duration::from_millis(300),
                    with_fts_deadline_advance_after_term(
                        1,
                        stage_budget,
                        fetch_fts_candidates(
                            &runtime,
                            "local",
                            query,
                            None,
                            &[],
                            &[],
                            CANDIDATE_POOL,
                        ),
                    ),
                ),
            ),
        )
        .await
        .expect("partial fetch");
        assert!(
            !fallback.atoms.is_empty(),
            "the fallback must still produce candidates ({query})"
        );
        let fallback_first = fallback
            .atoms
            .first()
            .map(|atom| atom.content.clone())
            .expect("the fallback must still produce candidates");
        assert!(
            fallback_first.contains(&topic(fallback_first_term)),
            "with the probe cut off the fetch must use the candidate-list \
                 order, expected {fallback_first_term} first ({query}); got {} \
                 atoms, first content {fallback_first:?}",
            fallback.atoms.len()
        );
        // And the degradation is still reported rather than read as a miss.
        assert!(
            fallback.timeout.is_some(),
            "a cut-off probe must still be reported ({query})"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn rare_term_survives_stage_expiry_independent_of_query_order() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    seed_low_overlap_corpus(&runtime, 1_000, 20).await;
    let deadline = std::time::Duration::from_secs(1);
    for query in ["term1 term18", "term18 term1"] {
        let outcome = khive_storage::scope_request_read_deadline(
            deadline,
            FTS_TEST_DEADLINE_ADVANCE.scope(
                FtsTestDeadlineAdvance {
                    after_completed_terms: 1,
                    by: deadline,
                },
                fetch_fts_candidates(&runtime, "local", query, None, &[], &[], CANDIDATE_POOL),
            ),
        )
        .await
        .expect("partial fetch");
        assert!(outcome.timeout.is_some());
        assert_eq!(outcome.state, LexicalCandidateState::PartialTimeout);
        assert_eq!(
            outcome.atoms.len(),
            50,
            "the rarer term must complete first"
        );
        assert!(outcome
            .atoms
            .iter()
            .all(|atom| atom.content.contains("term18")));
    }
}

/// Issue #1930: the old OR-joined query returned an all-or-nothing error
/// when it crossed the request deadline. The per-term fetch must instead
/// keep candidates from a completed term and report `timed_out`. Paused
/// Tokio time and the test-only per-term deadline control place expiry
/// exactly between two queries, so the assertion never depends on corpus
/// work taking longer than a machine-specific wall-clock budget.
///
/// Scope: this covers the per-term BOUNDARY only. Advancing only the
/// async clock means the post-boundary term is refused by the
/// pre-registration deadline check, never by an in-flight SQLite
/// interrupt — that path has its own wall-clock test below. The old
/// wall-clock old-query oracle (running the pre-fix OR-joined SQL over
/// this corpus against a tuned budget) is deliberately retired with the
/// machine-timing flake it depended on; the all-or-nothing behavior of a
/// single statement crossing its deadline is what the in-flight test
/// below pins.
#[tokio::test(start_paused = true)]
async fn per_term_fetch_degrades_at_controlled_deadline_boundary() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    const N: u32 = 1_000;
    const VOCAB: u32 = 20;
    seed_low_overlap_corpus(&runtime, N, VOCAB).await;

    let query = "term0 term1";
    let deadline = std::time::Duration::from_millis(650);

    let new_result = khive_storage::scope_request_read_deadline(
        deadline,
        FTS_TEST_DEADLINE_ADVANCE.scope(
            FtsTestDeadlineAdvance {
                after_completed_terms: 1,
                by: deadline,
            },
            fetch_fts_candidates(&runtime, "local", query, None, &[], &[], CANDIDATE_POOL),
        ),
    )
    .await;
    let outcome = new_result.expect(
        "the per-term fetch must return partial degradation when the controlled deadline \
             expires between term queries",
    );
    assert!(
        outcome.timeout.is_some(),
        "the controlled deadline must be observed"
    );
    assert_eq!(outcome.state, LexicalCandidateState::PartialTimeout);
    assert!(
        !outcome.atoms.is_empty(),
        "candidates from the completed term must survive degradation"
    );
    assert!(
        outcome.atoms.len() <= CANDIDATE_POOL,
        "partial pool must respect the fetch cap; got {}",
        outcome.atoms.len()
    );
}

/// Companion to the boundary test above: prove that a wall-clock
/// deadline expiring during this pack's read path surfaces the typed
/// `StorageError::Timeout` — never an untyped error — end to end through
/// the runtime's reader surface. The statement is structurally slow (a
/// 1000^3 cross join — billions of row operations on any machine), so
/// the deadline expires long before it could complete.
///
/// Scope: which arm of the deadline machinery fires is scheduling-
/// dependent — the deadline can latch at reader checkout, at read
/// registration, or mid-statement via the progress handler — and every
/// arm must yield the same typed timeout, which is exactly this test's
/// assertion. The mid-statement arm specifically (progress-handler
/// interrupt of an executing statement, proven by a probe that counts
/// progress callbacks) is deterministically covered where the mechanism
/// lives: `request_deadline_interrupts_statement_without_outer_timeout`
/// in `crates/khive-db/src/sql_bridge.rs`, whose probe assertion fails
/// if SQLite work never started. This test does not re-prove the
/// in-flight arm; it pins the pack-visible contract over all arms.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wall_clock_deadline_on_pack_read_path_surfaces_typed_timeout() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let deadline = std::time::Duration::from_millis(50);
    let result = khive_storage::scope_request_read_deadline(deadline, async {
        let sql = runtime.sql();
        let mut reader = sql.reader().await.expect("reader");
        reader
            .query_all(SqlStatement {
                sql: "WITH RECURSIVE numbers(value) AS (\
                          SELECT 1 UNION ALL SELECT value + 1 FROM numbers WHERE value < 1000\
                          ) SELECT SUM(a.value * b.value * c.value) \
                          FROM numbers AS a CROSS JOIN numbers AS b CROSS JOIN numbers AS c"
                    .into(),
                params: vec![],
                label: Some("knowledge-deadline-probe".into()),
            })
            .await
    })
    .await;
    assert!(
        matches!(result, Err(khive_storage::StorageError::Timeout { .. })),
        "a read crossing the wall-clock deadline must surface the typed \
             timeout whichever deadline arm fires; got {result:?}"
    );
}

/// Issue #1930 rework: the per-term loop used to `break` as soon as the
/// merged pool reached `fetch_limit`, checked *before* querying the next
/// term. A term that sorts first and alone has more matches than
/// `fetch_limit` then fills the pool on its own turn, so every later
/// term never gets queried at all — pool membership depended on query
/// word order. This seeds "alpha" with more rows than `fetch_limit` and
/// a disjoint, small "beta" set, then asserts beta's rows survive into
/// the returned pool. Against the pre-fix early-break loop this must
/// FAIL: alpha's own per-term query (capped at `fetch_limit`) already
/// fills `combined` to `fetch_limit` before the loop reaches "beta", so
/// "beta" is never queried and none of its rows can appear.
#[tokio::test]
async fn round_robin_merge_keeps_later_term_candidates_from_starving() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    {
        let access = runtime.sql();
        let mut writer = access.writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: "WITH RECURSIVE x(n) AS ( \
                              VALUES(0) UNION ALL SELECT n + 1 FROM x WHERE n < 9 \
                          ) \
                          INSERT INTO knowledge_atoms ( \
                              id, namespace, slug, name, content, tags, properties, finalized, \
                              status, source_uri, source_type, created_at, updated_at, deleted_at \
                          ) \
                          SELECT \
                              printf('90000000-0000-0000-0000-%012d', x.n), \
                              'local', printf('alpha-%02d', x.n), printf('Alpha %02d', x.n), \
                              'synthetic content about alpha only', '[]', NULL, 1, \
                              'reviewed', NULL, NULL, x.n, x.n, NULL \
                          FROM x"
                    .to_string(),
                params: Vec::new(),
                label: None,
            })
            .await
            .expect("seed alpha rows");
        writer
            .execute(SqlStatement {
                sql: "WITH RECURSIVE x(n) AS ( \
                              VALUES(0) UNION ALL SELECT n + 1 FROM x WHERE n < 2 \
                          ) \
                          INSERT INTO knowledge_atoms ( \
                              id, namespace, slug, name, content, tags, properties, finalized, \
                              status, source_uri, source_type, created_at, updated_at, deleted_at \
                          ) \
                          SELECT \
                              printf('91000000-0000-0000-0000-%012d', x.n), \
                              'local', printf('beta-%02d', x.n), printf('Beta %02d', x.n), \
                              'synthetic content about beta only', '[]', NULL, 1, \
                              'reviewed', NULL, NULL, x.n, x.n, NULL \
                          FROM x"
                    .to_string(),
                params: Vec::new(),
                label: None,
            })
            .await
            .expect("seed beta rows");
    }

    let fetch_limit = 5;
    let outcome =
        fetch_fts_candidates(&runtime, "local", "alpha beta", None, &[], &[], fetch_limit)
            .await
            .expect("fetch must not error");
    assert!(outcome.timeout.is_none());
    assert_eq!(outcome.state, LexicalCandidateState::Matched);
    assert_eq!(outcome.atoms.len(), fetch_limit);

    let beta_present = outcome
        .atoms
        .iter()
        .any(|atom| atom.slug.starts_with("beta-"));
    assert!(
        beta_present,
        "round-robin merge must keep the second term's candidates in the pool \
             even though the first term alone has more matches than fetch_limit; \
             got {:?}",
        outcome
            .atoms
            .iter()
            .map(|a| a.slug.as_str())
            .collect::<Vec<_>>()
    );
}

/// Issue #1930 Amendment 2: the lexical stage's own budget must not
/// cancel the rest of the request when it expires. Paused Tokio time
/// plus the existing per-term deadline control place the expiry
/// deterministically between two term queries, inside a nested
/// `scope_request_read_deadline` call mirroring exactly what
/// `search_core` does around `fetch_fts_candidates` (a narrow stage
/// scope nested inside a much longer outer one).
///
/// Before this change, `search_core` called `fetch_fts_candidates`
/// directly under whatever deadline the caller installed, so this same
/// expiry — with no separate inner scope to pop back out of — left the
/// *outer* deadline expired too, and `ensure_request_read_active`
/// called afterward, still nested in that one shared scope, returned
/// `Err`. Confirmed by running this test against the fetch called
/// directly (no stage-budget wrap) before adding the wrap: `still_active`
/// came back `Err(Timeout { .. })` — this assertion is the red-before
/// case for that call shape; the wrap makes it green.
#[tokio::test(start_paused = true)]
async fn lexical_stage_budget_expiry_does_not_cancel_the_request() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    const N: u32 = 1_000;
    const VOCAB: u32 = 20;
    seed_low_overlap_corpus(&runtime, N, VOCAB).await;

    let query = "term0 term1";
    // This test wraps `fetch_fts_candidates` in its own explicit
    // `scope_request_read_deadline`, exactly mirroring what `search_core`
    // does around it in production — it never goes through
    // `lexical_stage_budget()`'s override seam, so a literal duration is
    // enough here.
    let budget = std::time::Duration::from_millis(50);
    let outer_deadline = std::time::Duration::from_secs(60);

    let (fetch, still_active) = khive_storage::scope_request_read_deadline(outer_deadline, async {
        let fetch = khive_storage::scope_request_read_deadline(
            budget,
            FTS_TEST_DEADLINE_ADVANCE.scope(
                FtsTestDeadlineAdvance {
                    after_completed_terms: 1,
                    by: budget,
                },
                fetch_fts_candidates(&runtime, "local", query, None, &[], &[], CANDIDATE_POOL),
            ),
        )
        .await
        .expect("the fetch must not hard-error on its own stage budget");

        let still_active = khive_storage::ensure_request_read_active("test.lexical_stage_budget");
        (fetch, still_active)
    })
    .await;

    assert!(
        fetch.timeout.is_some(),
        "the lexical stage's own narrower budget must be observed"
    );
    assert!(
        !fetch.atoms.is_empty(),
        "candidates from the completed term must survive the stage degradation"
    );
    assert!(
        still_active.is_ok(),
        "the outer request deadline must still be active once the lexical \
             stage's own budget expires and its scope returns; got {still_active:?}"
    );
}

#[test]
fn lexical_stage_allowance_scales_only_with_admitted_terms_and_is_capped() {
    let base = lexical_stage_budget();
    let budget = FtsTermBudget::new();
    assert_eq!(
        lexical_stage_budget_for_terms(base, budget.available_for("zzmass")),
        base,
    );
    assert_eq!(
        lexical_stage_budget_for_terms(base, budget.available_for("zzmass zzlass")),
        std::time::Duration::from_millis(2_500),
    );
    let many = distinct_term_query(FTS_TERM_COUNT_LIMIT * 2);
    assert_eq!(
        lexical_stage_budget_for_terms(base, budget.available_for(&many)),
        std::time::Duration::from_millis(8_000),
    );
    budget.admit(fts5_candidate_terms(&many));
    assert_eq!(budget.available_for("zzmass zzlass"), 0);
    assert_eq!(lexical_stage_budget_for_terms(base, 0), base);
}

/// A second bounded term read must have time to run after the original
/// 2 s single-term allowance has elapsed. The clock moves only after the
/// first term's rows are collected, so the old fixed 2 s stage returned a
/// partial timeout here even though both local terms have cheap matches.
#[tokio::test(start_paused = true)]
async fn multi_term_search_keeps_lexical_candidates_after_single_term_budget() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let access = runtime.sql();
    let mut writer = access.writer().await.expect("writer");
    writer
            .execute(SqlStatement {
                sql: "INSERT INTO knowledge_atoms \
                      (id, namespace, slug, name, content, tags, finalized, status, created_at, updated_at) \
                      VALUES \
                      ('92700000-0000-0000-0000-000000000011', 'local', 'zzmass-row', \
                       'First term', 'zzmass content', '[]', 1, 'reviewed', 0, 0), \
                      ('92700000-0000-0000-0000-000000000012', 'local', 'zzlass-row', \
                       'Second term', 'zzlass content', '[]', 1, 'reviewed', 0, 0)"
                    .into(),
                params: Vec::new(),
                label: None,
            })
            .await
            .expect("seed two independently matching terms");
    drop(writer);

    let token = runtime.authorize(Namespace::local()).expect("local token");
    let response = with_fts_deadline_advance_after_term(
        1,
        std::time::Duration::from_millis(2_100),
        KnowledgeHandlers::search(
            &runtime,
            &token,
            json!({"query": "zzmass zzlass", "rerank": false}),
            &vamana::new_shared(),
        ),
    )
    .await
    .expect("multi-term search");
    assert_eq!(response["candidate_provenance"]["lexical"], "matched");
    assert_eq!(response["total"], 2);
}

/// Companion pair for issue #1930 Amendment 2's phase-A overfetch/widen
/// behavior. A term whose top bm25/rowid page is entirely status-
/// ineligible must still surface its eligible rows once widening looks
/// past that page — 30 `deprecated` rows are inserted first (lower
/// rowids, so they sort first at equal bm25) and 3 `reviewed` rows
/// inserted after (higher rowids). `fetch_limit=5` with a single real
/// term gives `per_term_limit=5` and a first-round probe of `5*4=20`,
/// which the 30 deprecated rows alone fill — round one must see zero
/// eligible rows, forcing the widen arm to reach the 3 reviewed rows in
/// round two (probe 80, corpus only has 33 total so phase A returns
/// fewer than it asked for, correctly stopping further widening).
#[tokio::test]
async fn phase_b_widening_recovers_eligible_rows_behind_an_ineligible_top_page() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let access = runtime.sql();
    let mut writer = access.writer().await.expect("writer");
    writer
            .execute(SqlStatement {
                sql: "WITH RECURSIVE x(n) AS ( \
                          VALUES(0) UNION ALL SELECT n + 1 FROM x WHERE n < 29 \
                      ) \
                      INSERT INTO knowledge_atoms ( \
                          id, namespace, slug, name, content, tags, properties, finalized, \
                          status, source_uri, source_type, created_at, updated_at, deleted_at \
                      ) \
                      SELECT \
                          printf('92000000-0000-0000-0000-%012d', x.n), \
                          'local', printf('zzwidenzz-dep-%02d', x.n), printf('Widen Dep %02d', x.n), \
                          'synthetic content about zzwidenzz only', '[]', NULL, 1, \
                          'deprecated', NULL, NULL, x.n, x.n, NULL \
                      FROM x"
                    .to_string(),
                params: Vec::new(),
                label: None,
            })
            .await
            .expect("seed deprecated widen rows");
    writer
        .execute(SqlStatement {
            sql: "WITH RECURSIVE x(n) AS ( \
                          VALUES(0) UNION ALL SELECT n + 1 FROM x WHERE n < 2 \
                      ) \
                      INSERT INTO knowledge_atoms ( \
                          id, namespace, slug, name, content, tags, properties, finalized, \
                          status, source_uri, source_type, created_at, updated_at, deleted_at \
                      ) \
                      SELECT \
                          printf('92100000-0000-0000-0000-%012d', x.n), \
                          'local', printf('zzwidenzz-ok-%02d', x.n), printf('Widen Ok %02d', x.n), \
                          'synthetic content about zzwidenzz only', '[]', NULL, 1, \
                          'reviewed', NULL, NULL, 100 + x.n, 100 + x.n, NULL \
                      FROM x"
                .to_string(),
            params: Vec::new(),
            label: None,
        })
        .await
        .expect("seed reviewed widen rows");
    drop(writer);

    let outcome = fetch_fts_candidates(&runtime, "local", "zzwidenzz", None, &[], &[], 5)
        .await
        .expect("fetch must not error");

    assert!(outcome.timeout.is_none());
    assert_eq!(
        outcome.atoms.len(),
        3,
        "only the 3 reviewed rows are eligible; widening must have looked \
             past the all-deprecated first page to find them: {:?}",
        outcome
            .atoms
            .iter()
            .map(|a| a.slug.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        outcome
            .atoms
            .iter()
            .all(|a| a.slug.starts_with("zzwidenzz-ok-")),
        "no deprecated row may survive into the eligible set: {:?}",
        outcome
            .atoms
            .iter()
            .map(|a| a.slug.as_str())
            .collect::<Vec<_>>()
    );
}

/// Control for the widening test above: a term with genuinely fewer
/// matches than `per_term_limit` (2 reviewed rows, cap 5) must return
/// exactly those 2 without fabricating more. Phase A's first probe
/// (`5*4=20`) already exceeds the corpus size for this term, so it
/// returns short on round one and widening never triggers at all — this
/// is what proves `phase_a_full` correctly detects exhaustion instead
/// of retrying up to the ceiling.
#[tokio::test]
async fn phase_b_accepts_a_genuine_shortfall_without_widening() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let access = runtime.sql();
    let mut writer = access.writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO knowledge_atoms ( \
                          id, namespace, slug, name, content, tags, properties, finalized, \
                          status, source_uri, source_type, created_at, updated_at, deleted_at \
                      ) VALUES \
                      ('92200000-0000-0000-0000-000000000000', 'local', 'zzsparsezz-00', \
                       'Sparse 00', 'synthetic content about zzsparsezz only', '[]', NULL, 1, \
                       'reviewed', NULL, NULL, 0, 0, NULL), \
                      ('92200000-0000-0000-0000-000000000001', 'local', 'zzsparsezz-01', \
                       'Sparse 01', 'synthetic content about zzsparsezz only', '[]', NULL, 1, \
                       'reviewed', NULL, NULL, 1, 1, NULL)"
                .to_string(),
            params: Vec::new(),
            label: None,
        })
        .await
        .expect("seed sparse rows");
    drop(writer);

    let outcome = fetch_fts_candidates(&runtime, "local", "zzsparsezz", None, &[], &[], 5)
        .await
        .expect("fetch must not error");

    assert!(outcome.timeout.is_none());
    assert_eq!(
        outcome.atoms.len(),
        2,
        "a term with only 2 genuinely eligible matches must return exactly \
             those, not fabricate more via widening: {:?}",
        outcome
            .atoms
            .iter()
            .map(|a| a.slug.as_str())
            .collect::<Vec<_>>()
    );
}

/// Phase A carries no namespace predicate (it is index-only); the
/// namespace check moves entirely to phase B's hydration query. Seed a
/// matching row in another namespace and confirm it is hydrated out —
/// never returned — even though phase A's rowid list spans both
/// namespaces.
#[tokio::test]
async fn phase_a_cross_namespace_matches_are_hydrated_out() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let access = runtime.sql();
    let mut writer = access.writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO knowledge_atoms ( \
                          id, namespace, slug, name, content, tags, properties, finalized, \
                          status, source_uri, source_type, created_at, updated_at, deleted_at \
                      ) VALUES \
                      ('92300000-0000-0000-0000-000000000000', 'local', 'zzcrossns-local', \
                       'Cross NS Local', 'synthetic content about zzcrossns only', '[]', NULL, 1, \
                       'reviewed', NULL, NULL, 0, 0, NULL), \
                      ('92300000-0000-0000-0000-000000000001', 'other', 'zzcrossns-other', \
                       'Cross NS Other', 'synthetic content about zzcrossns only', '[]', NULL, 1, \
                       'reviewed', NULL, NULL, 1, 1, NULL)"
                .to_string(),
            params: Vec::new(),
            label: None,
        })
        .await
        .expect("seed cross-namespace rows");
    drop(writer);

    let outcome = fetch_fts_candidates(&runtime, "local", "zzcrossns", None, &[], &[], 10)
        .await
        .expect("fetch must not error");

    assert!(outcome.timeout.is_none());
    assert_eq!(
        outcome.atoms.len(),
        1,
        "only the local-namespace row is eligible: {:?}",
        outcome
            .atoms
            .iter()
            .map(|a| (a.slug.as_str(), a.namespace.as_str()))
            .collect::<Vec<_>>()
    );
    assert_eq!(outcome.atoms[0].namespace, "local");
    assert_eq!(outcome.atoms[0].slug, "zzcrossns-local");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual 200000-row lexical namespace measurement"]
async fn measure_cross_namespace_lexical_work() {
    async fn seed_namespace(
        runtime: &KhiveRuntime,
        ns: &str,
        id_prefix: &str,
        rows: i64,
        matching_rows: i64,
    ) {
        let access = runtime.sql();
        let mut writer = access.writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: "WITH RECURSIVE seq(n) AS ( \
                              VALUES(0) UNION ALL SELECT n + 1 FROM seq WHERE n + 1 < ?3 \
                          ) \
                          INSERT INTO knowledge_atoms ( \
                              id, namespace, slug, name, content, tags, finalized, \
                              status, created_at, updated_at \
                          ) \
                          SELECT printf('%s-%012d', ?2, n), ?1, \
                                 printf('measurement-%06d', n), 'Measurement Row', \
                                 CASE WHEN n < ?4 THEN \
                                     'namespacechannel content with ordinary padding text' \
                                 ELSE 'unrelated content with ordinary padding text' END, \
                                 '[]', 1, 'reviewed', n, n FROM seq"
                    .into(),
                params: vec![
                    SqlValue::Text(ns.into()),
                    SqlValue::Text(id_prefix.into()),
                    SqlValue::Integer(rows),
                    SqlValue::Integer(matching_rows),
                ],
                label: None,
            })
            .await
            .expect("seed namespace corpus");
    }

    for local_matches in [0, 3] {
        for foreign_present in [true, false] {
            let runtime = KhiveRuntime::memory().expect("in-memory runtime");
            if foreign_present {
                seed_namespace(
                    &runtime,
                    "tenant-a",
                    "92500000-0000-0000-0000",
                    200_000,
                    200_000,
                )
                .await;
            }
            seed_namespace(
                &runtime,
                "tenant-b",
                "92600000-0000-0000-0000",
                10,
                local_matches,
            )
            .await;

            for (mode, query) in [
                ("single", "namespacechannel"),
                ("multi", "namespacechannel absentchannel"),
            ] {
                let start = std::time::Instant::now();
                let outcome = khive_storage::scope_request_read_deadline(
                    lexical_stage_budget(),
                    fetch_fts_candidates(
                        &runtime,
                        "tenant-b",
                        query,
                        None,
                        &[],
                        &[],
                        CANDIDATE_POOL,
                    ),
                )
                .await
                .expect("bounded lexical fetch");
                let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                assert!(outcome
                    .atoms
                    .iter()
                    .all(|atom| atom.namespace == "tenant-b"));
                println!(
                        "LEXICAL_NAMESPACE mode={mode} foreign_present={foreign_present} local_matches={local_matches} rows={} lexical_elapsed_ms={elapsed_ms:.3} lexical_timeout={}",
                        outcome.atoms.len(),
                        outcome.timeout.is_some(),
                    );
            }
        }
    }
}

/// Foreign FTS matches must not change the public candidate state or
/// admit unrelated local rows. This retains the #2396 namespace boundary
/// after removing the recent-row fallback.
#[tokio::test]
async fn empty_result_fallback_never_leaks_a_foreign_namespace_match() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let access = runtime.sql();
    let mut writer = access.writer().await.expect("writer");
    writer
            .execute(SqlStatement {
                sql: "INSERT INTO knowledge_atoms ( \
                          id, namespace, slug, name, content, tags, properties, finalized, \
                          status, source_uri, source_type, created_at, updated_at, deleted_at \
                      ) VALUES \
                      ('92400000-0000-0000-0000-000000000000', 'local', 'unrelated-local-atom', \
                       'Unrelated Local Atom', 'generic filler text with no special term', \
                       '[]', NULL, 1, 'reviewed', NULL, NULL, 0, 0, NULL), \
                      ('92400000-0000-0000-0000-000000000001', 'tenant-b', 'foreign-term-atom', \
                       'Foreign Term Atom', 'synthetic content about zzoraclezz only', '[]', NULL, 1, \
                       'reviewed', NULL, NULL, 1, 1, NULL)"
                    .to_string(),
                params: Vec::new(),
                label: None,
            })
            .await
            .expect("seed oracle rows");
    drop(writer);

    let foreign_match = fetch_fts_candidates(&runtime, "local", "zzoraclezz", None, &[], &[], 5)
        .await
        .expect("fetch must not error");
    let no_match = fetch_fts_candidates(&runtime, "local", "zzoracleabsentzz", None, &[], &[], 5)
        .await
        .expect("fetch must not error");

    assert!(foreign_match.timeout.is_none());
    assert!(no_match.timeout.is_none());
    assert_eq!(foreign_match.state, LexicalCandidateState::NoMatch);
    assert_eq!(foreign_match.state, no_match.state);
    let foreign_slugs: Vec<&str> = foreign_match
        .atoms
        .iter()
        .map(|a| a.slug.as_str())
        .collect();
    let absent_slugs: Vec<&str> = no_match.atoms.iter().map(|a| a.slug.as_str()).collect();
    assert_eq!(
        foreign_slugs, absent_slugs,
        "a term matching only in another namespace must produce the exact \
             same response as a term matching nowhere at all — any \
             difference is a cross-namespace existence oracle"
    );
    assert!(
        foreign_slugs.is_empty(),
        "unrelated recent rows are not candidates"
    );
}

/// A real indexed match survives, without unrelated recent rows.
#[tokio::test]
async fn empty_result_fallback_control_index_match_skips_fallback() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let access = runtime.sql();
    let mut writer = access.writer().await.expect("writer");
    writer
            .execute(SqlStatement {
                sql: "INSERT INTO knowledge_atoms ( \
                          id, namespace, slug, name, content, tags, properties, finalized, \
                          status, source_uri, source_type, created_at, updated_at, deleted_at \
                      ) VALUES \
                      ('92400000-0000-0000-0000-000000000002', 'local', 'indexed-term-atom', \
                       'Indexed Term Atom', 'synthetic content about zzindexedzz only', '[]', NULL, 1, \
                       'reviewed', NULL, NULL, 0, 0, NULL), \
                      ('92400000-0000-0000-0000-000000000003', 'local', 'unrelated-recent-atom', \
                       'Unrelated Recent Atom', 'generic filler text with no special term', '[]', NULL, 1, \
                       'reviewed', NULL, NULL, 1, 1, NULL)"
                    .to_string(),
                params: Vec::new(),
                label: None,
            })
            .await
            .expect("seed control rows");
    drop(writer);

    let outcome = fetch_fts_candidates(&runtime, "local", "zzindexedzz", None, &[], &[], 5)
        .await
        .expect("fetch must not error");

    assert!(outcome.timeout.is_none());
    assert_eq!(
        outcome.atoms.len(),
        1,
        "only the indexed match may be returned, never the unrelated \
             fallback-only row: {:?}",
        outcome
            .atoms
            .iter()
            .map(|a| a.slug.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(outcome.atoms[0].slug, "indexed-term-atom");
    assert_eq!(outcome.state, LexicalCandidateState::Matched);
}

#[tokio::test]
async fn filtered_candidate_state_is_stable_behind_a_foreign_ceiling_prefix() {
    let mut public_responses = Vec::new();
    for foreign_prefix in [false, true] {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let access = runtime.sql();
        let mut writer = access.writer().await.expect("writer");
        if foreign_prefix {
            writer
                .execute(SqlStatement {
                    sql: "WITH RECURSIVE x(n) AS ( \
                                  VALUES(1) UNION ALL SELECT n + 1 FROM x WHERE n < 20 \
                              ) \
                              INSERT INTO knowledge_atoms ( \
                                  rowid, id, namespace, slug, name, content, tags, finalized, \
                                  status, created_at, updated_at \
                              ) \
                              SELECT x.n, printf('92710000-0000-0000-0000-%012d', x.n), \
                                  'foreign', printf('foreign-prefix-%02d', x.n), 'Foreign Match', \
                                  'zzfilteredceilingzz', '[]', 1, 'reviewed', 0, 0 \
                              FROM x"
                        .into(),
                    params: Vec::new(),
                    label: None,
                })
                .await
                .expect("seed foreign ceiling prefix");
        }
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO knowledge_atoms ( \
                              rowid, id, namespace, slug, name, content, tags, finalized, \
                              status, created_at, updated_at \
                          ) VALUES ( \
                              21, '92710000-0000-0000-0000-000000000021', 'local', \
                              'local-filtered-match', 'Local Filtered Match', \
                              'zzfilteredceilingzz', '[]', 0, 'draft', 0, 0 \
                          )"
                .into(),
                params: Vec::new(),
                label: None,
            })
            .await
            .expect("seed identical local filtered match");
        drop(writer);

        let mut reader = access.reader().await.expect("reader");
        let rows = reader
            .query_all(phase_a_rowids_statement("\"zzfilteredceilingzz\"", 20))
            .await
            .expect("inspect ceiling window");
        let rowids: Vec<_> = rows
            .iter()
            .filter_map(|row| row_i64(row, "rowid"))
            .collect();
        if foreign_prefix {
            assert_eq!(rowids, (1..=20).collect::<Vec<i64>>());
        } else {
            assert_eq!(rowids, [21]);
        }
        drop(reader);

        let token = runtime.authorize(Namespace::local()).expect("local token");
        let ann = vamana::new_shared();
        let response = with_phase_a_widen_ceiling_override(20, async {
            KnowledgeHandlers::search(
                &runtime,
                &token,
                json!({"query": "zzfilteredceilingzz", "rerank": false}),
                &ann,
            )
            .await
        })
        .await
        .expect("public filtered search");
        assert_eq!(
            response,
            json!({
                "results": [], "total": 0,
                "candidate_provenance": {"lexical": "filtered", "fallback": "none", "terms_truncated": false},
            }),
            "foreign matches must not change the same local filtered result: {foreign_prefix}"
        );
        public_responses.push(serde_json::to_vec(&response).expect("serialize public response"));
    }
    assert_eq!(public_responses[0], public_responses[1]);
}

/// Issue #2396 fix 2: when phase A's widening reaches the ceiling and the
/// eligible set is still short, more than `ceiling` ineligible top-ranked
/// rows must not be allowed to hide an eligible row further down the
/// bm25 ranking than phase A ever probed. The ceiling is overridden down
/// to 20 so the fixture can stay small: 20 `deprecated` rows (lower
/// rowids, so they sort first at equal bm25) exactly fill the first — and
/// at this override, only — probe window, so `eligible_now` is empty
/// right at the ceiling. The eligibility-scoped fallback query must then
/// recover the 2 `reviewed` rows seeded after them (higher rowids, never
/// inside the ceiling-bounded window). The ordinary (non-ceiling)
/// widening path and its shortfall control are covered by
/// `phase_b_widening_recovers_eligible_rows_behind_an_ineligible_top_page`
/// and `phase_b_accepts_a_genuine_shortfall_without_widening` above.
#[tokio::test]
async fn phase_b_ceiling_exhaustion_recovers_eligible_row_via_scoped_fallback() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let access = runtime.sql();
    let mut writer = access.writer().await.expect("writer");
    writer
            .execute(SqlStatement {
                sql: "WITH RECURSIVE x(n) AS ( \
                          VALUES(0) UNION ALL SELECT n + 1 FROM x WHERE n < 19 \
                      ) \
                      INSERT INTO knowledge_atoms ( \
                          id, namespace, slug, name, content, tags, properties, finalized, \
                          status, source_uri, source_type, created_at, updated_at, deleted_at \
                      ) \
                      SELECT \
                          printf('92500000-0000-0000-0000-%012d', x.n), \
                          'local', printf('zzceilingzz-dep-%02d', x.n), printf('Ceiling Dep %02d', x.n), \
                          'synthetic content about zzceilingzz only', '[]', NULL, 1, \
                          'deprecated', NULL, NULL, x.n, x.n, NULL \
                      FROM x"
                    .to_string(),
                params: Vec::new(),
                label: None,
            })
            .await
            .expect("seed deprecated ceiling rows");
    writer
            .execute(SqlStatement {
                sql: "WITH RECURSIVE x(n) AS ( \
                          VALUES(0) UNION ALL SELECT n + 1 FROM x WHERE n < 1 \
                      ) \
                      INSERT INTO knowledge_atoms ( \
                          id, namespace, slug, name, content, tags, properties, finalized, \
                          status, source_uri, source_type, created_at, updated_at, deleted_at \
                      ) \
                      SELECT \
                          printf('92510000-0000-0000-0000-%012d', x.n), \
                          'local', printf('zzceilingzz-ok-%02d', x.n), printf('Ceiling Ok %02d', x.n), \
                          'synthetic content about zzceilingzz only', '[]', NULL, 1, \
                          'reviewed', NULL, NULL, 100 + x.n, 100 + x.n, NULL \
                      FROM x"
                    .to_string(),
                params: Vec::new(),
                label: None,
            })
            .await
            .expect("seed reviewed ceiling rows");
    drop(writer);

    let outcome = with_phase_a_widen_ceiling_override(20, async {
        fetch_fts_candidates(&runtime, "local", "zzceilingzz", None, &[], &[], 5).await
    })
    .await
    .expect("fetch must not error");

    assert!(outcome.timeout.is_none());
    assert_eq!(
        outcome.atoms.len(),
        2,
        "the 2 reviewed rows sit beyond the ceiling-bounded phase-A \
             window; only the eligibility-scoped fallback query can recover \
             them: {:?}",
        outcome
            .atoms
            .iter()
            .map(|a| a.slug.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        outcome
            .atoms
            .iter()
            .all(|a| a.slug.starts_with("zzceilingzz-ok-")),
        "no deprecated row may survive into the eligible set: {:?}",
        outcome
            .atoms
            .iter()
            .map(|a| a.slug.as_str())
            .collect::<Vec<_>>()
    );
}

/// A member-token-sizing timeout returns no measurements, never a
/// placeholder zero that could be admitted as a free fold candidate.
#[tokio::test]
async fn member_token_sizes_report_timeout_instead_of_a_measured_zero() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let access = runtime.sql();
    let mut writer = access.writer().await.expect("writer");
    writer
            .execute(SqlStatement {
                sql: "INSERT INTO knowledge_atoms ( \
                          id, namespace, slug, name, content, tags, properties, finalized, \
                          status, source_uri, source_type, created_at, updated_at, deleted_at \
                      ) VALUES ( \
                          '92600000-0000-0000-0000-000000000000', 'local', 'sizing-member-atom', \
                          'Sizing Member Atom', \
                          'enough body content to price a non-zero token size for the owning domain once member sizing actually runs to completion', \
                          '[]', NULL, 1, 'reviewed', NULL, NULL, 0, 0, NULL)"
                    .to_string(),
                params: Vec::new(),
                label: None,
            })
            .await
            .expect("seed member atom");
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO knowledge_domains ( \
                          id, namespace, slug, name, description, tags, members, status, \
                          created_at, updated_at, deleted_at \
                      ) VALUES ( \
                          '92600000-0000-0000-0000-000000000001', 'local', 'sizing-domain', \
                          'Sizing Domain', NULL, '[]', '[\"sizing-member-atom\"]', 'reviewed', \
                          0, 0, NULL)"
                .to_string(),
            params: Vec::new(),
            label: None,
        })
        .await
        .expect("seed domain");
    drop(writer);

    let domain_ids = vec!["92600000-0000-0000-0000-000000000001".to_string()];

    let (degraded_sizes, timed_out) = khive_storage::scope_request_read_deadline(
        std::time::Duration::ZERO,
        load_domain_member_token_sizes(&runtime, "local", &domain_ids),
    )
    .await
    .expect("an expired read deadline must degrade, not error");
    assert!(
        timed_out,
        "an expired read deadline must be reported as unmeasured"
    );
    assert!(
        degraded_sizes.is_empty(),
        "a timed-out batch must not contain fabricated measurements"
    );

    let (healthy_sizes, healthy_timed_out) =
        load_domain_member_token_sizes(&runtime, "local", &domain_ids)
            .await
            .expect("undeadlined lookup must succeed");
    assert!(!healthy_timed_out);
    assert!(
        healthy_sizes
            .get(&domain_ids[0])
            .is_some_and(|sizing| sizing.tokens > 0 && sizing.live_members == 1),
        "control: without a deadline the lookup measures the real member \
             body cost; got {healthy_sizes:?}"
    );
}

/// Issue #2396 fix 4: the lexical-stage-budget override rides a
/// `tokio::task_local!`, so it is scoped to the task it wraps only.
/// Two concurrent tasks — one scoped to an override, one with none —
/// must observe different budgets; the prior process-global `AtomicU64`
/// override could not guarantee this; a task with no override of its own
/// could observe whatever value another concurrently running test last
/// stored.
#[tokio::test]
async fn lexical_stage_budget_override_does_not_leak_across_concurrent_tasks() {
    let with_override = tokio::spawn(with_lexical_stage_budget_override_ms(50, async {
        tokio::task::yield_now().await;
        lexical_stage_budget()
    }));
    let without_override = tokio::spawn(async {
        tokio::task::yield_now().await;
        lexical_stage_budget()
    });

    let overridden = with_override.await.expect("task must not panic");
    let baseline = without_override.await.expect("task must not panic");

    assert_eq!(overridden, std::time::Duration::from_millis(50));
    assert_eq!(
        baseline,
        std::time::Duration::from_millis(LEXICAL_STAGE_BUDGET_MS),
        "a concurrently running task with no override of its own must \
             never observe another task's override; got {baseline:?}"
    );
}

/// Issue: `MIN_TERM_LEN=3` drops every token of a query like "AI" before
/// FTS ever sees it, and the trigram tokenizer cannot match a phrase that
/// short either, so the atom was unreachable without ANN. The indexed
/// slug probe in the lexical stage must restore discoverability for both the
/// atom's exact-case name and a case-insensitive spelling, while a query
/// that matches no slug at all must still report a genuine miss.
#[tokio::test]
async fn short_exact_name_is_discoverable_without_ann() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    {
        let access = runtime.sql();
        let mut writer = access.writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO knowledge_atoms ( \
                              id, namespace, slug, name, content, tags, properties, finalized, \
                              status, source_uri, source_type, created_at, updated_at, deleted_at \
                          ) VALUES ( \
                              '93000000-0000-0000-0000-000000000001', 'local', \
                              'ai', 'AI', \
                              'artificial intelligence overview content for the corpus', '[]', \
                              NULL, 1, 'reviewed', NULL, NULL, 1000, 1000, NULL \
                          )"
                .to_string(),
                params: Vec::new(),
                label: None,
            })
            .await
            .expect("seed short-name atom");
    }

    let token = runtime.authorize(Namespace::local()).expect("local token");
    let ann = vamana::new_shared();

    let exact_case = KnowledgeHandlers::search(
        &runtime,
        &token,
        json!({"query": "AI", "rerank": false}),
        &ann,
    )
    .await
    .expect("exact-case short-name search must not error");
    assert_eq!(exact_case["total"], 1);
    assert_eq!(exact_case["results"][0]["slug"], "ai");
    assert_eq!(exact_case["candidate_provenance"]["lexical"], "exact_name");
    assert_eq!(exact_case["candidate_provenance"]["fallback"], "none");
    assert_eq!(
        exact_case["results"][0]["score_provenance"]["sources"],
        json!(["lexical"])
    );
    assert!(exact_case["results"][0]["score"].as_f64().unwrap() > 0.0);

    let lower_case = KnowledgeHandlers::search(
        &runtime,
        &token,
        json!({"query": "ai", "rerank": false}),
        &ann,
    )
    .await
    .expect("lower-case short-name search must not error");
    assert_eq!(lower_case["total"], 1);
    assert_eq!(lower_case["results"][0]["slug"], "ai");
    assert_eq!(lower_case["candidate_provenance"]["lexical"], "exact_name");

    let miss = KnowledgeHandlers::search(
        &runtime,
        &token,
        json!({"query": "zz", "rerank": false}),
        &ann,
    )
    .await
    .expect("non-matching short query must not error");
    assert_eq!(miss["total"], 0);
    assert_eq!(miss["candidate_provenance"]["lexical"], "no_match");

    // A role prefix is scored, never searched: it must not hide the probe.
    let role_qualified = KnowledgeHandlers::search(
        &runtime,
        &token,
        json!({"query": "AI", "role": "researcher", "rerank": false}),
        &ann,
    )
    .await
    .expect("role-qualified short-name search must not error");
    assert_eq!(role_qualified["total"], 1);
    assert_eq!(role_qualified["results"][0]["slug"], "ai");
    assert_eq!(
        role_qualified["candidate_provenance"]["lexical"],
        "exact_name"
    );
}

/// A deadline that expires inside the exact-name probe is a lexical
/// timeout, not a miss: the caller degrades the response on `TimedOut`
/// and would otherwise reach the final active-read check and error.
#[tokio::test]
async fn exact_name_probe_reports_an_expired_deadline_as_a_timeout() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let sql = runtime.sql();
    let mut reader = sql.reader().await.expect("reader before deadline");
    let configured = lexical_stage_budget();

    let expired = khive_storage::scope_request_read_deadline(std::time::Duration::ZERO, async {
        let mut stage =
            LexicalStage::new(LexicalPass::Full, tokio::time::Instant::now(), configured);
        let outcome =
            fetch_exact_name_candidate(reader.as_mut(), "local", "AI", None, &[], &[], &mut stage)
                .await;
        let timeout = stage.timeout.expect("exact probe captures timeout details");
        assert_eq!(timeout.phase, LexicalPhase::ExactNameProbe);
        assert_eq!(timeout.effective_budget_ms, 0);
        outcome
    })
    .await;
    assert!(
        matches!(expired, Ok(ExactNameProbe::TimedOut)),
        "an expired read deadline must surface as TimedOut, never as a \
             miss; got {expired:?}"
    );

    let mut stage = LexicalStage::new(LexicalPass::Full, tokio::time::Instant::now(), configured);
    let healthy =
        fetch_exact_name_candidate(reader.as_mut(), "local", "AI", None, &[], &[], &mut stage)
            .await
            .expect("undeadlined probe must succeed");
    assert!(
        matches!(healthy, ExactNameProbe::Miss),
        "control: without a deadline an unseeded name is a plain miss; got {healthy:?}"
    );
    assert!(stage.timeout.is_none());
}

/// The indexed exact-name probe must use the unique `(namespace, slug)`
/// index, never a full-namespace scan on `name` — no such index exists.
/// Same `EXPLAIN QUERY PLAN` style as
/// `crud::get_prefix_query_plan_uses_primary_key_range_seeks`.
#[tokio::test]
async fn exact_name_probe_query_plan_uses_slug_index() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut reader = runtime.sql().reader().await.expect("exact-name reader");
    let rows = reader
        .explain(exact_name_statement(
            "local",
            "ai",
            Some("atom"),
            &[],
            &["draft", "deprecated"],
        ))
        .await
        .expect("explain exact-name probe");
    let details: Vec<String> = rows
        .iter()
        .filter_map(|row| match row.get("detail") {
            Some(SqlValue::Text(detail)) => Some(detail.clone()),
            _ => None,
        })
        .collect();
    assert!(
        details.iter().any(|d| d.contains("SEARCH knowledge_atoms")
            && d.contains("USING INDEX")
            && d.contains("namespace=? AND slug=?")),
        "exact-name probe must use an index seek, not a table scan: {details:?}"
    );
    assert!(
        !details.iter().any(|d| d.contains("SCAN knowledge_atoms")),
        "exact-name probe must never full-scan knowledge_atoms: {details:?}"
    );
}

#[tokio::test]
async fn short_exact_name_probe_respects_namespace_status_type_and_soft_deletion() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let sql = runtime.sql();
    let mut writer = sql.writer().await.unwrap();
    for (index, (ns, slug, name, status, tags, deleted)) in [
        ("local", "ai", "AI", "draft", "[]", SqlValue::Null),
        ("tenant-b", "ai", "AI", "reviewed", "[]", SqlValue::Null),
        (
            "local",
            "ml",
            "ML",
            "reviewed",
            "[\"type:domain\"]",
            SqlValue::Null,
        ),
        (
            "local",
            "zz",
            "ZZ",
            "reviewed",
            "[]",
            SqlValue::Integer(1000),
        ),
        (
            "local",
            "custom-name",
            "UI",
            "reviewed",
            "[]",
            SqlValue::Null,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        writer.execute(SqlStatement {
                sql: "INSERT INTO knowledge_atoms (id, namespace, slug, name, content, tags, finalized, status, created_at, updated_at, deleted_at) \
                      VALUES (?1, ?2, ?3, ?4, 'unrelated overview', ?5, 1, ?6, 0, 0, ?7)".into(),
                params: vec![SqlValue::Text(format!("93000000-0000-0000-0001-{index:012}")), SqlValue::Text(ns.into()), SqlValue::Text(slug.into()), SqlValue::Text(name.into()), SqlValue::Text(tags.into()), SqlValue::Text(status.into()), deleted],
                label: None,
            }).await.unwrap();
    }
    drop(writer);
    for (ns, query, kind, statuses, expected_state, expected_count) in [
        (
            "local",
            "AI",
            Some("atom"),
            vec![],
            LexicalCandidateState::Filtered,
            0,
        ),
        (
            "local",
            "AI",
            Some("atom"),
            vec!["draft".to_string()],
            LexicalCandidateState::ExactName,
            1,
        ),
        (
            "tenant-b",
            "AI",
            Some("atom"),
            vec![],
            LexicalCandidateState::ExactName,
            1,
        ),
        (
            "tenant-c",
            "AI",
            Some("atom"),
            vec![],
            LexicalCandidateState::NoMatch,
            0,
        ),
        (
            "local",
            "ML",
            Some("atom"),
            vec![],
            LexicalCandidateState::Filtered,
            0,
        ),
        (
            "local",
            "ML",
            Some("domain"),
            vec![],
            LexicalCandidateState::ExactName,
            1,
        ),
        (
            "local",
            "ZZ",
            Some("atom"),
            vec![],
            LexicalCandidateState::NoMatch,
            0,
        ),
        (
            "local",
            "UI",
            Some("atom"),
            vec![],
            LexicalCandidateState::NoMatch,
            0,
        ),
    ] {
        let outcome = fetch_fts_candidates(
            &runtime,
            ns,
            query,
            kind,
            &statuses,
            &["draft", "deprecated"],
            5,
        )
        .await
        .unwrap();
        assert!(outcome.timeout.is_none());
        assert_eq!(
            outcome.state, expected_state,
            "{ns}/{query}/{kind:?}/{statuses:?}"
        );
        assert_eq!(outcome.atoms.len(), expected_count);
        assert!(outcome.atoms.iter().all(|atom| atom.namespace == ns));
    }
}

#[test]
fn fts_candidate_expression_recalls_non_contiguous_terms() {
    assert_eq!(
        fts5_candidate_terms("alpha beta alpha and").join(" OR "),
        "\"alpha\" OR \"alphas\" OR \"beta\" OR \"betas\""
    );
    assert_eq!(
        fts5_candidate_terms("RAG").join(" OR "),
        "\"rag\" OR \"rags\""
    );
    assert_eq!(
        fts5_candidate_terms("the and").join(" OR "),
        "\"the and\"",
        "stop-only queries retain the exact-phrase fallback"
    );
}

#[test]
fn rrf_fusion_preserves_per_hit_score_sources_and_ann_fallback() {
    let mut hybrid = vec![make_hit("shared", Some("reviewed"), 0.8)];
    let ann = vec![make_ann_hit("shared", Some("reviewed"), 0.9)];
    fuse_ann_hits(&mut hybrid, &ann, 0.0);
    assert_eq!(hybrid.len(), 1);
    assert_eq!(
        hybrid[0].provenance.to_json()["sources"],
        json!(["lexical", "ann"])
    );
    assert_eq!(candidate_fallback(&hybrid), "none");

    let mut ann_only = Vec::new();
    fuse_ann_hits(
        &mut ann_only,
        &[make_ann_hit("semantic", Some("reviewed"), 0.9)],
        0.0,
    );
    assert_eq!(ann_only.len(), 1);
    assert_eq!(ann_only[0].provenance.to_json()["sources"], json!(["ann"]));
    assert_eq!(candidate_fallback(&ann_only), "ann");
    assert_eq!(
        ann_only[0].provenance.to_json(),
        json!({
            "sources": ["ann"],
            "embedding_rerank": false,
            "normalization": "s_over_s_plus_1",
            "calibrated": false,
        })
    );

    let lexical_only = vec![make_hit("lexical", Some("reviewed"), 0.7)];
    assert_eq!(
        lexical_only[0].provenance.to_json()["sources"],
        json!(["lexical"])
    );
    assert_eq!(candidate_fallback(&lexical_only), "none");
}

#[tokio::test]
async fn embedding_rerank_provenance_is_true_when_rerank_runs() {
    let (runtime, _, fail_query) = rt_with_role_aware_recording_embedder();
    fail_query.store(false, std::sync::atomic::Ordering::SeqCst);
    {
        let access = runtime.sql();
        let mut writer = access.writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO knowledge_atoms ( \
                              id, namespace, slug, name, content, tags, properties, finalized, \
                              status, source_uri, source_type, created_at, updated_at, deleted_at \
                          ) VALUES ( \
                              '94000000-0000-0000-0000-000000000001', 'local', \
                              'rerank-target', 'Rerank Target', \
                              'content that the lexical stage must match for the rerank pass', \
                              '[]', NULL, 1, 'reviewed', NULL, NULL, 1000, 1000, NULL \
                          )"
                .to_string(),
                params: Vec::new(),
                label: None,
            })
            .await
            .expect("seed rerank target atom");
    }

    let token = runtime.authorize(Namespace::local()).expect("local token");
    let ann = vamana::new_shared();
    let out = KnowledgeHandlers::search(
        &runtime,
        &token,
        json!({"query": "rerank target content", "rerank": true}),
        &ann,
    )
    .await
    .expect("rerank-enabled search must not error");

    assert_eq!(out["total"], 1);
    assert_eq!(
        out["results"][0]["score_provenance"]["embedding_rerank"], true,
        "a successful embedding rerank must record embedding_rerank: true; got {out:?}"
    );
    assert_eq!(out["rerank_provenance"]["candidates"], 1);
    assert_eq!(out["rerank_provenance"]["embedded_fallback"], 1);
}

#[tokio::test]
async fn over_limit_fallback_candidate_keeps_whole_search_reranked() {
    let (runtime, _, _) = rt_with_role_aware_recording_embedder();
    let repeated_text = "bounded fallback candidate ";
    let long_content =
        repeated_text.repeat(lattice_embed::MAX_TEXT_BYTES / repeated_text.len() + 2);
    assert!(
        atom_embed_text_fields("Bounded Fallback Long", &long_content, "[]").len()
            > lattice_embed::MAX_TEXT_BYTES
    );
    {
        let access = runtime.sql();
        let mut writer = access.writer().await.expect("writer");
        for (id, slug, name, content) in [
            (
                "94000000-0000-0000-0000-000000000011",
                "bounded-fallback-long",
                "Bounded Fallback Long",
                long_content.as_str(),
            ),
            (
                "94000000-0000-0000-0000-000000000012",
                "bounded-fallback-short",
                "Bounded Fallback Short",
                "bounded fallback candidate with short content",
            ),
        ] {
            writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO knowledge_atoms ( \
                                  id, namespace, slug, name, content, tags, properties, finalized, \
                                  status, source_uri, source_type, created_at, updated_at, deleted_at \
                              ) VALUES (?, 'local', ?, ?, ?, '[]', NULL, 1, 'reviewed', \
                                        NULL, NULL, 1000, 1000, NULL)"
                            .to_string(),
                        params: vec![
                            SqlValue::Text(id.into()),
                            SqlValue::Text(slug.into()),
                            SqlValue::Text(name.into()),
                            SqlValue::Text(content.into()),
                        ],
                        label: None,
                    })
                    .await
                    .expect("seed fallback candidate without a stored vector");
        }
    }

    let token = runtime.authorize(Namespace::local()).expect("local token");
    let ann = vamana::new_shared();
    let out = KnowledgeHandlers::search(
        &runtime,
        &token,
        json!({"query": "bounded fallback candidate", "rerank": true}),
        &ann,
    )
    .await
    .expect("bounded fallback must not disable search reranking");

    assert_eq!(out["total"], 2, "both lexical candidates must be returned");
    let results = out["results"].as_array().expect("search results");
    assert_eq!(results.len(), 2);
    for result in results {
        assert_eq!(
            result["score_provenance"]["embedding_rerank"], true,
            "the long fallback must not skip reranking for either hit: {out:?}"
        );
    }
    assert_eq!(out["rerank_provenance"]["candidates"], 2);
    assert_eq!(out["rerank_provenance"]["from_stored"], 0);
    assert_eq!(out["rerank_provenance"]["embedded_fallback"], 2);
}

#[tokio::test]
async fn unsupported_vector_store_reports_fallback_provenance() {
    let (runtime, _, _) = rt_with_role_aware_recording_embedder();
    let store = crate::knowledge::a5_rerank_tests::NoReadStore;
    let mut hits = vec![make_hit(
        "94000000-0000-0000-0000-000000000001",
        Some("reviewed"),
        1.0,
    )];
    let query_vector = vec![0.25; ROLE_RECORDING_DIM];
    let provenance = rerank_search_from_store(
        &runtime,
        Some(&store),
        "local",
        &query_vector,
        &mut hits,
        0.7,
    )
    .await
    .expect("unsupported store degrades to document embedding")
    .expect("rerank ran");
    assert_eq!(provenance["stored_vector_lookup"], "unsupported");
    assert_eq!(provenance["candidates"], 1);
    assert_eq!(provenance["from_stored"], 0);
    assert_eq!(provenance["embedded_fallback"], 1);
    assert!(hits[0].provenance.embedding_rerank);
}

#[tokio::test]
async fn canonical_domain_without_mirror_vector_uses_document_fallback() {
    let (runtime, _, _) = rt_with_role_aware_recording_embedder();
    let token = runtime.authorize(Namespace::local()).expect("token");
    let store = runtime
        .vectors_for_model(&token, runtime.default_embedder_name())
        .expect("model vector store");
    let mut hits = vec![ScoredHit {
        id: "94000000-0000-0000-0000-000000000002".into(),
        slug: "canonical-only-domain".into(),
        name: "Canonical Only Domain".into(),
        content: Some("canonical description".into()),
        tags: Some("[\"domain-tag\",\"type:domain\"]".into()),
        atom_embed_text: None,
        finalized: false,
        is_domain: true,
        status: Some("reviewed".into()),
        score: 1.0,
        provenance: ScoreProvenance::lexical(),
    }];
    let query_vector = vec![0.25; ROLE_RECORDING_DIM];
    let provenance = rerank_search_from_store(
        &runtime,
        Some(store.as_ref()),
        "local",
        &query_vector,
        &mut hits,
        0.7,
    )
    .await
    .expect("missing mirror vector uses document fallback")
    .expect("rerank ran");
    assert_eq!(provenance["stored_vector_lookup"], "supported");
    assert_eq!(provenance["candidates"], 1);
    assert_eq!(provenance["from_stored"], 0);
    assert_eq!(provenance["embedded_fallback"], 1);
    assert!(hits[0].provenance.embedding_rerank);
}

#[tokio::test]
async fn spent_read_deadline_skips_document_fallback_rerank() {
    let (runtime, _, _) = rt_with_role_aware_recording_embedder();
    let store = crate::knowledge::a5_rerank_tests::NoReadStore;
    let mut hits = vec![make_hit(
        "94000000-0000-0000-0000-000000000001",
        Some("reviewed"),
        1.0,
    )];
    let query_vector = vec![0.25; ROLE_RECORDING_DIM];
    let result = khive_storage::scope_request_read_deadline(
        std::time::Duration::ZERO,
        rerank_search_from_store(
            &runtime,
            Some(&store),
            "local",
            &query_vector,
            &mut hits,
            0.7,
        ),
    )
    .await
    .expect("spent deadline degrades rather than errors");
    assert!(result.is_none());
    assert_eq!(hits[0].score, 1.0);
    assert!(!hits[0].provenance.embedding_rerank);
}

#[test]
fn exact_name_provenance_survives_merge_without_hiding_timeouts() {
    use LexicalCandidateState::{ExactName, Matched, NoMatch, PartialTimeout, TimedOut};
    for (states, expected) in [
        (vec![NoMatch, ExactName], ExactName),
        (vec![ExactName, Matched], Matched),
        (vec![ExactName, TimedOut], PartialTimeout),
        (vec![ExactName, PartialTimeout], PartialTimeout),
    ] {
        assert_eq!(LexicalCandidateState::merge(&states), expected);
    }
}

#[tokio::test]
async fn missing_ann_hydration_is_dropped_and_reported() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut hits = vec![ScoredHit {
        id: "00000000-0000-0000-0000-000000001763".to_string(),
        slug: String::new(),
        name: String::new(),
        content: None,
        tags: None,
        atom_embed_text: None,
        finalized: false,
        is_domain: false,
        status: None,
        score: 0.8,
        provenance: ScoreProvenance::ann(),
    }];

    let failures = hydrate_empty_hits(&runtime, "local", &mut hits).await;
    assert_eq!(failures, 1);
    assert!(hits.is_empty(), "unhydrated shells must never be returned");

    let mut response = json!({"results": [], "total": 0});
    attach_hydration_degradation(&mut response, failures);
    assert_eq!(response["degraded"]["hydration_failures"], 1);
}

#[test]
fn zero_hydration_failures_do_not_change_the_response() {
    let mut response = json!({"results": [], "total": 0});
    attach_hydration_degradation(&mut response, 0);
    assert!(response.get("degraded").is_none());
}

#[test]
fn hydration_degradation_preserves_existing_diagnostics() {
    let mut response = json!({
        "results": [],
        "total": 0,
        "degraded": {
            "reason": "ann_unavailable",
            "cache_safe": false,
        }
    });
    attach_hydration_degradation(&mut response, 7);
    assert_eq!(response["degraded"]["reason"], "ann_unavailable");
    assert_eq!(response["degraded"]["cache_safe"], false);
    assert_eq!(response["degraded"]["hydration_failures"], 7);
}

#[tokio::test]
async fn hydration_chunks_candidate_sets_above_sqlite_bind_ceiling() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    {
        let access = runtime.sql();
        let mut writer = access.writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: "WITH RECURSIVE x(n) AS ( \
                              VALUES(0) UNION ALL SELECT n + 1 FROM x WHERE n < 20 \
                          ), y(n) AS ( \
                              VALUES(0) UNION ALL SELECT n + 1 FROM y WHERE n < 49 \
                          ) \
                          INSERT INTO knowledge_atoms ( \
                              id, namespace, slug, name, content, tags, properties, finalized, \
                              status, source_uri, source_type, created_at, updated_at, deleted_at \
                          ) \
                          SELECT \
                              printf('70000000-0000-0000-0000-%012d', x.n * 50 + y.n), \
                              'local', printf('hydrate-%04d', x.n * 50 + y.n), \
                              printf('Hydrate %04d', x.n * 50 + y.n), 'hydration content', \
                              '[]', NULL, 1, 'reviewed', NULL, NULL, \
                              x.n * 50 + y.n, x.n * 50 + y.n, NULL \
                          FROM x CROSS JOIN y WHERE x.n * 50 + y.n < 1005"
                    .to_string(),
                params: Vec::new(),
                label: None,
            })
            .await
            .expect("seed hydration rows");
    }

    let mut hits: Vec<ScoredHit> = (0..1005)
        .map(|i| ScoredHit {
            id: format!("70000000-0000-0000-0000-{i:012}"),
            slug: String::new(),
            name: String::new(),
            content: None,
            tags: None,
            atom_embed_text: None,
            finalized: false,
            is_domain: false,
            status: None,
            score: 1.0,
            provenance: ScoreProvenance::ann(),
        })
        .collect();

    let failures = hydrate_empty_hits(&runtime, "local", &mut hits).await;
    assert_eq!(failures, 0);
    assert_eq!(hits.len(), 1005);
    assert!(hits.iter().all(|hit| hit.slug.starts_with("hydrate-")));
}

/// Pins the production plan measured on the live store (179,809-row
/// `knowledge_atoms`, no `sqlite_stat1`): with 250 literal ids and no
/// `ANALYZE`, the planner must pick the primary-key auto-index, never a
/// namespace index. A scratch, statistics-free database reproduces the
/// same wrong-index choice the live store made, because the planner's
/// default no-statistics guess (an indexed equality is ~10 rows) is what
/// drove the original defect, not data volume.
#[tokio::test]
async fn hydrate_atoms_statement_plan_uses_primary_key_not_namespace_index() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let ids: Vec<String> = (0..250)
        .map(|i| format!("aaaaaaaa-0000-0000-0000-{i:012}"))
        .collect();

    let mut reader = runtime.sql().reader().await.expect("plan reader");
    let rows = reader
        .explain(hydrate_atoms_statement("local", &ids))
        .await
        .expect("explain atom hydration statement");
    let details: Vec<String> = rows
        .iter()
        .filter_map(|row| match row.get("detail") {
            Some(SqlValue::Text(detail)) => Some(detail.clone()),
            _ => None,
        })
        .collect();

    assert!(
        details
            .iter()
            .any(|detail| detail.contains("USING INDEX sqlite_autoindex_knowledge_atoms_1")),
        "atom hydration must seek the primary key: {details:?}"
    );
    assert!(
        !details
            .iter()
            .any(|detail| detail.contains("idx_knowledge_atoms_ns")),
        "atom hydration must not fall back to a namespace index: {details:?}"
    );
}

/// Domains twin of the atoms plan-pin above — same shape, same reason
/// (`knowledge_domains` also carries a namespace index that the
/// no-statistics planner would otherwise prefer).
#[tokio::test]
async fn hydrate_domains_statement_plan_uses_primary_key_not_namespace_index() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let ids: Vec<String> = (0..250)
        .map(|i| format!("bbbbbbbb-0000-0000-0000-{i:012}"))
        .collect();

    let mut reader = runtime.sql().reader().await.expect("plan reader");
    let rows = reader
        .explain(hydrate_domains_statement("local", &ids))
        .await
        .expect("explain domain hydration statement");
    let details: Vec<String> = rows
        .iter()
        .filter_map(|row| match row.get("detail") {
            Some(SqlValue::Text(detail)) => Some(detail.clone()),
            _ => None,
        })
        .collect();

    assert!(
        details
            .iter()
            .any(|detail| { detail.contains("USING INDEX sqlite_autoindex_knowledge_domains_1") }),
        "domain hydration must seek the primary key: {details:?}"
    );
    assert!(
        !details
            .iter()
            .any(|detail| detail.contains("idx_knowledge_domains_ns")),
        "domain hydration must not fall back to a namespace index: {details:?}"
    );
}

/// Issue #2396 fix 5, same plan-pin shape as the two tests above applied
/// to the lexical phase-B hydration statement: at `HYDRATION_ID_CHUNK`
/// (900) rowids, the no-statistics planner must seek the integer primary
/// key, never fall back to `idx_knowledge_atoms_ns`.
#[tokio::test]
async fn phase_b_hydration_statement_plan_uses_primary_key_not_namespace_index() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let rowids: Vec<i64> = (1..=HYDRATION_ID_CHUNK as i64).collect();

    let mut reader = runtime.sql().reader().await.expect("plan reader");
    let rows = reader
        .explain(phase_b_hydration_statement("local", &rowids, &[], &[], ""))
        .await
        .expect("explain phase-b hydration statement");
    let details: Vec<String> = rows
        .iter()
        .filter_map(|row| match row.get("detail") {
            Some(SqlValue::Text(detail)) => Some(detail.clone()),
            _ => None,
        })
        .collect();

    assert!(
        details
            .iter()
            .any(|detail| detail.contains("USING INTEGER PRIMARY KEY")),
        "phase-b hydration must seek the rowid primary key: {details:?}"
    );
    assert!(
        !details
            .iter()
            .any(|detail| detail.contains("idx_knowledge_atoms_ns")),
        "phase-b hydration must not fall back to a namespace index: {details:?}"
    );
}

/// Functional companion to the plan-pin tests above: the primary-key-first
/// rewrite must not weaken namespace scoping. Seed atoms in two
/// namespaces, hydrate ids drawn from both against a single namespace,
/// and confirm the other namespace's row never comes back.
#[tokio::test]
async fn hydrate_atoms_statement_still_scopes_by_namespace() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let access = runtime.sql();
    let mut writer = access.writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO knowledge_atoms ( \
                          id, namespace, slug, name, content, tags, properties, finalized, \
                          status, source_uri, source_type, created_at, updated_at, deleted_at \
                      ) VALUES \
                      ('90000000-0000-0000-0000-000000000001', 'local', 'local-one', \
                       'Local One', 'local content', '[]', NULL, 1, 'reviewed', NULL, NULL, \
                       1, 1, NULL), \
                      ('90000000-0000-0000-0000-000000000002', 'local', 'local-two', \
                       'Local Two', 'local content', '[]', NULL, 1, 'reviewed', NULL, NULL, \
                       2, 2, NULL), \
                      ('90000000-0000-0000-0000-000000000003', 'other', 'other-one', \
                       'Other One', 'other content', '[]', NULL, 1, 'reviewed', NULL, NULL, \
                       3, 3, NULL)"
                .to_string(),
            params: Vec::new(),
            label: None,
        })
        .await
        .expect("seed cross-namespace atoms");
    drop(writer);

    let mut hits: Vec<ScoredHit> = [
        "90000000-0000-0000-0000-000000000001",
        "90000000-0000-0000-0000-000000000002",
        "90000000-0000-0000-0000-000000000003",
    ]
    .iter()
    .map(|id| ScoredHit {
        id: id.to_string(),
        slug: String::new(),
        name: String::new(),
        content: None,
        tags: None,
        atom_embed_text: None,
        finalized: false,
        is_domain: false,
        status: None,
        score: 1.0,
        provenance: ScoreProvenance::ann(),
    })
    .collect();

    let failures = hydrate_empty_hits(&runtime, "local", &mut hits).await;
    assert_eq!(
        failures, 1,
        "the other-namespace row must be reported as an unhydrated candidate"
    );
    assert_eq!(hits.len(), 2, "only the local-namespace rows may hydrate");
    assert!(
        hits.iter()
            .all(|hit| hit.id != "90000000-0000-0000-0000-000000000003"),
        "the other-namespace row must never be returned: {:?}",
        hits.iter().map(|h| &h.id).collect::<Vec<_>>()
    );
    assert!(hits.iter().any(|hit| hit.slug == "local-one"));
    assert!(hits.iter().any(|hit| hit.slug == "local-two"));
}

// ── embed-intent regression ───────────────────────────────────────────────
// Guard that the shared ANN candidate runner and the compose KG-blend gate
// use the query-intent embedding call, not the generic
// `runtime.embed(...)`. Uses include_str! so the assertion runs on the
// actual source bytes, but splits the needle to avoid matching the
// needle itself in test source.
#[test]
fn knowledge_ann_query_paths_use_query_intent_embed() {
    let src = [
        include_str!("search.rs"),
        include_str!("search/ann_search.rs"),
        include_str!("search/compose_packing.rs"),
    ]
    .join("\n");
    // Build needle at runtime to avoid self-match in include_str.
    let generic_needle: String = [".embed(", "raw_query)"].concat();
    let generic_borrowed_needle: String = [".embed(", "&raw_query)"].concat();
    let generic_count = src
        .lines()
        // Skip lines that are part of this test body (contain "concat" or "needle").
        .filter(|l| !l.contains("concat") && !l.contains("needle"))
        .filter(|l| l.contains(&generic_needle) || l.contains(&generic_borrowed_needle))
        .count();
    assert_eq!(
        generic_count, 0,
        "ANN query paths must not call generic {generic_needle}; \
             found {generic_count} occurrence(s) — use embed_query instead"
    );

    // Positive check (#2307): the assertion above only proves the generic
    // path is *absent* — a mutation that replaced every production
    // `embed_query` call with a different method entirely (e.g.
    // `embed_document`) would still pass it, since that mutation never
    // introduces the generic-embed needle either. Count the query-intent
    // call sites directly so a silent removal (or mutation-away) of one
    // is caught: the shared ANN candidate runner and `compose`'s
    // KG-blend gate use raw_query, while search rerank can independently
    // attempt its query if the candidate runner did not.
    let query_intent_needle: String = [".embed_query(", "raw_query)"].concat();
    let query_intent_borrowed_needle: String = [".embed_query(", "&raw_query)"].concat();
    let query_intent_count = src
        .lines()
        .filter(|l| !l.contains("concat") && !l.contains("needle"))
        .filter(|l| l.contains(&query_intent_needle) || l.contains(&query_intent_borrowed_needle))
        .count();
    let rerank_query_needle: String = [".embed_query(", "query)"].concat();
    let rerank_query_count = src
        .lines()
        .filter(|l| !l.contains("concat") && !l.contains("needle"))
        .filter(|l| l.contains(&rerank_query_needle))
        .count();
    assert_eq!(
        query_intent_count, 2,
        "expected exactly 2 query-intent call sites \
             (shared ANN runner + compose KG-blend gate), found {query_intent_count}"
    );
    assert_eq!(
        rerank_query_count, 1,
        "search rerank must retain its query-intent fallback call site"
    );
}

/// #2232: once a rerank stage has successfully embedded the query, later
/// stages embed candidates only. The recording provider sees the literal
/// query exactly once across two independent candidate pools.
#[tokio::test]
async fn query_embedding_cache_reuses_query_vector_across_reranks() {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use khive_runtime::{AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig};
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

    const MODEL_KEY: &str = "all-minilm-l6-v2";
    const DIM: usize = 384;

    struct RecordingService {
        texts: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl EmbeddingService for RecordingService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.texts
                .lock()
                .expect("recording lock")
                .extend(texts.iter().cloned());
            Ok(texts.iter().map(|_| vec![0.5; DIM]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "query-reuse-recording-service"
        }
    }

    struct RecordingProvider {
        texts: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl EmbedderProvider for RecordingProvider {
        fn name(&self) -> &str {
            MODEL_KEY
        }

        fn dimensions(&self) -> usize {
            DIM
        }

        async fn build(&self) -> Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(RecordingService {
                texts: Arc::clone(&self.texts),
            }))
        }
    }

    let texts = Arc::new(Mutex::new(Vec::new()));
    let runtime = KhiveRuntime::new(RuntimeConfig {
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
        db_path: None,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        additional_embedding_models: Vec::new(),
        gate: Arc::new(AllowAllGate),
        packs: vec!["knowledge".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: Vec::new(),
        allowed_outbound_namespaces: Vec::new(),
        actor_id: None,
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("runtime");
    runtime.register_embedder(RecordingProvider {
        texts: Arc::clone(&texts),
    });

    let query = "one request-local query vector";
    let mut query_embedding = QueryEmbeddingCache::default();
    let first = vec![
        "first candidate".to_string(),
        "second candidate".to_string(),
    ];
    let second = vec!["third candidate".to_string()];
    assert_eq!(
        embed_cosine_scores(&runtime, query, &mut query_embedding, &first)
            .await
            .expect("first rerank")
            .expect("first scores")
            .len(),
        2
    );
    assert!(
        query_embedding.any().is_some(),
        "first rerank must fill cache"
    );
    assert!(
        query_embedding.role_specific.is_not_attempted(),
        "embed_cosine_scores must cache the batch vector as generic, \
             never role_specific — it came from embed_batch, not embed_query"
    );
    assert_eq!(
        embed_cosine_scores(&runtime, query, &mut query_embedding, &second)
            .await
            .expect("second rerank")
            .expect("second scores")
            .len(),
        1
    );

    let recorded = texts.lock().expect("recording lock");
    assert_eq!(
        recorded
            .iter()
            .filter(|text| text.as_str() == query)
            .count(),
        1,
        "the shared query must be embedded exactly once: {recorded:?}"
    );
    assert_eq!(recorded.len(), 4, "one query plus three candidates");
}

// ── #2307: failed query embeddings must not be retried within a request ──
//
// Extends the #2232 `RecordingService` pattern above with a separate,
// overridable `embed_query` (distinct from the generic `embed`) so these
// tests can fail the query embedding on demand and prove which method a
// given text actually reached.

const ROLE_RECORDING_MODEL_KEY: &str = "all-minilm-l6-v2";
const ROLE_RECORDING_DIM: usize = 384;
const ROLE_RECORDING_QUERY: &str = "graph traversal caching strategies distributed \
         knowledge retrieval systems degraded embedding providers";
// Atom content must clear the 20-word minimum; repeats the query terms
// (for lexical relevance) plus filler.
const ROLE_RECORDING_ATOM_CONTENT: &str = "graph traversal caching strategies distributed \
         knowledge retrieval systems degraded embedding providers require resilient fallback \
         behavior across production knowledge retrieval pipelines and search infrastructure";

#[derive(Debug, Default)]
struct RoleAwareRecordingCalls {
    query: Vec<String>,
    generic: Vec<String>,
    query_gate: Option<std::sync::Arc<tokio::sync::Semaphore>>,
}

struct RoleAwareRecordingService {
    calls: std::sync::Arc<std::sync::Mutex<RoleAwareRecordingCalls>>,
    fail_query: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for RoleAwareRecordingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        self.calls
            .lock()
            .expect("recording lock")
            .generic
            .extend(texts.iter().cloned());
        Ok(texts
            .iter()
            .map(|_| vec![0.5; ROLE_RECORDING_DIM])
            .collect())
    }

    async fn embed_query(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        let gate = {
            let mut calls = self.calls.lock().expect("recording lock");
            calls.query.extend(texts.iter().cloned());
            calls.query_gate.clone()
        };
        if let Some(gate) = gate {
            gate.acquire()
                .await
                .expect("query gate remains open")
                .forget();
        }
        if self.fail_query.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(lattice_embed::EmbedError::InferenceFailed(
                "forced query-embedding failure".into(),
            ));
        }
        Ok(texts
            .iter()
            .map(|_| vec![0.25; ROLE_RECORDING_DIM])
            .collect())
    }

    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "role-aware-recording-service"
    }
}

struct RoleAwareRecordingProvider {
    calls: std::sync::Arc<std::sync::Mutex<RoleAwareRecordingCalls>>,
    fail_query: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for RoleAwareRecordingProvider {
    fn name(&self) -> &str {
        ROLE_RECORDING_MODEL_KEY
    }

    fn dimensions(&self) -> usize {
        ROLE_RECORDING_DIM
    }

    async fn build(
        &self,
    ) -> Result<std::sync::Arc<dyn lattice_embed::EmbeddingService>, RuntimeError> {
        Ok(std::sync::Arc::new(RoleAwareRecordingService {
            calls: std::sync::Arc::clone(&self.calls),
            fail_query: std::sync::Arc::clone(&self.fail_query),
        }))
    }
}

fn rt_with_role_aware_recording_embedder() -> (
    KhiveRuntime,
    std::sync::Arc<std::sync::Mutex<RoleAwareRecordingCalls>>,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(RoleAwareRecordingCalls::default()));
    let fail_query = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let runtime = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
        db_path: None,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: Some(lattice_embed::EmbeddingModel::AllMiniLmL6V2),
        additional_embedding_models: Vec::new(),
        gate: std::sync::Arc::new(khive_runtime::AllowAllGate),
        packs: vec!["kg".to_string(), "knowledge".to_string()],
        backend_id: khive_runtime::BackendId::main(),
        brain_profile: None,
        visible_namespaces: Vec::new(),
        allowed_outbound_namespaces: Vec::new(),
        actor_id: None,
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    runtime.register_embedder(RoleAwareRecordingProvider {
        calls: std::sync::Arc::clone(&calls),
        fail_query: std::sync::Arc::clone(&fail_query),
    });
    (runtime, calls, fail_query)
}

#[tokio::test]
async fn search_and_suggest_start_lexical_reads_while_query_embedding_waits() {
    for suggest in [false, true] {
        let (runtime, calls, _) = rt_with_role_aware_recording_embedder();
        let access = runtime.sql();
        let mut writer = access.writer().await.expect("writer");
        writer
                .execute(SqlStatement {
                    sql: "INSERT INTO knowledge_atoms \
                          (id, namespace, slug, name, content, tags, finalized, status, created_at, updated_at) \
                          VALUES ('94000000-0000-0000-0000-000000000238', 'local', \
                                  'candidate-overlap', 'Candidate Overlap', \
                                  'graph traversal caching strategies distributed knowledge retrieval', \
                                  '[]', 1, 'reviewed', 0, 0)"
                        .into(),
                    params: Vec::new(),
                    label: None,
                })
                .await
                .expect("seed lexical hit");
        drop(writer);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        calls.lock().expect("recording lock").query_gate = Some(Arc::clone(&gate));
        let token = runtime.authorize(Namespace::local()).expect("token");
        let ann = vamana::new_shared_for_role(false);
        let probes = Arc::new(Mutex::new(Vec::new()));
        let query = "graph traversal caching strategies distributed knowledge retrieval";
        let future = TERM_PROBES.scope(probes.clone(), async {
            if suggest {
                KnowledgeHandlers::suggest(&runtime, &token, json!({"query": query}), &ann).await
            } else {
                KnowledgeHandlers::search(
                    &runtime,
                    &token,
                    json!({"query": query, "rerank": false}),
                    &ann,
                )
                .await
            }
        });
        tokio::pin!(future);
        let overlapped = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    let embedding_started = !calls.lock().expect("recording lock").query.is_empty();
                    let lexical_started = !probes.lock().expect("term probes").is_empty();
                    if embedding_started && lexical_started {
                        break;
                    }
                    tokio::select! {
                        result = &mut future => panic!("candidate stages finished before gate release: {result:?}"),
                        () = tokio::time::sleep(std::time::Duration::from_millis(1)) => {}
                    }
                }
            })
            .await;
        gate.add_permits(1);
        assert!(
            overlapped.is_ok(),
            "lexical read waited for embedding: suggest={suggest}"
        );
        let response = future
            .await
            .expect("candidate stages complete after gate release");
        if !suggest {
            assert_eq!(response["results"][0]["slug"], "candidate-overlap");
            assert_eq!(response["candidate_provenance"]["lexical"], "matched");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn failed_embedding_and_timed_out_lexical_stage_return_degraded_search() {
    let (runtime, _, fail_query) = rt_with_role_aware_recording_embedder();
    fail_query.store(true, std::sync::atomic::Ordering::SeqCst);
    let token = runtime.authorize(Namespace::local()).expect("token");
    let ann = vamana::new_shared_for_role(false);
    let response = crate::knowledge::lexical_timeout::tests::with_timeout(
        vec![LexicalPhase::ReaderOpen],
        std::time::Duration::from_millis(1),
        KnowledgeHandlers::search(
            &runtime,
            &token,
            json!({"query": "graph traversal caching strategies", "rerank": false}),
            &ann,
        ),
    )
    .await
    .expect("both recoverable candidate failures remain a response");
    assert_eq!(response["total"], 0);
    assert_eq!(response["candidate_provenance"]["lexical"], "timed_out");
    assert_eq!(response["degraded"]["lexical_timeout"], true);
}

#[tokio::test(start_paused = true)]
async fn completed_ann_candidate_survives_lexical_stage_timeout() {
    let (runtime, _, _) = rt_with_role_aware_recording_embedder();
    let runtime = runtime.with_ann_fresh_tail_enabled(false);
    let atom_id = Uuid::from_u128(0x94000000000000000000000000000239);
    let access = runtime.sql();
    let mut writer = access.writer().await.expect("writer");
    writer
            .execute(SqlStatement {
                sql: "INSERT INTO knowledge_atoms \
                      (id, namespace, slug, name, content, tags, finalized, status, created_at, updated_at) \
                      VALUES (?1, 'local', 'ann-timeout-survivor', 'ANN Timeout Survivor', \
                              'dense candidate outside this lexical query', '[]', 1, 'reviewed', 0, 0)"
                    .into(),
                params: vec![SqlValue::Text(atom_id.to_string())],
                label: None,
            })
            .await
            .expect("seed ANN hydration row");
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO ann_consumer_watermark \
                      (consumer, namespace, embedding_model, watermark) \
                      VALUES ('knowledge:knowledge.atom', 'local', ?1, 0)"
                .into(),
            params: vec![SqlValue::Text(ROLE_RECORDING_MODEL_KEY.into())],
            label: None,
        })
        .await
        .expect("register loaded ANN consumer");
    drop(writer);

    let ann = vamana::new_shared_for_role(false);
    let key = vamana::AnnKey::new("local", ROLE_RECORDING_MODEL_KEY);
    let bridge = vamana::AnnBridge::build(
        vec![0.25; ROLE_RECORDING_DIM],
        ROLE_RECORDING_DIM,
        vec![atom_id],
    )
    .expect("build loaded bridge");
    vamana::install_if_fresher(&ann, &key, bridge).await;
    let token = runtime.authorize(Namespace::local()).expect("token");
    let response = crate::knowledge::lexical_timeout::tests::with_timeout(
        vec![LexicalPhase::ReaderOpen],
        std::time::Duration::from_millis(1),
        KnowledgeHandlers::search(
            &runtime,
            &token,
            json!({"query": "graph traversal caching strategies", "rerank": false}),
            &ann,
        ),
    )
    .await
    .expect("lexical timeout retains completed ANN result");
    assert_eq!(response["total"], 1);
    assert_eq!(response["results"][0]["slug"], "ann-timeout-survivor");
    assert_eq!(
        response["results"][0]["score_provenance"]["sources"],
        json!(["ann"])
    );
    assert_eq!(response["candidate_provenance"]["fallback"], "ann");
    assert_eq!(response["degraded"]["lexical_timeout"], true);
}

fn build_role_recording_registry(rt: &KhiveRuntime) -> khive_runtime::VerbRegistry {
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(crate::KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    registry
}

/// Seeds one atom as a domain member so `compose`'s auto flow reaches the
/// Rerank phase; auto-compose skips domains with no live members before
/// the KG-blend gate these tests exercise.
async fn seed_role_recording_corpus(registry: &khive_runtime::VerbRegistry) {
    registry
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{
                    "slug": "role-recording-atom",
                    "name": "Role Recording Atom",
                    "finalized": true,
                    "content": ROLE_RECORDING_ATOM_CONTENT
                }]
            }),
        )
        .await
        .expect("upsert atom");
    registry
        .dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [{
                    "slug": "role-recording-domain",
                    "name": "Role Recording Domain",
                    "description": ROLE_RECORDING_ATOM_CONTENT,
                    "members": ["role-recording-atom"]
                }]
            }),
        )
        .await
        .expect("upsert domain");
    registry
        .dispatch("knowledge.index", json!({ "rebuild_ann": false }))
        .await
        .expect("index");
}

/// (a) A failed `embed_query` inside `suggest` must not be retried by
/// `compose`'s KG-blend gate immediately afterward in the same
/// auto-compose request. Before the fix, `compose` saw
/// `role_specific: None` — indistinguishable from "never tried" — and
/// spent a second failing provider call; the query text was attempted
/// twice. Red before the fix: 2 attempts.
#[tokio::test]
async fn compose_auto_does_not_retry_role_specific_embed_after_suggest_failure() {
    let (rt, calls, fail_query) = rt_with_role_aware_recording_embedder();
    let registry = build_role_recording_registry(&rt);
    seed_role_recording_corpus(&registry).await;
    fail_query.store(true, std::sync::atomic::Ordering::SeqCst);

    let ann = vamana::new_shared();
    let token = rt.authorize(Namespace::local()).expect("authorize");
    let result = KnowledgeHandlers::compose(
        &rt,
        &token,
        json!({ "query": ROLE_RECORDING_QUERY }),
        &ann,
        HashMap::new(),
    )
    .await
    .expect("compose must not Err when the query embedding is degraded");

    let attempts = calls
        .lock()
        .expect("recording lock")
        .query
        .iter()
        .filter(|text| text.as_str() == ROLE_RECORDING_QUERY)
        .count();
    assert_eq!(
        attempts, 1,
        "a failed embed_query must not be retried later in the same request; result: {result}"
    );
    assert_eq!(
        result["data"]["count"].as_u64(),
        Some(1),
        "the atom must still reach the briefing through the degraded blend path; \
             result: {result}"
    );
}

/// (b) Control: without a prior failure, `suggest`'s successful
/// role-specific embed is the *only* attempt across the whole
/// auto-compose request — `compose`'s KG-blend gate reuses it rather than
/// embedding again. The narrower claim (a rerank stage specifically
/// reuses a cached vector across candidate batches) is covered by
/// `query_embedding_cache_reuses_query_vector_across_reranks` (#2232)
/// above; this test covers the handler-level chain instead.
#[tokio::test]
async fn compose_auto_reuses_successful_suggest_embed_without_retry() {
    let (rt, calls, _fail_query) = rt_with_role_aware_recording_embedder();
    let registry = build_role_recording_registry(&rt);
    seed_role_recording_corpus(&registry).await;

    let ann = vamana::new_shared();
    let token = rt.authorize(Namespace::local()).expect("authorize");
    let result = KnowledgeHandlers::compose(
        &rt,
        &token,
        json!({ "query": ROLE_RECORDING_QUERY }),
        &ann,
        HashMap::new(),
    )
    .await
    .expect("compose must not Err");

    let attempts = calls
        .lock()
        .expect("recording lock")
        .query
        .iter()
        .filter(|text| text.as_str() == ROLE_RECORDING_QUERY)
        .count();
    assert_eq!(
        attempts, 1,
        "a successful embed_query must be reused, not repeated; result: {result}"
    );
    assert_eq!(
        result["data"]["count"].as_u64(),
        Some(1),
        "result: {result}"
    );
}

/// (c) Control: a `compose` call that never goes through `suggest` (explicit
/// `domain_ids`, so auto-mode never runs) still embeds the query exactly
/// once for its own KG-blend gate — `NotAttempted` always authorizes the
/// one attempt a stage that never tried is entitled to.
#[tokio::test]
async fn compose_direct_call_without_suggest_still_embeds_query_once() {
    let (rt, calls, _fail_query) = rt_with_role_aware_recording_embedder();
    let registry = build_role_recording_registry(&rt);
    seed_role_recording_corpus(&registry).await;

    let ann = vamana::new_shared();
    let token = rt.authorize(Namespace::local()).expect("authorize");
    let result = KnowledgeHandlers::compose(
        &rt,
        &token,
        json!({
            "query": ROLE_RECORDING_QUERY,
            "domain_ids": ["role-recording-domain"],
        }),
        &ann,
        HashMap::new(),
    )
    .await
    .expect("compose must not Err");

    let attempts = calls
        .lock()
        .expect("recording lock")
        .query
        .iter()
        .filter(|text| text.as_str() == ROLE_RECORDING_QUERY)
        .count();
    assert_eq!(
        attempts, 1,
        "a stage that never tried must still embed once; result: {result}"
    );
    assert_eq!(
        result["data"]["count"].as_u64(),
        Some(1),
        "result: {result}"
    );
}

/// (d) #2307 item 2: `search`, `suggest`, and `compose` must each dispatch
/// the query text through `EmbeddingService::embed_query` specifically —
/// never through the generic `embed`. The source-scan guard below this
/// test only ever checked the generic path's *absence*; a mutation
/// swapping every production `embed_query` call for `embed_document`
/// still passed it (documented in the crate's fix report for #2307).
/// This behavioral check closes that gap by observing which method the
/// query text actually reaches at runtime.
#[tokio::test]
async fn search_suggest_compose_dispatch_query_through_embed_query_not_generic() {
    for verb in ["search", "suggest", "compose"] {
        let (rt, calls, _fail_query) = rt_with_role_aware_recording_embedder();
        let registry = build_role_recording_registry(&rt);
        seed_role_recording_corpus(&registry).await;
        let ann = vamana::new_shared();
        let token = rt.authorize(Namespace::local()).expect("authorize");

        match verb {
            "search" => {
                KnowledgeHandlers::search(
                    &rt,
                    &token,
                    json!({ "query": ROLE_RECORDING_QUERY }),
                    &ann,
                )
                .await
                .expect("search must not Err");
            }
            "suggest" => {
                KnowledgeHandlers::suggest(
                    &rt,
                    &token,
                    json!({ "query": ROLE_RECORDING_QUERY }),
                    &ann,
                )
                .await
                .expect("suggest must not Err");
            }
            "compose" => {
                KnowledgeHandlers::compose(
                    &rt,
                    &token,
                    json!({ "query": ROLE_RECORDING_QUERY }),
                    &ann,
                    HashMap::new(),
                )
                .await
                .expect("compose must not Err");
            }
            _ => unreachable!(),
        }

        let recorded = calls.lock().expect("recording lock");
        assert!(
            recorded
                .query
                .iter()
                .any(|text| text == ROLE_RECORDING_QUERY),
            "{verb}: query text must reach embed_query at least once; recorded={recorded:?}"
        );
        assert!(
            !recorded
                .generic
                .iter()
                .any(|text| text == ROLE_RECORDING_QUERY),
            "{verb}: query text must never reach the generic embed path; recorded={recorded:?}"
        );
    }
}

// ── filter_by_excluded_statuses ───────────────────────────────────────────

fn make_hit(id: &str, status: Option<&str>, score: f32) -> ScoredHit {
    ScoredHit {
        id: id.to_string(),
        slug: id.to_string(),
        name: id.to_string(),
        content: None,
        tags: None,
        atom_embed_text: None,
        finalized: false,
        is_domain: false,
        status: status.map(str::to_string),
        score,
        provenance: ScoreProvenance::lexical(),
    }
}

fn make_ann_hit(id: &str, status: Option<&str>, score: f32) -> ScoredHit {
    let mut hit = make_hit(id, status, score);
    hit.provenance = ScoreProvenance::ann();
    hit
}

#[test]
fn rrf_overlap_retains_both_candidate_sources() {
    let mut hits = vec![
        make_hit("shared", Some("reviewed"), 8.0),
        make_hit("lexical-only", Some("reviewed"), 4.0),
    ];
    let ann = vec![
        make_ann_hit("shared", Some("reviewed"), 0.9),
        make_ann_hit("ann-only", Some("reviewed"), 0.8),
    ];
    fuse_ann_hits(&mut hits, &ann, 0.0);

    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].id, "shared");
    assert_eq!(hits[0].score, 1.0);
    assert_eq!(
        hits[0].provenance.to_json(),
        json!({
            "sources": ["lexical", "ann"],
            "embedding_rerank": false,
            "normalization": "s_over_s_plus_1",
            "calibrated": false,
        })
    );
    let lexical = hits.iter().find(|hit| hit.id == "lexical-only").unwrap();
    let ann = hits.iter().find(|hit| hit.id == "ann-only").unwrap();
    assert_eq!(lexical.provenance, ScoreProvenance::lexical());
    assert_eq!(ann.provenance, ScoreProvenance::ann());
    assert_eq!(lexical.score, ann.score);
}

#[test]
fn decomposed_overlap_merges_provenance_without_counting_duplicates_twice() {
    let full = vec![
        make_hit("shared", Some("reviewed"), 8.0),
        make_hit("full-only", Some("reviewed"), 6.0),
    ];
    let subqueries = [
        vec![
            make_ann_hit("shared", Some("reviewed"), 100.0),
            make_hit("sub-shared", Some("reviewed"), 10.0),
            make_ann_hit("sub-shared", Some("reviewed"), 50.0),
        ],
        vec![
            make_ann_hit("shared", Some("reviewed"), 100.0),
            make_ann_hit("sub-shared", Some("reviewed"), 50.0),
            make_ann_hit("ann-only", Some("reviewed"), 2.0),
        ],
    ];
    let hits = merge_decomposed_hits(full, subqueries, 0.5, 10);

    let ids: Vec<_> = hits.iter().map(|hit| hit.id.as_str()).collect();
    assert_eq!(ids, ["shared", "full-only", "sub-shared", "ann-only"]);
    assert_eq!(hits[0].score, 12.0);
    assert_eq!(hits[1].score, 6.0);
    assert_eq!(hits[2].score, 4.5);
    assert!((hits[3].score - 0.6).abs() < 1e-6);
    for hit in [&hits[0], &hits[2]] {
        assert!(hit.provenance.lexical && hit.provenance.ann);
        assert!(!hit.provenance.embedding_rerank);
    }
    assert_eq!(hits[1].provenance, ScoreProvenance::lexical());
    assert_eq!(hits[3].provenance, ScoreProvenance::ann());
}

#[test]
fn candidate_fallback_requires_ann_without_any_lexical_source() {
    let lexical = make_hit("lexical", None, 1.0);
    let ann = make_ann_hit("ann", None, 1.0);
    let mut overlap = lexical.clone();
    overlap.provenance.merge_sources(ann.provenance);
    assert_eq!(candidate_fallback(&[]), "none");
    assert_eq!(candidate_fallback(std::slice::from_ref(&ann)), "ann");
    assert_eq!(candidate_fallback(std::slice::from_ref(&lexical)), "none");
    assert_eq!(candidate_fallback(&[lexical, ann]), "none");
    assert_eq!(candidate_fallback(&[overlap]), "none");
}

#[tokio::test]
async fn skipped_embedding_rerank_preserves_scores_and_provenance() {
    let runtime = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: None,
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime without embeddings");
    assert!(runtime.config().db_path.is_none());
    let mut hits = vec![
        make_hit("lexical", None, 3.0),
        make_ann_hit("ann", None, 0.8),
    ];
    let applied = rerank_with_embeddings(
        &runtime,
        "provenance query",
        &mut QueryEmbeddingCache::default(),
        &mut hits,
        0.7,
    )
    .await
    .expect("optional rerank");
    assert!(!applied);
    assert_eq!(hits[0].score, 3.0);
    assert_eq!(hits[1].score, 0.8);
    assert_eq!(hits[0].provenance, ScoreProvenance::lexical());
    assert_eq!(hits[1].provenance, ScoreProvenance::ann());
}

#[tokio::test]
async fn search_reports_lexical_and_ann_sources_and_successful_embedding_rerank() {
    let (runtime, calls, fail_query) = rt_with_role_aware_recording_embedder();
    let registry = build_role_recording_registry(&runtime);
    seed_role_recording_corpus(&registry).await;
    fail_query.store(false, std::sync::atomic::Ordering::SeqCst);
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let ann = vamana::new_shared();

    for rerank in [false, true] {
        calls.lock().expect("recording lock").generic.clear();
        let response = KnowledgeHandlers::search(
            &runtime,
            &token,
            json!({"query": ROLE_RECORDING_QUERY, "kind": "atom", "rerank": rerank}),
            &ann,
        )
        .await
        .expect("search");
        assert_eq!(response["total"], 1, "{response}");
        assert_eq!(response["results"][0]["slug"], "role-recording-atom");
        assert_eq!(
            response["results"][0]["score_provenance"],
            json!({
                "sources": ["lexical", "ann"],
                "embedding_rerank": rerank,
                "normalization": "s_over_s_plus_1",
                "calibrated": false,
            })
        );
        let recorded = calls.lock().expect("recording lock");
        if rerank {
            assert_eq!(
                response["rerank_provenance"]["stored_vector_lookup"],
                "supported"
            );
            assert_eq!(response["rerank_provenance"]["from_stored"], 1);
            assert_eq!(response["rerank_provenance"]["embedded_fallback"], 0);
            assert!(recorded.generic.is_empty());
        } else {
            assert!(response.get("rerank_provenance").is_none());
            assert!(recorded.generic.is_empty());
        }
    }
}

#[test]
fn explicit_status_is_an_exact_allowlist_for_hydrated_hits() {
    let mut hits = vec![
        make_hit("reviewed", Some("reviewed"), 0.9),
        make_hit("draft", Some("draft"), 0.8),
        make_hit("deprecated", Some("deprecated"), 0.7),
        make_hit("missing-status", None, 0.6),
    ];
    filter_hits_by_status(&mut hits, &["draft".to_string()], &[]);
    let ids: Vec<&str> = hits.iter().map(|hit| hit.id.as_str()).collect();
    assert_eq!(ids, ["draft"]);
}

#[test]
fn deprecated_multiplier_gate_uses_resolved_status_policy() {
    assert!(!deprecated_allowed_by_status_policy(
        &[],
        &["draft", "deprecated"]
    ));
    assert!(!deprecated_allowed_by_status_policy(&[], &["deprecated"]));
    assert!(deprecated_allowed_by_status_policy(&[], &["reviewed"]));
    assert!(!deprecated_allowed_by_status_policy(
        &["reviewed".to_string()],
        &[]
    ));
    assert!(deprecated_allowed_by_status_policy(
        &["deprecated".to_string()],
        &[]
    ));
}

#[test]
fn filter_excluded_statuses_removes_draft_hits() {
    let mut hits = vec![
        make_hit("reviewed-1", Some("reviewed"), 0.8),
        make_hit("draft-1", Some("draft"), 0.7),
        make_hit("reviewed-2", Some("reviewed"), 0.6),
        make_hit("draft-2", Some("draft"), 0.5),
    ];
    filter_by_excluded_statuses(&mut hits, &["draft", "deprecated"]);
    let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(
        ids,
        ["reviewed-1", "reviewed-2"],
        "draft hits must be removed"
    );
}

#[test]
fn filter_excluded_statuses_removes_deprecated_hits() {
    let mut hits = vec![
        make_hit("reviewed-1", Some("reviewed"), 0.9),
        make_hit("deprecated-1", Some("deprecated"), 0.8),
    ];
    filter_by_excluded_statuses(&mut hits, &["draft", "deprecated"]);
    let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(ids, ["reviewed-1"]);
}

#[test]
fn filter_excluded_statuses_empty_list_is_noop() {
    let mut hits = vec![
        make_hit("draft-1", Some("draft"), 0.9),
        make_hit("reviewed-1", Some("reviewed"), 0.8),
    ];
    filter_by_excluded_statuses(&mut hits, &[]);
    assert_eq!(hits.len(), 2, "empty exclude list must be a no-op");
}

#[test]
fn filter_excluded_statuses_null_status_treated_as_not_excluded() {
    // Hits with no status (ANN-sourced before hydration completes) must not
    // be removed by the status exclusion — they are not drafts or deprecated.
    let mut hits = vec![
        make_hit("no-status", None, 0.9),
        make_hit("draft-1", Some("draft"), 0.7),
    ];
    filter_by_excluded_statuses(&mut hits, &["draft", "deprecated"]);
    let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(ids, ["no-status"], "null-status hit must survive exclusion");
}

#[test]
fn normalize_rrf_score_is_bounded_and_monotonic() {
    let k = RRF_K;
    let max_single = 1.0f32 / (k as f32 + 1.0);
    let scores_single = [
        max_single * 0.25,
        max_single * 0.5,
        max_single,
        max_single * 1.5,
    ];
    let normed_single: Vec<f32> = scores_single
        .iter()
        .map(|&r| normalize_rrf_score(r, 1, k))
        .collect();
    for &s in &normed_single {
        assert!((0.0..=1.0).contains(&s), "score out of range: {s}");
    }
    assert!(normed_single[0] < normed_single[1]);
    assert!(normed_single[1] < normed_single[2]);
    assert_eq!(normed_single[3], 1.0);

    let max_two = 2.0f32 / (k as f32 + 1.0);
    let scores_two = [max_two * 0.25, max_two * 0.75, max_two, max_two * 2.0];
    let normed_two: Vec<f32> = scores_two
        .iter()
        .map(|&r| normalize_rrf_score(r, 2, k))
        .collect();
    for &s in &normed_two {
        assert!((0.0..=1.0).contains(&s), "score out of range: {s}");
    }
    assert!(normed_two[0] < normed_two[1]);
    assert!(normed_two[1] < normed_two[2]);
    assert_eq!(normed_two[3], 1.0);

    let raw = [0.001f32, 0.005, 0.010, 0.015];
    let normed: Vec<f32> = raw.iter().map(|&r| normalize_rrf_score(r, 1, k)).collect();
    let raw_order: Vec<usize> = {
        let mut idx: Vec<usize> = (0..raw.len()).collect();
        idx.sort_by(|&a, &b| raw[b].partial_cmp(&raw[a]).unwrap());
        idx
    };
    let norm_order: Vec<usize> = {
        let mut idx: Vec<usize> = (0..normed.len()).collect();
        idx.sort_by(|&a, &b| normed[b].partial_cmp(&normed[a]).unwrap());
        idx
    };
    assert_eq!(
        raw_order, norm_order,
        "normalization must not invert ranking"
    );
}

#[test]
fn normalize_rrf_score_zero_source_count_returns_zero() {
    assert_eq!(normalize_rrf_score(0.5, 0, RRF_K), 0.0);
}

/// Fusion-admitted hit whose score the status multiplier squashes below
/// `min_score` must not survive the late floor. Reproduction arithmetic:
/// a single-source RRF top hit normalizes to 1.0, then `s/(s+1)` with
/// multiplier 1.0 squashes it to 0.5 — below a 0.7 floor.
#[test]
fn min_score_floor_drops_hit_squashed_below_threshold_by_status_multiplier() {
    let mut hits = vec![make_hit("atom-1", Some("reviewed"), 0.0)];
    fuse_ann_hits(&mut hits, &[], 0.7);
    assert_eq!(hits.len(), 1, "fusion stage must admit the 1.0 RRF hit");
    assert_eq!(hits[0].score, 1.0);

    apply_status_multipliers(&mut hits, false);
    assert!((hits[0].score - 0.5).abs() < 1e-6, "1.0 squashes to 0.5");

    enforce_min_score_floor(&mut hits, 0.7);
    assert!(
        hits.is_empty(),
        "0.5 post-multiplier score must not clear a 0.7 floor"
    );
}

#[test]
fn min_score_floor_keeps_hit_at_or_above_threshold_after_multiplier() {
    let mut hits = vec![make_hit("atom-1", Some("reviewed"), 0.0)];
    fuse_ann_hits(&mut hits, &[], 0.4);
    assert_eq!(hits.len(), 1, "fusion stage must admit the 1.0 RRF hit");

    apply_status_multipliers(&mut hits, false);
    let squashed = hits[0].score;

    enforce_min_score_floor(&mut hits, 0.4);
    assert_eq!(
        hits.len(),
        1,
        "0.5 post-multiplier score clears a 0.4 floor"
    );
    assert_eq!(hits[0].id, "atom-1");
    assert!(hits[0].score >= 0.4);
    assert_eq!(hits[0].score, squashed, "floor must not rewrite scores");
}

/// `min_score = 0.0` (the absent default) must return the identical set —
/// the floor cannot alter the no-threshold path.
#[test]
fn min_score_floor_zero_is_noop_on_multiplier_survivors() {
    let build = || {
        let mut hits = vec![
            make_hit("atom-1", Some("reviewed"), 0.9),
            make_hit("atom-2", Some("draft"), 0.6),
            make_hit("atom-3", Some("deprecated"), 0.8),
        ];
        apply_status_multipliers(&mut hits, false);
        hits
    };

    let before = build();
    let mut after = build();
    enforce_min_score_floor(&mut after, 0.0);

    let ids_before: Vec<&str> = before.iter().map(|h| h.id.as_str()).collect();
    let ids_after: Vec<&str> = after.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(ids_after, ids_before);
    for (a, b) in after.iter().zip(before.iter()) {
        assert_eq!(a.score, b.score);
    }
}

/// Body-line metadata is best-effort: a read-deadline timeout during the
/// aggregate lookup degrades to `Ok(None)` (rendered as `body_lines:
/// null` plus a degradation flag by the handler) instead of failing an
/// already-ranked search with an `Internal` error.
#[tokio::test]
async fn body_line_counts_degrade_to_none_under_expired_read_deadline() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let atom_ids = vec!["10000000-0000-0000-0000-000000000001".to_owned()];

    let degraded = khive_storage::scope_request_read_deadline(
        std::time::Duration::ZERO,
        load_atom_body_line_counts(&runtime, "local", &atom_ids),
    )
    .await;
    assert!(
        matches!(degraded, Ok(None)),
        "an expired read deadline must degrade body-line metadata to \
             None, never error the search; got {degraded:?}"
    );

    let healthy = load_atom_body_line_counts(&runtime, "local", &atom_ids)
        .await
        .expect("undeadlined lookup must succeed");
    assert_eq!(
        healthy.and_then(|counts| counts.get(&atom_ids[0]).copied()),
        Some(0),
        "control: without a deadline the lookup returns real counts"
    );
}
