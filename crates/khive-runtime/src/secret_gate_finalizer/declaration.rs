//! Runtime-owned declaration of finalizer constructor entry points
//! (ADR-115 Amendment 1 §3; executable contract §2).
//!
//! This is the sole admission criterion: a mutation is admission-capable if
//! and only if the final stored entity, note, or knowledge candidate reaches one of the
//! constructors declared below. The acceptance matrix in [`super::matrix`]
//! is generated from this declaration — it must never be hand-maintained in
//! parallel. Git, session, MCP direct writes, edge metadata, proposal-only
//! metadata, merge reasons, and embedding-content overrides remain outside
//! this admission surface under their separately owned follow-ons.

/// The stored substrate a declared constructor produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[allow(dead_code)] // consumed by the execution/outcome increment (step 15)
pub(crate) enum Substrate {
    Entity,
    Note,
    KnowledgeAtom,
    KnowledgeDomain,
}

/// One admission-capable finalizer entry point.
///
/// `id` is the stable, unique identifier used by the generated acceptance
/// matrix and by downstream reachability checks; it is not a wire value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // consumed by the execution/outcome increment (step 15)
pub(crate) struct FinalizerEntryPoint {
    pub(crate) id: &'static str,
    pub(crate) constructor: &'static str,
    pub(crate) mutation: &'static str,
    pub(crate) substrate: Substrate,
    pub(crate) origins: &'static [&'static str],
}

/// The complete, closed set of admission-capable finalizer entry points.
///
/// The original six entity/note constructors plus the two knowledge target
/// families introduced by #2058. Direct origins remain origins rather than
/// parallel constructor families.
#[allow(dead_code)] // consumed by the execution/outcome increment (step 15)
pub(crate) const FINALIZER_ENTRY_POINTS: &[FinalizerEntryPoint] = &[
    FinalizerEntryPoint {
        id: "entity.create",
        constructor: "entity candidate",
        mutation: "create",
        substrate: Substrate::Entity,
        origins: &[
            "runtime",
            "code.ingest",
            "portability.restore",
            "proposal.materialize",
        ],
    },
    FinalizerEntryPoint {
        id: "entity.update",
        constructor: "entity candidate",
        mutation: "update",
        substrate: Substrate::Entity,
        origins: &["runtime", "code.ingest", "curation", "merge"],
    },
    FinalizerEntryPoint {
        id: "entity.bulk",
        constructor: "entity candidate",
        mutation: "bulk",
        substrate: Substrate::Entity,
        origins: &["runtime", "atomic_prepare", "proposal.materialize"],
    },
    FinalizerEntryPoint {
        id: "note.create",
        constructor: "note candidate",
        mutation: "create",
        substrate: Substrate::Note,
        origins: &["runtime", "code.ingest", "proposal.materialize"],
    },
    FinalizerEntryPoint {
        id: "note.update",
        constructor: "note candidate",
        mutation: "update",
        substrate: Substrate::Note,
        origins: &["runtime", "code.ingest", "curation", "merge"],
    },
    FinalizerEntryPoint {
        id: "note.atomic_message",
        constructor: "note candidate",
        mutation: "atomic message",
        substrate: Substrate::Note,
        origins: &["runtime"],
    },
    FinalizerEntryPoint {
        id: "knowledge.atom",
        constructor: "knowledge atom candidate",
        mutation: "upsert",
        substrate: Substrate::KnowledgeAtom,
        origins: &["knowledge.crud", "knowledge.sections"],
    },
    FinalizerEntryPoint {
        id: "knowledge.domain",
        constructor: "knowledge domain candidate",
        mutation: "upsert",
        substrate: Substrate::KnowledgeDomain,
        origins: &["knowledge.crud"],
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn declares_complete_entry_point_set() {
        assert_eq!(FINALIZER_ENTRY_POINTS.len(), 8);
    }

    #[test]
    fn entry_point_ids_are_unique() {
        let ids: BTreeSet<&str> = FINALIZER_ENTRY_POINTS.iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), FINALIZER_ENTRY_POINTS.len());
    }

    #[test]
    fn matches_contract_id_list_exactly() {
        let ids: Vec<&str> = FINALIZER_ENTRY_POINTS.iter().map(|e| e.id).collect();
        assert_eq!(
            ids,
            vec![
                "entity.create",
                "entity.update",
                "entity.bulk",
                "note.create",
                "note.update",
                "note.atomic_message",
                "knowledge.atom",
                "knowledge.domain",
            ]
        );
    }

    #[test]
    fn follow_on_inventory_origins_are_mechanized_in_the_declaration() {
        let origin_pairs: BTreeSet<(&str, &str)> = FINALIZER_ENTRY_POINTS
            .iter()
            .flat_map(|entry| entry.origins.iter().map(move |origin| (entry.id, *origin)))
            .collect();
        for required in [
            ("entity.create", "portability.restore"),
            ("entity.create", "proposal.materialize"),
            ("entity.update", "curation"),
            ("entity.update", "merge"),
            ("note.create", "proposal.materialize"),
            ("note.update", "curation"),
            ("note.update", "merge"),
            ("knowledge.atom", "knowledge.crud"),
            ("knowledge.atom", "knowledge.sections"),
            ("knowledge.domain", "knowledge.crud"),
        ] {
            assert!(
                origin_pairs.contains(&required),
                "property-bearing write origin {required:?} is missing from the finalizer census"
            );
        }
    }

    /// The declaration is backed by source markers at every non-delegating
    /// property-bearing writer. Adding or renaming a writer without routing it
    /// through the boundary makes this census fail in CI instead of silently
    /// creating another bypass.
    #[test]
    fn declared_origins_have_mechanized_source_reachability() {
        let runtime_operations = include_str!("../operations.rs");
        let curation = include_str!("../curation.rs");
        let atomic_prepare = include_str!("../atomic_prepare.rs");
        let atomic_message = include_str!("../atomic_message.rs");
        let portability = include_str!("../portability.rs");
        let knowledge = include_str!("../../../khive-pack-knowledge/src/knowledge/crud.rs");
        let code_ingest = include_str!("../../../kkernel/src/code_ingest.rs");

        for (source_name, source, marker) in [
            ("operations", runtime_operations, "entity.create runtime"),
            ("operations", runtime_operations, "entity.bulk runtime"),
            ("operations", runtime_operations, "note.create runtime"),
            ("curation", curation, "entity.update curation"),
            ("curation", curation, "entity.update merge"),
            ("curation", curation, "note.update curation"),
            ("curation", curation, "note.update merge"),
            (
                "atomic_prepare",
                atomic_prepare,
                "entity.bulk proposal.materialize",
            ),
            (
                "atomic_prepare",
                atomic_prepare,
                "note.create proposal.materialize",
            ),
            ("atomic_message", atomic_message, "note.atomic_message"),
            (
                "portability",
                portability,
                "entity.create portability.restore",
            ),
            ("code_ingest", code_ingest, "entity.create code.ingest"),
            ("code_ingest", code_ingest, "note.create code.ingest"),
            ("knowledge", knowledge, "knowledge.atom knowledge.crud"),
            ("knowledge", knowledge, "knowledge.domain knowledge.crud"),
        ] {
            let full_marker = format!("secret-gate-finalizer-entry: {marker}");
            assert!(
                source.contains(&full_marker),
                "{source_name} is missing finalizer reachability marker {full_marker:?}"
            );
        }
    }
}
