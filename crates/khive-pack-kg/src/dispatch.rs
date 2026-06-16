//! PackRuntime impl for KgPack plus inventory self-registration.

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{EdgeEndpointRule, HandlerDef};

use crate::handler_defs::{handle_verbs, KG_HANDLERS};
use crate::pack::{KgPack, KG_EDGE_RULES};

struct KgPackFactory;

impl khive_runtime::PackFactory for KgPackFactory {
    fn name(&self) -> &'static str {
        "kg"
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(KgPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&KgPackFactory) }

#[async_trait]
impl PackRuntime for KgPack {
    fn name(&self) -> &str {
        "kg"
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        use khive_types::Pack;
        <KgPack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        use khive_types::Pack;
        <KgPack as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        &KG_HANDLERS
    }

    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        &KG_EDGE_RULES
    }

    async fn warm(&self) {
        let _ = self.runtime.embed("khive warmup").await;
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        // The `verbs` introspection verb has no namespace side-effect and is routed
        // before graph dispatch.
        if verb == "verbs" {
            return handle_verbs(params, registry);
        }

        // KG graph operations honor the NamespaceToken minted by VerbRegistry::dispatch.
        let graph_token = token;

        // Peek at `kind` for verbs that can operate on both entities and notes.
        let raw_kind = params
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let kind_is_entity_or_edge = matches!(
            raw_kind.as_str(),
            "entity"
                | "edge"
                | "concept"
                | "document"
                | "dataset"
                | "project"
                | "person"
                | "org"
                | "artifact"
                | "service"
                | "resource"
        );

        match verb {
            // Kind-discriminated: override only for entity/edge kinds.
            "create" | "list" | "search" => {
                let tok = if kind_is_entity_or_edge {
                    graph_token
                } else {
                    token
                };
                match verb {
                    "create" => self.handle_create(tok, params, registry).await,
                    "list" => self.handle_list(tok, params, registry).await,
                    _ => self.handle_search(tok, params, registry).await,
                }
            }
            // Pure graph verbs: always use graph namespace.
            "link" => self.handle_link(graph_token, params).await,
            "neighbors" => self.handle_neighbors(graph_token, params).await,
            "traverse" => self.handle_traverse(graph_token, params).await,
            "query" => self.handle_query(graph_token, params).await,
            "propose" => self.handle_propose(graph_token, params).await,
            "review" => self.handle_review(graph_token, params, registry).await,
            "withdraw" => self.handle_withdraw(graph_token, params).await,
            "stats" => self.handle_stats(graph_token, params).await,
            "merge" => self.handle_merge(graph_token, params, registry).await,
            // UUID-based: entities/edges use graph token, notes/events use caller token.
            "get" => self.handle_get(token, graph_token, params).await,
            "update" => self.handle_update(graph_token, params, registry).await,
            "delete" => self.handle_delete(graph_token, params, registry).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "kg pack does not handle verb {verb:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};
    use serde_json::json;

    use super::*;

    /// Entity created under `tenant-a` is namespaced correctly; by-ID get is namespace-agnostic.
    ///
    /// PR-A1 (ADR-007): by-ID `get` returns a record regardless of the caller's namespace.
    /// Namespace on the returned record must still reflect the creator's namespace.
    /// list/search still filter by namespace (PR-B scope).
    #[tokio::test]
    async fn kg_create_entity_honors_caller_namespace() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");

        let tenant_a = rt
            .authorize(Namespace::parse("tenant-a").expect("valid namespace"))
            .unwrap();
        let tenant_b = rt
            .authorize(Namespace::parse("tenant-b").expect("valid namespace"))
            .unwrap();

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let pack = KgPack::new(rt.clone());

        // Create an entity using tenant-a token.
        let result = pack
            .dispatch(
                "create",
                json!({
                    "kind": "concept",
                    "name": "TenantConcept",
                    "description": "concept created by tenant-a"
                }),
                &registry,
                &tenant_a,
            )
            .await
            .expect("create must succeed");

        let entity_id = result
            .get("id")
            .and_then(|v| v.as_str())
            .expect("result must contain id");

        // namespace on the stored record must be tenant-a's namespace.
        assert_eq!(
            result
                .get("namespace")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "tenant-a",
            "entity namespace must be the creator's namespace"
        );

        // tenant-a can retrieve the entity it created.
        let get_result = pack
            .dispatch("get", json!({ "id": entity_id }), &registry, &tenant_a)
            .await
            .expect("tenant-a must retrieve entity in its own namespace");
        assert_eq!(
            get_result
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "TenantConcept",
            "tenant-a must read back the entity it created"
        );

        // PR-A1: tenant-b can also retrieve the entity by UUID — by-ID get is namespace-agnostic.
        let cross_ns = pack
            .dispatch("get", json!({ "id": entity_id }), &registry, &tenant_b)
            .await
            .expect("tenant-b must find entity by UUID after PR-A1");
        assert_eq!(
            cross_ns
                .get("namespace")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "tenant-a",
            "namespace on fetched record must still be tenant-a (not rewritten to caller ns)"
        );

        // list from tenant-b must NOT return tenant-a's entity (list is namespace-scoped — PR-B).
        let list = pack
            .dispatch("list", json!({ "kind": "entity" }), &registry, &tenant_b)
            .await
            .expect("list must succeed");
        // list returns a JSON array directly (Vec<Entity> serialized) — not an object with
        // an "items" key. Panic on any other shape so the assertion is never vacuous.
        let items: Vec<serde_json::Value> = match list {
            serde_json::Value::Array(arr) => arr,
            other => panic!("list must return a JSON array; got: {other:?}"),
        };
        assert!(
            items
                .iter()
                .all(|e| e.get("namespace").and_then(|v| v.as_str()) != Some("tenant-a")),
            "list from tenant-b must not include tenant-a entities; got {items:?}"
        );
    }

    /// Two creates with no explicit namespace land in the same `local` namespace.
    #[tokio::test]
    async fn kg_oss_default_namespace_entities_colocate() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let local_token = rt.authorize(Namespace::local()).unwrap();

        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let pack = KgPack::new(rt.clone());

        // Two creates with the default local token — no explicit namespace.
        let r1 = pack
            .dispatch(
                "create",
                json!({ "kind": "concept", "name": "Alpha" }),
                &registry,
                &local_token,
            )
            .await
            .expect("first create must succeed");
        let r2 = pack
            .dispatch(
                "create",
                json!({ "kind": "concept", "name": "Beta" }),
                &registry,
                &local_token,
            )
            .await
            .expect("second create must succeed");

        let id1 = r1.get("id").and_then(|v| v.as_str()).expect("id1");
        let id2 = r2.get("id").and_then(|v| v.as_str()).expect("id2");

        // Both entities readable via the local token — co-located in default namespace.
        pack.dispatch("get", json!({ "id": id1 }), &registry, &local_token)
            .await
            .expect("Alpha must be retrievable via local token");
        pack.dispatch("get", json!({ "id": id2 }), &registry, &local_token)
            .await
            .expect("Beta must be retrievable via local token");
    }

    /// ADR-007 Rev 2, Rule 3 (PR-B): shared-brain visible-set regression.
    ///
    /// The dispatch token minted by VerbRegistry must always have visible = [primary_namespace]
    /// regardless of what `actor.visible_namespaces` was configured on the registry. All
    /// multi-record ops (list) scope to the single shared "local" set.
    ///
    /// This test verifies two invariants:
    ///
    /// 1. Records written to "local" (the OSS default namespace) are visible in a list
    ///    dispatched via the registry — the shared-brain property.
    /// 2. A record written to a non-"local" namespace (stranded record, written via a
    ///    directly-minted token) does NOT appear in the "local" list even when that
    ///    extra namespace appears in the registry's visible_namespaces config — proving
    ///    the visible set is collapsed to [primary] and not widened by the config field.
    #[tokio::test]
    async fn dispatch_list_scopes_to_primary_namespace_only_adr007_prb() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");

        let extra_ns = Namespace::parse("alpha-test-ns").expect("valid namespace");

        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.with_visible_namespaces(vec![extra_ns.clone()]);
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry builds");

        let pack = KgPack::new(rt.clone());
        let local_token = rt.authorize(Namespace::local()).expect("authorize local");

        let entity_in_local = pack
            .dispatch(
                "create",
                json!({ "kind": "concept", "name": "SharedBrainConcept" }),
                &registry,
                &local_token,
            )
            .await
            .expect("create in local must succeed");
        let local_id = entity_in_local
            .get("id")
            .and_then(|v| v.as_str())
            .expect("id");
        assert_eq!(
            entity_in_local
                .get("namespace")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "local",
            "entity written via local token must land in 'local'"
        );

        let extra_token = rt.authorize(extra_ns.clone()).expect("authorize extra-ns");
        let entity_in_extra = pack
            .dispatch(
                "create",
                json!({ "kind": "concept", "name": "StrandedConcept" }),
                &registry,
                &extra_token,
            )
            .await
            .expect("create in extra-ns must succeed");
        let extra_id = entity_in_extra
            .get("id")
            .and_then(|v| v.as_str())
            .expect("id");
        assert_eq!(
            entity_in_extra
                .get("namespace")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "alpha-test-ns",
            "entity written via extra-ns token must land in 'alpha-test-ns'"
        );

        let list_result = registry
            .dispatch("list", json!({ "kind": "entity" }))
            .await
            .expect("list must succeed");
        // list returns a JSON array directly (Vec<Entity> serialized).
        let items: Vec<serde_json::Value> = match list_result {
            serde_json::Value::Array(arr) => arr,
            other => panic!("list must return a JSON array; got: {other:?}"),
        };

        let ids: Vec<&str> = items
            .iter()
            .filter_map(|e| e.get("id").and_then(|v| v.as_str()))
            .collect();

        assert!(
            ids.contains(&local_id),
            "shared-brain: 'local' entity must appear in list; got ids: {ids:?}"
        );
        assert!(
            !ids.contains(&extra_id),
            "visible-set collapse: 'alpha-test-ns' entity must NOT appear in list \
             even though that namespace was in registry.visible_namespaces; got ids: {ids:?}"
        );
    }

    /// ADR-007 Rev 2, Rule 0 regression: non-local actor config does NOT route storage.
    ///
    /// Builds a VerbRegistry whose `default_namespace` is `"lambda:leo"` (simulating
    /// `[actor] id = "lambda:leo"` or `--actor lambda:leo`).  Dispatches `create` and
    /// `list` through `VerbRegistry::dispatch` (the real MCP path — not `pack.dispatch`).
    ///
    /// Asserts:
    /// 1. The created entity lands in `"local"`, not `"lambda:leo"`.
    /// 2. A subsequent `list` via the registry returns the entity, proving write+read both
    ///    operate on `"local"` regardless of the non-local actor configuration.
    /// 3. A direct-token `list` scoped to `"lambda:leo"` returns an empty set, proving the
    ///    storage was never written to the actor namespace.
    #[tokio::test]
    async fn non_local_actor_config_does_not_route_storage_adr007_rule0() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");

        // Registry whose default namespace mirrors what `[actor] id = "lambda:leo"` sets.
        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("lambda:leo");
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry builds");

        // Create via VerbRegistry::dispatch — this exercises the real mint path.
        let create_result = registry
            .dispatch(
                "create",
                json!({ "kind": "concept", "name": "ActorRouteProbe" }),
            )
            .await
            .expect("create must succeed through registry dispatch");

        let entity_id = create_result
            .get("id")
            .and_then(|v| v.as_str())
            .expect("create result must carry 'id'");

        // ADR-007 Rule 0: storage namespace must be 'local', not the actor/default namespace.
        assert_eq!(
            create_result
                .get("namespace")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "local",
            "entity must land in 'local' even when default_namespace='lambda:leo'; \
             actor identity must not silently become the storage namespace (ADR-007 Rule 0)"
        );

        // list via the registry must return the entity from 'local'.
        let list_result = registry
            .dispatch("list", json!({ "kind": "entity" }))
            .await
            .expect("list must succeed through registry dispatch");
        let ids_in_registry_list: Vec<&str> = match &list_result {
            serde_json::Value::Array(arr) => arr
                .iter()
                .filter_map(|e| e.get("id").and_then(|v| v.as_str()))
                .collect(),
            other => panic!("list must return a JSON array; got: {other:?}"),
        };
        assert!(
            ids_in_registry_list.contains(&entity_id),
            "entity created via non-local actor registry must be readable from 'local' list; \
             got ids: {ids_in_registry_list:?}"
        );

        // A token pinned to the actor namespace must see an empty store — nothing was
        // written there, confirming storage was not routed through the actor namespace.
        let pack = KgPack::new(rt.clone());
        let actor_ns_token = rt
            .authorize(Namespace::parse("lambda:leo").expect("valid namespace"))
            .expect("authorize lambda:leo token");
        let actor_list_result = pack
            .dispatch(
                "list",
                json!({ "kind": "entity" }),
                &registry,
                &actor_ns_token,
            )
            .await
            .expect("direct pack list via actor-ns token must succeed");
        let ids_in_actor_ns: Vec<&str> = match &actor_list_result {
            serde_json::Value::Array(arr) => arr
                .iter()
                .filter_map(|e| e.get("id").and_then(|v| v.as_str()))
                .collect(),
            other => panic!("list must return a JSON array; got: {other:?}"),
        };
        assert!(
            !ids_in_actor_ns.contains(&entity_id),
            "storage must not have been routed to 'lambda:leo'; \
             entity must NOT appear when listing directly under that namespace; \
             got ids: {ids_in_actor_ns:?}"
        );
    }
}
