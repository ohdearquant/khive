//! `kkernel pack list` and `kkernel pack handler` — introspection over
//! registered packs.
//!
//! Both subcommands operate on a `VerbRegistry` built from the active pack
//! set. They return data — JSON for machines, a table for humans — without
//! invoking any handler.
//!
//! Pack registration uses dynamic self-registration via `inventory!`. This
//! module consumes whatever is registered and prints it.

use anyhow::{anyhow, Context, Result};
use khive_runtime::pack::{PackRegistry, VerbRegistry, VerbRegistryBuilder, Visibility};
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use serde::Serialize;

/// Visibility tier of a registered handler.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VerbVisibility {
    /// Externally invokable — surfaced on the MCP `request` tool wire.
    Verb,
    /// Internal pipeline step — addressable via the DSL but NOT on the MCP wire.
    Subhandler,
}

impl From<&Visibility> for VerbVisibility {
    fn from(v: &Visibility) -> Self {
        match v {
            Visibility::Verb => VerbVisibility::Verb,
            Visibility::Subhandler => VerbVisibility::Subhandler,
        }
    }
}

/// Description of a single registered handler.
///
/// Includes `visibility` and `category` alongside `name` and `description`
/// so introspection clients can distinguish MCP-exposed verbs from internal
/// subhandlers and surface speech-act classification.
#[derive(Debug, Serialize)]
pub struct VerbInfo {
    pub name: String,
    pub description: String,
    pub visibility: VerbVisibility,
    pub category: String,
}

/// Description of a single registered pack.
#[derive(Debug, Serialize)]
pub struct PackInfo {
    pub name: String,
    pub note_kinds: Vec<String>,
    pub entity_kinds: Vec<String>,
    pub requires: Vec<String>,
    pub verbs: Vec<VerbInfo>,
}

/// Build an in-memory introspection registry containing every discoverable
/// pack. Returns `(registry, runtime)` so the caller can hold the runtime
/// alive for the duration of the introspection call.
///
/// # Strict-actor-mode exemption
///
/// This function does NOT call `enforce_strict_actor_mode`. That enforcement
/// seam protects the **comm dispatch boundary** — it prevents a server from
/// silently accepting comm operations without a configured actor identity.
/// `build_registry` is metadata/introspection-only: it enumerates verb names,
/// note kinds, and entity kinds from the registered packs without ever
/// dispatching a verb or reading comm/tenant data. There is no tenant-isolation
/// risk here, so requiring an actor identity would make `kkernel pack list`
/// and `kkernel pack handler` fail under `KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1`
/// without any security benefit — an operator must be able to introspect a
/// strict-mode deployment. See `enforce_strict_actor_mode` in
/// `crates/khive-mcp/src/serve.rs` for the authoritative boundary definition.
fn build_registry() -> Result<(VerbRegistry, KhiveRuntime)> {
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: khive_runtime::Namespace::parse("kkernel-introspect")
            .unwrap_or_else(|_| khive_runtime::Namespace::local()),
        embedding_model: None,
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).context("building introspection runtime")?;
    let mut builder = VerbRegistryBuilder::new();
    let names: Vec<String> = PackRegistry::discovered_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    PackRegistry::register_packs(&names, runtime.clone(), &mut builder)
        .map_err(|n| anyhow!("pack {n:?} declared in inventory but factory missing"))?;
    let registry = builder.build().context("building VerbRegistry")?;
    Ok((registry, runtime))
}

fn pack_info_from_registry(registry: &VerbRegistry, name: &str) -> Option<PackInfo> {
    // pack_verbs returns None if name isn't registered — gate everything off it.
    let verbs = registry.pack_verbs(name)?;
    Some(PackInfo {
        name: name.to_string(),
        note_kinds: registry
            .pack_note_kinds(name)
            .unwrap_or(&[])
            .iter()
            .map(|s| s.to_string())
            .collect(),
        entity_kinds: registry
            .pack_entity_kinds(name)
            .unwrap_or(&[])
            .iter()
            .map(|s| s.to_string())
            .collect(),
        requires: registry
            .pack_requires(name)
            .unwrap_or(&[])
            .iter()
            .map(|s| s.to_string())
            .collect(),
        verbs: verbs
            .iter()
            .map(|v| VerbInfo {
                name: v.name.to_string(),
                description: v.description.to_string(),
                visibility: VerbVisibility::from(&v.visibility),
                category: format!("{:?}", v.category),
            })
            .collect(),
    })
}

