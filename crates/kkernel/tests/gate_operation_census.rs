//! The reviewed access table must cover the complete linked production surface.
//!
//! This includes internal handlers and optional packs, not only MCP-visible
//! verbs. Dynamic/mounted names still fail closed at runtime; they cannot be
//! inferred safe from a namespace, synonym, or speech-act category.

use std::collections::BTreeSet;

use khive_runtime::pack::{PackRegistry, VerbRegistryBuilder, Visibility};
use khive_runtime::{
    classify_operation, KhiveRuntime, OperationAccess, RuntimeConfig, CLASSIFIED_OPERATIONS,
    OPERATION_CLASSIFIER_VERSION,
};
// Keep kkernel's production force-link anchors, including feature-gated packs.
use kkernel as _;

#[test]
fn every_production_handler_has_an_explicit_reviewed_access_class() {
    let discovered: BTreeSet<_> = PackRegistry::discovered_names().into_iter().collect();
    let mut expected_packs: BTreeSet<_> = [
        "agent",
        "blob",
        "brain",
        "code",
        "comm",
        "exec",
        "git",
        "gtd",
        "kg",
        "knowledge",
        "memory",
        "schedule",
        "session",
        "telemetry",
        "tool",
        "web",
        "workspace",
    ]
    .into_iter()
    .collect();
    if cfg!(feature = "pack-formal") {
        expected_packs.insert("formal");
    }
    if cfg!(feature = "pack-moodboard") {
        expected_packs.insert("moodboard");
    }
    assert_eq!(
        discovered, expected_packs,
        "review new production packs; do not silently narrow the census"
    );

    let packs: Vec<String> = discovered.into_iter().map(str::to_owned).collect();
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: packs.clone(),
        actor_id: None,
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory metadata runtime");
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(&packs, runtime, &mut builder)
        .expect("register every discovered production pack");
    let registry = builder.build_metadata().expect("complete handler metadata");
    let handlers = registry.all_handlers_with_names();
    assert!(
        handlers
            .iter()
            .any(|(_, handler)| matches!(handler.visibility, Visibility::Subhandler)),
        "the census must include internal handlers"
    );
    let unclassified: Vec<_> = handlers
        .iter()
        .filter(|(_, handler)| classify_operation(handler.name).is_none())
        .map(|(pack, handler)| format!("{pack}: {}", handler.name))
        .collect();
    assert!(
        unclassified.is_empty(),
        "review and classify every new handler in operation.rs and \
         khive-gate/docs/api/operation-access.md (classifier {OPERATION_CLASSIFIER_VERSION}): \
         {unclassified:?}"
    );

    let actual: BTreeSet<_> = handlers.iter().map(|(_, handler)| handler.name).collect();
    assert_eq!(actual.len(), handlers.len(), "duplicate registered names");
    let reviewed_static: BTreeSet<_> = CLASSIFIED_OPERATIONS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !matches!(*name, "authorize" | "authorize.visible"))
        .filter(|name| cfg!(feature = "pack-moodboard") || !name.starts_with("moodboard."))
        .collect();
    assert_eq!(
        actual, reviewed_static,
        "the table must name actual handlers, including aliases and subhandlers"
    );
}

#[test]
fn reviewed_document_and_pseudo_operations_match_the_classifier() {
    let document = include_str!("../../khive-gate/docs/api/operation-access.md");
    let documented: BTreeSet<_> = document
        .lines()
        .filter_map(|line| {
            let mut cells = line.strip_prefix('|')?.split('|').map(str::trim);
            let name = cells.next()?.strip_prefix('`')?.strip_suffix('`')?;
            Some((name, cells.next()?))
        })
        .collect();
    assert!(!OPERATION_CLASSIFIER_VERSION.is_empty());
    for &(name, access) in CLASSIFIED_OPERATIONS {
        let label = match access {
            OperationAccess::Read => "Read",
            OperationAccess::Write => "Write",
        };
        assert!(
            documented.contains(&(name, label)),
            "missing reviewed table row: {name} {label}"
        );
    }
    assert_eq!(
        classify_operation("authorize"),
        Some(OperationAccess::Write)
    );
    assert_eq!(
        classify_operation("authorize.visible"),
        Some(OperationAccess::Read)
    );
    assert_eq!(
        classify_operation("brain.emit"),
        Some(OperationAccess::Write)
    );
    assert_eq!(
        classify_operation("memory.recall_embed"),
        Some(OperationAccess::Read)
    );
    for name in ["new_pack.read", "memory.recall.embed", "mounted.safe_read"] {
        assert_eq!(
            classify_operation(name),
            None,
            "unreviewed operation {name} must remain distinguishable from explicit Write"
        );
    }
}
