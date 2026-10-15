//! Canonical, I/O-free edge endpoint contracts shared by live and offline validation.

use crate::{EdgeRelation, EndpointKind};

/// `true` if `spec` matches the given substrate + kind + entity_type triple.
///
/// Pure and DB-free — exposed so offline consumers (e.g. `kkernel kg
/// validate`, which parses `(substrate, kind, entity_type)` straight out of
/// NDJSON with no live record to resolve) can apply the exact same
/// `EdgeEndpointRule` matching semantics `pack_rule_allows` uses internally,
/// instead of re-deriving a parallel matcher that could drift out of sync.
pub fn endpoint_matches(
    spec: &EndpointKind,
    substrate: &str,
    kind: &str,
    entity_type: Option<&str>,
) -> bool {
    match spec {
        EndpointKind::EntityOfKind(k) => substrate == "entity" && *k == kind,
        EndpointKind::NoteOfKind(k) => substrate == "note" && *k == kind,
        EndpointKind::EntityOfType {
            kind: k,
            entity_type: t,
        } => substrate == "entity" && *k == kind && entity_type == Some(*t),
    }
}

/// Base entity endpoint allowlist — the closed set of permitted entity→entity
/// relation triples.
///
/// Each entry `(src_kind, relation, tgt_kind)` explicitly allows that combination.
/// `"*"` as `src_kind` means "any entity kind" (used by `instance_of` whose source
/// is unrestricted).
///
/// Pack rules (via `EDGE_RULES`) are additive — they cannot remove rows here.
/// Exposed via `base_entity_endpoint_rules()` for the ADR-076 certificate tests.
pub const BASE_ENTITY_ENDPOINT_RULES: &[(&str, EdgeRelation, &str)] = &[
    // Structure
    ("concept", EdgeRelation::Contains, "concept"),
    ("project", EdgeRelation::Contains, "project"),
    ("project", EdgeRelation::Contains, "artifact"),
    ("org", EdgeRelation::Contains, "project"),
    ("org", EdgeRelation::Contains, "service"),
    ("concept", EdgeRelation::PartOf, "concept"),
    ("project", EdgeRelation::PartOf, "project"),
    ("project", EdgeRelation::PartOf, "org"),
    ("*", EdgeRelation::InstanceOf, "concept"),
    ("service", EdgeRelation::InstanceOf, "project"),
    // ADR-002 amendment (ADR-191): web hyperlink — a document points at
    // another document it links to. No qualifier inference (unlike
    // depends_on); the endpoint pair is intentionally narrow (document only,
    // no service/concept targets — see ADR-191 D2/F10).
    ("document", EdgeRelation::LinksTo, "document"),
    // ADR-196 location, amended 2026-10-05: the source occupies or is manifested in the target
    // without being a constituent of it; base rows for concept and org, packs narrow the rest.
    ("concept", EdgeRelation::LocatedIn, "concept"),
    ("org", EdgeRelation::LocatedIn, "concept"),
    // Ownership
    ("person", EdgeRelation::Owns, "org"),
    ("org", EdgeRelation::Owns, "org"),
    // Derivation
    ("concept", EdgeRelation::Extends, "concept"),
    ("concept", EdgeRelation::VariantOf, "concept"),
    ("artifact", EdgeRelation::VariantOf, "artifact"),
    ("concept", EdgeRelation::IntroducedBy, "document"),
    ("concept", EdgeRelation::IntroducedBy, "person"),
    ("artifact", EdgeRelation::IntroducedBy, "document"),
    ("project", EdgeRelation::IntroducedBy, "document"),
    // ADR-002 amendment (ADR-167): service provenance — the document that
    // introduced a service (its ADR or design record).
    ("service", EdgeRelation::IntroducedBy, "document"),
    ("document", EdgeRelation::IntroducedBy, "person"),
    ("document", EdgeRelation::IntroducedBy, "org"),
    ("concept", EdgeRelation::IntroducedBy, "org"),
    // Provenance
    ("artifact", EdgeRelation::DerivedFrom, "dataset"),
    ("artifact", EdgeRelation::DerivedFrom, "document"),
    ("artifact", EdgeRelation::DerivedFrom, "project"),
    ("artifact", EdgeRelation::DerivedFrom, "artifact"),
    // ADR-002 amendment 2026-07-27: publication provenance — a curated or
    // filtered publication copy points at the canonical source document.
    ("document", EdgeRelation::DerivedFrom, "document"),
    // Temporal
    ("document", EdgeRelation::Precedes, "document"),
    ("dataset", EdgeRelation::Precedes, "dataset"),
    ("artifact", EdgeRelation::Precedes, "artifact"),
    ("service", EdgeRelation::Precedes, "service"),
    ("project", EdgeRelation::Precedes, "project"),
    // Dependency
    ("project", EdgeRelation::DependsOn, "project"),
    ("service", EdgeRelation::DependsOn, "project"),
    ("service", EdgeRelation::DependsOn, "service"),
    ("service", EdgeRelation::DependsOn, "artifact"),
    ("service", EdgeRelation::DependsOn, "dataset"),
    ("artifact", EdgeRelation::DependsOn, "project"),
    ("artifact", EdgeRelation::DependsOn, "service"),
    ("document", EdgeRelation::DependsOn, "document"),
    ("concept", EdgeRelation::Enables, "concept"),
    ("service", EdgeRelation::Enables, "concept"),
    ("dataset", EdgeRelation::Enables, "concept"),
    // Implementation
    ("project", EdgeRelation::Implements, "concept"),
    ("service", EdgeRelation::Implements, "concept"),
    // Lateral
    ("concept", EdgeRelation::CompetesWith, "concept"),
    ("project", EdgeRelation::CompetesWith, "project"),
    ("service", EdgeRelation::CompetesWith, "service"),
    ("org", EdgeRelation::CompetesWith, "org"),
    ("concept", EdgeRelation::ComposedWith, "concept"),
    ("project", EdgeRelation::ComposedWith, "project"),
    // Versioning (Supersedes — Concept/Document/Artifact/Service/Dataset only)
    ("concept", EdgeRelation::Supersedes, "concept"),
    ("document", EdgeRelation::Supersedes, "document"),
    ("artifact", EdgeRelation::Supersedes, "artifact"),
    ("service", EdgeRelation::Supersedes, "service"),
    ("dataset", EdgeRelation::Supersedes, "dataset"),
    // Epistemic (Supports/Refutes — evidence sources → Concept claim only)
    ("concept", EdgeRelation::Supports, "concept"),
    ("document", EdgeRelation::Supports, "concept"),
    ("dataset", EdgeRelation::Supports, "concept"),
    ("artifact", EdgeRelation::Supports, "concept"),
    ("concept", EdgeRelation::Refutes, "concept"),
    ("document", EdgeRelation::Refutes, "concept"),
    ("dataset", EdgeRelation::Refutes, "concept"),
    ("artifact", EdgeRelation::Refutes, "concept"),
];

/// Returns the base entity endpoint allowlist.
///
/// The returned slice is the same data that `base_entity_rule_allows` consults at
/// runtime. Exposed for the ADR-076 certificate tests in `khive-pack-kg`, which
/// must audit live rules rather than hand-copied snapshots.
pub fn base_entity_endpoint_rules() -> &'static [(&'static str, EdgeRelation, &'static str)] {
    BASE_ENTITY_ENDPOINT_RULES
}

/// `true` if `(src_kind, relation, tgt_kind)` is in the base entity endpoint
/// allowlist. Pure and DB-free — exposed alongside [`base_entity_endpoint_rules`]
/// so offline consumers (e.g. `kkernel kg validate`) can apply the exact same
/// base-table membership test the live validator uses, instead of re-deriving
/// a parallel `.any()` predicate over a hand-copied allowlist.
pub fn base_entity_rule_allows(src_kind: &str, relation: EdgeRelation, tgt_kind: &str) -> bool {
    BASE_ENTITY_ENDPOINT_RULES.iter().any(|(src, rel, tgt)| {
        *rel == relation && (*src == "*" || *src == src_kind) && *tgt == tgt_kind
    })
}
