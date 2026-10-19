use super::{backend_id, memory_runtime, BackendRegistry, SubstrateCoordinator};
use khive_runtime::{KhiveRuntime, NamespaceToken};
use khive_storage::{Edge, EdgeRelation};
use khive_types::namespace::Namespace;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Copy, Debug)]
enum Route {
    Local,
    CoordinatorSameBackend,
    CoordinatorCrossBackend,
}

const ROUTES: [Route; 3] = [
    Route::Local,
    Route::CoordinatorSameBackend,
    Route::CoordinatorCrossBackend,
];

struct Fixture {
    route: Route,
    source: Arc<KhiveRuntime>,
    remote: Arc<KhiveRuntime>,
    source_token: NamespaceToken,
    remote_token: NamespaceToken,
    coordinator: SubstrateCoordinator,
    namespace: Namespace,
}

impl Fixture {
    fn new(route: Route) -> Self {
        let source = memory_runtime();
        let remote = memory_runtime();
        let namespace = Namespace::local();
        let source_token = source.authorize(namespace.clone()).unwrap();
        let remote_token = remote.authorize(namespace.clone()).unwrap();
        let mut registry = BackendRegistry::new();
        registry.register(backend_id("main"), Arc::clone(&source));
        registry.register(backend_id("lore"), Arc::clone(&remote));
        Self {
            route,
            source,
            remote,
            source_token,
            remote_token,
            coordinator: SubstrateCoordinator::new(registry),
            namespace,
        }
    }

    async fn pair(&self, source_kind: &str, target_kind: &str) -> (Uuid, Uuid) {
        let source = self
            .source
            .create_entity_with_embedding_report(
                &self.source_token,
                source_kind,
                None,
                "DependencySource",
                None,
                None,
                vec![],
            )
            .await
            .unwrap()
            .0;
        let (runtime, token) = match self.route {
            Route::CoordinatorCrossBackend => (self.remote.as_ref(), &self.remote_token),
            _ => (self.source.as_ref(), &self.source_token),
        };
        let target = runtime
            .create_entity_with_embedding_report(
                token,
                target_kind,
                None,
                "DependencyTarget",
                None,
                None,
                vec![],
            )
            .await
            .unwrap()
            .0;
        (source.id, target.id)
    }

    async fn link(
        &self,
        source: Uuid,
        target: Uuid,
        metadata: Option<Value>,
    ) -> Result<Edge, String> {
        match self.route {
            Route::Local => self
                .source
                .link(
                    &self.source_token,
                    source,
                    target,
                    EdgeRelation::DependsOn,
                    1.0,
                    metadata,
                )
                .await
                .map_err(|error| error.to_string()),
            _ => {
                self.coordinator
                    .link_cross_backend(
                        &self.namespace,
                        source,
                        target,
                        EdgeRelation::DependsOn,
                        1.0,
                        metadata,
                    )
                    .await
            }
        }
    }

    async fn stored(&self, returned: &Edge, source: Uuid, target: Uuid) -> Edge {
        let stored = self
            .source
            .graph(&self.source_token)
            .unwrap()
            .get_edge(returned.id)
            .await
            .unwrap()
            .expect("edge is stored on its source backend");
        assert_eq!(
            serde_json::to_value(&stored).unwrap(),
            serde_json::to_value(returned).unwrap(),
            "writer-observed edge must match storage: {:?}",
            self.route
        );
        assert_eq!(stored.source_id, source);
        assert_eq!(stored.target_id, target);
        assert_eq!(stored.relation, EdgeRelation::DependsOn);
        assert_eq!(
            stored.target_backend.as_deref(),
            match self.route {
                Route::CoordinatorCrossBackend => Some("lore"),
                _ => None,
            }
        );
        assert!(self
            .remote
            .graph(&self.remote_token)
            .unwrap()
            .get_edge(stored.id)
            .await
            .unwrap()
            .is_none());
        stored
    }

    async fn edge_counts(&self) -> (u64, u64) {
        let source = self
            .source
            .graph(&self.source_token)
            .unwrap()
            .count_edges(Default::default())
            .await
            .unwrap();
        let remote = self
            .remote
            .graph(&self.remote_token)
            .unwrap()
            .count_edges(Default::default())
            .await
            .unwrap();
        (source, remote)
    }
}

#[tokio::test]
async fn dependency_defaults_are_stored_for_local_and_coordinator_routes() {
    // Contract oracle: production inference remains owned by khive-runtime.
    let expected_defaults = [
        ("document", "document", "normative"),
        ("project", "project", "build"),
        ("service", "service", "runtime"),
        ("service", "dataset", "data"),
        ("service", "artifact", "artifact"),
        ("artifact", "project", "tooling"),
        ("artifact", "service", "tooling"),
    ];
    for route in ROUTES {
        let fixture = Fixture::new(route);
        for (source_kind, target_kind, expected) in expected_defaults {
            let (source, target) = fixture.pair(source_kind, target_kind).await;
            let returned = fixture.link(source, target, None).await.unwrap();
            let stored = fixture.stored(&returned, source, target).await;
            assert_eq!(
                stored.metadata,
                Some(json!({"dependency_kind": expected})),
                "{route:?}: {source_kind} -> {target_kind}"
            );
        }
    }
}

#[tokio::test]
async fn explicit_dependency_kind_wins_on_local_and_coordinator_routes() {
    for route in ROUTES {
        let fixture = Fixture::new(route);
        let (source, target) = fixture.pair("document", "document").await;
        let metadata = json!({"dependency_kind": "data", "label": "keep"});
        let returned = fixture
            .link(source, target, Some(metadata.clone()))
            .await
            .unwrap();
        let stored = fixture.stored(&returned, source, target).await;
        assert_eq!(stored.metadata, Some(metadata), "{route:?}");
    }
}

#[tokio::test]
async fn no_default_dependency_pair_keeps_metadata_on_all_routes() {
    for route in ROUTES {
        let fixture = Fixture::new(route);
        for metadata in [None, Some(json!({"label": "keep"}))] {
            let (source, target) = fixture.pair("service", "project").await;
            let returned = fixture
                .link(source, target, metadata.clone())
                .await
                .unwrap();
            let stored = fixture.stored(&returned, source, target).await;
            assert_eq!(stored.metadata, metadata, "{route:?}");
        }
    }
}

#[tokio::test]
async fn unknown_dependency_kind_is_refused_without_edges_on_all_routes() {
    for route in ROUTES {
        let fixture = Fixture::new(route);
        for (source_kind, target_kind) in [("document", "document"), ("service", "project")] {
            let (source, target) = fixture.pair(source_kind, target_kind).await;
            let before = fixture.edge_counts().await;
            let error = fixture
                .link(
                    source,
                    target,
                    Some(json!({"dependency_kind": "unknown-kind", "label": "keep"})),
                )
                .await
                .expect_err("an explicit unknown qualifier must be refused");
            assert!(
                error.contains("unknown dependency_kind"),
                "{route:?}: {source_kind} -> {target_kind}: {error}"
            );
            assert_eq!(fixture.edge_counts().await, before, "{route:?}");
        }
    }
}
