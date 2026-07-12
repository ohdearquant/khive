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

    fn register_entity_type_validator(&self, _runtime: &KhiveRuntime) {
        // Install the validator on the runtime this pack OWNS, not on the
        // caller-supplied runtime.  In a multi-backend deployment the pack
        // is constructed with a per-pack runtime (see PackRegistry::
        // register_packs_with_runtimes); `self.runtime` is that runtime.
        // In a single-backend deployment `self.runtime` IS the single
        // runtime, so behaviour is identical to the previous call-through.
        let validator: EntityTypeValidatorFn = Arc::new(|kind, entity_type| {
            let Some(raw) = entity_type else {
                return Ok(None);
            };
            let ek: khive_types::EntityKind = kind
                .parse()
                .map_err(|_| RuntimeError::InvalidInput(format!("unknown entity kind {kind:?}")))?;
            let resolved = crate::entity_type_registry::EntityTypeRegistry::global()
                .resolve(ek, Some(raw))
                .map_err(RuntimeError::from)?;
            Ok(resolved.entity_type)
        });
        self.runtime.install_entity_type_validator(validator);
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
            "context" => self.handle_context(graph_token, params).await,
            "query" => self.handle_query(graph_token, params).await,
            "propose" => self.handle_propose(graph_token, params).await,
            "review" => self.handle_review(graph_token, params, registry).await,
            "withdraw" => self.handle_withdraw(graph_token, params).await,
            "stats" => self.handle_stats(graph_token, params).await,
            "resolve" => self.handle_resolve(graph_token, params, registry).await,
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

    /// #569 regression: `search(kind="note", ...)` must fail loud on a residual
    /// FTS5 metacharacter, exercised end to end through verb dispatch.
    ///
    /// `sanitize_fts5_query` (khive-db) strips known-unsafe characters like `$`,
    /// but by design stays minimal — it does not strip every character SQLite
    /// FTS5's bareword parser rejects. `@` is one residual character that still
    /// crashes the parser. `search_notes` (khive-runtime/operations.rs) now
    /// surfaces that FTS parser error as `RuntimeError::InvalidInput` instead
    /// of silently degrading to vector-only results (#569). This assertion
    /// fails against the pre-#569 fail-open behavior (which returned `Ok`
    /// here) and passes once the FTS leg fails closed.
    #[tokio::test]
    async fn handler_search_note_residual_fts5_char_fails_loud() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let tok = rt.authorize(Namespace::local()).unwrap();

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");
        let pack = KgPack::new(rt.clone());

        pack.dispatch(
            "create",
            json!({
                "kind": "observation",
                "content": "use foo@bar to chain calls"
            }),
            &registry,
            &tok,
        )
        .await
        .expect("note create must succeed");

        let result = pack
            .dispatch(
                "search",
                json!({
                    "kind": "note",
                    "query": "foo@bar",
                    "limit": 10
                }),
                &registry,
                &tok,
            )
            .await;

        assert!(
            result.is_err(),
            "#569 search(kind=\"note\") must fail loud on a residual FTS5 char ('@'), \
             not silently degrade to vector-only results, got: {:?}",
            result.ok()
        );
    }

    // ---- `resolve` verb (unified-verb draft ADR, Slice 1) ----
    //
    // Ring admission happens at the `VerbRegistry::dispatch_with_identity`
    // boundary (see `pack.rs`), not inside `KgPack::dispatch` — these tests
    // go through `registry.dispatch(verb, params)` (not the direct
    // `pack.dispatch(verb, params, &registry, &tok)` bypass most other tests
    // in this module use) specifically so the admission hook fires.

    /// A UUID ref resolves through the id-string passthrough stage,
    /// regardless of whether the entity was ever touched through this
    /// registry's ring — exercised end to end through verb dispatch.
    #[tokio::test]
    async fn resolve_id_string_passthrough_through_dispatch() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let created = registry
            .dispatch(
                "create",
                json!({"kind": "concept", "name": "ResolveIdTarget"}),
            )
            .await
            .expect("create must succeed");
        let id = created.get("id").and_then(|v| v.as_str()).unwrap();

        let result = registry
            .dispatch("resolve", json!({"refs": [id]}))
            .await
            .expect("resolve must succeed");
        let results = result.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].get("status").and_then(|v| v.as_str()),
            Some("resolved")
        );
        assert_eq!(results[0].get("id").and_then(|v| v.as_str()), Some(id));
        assert_eq!(
            results[0].get("confidence").and_then(|v| v.as_f64()),
            Some(1.0)
        );
    }

    /// The dispatch-boundary ring admits `create`'s returned id under its
    /// name; a later `resolve(refs=[name])` call by the SAME actor resolves
    /// it via the ring stage without ever running hybrid search over a
    /// matching name. Proves the ring, not just the id-string passthrough.
    #[tokio::test]
    async fn resolve_via_recently_referenced_ring_after_create() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let created = registry
            .dispatch(
                "create",
                json!({"kind": "concept", "name": "the old record"}),
            )
            .await
            .expect("create must succeed");
        let id = created.get("id").and_then(|v| v.as_str()).unwrap();

        let result = registry
            .dispatch("resolve", json!({"refs": ["the old record"]}))
            .await
            .expect("resolve must succeed");
        let results = result.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(
            results[0].get("status").and_then(|v| v.as_str()),
            Some("resolved"),
            "ring admission from `create` must let `resolve` find it by name: {results:?}"
        );
        assert_eq!(results[0].get("id").and_then(|v| v.as_str()), Some(id));
        assert_eq!(
            results[0].get("confidence").and_then(|v| v.as_f64()),
            Some(0.95),
            "an exact ring-name match must resolve at RING_EXACT_CONFIDENCE"
        );
    }

    /// Two entities admitted to the ring under the exact same name resolve
    /// as `Ambiguous`, never a silent pick (F7 of the unified-verb draft ADR).
    #[tokio::test]
    async fn resolve_ambiguous_on_duplicate_ring_names() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        for _ in 0..2 {
            registry
                .dispatch(
                    "create",
                    json!({"kind": "concept", "name": "duplicate ring name"}),
                )
                .await
                .expect("create must succeed");
        }

        let result = registry
            .dispatch("resolve", json!({"refs": ["duplicate ring name"]}))
            .await
            .expect("resolve must succeed");
        let results = result.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(
            results[0].get("status").and_then(|v| v.as_str()),
            Some("ambiguous")
        );
        let candidates = results[0]
            .get("candidates")
            .and_then(|v| v.as_array())
            .expect("ambiguous result must carry candidates");
        assert_eq!(candidates.len(), 2);
    }

    /// A ref that matches nothing in the ring or hybrid search is `NotFound`.
    #[tokio::test]
    async fn resolve_not_found_when_nothing_matches() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let result = registry
            .dispatch(
                "resolve",
                json!({"refs": ["nothing in this empty graph matches"]}),
            )
            .await
            .expect("resolve must succeed");
        let results = result.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(
            results[0].get("status").and_then(|v| v.as_str()),
            Some("not_found")
        );
    }

    /// `search` result-sets never admit to the ring (gate condition,
    /// 2026-07-09): an entity that only ever went through `search` (never
    /// `create`/`get`/`update`/`delete`/`merge`/`link` on THIS registry) must
    /// not resolve via the ring's high-confidence exact-match stage. It is
    /// created directly on the runtime (bypassing dispatch, hence bypassing
    /// admission entirely) so the only way `resolve` can find it via the ring
    /// is stage 2 — proven by a confidence that never equals the ring's fixed
    /// 0.95 exact-match / 0.7 substring-match bands (it instead comes from
    /// the stage-3 exact-name storage lookup, #849, since the ref is this
    /// entity's exact name).
    #[tokio::test]
    async fn resolve_search_result_sets_never_populate_the_ring() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let direct_tok = rt.authorize(Namespace::local()).unwrap();
        rt.create_entity(
            &direct_tok,
            "concept",
            None,
            "ring sparsity probe target",
            None,
            None,
            vec![],
        )
        .await
        .expect("direct entity create must succeed");

        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        // Search alone must never populate the ring.
        registry
            .dispatch(
                "search",
                json!({"kind": "entity", "query": "ring sparsity probe target"}),
            )
            .await
            .expect("search must succeed");

        let result = registry
            .dispatch("resolve", json!({"refs": ["ring sparsity probe target"]}))
            .await
            .expect("resolve must succeed");
        let results = result.get("results").and_then(|v| v.as_array()).unwrap();
        // The ref is this entity's exact name, so it now resolves via the
        // stage-3 exact-name storage lookup (#849) at EXACT_NAME_CONFIDENCE
        // (0.98) regardless of the ring — proving the ring itself stayed
        // empty: a ring hit would score 0.95 (exact) or 0.7 (substring),
        // never 0.98.
        assert_eq!(
            results[0].get("status").and_then(|v| v.as_str()),
            Some("resolved")
        );
        let confidence = results[0]
            .get("confidence")
            .and_then(|v| v.as_f64())
            .unwrap();
        assert_eq!(
            confidence, 0.98,
            "a resolved hit here must come from the exact-name storage lookup, \
             not the ring (which would score 0.95 exact / 0.7 substring); got {confidence}"
        );
    }

    /// Regression for the namespace-key mismatch (review finding, 2026-07-09
    /// fix round): ring admission used the gate-resolved `ns` (which is the
    /// configured `default_namespace`, e.g. `"lambda:leo"`, on the default
    /// dispatch path) while `resolve_reference`'s ring lookup used
    /// `token.namespace()` (always `"local"` on that same path, per ADR-007
    /// Rule 0/3b). With a non-local `default_namespace`, a `create` followed
    /// by a same-actor `resolve` on the same registry must still hit the
    /// ring — proving admission and lookup are keyed identically.
    #[tokio::test]
    async fn resolve_via_ring_survives_non_local_default_namespace() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("lambda:leo");
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let created = registry
            .dispatch(
                "create",
                json!({"kind": "concept", "name": "namespace key parity target"}),
            )
            .await
            .expect("create must succeed");
        let id = created.get("id").and_then(|v| v.as_str()).unwrap();

        let result = registry
            .dispatch("resolve", json!({"refs": ["namespace key parity target"]}))
            .await
            .expect("resolve must succeed");
        let results = result.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(
            results[0].get("status").and_then(|v| v.as_str()),
            Some("resolved"),
            "ring admission and lookup must key on the same namespace even when \
             default_namespace is non-local: {results:?}"
        );
        assert_eq!(results[0].get("id").and_then(|v| v.as_str()), Some(id));
        assert_eq!(
            results[0].get("confidence").and_then(|v| v.as_f64()),
            Some(0.95),
            "must resolve via the ring's exact-match stage, not fall through to search"
        );
    }

    /// Id-string passthrough is entity-scoped, identically for full UUIDs
    /// and short prefixes (review finding, 2026-07-09 fix round): a note's
    /// id-string is `NotFound` through `resolve`, even though `get` on the
    /// same id would succeed.
    #[tokio::test]
    async fn resolve_id_string_passthrough_is_entity_only() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let created = registry
            .dispatch(
                "create",
                json!({"kind": "note", "note_kind": "observation", "content": "a note body"}),
            )
            .await
            .expect("create must succeed");
        let id = created.get("id").and_then(|v| v.as_str()).unwrap();

        // `get` on the note id succeeds — sanity-checking the id is real.
        registry
            .dispatch("get", json!({"id": id}))
            .await
            .expect("get on the note id must succeed");

        let full_uuid_result = registry
            .dispatch("resolve", json!({"refs": [id]}))
            .await
            .expect("resolve must succeed");
        let full_uuid_results = full_uuid_result
            .get("results")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(
            full_uuid_results[0].get("status").and_then(|v| v.as_str()),
            Some("not_found"),
            "a note's full-UUID ref must not resolve through the entity-only \
             id-string passthrough: {full_uuid_results:?}"
        );

        let prefix = &id[..8];
        let prefix_result = registry
            .dispatch("resolve", json!({"refs": [prefix]}))
            .await
            .expect("resolve must succeed");
        let prefix_results = prefix_result
            .get("results")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(
            prefix_results[0].get("status").and_then(|v| v.as_str()),
            Some("not_found"),
            "a note's short-prefix ref must behave identically to its full UUID: \
             {prefix_results:?}"
        );
    }

    /// `resolve` is registered as a public verb and appears in `verbs()`.
    #[tokio::test]
    async fn resolve_appears_in_verbs_introspection() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let result = registry
            .dispatch("verbs", json!({}))
            .await
            .expect("verbs must succeed");
        let verbs = result.get("verbs").and_then(|v| v.as_array()).unwrap();
        let names: Vec<&str> = verbs
            .iter()
            .filter_map(|v| v.get("verb").and_then(|n| n.as_str()))
            .collect();
        assert!(
            names.contains(&"resolve"),
            "resolve must be registered as a public verb; got {names:?}"
        );
    }

    // ---- exact-name storage lookup (stage 3, #849) ----
    //
    // These entities are created directly on the runtime (bypassing
    // `registry.dispatch`, hence bypassing ring admission entirely — same
    // technique as `resolve_search_result_sets_never_populate_the_ring`), so
    // the only way `resolve` can find them is the exact-name storage lookup
    // (or, for the fallback-preserved case, hybrid search).

    /// An entity that already exists but was never referenced through this
    /// registry's ring resolves via the new exact-name storage lookup, at
    /// `EXACT_NAME_CONFIDENCE` (0.98) — above the ring's bands, below the
    /// absolute certainty of an id-string passthrough (1.0). Also regression
    /// coverage for the literal #849 repro: `kind="entity"` is the bare
    /// substrate label (no filter), not a literal `entities.kind` value.
    #[tokio::test]
    async fn resolve_exact_name_hit_resolves_high_confidence() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let direct_tok = rt.authorize(Namespace::local()).unwrap();
        let entity = rt
            .create_entity(&direct_tok, "concept", None, "RoLoRA", None, None, vec![])
            .await
            .expect("direct entity create must succeed");

        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let result = registry
            .dispatch("resolve", json!({"refs": ["RoLoRA"], "kind": "entity"}))
            .await
            .expect("resolve must succeed");
        let results = result.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(
            results[0].get("status").and_then(|v| v.as_str()),
            Some("resolved"),
            "an existing exact name must resolve even when never referenced \
             through this session's ring: {results:?}"
        );
        assert_eq!(
            results[0].get("id").and_then(|v| v.as_str()),
            Some(entity.id.to_string().as_str())
        );
        assert_eq!(
            results[0].get("confidence").and_then(|v| v.as_f64()),
            Some(0.98)
        );
    }

    /// Exact-name storage lookup is Unicode-safe and does not tokenize names.
    /// CJK and embedded spaces therefore resolve with the same confidence as
    /// an ASCII single-token name.
    #[tokio::test]
    async fn resolve_exact_name_handles_cjk_and_spaces() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let direct_tok = rt.authorize(Namespace::local()).unwrap();
        let cjk = rt
            .create_entity(
                &direct_tok,
                "concept",
                None,
                "知识 图谱",
                None,
                None,
                vec![],
            )
            .await
            .expect("direct CJK entity create must succeed");
        let spaced = rt
            .create_entity(
                &direct_tok,
                "concept",
                None,
                "Entity Name With Spaces",
                None,
                None,
                vec![],
            )
            .await
            .expect("direct spaced-name entity create must succeed");

        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let result = registry
            .dispatch(
                "resolve",
                json!({"refs": ["知识 图谱", "Entity Name With Spaces"], "kind": "entity"}),
            )
            .await
            .expect("resolve must succeed");
        let results = result.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(results.len(), 2);
        for (result, expected_id) in results.iter().zip([cjk.id, spaced.id]) {
            assert_eq!(
                result.get("status").and_then(|v| v.as_str()),
                Some("resolved")
            );
            assert_eq!(
                result.get("id").and_then(|v| v.as_str()),
                Some(expected_id.to_string().as_str())
            );
            assert_eq!(
                result.get("confidence").and_then(|v| v.as_f64()),
                Some(0.98)
            );
        }

        let whitespace_variant = registry
            .dispatch(
                "resolve",
                json!({"refs": ["Entity  Name With Spaces"], "kind": "entity"}),
            )
            .await
            .expect("whitespace-variant resolve must succeed");
        let whitespace_results = whitespace_variant
            .get("results")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_ne!(
            whitespace_results[0]
                .get("confidence")
                .and_then(|v| v.as_f64()),
            Some(0.98),
            "interior whitespace must be preserved by exact-name lookup: {whitespace_results:?}"
        );
    }

    /// The storage exact-name tier is case-sensitive. A case-only variant
    /// can still resolve through case-insensitive hybrid search, but it must
    /// not receive the exact tier's 0.98 confidence.
    #[tokio::test]
    async fn resolve_case_variant_uses_hybrid_fallback() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let direct_tok = rt.authorize(Namespace::local()).unwrap();
        let entity = rt
            .create_entity(
                &direct_tok,
                "concept",
                None,
                "Case Sensitive Entity",
                None,
                None,
                vec![],
            )
            .await
            .expect("direct entity create must succeed");

        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let result = registry
            .dispatch("resolve", json!({"refs": ["case sensitive entity"]}))
            .await
            .expect("resolve must succeed");
        let results = result.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(
            results[0].get("status").and_then(|v| v.as_str()),
            Some("resolved"),
            "case-only variant should remain discoverable through hybrid search: {results:?}"
        );
        assert_eq!(
            results[0].get("id").and_then(|v| v.as_str()),
            Some(entity.id.to_string().as_str())
        );
        assert!(
            results[0]
                .get("confidence")
                .and_then(|v| v.as_f64())
                .is_some_and(|confidence| confidence < 0.98),
            "case-only variant must not be reported as an exact-name hit: {results:?}"
        );
    }

    /// Two entities sharing the exact same name resolve as `Ambiguous`,
    /// never a silent pick, mirroring the ring's duplicate-name contract.
    #[tokio::test]
    async fn resolve_exact_name_ambiguous_on_duplicate_names() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let direct_tok = rt.authorize(Namespace::local()).unwrap();
        for _ in 0..2 {
            rt.create_entity(
                &direct_tok,
                "concept",
                None,
                "duplicate exact name",
                None,
                None,
                vec![],
            )
            .await
            .expect("direct entity create must succeed");
        }

        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let result = registry
            .dispatch("resolve", json!({"refs": ["duplicate exact name"]}))
            .await
            .expect("resolve must succeed");
        let results = result.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(
            results[0].get("status").and_then(|v| v.as_str()),
            Some("ambiguous")
        );
        let candidates = results[0]
            .get("candidates")
            .and_then(|v| v.as_array())
            .expect("ambiguous result must carry candidates");
        assert_eq!(candidates.len(), 2);
    }

    /// A ref with no exact-name storage match still falls through to the
    /// hybrid-search fallback (existing stage-4 behavior preserved): a
    /// partial-phrase query that cannot exact-match any name resolves via
    /// search, at a confidence below the exact-name stage's 0.98.
    #[tokio::test]
    async fn resolve_exact_name_miss_falls_through_to_hybrid_search() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let direct_tok = rt.authorize(Namespace::local()).unwrap();
        let entity = rt
            .create_entity(
                &direct_tok,
                "concept",
                None,
                "Existing Multi Word Fallback Target",
                None,
                None,
                vec![],
            )
            .await
            .expect("direct entity create must succeed");

        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let result = registry
            .dispatch("resolve", json!({"refs": ["Multi Word Fallback Target"]}))
            .await
            .expect("resolve must succeed");
        let results = result.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(
            results[0].get("status").and_then(|v| v.as_str()),
            Some("resolved"),
            "a non-exact phrase must still resolve via the preserved hybrid \
             fallback: {results:?}"
        );
        assert_eq!(
            results[0].get("id").and_then(|v| v.as_str()),
            Some(entity.id.to_string().as_str())
        );
        let confidence = results[0]
            .get("confidence")
            .and_then(|v| v.as_f64())
            .unwrap();
        assert!(
            confidence < 0.98,
            "a hybrid-search resolution must score below the exact-name \
             stage's confidence, proving it came from stage 4 not stage 3; \
             got {confidence}"
        );
    }

    /// A soft-deleted entity is invisible to the exact-name storage lookup
    /// (`deleted_at IS NULL` is baked into `query_entities`), matching the
    /// rest of the KG surface's soft-delete contract.
    #[tokio::test]
    async fn resolve_exact_name_soft_deleted_entity_not_matched() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let direct_tok = rt.authorize(Namespace::local()).unwrap();
        let entity = rt
            .create_entity(
                &direct_tok,
                "concept",
                None,
                "soon to be deleted",
                None,
                None,
                vec![],
            )
            .await
            .expect("direct entity create must succeed");
        rt.delete_entity(&direct_tok, entity.id, false)
            .await
            .expect("soft delete must succeed");

        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let result = registry
            .dispatch("resolve", json!({"refs": ["soon to be deleted"]}))
            .await
            .expect("resolve must succeed");
        let results = result.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(
            results[0].get("status").and_then(|v| v.as_str()),
            Some("not_found"),
            "a soft-deleted entity must not resolve by its old exact name: {results:?}"
        );
    }

    /// A granular `kind` filter narrows the exact-name lookup: two entities
    /// with the exact same name but different entity kinds resolve
    /// deterministically to the one matching `kind`, instead of `Ambiguous`.
    #[tokio::test]
    async fn resolve_exact_name_respects_kind_filter() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let direct_tok = rt.authorize(Namespace::local()).unwrap();
        let concept = rt
            .create_entity(
                &direct_tok,
                "concept",
                None,
                "shared exact name",
                None,
                None,
                vec![],
            )
            .await
            .expect("direct entity create must succeed");
        rt.create_entity(
            &direct_tok,
            "document",
            None,
            "shared exact name",
            None,
            None,
            vec![],
        )
        .await
        .expect("direct entity create must succeed");

        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let result = registry
            .dispatch(
                "resolve",
                json!({"refs": ["shared exact name"], "kind": "concept"}),
            )
            .await
            .expect("resolve must succeed");
        let results = result.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(
            results[0].get("status").and_then(|v| v.as_str()),
            Some("resolved"),
            "kind=\"concept\" must narrow the two same-name entities down to \
             one match instead of surfacing Ambiguous: {results:?}"
        );
        assert_eq!(
            results[0].get("id").and_then(|v| v.as_str()),
            Some(concept.id.to_string().as_str())
        );
    }

    /// `resolve`'s `kind` param is entity-only, matching the id-string
    /// passthrough and ring stages: a note kind is rejected with a clear
    /// error rather than silently over-filtering to zero matches.
    #[tokio::test]
    async fn resolve_kind_rejects_non_entity_kind() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let result = registry
            .dispatch("resolve", json!({"refs": ["anything"], "kind": "note"}))
            .await;
        assert!(
            result.is_err(),
            "resolve(kind=\"note\") must fail loud, not silently over-filter"
        );
    }
}
