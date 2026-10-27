mod search_text_mode_tests {
    use super::*;

    async fn search_entry(
        server: &KhiveMcpServer,
        kind: &str,
        mode: Option<&str>,
        query: &str,
    ) -> Value {
        let mut args = json!({"kind": kind, "query": query, "source": "text"});
        if let Some(mode) = mode {
            args["text_mode"] = json!(mode);
        }
        let response = server
            .dispatch_request_local(RequestParams {
                ops: json!([{"tool": "search", "args": args}]).to_string(),
                presentation: Some("verbose".to_string()),
                format: Some("json".to_string()),
                ..Default::default()
            })
            .await
            .expect("MCP search dispatch");
        let envelope: Value = serde_json::from_str(&response).expect("JSON envelope");
        envelope["results"][0].clone()
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn lexical_mode_changes_entity_and_note_recall_without_changing_default() {
        let server = in_memory_kg_server();
        let runtime = server.runtime.as_ref().expect("single runtime");
        let token = runtime
            .authorize(Namespace::local())
            .expect("local namespace");
        let entity = runtime
            .create_entity_with_embedding_report(
                &token,
                "concept",
                None,
                "solstice beacon",
                None,
                None,
                vec![],
            )
            .await
            .map(|(row, _report)| row)
            .expect("seed entity");
        let note = runtime
            .create_note(
                &token,
                "observation",
                Some("solstice beacon"),
                "solstice beacon",
                None,
                None,
                vec![],
            )
            .await
            .expect("seed note");

        for (kind, expected_id) in [("entity", entity.id), ("note", note.id)] {
            let default = search_entry(&server, kind, None, "solstice beacon harbor").await;
            let explicit_all =
                search_entry(&server, kind, Some("all_terms"), "solstice beacon harbor").await;
            for entry in [&default, &explicit_all] {
                assert_eq!(entry["ok"], true, "{kind}: {entry}");
                assert_eq!(entry["result"], json!([]));
                assert_eq!(entry["arm_participation"]["text"]["mode"], "all_terms");
                assert_eq!(entry["arm_participation"]["text"]["candidate_count"], 0);
                assert_eq!(
                    entry["arm_participation"]["text"]["reason"],
                    "No text candidate survived matching, filtering, fusion, and the result limit. Plain text search combines normalized term groups conjunctively; try fewer terms."
                );
            }

            let any_term =
                search_entry(&server, kind, Some("any_term"), "solstice beacon harbor").await;
            assert_eq!(any_term["ok"], true, "{kind}: {any_term}");
            assert_eq!(
                any_term["result"].as_array().expect("result array").len(),
                1
            );
            assert_eq!(any_term["result"][0]["id"], expected_id.to_string());
            assert_eq!(any_term["result"][0]["source"], "text");
            assert_eq!(any_term["arm_participation"]["text"]["mode"], "any_term");
            assert_eq!(any_term["arm_participation"]["text"]["candidate_count"], 1);
            assert!(any_term["arm_participation"]["text"]
                .get("reason")
                .is_none());

            let parallel = server
                .dispatch_request_local(RequestParams {
                    ops: json!([
                        {"tool": "search", "args": {
                            "kind": kind,
                            "query": "solstice beacon harbor",
                            "source": "text",
                            "text_mode": "any_term"
                        }},
                        {"tool": "stats", "args": {}}
                    ])
                    .to_string(),
                    presentation: Some("verbose".to_string()),
                    format: Some("json".to_string()),
                    ..Default::default()
                })
                .await
                .expect("parallel MCP search dispatch");
            let parallel: Value = serde_json::from_str(&parallel).expect("JSON envelope");
            let parallel_entry = &parallel["results"][0];
            assert_eq!(parallel_entry["ok"], true, "{kind}: {parallel_entry}");
            assert_eq!(parallel_entry["result"][0]["id"], expected_id.to_string());
            assert_eq!(
                parallel_entry["arm_participation"]["text"]["mode"],
                "any_term"
            );

            let empty_any = search_entry(
                &server,
                kind,
                Some("any_term"),
                "unmatchedquartz unmatchedcedar",
            )
            .await;
            assert_eq!(empty_any["result"], json!([]));
            assert_eq!(empty_any["arm_participation"]["text"]["mode"], "any_term");
            assert_eq!(
                empty_any["arm_participation"]["text"]["reason"],
                "No text candidate survived matching, filtering, fusion, and the result limit."
            );

            let invalid = search_entry(&server, kind, Some("unknown"), "solstice").await;
            assert_eq!(invalid["ok"], false, "{kind}: {invalid}");
            assert!(invalid["error"]
                .to_string()
                .contains("text_mode must be one of: all_terms, any_term"));
        }
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn registry_chain_reports_validated_any_term_mode() {
        let server = in_memory_kg_server();
        let runtime = server.runtime.as_ref().expect("single runtime");
        let token = runtime
            .authorize(Namespace::local())
            .expect("local namespace");
        let entity = runtime
            .create_entity_with_embedding_report(
                &token,
                "concept",
                None,
                "solstice beacon",
                None,
                None,
                vec![],
            )
            .await
            .map(|(row, _report)| row)
            .expect("seed entity");

        let chain = server
            .dispatch_request_local(RequestParams {
                ops: r#"search(kind="entity", query="solstice beacon harbor", source="text", text_mode="any_term") | stats()"#.to_string(),
                presentation: Some("verbose".to_string()),
                format: Some("json".to_string()),
                ..Default::default()
            })
            .await
            .expect("chain MCP search dispatch");
        let chain: Value = serde_json::from_str(&chain).expect("JSON chain envelope");
        assert_eq!(chain["summary"]["succeeded"], 2, "{chain}");
        let search = &chain["results"][0];
        assert_eq!(search["ok"], true, "{search}");
        assert_eq!(search["result"][0]["id"], entity.id.to_string());
        assert_eq!(search["arm_participation"]["text"]["mode"], "any_term");

        let invalid = server
            .dispatch_request_local(RequestParams {
                ops: r#"search(kind="entity", query="solstice", source="text", text_mode="unknown") | stats()"#.to_string(),
                ..Default::default()
            })
            .await
            .expect("invalid chain request envelope");
        let invalid: Value = serde_json::from_str(&invalid).expect("JSON invalid envelope");
        assert_eq!(invalid["results"][0]["ok"], false, "{invalid}");
        assert_eq!(invalid["summary"]["aborted"], 1, "{invalid}");
        assert!(invalid["results"][0]["error"]
            .to_string()
            .contains("text_mode must be one of: all_terms, any_term"));
    }
}

mod note_status_disclosure_tests {
    use super::*;
    use khive_storage::note::NoteStore;
    use std::time::Duration;

    const STATUS_QUERY: &str = "statusdisclosurefixture";

    async fn bounded_status(future: impl Future<Output = ()>) {
        tokio::time::timeout(Duration::from_secs(30), future)
            .await
            .expect("note disclosure fixture exceeded its cooperative async budget");
    }

    async fn status_request(
        server: &KhiveMcpServer,
        ops: String,
        presentation: &str,
        format: &str,
    ) -> Value {
        let response = server
            .dispatch_request_local(RequestParams {
                ops,
                presentation: Some(presentation.into()),
                format: Some(format.into()),
                ..Default::default()
            })
            .await
            .expect("MCP request envelope");
        serde_json::from_str(&response).expect("structured MCP response")
    }

    async fn seed_status_notes(runtime: &KhiveRuntime) {
        let token = runtime
            .authorize(Namespace::local())
            .expect("local seed token");
        for status in ["active", "archived", "custom"] {
            let mut note = runtime
                .create_note(
                    &token,
                    "observation",
                    None,
                    STATUS_QUERY,
                    None,
                    None,
                    vec![],
                )
                .await
                .expect("create indexed fixture note");
            note.status = status.into();
            runtime
                .backend()
                .upsert_note(note)
                .await
                .expect("fixture lifecycle update");
        }
    }

    fn disclosure(entry: &Value, selector: &str, count: u64) {
        assert_eq!(entry["ok"], true, "{entry}");
        assert_eq!(entry["note_status"], selector);
        assert_eq!(entry["note_status_dropped"].as_u64(), Some(count));
    }

    fn absent(entry: &Value) {
        assert!(entry.get("note_status").is_none(), "{entry}");
        assert!(entry.get("note_status_dropped").is_none(), "{entry}");
        assert!(entry.pointer("/error/search/note_status").is_none());
        assert!(entry.pointer("/error/search/note_status_dropped").is_none());
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn registry_note_disclosure_preserves_raw_arrays_on_single_parallel_and_chain() {
        bounded_status(async {
            let server = in_memory_kg_server();
            seed_status_notes(server.runtime.as_ref().unwrap()).await;
            for (selector, count) in [("active", 2), ("archived", 2), ("any", 0)] {
                for query in [STATUS_QUERY, "missingstatusfixture"] {
                    let args = json!({"kind":"note", "query":query, "source":"text", "note_status":selector});
                    let raw = server.registry.dispatch("search", args.clone()).await.unwrap();
                    assert!(raw.is_array());
                    let raw_bytes = serde_json::to_vec(&raw).unwrap();
                    for ops in [
                        json!([{"tool":"search","args":args.clone()}]).to_string(),
                        json!([{"tool":"search","args":args.clone()}, {"tool":"stats","args":{}}]).to_string(),
                        format!("search(kind=\"note\", query=\"{query}\", source=\"text\", note_status=\"{selector}\") | stats()"),
                    ] {
                        let response = status_request(&server, ops, "verbose", "json").await;
                        let entry = &response["results"][0];
                        disclosure(entry, selector, if query == STATUS_QUERY { count } else { 0 });
                        assert_eq!(serde_json::to_vec(&entry["result"]).unwrap(), raw_bytes,
                            "raw result bytes must remain identical on this route");
                        for sibling in response["results"].as_array().unwrap().iter().skip(1) { absent(sibling); }
                        assert!(response.get("note_status").is_none());
                        assert!(response.get("note_status_dropped").is_none());
                    }
                }
            }
            let omitted = status_request(&server,
                json!([{"tool":"search","args":{"kind":"note","query":STATUS_QUERY,"source":"text"}}]).to_string(),
                "verbose", "json").await;
            disclosure(&omitted["results"][0], "active", 2);
            let null = status_request(&server,
                json!([{"tool":"search","args":{"kind":"note","query":STATUS_QUERY,"source":"text","note_status":null}}]).to_string(),
                "verbose", "json").await;
            disclosure(&null["results"][0], "active", 2);
        }).await;
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn registry_mixed_absence_and_prev_remain_result_only() {
        bounded_status(async {
            let server = in_memory_kg_server();
            seed_status_notes(server.runtime.as_ref().unwrap()).await;
            let response = status_request(&server, json!([
                {"tool":"search","args":{"kind":"note","query":STATUS_QUERY}},
                {"tool":"search","args":{"kind":"entity","query":STATUS_QUERY}},
                {"tool":"search","args":{"help":true}},
                {"tool":"search","args":{"kind":"note","query":STATUS_QUERY,"note_status":"invalid"}},
                {"tool":"stats","args":{}}
            ]).to_string(), "verbose", "json").await;
            disclosure(&response["results"][0], "active", 2);
            for index in 1..5 { absent(&response["results"][index]); }
            assert_eq!(response["results"][3]["ok"], false);
            let chain = status_request(&server,
                format!("search(kind=\"note\", query=\"{STATUS_QUERY}\", source=\"text\") | get(id=$prev.0.id)"),
                "verbose", "json").await;
            disclosure(&chain["results"][0], "active", 2);
            assert_eq!(chain["results"][1]["ok"], true, "array-indexed $prev must remain usable: {chain}");
            absent(&chain["results"][1]);
        }).await;
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn all_nine_presentations_keep_typed_siblings_and_format_only_hits() {
        bounded_status(async {
            let server = in_memory_kg_server();
            seed_status_notes(server.runtime.as_ref().unwrap()).await;
            for (selector, query, count) in [("active", STATUS_QUERY, 2), ("any", STATUS_QUERY, 0), ("active", "missingstatusfixture", 0)] {
                let ops = json!([{"tool":"search","args":{"kind":"note","query":query,"source":"text","note_status":selector}}]).to_string();
                for (name, presentation) in [("agent", PresentationMode::Agent), ("human", PresentationMode::Human), ("verbose", PresentationMode::Verbose)] {
                    let json_response = status_request(&server, ops.clone(), name, "json").await;
                    let json_entry = &json_response["results"][0];
                    disclosure(json_entry, selector, count);
                    assert!(json_entry["result"].is_array());
                    for (format_name, format) in [("json", OutputFormat::Json), ("auto", OutputFormat::Auto), ("table", OutputFormat::Table)] {
                        let response = status_request(&server, ops.clone(), name, format_name).await;
                        let entry = &response["results"][0];
                        disclosure(entry, selector, count);
                        let expected_result = if format == OutputFormat::Json {
                            json_entry["result"].clone()
                        } else {
                            Value::String(khive_runtime::presentation::render_format_with_note_content(
                                json_entry["result"].clone(), format, presentation, NoteContentScope::None,
                            ))
                        };
                        assert_eq!(entry["result"], expected_result, "{name}/{format_name}");
                        let mut expected_entry = json_entry.clone();
                        expected_entry["result"] = expected_result;
                        let mut actual_entry = entry.clone();
                        // Execution usage is measured afresh per request; it is not presentation data.
                        expected_entry.as_object_mut().unwrap().remove("usage");
                        actual_entry.as_object_mut().unwrap().remove("usage");
                        assert_eq!(actual_entry, expected_entry, "only result rendering may change");
                        if let Some(rendered) = entry["result"].as_str() {
                            assert!(!rendered.contains("note_status"));
                            assert!(!rendered.contains("note_status_dropped"));
                        }
                    }
                }
            }
        }).await;
    }

    fn registry_scope_fixture() -> (VerbRegistry, KhiveRuntime) {
        let runtime = KhiveRuntime::memory().unwrap();
        let mut builder = VerbRegistryBuilder::new();
        builder.with_gate(Arc::new(khive_runtime::AllowAllGate));
        builder.with_default_namespace("status-gate-only");
        builder.with_actor_id(Some("lambda:status-reader".into()));
        builder.with_runtime_event_store(&runtime).unwrap();
        khive_runtime::PackRegistry::register_packs(&["kg".into()], runtime.clone(), &mut builder)
            .unwrap();
        let registry = builder.build().unwrap();
        runtime.install_edge_rules(registry.all_edge_rules());
        (registry, runtime)
    }

    async fn actual_projection(
        explicit: bool,
    ) -> (String, String, std::collections::HashSet<String>) {
        let (registry, _runtime) = registry_scope_fixture();
        let mut args = json!({"kind":"note", "query":STATUS_QUERY});
        if explicit {
            args["namespace"] = json!("status-gate-only");
        }
        registry
            .dispatch_search_with_registry_scope(&args, None, |_owner, token| async move {
                let visible: std::collections::HashSet<String> = token
                    .visible_namespaces()
                    .iter()
                    .map(|namespace| namespace.as_str().to_owned())
                    .collect();
                Ok(khive_runtime::InterceptedDispatchResult::new(
                    json!([]),
                    (
                        token.namespace().as_str().to_owned(),
                        token.gate_namespace().as_str().to_owned(),
                        visible,
                    ),
                ))
            })
            .await
            .expect("actual registered KG owner")
            .expect("single authorized intercept")
            .metadata
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn registry_default_gate_namespace_is_not_an_implicit_read_source() {
        bounded_status(async {
            let (primary, gate, visible) = actual_projection(false).await;
            assert!(
                !visible.contains("status-gate-only"),
                "default gate namespace must not become an implicit read source"
            );
            assert_eq!(primary, "local");
            assert_eq!(gate, "status-gate-only");
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn registry_actor_binding_namespace_is_visible_without_declaration() {
        bounded_status(async {
            let (primary, gate, visible) = actual_projection(false).await;
            assert!(
                visible.contains("lambda:status-reader"),
                "actor binding namespace must remain visible even when undeclared"
            );
            assert!(visible.contains("local"));
            assert_eq!(primary, "local");
            assert_eq!(gate, "status-gate-only");
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn registry_explicit_namespace_keeps_the_narrow_projection() {
        bounded_status(async {
            let (primary, gate, visible) = actual_projection(true).await;
            assert_eq!(
                visible,
                std::collections::HashSet::from(["status-gate-only".to_string()])
            );
            assert_eq!(primary, "status-gate-only");
            assert_eq!(gate, "status-gate-only");
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn registry_only_server_keeps_note_metadata_without_host_runtime() {
        bounded_status(async {
            let (registry, runtime) = registry_scope_fixture();
            seed_status_notes(&runtime).await;
            let server = KhiveMcpServer::from_registry_with_meta(registry, "local", "status-fixture");
            assert!(server.runtime.is_none(), "must exercise actual registered owner access");
            let response = status_request(&server,
                json!([{"tool":"search","args":{"kind":"note","query":STATUS_QUERY,"source":"text"}}]).to_string(),
                "verbose", "json").await;
            disclosure(&response["results"][0], "active", 2);
            assert_eq!(response["results"][0]["result"].as_array().unwrap().len(), 1);
        }).await;
    }

    #[test]
    #[serial_test::serial(config_ledger)]
    fn disclosure_is_absent_after_frame_and_both_depth_refusals() {
        let registry = frame_budget_category_test_registry();
        let make_success = |result| OpSuccess {
            degradation: SearchDegradation::complete(&json!([]), false),
            result,
            note_status_disclosure: Some((khive_storage::note::NoteStatusSelector::Active, 7)),
        };
        let success = ok_envelope("search".into(), make_success(json!([])));
        disclosure(&success, "active", 7);
        let omitted = frame_budget_omission(&success, &registry);
        assert_eq!(omitted["ok"], false);
        absent(&omitted);
        let deep = nest_object(khive_request::NESTING_DEPTH_LIMIT + 8, json!(true));
        let chain = chain_ok_envelope_or_depth_error("search".into(), make_success(deep))
            .expect_err("depth guard must reject")
            .into_entry();
        assert_eq!(chain["ok"], false);
        absent(&chain);
        let deep = nest_object(khive_request::NESTING_DEPTH_LIMIT + 8, json!(true));
        let ordinary = present_ok_envelope_or_depth_error(
            "search".into(),
            make_success(deep),
            PresentationMode::Verbose,
            0,
            khive_types::VerbPresentationPolicy::Standard,
            NoteContentScope::None,
        );
        assert_eq!(ordinary["ok"], false);
        absent(&ordinary);
    }

    struct BrokenDisclosurePack {
        disclosure: Option<(khive_storage::note::NoteStatusSelector, u64)>,
    }

    impl khive_types::Pack for BrokenDisclosurePack {
        const NAME: &'static str = "broken-search-disclosure";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [khive_runtime::HandlerDef] =
            <FrameBudgetCategoryTestPack as khive_types::Pack>::HANDLERS;
    }

    #[async_trait::async_trait]
    impl khive_runtime::PackRuntime for BrokenDisclosurePack {
        fn name(&self) -> &str {
            "broken-search-disclosure"
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            &[]
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            &[]
        }
        fn handlers(&self) -> &'static [khive_runtime::HandlerDef] {
            <FrameBudgetCategoryTestPack as khive_types::Pack>::HANDLERS
        }
        async fn dispatch(
            &self,
            _verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &khive_runtime::NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Ok(json!([]))
        }
        async fn dispatch_search_intercepted(
            &self,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &khive_runtime::NamespaceToken,
        ) -> Result<
            khive_runtime::InterceptedDispatchResult<
                Option<(khive_storage::note::NoteStatusSelector, u64)>,
            >,
            RuntimeError,
        > {
            Ok(khive_runtime::InterceptedDispatchResult::new(
                json!([]),
                self.disclosure,
            ))
        }
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn custom_note_owners_fail_closed_without_matching_disclosure_but_entities_dispatch() {
        bounded_status(async {
            for disclosure in [
                None,
                Some((khive_storage::note::NoteStatusSelector::Archived, 0)),
            ] {
                let mut builder = VerbRegistryBuilder::new();
                builder.with_gate(Arc::new(khive_runtime::AllowAllGate));
                builder.register(BrokenDisclosurePack { disclosure });
                let server = KhiveMcpServer::from_registry_with_meta(
                    builder.build().unwrap(),
                    "local",
                    "status-fixture",
                );
                let response = status_request(
                    &server,
                    json!([
                        {"tool":"search","args":{"kind":"note","query":"x"}},
                        {"tool":"search","args":{"kind":"entity","query":"x"}}
                    ])
                    .to_string(),
                    "verbose",
                    "json",
                )
                .await;
                let failed = &response["results"][0];
                assert_eq!(failed["ok"], false);
                absent(failed);
                assert!(failed["error"]
                    .to_string()
                    .contains("omitted or mismatched status disclosure"));
                let entity = &response["results"][1];
                assert_eq!(entity["ok"], true);
                assert_eq!(entity["result"], json!([]));
                absent(entity);
            }
            let mut builder = VerbRegistryBuilder::new();
            builder.with_gate(Arc::new(khive_runtime::AllowAllGate));
            builder.register(FrameBudgetCategoryTestPack); // Uses actual default unavailable hook.
            let server = KhiveMcpServer::from_registry_with_meta(
                builder.build().unwrap(),
                "local",
                "status-fixture",
            );
            let response = status_request(
                &server,
                json!([{"tool":"search","args":{"kind":"note","query":"x"}}]).to_string(),
                "verbose",
                "json",
            )
            .await;
            assert_eq!(response["results"][0]["ok"], false);
            absent(&response["results"][0]);
            assert!(response["results"][0]["error"]
                .to_string()
                .contains("does not support intercepted search"));
        })
        .await;
    }
}
