//! EntityTypeRegistry — validates and normalises `(EntityKind, entity_type)` pairs.
//!
//! `entity_type` is a first-class column on the `entities` table but was
//! previously stored as a free-text string with no validation.  This module
//! introduces a static registry that:
//!
//! 1. Declares the canonical subtypes for every `EntityKind`.
//! 2. Resolves aliases to canonical names (e.g. `"algo"` → `"algorithm"`).
//! 3. Infers the `EntityKind` from a bare subtype string (e.g. `"paper"` →
//!    `(Document, "paper")`), which allows `kind="paper"` at the wire level
//!    without an explicit `entity_kind`.
//! 4. Rejects `entity_type` values that don't belong to the supplied kind.
//! 5. Is extensible: external packs (e.g. `brain`) can call
//!    [`EntityTypeRegistry::register`] to append their own subtypes.
//!
//! # Design notes
//!
//! - The registry is kept in `khive-pack-kg`, not `khive-types`, because
//!   domain-specific subtype names are pack-owned vocabulary.
//! - A `once_cell::sync::Lazy` global holds the default registry with all
//!   built-in subtypes pre-populated.  Packs that extend it create a clone,
//!   add entries, and store the result.  The typical path is to call
//!   [`EntityTypeRegistry::global`] which returns the built-in registry; the
//!   handful of packs that extend it can call
//!   [`EntityTypeRegistry::with_extra`] to derive an extended copy.

use std::collections::HashMap;

use khive_types::EntityKind;

use crate::RuntimeError;

/// One entry in the registry: a canonical subtype name for a specific kind,
/// together with any accepted aliases.
#[derive(Clone, Debug)]
pub struct EntityTypeDef {
    pub kind: EntityKind,
    /// Canonical name that is written to the DB.
    pub type_name: &'static str,
    /// Alternative spellings that are accepted at the wire level but
    /// normalised to `type_name` before storage.
    pub aliases: &'static [&'static str],
}

/// Static table of built-in subtypes (non-exhaustive; packs may extend).
///
/// Ordered by kind so it is easy to scan visually.
static BUILTIN_DEFS: &[EntityTypeDef] = &[
    // Document
    EntityTypeDef {
        kind: EntityKind::Document,
        type_name: "paper",
        aliases: &["preprint", "article"],
    },
    EntityTypeDef {
        kind: EntityKind::Document,
        type_name: "report",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Document,
        type_name: "specification",
        aliases: &["spec"],
    },
    EntityTypeDef {
        kind: EntityKind::Document,
        type_name: "standard",
        aliases: &[],
    },
    // Concept
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "algorithm",
        aliases: &["algo"],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "technique",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "architecture",
        aliases: &["arch"],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "model",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "benchmark",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "dataset_concept",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "research_gap",
        aliases: &["gap"],
    },
    // Project
    EntityTypeDef {
        kind: EntityKind::Project,
        type_name: "tool",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Project,
        type_name: "library",
        aliases: &["lib"],
    },
    EntityTypeDef {
        kind: EntityKind::Project,
        type_name: "crate",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Project,
        type_name: "service",
        aliases: &["svc"],
    },
    // Org
    EntityTypeDef {
        kind: EntityKind::Org,
        type_name: "company",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Org,
        type_name: "university",
        aliases: &["uni"],
    },
    EntityTypeDef {
        kind: EntityKind::Org,
        type_name: "lab",
        aliases: &[],
    },
    // Artifact
    EntityTypeDef {
        kind: EntityKind::Artifact,
        type_name: "checkpoint",
        aliases: &["ckpt"],
    },
    EntityTypeDef {
        kind: EntityKind::Artifact,
        type_name: "config",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Artifact,
        type_name: "schema",
        aliases: &[],
    },
    // Service — no standard subtypes; packs may extend.
    // Person  — no standard subtypes.
    // Dataset — no standard subtypes.
];

