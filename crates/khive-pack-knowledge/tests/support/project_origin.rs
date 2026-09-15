mod project_origin_witnesses {
    use super::*;
    use khive_runtime::{EdgeListFilter, Namespace, Resolved};
    use khive_storage::EdgeRelation;
    use uuid::Uuid;

    fn fixture() -> (KhiveRuntime, Fixture) {
        let runtime = rt();
        let fixture = pack(runtime.clone());
        (runtime, fixture)
    }

    async fn entity(fixture: &Fixture, kind: &str, name: &str) -> Uuid {
        let value = fixture
            .dispatch(
                "create",
                json!({
                    "kind": kind, "name": name, "skip_dedup_check": true
                }),
            )
            .await
            .expect("registered entity creation control");
        assert_eq!(value["kind"], kind, "{value}");
        let id = Uuid::parse_str(value["id"].as_str().expect("entity UUID")).unwrap();
        let read = fixture
            .dispatch("get", json!({"id": id.to_string()}))
            .await
            .expect("registered entity read control");
        assert_eq!(read["id"], id.to_string());
        assert_eq!(read["kind"], kind);
        id
    }

    fn origin(source: Uuid, target: Uuid) -> Value {
        json!({"source_id": source.to_string(), "target_id": target.to_string(),
            "relation": "introduced_by", "weight": 0.75})
    }

    async fn edges(runtime: &KhiveRuntime) -> Vec<khive_storage::Edge> {
        runtime
            .list_edges(
                &runtime
                    .authorize(Namespace::local())
                    .expect("authorize local fixture"),
                EdgeListFilter::default(),
                100,
                0,
            )
            .await
            .expect("list persisted edges")
    }

    async fn exact_edge(runtime: &KhiveRuntime, receipt: &Value, source: Uuid, target: Uuid) {
        let id = receipt
            .get("full_id")
            .or_else(|| receipt.get("id"))
            .and_then(Value::as_str)
            .expect("edge receipt id");
        let id = Uuid::parse_str(id).expect("full edge UUID");
        let edge = runtime
            .get_edge(
                &runtime
                    .authorize(Namespace::local())
                    .expect("authorize local fixture"),
                id,
            )
            .await
            .unwrap()
            .expect("receipt identifies a persisted live edge");
        assert_eq!(edge.id.0, id);
        assert_eq!(edge.source_id, source);
        assert_eq!(edge.target_id, target);
        assert_eq!(edge.relation, EdgeRelation::IntroducedBy);
        assert_eq!(edge.weight, 0.75);
        assert_eq!(edge.namespace, "local");
        assert!(edge.deleted_at.is_none());
        assert!(
            !edges(runtime)
                .await
                .iter()
                .any(|row| row.source_id == target && row.target_id == source),
            "no inverse origin edge"
        );
    }

    async fn legacy_origin(runtime: &KhiveRuntime, fixture: &Fixture) {
        let service = entity(fixture, "service", "Existing service origin").await;
        let document = entity(fixture, "document", "Existing service design").await;
        let value = fixture
            .dispatch("link", origin(service, document))
            .await
            .expect("legacy service origin is admitted");
        exact_edge(runtime, &value, service, document).await;
    }

    #[tokio::test]
    async fn singleton_origin_persists_exact_project_document_edge() {
        let (runtime, fixture) = fixture();
        legacy_origin(&runtime, &fixture).await;
        let project = entity(&fixture, "project", "Project origin singleton").await;
        let document = entity(&fixture, "document", "Project origin design").await;
        let before = edges(&runtime).await.len();
        println!("ORIGIN2579 singleton legacy and entity controls complete");
        let value = fixture
            .dispatch("link", origin(project, document))
            .await
            .expect("ORIGIN2579 singleton admits project introduced_by document");
        exact_edge(&runtime, &value, project, document).await;
        assert_eq!(edges(&runtime).await.len(), before + 1);
    }

    #[tokio::test]
    async fn adjacent_origin_pairs_and_document_concept_dependency_stay_closed() {
        let (runtime, fixture) = fixture();
        legacy_origin(&runtime, &fixture).await;
        let project = entity(&fixture, "project", "Closed-pair project").await;
        let doc = entity(&fixture, "document", "Closed-pair document").await;
        let concept = entity(&fixture, "concept", "Implemented concept").await;
        let person = entity(&fixture, "person", "Person control").await;
        let org = entity(&fixture, "org", "Organization control").await;
        let dataset = entity(&fixture, "dataset", "Dataset control").await;
        let dependency_doc = entity(&fixture, "document", "Normative dependency").await;
        fixture
            .dispatch(
                "link",
                json!({"source_id": project.to_string(),
            "target_id": concept.to_string(), "relation": "implements"}),
            )
            .await
            .expect("legacy project implements concept");
        fixture
            .dispatch(
                "link",
                json!({"source_id": doc.to_string(),
            "target_id": dependency_doc.to_string(), "relation": "depends_on"}),
            )
            .await
            .expect("legacy normative document dependency");
        let before = edges(&runtime).await.len();
        for (source, target, relation) in [
            (doc, project, "introduced_by"),
            (project, person, "introduced_by"),
            (project, org, "introduced_by"),
            (project, doc, "derived_from"),
            (project, doc, "implements"),
            (doc, concept, "depends_on"),
            (person, doc, "introduced_by"),
            (org, doc, "introduced_by"),
            (dataset, doc, "introduced_by"),
        ] {
            let error = fixture
                .dispatch(
                    "link",
                    json!({"source_id": source.to_string(),
                "target_id": target.to_string(), "relation": relation}),
                )
                .await
                .expect_err("adjacent pair remains outside the closed contract");
            assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error:?}");
            assert_eq!(edges(&runtime).await.len(), before);
        }
    }

    #[tokio::test]
    async fn missing_and_tombstoned_origin_endpoints_still_refuse_without_edges() {
        let (runtime, fixture) = fixture();
        legacy_origin(&runtime, &fixture).await;
        let project = entity(&fixture, "project", "Live project").await;
        let doc = entity(&fixture, "document", "Deleted origin document").await;
        let deleted_project = entity(&fixture, "project", "Deleted project").await;
        let live_doc = entity(&fixture, "document", "Live origin document").await;
        assert!(runtime
            .delete_entity(
                &runtime
                    .authorize(Namespace::local())
                    .expect("authorize local fixture"),
                doc,
                false
            )
            .await
            .unwrap());
        assert!(runtime
            .delete_entity(
                &runtime
                    .authorize(Namespace::local())
                    .expect("authorize local fixture"),
                deleted_project,
                false
            )
            .await
            .unwrap());
        assert!(runtime
            .get_entity(
                &runtime
                    .authorize(Namespace::local())
                    .expect("authorize local fixture"),
                doc
            )
            .await
            .is_err());
        assert!(runtime
            .get_entity(
                &runtime
                    .authorize(Namespace::local())
                    .expect("authorize local fixture"),
                deleted_project
            )
            .await
            .is_err());
        let before = edges(&runtime).await.len();
        for (source, target) in [
            (project, Uuid::new_v4()),
            (project, doc),
            (Uuid::new_v4(), doc),
            (Uuid::new_v4(), live_doc),
            (deleted_project, live_doc),
        ] {
            let error = fixture
                .dispatch("link", origin(source, target))
                .await
                .expect_err("missing or deleted endpoint is not admitted by kind");
            assert!(matches!(error, RuntimeError::NotFound(_)), "{error:?}");
            assert_eq!(edges(&runtime).await.len(), before);
        }
    }

    #[tokio::test]
    async fn atomic_bulk_origin_admits_both_project_edges() {
        let (runtime, fixture) = fixture();
        for kind in ["service", "project"] {
            let source = entity(&fixture, kind, &format!("Atomic bulk {kind}")).await;
            let first = entity(&fixture, "document", &format!("First {kind} source")).await;
            let second = entity(&fixture, "document", &format!("Second {kind} source")).await;
            let before = edges(&runtime).await.len();
            println!("ORIGIN2579 atomic bulk testing {kind}");
            let value = fixture
                .dispatch(
                    "link",
                    json!({"links": [origin(source, first),
                origin(source, second)], "atomic": true, "verbose": true}),
                )
                .await
                .expect("ORIGIN2579 atomic bulk admits both origins");
            assert_eq!(value["attempted"], 2);
            assert_eq!(value["created"], 2);
            assert_eq!(value["failed"], 0);
            assert_eq!(value["skipped"], 0);
            let rows = value["edges"].as_array().expect("verbose edges");
            assert_eq!(rows.len(), 2);
            for (row, target) in rows.iter().zip([first, second]) {
                exact_edge(&runtime, row, source, target).await;
            }
            assert_eq!(edges(&runtime).await.len(), before + 2);
            println!("ORIGIN2579 atomic bulk {kind} control complete");
        }
    }

    #[tokio::test]
    async fn best_effort_bulk_keeps_valid_origin_and_refuses_reverse() {
        let (runtime, fixture) = fixture();
        for kind in ["service", "project"] {
            let source = entity(&fixture, kind, &format!("Best effort {kind}")).await;
            let doc = entity(&fixture, "document", &format!("Best effort {kind} design")).await;
            let before = edges(&runtime).await.len();
            let value = fixture
                .dispatch(
                    "link",
                    json!({"links": [origin(source, doc),
                origin(doc, source)], "atomic": false, "verbose": true}),
                )
                .await
                .expect("best effort returns a result for mixed endpoints");
            println!("ORIGIN2579 best effort counts for {kind}: {value}");
            assert_eq!(value["created"], 1, "ORIGIN2579 one valid {kind} origin");
            assert_eq!(value["failed"], 1);
            assert_eq!(value["attempted"], 2);
            assert_eq!(value["skipped"], 0);
            let rows = value["edges"].as_array().expect("verbose successful edges");
            assert_eq!(rows.len(), 1);
            exact_edge(&runtime, &rows[0], source, doc).await;
            assert_eq!(edges(&runtime).await.len(), before + 1);
            println!("ORIGIN2579 best effort {kind} control complete");
        }
    }

    #[tokio::test]
    async fn atomic_bulk_mixed_endpoints_never_partially_write() {
        let (runtime, fixture) = fixture();
        legacy_origin(&runtime, &fixture).await;
        for kind in ["service", "project"] {
            let source = entity(&fixture, kind, &format!("Rollback {kind}")).await;
            let doc = entity(&fixture, "document", &format!("Rollback {kind} design")).await;
            let before = edges(&runtime).await.len();
            let error = fixture
                .dispatch(
                    "link",
                    json!({"links": [origin(source, doc),
                origin(doc, source)], "atomic": true}),
                )
                .await
                .expect_err("invalid reverse origin rejects entire bulk");
            assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error:?}");
            assert_eq!(
                edges(&runtime).await.len(),
                before,
                "no partial atomic edge"
            );
        }
    }

    #[tokio::test]
    async fn query_diagnostics_recognize_project_origin_without_relaxing_reverse() {
        let (runtime, fixture) = fixture();
        legacy_origin(&runtime, &fixture).await;
        let legacy = fixture
            .dispatch(
                "query",
                json!({"query":
            "MATCH (a:service)-[e:introduced_by]->(b:document) RETURN e"}),
            )
            .await
            .unwrap();
        assert!(legacy.get("warnings").is_none(), "{legacy}");
        let reverse = fixture
            .dispatch(
                "query",
                json!({"query":
            "MATCH (a:document)-[e:introduced_by]->(b:project) RETURN e"}),
            )
            .await
            .unwrap();
        assert!(reverse["warnings"]
            .as_array()
            .expect("reverse warnings")
            .iter()
            .any(|v| v.as_str().is_some_and(|s| s.contains("can never match"))));
        println!("ORIGIN2579 possible and impossible query controls complete");
        let value = fixture
            .dispatch(
                "query",
                json!({"query":
            "MATCH (a:project)-[e:introduced_by]->(b:document) RETURN e"}),
            )
            .await
            .unwrap();
        assert!(
            value.get("warnings").is_none(),
            "ORIGIN2579 project origin is possible: {value}"
        );
        let project = entity(&fixture, "project", "Queried project").await;
        let doc = entity(&fixture, "document", "Queried origin").await;
        fixture
            .dispatch("link", origin(project, doc))
            .await
            .unwrap();
        let rows = runtime
            .query(
                &runtime
                    .authorize(Namespace::local())
                    .expect("authorize local fixture"),
                "MATCH (a:project)-[e:introduced_by]->(b:document) RETURN e",
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "query reads the persisted edge");
    }

    #[tokio::test]
    async fn resolved_endpoint_validation_uses_the_same_project_origin_rule() {
        let (runtime, fixture) = fixture();
        let tok = runtime
            .authorize(Namespace::local())
            .expect("authorize local fixture");
        let service = entity(&fixture, "service", "Resolved service").await;
        let project = entity(&fixture, "project", "Resolved project").await;
        let document = entity(&fixture, "document", "Resolved origin").await;
        let svc = Resolved::Entity(runtime.get_entity(&tok, service).await.unwrap());
        let proj = Resolved::Entity(runtime.get_entity(&tok, project).await.unwrap());
        let doc = Resolved::Entity(runtime.get_entity(&tok, document).await.unwrap());
        runtime
            .validate_link_endpoints_by_resolved(
                service,
                document,
                EdgeRelation::IntroducedBy,
                Some(&svc),
                Some(&doc),
            )
            .unwrap();
        assert!(runtime
            .validate_link_endpoints_by_resolved(
                document,
                project,
                EdgeRelation::IntroducedBy,
                Some(&doc),
                Some(&proj)
            )
            .is_err());
        assert!(runtime
            .validate_link_endpoints_by_resolved(
                project,
                document,
                EdgeRelation::IntroducedBy,
                Some(&proj),
                None
            )
            .is_err());
        assert!(khive_runtime::operations::base_entity_rule_allows(
            "service",
            EdgeRelation::IntroducedBy,
            "document"
        ));
        assert!(!khive_runtime::operations::base_entity_rule_allows(
            "document",
            EdgeRelation::IntroducedBy,
            "project"
        ));
        assert!(!khive_runtime::operations::base_entity_rule_allows(
            "document",
            EdgeRelation::DependsOn,
            "concept"
        ));
        println!("ORIGIN2579 resolved legacy/reverse/missing controls complete");
        runtime
            .validate_link_endpoints_by_resolved(
                project,
                document,
                EdgeRelation::IntroducedBy,
                Some(&proj),
                Some(&doc),
            )
            .expect("ORIGIN2579 resolved project origin uses shared base rule");
        assert!(
            khive_runtime::operations::base_entity_rule_allows(
                "project",
                EdgeRelation::IntroducedBy,
                "document"
            ),
            "ORIGIN2579 offline consumers use the same actual base predicate"
        );
        assert!(edges(&runtime).await.is_empty(), "validation is read-only");
    }

    #[tokio::test]
    async fn cite_keeps_existing_receipt_and_delegates_project_uuid() {
        let (runtime, fixture) = fixture();
        for kind in ["concept", "project"] {
            let source = entity(&fixture, kind, &format!("Cited {kind}")).await;
            let doc = entity(&fixture, "document", &format!("Citation for {kind}")).await;
            println!("ORIGIN2579 citation testing {kind}");
            let value = fixture
                .dispatch(
                    "knowledge.cite",
                    json!({"concept_id": source.to_string(),
                "source_id": doc.to_string(), "weight": 0.75}),
                )
                .await
                .expect("ORIGIN2579 cite preserves shared origin delegation");
            let mut keys: Vec<_> = value
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                [
                    "concept_id",
                    "full_id",
                    "id",
                    "relation",
                    "source_id",
                    "weight"
                ]
            );
            assert_eq!(value["concept_id"], source.to_string());
            assert_eq!(value["source_id"], doc.to_string());
            assert_eq!(value["relation"], "introduced_by");
            assert_eq!(value["weight"], 0.75);
            assert_eq!(value["id"].as_str().unwrap().len(), 8);
            exact_edge(&runtime, &value, source, doc).await;
            println!("ORIGIN2579 citation {kind} control complete");
        }
    }

    #[test]
    fn link_help_names_exact_project_origin_pair() {
        let (_, fixture) = fixture();
        let handler = fixture
            .registry
            .all_verbs()
            .into_iter()
            .find(|h| h.name == "link")
            .unwrap();
        let description = handler
            .params
            .iter()
            .find(|p| p.name == "relation")
            .unwrap()
            .description;
        let clause = description
            .split_once("introduced_by: ")
            .unwrap()
            .1
            .split(". ")
            .next()
            .unwrap();
        assert!(clause.contains("service->document"));
        assert!(clause.contains("concept->document"));
        assert!(!clause.contains("project->person"));
        assert!(!clause.contains("document->project"));
        println!("ORIGIN2579 help legacy and neighboring-pair controls complete");
        assert_eq!(
            clause.matches("project->document").count(),
            1,
            "ORIGIN2579 help names exactly one project origin pair: {clause}"
        );
    }
}
