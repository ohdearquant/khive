mod search_text_reason_tests {
    use super::*;

    const REASON: &str = "No text candidate survived matching, filtering, fusion, and the result limit. Plain text search combines normalized term groups conjunctively; try fewer terms.";

    fn legacy_arms(
        actual: &Value,
        text_status: &str,
        text_count: usize,
        vector_status: &str,
        vector_count: usize,
    ) {
        let mut legacy = actual.clone();
        legacy["text"]
            .as_object_mut()
            .expect("text arm object")
            .remove("reason");
        assert_eq!(
            legacy,
            json!({
                "text": {"status": text_status, "candidate_count": text_count},
                "vector": {"status": vector_status, "candidate_count": vector_count}
            }),
            "legacy arm status/count/shape changed"
        );
    }

    // Call only after every legacy control for the selected test has run.
    // Absence checks first distinguish incorrect positive/error predicates
    // from the unchanged baseline's missing zero-contribution explanation.
    fn reasons(test: &str, rows: Vec<(String, Value, bool)>) {
        eprintln!("{test}: legacy-controls-passed; reason rows={}", rows.len());
        for (label, arms, _) in rows.iter().filter(|(_, _, expected)| !expected) {
            assert!(arms["text"].get("reason").is_none(), "{label}: unexpected text reason");
        }
        for (label, arms, _) in rows.iter().filter(|(_, _, expected)| *expected) {
            assert_eq!(arms["text"].get("reason"), Some(&json!(REASON)), "{label}: exact reason");
        }
    }

    #[test]
    fn serializer_condition_matrix() {
        let statuses = [
            (SearchArmStatus::Ran, "ran"),
            (SearchArmStatus::Skipped, "skipped"),
            (SearchArmStatus::Error, "error"),
        ];
        let mut rows = Vec::new();
        for (text_status, text_name) in statuses {
            for count in [0, 1, 2] {
                for (vector_status, vector_name) in statuses {
                    let arms = crate::server::search_arm_participation_value(SearchArmParticipation {
                        text: SearchArmEvidence { status: text_status, candidate_count: count },
                        vector: SearchArmEvidence { status: vector_status, candidate_count: 0 },
                    });
                    legacy_arms(&arms, text_name, count, vector_name, 0);
                    rows.push((
                        format!("text={text_name}/{count}, vector={vector_name}/0"),
                        arms,
                        text_name == "ran" && count == 0,
                    ));
                }
            }
        }
        assert_eq!(rows.len(), 27);
        reasons("T1", rows);
    }

    async fn search_pair(server: &KhiveMcpServer, query: &str, limit: u32) -> (Value, Value) {
        let args = json!({"kind": "entity", "query": query, "limit": limit});
        let raw = server.registry.dispatch("search", args.clone()).await.expect("raw pack search");
        assert!(raw.is_array(), "raw pack shape must remain an array");
        let response = server.dispatch_request_local(RequestParams {
            ops: json!([{"tool": "search", "args": args}]).to_string(),
            presentation: Some("verbose".to_string()),
            format: Some("json".to_string()),
            ..Default::default()
        }).await.expect("MCP search dispatch");
        let envelope: Value = serde_json::from_str(&response).expect("JSON envelope");
        assert_eq!(envelope["summary"], json!({"total": 1, "succeeded": 1, "failed": 0, "aborted": 0}));
        assert_eq!(envelope["results"].as_array().expect("results").len(), 1);
        let entry = envelope["results"][0].clone();
        assert_eq!(entry["ok"], true);
        assert_eq!(entry["tool"], "search");
        assert_eq!(entry["status"], "complete");
        assert_eq!(entry["result"], raw, "complete canonical hit array must remain unchanged");
        assert!(entry.get("partial").is_none());
        assert!(entry.get("backend_errors").is_none());
        assert!(entry.get("error").is_none());
        (raw, entry["arm_participation"].clone())
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn single_backend_empty_no_match_and_zero_limit() {
        let server = in_memory_kg_server();
        let mut rows = Vec::new();
        let (empty, arms) = search_pair(&server, "unmatchedquartz", 10).await;
        assert_eq!(empty, json!([]));
        legacy_arms(&arms, "ran", 0, "skipped", 0);
        rows.push(("empty corpus".to_string(), arms, true));

        let runtime = server.runtime.as_ref().expect("single runtime");
        let token = runtime.authorize(Namespace::local()).expect("local namespace");
        let entity = runtime.create_entity(
            &token, "concept", None, "quartzfixture", Some("indexed local reference"), None, vec![],
        ).await.expect("seed indexed entity");
        for (label, query, limit, count) in [
            ("positive", "quartzfixture", 10, 1),
            ("no matching token", "unmatchedquartz", 10, 0),
            ("zero result limit", "quartzfixture", 0, 0),
        ] {
            let (raw, arms) = search_pair(&server, query, limit).await;
            assert_eq!(raw.as_array().expect("hit array").len(), count, "{label}");
            if count == 1 {
                assert_eq!(raw[0]["id"], entity.id.to_string());
                assert_eq!(raw[0]["source"], "text");
                assert_eq!(raw[0]["name"], "quartzfixture");
            } else {
                assert_eq!(raw, json!([]));
            }
            legacy_arms(&arms, "ran", count, "skipped", 0);
            rows.push((label.to_string(), arms, count == 0));
        }
        assert_eq!(rows.len(), 4);
        reasons("T2", rows);
    }

    #[test]
    #[serial_test::serial(config_ledger)]
    fn coordinator_status_and_error_routes() {
        // Typed coordinator inputs exercise classification and the shared
        // serializer, independently of the real retrieval fixture below.
        let mut vector_only = degraded_search_result([(
            "archive".to_string(), BackendSearchFailure::backend("storage unavailable"),
        )]);
        vector_only.partial = false;
        vector_only.per_backend[0].error = None;
        vector_only.per_backend[0].vector_selected = true;
        vector_only.per_backend[0].vector_error = Some("embedding unavailable".to_string());
        let vector_entry = ok_envelope("search".to_string(), OpSuccess {
            result: json!([]),
            degradation: SearchDegradation::from_result(&vector_only, &json!([])),
        });
        assert_eq!(vector_entry["ok"], true);
        assert_eq!(vector_entry["status"], "complete");
        assert_eq!(vector_entry["result"], json!([]));
        assert!(vector_entry.get("partial").is_none());
        assert!(vector_entry.get("backend_errors").is_none());
        legacy_arms(&vector_entry["arm_participation"], "ran", 0, "error", 0);

        let mut failed = degraded_search_result([(
            "archive".to_string(), BackendSearchFailure::backend("storage unavailable"),
        )]);
        failed.per_backend[0].vector_selected = true;
        let surviving = json!([{"id": "11111111-1111-1111-1111-111111111111", "source": "vector"}]);
        let partial = ok_envelope("search".to_string(), OpSuccess {
            result: surviving.clone(),
            degradation: SearchDegradation::from_result(&failed, &surviving),
        });
        assert_eq!(partial["ok"], true);
        assert_eq!(partial["result"], surviving);
        assert_eq!(partial["status"], "partial");
        assert_eq!(partial["partial"], true);
        assert_eq!(partial["missing_backends"], json!(["archive"]));
        assert_eq!(partial["backend_errors"], json!({"archive": {"kind": "backend_error", "message": "storage unavailable"}}));
        legacy_arms(&partial["arm_participation"], "error", 0, "error", 1);

        let diagnostic = search_diagnostic_value(&SearchDegradation::from_result(&failed, &json!([])));
        assert_eq!(diagnostic["kind"], "search_incomplete");
        assert_eq!(diagnostic["message"], "no-match was not established because selected backends failed");
        assert_eq!(diagnostic["retryable"], false);
        assert!(diagnostic.get("retry_after_ms").is_none());
        assert_eq!(diagnostic["missing_backends"], partial["missing_backends"]);
        assert_eq!(diagnostic["backend_errors"], partial["backend_errors"]);
        legacy_arms(&diagnostic["arm_participation"], "error", 0, "error", 0);
        let omitted = frame_budget_omission(
            &json!({"ok": false, "tool": "search", "error": diagnostic}),
            &frame_budget_category_test_registry(),
        );
        assert_eq!(omitted["ok"], false);
        assert_eq!(omitted["error"], diagnostic, "typed incomplete error is retained whole");
        reasons("T3", vec![
            ("whole-backend partial".to_string(), partial["arm_participation"].clone(), false),
            ("whole-backend incomplete".to_string(), diagnostic["arm_participation"].clone(), false),
            ("omitted incomplete".to_string(), omitted["error"]["arm_participation"].clone(), false),
            ("vector-only failure".to_string(), vector_entry["arm_participation"].clone(), true),
        ]);
    }

    #[test]
    #[serial_test::serial(config_ledger)]
    fn presentation_and_frame_omission() {
        let registry = frame_budget_category_test_registry();
        let mut rows = Vec::new();
        for mode in [PresentationMode::Agent, PresentationMode::Verbose, PresentationMode::Human] {
            let entry = present_ok_envelope_or_depth_error(
                "search".to_string(),
                OpSuccess { result: json!([]), degradation: SearchDegradation::complete(&json!([]), false) },
                mode,
                0,
                khive_types::VerbPresentationPolicy::Standard,
                khive_runtime::presentation::NoteContentScope::None,
            );
            assert_eq!(entry["ok"], true);
            assert_eq!(entry["status"], "complete");
            assert_eq!(entry["result"], json!([]));
            legacy_arms(&entry["arm_participation"], "ran", 0, "skipped", 0);
            let omitted = frame_budget_omission(&entry, &registry);
            assert_eq!(omitted["ok"], false);
            assert_eq!(omitted["executed"], true);
            assert_eq!(omitted["tool"], "search");
            assert!(omitted.get("result").is_none());
            assert!(omitted.get("status").is_none());
            assert!(omitted.get("arm_participation").is_none());
            assert_eq!(omitted["error"]["code"], "response_frame_budget_exceeded");
            assert_eq!(omitted["error"]["retryable"], false);
            assert_eq!(omitted["error"]["recoverable"], "read_outcome");
            assert_eq!(omitted["error"]["search"]["status"], "complete");
            assert_eq!(omitted["error"]["search"]["arm_participation"], entry["arm_participation"]);
            legacy_arms(&omitted["error"]["search"]["arm_participation"], "ran", 0, "skipped", 0);
            rows.push((format!("{mode:?} success"), entry["arm_participation"].clone(), true));
            rows.push((format!("{mode:?} omission"), omitted["error"]["search"]["arm_participation"].clone(), true));
        }
        assert_eq!(rows.len(), 6);
        reasons("T4", rows);
    }

    #[cfg(feature = "bench-embedder")]
    mod real_retrieval {
        use super::*;
        use khive_runtime::EmbedderProvider;
        use khive_storage::{TextFilter, TextQueryMode, TextSearchRequest};
        use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
        use std::collections::BTreeSet;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct LocalEmbeddingService { dimensions: usize, calls: Arc<AtomicUsize> }

        #[async_trait::async_trait]
        impl EmbeddingService for LocalEmbeddingService {
            async fn embed(&self, texts: &[String], _model: EmbeddingModel) -> Result<Vec<Vec<f32>>, EmbedError> {
                self.calls.fetch_add(texts.len(), Ordering::SeqCst);
                Ok(texts.iter().map(|text| {
                    let mut vector = vec![0.0; self.dimensions];
                    let (x, y) = if text.contains("companion") {
                        (0.8, 0.6)
                    } else if text.contains("archival reference") {
                        (0.6, 0.8)
                    } else {
                        (1.0, 0.0)
                    };
                    vector[0] = x;
                    vector[1] = y;
                    vector
                }).collect())
            }
            fn supports_model(&self, _model: EmbeddingModel) -> bool { true }
            fn name(&self) -> &'static str { "search-text-reason-local-fixture" }
        }

        struct LocalEmbeddingProvider { name: String, dimensions: usize, calls: Arc<AtomicUsize> }

        #[async_trait::async_trait]
        impl EmbedderProvider for LocalEmbeddingProvider {
            fn name(&self) -> &str { &self.name }
            fn dimensions(&self) -> usize { self.dimensions }
            async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
                Ok(Arc::new(LocalEmbeddingService { dimensions: self.dimensions, calls: Arc::clone(&self.calls) }))
            }
        }

        #[tokio::test]
        #[serial_test::serial(config_ledger)]
        async fn four_query_forms() {
            let model = EmbeddingModel::AllMiniLmL6V2;
            let calls = Arc::new(AtomicUsize::new(0));
            let runtime = KhiveRuntime::new(RuntimeConfig {
                db_path: None,
                default_namespace: Namespace::local(),
                embedding_model: Some(model),
                packs: vec!["kg".to_string()],
                ..RuntimeConfig::no_embeddings()
            }).expect("in-memory fixture runtime");
            // Runtime construction registers metadata and lazy providers only.
            // Replace the provider before server construction or any embed call.
            runtime.register_embedder(LocalEmbeddingProvider {
                name: model.to_string(), dimensions: model.dimensions(), calls: Arc::clone(&calls),
            });
            assert_eq!(runtime.embedder(&model.to_string()).await.expect("local provider").name(), "search-text-reason-local-fixture");
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            let server = KhiveMcpServer::new(runtime.clone()).expect("fixture MCP server");
            let token = runtime.authorize(Namespace::local()).expect("local namespace");
            let names = [
                "khive ADR-061 Pack-Extensible by-ID Resolution",
                "khive ADR-061 Pack-Extensible by-ID Resolution companion",
                "ADR-061 archival reference",
            ];
            let descriptions = [
                format!("{} ", names[0]).repeat(8),
                "catalogue background material ".repeat(90),
                "historical catalogue background material ".repeat(150),
            ];
            let mut ids = Vec::new();
            for (name, description) in names.iter().zip(&descriptions) {
                let entity = runtime.create_entity(
                    &token, "concept", None, name, Some(description), None, vec![],
                ).await.expect("entity and its real FTS/vector indexes");
                ids.push(entity.id);
            }
            assert_eq!(ids.len(), 3);
            assert_eq!(calls.load(Ordering::SeqCst), 3, "every record used the local embedder");
            let mut rows = Vec::new();
            for (query, text_count, sources) in [
                ("khive ADR-061", 2, ["both", "both", "vector"]),
                ("ADR-061", 3, ["both", "both", "both"]),
                ("Pack-Extensible by-ID Resolution", 2, ["both", "both", "vector"]),
                ("khive ADR-061 pack extensible by-ID resolution amendment private record merge diagnostics owning pack", 0, ["vector", "vector", "vector"]),
            ] {
                let lexical = runtime.text(&token).expect("real text index").search(TextSearchRequest {
                    query: query.to_string(), mode: TextQueryMode::Plain,
                    filter: Some(TextFilter { namespaces: vec!["local".to_string()], ..TextFilter::default() }),
                    top_k: 12, snippet_chars: 200,
                }).await.expect("real Plain FTS search");
                assert_eq!(lexical.iter().map(|hit| hit.subject_id).collect::<Vec<_>>(), ids[..text_count], "{query}: lexical IDs and order");
                let (raw, arms) = search_pair(&server, query, 3).await;
                let hits = raw.as_array().expect("canonical hits");
                assert_eq!(hits.len(), 3, "{query}");
                for (index, hit) in hits.iter().enumerate() {
                    assert_eq!(hit["id"], ids[index].to_string(), "{query}: ordered ID {index}");
                    assert_eq!(hit["name"], names[index]);
                    assert_eq!(hit["title"], names[index]);
                    assert_eq!(hit["kind"], "concept");
                    assert_eq!(hit["entity_kind"], "concept");
                    assert_eq!(hit["source"], sources[index], "{query}: source {index}");
                    assert!(hit["score"].as_f64().expect("numeric score") > 0.0);
                    assert!(hit["created_at"].is_string());
                    assert!(hit["snippet"].is_string());
                    let expected_keys = BTreeSet::from([
                        "created_at",
                        "entity_kind",
                        "id",
                        "kind",
                        "name",
                        "score",
                        "snippet",
                        "source",
                        "title",
                        "updated_at",
                        "version",
                    ]);
                    let actual_keys = hit
                        .as_object()
                        .expect("hit object")
                        .keys()
                        .map(String::as_str)
                        .collect::<BTreeSet<_>>();
                    let missing_keys = expected_keys
                        .difference(&actual_keys)
                        .copied()
                        .collect::<Vec<_>>();
                    let extra_keys = actual_keys
                        .difference(&expected_keys)
                        .copied()
                        .collect::<Vec<_>>();
                    assert_eq!(
                        actual_keys,
                        expected_keys,
                        "{query}: entity hit keys mismatch; missing: {missing_keys:?}; extra: {extra_keys:?}"
                    );
                    assert!(
                        hit["updated_at"].is_string() || hit["updated_at"].is_null(),
                        "{query}: updated_at must be a string or null"
                    );
                    assert!(
                        hit["version"].as_i64().is_some() || hit["version"].is_null(),
                        "{query}: version must be an integer or null"
                    );
                }
                legacy_arms(&arms, "ran", text_count, "ran", 3);
                eprintln!("T5 legacy query={query:?}, text={text_count}, vector=3, hits={raw}");
                rows.push((query.to_string(), arms, text_count == 0));
            }
            assert_eq!(rows.len(), 4);
            assert!(calls.load(Ordering::SeqCst) >= 11, "seed and both dispatch paths used the local embedder");
            reasons("T5", rows);
        }
    }
}