/// Resolved output of [`EntityTypeRegistry::resolve`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedType {
    /// The canonical `EntityKind` for the resolved combination.
    pub kind: EntityKind,
    /// The canonical type name to store, or `None` when no `entity_type` was
    /// supplied and none was inferred.
    pub entity_type: Option<String>,
}

/// Registry for `(EntityKind, entity_type)` pair validation and alias
/// normalisation.
///
/// Build the default registry with [`EntityTypeRegistry::new`]; the lazily
/// initialised global is available via [`EntityTypeRegistry::global`].
/// Extend it with [`EntityTypeRegistry::with_extra`].
#[derive(Clone)]
pub struct EntityTypeRegistry {
    /// `alias_or_name (lowercase) → def index`.  Covers both canonical names
    /// and all registered aliases.
    lookup: HashMap<String, usize>,
    defs: Vec<EntityTypeDef>,
}

impl EntityTypeRegistry {
    /// Build a fresh registry from the supplied definitions.
    ///
    /// Panics in debug builds when two definitions share an alias under the
    /// same `EntityKind` (would produce an ambiguous lookup).
    pub fn new(defs: impl IntoIterator<Item = EntityTypeDef>) -> Self {
        let defs: Vec<EntityTypeDef> = defs.into_iter().collect();
        let mut lookup: HashMap<String, usize> = HashMap::new();
        for (idx, def) in defs.iter().enumerate() {
            let canonical_key = format!("{}:{}", def.kind.name(), def.type_name);
            lookup.insert(canonical_key, idx);
            let bare_key = def.type_name.to_string();
            // Bare name without kind prefix — only insert when unambiguous.
            lookup.entry(bare_key).or_insert(idx);
            for alias in def.aliases {
                let kind_alias_key = format!("{}:{}", def.kind.name(), alias);
                lookup.insert(kind_alias_key, idx);
                lookup.entry(alias.to_string()).or_insert(idx);
            }
        }
        Self { lookup, defs }
    }

    /// Return the built-in registry (subtypes from [`BUILTIN_DEFS`]).
    pub fn builtin() -> Self {
        Self::new(BUILTIN_DEFS.iter().cloned())
    }

    /// Derive a registry that includes all built-in subtypes plus the
    /// caller-supplied extras (used by packs that extend the vocabulary).
    pub fn with_extra(extra: impl IntoIterator<Item = EntityTypeDef>) -> Self {
        let defs: Vec<EntityTypeDef> = BUILTIN_DEFS.iter().cloned().chain(extra).collect();
        Self::new(defs)
    }

    /// Register additional subtypes into an existing registry clone.
    ///
    /// Intended for pack initialisation: a pack calls `registry.register(...)`
    /// on a cloned global to obtain an extended copy for its lifetime.
    pub fn register(&mut self, def: EntityTypeDef) {
        let idx = self.defs.len();
        let canonical_key = format!("{}:{}", def.kind.name(), def.type_name);
        self.lookup.insert(canonical_key, idx);
        self.lookup.entry(def.type_name.to_string()).or_insert(idx);
        for alias in def.aliases {
            let kind_alias_key = format!("{}:{}", def.kind.name(), alias);
            self.lookup.insert(kind_alias_key, idx);
            self.lookup.entry(alias.to_string()).or_insert(idx);
        }
        self.defs.push(def);
    }