/// Enumerate all registered packs and their full surface.
pub fn list_packs() -> Result<Vec<PackInfo>> {
    let (registry, _runtime) = build_registry()?;
    let names: Vec<String> = registry
        .pack_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    Ok(names
        .iter()
        .filter_map(|n| pack_info_from_registry(&registry, n))
        .collect())
}

/// Return the full handler surface for one pack — its verbs with descriptions,
/// note kinds, entity kinds, and required pack dependencies.
///
/// Returns `Ok(None)` if no pack with `name` is registered.
pub fn pack_handler(name: &str) -> Result<Option<PackInfo>> {
    let (registry, _runtime) = build_registry()?;
    Ok(pack_info_from_registry(&registry, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Regression: introspection registry construction MUST succeed under
    /// `KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1` with the `comm` pack registered and
    /// no actor identity configured. `build_registry` is exempt from the
    /// strict-actor enforcement seam because it is metadata-only and never
    /// dispatches verbs — requiring an actor would make `kkernel pack list`
    /// unusable against a strict-mode deployment with zero security benefit.
    ///
    /// If this test ever fails it means `enforce_strict_actor_mode` was
    /// accidentally wired into the introspection path — that is a usability
    /// regression, not a security improvement.
    #[test]
    #[serial]
    fn introspection_registry_builds_under_strict_mode_without_actor() {
        let prev = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");

        let result = build_registry();

        match prev {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }

        assert!(
            result.is_ok(),
            "build_registry (introspection-only) must succeed under \
             KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1 + no actor: strict-actor enforcement \
             applies only to dispatch paths, not introspection. Got: {:?}",
            result.err()
        );

        // Confirm the registry is functional: comm pack must appear in the surface
        // even under strict mode.
        let (registry, _runtime) = result.unwrap();
        let pack_names: Vec<&str> = registry.pack_names().into_iter().collect();
        assert!(
            pack_names.contains(&"comm"),
            "comm pack must be present in introspection registry under strict mode; \
             got: {pack_names:?}"
        );
    }

    #[test]
    fn list_packs_returns_at_least_kg() {
        let packs = list_packs().expect("list_packs succeeds");
        assert!(!packs.is_empty(), "at least one pack must register");
        let names: Vec<&str> = packs.iter().map(|p| p.name.as_str()).collect();
        assert!(
            names.contains(&"kg"),
            "kg pack must be registered; got {names:?}"
        );
    }

    #[test]
    fn pack_handler_for_kg_returns_full_surface() {
        let info = pack_handler("kg")
            .expect("pack_handler succeeds")
            .expect("kg pack must exist");
        assert_eq!(info.name, "kg");
        assert!(
            !info.verbs.is_empty(),
            "kg pack must expose verbs; got {:?}",
            info.verbs
        );
        // kg pack ships 20 verbs: 11 base + propose/review/withdraw (3) + verbs
        // + stats (2) + context (1, ADR-089) + resolve (1) + whoami (1)
        // + db_diagnostics (1, ADR-091)
        assert_eq!(
            info.verbs.len(),
            20,
            "kg pack must expose 20 verbs; got {}: {:?}",
            info.verbs.len(),
            info.verbs.iter().map(|v| &v.name).collect::<Vec<_>>()
        );
        assert!(
            info.verbs.iter().any(|v| v.name == "db_diagnostics"),
            "kg pack must expose db_diagnostics; got {:?}",
            info.verbs.iter().map(|v| &v.name).collect::<Vec<_>>()
        );
        // F126: VerbInfo must include visibility and category fields.
        let create = info.verbs.iter().find(|v| v.name == "create").unwrap();
        assert_eq!(
            create.visibility,
            VerbVisibility::Verb,
            "kg create must have Verb visibility"
        );
        assert!(
            !create.category.is_empty(),
            "kg create must have a non-empty category"
        );
    }

    #[test]
    fn memory_pack_subhandlers_carry_subhandler_visibility() {
        let info = pack_handler("memory")
            .expect("pack_handler succeeds")
            .expect("memory pack must exist");
        // recall.embed, recall.candidates, recall.fuse, recall.score are Subhandler.
        let subhandlers: Vec<&VerbInfo> = info
            .verbs
            .iter()
            .filter(|v| v.visibility == VerbVisibility::Subhandler)
            .collect();
        assert!(
            !subhandlers.is_empty(),
            "memory pack must have subhandler entries; got none in {:?}",
            info.verbs.iter().map(|v| &v.name).collect::<Vec<_>>()
        );
        // memory.recall_embed must be a subhandler.
        let embed = info
            .verbs
            .iter()
            .find(|v| v.name == "memory.recall_embed")
            .expect("memory.recall_embed must be in the handler list");
        assert_eq!(
            embed.visibility,
            VerbVisibility::Subhandler,
            "memory.recall_embed must have Subhandler visibility (F119)"
        );
    }

    #[test]
    fn pack_handler_unknown_returns_none() {
        let info = pack_handler("does_not_exist").unwrap();
        assert!(info.is_none(), "unknown pack returns None, not Err");
    }

    /// Every `uuid`/`array of uuid` parameter on every REAL registered
    /// handler (every pack linked into this binary via `inventory!`
    /// self-registration, not a synthetic test pack) must declare a
    /// resolution mode other than `IdResolutionMode::NotApplicable`.
    ///
    /// `khive-runtime`'s own unit tests can only exercise a synthetic pack —
    /// it cannot depend on `khive-pack-kg`/`khive-pack-gtd`/etc. without a
    /// circular dependency. `kkernel` is the first crate in the dependency
    /// graph that links every default pack, so this is where a real
    /// coverage gap becomes visible: a new `uuid` param added to any pack
    /// with no `resolution_mode` set (leaving the struct's zero-value
    /// `NotApplicable`) fails HERE, not in a hand-picked fixture.
    #[test]
    fn every_uuid_param_across_every_registered_pack_declares_a_resolution_mode() {
        let (registry, _runtime) = build_registry().expect("introspection registry builds");
        let mut missing: Vec<String> = Vec::new();
        for (pack_name, handler) in registry.all_handlers_with_names() {
            for param in handler.params.iter() {
                let is_id_typed = param.param_type == "uuid" || param.param_type == "array of uuid";
                if is_id_typed
                    && param.resolution_mode == khive_runtime::IdResolutionMode::NotApplicable
                {
                    missing.push(format!(
                        "{pack_name}.{}::{} (param_type={:?})",
                        handler.name, param.name, param.param_type
                    ));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "every uuid/array-of-uuid parameter must declare a non-NotApplicable \
             IdResolutionMode so describe_verb renders an accurate contract instead of \
             silently omitting one; missing on: {missing:?}"
        );
    }

    /// Companion to the coverage check above: a mode-appropriate contract
    /// must actually be rendered into `describe_verb`'s params array for a
    /// representative primary-scoped verb from each of the four non-Unscoped
    /// modes, proving the rendering path (not just the declared field) is
    /// wired correctly end to end against real pack definitions.
    #[test]
    fn describe_verb_renders_mode_specific_contract_for_real_handlers() {
        let (registry, _runtime) = build_registry().expect("introspection registry builds");

        let cases: &[(&str, &str, &str)] = &[
            // UnscopedById
            ("get", "id", "no namespace filter"),
            // PrefixScopedToPrimary
            (
                "neighbors",
                "node_id",
                "no namespace check performed by this resolver",
            ),
            // FullAndPrefixScopedToPrimary
            ("review", "id", "both a full"),
            // FullUuidOnlyScopedToPrimary
            (
                "propose",
                "parent_id",
                "rejected outright because this field stores",
            ),
            // UnscopedFullUuidOnly
            (
                "memory.feedback",
                "target_id",
                "no namespace check is performed on this parameter itself",
            ),
        ];

        for (verb, param_name, expected_fragment) in cases {
            let result = registry
                .describe_verb(verb)
                .unwrap_or_else(|e| panic!("describe_verb({verb:?}) must succeed: {e}"));
            let params = result["params"]
                .as_array()
                .unwrap_or_else(|| panic!("{verb:?} help envelope must carry a params array"));
            let param = params
                .iter()
                .find(|p| p["name"] == *param_name)
                .unwrap_or_else(|| panic!("{verb:?} must declare param {param_name:?}"));
            let description = param["description"]
                .as_str()
                .expect("description must be a string");
            assert!(
                description.contains(expected_fragment),
                "{verb:?}.{param_name} description must contain {expected_fragment:?} for its \
                 declared resolution mode; got: {description}"
            );
        }
    }
}
