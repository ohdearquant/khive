//! Actual multi-backend task fanout must carry content-free audit facts.
use std::sync::Arc;

use khive_pack_kg::handlers::ValidatedSearchRequest;
use khive_runtime::{
    BackendId, KhiveRuntime, Namespace, PackRegistry, RuntimeConfig, VerbRegistryBuilder,
};
use khive_storage::{EventFilter, PageRequest};
use khive_types::EventKind;
use serde_json::json;

use super::{BackendRegistry, SubstrateCoordinator};

#[tokio::test]
async fn intercepted_fanout_records_empty_text_execution_and_empty_backend_set() {
    for backend_count in [0, 1, 2] {
        let rt = KhiveRuntime::new_for_test(RuntimeConfig {
            db_path: None,
            packs: vec!["kg".into()],
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let token = rt.authorize(Namespace::local()).unwrap();
        let store = rt.events(&token).unwrap();
        let mut builder = VerbRegistryBuilder::new();
        builder.with_event_store(store.clone());
        PackRegistry::register_packs(&["kg".into()], rt.clone(), &mut builder).unwrap();
        let registry = builder.build().unwrap();
        let mut backends = BackendRegistry::new();
        for name in ["main", "lore"].into_iter().take(backend_count) {
            let backend = KhiveRuntime::new_for_test(RuntimeConfig {
                db_path: None,
                packs: vec!["kg".into()],
                ..RuntimeConfig::no_embeddings()
            })
            .unwrap();
            backends.register(BackendId::parse(name).unwrap(), Arc::new(backend));
        }
        let coordinator = SubstrateCoordinator::new(backends);
        let args = json!({"kind":"entity", "query":"privateauditneedle", "source":"vector"});
        let request = ValidatedSearchRequest::from_value(args.clone(), &registry).unwrap();
        registry
            .dispatch_intercepted_with_identity("search", &args, None, |ns| async move {
                let (entities, notes, _) = coordinator.fan_out_search(&request, &ns).await;
                assert!(entities.is_empty() && notes.is_empty());
                Ok(json!([]))
            })
            .await
            .unwrap();
        let page = store
            .query_events(
                EventFilter {
                    kinds: vec![EventKind::Audit],
                    verbs: vec!["search".into()],
                    ..Default::default()
                },
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        let payload = &page.items[0].payload;
        assert_eq!(
            payload["arms"],
            if backend_count == 0 { "none" } else { "text" }
        );
        assert_eq!(payload["result_count"], 0);
        assert_eq!(payload["effective_limit"], 10);
        assert_eq!(payload["match_mode"], "all_terms");
        assert!(!payload.to_string().contains("privateauditneedle"));
    }
}
