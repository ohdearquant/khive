use super::*;
use crate::knowledge::lexical_timeout::tests::{with_pass_timeouts, with_timeout};
use khive_pack_kg::KgPack;
use khive_runtime::{VerbRegistry, VerbRegistryBuilder};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tracing::{field::Visit, instrument::WithSubscriber, span, Event, Metadata, Subscriber};

fn registry(runtime: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(crate::KnowledgePack::new(runtime.clone()));
    let registry = builder.build().expect("registry");
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

async fn fixture(foreign: bool) -> KhiveRuntime {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let sql = runtime.sql();
    let mut writer = sql.writer().await.expect("writer");
    for (id, ns, slug, content) in [
        (
            "10000000-0000-0000-0000-000000000001",
            "local",
            "local-control",
            "unrelated background",
        ),
        (
            "10000000-0000-0000-0000-000000000002",
            "tenant-b",
            "foreign-control",
            "zzoraclezz",
        ),
    ]
    .into_iter()
    .take(if foreign { 2 } else { 1 })
    {
        writer.execute(SqlStatement {
            sql: "INSERT INTO knowledge_atoms (id, namespace, slug, name, content, tags, finalized, status, created_at, updated_at) VALUES (?1, ?2, ?3, ?3, ?4, '[]', 1, 'reviewed', 0, 0)".into(),
            params: [id, ns, slug, content].into_iter().map(|value| SqlValue::Text(value.into())).collect(),
            label: None,
        }).await.expect("seed fixture");
    }
    drop(writer);
    runtime
}

async fn fetch(runtime: &KhiveRuntime, query: &str) -> FtsFetchOutcome {
    let configured = lexical_stage_budget();
    let started = tokio::time::Instant::now();
    khive_storage::scope_request_read_deadline(configured, async {
        fetch_fts_candidates(
            runtime,
            "local",
            query,
            None,
            &[],
            &[],
            5,
            &FtsTermBudget::new(),
            LexicalStage::new(LexicalPass::Full, started, configured),
        )
        .await
    })
    .await
    .expect("lexical fetch")
}

#[derive(Clone, Default)]
struct TimeoutEvents(Arc<Mutex<Vec<Value>>>);

#[derive(Default)]
struct Fields(serde_json::Map<String, Value>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().into(), json!(format!("{value:?}")));
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().into(), json!(value));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().into(), json!(value));
    }
}

