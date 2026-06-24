//! PackRuntime impl for KgPack plus inventory self-registration.

use async_trait::async_trait;
use serde_json::Value;

use std::sync::Arc;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    EntityTypeValidatorFn, KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry,
};
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

    fn register_entity_type_validator(&self, runtime: &KhiveRuntime) {
        let validator: EntityTypeValidatorFn = Arc::new(|kind, entity_type| {
            let Some(raw) = entity_type else {
                return Ok(None);
            };
            let ek: khive_types::EntityKind = kind
                .parse()
                .map_err(|_| RuntimeError::InvalidInput(format!("unknown entity kind {kind:?}")))?;
            crate::entity_type_registry::EntityTypeRegistry::global()
                .resolve(ek, Some(raw))
                .map(|r| r.entity_type)
        });
        runtime.install_entity_type_validator(validator);
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
            "get" => self.handle_get(token, graph_token, params, registry).await,
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

    /// ADR-007 Rev 4, Rule 3b: default read scope honors configured visible set.
    ///
    /// The dispatch token minted by VerbRegistry on the default (no explicit `namespace=`)
    /// path widens the read scope to `['local'] ∪ visible_namespaces`. Both the 'local'
    /// entity AND the entity written to a configured extra namespace must appear in a
    /// registry-dispatched list without any explicit `namespace=` parameter.
    ///
    /// This test verifies:
    ///
    /// 1. Records written to "local" are visible in the list — the shared-brain property.
    /// 2. A record written to a configured visible namespace (via a directly-minted token)
    ///    ALSO appears in the registry-dispatched list — the Rev 4 read-scope widening.
    #[tokio::test]
    async fn dispatch_list_honors_configured_visible_namespaces_adr007_rev4() {
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
                json!({ "kind": "concept", "name": "ConfiguredVisibleConcept" }),
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
            ids.contains(&extra_id),
            "Rev 4 read-scope widening: 'alpha-test-ns' entity MUST appear in list \
             because that namespace is in registry.visible_namespaces; got ids: {ids:?}"
        );
    }

    /// ADR-007 Rev 4: backward-compat — with visible_namespaces UNSET, default read scope = ['local'] only.
    ///
    /// A registry with no `visible_namespaces` configured has the same behavior as Rev 3:
    /// list returns only records in 'local'. A record written to a different namespace via
    /// a directly-minted token does NOT appear in the registry list.
    #[tokio::test]
    async fn dispatch_list_empty_visible_namespaces_scopes_to_local_only_adr007_rev4() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");

        // Registry with NO visible_namespaces — backward-compat with Rev 3.
        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        // Intentionally NOT calling with_visible_namespaces — default is empty.
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry builds");

        let pack = KgPack::new(rt.clone());
        let local_token = rt.authorize(Namespace::local()).expect("authorize local");
        let other_token = rt
            .authorize(Namespace::parse("other-ns").expect("valid"))
            .expect("authorize other-ns");

        let entity_in_local = pack
            .dispatch(
                "create",
                json!({ "kind": "concept", "name": "LocalConcept" }),
                &registry,
                &local_token,
            )
            .await
            .expect("create in local must succeed");
        let local_id = entity_in_local
            .get("id")
            .and_then(|v| v.as_str())
            .expect("id");

        let entity_in_other = pack
            .dispatch(
                "create",
                json!({ "kind": "concept", "name": "OtherNsConcept" }),
                &registry,
                &other_token,
            )
            .await
            .expect("create in other-ns must succeed");
        let other_id = entity_in_other
            .get("id")
            .and_then(|v| v.as_str())
            .expect("id");

        let list_result = registry
            .dispatch("list", json!({ "kind": "entity" }))
            .await
            .expect("list must succeed");
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
            "backward-compat: 'local' entity must appear in list; got ids: {ids:?}"
        );
        assert!(
            !ids.contains(&other_id),
            "backward-compat: 'other-ns' entity must NOT appear when visible_namespaces is unset; \
             got ids: {ids:?}"
        );
    }

    /// ADR-007 Rev 4: 'local' is always included in the default read scope,
    /// even when visible_namespaces does not explicitly list it.
    ///
    /// Configuring visible_namespaces = ["other-ns"] (without "local") must still
    /// return records from BOTH 'local' and 'other-ns' in the registry list.
    #[tokio::test]
    async fn dispatch_list_local_always_included_when_visible_ns_set_adr007_rev4() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");

        let other_ns = Namespace::parse("other-ns").expect("valid namespace");

        // Registry with visible_namespaces = ["other-ns"] — does NOT contain "local".
        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.with_visible_namespaces(vec![other_ns.clone()]);
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry builds");

        let pack = KgPack::new(rt.clone());
        let local_token = rt.authorize(Namespace::local()).expect("authorize local");
        let other_token = rt.authorize(other_ns.clone()).expect("authorize other-ns");

        let entity_local = pack
            .dispatch(
                "create",
                json!({ "kind": "concept", "name": "LocalEntity" }),
                &registry,
                &local_token,
            )
            .await
            .expect("create in local must succeed");
        let local_id = entity_local.get("id").and_then(|v| v.as_str()).expect("id");

        let entity_other = pack
            .dispatch(
                "create",
                json!({ "kind": "concept", "name": "OtherEntity" }),
                &registry,
                &other_token,
            )
            .await
            .expect("create in other-ns must succeed");
        let other_id = entity_other.get("id").and_then(|v| v.as_str()).expect("id");

        let list_result = registry
            .dispatch("list", json!({ "kind": "entity" }))
            .await
            .expect("list must succeed");
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
            "'local' must always appear in list even when not in visible_namespaces config; \
             got ids: {ids:?}"
        );
        assert!(
            ids.contains(&other_id),
            "'other-ns' must appear because it is in visible_namespaces; got ids: {ids:?}"
        );
    }

    /// ADR-007 Rev 4: explicit namespace= param is a precise single-namespace escape, NOT widened.
    ///
    /// With visible_namespaces=["other-ns"] configured, a list(namespace="other-ns") call
    /// scopes to EXACTLY ["other-ns"] and does NOT include 'local' or the union set.
    /// This preserves the ability to read a single named set precisely.
    #[tokio::test]
    async fn dispatch_explicit_namespace_param_is_precise_not_widened_adr007_rev4() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");

        let other_ns = Namespace::parse("other-ns").expect("valid namespace");

        // Registry with visible_namespaces = ["other-ns"].
        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.with_visible_namespaces(vec![other_ns.clone()]);
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry builds");

        let pack = KgPack::new(rt.clone());
        let local_token = rt.authorize(Namespace::local()).expect("authorize local");
        let other_token = rt.authorize(other_ns.clone()).expect("authorize other-ns");

        // Write one entity in 'local' and one in 'other-ns'.
        let entity_local = pack
            .dispatch(
                "create",
                json!({ "kind": "concept", "name": "LocalPrecise" }),
                &registry,
                &local_token,
            )
            .await
            .expect("create in local must succeed");
        let local_id = entity_local.get("id").and_then(|v| v.as_str()).expect("id");

        let entity_other = pack
            .dispatch(
                "create",
                json!({ "kind": "concept", "name": "OtherPrecise" }),
                &registry,
                &other_token,
            )
            .await
            .expect("create in other-ns must succeed");
        let other_id = entity_other.get("id").and_then(|v| v.as_str()).expect("id");

        // list(namespace="other-ns") — explicit escape: must scope to EXACTLY "other-ns".
        let list_result = registry
            .dispatch("list", json!({ "kind": "entity", "namespace": "other-ns" }))
            .await
            .expect("list with explicit namespace must succeed");
        let items: Vec<serde_json::Value> = match list_result {
            serde_json::Value::Array(arr) => arr,
            other => panic!("list must return a JSON array; got: {other:?}"),
        };
        let ids: Vec<&str> = items
            .iter()
            .filter_map(|e| e.get("id").and_then(|v| v.as_str()))
            .collect();

        assert!(
            ids.contains(&other_id),
            "explicit namespace='other-ns' must return the other-ns entity; got ids: {ids:?}"
        );
        assert!(
            !ids.contains(&local_id),
            "explicit namespace='other-ns' must NOT include 'local' entity — \
             the explicit param is a precise escape, not widened by visible_namespaces; \
             got ids: {ids:?}"
        );
    }

    /// ADR-007 Rev 4, Rule 0 regression: non-local actor config does NOT route WRITE storage.
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

    // ---- Handler-level regression tests for issue #225 (filter-pushdown cliff) ----
    //
    // These tests operate at the `pack.dispatch("search", ...)` level and prove the
    // REAL handler cliff: with `limit=1` the handler sets `search_limit =
    // (1 * 50).min(500) = 50`, which means only 50 candidates enter the runtime.
    // To push the target BEYOND the pre-fix cliff we insert 51 non-matching decoys
    // so that the target sits at rank 52 in the unfiltered FTS ordering.
    //
    // Without predicate pushdown into `hybrid_search` / `search_notes` the runtime
    // received `search_limit = 50` candidates, all decoys, and never saw the target.
    // With the fix the runtime applies the filter BEFORE truncation over all 200
    // candidates (50 × CANDIDATE_MULTIPLIER = 4) and surfaces the target.
    //
    // BUDGET CONSTANTS (from handlers/search.rs and retrieval.rs):
    //   handler search_limit = (limit * 50).min(500)  → limit=1 → 50
    //   runtime candidates   = search_limit * 4       → 200
    //   old cliff: rank 51 (beyond handler 50-record scan)
    //   new cliff: rank 201 (beyond runtime 200-candidate budget)
    //
    // Corpus: 51 decoys (ranks 1-51 in FTS) + 1 target (rank 52). Target has the
    // discriminating tag/property; decoys do not. `limit=1`.

    /// Handler-level regression for issue #225, entity branch, tag filter.
    ///
    /// 51 decoys rank above the target in FTS but lack the required tag.
    /// The target carries the required tag and sits at rank 52.
    /// With predicate pushdown the handler returns the target despite the cliff.
    #[tokio::test]
    async fn handler_search_entity_tag_filter_beyond_scan_cliff() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let tok = rt.authorize(Namespace::local()).unwrap();

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");
        let pack = KgPack::new(rt.clone());

        // 51 decoys: high TF on query words ensures they rank above the target in
        // FTS regardless of length normalization. No target tag.
        let decoy_blob = "cliff query ".repeat(20);
        for i in 0..51usize {
            pack.dispatch(
                "create",
                json!({
                    "kind": "concept",
                    "name": format!("{decoy_blob}decoy {i}"),
                    "description": format!("{decoy_blob}decoy {i} description"),
                    "tags": ["decoy-tag"]
                }),
                &registry,
                &tok,
            )
            .await
            .expect("decoy create must succeed");
        }

        // Target: single occurrence of each query word, carries the required tag.
        let target_result = pack
            .dispatch(
                "create",
                json!({
                    "kind": "concept",
                    "name": "cliff query target",
                    "description": "cliff query target description",
                    "tags": ["cliff-target-tag"]
                }),
                &registry,
                &tok,
            )
            .await
            .expect("target create must succeed");
        let target_id = target_result
            .get("id")
            .and_then(|v| v.as_str())
            .expect("target must have id");

        // Search with tag filter and limit=1. The handler widens to search_limit=50
        // which only covers the 51 decoys without the fix. With the fix, the
        // predicate is applied before truncation and the target is returned.
        let result = pack
            .dispatch(
                "search",
                json!({
                    "kind": "concept",
                    "query": "cliff query",
                    "tags": ["cliff-target-tag"],
                    "limit": 1
                }),
                &registry,
                &tok,
            )
            .await
            .expect("search must succeed");

        let hits = result.as_array().expect("search must return an array");
        assert_eq!(hits.len(), 1, "exactly one hit expected; got {hits:?}");
        assert_eq!(
            hits[0].get("id").and_then(|v| v.as_str()).unwrap_or(""),
            target_id,
            "the tag-filtered entity (rank 52) must be returned despite the handler scan cliff; \
             got {hits:?}"
        );
    }

    /// Handler-level regression for issue #225, entity branch, properties filter.
    ///
    /// 51 decoys rank above the target in FTS but have the wrong property value.
    /// The target has the required property and sits at rank 52.
    #[tokio::test]
    async fn handler_search_entity_props_filter_beyond_scan_cliff() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let tok = rt.authorize(Namespace::local()).unwrap();

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");
        let pack = KgPack::new(rt.clone());

        let decoy_blob = "props cliff signal ".repeat(20);
        for i in 0..51usize {
            pack.dispatch(
                "create",
                json!({
                    "kind": "concept",
                    "name": format!("{decoy_blob}decoy {i}"),
                    "description": format!("{decoy_blob}decoy {i} description"),
                    "properties": {"domain": "other"}
                }),
                &registry,
                &tok,
            )
            .await
            .expect("decoy create must succeed");
        }

        let target_result = pack
            .dispatch(
                "create",
                json!({
                    "kind": "concept",
                    "name": "props cliff signal target",
                    "description": "props cliff signal target description",
                    "properties": {"domain": "props-target"}
                }),
                &registry,
                &tok,
            )
            .await
            .expect("target create must succeed");
        let target_id = target_result
            .get("id")
            .and_then(|v| v.as_str())
            .expect("target must have id");

        let result = pack
            .dispatch(
                "search",
                json!({
                    "kind": "concept",
                    "query": "props cliff signal",
                    "properties": {"domain": "props-target"},
                    "limit": 1
                }),
                &registry,
                &tok,
            )
            .await
            .expect("search must succeed");

        let hits = result.as_array().expect("search must return an array");
        assert_eq!(hits.len(), 1, "exactly one hit expected; got {hits:?}");
        assert_eq!(
            hits[0].get("id").and_then(|v| v.as_str()).unwrap_or(""),
            target_id,
            "the props-filtered entity (rank 52) must be returned despite the handler scan cliff; \
             got {hits:?}"
        );
    }

    /// Handler-level regression for issue #225, note branch, tag filter.
    ///
    /// 51 observation notes rank above the target in FTS but lack the required tag.
    /// The target carries the required tag and sits at rank 52.
    #[tokio::test]
    async fn handler_search_note_tag_filter_beyond_scan_cliff() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let tok = rt.authorize(Namespace::local()).unwrap();

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");
        let pack = KgPack::new(rt.clone());

        let decoy_blob = "note cliff token ".repeat(20);
        for i in 0..51usize {
            pack.dispatch(
                "create",
                json!({
                    "kind": "observation",
                    "content": format!("{decoy_blob}decoy {i}"),
                    "properties": {"tags": ["note-decoy-tag"]}
                }),
                &registry,
                &tok,
            )
            .await
            .expect("decoy note create must succeed");
        }

        let target_result = pack
            .dispatch(
                "create",
                json!({
                    "kind": "observation",
                    "content": "note cliff token target",
                    "properties": {"tags": ["note-cliff-target-tag"]}
                }),
                &registry,
                &tok,
            )
            .await
            .expect("target note create must succeed");
        let target_id = target_result
            .get("id")
            .and_then(|v| v.as_str())
            .expect("target must have id");

        let result = pack
            .dispatch(
                "search",
                json!({
                    "kind": "note",
                    "query": "note cliff token",
                    "tags": ["note-cliff-target-tag"],
                    "limit": 1
                }),
                &registry,
                &tok,
            )
            .await
            .expect("search must succeed");

        let hits = result.as_array().expect("search must return an array");
        assert_eq!(hits.len(), 1, "exactly one hit expected; got {hits:?}");
        assert_eq!(
            hits[0].get("id").and_then(|v| v.as_str()).unwrap_or(""),
            target_id,
            "the tag-filtered note (rank 52) must be returned despite the handler scan cliff; \
             got {hits:?}"
        );
    }

    /// Handler-level regression for issue #225, note branch, properties filter.
    ///
    /// 51 observation notes rank above the target in FTS but have the wrong property.
    /// The target has the required property value and sits at rank 52.
    #[tokio::test]
    async fn handler_search_note_props_filter_beyond_scan_cliff() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let tok = rt.authorize(Namespace::local()).unwrap();

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");
        let pack = KgPack::new(rt.clone());

        let decoy_blob = "note props wave ".repeat(20);
        for i in 0..51usize {
            pack.dispatch(
                "create",
                json!({
                    "kind": "observation",
                    "content": format!("{decoy_blob}decoy {i}"),
                    "properties": {"category": "other", "tags": ["note-decoy-tag"]}
                }),
                &registry,
                &tok,
            )
            .await
            .expect("decoy note create must succeed");
        }

        let target_result = pack
            .dispatch(
                "create",
                json!({
                    "kind": "observation",
                    "content": "note props wave target",
                    "properties": {"category": "note-props-target", "tags": ["note-decoy-tag"]}
                }),
                &registry,
                &tok,
            )
            .await
            .expect("target note create must succeed");
        let target_id = target_result
            .get("id")
            .and_then(|v| v.as_str())
            .expect("target must have id");

        let result = pack
            .dispatch(
                "search",
                json!({
                    "kind": "note",
                    "query": "note props wave",
                    "properties": {"category": "note-props-target"},
                    "limit": 1
                }),
                &registry,
                &tok,
            )
            .await
            .expect("search must succeed");

        let hits = result.as_array().expect("search must return an array");
        assert_eq!(hits.len(), 1, "exactly one hit expected; got {hits:?}");
        assert_eq!(
            hits[0].get("id").and_then(|v| v.as_str()).unwrap_or(""),
            target_id,
            "the props-filtered note (rank 52) must be returned despite the handler scan cliff; \
             got {hits:?}"
        );
    }
}
