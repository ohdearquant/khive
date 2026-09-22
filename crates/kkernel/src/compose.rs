//! Host-composition entry point (ADR-191 D6, ADR-192 S4).
//!
//! `kkernel`'s own linked pack set is discovered at link time through
//! `inventory` (`crates/kkernel/src/lib.rs`'s `_pack_links` force-link
//! anchor). A pack compiled outside this repository against a pinned
//! revision — the D6 extension seam — has no presence in that inventory
//! short of its own host binary's own force-link anchor, and S4's
//! credential providers and request hooks are composed the same way: "a
//! downstream composition is one function call: base packs, extra packs,
//! credential providers, request hooks." This module is that one function
//! call for pack factories: it constructs a `VerbRegistry` from kkernel's
//! linked set plus caller-supplied `&'static dyn PackFactory` values,
//! exactly as `khive-mcp/src/serve.rs`'s multi-backend boot path constructs
//! one from the linked set alone
//! (`khive_runtime::PackRegistry::register_packs_with_runtimes`) — this
//! entry point is a thin wrapper over the sibling function that also
//! accepts extras
//! (`khive_runtime::PackRegistry::register_packs_with_runtimes_with_extra_factories`).

use std::collections::HashMap;

use khive_runtime::{
    KhiveRuntime, PackFactory, PackRegistry, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};

/// Build a `VerbRegistry` from `names`, resolving each against kkernel's
/// linked pack set plus `extra_factories`.
///
/// `runtimes` maps a pack name to the `KhiveRuntime` it should write
/// through (one per configured backend, mirroring
/// `resolve_pack_backend_config`'s per-pack routing); `default_runtime`
/// covers any name absent from that map, and also supplies the registry's
/// gate, default namespace, visible namespaces, and actor id — the same
/// fields `khive_runtime::PackRegistry::build_ingest_registry` reads off a
/// single runtime for its own one-shot registry construction. The returned
/// registry's aggregated edge rules are installed back onto `default_runtime`
/// and onto every runtime in `runtimes`, mirroring
/// `build_registry_for_multi_backend_inner`'s post-`build()` step in
/// `khive-mcp/src/serve.rs`.
///
/// An inventory-discovered pack always wins a name collision with an
/// `extra_factories` entry — see
/// `register_packs_with_runtimes_with_extra_factories` for the full
/// collision rule.
pub fn compose_registry_with_extra_packs(
    names: &[String],
    runtimes: &HashMap<String, KhiveRuntime>,
    default_runtime: &KhiveRuntime,
    extra_factories: &[&'static dyn PackFactory],
) -> Result<VerbRegistry, RuntimeError> {
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(default_runtime.config().gate.clone());
    builder.with_default_namespace(default_runtime.config().default_namespace.as_str());
    builder.with_visible_namespaces(default_runtime.config().visible_namespaces.clone());
    builder.with_actor_id(default_runtime.config().actor_id.clone());

    PackRegistry::register_packs_with_runtimes_with_extra_factories(
        extra_factories,
        names,
        runtimes,
        default_runtime,
        &mut builder,
    )
    .map_err(|e| RuntimeError::Internal(format!("pack registration failed: {e:?}")))?;

    let registry = builder.build()?;

    default_runtime.install_edge_rules(registry.all_edge_rules());
    for rt in runtimes.values() {
        rt.install_edge_rules(registry.all_edge_rules());
    }

    Ok(registry)
}
