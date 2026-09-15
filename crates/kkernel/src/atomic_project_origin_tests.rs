mod atomic_project_origin_witnesses {
    use super::*;

    async fn fixture(kind: &str) -> (tempfile::TempDir, RuntimeConfig, Uuid, Uuid) {
        let directory = tempfile::tempdir().expect("atomic provenance directory");
        let config = RuntimeConfig {
            db_path: Some(directory.path().join("origin.db")),
            packs: vec!["kg".into()],
            brain_profile: None,
            ..RuntimeConfig::no_embeddings()
        };
        let runtime = KhiveRuntime::new(config.clone()).expect("file runtime");
        let token = runtime
            .authorize(Namespace::local())
            .expect("authorize local fixture");
        let source = runtime
            .create_entity(
                &token,
                kind,
                None,
                "Atomic origin source",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let document = runtime
            .create_entity(
                &token,
                "document",
                None,
                "Atomic origin document",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            runtime.get_entity(&token, source.id).await.unwrap().kind,
            kind
        );
        assert_eq!(
            runtime.get_entity(&token, document.id).await.unwrap().kind,
            "document"
        );
        drop(runtime);
        (directory, config, source.id, document.id)
    }

    fn link(source: Uuid, target: Uuid) -> OpsFileEntry {
        OpsFileEntry {
            tool: "link".into(),
            args: json!({
                "source_id": source.to_string(), "target_id": target.to_string(),
                "relation": "introduced_by", "weight": 0.75,
            }),
        }
    }

    #[tokio::test]
    async fn atomic_ops_file_commits_project_origin_with_exact_receipt() {
        for kind in ["service", "project"] {
            let (_directory, config, source, document) = fixture(kind).await;
            println!("ORIGIN2579 atomic executor commit testing {kind}");
            let value = execute_atomic_ops_file(
                vec![link(source, document)],
                config.clone(),
                &KhiveConfig::default(),
                10,
            )
            .await
            .expect("ORIGIN2579 atomic executor admits project origin");
            assert_eq!(value["atomic"]["committed"], true, "{value}");
            assert_eq!(value["summary"]["succeeded"], 1, "{value}");
            assert_eq!(value["summary"]["failed"], 0, "{value}");
            let receipt = &value["results"][0]["result"];
            assert_eq!(receipt["source_id"], source.to_string());
            assert_eq!(receipt["target_id"], document.to_string());
            assert_eq!(receipt["relation"], "introduced_by");
            assert_eq!(receipt["weight"], 0.75);
            let runtime = KhiveRuntime::new(config).expect("read committed runtime");
            let rows = runtime
                .list_edges(
                    &runtime
                        .authorize(Namespace::local())
                        .expect("authorize local fixture"),
                    EdgeListFilter::default(),
                    100,
                    0,
                )
                .await
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].id.0.to_string(), receipt["id"].as_str().unwrap());
            assert_eq!(rows[0].source_id, source);
            assert_eq!(rows[0].target_id, document);
            assert_eq!(rows[0].relation, EdgeRelation::IntroducedBy);
            assert_eq!(rows[0].weight, 0.75);
            assert!(rows[0].deleted_at.is_none());
            println!("ORIGIN2579 atomic executor commit {kind} control complete");
        }
    }

    #[tokio::test]
    async fn atomic_ops_file_target_deletion_rolls_back_before_project_link() {
        for kind in ["service", "project"] {
            let (_directory, config, source, document) = fixture(kind).await;
            let operations = vec![
                OpsFileEntry {
                    tool: "delete".into(),
                    args: json!({"id": document.to_string(), "kind": "entity"}),
                },
                link(source, document),
            ];
            println!("ORIGIN2579 atomic executor rollback testing {kind}");
            let value =
                execute_atomic_ops_file(operations, config.clone(), &KhiveConfig::default(), 10)
                    .await
                    .expect("ORIGIN2579 project origin prepares then guards the target at commit");
            assert_eq!(value["atomic"]["rolled_back"], true, "{value}");
            assert_eq!(value["atomic"]["failed_op_index"], 1, "{value}");
            assert!(
                value["atomic"]["error"]
                    .as_str()
                    .unwrap()
                    .contains("guard failed"),
                "{value}"
            );
            assert_eq!(value["summary"]["succeeded"], 0);
            let runtime = KhiveRuntime::new(config).expect("read rolled-back runtime");
            assert_eq!(
                runtime
                    .get_entity(
                        &runtime
                            .authorize(Namespace::local())
                            .expect("authorize local fixture"),
                        document
                    )
                    .await
                    .unwrap()
                    .kind,
                "document",
                "earlier deletion must roll back"
            );
            assert!(
                runtime
                    .list_edges(
                        &runtime
                            .authorize(Namespace::local())
                            .expect("authorize local fixture"),
                        EdgeListFilter::default(),
                        100,
                        0
                    )
                    .await
                    .unwrap()
                    .is_empty(),
                "no edge or inverse remains"
            );
            println!("ORIGIN2579 atomic executor rollback {kind} control complete");
        }
    }
}
