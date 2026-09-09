//! Contract test: every non-kg verb must be namespaced as `<pack>.<verb>`.
//!
//! The kg substrate pack owns the bare verb names (create, get, list, …).
//! Every other pack must prefix its verbs with the pack name followed by a
//! single dot: `memory.recall`, `gtd.assign`, etc. Sub-variants use a single
//! additional underscore-delimited segment, NOT a second dot:
//! `memory.recall_embed`, not `memory.recall.embed`.
//!
//! One documented exception: the kg pack also carries the `stream`
//! sub-namespace (`stream.append`, `stream.read`, `stream.stat`). ADR-174
//! section 2 registers those three verbs under kg because the entries are kg
//! notes and the append writes the note and the ledger row in one writer
//! transaction, which a separate pack could only do by reaching into kg's
//! write path. The exception is pack-scoped and closed: kg may carry exactly
//! the sub-namespaces listed in `KG_SUB_NAMESPACES` and nothing else; no
//! other pack may borrow a prefix that is not its own name.
//!
//! This test walks every `HandlerDef` across every pack registered in the
//! `inventory` (i.e. linked into this test binary) and asserts:
//!   1. A name without a dot must be in the kg-substrate allowlist.
//!   2. A name with exactly one dot must have a prefix equal to `Pack::NAME`
//!      (validated via `all_handlers_with_names`), or, for the kg pack only,
//!      a documented sub-namespace.
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
/// The 20 entries cover CRUD + graph + curation + proposal primitives, plus
/// `stats` for aggregate namespace metrics, `verbs` for verb-registry
/// introspection (J-help PR #464), `context` for entity-anchored graph
/// context in one call (ADR-089), `resolve` for reference resolution (S1),
/// `whoami` for caller identity introspection, and `db_diagnostics` for the
/// WAL/checkpoint operator diagnostics surface (ADR-091).
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
    "whoami",
    "db_diagnostics",
];

/// Dotted prefixes the kg pack may carry beside its bare verbs. Closed list:
/// `stream` per ADR-174 section 2 ("registered by the kg pack because the
/// entries are its notes"). Adding a name here is a contract change and
/// needs the ADR line that grants it.
const KG_SUB_NAMESPACES: &[&str] = &["stream"];

/// The contract's classification, one place for both tests and the decoy
/// test below: every violation as a sentence naming the pack and the verb.
fn violations(handlers: &[(String, String)]) -> Vec<String> {
    let mut violations: Vec<String> = Vec::new();

    for (pack_name, verb_name) in handlers {
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
            // Exactly one dot — prefix must match the pack name, or be one of
            // the kg pack's documented sub-namespaces.
            1 => {
                let prefix = verb_name.split('.').next().unwrap_or("");
                let documented_kg_sub_namespace =
                    pack_name == "kg" && KG_SUB_NAMESPACES.contains(&prefix);
                if prefix != pack_name && !documented_kg_sub_namespace {
                    violations.push(format!(
                        "pack={pack_name:?} verb={verb_name:?}: prefix {prefix:?} does not \
                         match pack name {pack_name:?} and is not a documented kg \
                         sub-namespace ({KG_SUB_NAMESPACES:?}, ADR-174 section 2)."
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

    violations
}

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
/// name that owns it; the kg pack may also carry its documented sub-namespaces.
#[test]
fn every_non_kg_verb_is_namespaced() {
    let handlers = build_full_registry();
    let violations = violations(&handlers);
    assert!(
        violations.is_empty(),
        "Verb namespace contract violations:\n{}",
        violations.join("\n")
    );
}

/// The allow-list is a decoy-tested gate, not a blanket: a dotted kg verb
/// outside the documented sub-namespace, another pack borrowing the `stream`
/// prefix, and a two-dot kg name each still redden the contract, while the
/// live registry alone passes.
#[test]
fn a_dotted_kg_verb_outside_the_documented_sub_namespace_still_reddens() {
    let live = build_full_registry();
    assert!(
        violations(&live).is_empty(),
        "control: the live registry must pass before decoys are judged"
    );
    assert!(
        live.iter()
            .any(|(pack, verb)| pack == "kg" && verb == "stream.append"),
        "control: the documented sub-namespace must be present in the live registry"
    );

    let decoys: Vec<(String, String)> = vec![
        ("kg".into(), "ledger.append".into()),
        ("memory".into(), "stream.read".into()),
        ("kg".into(), "stream.append.now".into()),
    ];
    for decoy in &decoys {
        let mut handlers = live.clone();
        handlers.push(decoy.clone());
        let found = violations(&handlers);
        assert_eq!(
            found.len(),
            1,
            "decoy {decoy:?} must produce exactly one violation; got {found:?}"
        );
        assert!(
            found[0].contains(&format!("verb={:?}", decoy.1)),
            "the violation must name the decoy verb: {found:?}"
        );
    }
}

/// Complementary check: the kg substrate pack must expose all mandated bare
/// verbs, and any dotted name it carries must sit in a documented
/// sub-namespace with exactly one dot. This catches regressions in the kg
/// pack itself.
#[test]
fn kg_pack_exposes_bare_verbs_or_a_documented_sub_namespace() {
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

    // A dotted kg verb sits in a documented sub-namespace, one dot only.
    let undocumented: Vec<&&str> = kg_verbs
        .iter()
        .filter(|v| v.contains('.'))
        .filter(|v| {
            let mut parts = v.split('.');
            let prefix = parts.next().unwrap_or("");
            let rest = parts.next().unwrap_or("");
            parts.next().is_some() || rest.is_empty() || !KG_SUB_NAMESPACES.contains(&prefix)
        })
        .collect();
    assert!(
        undocumented.is_empty(),
        "kg pack may carry dotted names only under {KG_SUB_NAMESPACES:?} (ADR-174 section 2); \
         found: {undocumented:?}"
    );
}
