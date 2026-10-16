use super::*;

#[test]
fn decode_embedding_round_trips() {
    let values: Vec<f32> = vec![1.0, -0.5, 0.0, 42.5];
    let blob: Vec<u8> = values.iter().flat_map(|f| f.to_le_bytes()).collect();
    let decoded = decode_embedding(&blob);
    assert_eq!(decoded.len(), 4);
    for (a, b) in values.iter().zip(decoded.iter()) {
        assert!((a - b).abs() < 1e-6, "mismatch: {a} vs {b}");
    }
}

#[test]
fn decode_embedding_rejects_misaligned_blob() {
    let blob = vec![0u8; 7];
    assert!(decode_embedding(&blob).is_empty());
}

#[test]
fn cosine_similarity_identical_vectors() {
    let v = vec![1.0f32, 0.0, 0.0];
    assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
}

#[test]
fn cosine_similarity_orthogonal_vectors() {
    let a = vec![1.0f32, 0.0];
    let b = vec![0.0f32, 1.0];
    assert!(cosine_similarity(&a, &b).abs() < 1e-6);
}

#[test]
fn cosine_similarity_mismatched_lengths_returns_zero() {
    let a = vec![1.0f32, 2.0];
    let b = vec![1.0f32];
    assert_eq!(cosine_similarity(&a, &b), 0.0);
}

#[test]
fn bm25_scores_higher_for_matching_doc() {
    let terms = vec!["rust".to_string(), "memory".to_string()];
    let docs = &[
        (
            "Rust memory model",
            "Rust ownership and memory safety rules",
        ),
        ("Python lists", "Python list operations and indexing"),
    ];
    let scores = compute_bm25_scores(&terms, docs).unwrap();
    assert_eq!(scores.len(), 2);
    assert!(scores[0] > scores[1], "matching doc must score higher");
}

#[test]
fn bm25_empty_terms_returns_zeros() {
    let docs = &[("heading", "content"), ("heading2", "content2")];
    let scores = compute_bm25_scores(&[], docs).unwrap();
    assert!(scores.iter().all(|&s| s == 0.0));
}

#[test]
fn score_sections_sorted_desc() {
    let query_emb: Vec<f32> = vec![1.0, 0.0];
    let mut atom_cos: HashMap<String, f32> = HashMap::new();
    atom_cos.insert("a1".to_string(), 0.9);

    let sec = ScoredSection {
        id: "s1".to_string(),
        atom_id: "a1".to_string(),
        section_type: "overview".to_string(),
        heading: "Overview".to_string(),
        content: "introduction to the topic".to_string(),
        embedding: Some(vec![1.0, 0.0]),
    };
    let sec2 = ScoredSection {
        id: "s2".to_string(),
        atom_id: "a1".to_string(),
        section_type: "references".to_string(),
        heading: "References".to_string(),
        content: "unrelated bibliography content".to_string(),
        embedding: Some(vec![0.0, 1.0]),
    };

    let mut sections: HashMap<String, Vec<ScoredSection>> = HashMap::new();
    sections.insert("a1".to_string(), vec![sec, sec2]);

    let domain_scores: HashMap<String, f32> = HashMap::new();
    let type_weights: HashMap<String, f32> = HashMap::new();

    let results = score_sections(
        "overview introduction topic",
        &query_emb,
        &atom_cos,
        &sections,
        &domain_scores,
        &type_weights,
        &ComposeScoreWeights::default(),
    )
    .unwrap();

    assert_eq!(results.len(), 2);
    assert!(results[0].score >= results[1].score, "must be sorted desc");
    assert_eq!(results[0].section_id, "s1");
}

#[test]
fn default_weights_sum_to_one() {
    let w = ComposeScoreWeights::default();
    let sum = w.section_cosine + w.section_bm25 + w.atom_cosine + w.domain_score + w.type_weight;
    assert!((sum - 1.0).abs() < 1e-6, "weights sum to {sum}");
}

#[test]
fn unembedded_section_still_scored_via_keyword_signals() {
    let query_emb: Vec<f32> = vec![1.0, 0.0];
    let mut atom_cos: HashMap<String, f32> = HashMap::new();
    atom_cos.insert("a1".to_string(), 0.8);

    let embedded = ScoredSection {
        id: "s1".to_string(),
        atom_id: "a1".to_string(),
        section_type: "overview".to_string(),
        heading: "Overview".to_string(),
        content: "topic introduction".to_string(),
        embedding: Some(vec![1.0, 0.0]),
    };
    let unembedded = ScoredSection {
        id: "s2".to_string(),
        atom_id: "a1".to_string(),
        section_type: "details".to_string(),
        heading: "Details".to_string(),
        content: "topic details and explanation".to_string(),
        embedding: None,
    };

    let mut sections: HashMap<String, Vec<ScoredSection>> = HashMap::new();
    sections.insert("a1".to_string(), vec![embedded, unembedded]);

    let domain_scores: HashMap<String, f32> = HashMap::new();
    let type_weights: HashMap<String, f32> = HashMap::new();

    let results = score_sections(
        "topic overview",
        &query_emb,
        &atom_cos,
        &sections,
        &domain_scores,
        &type_weights,
        &ComposeScoreWeights::default(),
    )
    .unwrap();

    assert_eq!(results.len(), 2, "both sections must be scored");
    let unembedded_result = results.iter().find(|r| r.section_id == "s2").unwrap();
    assert_eq!(
        unembedded_result.score_breakdown.section_cosine, 0.0,
        "unembedded section_cosine must be 0"
    );
    assert!(
        unembedded_result.score > 0.0,
        "unembedded section must still have positive score from other signals"
    );
}