    /// Validate and normalise a `(kind_str, entity_type)` wire pair.
    ///
    /// Semantics:
    ///
    /// - `entity_type = None` → accepted for all kinds; `ResolvedType.entity_type`
    ///   is also `None`.
    /// - `entity_type = Some(t)` where `t` is a canonical name or alias valid
    ///   for `kind_str` → normalised to the canonical name.
    /// - `entity_type = Some(t)` where `t` belongs to a *different* kind →
    ///   `InvalidInput` listing valid subtypes for the supplied kind.
    /// - `entity_type = Some(t)` where `t` is not recognised at all →
    ///   `InvalidInput` listing valid subtypes for the supplied kind.
    ///
    /// `kind_str` must already be a canonical kind name (the result of
    /// `EntityKind::from_str(raw).map(|k| k.name())`).  Callers should
    /// resolve the kind string first.
    pub fn resolve(
        &self,
        kind: EntityKind,
        entity_type: Option<&str>,
    ) -> Result<ResolvedType, RuntimeError> {
        let Some(raw_type) = entity_type else {
            return Ok(ResolvedType {
                kind,
                entity_type: None,
            });
        };

        let normalised = raw_type.trim().to_ascii_lowercase();

        // Try kind-qualified lookup first (unambiguous).
        let kind_key = format!("{}:{}", kind.name(), normalised);
        if let Some(&idx) = self.lookup.get(&kind_key) {
            let def = &self.defs[idx];
            return Ok(ResolvedType {
                kind,
                entity_type: Some(def.type_name.to_string()),
            });
        }

        // Try bare lookup — only valid when the bare name belongs to this kind.
        if let Some(&idx) = self.lookup.get(&normalised) {
            let def = &self.defs[idx];
            if def.kind == kind {
                return Ok(ResolvedType {
                    kind,
                    entity_type: Some(def.type_name.to_string()),
                });
            }
            // The name exists but belongs to a different kind.
            return Err(RuntimeError::InvalidInput(format!(
                "entity_type {:?} belongs to {:?}, not {:?}; valid types for {:?}: {}",
                raw_type,
                def.kind.name(),
                kind.name(),
                kind.name(),
                self.valid_types_for(kind),
            )));
        }

        // Not found at all.
        Err(RuntimeError::InvalidInput(format!(
            "unknown entity_type {:?} for {:?}; valid: {}",
            raw_type,
            kind.name(),
            self.valid_types_for(kind),
        )))
    }

    /// Comma-separated list of canonical type names valid for `kind`.
    pub fn valid_types_for(&self, kind: EntityKind) -> String {
        let mut names: Vec<&str> = self
            .defs
            .iter()
            .filter(|d| d.kind == kind)
            .map(|d| d.type_name)
            .collect();
        names.sort_unstable();
        if names.is_empty() {
            "(none registered)".to_string()
        } else {
            names.join(" | ")
        }
    }
}

// ── Module-level lazy global ─────────────────────────────────────────────────

use std::sync::OnceLock;

static GLOBAL_REGISTRY: OnceLock<EntityTypeRegistry> = OnceLock::new();

impl EntityTypeRegistry {
    /// Return a reference to the module-level built-in registry.
    ///
    /// Initialised on first access; subsequent calls are zero-cost reads.
    pub fn global() -> &'static EntityTypeRegistry {
        GLOBAL_REGISTRY.get_or_init(EntityTypeRegistry::builtin)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use khive_types::EntityKind;

    fn reg() -> EntityTypeRegistry {
        EntityTypeRegistry::builtin()
    }

    // ── Basic happy-path resolution ──────────────────────────────────────────

    #[test]
    fn resolve_paper_infers_document() {
        let r = reg();
        let res = r
            .resolve(EntityKind::Document, Some("paper"))
            .expect("paper is a valid Document subtype");
        assert_eq!(res.kind, EntityKind::Document);
        assert_eq!(res.entity_type.as_deref(), Some("paper"));
    }

    #[test]
    fn resolve_none_entity_type_always_ok() {
        let r = reg();
        for kind in EntityKind::ALL {
            let res = r.resolve(kind, None).expect("None entity_type always ok");
            assert_eq!(res.entity_type, None);
        }
    }

    #[test]
    fn resolve_algo_alias_to_algorithm() {
        let r = reg();
        let res = r
            .resolve(EntityKind::Concept, Some("algo"))
            .expect("algo is a valid alias for algorithm");
        assert_eq!(res.kind, EntityKind::Concept);
        assert_eq!(res.entity_type.as_deref(), Some("algorithm"));
    }