impl Subscriber for TimeoutEvents {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }
    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields.0.contains_key("stage_elapsed_ms") {
            fields.0.remove("message");
            self.0
                .lock()
                .expect("events lock")
                .push(Value::Object(fields.0));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn each_catch_site_captures_its_phase_and_identical_structured_event() {
    let runtime = fixture(true).await;
    for (phase, label, query) in [
        (LexicalPhase::ReaderOpen, "reader_open", "zzoraclezz"),
        (LexicalPhase::TermFrequency, "term_frequency", "zzoraclezz"),
        (LexicalPhase::PhaseARowids, "phase_a_rowids", "zzoraclezz"),
        (
            LexicalPhase::PhaseBHydration,
            "phase_b_hydration",
            "zzoraclezz",
        ),
        (
            LexicalPhase::EligibilityFallback,
            "eligibility_fallback",
            "zzoraclezz",
        ),
        (
            LexicalPhase::NamespaceMembership,
            "namespace_membership",
            "zzoraclezz",
        ),
        (
            LexicalPhase::NamespaceExistence,
            "namespace_existence",
            "zzoraclezz",
        ),
        (LexicalPhase::ExactNameProbe, "exact_name_probe", "AI"),
    ] {
        let events = TimeoutEvents::default();
        let outcome = with_timeout(
            vec![phase],
            Duration::from_millis(7),
            with_phase_a_widen_ceiling_override(1, fetch(&runtime, query)),
        )
        .with_subscriber(events.clone())
        .await;
        let detail = outcome
            .timeout
            .unwrap_or_else(|| panic!("missing timeout detail for {label}"));
        // The term-frequency site is the ordering probe and runs under its own
        // quarter-of-the-stage bound; every other site runs under the stage's.
        // Asserting that here is what keeps the bound from being silently
        // mislabelled at a catch site nobody looks at.
        let probe_site = matches!(phase, LexicalPhase::TermFrequency);
        let expected = json!({
            "pass": "full", "phase": label,
            "bound": if probe_site { "ordering_probe" } else { "stage" },
            "stage_elapsed_ms": 7, "operation_elapsed_ms": 7,
            "configured_budget_ms": 2000, "effective_budget_ms": 2000,
            "read_budget_ms": if probe_site { 500 } else { 2000 },
        });
        assert_eq!(serde_json::to_value(detail).unwrap(), expected, "{label}");
        // The log record now also carries the completed-read breakdown, whose
        // value differs per catch site by construction: it names the phases that
        // finished before this one was cut. Assert it is present at every site,
        // then compare the rest, so "identical structured event" still means the
        // fixed fields and the new field is not silently optional.
        let mut logged = events.0.lock().unwrap().clone();
        assert_eq!(logged.len(), 1, "{label}");
        let mut logged = logged.remove(0);
        let completed = logged
            .as_object_mut()
            .expect("event is an object")
            .remove("completed_reads");
        assert!(completed.is_some(), "no completed-read breakdown: {label}");
        assert_eq!(logged, expected, "{label}");
        assert!(
            outcome.atoms.is_empty(),
            "base drops the unfinished term: {label}"
        );
        assert_eq!(outcome.state, LexicalCandidateState::TimedOut, "{label}");
        tokio::time::advance(Duration::from_millis(123)).await;
        assert_eq!(
            detail.stage_elapsed_ms, 7,
            "elapsed must be frozen at capture"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn short_exact_name_timeout_degrades_within_original_lexical_budget() {
    let runtime = fixture(false).await;
    let token = runtime.authorize(Namespace::local()).unwrap();
    let configured = Duration::from_millis(2000);
    khive_storage::scope_request_read_deadline(Duration::from_millis(1000), async {
        let started = tokio::time::Instant::now();
        let mut stage = LexicalStage::new(LexicalPass::Full, started, configured);
        stage
            .read(LexicalPhase::ReaderOpen, async {
                tokio::time::advance(Duration::from_millis(30)).await;
                Ok(())
            })
            .await
            .unwrap();
        let sql = runtime.sql();
        let mut reader = sql.reader().await.unwrap();
        let outcome = with_timeout(
            vec![LexicalPhase::ExactNameProbe],
            Duration::from_millis(7),
            fetch_exact_name_candidate(reader.as_mut(), "local", "AI", None, &[], &[], &mut stage),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, ExactNameProbe::TimedOut));
        let timeout = stage.timeout.unwrap();
        assert_eq!(timeout.configured_budget_ms, 2000);
        assert_eq!(timeout.effective_budget_ms, 1000);
        assert_eq!(timeout.stage_elapsed_ms, 37);
        assert_eq!(timeout.operation_elapsed_ms, 7);
    })
    .await;

    let response = with_timeout(
        vec![LexicalPhase::ExactNameProbe],
        Duration::from_millis(7),
        KnowledgeHandlers::search(
            &runtime,
            &token,
            json!({"query": "AI", "rerank": false}),
            &vamana::new_shared(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(response["total"], 0);
    assert_eq!(response["candidate_provenance"]["lexical"], "timed_out");
    assert_eq!(response["degraded"]["lexical_timeout"], true);
    assert_eq!(response["degraded"]["lexical_timeout_instrumented"], true);
    assert!(response["degraded"]
        .get("lexical_timeout_details")
        .is_none());
}

#[tokio::test(start_paused = true)]
async fn public_dispatch_preserves_boolean_and_all_three_pass_tags() {
    let runtime = fixture(false).await;
    let registry = registry(&runtime);
    for (verb, decompose, passes) in [
        ("knowledge.search", false, vec!["full"]),
        (
            "knowledge.search",
            true,
            vec!["full", "subquery_1", "subquery_2"],
        ),
        ("knowledge.suggest", false, vec!["full"]),
    ] {
        let mut args = json!({"query": "alpha beta gamma delta epsilon zeta"});
        if verb == "knowledge.search" {
            args["decompose"] = json!(decompose);
            args["rerank"] = json!(false);
        }
        let mut response = with_timeout(
            vec![LexicalPhase::TermFrequency],
            Duration::from_millis(9),
            registry.dispatch(verb, args),
        )
        .await
        .expect("public dispatch");
        let details = response["degraded"]["lexical_timeout_details"]
            .as_array()
            .expect("missing public lexical_timeout_details");
        assert_eq!(details.len(), passes.len(), "one record per executed pass");
        for (detail, pass) in details.iter().zip(passes) {
            assert_eq!(
                *detail,
                json!({
                    "pass": pass, "phase": "term_frequency",
                    "bound": "ordering_probe",
                    "stage_elapsed_ms": 9, "operation_elapsed_ms": 9,
                    "configured_budget_ms": 2000, "effective_budget_ms": 2000,
                    "read_budget_ms": 500,
                })
            );
        }
        response["degraded"]
            .as_object_mut()
            .unwrap()
            .remove("lexical_timeout_details");
        assert_eq!(
            response["degraded"]
                .as_object_mut()
                .unwrap()
                .remove("lexical_timeout_instrumented"),
            Some(json!(true))
        );
        // The injected expiry is the ORDERING PROBE's, so the response reports a
        // skipped optimization and NOT `lexical_timeout`: every term was still
        // queried and no rows are missing. `candidate_provenance` is unchanged,
        // because the candidate-state classification is deliberately untouched.
        let expected = if verb == "knowledge.search" {
            json!({
                "results": [], "total": 0,
                "candidate_provenance": {"lexical": "timed_out", "fallback": "none", "terms_truncated": false},
                "degraded": {"lexical_timeout": true, "lexical_ordering_probe_timeout": true}
            })
        } else {
            json!({"results": [], "total": 0,
                   "degraded": {"lexical_timeout": true, "lexical_ordering_probe_timeout": true}})
        };
        assert_eq!(response, expected, "empty-timeout response: {verb}");
    }
}

#[tokio::test(start_paused = true)]
async fn healthy_dispatch_omits_timeout_details() {
    let runtime = fixture(false).await;
    let registry = registry(&runtime);
    for verb in ["knowledge.search", "knowledge.suggest"] {
        let response = registry
            .dispatch(
                verb,
                json!({"query": "unrelated background alpha beta gamma"}),
            )
            .await
            .expect("healthy public dispatch");
        assert!(response
            .get("degraded")
            .and_then(|d| d.get("lexical_timeout_details"))
            .is_none());
        assert!(response
            .get("degraded")
            .and_then(|d| d.get("lexical_timeout"))
            .is_none());
        assert!(response
            .get("degraded")
            .and_then(|d| d.get("lexical_timeout_instrumented"))
            .is_none());
        assert_eq!(
            response["total"],
            if verb == "knowledge.search" { 1 } else { 0 }
        );
        if verb == "knowledge.search" {
            assert_eq!(
                response["candidate_provenance"],
                json!({"lexical": "matched", "fallback": "none", "terms_truncated": false})
            );
            assert_eq!(
                response["results"][0]["score_provenance"],
                json!({
                    "sources": ["lexical"], "embedding_rerank": false,
                    "normalization": "s_over_s_plus_1", "calibrated": false,
                })
            );
        } else {
            assert!(response.get("candidate_provenance").is_none());
        }
    }
}

#[tokio::test(start_paused = true)]
async fn public_details_do_not_reveal_foreign_matches_or_data_dependent_phases() {
    let mut responses = Vec::new();
    let mut observed = Vec::new();
    for foreign in [false, true] {
        let runtime = fixture(foreign).await;
        let registry = registry(&runtime);
        let healthy = fetch(&runtime, "zzoraclezz").await;
        assert!(healthy.atoms.is_empty());
        assert_eq!(healthy.state, LexicalCandidateState::NoMatch);
        assert!(healthy.timeout.is_none());
        let healthy = registry
            .dispatch(
                "knowledge.search",
                json!({"query": "zzoraclezz", "rerank": false}),
            )
            .await
            .expect("healthy public miss");
        assert_eq!(
            healthy,
            json!({
                "results": [], "total": 0,
                "candidate_provenance": {"lexical": "no_match", "fallback": "none", "terms_truncated": false},
            }),
            "a healthy miss must not reveal a foreign match: {foreign}"
        );
        // Both hidden phases are reachable through the same local matching row.
        for phase in [LexicalPhase::PhaseARowids, LexicalPhase::PhaseBHydration] {
            let events = TimeoutEvents::default();
            let response = with_timeout(
                vec![phase],
                Duration::from_millis(11),
                registry.dispatch(
                    "knowledge.search",
                    json!({"query": "unrelated", "rerank": false}),
                ),
            )
            .with_subscriber(events.clone())
            .await
            .expect("public dispatch");
            let events = events.0.lock().unwrap();
            assert_eq!(events.len(), 1);
            observed.push(events[0]["phase"].clone());
            responses.push(response);
        }
        for phase in [LexicalPhase::ReaderOpen, LexicalPhase::TermFrequency] {
            let response = with_timeout(
                vec![phase],
                Duration::from_millis(11),
                registry.dispatch(
                    "knowledge.search",
                    json!({"query": "zzoraclezz", "rerank": false}),
                ),
            )
            .await
            .expect("public timing-only phase");
            // Both phases in this loop are PUBLIC, but only one of them is the
            // ordering probe, so the bound, the read budget and the extra flag
            // all differ between them. That the two corpora agree for EACH is
            // the property under test; that the two phases differ from each
            // other is just the contract.
            let probe_site = matches!(phase, LexicalPhase::TermFrequency);
            let mut expected = json!({"results": [], "total": 0,
            "candidate_provenance": {"lexical": "timed_out", "fallback": "none", "terms_truncated": false},
            "degraded": {
                "lexical_timeout": true, "lexical_timeout_instrumented": true,
                "lexical_timeout_details": [{
                    "pass": "full", "phase": phase.label(),
                    "bound": if probe_site { "ordering_probe" } else { "stage" },
                    "stage_elapsed_ms": 11,
                    "operation_elapsed_ms": 11, "configured_budget_ms": 2000, "effective_budget_ms": 2000,
                    "read_budget_ms": if probe_site { 500 } else { 2000 },
                }]
            }});
            if probe_site {
                expected["degraded"]["lexical_ordering_probe_timeout"] = json!(true);
            }
            assert_eq!(
                response, expected,
                "public diagnostic must not vary with foreign corpus: {foreign}"
            );
        }
    }
    assert_eq!(
        observed,
        vec![
            json!("phase_a_rowids"),
            json!("phase_b_hydration"),
            json!("phase_a_rowids"),
            json!("phase_b_hydration"),
        ],
        "positive control: both hidden phases ran against both corpora"
    );
    for response in &responses {
        assert_eq!(
            *response,
            json!({"results": [], "total": 0,
            "candidate_provenance": {"lexical": "timed_out", "fallback": "none", "terms_truncated": false},
            "degraded": {
                "lexical_timeout": true, "lexical_timeout_instrumented": true
            }})
        );
    }
    let serialized: Vec<_> = responses
        .iter()
        .map(|response| serde_json::to_vec(response).expect("serialize public JSON"))
        .collect();
    assert!(
        serialized.windows(2).all(|pair| pair[0] == pair[1]),
        "public JSON must not reveal the foreign corpus or the hidden timeout phase"
    );
}

#[tokio::test(start_paused = true)]
async fn mixed_pass_capability_marker_does_not_reveal_foreign_matches() {
    let mut responses = Vec::new();
    for foreign in [false, true] {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        if foreign {
            let sql = runtime.sql();
            let mut writer = sql.writer().await.expect("writer");
            writer.execute(SqlStatement {
                sql: "INSERT INTO knowledge_atoms (id, namespace, slug, name, content, tags, finalized, status, created_at, updated_at) VALUES (?1, ?2, ?3, ?3, ?4, '[]', 1, 'reviewed', 0, 0)".into(),
                params: [
                    "10000000-0000-0000-0000-000000000002", "tenant-b",
                    "foreign-control", "zzoraclezz",
                ].into_iter().map(|value| SqlValue::Text(value.into())).collect(),
                label: None,
            }).await.expect("seed foreign row with no local rows");
        }
        let registry = registry(&runtime);
        let events = TimeoutEvents::default();
        // The full pass is identical; only the foreign match enables the hidden read.
        let response = with_pass_timeouts(
            vec![
                (LexicalPass::Full, LexicalPhase::TermFrequency),
                (LexicalPass::Subquery1, LexicalPhase::PhaseARowids),
            ],
            Duration::from_millis(11),
            registry.dispatch(
                "knowledge.search",
                json!({
                    "query": "zzoraclezz alphazz betazz gammazz",
                    "decompose": true, "rerank": false,
                }),
            ),
        )
        .with_subscriber(events.clone())
        .await
        .expect("mixed-pass public dispatch");
        let observed: Vec<_> = events
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|detail| (detail["pass"].clone(), detail["phase"].clone()))
            .collect();
        let mut expected = vec![(json!("full"), json!("term_frequency"))];
        if foreign {
            expected.push((json!("subquery_1"), json!("phase_a_rowids")));
        }
        assert_eq!(
            observed, expected,
            "positive control: only the foreign corpus enables the hidden timeout"
        );
        assert_eq!(
            response["degraded"]["lexical_timeout_instrumented"], true,
            "capability marker must be true regardless of hidden records: foreign={foreign}"
        );
        assert_eq!(
            response,
            json!({"results": [], "total": 0,
            "candidate_provenance": {"lexical": "partial_timeout", "fallback": "none", "terms_truncated": false},
            "degraded": {
                "lexical_timeout": true, "lexical_ordering_probe_timeout": true,
                "lexical_timeout_instrumented": true,
                "lexical_timeout_details": [{
                    "pass": "full", "phase": "term_frequency",
                    "bound": "ordering_probe",
                    "stage_elapsed_ms": 11, "operation_elapsed_ms": 11,
                    "configured_budget_ms": 2000, "effective_budget_ms": 2000,
                    "read_budget_ms": 500,
                }]
            }}),
            "both corpora must disclose exactly the same public record"
        );
        responses.push(serde_json::to_vec(&response).expect("serialize public JSON"));
    }
    assert_eq!(
        responses[0], responses[1],
        "public JSON must be byte-identical despite different hidden timeout records"
    );
}

#[test]
fn attachment_preserves_other_degradation_fields_and_hides_operator_only_phases() {
    let base = json!({"results": [],
    "candidate_provenance": {"lexical": "timed_out", "fallback": "none", "terms_truncated": false},
    "degraded": {
        "lexical_timeout": true, "reason": "ann_unavailable", "mode": "no_match", "cache_safe": false,
        "body_lines_timeout": true, "hydration_failures": 2, "member_sizing_timeout": ["domain"]
    }});
    for phase in [
        LexicalPhase::PhaseARowids,
        LexicalPhase::PhaseBHydration,
        LexicalPhase::EligibilityFallback,
        LexicalPhase::NamespaceMembership,
        LexicalPhase::NamespaceExistence,
    ] {
        let mut response = base.clone();
        attach_lexical_timeout_degradation(
            &mut response,
            &[LexicalTimeout {
                pass: LexicalPass::Full,
                phase,
                stage_elapsed_ms: 4,
                operation_elapsed_ms: 3,
                configured_budget_ms: 2000,
                effective_budget_ms: 70,
                bound: LexicalBound::Stage,
                read_budget_ms: 2000,
            }],
        );
        assert_eq!(
            response["degraded"]
                .as_object_mut()
                .unwrap()
                .remove("lexical_timeout_instrumented"),
            Some(json!(true))
        );
        assert_eq!(response, base);
    }
    let mut public = base.clone();
    attach_lexical_timeout_degradation(
        &mut public,
        &[LexicalTimeout {
            pass: LexicalPass::Full,
            phase: LexicalPhase::TermFrequency,
            stage_elapsed_ms: 4,
            operation_elapsed_ms: 3,
            configured_budget_ms: 2000,
            effective_budget_ms: 70,
            bound: LexicalBound::Stage,
            read_budget_ms: 2000,
        }],
    );
    assert_eq!(
        public["degraded"]
            .as_object_mut()
            .unwrap()
            .remove("lexical_timeout_instrumented"),
        Some(json!(true))
    );
    let detail = public["degraded"]
        .as_object_mut()
        .unwrap()
        .remove("lexical_timeout_details")
        .expect("missing public timeout detail");
    assert_eq!(detail.as_array().unwrap().len(), 1);
    assert_eq!(
        public, base,
        "adding details must preserve every pre-existing field"
    );
    attach_lexical_timeout_degradation(&mut public, &[]);
    assert_eq!(public, base, "healthy attachment must be a no-op");
}

struct TimedFrequencyReader(usize);

#[async_trait::async_trait]
impl khive_storage::SqlReader for TimedFrequencyReader {
    async fn query_row(
        &mut self,
        _: SqlStatement,
    ) -> khive_storage::types::StorageResult<Option<khive_storage::types::SqlRow>> {
        panic!("frequency probes must use query_all")
    }
    async fn query_all(
        &mut self,
        _: SqlStatement,
    ) -> khive_storage::types::StorageResult<Vec<khive_storage::types::SqlRow>> {
        self.0 += 1;
        if self.0 == 1 {
            tokio::time::advance(Duration::from_millis(25)).await;
            Ok(vec![khive_storage::types::SqlRow {
                columns: vec![khive_storage::types::SqlColumn {
                    name: "frequency".into(),
                    value: SqlValue::Integer(0),
                }],
            }])
        } else {
            tokio::time::advance(Duration::from_millis(7)).await;
            Err(khive_storage::StorageError::Timeout {
                operation: "test.probe".into(),
            })
        }
    }
    async fn query_scalar(
        &mut self,
        _: SqlStatement,
    ) -> khive_storage::types::StorageResult<Option<SqlValue>> {
        panic!("frequency probes must use query_all")
    }
    async fn explain(
        &mut self,
        _: SqlStatement,
    ) -> khive_storage::types::StorageResult<Vec<khive_storage::types::SqlRow>> {
        panic!("diagnostics must not issue explain queries")
    }
}

#[tokio::test(start_paused = true)]
async fn frequency_capture_times_the_failed_probe_not_the_whole_term_loop() {
    let configured = Duration::from_millis(2000);
    let started = tokio::time::Instant::now();
    khive_storage::scope_request_read_deadline(configured, async {
        let mut stage = LexicalStage::new(LexicalPass::Full, started, configured);
        let mut reader = TimedFrequencyReader(0);
        let result = rarest_fts_terms_first(
            &mut reader,
            vec!["first".into(), "second".into()],
            &mut stage,
        )
        .await;
        assert!(matches!(
            result,
            Err(khive_storage::StorageError::Timeout { .. })
        ));
        assert_eq!(reader.0, 2);
        let detail = stage
            .timeout
            .expect("missing term_frequency timeout detail");
        assert_eq!(detail.phase, LexicalPhase::TermFrequency);
        assert_eq!(detail.stage_elapsed_ms, 32);
        assert_eq!(
            detail.operation_elapsed_ms, 7,
            "operation elapsed must exclude the successful first probe"
        );
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn partial_scored_candidates_match_the_base_rare_term_fixture() {
    let runtime = KhiveRuntime::memory().expect("runtime");
    seed_low_overlap_corpus(&runtime, 1_000, 20).await;
    let weights = Weights::default();
    let term_budget = FtsTermBudget::new();
    let ctx = SearchCtx {
        runtime: &runtime,
        ns: "local",
        role: None,
        type_filter: None,
        min_score: 0.0,
        w: &weights,
        fetch_limit: 100,
        statuses: &[],
        exclude_statuses: &[],
        term_budget: &term_budget,
    };
    for query in ["term1 term18", "term18 term1"] {
        let outcome = with_fts_deadline_advance_after_term(
            1,
            Duration::from_millis(2000),
            search_core(&ctx, query, LexicalPass::Full),
        )
        .await
        .expect("partial scored results");
        assert_eq!(outcome.lexical_state, LexicalCandidateState::PartialTimeout);
        assert_eq!(
            outcome.lexical_timeouts.len(),
            1,
            "scored return must retain timeout detail"
        );
        assert_eq!(
            outcome.lexical_timeouts[0].phase,
            LexicalPhase::PhaseARowids
        );
        // The base fixture completes exactly the 50 term18 rows before the common term.
        let expected: Vec<_> = (0..50)
            .map(|i| format!("lowoverlap-{:06}", 18 + i * 20))
            .collect();
        assert_eq!(
            outcome
                .hits
                .iter()
                .map(|hit| hit.slug.clone())
                .collect::<Vec<_>>(),
            expected
        );
        let mut legacy = json!({});
        attach_lexical_timeout_degradation(&mut legacy, &outcome.lexical_timeouts);
        assert_eq!(
            legacy["degraded"]
                .as_object_mut()
                .unwrap()
                .remove("lexical_timeout_instrumented"),
            Some(json!(true))
        );
        assert_eq!(legacy, json!({"degraded": {"lexical_timeout": true}}));
    }
}

#[tokio::test(start_paused = true)]
async fn configured_budget_uses_the_stage_override() {
    let runtime = fixture(false).await;
    let registry = registry(&runtime);
    let response = with_lexical_stage_budget_override_ms(
        137,
        with_timeout(
            vec![LexicalPhase::TermFrequency],
            Duration::from_millis(9),
            registry.dispatch(
                "knowledge.search",
                json!({"query": "zzoraclezz", "rerank": false}),
            ),
        ),
    )
    .await
    .expect("public dispatch");
    assert_eq!(
        response["candidate_provenance"],
        json!({"lexical": "timed_out", "fallback": "none", "terms_truncated": false})
    );
    assert_eq!(
        response["degraded"]["lexical_timeout_details"][0]["configured_budget_ms"],
        137
    );
    assert_eq!(
        response["degraded"]["lexical_timeout_details"][0]["effective_budget_ms"],
        137
    );
}

/// Build a timeout record differing only in which budget governed the read, so
/// a test can vary the bound with every other field held equal.
fn bounded_record(
    bound: LexicalBound,
    phase: LexicalPhase,
    stage_elapsed_ms: u64,
    read_budget_ms: u64,
) -> LexicalTimeout {
    LexicalTimeout {
        pass: LexicalPass::Full,
        phase,
        stage_elapsed_ms,
        operation_elapsed_ms: 123,
        configured_budget_ms: 2000,
        effective_budget_ms: 1999,
        bound,
        read_budget_ms,
    }
}

/// A skipped ordering hint and a cut candidate fetch must not produce the same
/// `degraded` payload (issue #2879).
///
/// Both records name the SAME phase. That is the point of the fixture: the
/// bound is the only thing that differs, so a fix that keyed off
/// `term_frequency` instead of the bound would fail here rather than passing
/// for the wrong reason.
#[test]
fn a_probe_fallback_and_a_cut_fetch_do_not_produce_the_same_degraded_payload() {
    let probe = bounded_record(
        LexicalBound::OrderingProbe,
        LexicalPhase::TermFrequency,
        501,
        500,
    );
    let cut = bounded_record(LexicalBound::Stage, LexicalPhase::TermFrequency, 2000, 2000);

    let mut probe_out = json!({});
    attach_lexical_timeout_degradation(&mut probe_out, &[probe]);
    let mut cut_out = json!({});
    attach_lexical_timeout_degradation(&mut cut_out, &[cut]);

    // The discriminating assertion runs FIRST, deliberately. Under the
    // shared-flag behaviour the two arms collapse to one payload, and a
    // per-arm equality placed ahead of this would fire instead -- so the
    // assertion that actually separates "the optimization was skipped" from
    // "rows are missing" would never execute in the case it exists for.
    assert_ne!(
        probe_out, cut_out,
        "a skipped ordering hint and a cut candidate fetch must be tellable apart \
         somewhere in the payload, or a complete response reads as though rows were lost"
    );

    assert_eq!(
        probe_out["degraded"]["lexical_ordering_probe_timeout"],
        json!(true),
        "the skipped optimization must still be disclosed, under its own name"
    );
    assert!(
        cut_out["degraded"]
            .get("lexical_ordering_probe_timeout")
            .is_none(),
        "a stage-bounded read is not an ordering-probe fallback"
    );

    // BOTH keep `lexical_timeout`, and that is deliberate. Issue #2879 asked for
    // it to be ABSENT on a pure fallback. That cannot be done safely: the flag
    // would then be false for a probe-only expiry and true as soon as a later
    // phase timed out, and the later phases are reachable only through GLOBAL
    // index matches -- so a caller would learn that another namespace's row
    // matched by watching this flag appear. The coarse flag is load-bearing
    // BECAUSE it is coarse. What the caller gets instead is the record's own
    // `bound`, safe to publish because the probe runs in a public phase.
    assert_eq!(probe_out["degraded"]["lexical_timeout"], json!(true));
    assert_eq!(cut_out["degraded"]["lexical_timeout"], json!(true));
    assert_eq!(
        probe_out["degraded"]["lexical_timeout_details"][0]["bound"],
        json!("ordering_probe")
    );
    assert_eq!(
        cut_out["degraded"]["lexical_timeout_details"][0]["bound"],
        json!("stage"),
        "the bound is what separates them in the public record"
    );
}

/// A decomposed request runs up to three passes, so one pass can fall back on
/// its ordering probe while another has its candidate fetch cut. Those are two
/// different things that happened and both must be reported.
#[test]
fn a_pass_that_fell_back_and_a_pass_that_was_cut_are_both_reported() {
    let mut out = json!({});
    attach_lexical_timeout_degradation(
        &mut out,
        &[
            bounded_record(
                LexicalBound::OrderingProbe,
                LexicalPhase::TermFrequency,
                501,
                500,
            ),
            bounded_record(
                LexicalBound::Stage,
                LexicalPhase::PhaseBHydration,
                2000,
                2000,
            ),
        ],
    );
    assert_eq!(out["degraded"]["lexical_timeout"], json!(true));
    assert_eq!(
        out["degraded"]["lexical_ordering_probe_timeout"],
        json!(true)
    );
}

/// The record names the budget that governed the read it describes.
///
/// `configured_budget_ms` and `effective_budget_ms` keep their documented
/// meaning -- both describe the stage at its entry -- which is exactly why a
/// probe-bounded record needs a third number: without it the record reads as a
/// 2000 ms budget that expired at 501 ms.
#[test]
fn a_probe_bounded_record_names_the_probe_budget_not_the_stage_budget() {
    let probe = bounded_record(
        LexicalBound::OrderingProbe,
        LexicalPhase::TermFrequency,
        501,
        500,
    );
    assert_eq!(probe.read_budget_ms, 500);
    assert_eq!(
        probe.effective_budget_ms, 1999,
        "the stage-entry allowance keeps its documented meaning and is not overwritten"
    );
    assert_ne!(
        probe.read_budget_ms, probe.effective_budget_ms,
        "if these were equal the new field would carry no information the old one lacked"
    );

    let cut = bounded_record(
        LexicalBound::Stage,
        LexicalPhase::PhaseBHydration,
        2000,
        1999,
    );
    assert_eq!(
        cut.read_budget_ms, cut.effective_budget_ms,
        "a stage-bounded read is governed by the EFFECTIVE budget: a parent deadline tighter than          the stage budget is what actually cuts it, so naming the configured budget here would          reintroduce the defect this field exists to remove"
    );
}

/// `degraded.lexical_ordering_probe_timeout` is safe to publish ONLY because the
/// ordering probe runs in a phase whose entry does not depend on corpus
/// contents. `LexicalPhase::public` is where that property is declared, and this
/// test is the tripwire on it.
///
/// If `term_frequency` were ever reclassified as operator-only — which is what
/// would happen if its reachability became corpus-dependent — then the probe
/// flag would begin telling a caller that a row in another namespace matched.
/// That is precisely the disclosure the neighbouring `lexical_timeout` flag is
/// kept deliberately COARSE to avoid, and it would arrive through the new flag
/// instead. The reclassification must fail a test rather than pass review, so
/// the invariant is asserted here and not only described in `docs/design.md`.
#[test]
fn the_ordering_probe_flag_rests_on_term_frequency_being_corpus_independent() {
    assert!(
        LexicalPhase::TermFrequency.public(),
        "the ordering probe's phase must be corpus-independent, or the probe flag \
         derived from it becomes a cross-namespace disclosure channel"
    );

    // The exact public set, asserted as a set rather than a sample: a phase
    // ADDED here is a phase whose records start reaching callers, and a phase
    // REMOVED is one a published flag may no longer rest on. Either direction
    // needs a deliberate decision, so either direction reddens this.
    let public: Vec<&str> = [
        LexicalPhase::ReaderOpen,
        LexicalPhase::TermFrequency,
        LexicalPhase::PhaseARowids,
        LexicalPhase::PhaseBHydration,
        LexicalPhase::EligibilityFallback,
        LexicalPhase::NamespaceMembership,
        LexicalPhase::NamespaceExistence,
        LexicalPhase::ExactNameProbe,
    ]
    .into_iter()
    .filter(|phase| phase.public())
    .map(LexicalPhase::label)
    .collect();
    assert_eq!(
        public,
        vec!["reader_open", "term_frequency"],
        "the public phase set decides what every published flag and record may reveal"
    );
}