// ── ComposeTiming (#887) ────────────────────────────────────────────────

#[test]
fn slow_threshold_is_sane() {
    // Sanity-bounds the const rather than pinning it to exactly 10_000,
    // so a deliberate future retune doesn't require touching the test —
    // but a typo (e.g. dropping three zeros) still fails loudly. Both
    // sides are compile-time-constant, so `clippy::assertions_on_constants`
    // requires the `const { }` wrapper (this becomes a build-time check,
    // which is strictly stronger than a runtime test assertion).
    const {
        assert!(
            COMPOSE_SLOW_THRESHOLD_MS >= 1_000,
            "threshold must be well above a healthy sub-second compose"
        );
    }
    const {
        assert!(
            COMPOSE_SLOW_THRESHOLD_MS <= 60_000,
            "threshold must be well under the 300s client-side MCP timeout \
                 (#887) to give advance warning"
        );
    }
}

#[test]
fn begin_accumulates_duration_under_repeated_phase_names() {
    let mut t = ComposeTiming::start("test query", false);
    t.begin(Phase::Suggest).unwrap();
    std::thread::sleep(Duration::from_millis(2));
    // Second DB fetch (e.g. load_sections) accumulates into the same
    // Fetch bucket instead of overwriting or duplicating it.
    t.begin(Phase::Fetch).unwrap();
    std::thread::sleep(Duration::from_millis(2));
    t.begin(Phase::Rerank).unwrap();
    std::thread::sleep(Duration::from_millis(2));
    t.begin(Phase::Fetch).unwrap();
    std::thread::sleep(Duration::from_millis(2));
    t.begin(Phase::Trim).unwrap();

    let fetch_ms = t.phase_totals[Phase::Fetch.index()].as_millis();
    let rerank_ms = t.phase_totals[Phase::Rerank.index()].as_millis();
    assert!(
        fetch_ms >= 4,
        "accumulated fetch time must cover both begin(Fetch) spans (~4ms), got {fetch_ms}ms"
    );
    assert!(
        rerank_ms >= 2,
        "rerank bucket must cover its single span, got {rerank_ms}ms"
    );

    t.finish(3);
}

#[tokio::test]
async fn compose_stage_boundary_refuses_work_after_request_cancellation() {
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    khive_storage::scope_request_read_cancellation(cancel_rx, async move {
        let mut timing = ComposeTiming::start("cancel between stages", true);
        timing
            .begin(Phase::Suggest)
            .expect("active request may enter its first compose stage");
        cancel_tx.send(true).unwrap();

        let cancelled = timing.begin(Phase::Fetch);
        assert!(
            cancelled.is_err(),
            "compose must not start a later read phase after its request is cancelled"
        );
    })
    .await;
}

#[test]
fn helper_level_flush_active_covers_an_in_flight_phase() {
    // Unit-level check of `flush_active` itself, kept separate from the
    // call-site regressions below. Does not by itself guard `finish`/`Drop`; see
    // `finish_call_site_flushes_the_still_active_phase` and
    // `drop_call_site_flushes_the_still_active_phase` for that.
    let mut t = ComposeTiming::start("test query", true);
    t.begin(Phase::Suggest).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    // No begin(Phase::Fetch) — Suggest is still the active phase.
    t.flush_active();
    let suggest_ms = t.phase_totals[Phase::Suggest.index()].as_millis();
    assert!(
        suggest_ms >= 5,
        "in-flight Suggest phase must be flushed with nonzero duration, got {suggest_ms}ms"
    );
    t.finish(0);
}

#[test]
fn finish_call_site_flushes_the_still_active_phase() {
    // Regression for PR #915: the previous
    // version of this test called the private `flush_active` manually
    // before `finish`, so deleting `self.flush_active()` from inside
    // `finish` itself would have left the test green. This version calls
    // `finish` directly — with Suggest still active and NO manual
    // pre-flush, and asserts on `finish`'s own return value (the
    // post-flush breakdown), so the assertion can only pass if `finish`
    // performed the flush itself.
    let mut t = ComposeTiming::start("test query", true);
    t.begin(Phase::Suggest).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    // No begin(Phase::Fetch), no manual flush_active() — Suggest is still
    // the active phase when finish() is called, simulating finish()
    // being reached mid-phase (e.g. an error return partway through
    // suggest, matching the incident this feature diagnoses).
    let breakdown = t.finish(0);
    let suggest_ms = breakdown[Phase::Suggest.index()].1;
    assert!(
        suggest_ms >= 5,
        "finish() must flush the in-flight Suggest phase before returning/emitting, \
             got {suggest_ms}ms in breakdown {breakdown:?}"
    );
}