    #[test]
    fn resolve_spec_alias_to_specification() {
        let r = reg();
        let res = r
            .resolve(EntityKind::Document, Some("spec"))
            .expect("spec is alias for specification");
        assert_eq!(res.entity_type.as_deref(), Some("specification"));
    }

    // ── Rejection tests ──────────────────────────────────────────────────────

    #[test]
    fn reject_brain_profile_for_concept() {
        let r = reg();
        let err = r
            .resolve(EntityKind::Concept, Some("brain_profile"))
            .expect_err("brain_profile is not a Concept subtype");
        let msg = format!("{err}");
        assert!(
            msg.contains("brain_profile"),
            "error must mention the rejected type; got: {msg}"
        );
        assert!(
            msg.contains("concept"),
            "error must mention the target kind; got: {msg}"
        );
    }

    #[test]
    fn reject_unknown_subtype_with_valid_list() {
        let r = reg();
        let err = r
            .resolve(EntityKind::Document, Some("mystery_type"))
            .expect_err("mystery_type must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("mystery_type"),
            "error must echo the rejected value; got: {msg}"
        );
        // Valid subtypes for Document must appear.
        assert!(
            msg.contains("paper"),
            "error must list valid Document subtypes; got: {msg}"
        );
    }

    #[test]
    fn reject_wrong_kind_subtype_mentions_correct_kind() {
        // "paper" belongs to Document; passing it for Concept must fail.
        let r = reg();
        let err = r
            .resolve(EntityKind::Concept, Some("paper"))
            .expect_err("paper is a Document subtype, not Concept");
        let msg = format!("{err}");
        assert!(
            msg.contains("document") || msg.contains("Document"),
            "error must name the correct kind; got: {msg}"
        );
    }

    // ── Extensibility ────────────────────────────────────────────────────────

    #[test]
    fn register_brain_profile_for_concept() {
        let mut r = EntityTypeRegistry::builtin();
        r.register(EntityTypeDef {
            kind: EntityKind::Concept,
            type_name: "brain_profile",
            aliases: &[],
        });
        let res = r
            .resolve(EntityKind::Concept, Some("brain_profile"))
            .expect("brain_profile registered for Concept");
        assert_eq!(res.entity_type.as_deref(), Some("brain_profile"));
    }

    #[test]
    fn with_extra_adds_subtypes() {
        let r = EntityTypeRegistry::with_extra([EntityTypeDef {
            kind: EntityKind::Service,
            type_name: "api",
            aliases: &["endpoint"],
        }]);
        let res = r
            .resolve(EntityKind::Service, Some("endpoint"))
            .expect("endpoint alias for api");
        assert_eq!(res.entity_type.as_deref(), Some("api"));
    }

    // ── Case insensitivity ───────────────────────────────────────────────────

    #[test]
    fn resolve_is_case_insensitive() {
        let r = reg();
        let res = r
            .resolve(EntityKind::Concept, Some("Algorithm"))
            .expect("Algorithm (mixed case) must resolve");
        assert_eq!(res.entity_type.as_deref(), Some("algorithm"));
    }

    // ── valid_types_for ──────────────────────────────────────────────────────

    #[test]
    fn valid_types_for_person_is_none_registered() {
        let r = reg();
        let s = r.valid_types_for(EntityKind::Person);
        assert_eq!(
            s, "(none registered)",
            "Person has no built-in subtypes; got: {s}"
        );
    }

    #[test]
    fn valid_types_for_concept_includes_algorithm() {
        let r = reg();
        let s = r.valid_types_for(EntityKind::Concept);
        assert!(
            s.contains("algorithm"),
            "Concept valid types must include algorithm; got: {s}"
        );
    }

    // ── Global registry ──────────────────────────────────────────────────────

    #[test]
    fn global_registry_is_accessible() {
        let r = EntityTypeRegistry::global();
        let res = r
            .resolve(EntityKind::Document, Some("paper"))
            .expect("global registry must resolve paper");
        assert_eq!(res.entity_type.as_deref(), Some("paper"));
    }
}
