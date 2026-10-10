use super::*;
use crate::Namespace;
use serde_json::json;
use std::io::Write;
use std::sync::{Arc, Mutex};

fn archive(kinds: &[&str]) -> KgArchive {
    let at = DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
        .unwrap()
        .with_timezone(&Utc);
    KgArchive {
        format: "khive-kg".into(),
        version: "0.1".into(),
        namespace: "local".into(),
        exported_at: at,
        entities: kinds
            .iter()
            .enumerate()
            .map(|(index, kind)| ExportedEntity {
                id: Uuid::from_u128(index as u128 + 1),
                kind: (*kind).into(),
                entity_type: Some("kept-subtype".into()),
                name: format!("Import{index}"),
                description: Some("complete source text".into()),
                properties: Some(json!({"keep":[1,true,"text"]})),
                tags: vec!["tag".into()],
                created_at: at,
                updated_at: at + chrono::Duration::seconds(1),
            })
            .collect(),
        edges: vec![],
    }
}

fn runtime(installed: bool) -> KhiveRuntime {
    let runtime = KhiveRuntime::memory().unwrap();
    if installed {
        runtime.install_kind_registry(vec!["concept".into(), "resource".into()], vec![]);
    }
    runtime
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn strict_import_defaults_refuse_late_unknown_kinds_before_any_write() {
    for installed in [false, true] {
        let runtime = runtime(installed);
        let token = runtime.authorize(Namespace::local()).unwrap();
        let input = archive(&["concept", "Uninstalled"]);
        assert!(runtime.import_kg(&input, &token).await.is_err());
        assert!(runtime
            .import_kg_json(&serde_json::to_string(&input).unwrap(), &token)
            .await
            .is_err());
        assert!(runtime.export_kg(&token).await.unwrap().entities.is_empty());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn relaxed_import_preserves_fields_and_fts_warns_once_and_does_not_register_kinds() {
    for installed in [false, true] {
        let runtime = runtime(installed);
        let token = runtime.authorize(Namespace::local()).unwrap();
        let input = archive(&["concept", " Future型 ", " Future型 ", " future型 ", "Other"]);
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let validation = runtime
            .validate_kg_import(&input, ImportKindPolicy::PreserveUnknown)
            .unwrap();
        assert_eq!(
            validation.unknown_entity_kinds,
            [" Future型 ", " future型 ", "Other"]
        );
        assert!(capture.0.lock().unwrap().is_empty());
        let summary = runtime
            .import_kg_with_policy(&input, &token, ImportKindPolicy::PreserveUnknown)
            .await
            .unwrap();
        assert_eq!(summary.entities_imported, 5);
        assert_eq!(summary.edges_imported, 0);
        let notices = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            notices
                .matches("preserving unknown entity kind during explicit import")
                .count(),
            3,
            "{notices}"
        );
        for expected in &input.entities {
            let actual = runtime.get_entity(&token, expected.id).await.unwrap();
            assert_eq!(actual.kind, expected.kind);
            assert_eq!(actual.entity_type, expected.entity_type);
            assert_eq!(actual.name, expected.name);
            assert_eq!(actual.description, expected.description);
            assert_eq!(actual.properties, expected.properties);
            assert_eq!(actual.tags, expected.tags);
            assert_eq!(actual.created_at, expected.created_at.timestamp_micros());
            assert_eq!(actual.updated_at, expected.updated_at.timestamp_micros());
            let document = runtime
                .text(&token)
                .unwrap()
                .get_document("local", expected.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                document.record_kind.as_deref(),
                Some(expected.kind.as_str())
            );
            assert_eq!(document.title.as_deref(), Some(expected.name.as_str()));
            assert!(document.body.contains("complete source text"));
        }
        let exported = runtime.export_kg(&token).await.unwrap();
        assert!(runtime.import_kg(&exported, &token).await.is_err());
        assert_eq!(
            runtime.import_entity_kind_registry().unwrap(),
            if installed {
                vec!["concept".to_string(), "resource".to_string()]
            } else {
                vec![]
            }
        );
        let create = runtime
            .create_entity(&token, "Other", None, "Ordinary", None, None, vec![])
            .await;
        let prepare = crate::atomic_prepare::prepare_add_entity(
            &runtime,
            &token,
            &json!({"kind":"Other","name":"Atomic"}),
        )
        .await;
        if installed {
            assert!(create.is_err());
            assert!(prepare.is_err());
        } else {
            assert!(
                create.is_ok(),
                "bare-runtime ordinary create remains permissive"
            );
            assert!(prepare.is_ok());
        }
    }
}

#[tokio::test]
async fn relaxed_import_keeps_all_deterministic_refusals_before_writes() {
    for policy in [ImportKindPolicy::Strict, ImportKindPolicy::PreserveUnknown] {
        for case in 0..10 {
            let runtime = runtime(true);
            let token = runtime.authorize(Namespace::local()).unwrap();
            let mut input = archive(&["concept", "concept"]);
            let at = input.exported_at;
            input.edges.push(ExportedEdge {
                edge_id: Uuid::from_u128(100),
                source: input.entities[0].id,
                target: input.entities[1].id,
                relation: EdgeRelation::Extends,
                weight: 0.7,
                properties: None,
                created_at: at,
                updated_at: at,
            });
            match case {
                0 => input.entities[1].kind = " ".into(),
                1 => input.entities[1].name = " ".into(),
                2 => input.entities[1].properties = Some(json!({"khive:secret_gate":"forged"})),
                3 => input.entities[1].properties = Some(json!({"khive:web_receipt":"forged"})),
                4 => input.edges[0].properties = Some(json!({"khive:secret_gate":"forged"})),
                5 => input.edges[0].properties = Some(json!({"khive:web_receipt":"forged"})),
                6 => input.edges[0].properties = Some(json!({"api_key":"AKIAFAKEKEY1234567890"})),
                7 => input.edges[0].weight = f64::NAN,
                8 => input.edges[0].weight = -0.1,
                9 => input.edges[0].weight = 1.1,
                _ => unreachable!(),
            }
            let result = runtime.import_kg_with_policy(&input, &token, policy).await;
            assert!(result.is_err(), "case {case}: {result:?}");
            assert!(
                runtime.export_kg(&token).await.unwrap().entities.is_empty(),
                "case {case}"
            );
        }
        let runtime = runtime(true);
        let token = runtime.authorize(Namespace::local()).unwrap();
        let mut input = serde_json::to_value(archive(&["concept", "concept"])).unwrap();
        input["edges"] = json!([{"edge_id":Uuid::from_u128(100),"source":Uuid::from_u128(1),"target":Uuid::from_u128(2),"relation":"future","weight":0.7}]);
        assert!(runtime
            .import_kg_json_with_policy(&input.to_string(), &token, policy)
            .await
            .is_err());
        assert!(runtime.export_kg(&token).await.unwrap().entities.is_empty());
    }
}

#[tokio::test]
async fn known_pack_and_endpoint_skip_behavior_survive_relaxed_import() {
    let runtime = runtime(true);
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut input = archive(&["concept", "concept", "resource"]);
    let at = input.exported_at;
    for (edge_id, target) in [(100, Uuid::from_u128(2)), (101, Uuid::from_u128(999))] {
        input.edges.push(ExportedEdge {
            edge_id: Uuid::from_u128(edge_id),
            source: input.entities[0].id,
            target,
            relation: EdgeRelation::Extends,
            weight: 0.7,
            properties: None,
            created_at: at,
            updated_at: at,
        });
    }
    let summary = runtime
        .import_kg_with_policy(&input, &token, ImportKindPolicy::PreserveUnknown)
        .await
        .unwrap();
    assert_eq!(summary.entities_imported, 3);
    assert_eq!(summary.edges_imported, 1);
    assert_eq!(summary.edges_skipped, 1);
    for kind in ["Paper", " RESOURCE "] {
        let noncanonical = archive(&[kind]);
        assert!(runtime
            .validate_kg_import(&noncanonical, ImportKindPolicy::PreserveUnknown)
            .is_err());
    }
}

#[tokio::test]
async fn explicit_json_policy_api_imports_unknown_kind_and_default_api_refuses_it() {
    let runtime = runtime(true);
    let token = runtime.authorize(Namespace::local()).unwrap();
    let input = archive(&["Uninstalled"]);
    let json = serde_json::to_string(&input).unwrap();
    assert!(runtime.import_kg_json(&json, &token).await.is_err());
    let summary = runtime
        .import_kg_json_with_policy(&json, &token, ImportKindPolicy::PreserveUnknown)
        .await
        .unwrap();
    assert_eq!(summary.entities_imported, 1);
    assert_eq!(
        runtime
            .get_entity(&token, input.entities[0].id)
            .await
            .unwrap()
            .kind,
        "Uninstalled"
    );
}