// ── tracing-capture harness for the Drop path ──────────────────────────
//
// `Drop::drop` cannot return a value (unlike `finish`), and its
// "abandoned" WARN fires unconditionally — no slow-threshold gate — so a
// real `tracing::Subscriber` capture is both necessary and cheap here
// (no multi-second sleep needed to cross COMPOSE_SLOW_THRESHOLD_MS, as
// would be required to observe `finish`'s WARN the same way). Mirrors the
// `CaptureSubscriber`/`Visit` pattern already used for the same purpose
// in `khive-runtime/src/pack.rs` and `khive-db/src/checkpoint.rs` —
// installed only as this test's thread-local default via
// `tracing::subscriber::with_default`, so it cannot observe or interfere
// with any other test.

#[derive(Clone, Debug, Default)]
struct CapturedEvent {
    message: Option<String>,
    phases: Option<String>,
}

#[derive(Default)]
struct CapturedEventVisitor(CapturedEvent);

impl tracing::field::Visit for CapturedEventVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let formatted = format!("{value:?}");
        match field.name() {
            "message" => {
                self.0.message = Some(
                    formatted
                        .trim_start_matches('"')
                        .trim_end_matches('"')
                        .to_string(),
                )
            }
            "phases" => self.0.phases = Some(formatted),
            _ => {}
        }
    }
}

struct CaptureSubscriber {
    events: std::sync::Arc<std::sync::Mutex<Vec<CapturedEvent>>>,
}

impl tracing::Subscriber for CaptureSubscriber {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut visitor = CapturedEventVisitor::default();
        event.record(&mut visitor);
        self.events.lock().unwrap().push(visitor.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn drop_call_site_flushes_the_still_active_phase() {
    // Regression for PR #915: the previous
    // version of this test called the private `flush_active` manually
    // before `drop(t)`, so deleting `self.flush_active()` from inside
    // `Drop::drop` itself would have left the test green. This version
    // captures the real event `Drop::drop` emits with Rerank still
    // active and NO manual pre-flush, so the assertion can only pass if
    // `Drop` performed the flush itself.
    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber {
        events: std::sync::Arc::clone(&buffer),
    };

    fn is_abandoned(e: &CapturedEvent) -> bool {
        e.message.as_deref()
            == Some(
                "knowledge.compose request abandoned before completion \
                     (client disconnect, cancellation, or daemon shutdown)",
            )
    }

    tracing::subscriber::with_default(subscriber, || {
        // The tests of this crate share one process when run by the
        // standard harness. While this capture is the only scoped
        // subscriber alive, tracing-core skips its dispatcher lock, so a
        // test on another thread that hits the WARN callsite for the first
        // time computes the callsite's interest from that thread's own
        // default (no subscriber) and caches it as disabled; the event
        // below then never reaches the capture. Rebuilding the interest
        // cache from this thread just before the drop recomputes it with
        // the capture installed. A first registration happens once per
        // callsite, so one that races the rebuild can only spoil the first
        // attempt. Every attempt drops a fresh value with Rerank still
        // active, and the assertions below are unchanged.
        for _ in 0..3 {
            let mut t = ComposeTiming::start("abandoned", false);
            t.begin(Phase::Rerank).unwrap();
            std::thread::sleep(Duration::from_millis(5));
            // No begin(next phase), no manual flush_active() — Rerank is
            // still active when `t` is dropped, simulating a future dropped
            // mid-phase (client disconnect, cancellation, daemon shutdown).
            tracing::callsite::rebuild_interest_cache();
            drop(t);
            if buffer.lock().unwrap().iter().any(is_abandoned) {
                break;
            }
        }
    });

    let events = buffer.lock().unwrap();
    let abandoned = events
        .iter()
        .find(|e| is_abandoned(e))
        .unwrap_or_else(|| panic!("expected the abandoned-request WARN, got: {events:?}"));
    let phases = abandoned
        .phases
        .as_deref()
        .expect("abandoned WARN must carry a phases field");
    assert!(
        !phases.contains("(\"rerank\", 0)"),
        "Drop must flush the in-flight Rerank phase into the emitted breakdown \
             (nonzero duration), got: {phases}"
    );
}

#[test]
fn begin_with_no_prior_active_phase_does_not_panic() {
    // The very first `begin` call has nothing to flush.
    let t = ComposeTiming::start("q", true);
    drop(t);
}

#[test]
fn query_bytes_is_byte_length_not_char_length() {
    // Multi-byte UTF-8 query: byte length (the documented, O(1) choice)
    // must not silently become a char count.
    let t = ComposeTiming::start("héllo wörld", false);
    assert_eq!(
        t.query_bytes, 13,
        "2 non-ASCII chars, each 2 bytes in UTF-8"
    );
    t.finish(0);
}
