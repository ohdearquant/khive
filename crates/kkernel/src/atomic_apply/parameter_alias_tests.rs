mod parameter_alias_tests {
    use super::*;

    struct Fixture {
        _directory: tempfile::TempDir,
        config: RuntimeConfig,
        runtime: KhiveRuntime,
        source: Uuid,
        target: Uuid,
        tasks: [Uuid; 2],
    }

    impl Fixture {
        async fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let config = RuntimeConfig {
                db_path: Some(directory.path().join("aliases.db")),
                packs: vec!["kg".into(), "gtd".into()],
                actor_id: None,
                brain_profile: None,
                ..RuntimeConfig::no_embeddings()
            };
            let runtime = KhiveRuntime::new(config.clone()).unwrap();
            let mut builder = VerbRegistryBuilder::new();
            builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
            builder.register(khive_pack_gtd::GtdPack::new(runtime.clone()));
            let registry = builder.build().unwrap();
            registry
                .apply_schema_plans_with_map(&Default::default(), runtime.backend())
                .unwrap();
            let token = runtime.authorize(Namespace::local()).unwrap();
            let mut entities = Vec::new();
            for name in ["Alias source", "Alias target"] {
                entities.push(
                    runtime
                        .create_entity_with_embedding_report(
                            &token,
                            "concept",
                            None,
                            name,
                            None,
                            None,
                            vec![],
                        )
                        .await
                        .unwrap()
                        .0
                        .id,
                );
            }
            let tasks = [
                seed_task(&runtime, &token, "next").await,
                seed_task(&runtime, &token, "next").await,
            ];
            Self {
                _directory: directory,
                config,
                runtime,
                source: entities[0],
                target: entities[1],
                tasks,
            }
        }

        async fn execute(&self, ops: Vec<OpsFileEntry>) -> anyhow::Result<Value> {
            execute_atomic_ops_file(ops, self.config.clone(), &KhiveConfig::default(), 10).await
        }

        fn link(&self) -> Value {
            json!({"source_id": self.source, "target_id": self.target,
                "relation": "contains", "weight": 0.75, "metadata": {"proof": "alias"}})
        }

        async fn task(&self, index: usize) -> khive_storage::Note {
            let token = self.runtime.authorize(Namespace::local()).unwrap();
            self.runtime
                .notes(&token)
                .unwrap()
                .get_note(self.tasks[index])
                .await
                .unwrap()
                .unwrap()
        }

        async fn snapshot(&self) -> Value {
            let mut snapshot = serde_json::Map::new();
            let mut reader = self.runtime.sql().reader().await.unwrap();
            for table in [
                "entities",
                "notes",
                "graph_edges",
                "events",
                "gtd_lifecycle_audit",
            ] {
                let rows = reader
                    .query_all(khive_storage::SqlStatement {
                        sql: format!("SELECT * FROM {table} ORDER BY rowid"),
                        params: vec![],
                        label: Some("parameter_alias_snapshot".into()),
                    })
                    .await
                    .unwrap();
                snapshot.insert(table.into(), serde_json::to_value(rows).unwrap());
            }
            Value::Object(snapshot)
        }
    }

    fn op(tool: &str, args: Value) -> OpsFileEntry {
        OpsFileEntry {
            tool: tool.into(),
            args,
        }
    }

    fn alias(mut args: Value, canonical: &str, spelling: &str) -> Value {
        let map = args.as_object_mut().unwrap();
        let value = map.remove(canonical).unwrap();
        map.insert(spelling.into(), value);
        args
    }

    fn committed_result(envelope: &Value) -> &Value {
        assert_eq!(envelope["atomic"]["committed"], true, "{envelope}");
        assert_eq!(envelope["summary"]["succeeded"], 1, "{envelope}");
        assert_eq!(envelope["summary"]["failed"], 0, "{envelope}");
        assert!(
            envelope["atomic"].get("degradations").is_none(),
            "{envelope}"
        );
        let entry = &envelope["results"][0];
        assert_eq!(entry["ok"], true, "{envelope}");
        assert!(entry.get("error").is_none(), "{envelope}");
        entry
            .get("result")
            .expect("normal result, not committed_degraded")
    }

    #[tokio::test]
    async fn atomic_link_aliases_persist_the_canonical_edge_after_resolution() {
        if crate::test_process::run_in_child() {
            return;
        }
        let fixture = Fixture::new().await;
        let canonical = fixture
            .execute(vec![op("link", fixture.link())])
            .await
            .unwrap();
        let expected = committed_result(&canonical);
        let mut variants = Vec::new();
        for (canonical, spelling) in [
            ("source_id", "source"),
            ("target_id", "target"),
            ("relation", "kind"),
        ] {
            variants.push(alias(fixture.link(), canonical, spelling));
        }
        variants.push(
            json!({"source": "Alias source", "target": &fixture.target.simple().to_string()[..12],
            "kind": "contains", "weight": 0.75, "metadata": {"proof": "alias"}}),
        );
        for args in variants {
            let envelope = fixture.execute(vec![op("link", args)]).await.unwrap();
            let actual = committed_result(&envelope);
            for field in [
                "id",
                "source_id",
                "target_id",
                "relation",
                "weight",
                "metadata",
            ] {
                assert_eq!(actual[field], expected[field], "{field}: {envelope}");
            }
        }
        let token = fixture.runtime.authorize(Namespace::local()).unwrap();
        let edges = fixture
            .runtime
            .list_edges(&token, EdgeListFilter::default(), 10, 0)
            .await
            .unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].source_id, fixture.source);
        assert_eq!(edges[0].target_id, fixture.target);
        assert_eq!(edges[0].relation, EdgeRelation::Contains);
        assert_eq!(edges[0].weight, 0.75);
        assert_eq!(edges[0].metadata, Some(json!({"proof": "alias"})));
    }

    #[tokio::test]
    async fn atomic_task_aliases_persist_transition_and_completion_results() {
        if crate::test_process::run_in_child() {
            return;
        }
        let fixture = Fixture::new().await;
        for (index, status_key, result_key) in [(0, "status", "result"), (1, "to", "note")] {
            let mut args = json!({"id": fixture.tasks[index], "note": "starting work"});
            args[status_key] = json!("in_progress");
            let envelope = fixture
                .execute(vec![op("gtd.transition", args)])
                .await
                .unwrap();
            let receipt = committed_result(&envelope);
            assert_eq!(receipt["transitioned"], true);
            assert_eq!(receipt["from"], "next");
            assert_eq!(receipt["to"], "active");
            assert_eq!(receipt["audit_persisted"], true);
            assert_eq!(
                fixture.task(index).await.properties.unwrap()["status"],
                "active"
            );

            let mut args = json!({"id": fixture.tasks[index]});
            args[result_key] = json!("shipped clean");
            let envelope = fixture
                .execute(vec![op("gtd.complete", args)])
                .await
                .unwrap();
            let receipt = committed_result(&envelope);
            assert_eq!(receipt["completed"], true);
            assert_eq!(receipt["from"], "active");
            assert_eq!(receipt["to"], "done");
            assert_eq!(receipt["audit_persisted"], true);
            let task = fixture.task(index).await;
            let properties = task.properties.unwrap();
            assert_eq!(properties["status"], "done");
            assert_eq!(properties["result"], "shipped clean");
            assert_eq!(properties["completed_at"], receipt["completed_at"]);
        }
    }

    #[tokio::test]
    async fn atomic_transition_alias_noop_renders_without_degradation_or_writes() {
        if crate::test_process::run_in_child() {
            return;
        }
        let fixture = Fixture::new().await;
        let token = fixture.runtime.authorize(Namespace::local()).unwrap();
        let task = seed_task(&fixture.runtime, &token, "inbox").await;
        let before = fixture.snapshot().await;
        let canonical = fixture
            .execute(vec![op(
                "gtd.transition",
                json!({
                    "id": task, "status": "inbox"
                }),
            )])
            .await
            .unwrap();
        let aliased = fixture
            .execute(vec![op(
                "gtd.transition",
                json!({
                    "id": task, "to": "todo"
                }),
            )])
            .await
            .unwrap();
        let canonical = committed_result(&canonical);
        let aliased = committed_result(&aliased);
        assert_eq!(aliased, canonical);
        assert_eq!(aliased["transitioned"], false);
        assert_eq!(aliased["note_recorded"], false);
        assert_eq!(aliased["from"], "inbox");
        assert_eq!(aliased["to"], "inbox");
        assert_eq!(aliased["reason"], "already in target status");
        assert_eq!(fixture.snapshot().await, before);
    }

    #[tokio::test]
    async fn atomic_alias_conflicts_including_null_prevent_the_entire_unit() {
        if crate::test_process::run_in_child() {
            return;
        }
        let fixture = Fixture::new().await;
        let before = fixture.snapshot().await;
        let cases = [
            ("link", fixture.link(), "source_id", "source"),
            ("link", fixture.link(), "target_id", "target"),
            ("link", fixture.link(), "relation", "kind"),
            (
                "gtd.transition",
                json!({"id": fixture.tasks[0], "status": "active"}),
                "status",
                "to",
            ),
            (
                "gtd.complete",
                json!({"id": fixture.tasks[0], "result": "done"}),
                "result",
                "note",
            ),
        ];
        for (tool, args, canonical, spelling) in cases {
            for (canonical_value, alias_value) in [
                (args[canonical].clone(), args[canonical].clone()),
                (args[canonical].clone(), json!("different")),
                (Value::Null, args[canonical].clone()),
                (args[canonical].clone(), Value::Null),
                (Value::Null, Value::Null),
            ] {
                let mut args = args.clone();
                args[canonical] = canonical_value;
                args[spelling] = alias_value;
                let error = fixture
                    .execute(vec![
                        op(
                            "update",
                            json!({"id": fixture.source, "name": "must not commit"}),
                        ),
                        op(tool, args),
                    ])
                    .await
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains(&format!("`{spelling}` is an alias for `{canonical}`")),
                    "{error}"
                );
                assert!(error.contains("supply only one"), "{error}");
                assert_eq!(fixture.snapshot().await, before);
            }
        }
    }

    #[tokio::test]
    async fn atomic_aliases_preserve_unknown_field_lists_and_bulk_entry_strictness() {
        if crate::test_process::run_in_child() {
            return;
        }
        let fixture = Fixture::new().await;
        let before = fixture.snapshot().await;
        for (tool, mut args, canonical, spelling) in [
            ("link", fixture.link(), "source_id", "source"),
            ("link", fixture.link(), "target_id", "target"),
            ("link", fixture.link(), "relation", "kind"),
            (
                "gtd.transition",
                json!({"id": fixture.tasks[0], "status": "active"}),
                "status",
                "to",
            ),
            (
                "gtd.complete",
                json!({"id": fixture.tasks[0], "result": "done"}),
                "result",
                "note",
            ),
        ] {
            args["misspelled"] = json!(true);
            let canonical_error = fixture
                .execute(vec![op(tool, args.clone())])
                .await
                .unwrap_err()
                .to_string();
            let alias_error = fixture
                .execute(vec![op(tool, alias(args, canonical, spelling))])
                .await
                .unwrap_err()
                .to_string();
            assert_eq!(alias_error, canonical_error);
            assert!(
                alias_error.contains("unknown field `misspelled`"),
                "{alias_error}"
            );
            assert!(
                alias_error.contains(&format!("`{canonical}`")),
                "{alias_error}"
            );
        }
        for (canonical, spelling) in [
            ("source_id", "source"),
            ("target_id", "target"),
            ("relation", "kind"),
        ] {
            let entry = alias(fixture.link(), canonical, spelling);
            let error = fixture
                .execute(vec![op("link", json!({"links": [entry]}))])
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(&format!("unknown field `{spelling}`")),
                "{error}"
            );
            assert!(error.contains(&format!("`{canonical}`")), "{error}");
        }
        assert_eq!(fixture.snapshot().await, before);
    }
}
