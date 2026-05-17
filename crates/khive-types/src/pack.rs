//! Pack trait — the composition unit for khive's verb-pack architecture.
//!
//! A pack declares vocabulary (note kinds, entity kinds) and verbs that it
//! contributes to the runtime. The runtime collects all pack vocabularies at
//! init and validates kind strings against the merged set; verbs are dispatched
//! through the registry.
//!
//! This trait lives in khive-types (no_std, zero deps) so anything that needs
//! to validate kinds can depend only on types, not the full runtime.

/// Verb metadata for discovery and documentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerbDef {
    pub name: &'static str,
    pub description: &'static str,
}

/// A composable module that contributes vocabulary and verbs to the khive runtime.
///
/// Packs declare what entity kinds, note kinds, and verbs they introduce.
/// The runtime merges vocabularies from all loaded packs and rejects
/// unregistered kinds at the service boundary.
///
/// Edge relations remain a closed enum (ADR-021) and are NOT pack-extensible.
pub trait Pack {
    /// Short identifier for this pack (e.g. "kg", "lambda", "leo").
    const NAME: &'static str;

    /// Note kinds this pack contributes to the runtime vocabulary.
    const NOTE_KINDS: &'static [&'static str];

    /// Entity kinds this pack contributes to the runtime vocabulary.
    const ENTITY_KINDS: &'static [&'static str];

    /// Verbs this pack handles. The runtime routes verb calls to the pack
    /// that declares them.
    const VERBS: &'static [VerbDef];
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
