//! `kkernel pack list` and `kkernel pack handler` — introspection over
//! registered packs.
//!
//! Both subcommands operate on a `VerbRegistry` built from the active pack
//! set. They return data — JSON for machines, a table for humans — without
//! invoking any handler.
//!
//! ADR-076 establishes that pack registration eventually lives in the kernel.
//! For now both binaries collect the same set via `inventory!`; this module
//! consumes whatever is registered and prints it.

use anyhow::{anyhow, Context, Result};
use khive_runtime::pack::{PackRegistry, VerbRegistry, VerbRegistryBuilder};
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use serde::Serialize;

/// Description of a single registered verb.
#[derive(Debug, Serialize)]
pub struct VerbInfo {
    pub name: String,
    pub description: String,
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
fn build_registry() -> Result<(VerbRegistry, KhiveRuntime)> {
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: "kkernel-introspect".to_string(),
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
        // ADR-024 requires 11 KG verbs
        assert_eq!(
            info.verbs.len(),
            11,
            "kg pack must expose 11 verbs (ADR-024); got {}: {:?}",
            info.verbs.len(),
            info.verbs.iter().map(|v| &v.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn pack_handler_unknown_returns_none() {
        let info = pack_handler("does_not_exist").unwrap();
        assert!(info.is_none(), "unknown pack returns None, not Err");
    }
}
