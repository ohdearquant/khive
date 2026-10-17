//! Runtime hooks installed after pack construction and before serving.

use super::*;

pub(super) fn initialize_runtime_hooks(
    registry: &khive_runtime::VerbRegistry,
    default_runtime: &KhiveRuntime,
    per_pack_runtimes_local: &HashMap<String, KhiveRuntime>,
) -> khive_runtime::RuntimeResult<()> {
    default_runtime.install_edge_rules(registry.all_edge_rules());
    for rt in per_pack_runtimes_local.values() {
        rt.install_edge_rules(registry.all_edge_rules());
    }
    let mut embedding_runtimes = vec![default_runtime];
    embedding_runtimes.extend(per_pack_runtimes_local.values());
    registry.initialize_embedding_engines(&embedding_runtimes)?;
    registry.call_register_entity_type_validators(default_runtime);
    // #2943: install entity-kind update hooks (same scope/timing as the
    // entity-type validator above — entities live on the shared/main graph,
    // reached through `core()`, never on a per-pack secondary backend).
    default_runtime.install_entity_kind_hooks(registry.entity_kind_hooks());
    // #750: install pack-owned note-mutation hooks (currently
    // only khive-pack-memory's warm-ANN-cache invalidation) so KG's
    // update/delete verbs notify caching packs even though there is no
    // crate-level dependency between them.
    registry.call_register_note_mutation_hooks(default_runtime);
    registry.call_register_note_search_ann_providers(default_runtime);
    for rt in per_pack_runtimes_local.values() {
        registry.call_register_note_search_ann_providers(rt);
    }
    // Note-write identity: install the pack-owned kind set and the pack-owned
    // note-write validator so identity properties are derived at the write and
    // preserved through merge/update on every path, including the ones that
    // reach no pack verb. Each per-pack runtime is constructed independently
    // in the multi-backend boot path (unlike the single-backend
    // `KhiveMcpServer::with_packs` path), so both must be installed on every
    // runtime that could actually serve a generic `create`/`update`/`merge`
    // for a pack-owned kind, not just `default_runtime`.
    let owned_note_kinds: Vec<String> = registry
        .pack_owned_note_kinds()
        .into_iter()
        .map(str::to_string)
        .collect();
    default_runtime.install_pack_owned_note_kinds(owned_note_kinds.clone());
    let note_embedding_policies = registry.all_note_embedding_policies();
    default_runtime.install_note_embedding_policies(&note_embedding_policies);
    for rt in per_pack_runtimes_local.values() {
        rt.install_pack_owned_note_kinds(owned_note_kinds.clone());
        rt.install_note_embedding_policies(&note_embedding_policies);
    }
    // The validator is installed on every runtime the kind list reaches, not
    // just the default: each per-pack runtime is built independently, so none
    // of them shares the default's validator slot, and `core()` clones the
    // secondary's own slots rather than the default's. A runtime that has the
    // kind list but no validator enforces half the rule.
    registry.call_register_note_write_validators(default_runtime);
    for rt in per_pack_runtimes_local.values() {
        registry.call_register_note_write_validators(rt);
    }

    Ok(())
}
