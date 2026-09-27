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
            .create_entity(
                &token,
                "concept",
                None,
                "solstice beacon",
                None,
                None,
                vec![],
            )
            .await
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
            .create_entity(
                &token,
                "concept",
                None,
                "solstice beacon",
                None,
                None,
                vec![],
            )
            .await
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
