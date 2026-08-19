//! Runtime-owned declaration of finalizer constructor entry points
//! (ADR-115 Amendment 1 §3; executable contract §2).
//!
//! This is the sole admission criterion: a mutation is admission-capable if
//! and only if the final stored entity or note candidate reaches one of the
//! constructors declared below. The acceptance matrix in [`super::matrix`]
//! is generated from this declaration — it must never be hand-maintained in
//! parallel. Curation, atomic-prepare, proposal materialization, knowledge,
//! git, session, MCP direct writes, edge metadata, proposal-only metadata,
//! merge reasons, and embedding-content overrides are deliberately absent:
//! they remain reservation-only and out of the admission surface.

/// The stored substrate a declared constructor produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[allow(dead_code)] // consumed by the execution/outcome increment (step 15)
pub(crate) enum Substrate {
    Entity,
    Note,
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
/// Exactly six rows, per the executable contract §2 table. Direct
/// code-ingest is an origin of an entity or note row, not a seventh family.
#[allow(dead_code)] // consumed by the execution/outcome increment (step 15)
pub(crate) const FINALIZER_ENTRY_POINTS: &[FinalizerEntryPoint] = &[
    FinalizerEntryPoint {
        id: "entity.create",
        constructor: "entity candidate",
        mutation: "create",
        substrate: Substrate::Entity,
        origins: &["runtime", "code.ingest"],
    },
    FinalizerEntryPoint {
        id: "entity.update",
        constructor: "entity candidate",
        mutation: "update",
        substrate: Substrate::Entity,
        origins: &["runtime", "code.ingest"],
    },
    FinalizerEntryPoint {
        id: "entity.bulk",
        constructor: "entity candidate",
        mutation: "bulk",
        substrate: Substrate::Entity,
        origins: &["runtime"],
    },
    FinalizerEntryPoint {
        id: "note.create",
        constructor: "note candidate",
        mutation: "create",
        substrate: Substrate::Note,
        origins: &["runtime", "code.ingest"],
    },
    FinalizerEntryPoint {
        id: "note.update",
        constructor: "note candidate",
        mutation: "update",
        substrate: Substrate::Note,
        origins: &["runtime", "code.ingest"],
    },
    FinalizerEntryPoint {
        id: "note.atomic_message",
        constructor: "note candidate",
        mutation: "atomic message",
        substrate: Substrate::Note,
        origins: &["runtime"],
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn declares_exactly_six_entry_points() {
        assert_eq!(FINALIZER_ENTRY_POINTS.len(), 6);
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
            ]
        );
    }
}
