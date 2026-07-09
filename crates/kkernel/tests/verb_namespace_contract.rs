//! Contract test: every non-kg verb must be namespaced as `<pack>.<verb>`.
//!
//! The kg substrate pack owns the 17 bare verb names (create, get, list, …).
//! Every other pack must prefix its verbs with the pack name followed by a
//! single dot: `memory.recall`, `gtd.assign`, etc. Sub-variants use a single
//! additional underscore-delimited segment, NOT a second dot:
//! `memory.recall_embed`, not `memory.recall.embed`.
//!
//! This test walks every `HandlerDef` across every pack registered in the
//! `inventory` (i.e. linked into this test binary) and asserts:
//!   1. A name without a dot must be in the kg-substrate allowlist.
//!   2. A name with exactly one dot must have a prefix equal to `Pack::NAME`
//!      (validated via `all_handlers_with_names`).
//!   3. A name with two or more dots is always invalid — sub-variants use
//!      underscore, not nesting dots.
//!
//! Failures list every offending handler name so a single CI run surfaces all
//! violations rather than stopping at the first.

use khive_runtime::pack::{PackRegistry, VerbRegistryBuilder};
use khive_runtime::{KhiveRuntime, RuntimeConfig};

// Force all pack crates into the binary so their `inventory::submit!` blocks run.
// This mirrors the force-link block in kkernel::lib — the test binary is a separate
// linking unit and needs its own anchors.
#[allow(unused_imports)]
use khive_pack_brain::BrainPack as _;
#[allow(unused_imports)]
use khive_pack_comm::CommPack as _;
#[allow(unused_imports)]
use khive_pack_gtd::GtdPack as _;
#[allow(unused_imports)]
use khive_pack_kg::KgPack as _;
#[allow(unused_imports)]
use khive_pack_knowledge::KnowledgePack as _;
#[allow(unused_imports)]
use khive_pack_memory::MemoryPack as _;
#[allow(unused_imports)]
use khive_pack_schedule::SchedulePack as _;

/// Bare verb names owned by the kg substrate pack. These are the only names
/// permitted to omit the `<pack>.` prefix.
///
/// The 18 entries cover CRUD + graph + curation + proposal primitives, plus
/// `stats` for aggregate namespace metrics, `verbs` for verb-registry
/// introspection (J-help PR #464), `context` for entity-anchored graph
/// context in one call (ADR-089), and `resolve` for reference resolution (S1).
const KG_SUBSTRATE_VERBS: &[&str] = &[
    "create",
    "get",
    "list",
    "stats",
    "update",
    "delete",
    "search",
    "link",
    "neighbors",
    "traverse",
    "query",
    "merge",
    "propose",
    "review",
    "withdraw",
    "verbs",
    "context",
    "resolve",
];

fn build_full_registry() -> Vec<(String, String)> {
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: khive_runtime::Namespace::parse("verb-contract-test")
            .unwrap_or_else(|_| khive_runtime::Namespace::local()),
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("runtime for contract test");
    let mut builder = VerbRegistryBuilder::new();
    let names: Vec<String> = PackRegistry::discovered_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    PackRegistry::register_packs(&names, runtime, &mut builder)
        .expect("all inventory packs must register cleanly");
    let registry = builder.build().expect("VerbRegistry build");
    registry
        .all_handlers_with_names()
        .into_iter()
        .map(|(pack_name, handler)| (pack_name.to_string(), handler.name.to_string()))
        .collect()
}

/// Every non-kg verb name must carry exactly one dot-prefix matching the pack
/// name that owns it.
#[test]
fn every_non_kg_verb_is_namespaced() {
    let handlers = build_full_registry();

    let mut violations: Vec<String> = Vec::new();

    for (pack_name, verb_name) in &handlers {
        let dot_count = verb_name.chars().filter(|&c| c == '.').count();

        match dot_count {
            // No dot — must be an allowed kg substrate verb.
            0 => {
                if !KG_SUBSTRATE_VERBS.contains(&verb_name.as_str()) {
                    violations.push(format!(
                        "pack={pack_name:?} verb={verb_name:?}: bare name is not in the \
                         kg-substrate allowlist. Add `{pack_name}.` prefix."
                    ));
                }
            }
            // Exactly one dot — prefix must match the pack name.
            1 => {
                let prefix = verb_name.split('.').next().unwrap_or("");
                if prefix != pack_name {
                    violations.push(format!(
                        "pack={pack_name:?} verb={verb_name:?}: prefix {prefix:?} does not \
                         match pack name {pack_name:?}."
                    ));
                }
            }
            // Two or more dots — always invalid (sub-variants use underscore, not nesting dots).
            _ => {
                violations.push(format!(
                    "pack={pack_name:?} verb={verb_name:?}: name contains {dot_count} dots; \
                     sub-variants must use underscore, not nested dots. \
                     Example: `{pack_name}.recall_embed`, not `{pack_name}.recall.embed`."
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Verb namespace contract violations:\n{}",
        violations.join("\n")
    );
}

/// Complementary check: the kg substrate pack must expose all 17 mandated bare
/// verbs and no dotted ones. This catches regressions in the kg pack itself.
#[test]
fn kg_pack_exposes_bare_verbs_only() {
    let handlers = build_full_registry();

    let kg_verbs: Vec<&str> = handlers
        .iter()
        .filter(|(pack, _)| pack == "kg")
        .map(|(_, verb)| verb.as_str())
        .collect();

    // Every kg-substrate allowlist name must be present.
    let missing: Vec<&&str> = KG_SUBSTRATE_VERBS
        .iter()
        .filter(|v| !kg_verbs.contains(v))
        .collect();
    assert!(
        missing.is_empty(),
        "kg pack is missing substrate verbs: {missing:?}"
    );

    // No kg verb may carry a dot.
    let dotted: Vec<&&str> = kg_verbs.iter().filter(|v| v.contains('.')).collect();
    assert!(
        dotted.is_empty(),
        "kg pack must not use dotted verb names; found: {dotted:?}"
    );
}
