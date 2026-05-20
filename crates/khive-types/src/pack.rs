//! Pack trait — the declarative composition unit for khive (ADR-025).
//!
//! A pack declares vocabulary (note kinds, entity kinds), verbs, and edge
//! endpoint rules. This is purely static metadata — no I/O, no async.
//! Runtime dispatch lives in `khive-runtime` (`PackRuntime` trait +
//! `VerbRegistry`).
//!
//! This trait lives in khive-types (no_std, zero deps) so downstream crates
//! can reference pack metadata without pulling in the full runtime.

use crate::edge::EdgeRelation;

/// Verb metadata for discovery and documentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerbDef {
    pub name: &'static str,
    pub description: &'static str,
}

/// Match spec for one end of an [`EdgeEndpointRule`] (ADR-031).
///
/// Identifies a substrate + kind pair that the rule applies to. Note that
/// `kind` strings refer to the pack-declared note kinds / entity kinds — not
/// the closed [`EdgeRelation`] set, which is universal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointKind {
    /// A note whose `kind` field equals the given string (e.g. `"task"`).
    NoteOfKind(&'static str),
    /// An entity whose `kind` field equals the given string (e.g. `"concept"`).
    EntityOfKind(&'static str),
}

/// A pack-declared endpoint rule for a specific edge relation (ADR-031).
///
/// Rules are **additive**: they extend the set of allowed
/// `(source, relation, target)` triples beyond the ADR-002 base contract.
/// Packs cannot tighten the base rules — only broaden them. The closed
/// [`EdgeRelation`] taxonomy itself is not extended; only the endpoint
/// contract per relation is.
///
/// Example — GTD pack allows `depends_on` between task notes:
///
/// ```ignore
/// EdgeEndpointRule {
///     relation: EdgeRelation::DependsOn,
///     source: EndpointKind::NoteOfKind("task"),
///     target: EndpointKind::NoteOfKind("task"),
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeEndpointRule {
    pub relation: EdgeRelation,
    pub source: EndpointKind,
    pub target: EndpointKind,
}

/// A composable module that contributes vocabulary, verbs, and edge endpoint
/// rules to the khive runtime.
///
/// Packs declare what entity kinds, note kinds, and verbs they introduce, and
/// optionally extend the per-relation endpoint contract via [`EDGE_RULES`].
/// The runtime merges vocabularies from all loaded packs and rejects
/// unregistered kinds at the service boundary.
///
/// The closed [`EdgeRelation`] enum (ADR-021) is not extensible — only its
/// per-relation endpoint contract is (ADR-031).
///
/// [`EDGE_RULES`]: Pack::EDGE_RULES
pub trait Pack {
    /// Short identifier for this pack (e.g. "kg", "tasks").
    const NAME: &'static str;

    /// Note kinds this pack contributes to the runtime vocabulary.
    const NOTE_KINDS: &'static [&'static str];

    /// Entity kinds this pack contributes to the runtime vocabulary.
    const ENTITY_KINDS: &'static [&'static str];

    /// Verbs this pack handles. The runtime routes verb calls to the pack
    /// that declares them.
    const VERBS: &'static [VerbDef];

    /// Additional edge endpoint rules this pack contributes (ADR-031).
    ///
    /// Defaults to empty — packs that introduce no new endpoint pairs (or
    /// only rely on the ADR-002 base contract) can ignore this.
    const EDGE_RULES: &'static [EdgeEndpointRule] = &[];

    /// Other pack names whose vocabulary this pack references (ADR-037).
    ///
    /// The runtime checks that every name in `REQUIRES` appears in the
    /// loaded pack set before any pack is registered. Defaults to empty
    /// so existing packs compile without changes.
    const REQUIRES: &'static [&'static str] = &[];
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestPack;

    impl Pack for TestPack {
        const NAME: &'static str = "test";
        const NOTE_KINDS: &'static [&'static str] = &["memo"];
        const ENTITY_KINDS: &'static [&'static str] = &["widget"];
        const VERBS: &'static [VerbDef] = &[VerbDef {
            name: "do_thing",
            description: "does a thing",
        }];
    }

    #[test]
    fn pack_trait_compiles() {
        assert_eq!(TestPack::NAME, "test");
        assert_eq!(TestPack::NOTE_KINDS, &["memo"]);
        assert_eq!(TestPack::ENTITY_KINDS, &["widget"]);
        assert_eq!(TestPack::VERBS.len(), 1);
        assert_eq!(TestPack::VERBS[0].name, "do_thing");
    }
}
