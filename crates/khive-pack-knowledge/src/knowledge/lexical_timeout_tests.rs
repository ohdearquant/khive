use super::*;
use crate::knowledge::lexical_timeout::tests::with_timeout;
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
            LexicalPhase::RecentFallback,
            "recent_fallback",
            "zzabsentzz",
        ),
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
        let expected = json!({
            "pass": "full", "phase": label,
            "stage_elapsed_ms": 7, "operation_elapsed_ms": 7,
            "configured_budget_ms": 2000, "effective_budget_ms": 2000,
        });
        assert_eq!(serde_json::to_value(detail).unwrap(), expected, "{label}");
        assert_eq!(*events.0.lock().unwrap(), vec![expected], "{label}");
        assert!(
            outcome.atoms.is_empty(),
            "base drops the unfinished term: {label}"
        );
        tokio::time::advance(Duration::from_millis(123)).await;
        assert_eq!(
            detail.stage_elapsed_ms, 7,
            "elapsed must be frozen at capture"
        );
    }
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
                    "stage_elapsed_ms": 9, "operation_elapsed_ms": 9,
                    "configured_budget_ms": 2000, "effective_budget_ms": 2000,
                })
            );
        }
        response["degraded"]
            .as_object_mut()
            .unwrap()
            .remove("lexical_timeout_details");
        assert_eq!(
            response,
            json!({"results": [], "total": 0, "degraded": {"lexical_timeout": true}}),
            "legacy empty-timeout response must be unchanged: {verb}"
        );
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
        assert_eq!(
            response["total"],
            if verb == "knowledge.search" { 1 } else { 0 }
        );
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
        assert_eq!(
            healthy
                .atoms
                .iter()
                .map(|atom| atom.slug.as_str())
                .collect::<Vec<_>>(),
            ["local-control"]
        );
        assert!(healthy.timeout.is_none());
        let events = TimeoutEvents::default();
        // At the first corpus-dependent read, both arms fail at the same fake time.
        // The foreign row changes the phase, not the local results or elapsed time.
        let response = with_timeout(
            vec![LexicalPhase::PhaseARowids, LexicalPhase::RecentFallback],
            Duration::from_millis(11),
            registry.dispatch(
                "knowledge.search",
                json!({"query": "zzoraclezz", "rerank": false}),
            ),
        )
        .with_subscriber(events.clone())
        .await
        .expect("public dispatch");
        observed.push(events.0.lock().unwrap()[0]["phase"].clone());
        responses.push(response);
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
            assert_eq!(
                response,
                json!({"results": [], "total": 0, "degraded": {
                    "lexical_timeout": true, "lexical_timeout_details": [{
                        "pass": "full", "phase": phase.label(), "stage_elapsed_ms": 11,
                        "operation_elapsed_ms": 11, "configured_budget_ms": 2000, "effective_budget_ms": 2000,
                    }]
                }}),
                "public diagnostic must not vary with foreign corpus: {foreign}"
            );
        }
    }
    assert_eq!(
        observed,
        vec![json!("recent_fallback"), json!("phase_a_rowids")],
        "positive control: foreign corpus actually changed the internal phase"
    );
    assert_eq!(
        responses[0], responses[1],
        "the public response must not disclose that difference"
    );
    assert_eq!(
        responses[0],
        json!({"results": [], "total": 0, "degraded": {"lexical_timeout": true}})
    );
}

#[test]
fn attachment_preserves_other_degradation_fields_and_hides_operator_only_phases() {
    let base = json!({"results": [], "degraded": {
        "lexical_timeout": true, "reason": "ann_unavailable", "mode": "no_match", "cache_safe": false,
        "body_lines_timeout": true, "hydration_failures": 2, "member_sizing_timeout": ["domain"]
    }});
    for phase in [
        LexicalPhase::PhaseARowids,
        LexicalPhase::PhaseBHydration,
        LexicalPhase::EligibilityFallback,
        LexicalPhase::NamespaceMembership,
        LexicalPhase::RecentFallback,
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
            }],
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
        }],
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
            Ok(Vec::new())
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
    };
    for query in ["term1 term18", "term18 term1"] {
        let outcome = with_fts_deadline_advance_after_term(
            1,
            Duration::from_millis(2000),
            search_core(&ctx, query, LexicalPass::Full),
        )
        .await
        .expect("partial scored results");
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
        response["degraded"]["lexical_timeout_details"][0]["configured_budget_ms"],
        137
    );
    assert_eq!(
        response["degraded"]["lexical_timeout_details"][0]["effective_budget_ms"],
        137
    );
}
