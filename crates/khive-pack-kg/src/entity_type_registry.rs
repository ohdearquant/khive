//! EntityTypeRegistry — validates and normalises `(EntityKind, entity_type)` pairs.

use std::collections::HashMap;

use khive_types::EntityKind;

use khive_runtime::RuntimeError;

/// Normalise a raw `entity_type` string to canonical snake_case.
///
/// Pipeline: trim → lowercase → runs of separators (space, hyphen, underscore)
/// collapsed to a single `_` → leading/trailing `_` stripped.
///
/// This implements the ADR-001:106 write-time normalisation step that precedes
/// alias resolution.
fn to_snake_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_sep = true; // treat start as separator so leading _ are stripped
    for ch in s.chars() {
        if ch == ' ' || ch == '-' || ch == '_' {
            if !prev_sep && !out.is_empty() {
                out.push('_');
                prev_sep = true;
            }
        } else {
            out.push(ch.to_ascii_lowercase());
            prev_sep = false;
        }
    }
    // Strip trailing `_` that the loop may have emitted.
    if out.ends_with('_') {
        out.pop();
    }
    out
}

/// One entry in the registry: a canonical subtype name for a specific kind,
/// together with any accepted aliases.
#[derive(Clone, Debug)]
pub struct EntityTypeDef {
    /// The entity kind this subtype belongs to.
    pub kind: EntityKind,
    /// Canonical name that is written to the DB.
    pub type_name: &'static str,
    /// Alternative spellings that are accepted at the wire level but
    /// normalised to `type_name` before storage.
    pub aliases: &'static [&'static str],
}

/// Static table of built-in subtypes (non-exhaustive; packs may extend).
///
static BUILTIN_DEFS: &[EntityTypeDef] = &[
    // ── Document ────────────────────────────────────────────────────────────
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
        type_name: "blog_post",
        aliases: &["blog"],
    },
    EntityTypeDef {
        kind: EntityKind::Document,
        type_name: "book",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Document,
        type_name: "specification",
        aliases: &["spec"],
    },
    EntityTypeDef {
        kind: EntityKind::Document,
        type_name: "documentation",
        aliases: &["docs"],
    },
    EntityTypeDef {
        kind: EntityKind::Document,
        type_name: "thesis",
        aliases: &[],
    },
    // ── Concept ─────────────────────────────────────────────────────────────
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "algorithm",
        aliases: &["algo"],
    },
    // ── Formal math ─────────────────────────────────────────────────────────
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "theorem",
        aliases: &["lemma", "proposition", "corollary"],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "definition",
        aliases: &["def"],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "structure",
        aliases: &["inductive", "struct", "class"],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "instance",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "axiom",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "goal",
        aliases: &["proof_goal"],
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
        // "model" is the alias; canonical name is "model_family"
        // to distinguish from a Dataset or Artifact trained model instance.
        type_name: "model_family",
        aliases: &["model"],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "theory",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "research_gap",
        aliases: &["gap"],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "design_pattern",
        aliases: &["pattern"],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "mathematical_operation",
        aliases: &["math_op"],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "metric",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "objective",
        aliases: &["loss"],
    },
    // ── Code (ADR-085) ──────────────────────────────────────────────────────
    // "struct" and "class" are not registered as aliases here: formal-math
    // "structure" already owns them (see above), and ADR-085 requires
    // ingesters to write the canonical code tokens instead.
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "module",
        aliases: &["mod", "namespace"],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "function",
        aliases: &["fn", "func", "method"],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "datatype",
        aliases: &["enum", "record", "type_alias"],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "interface",
        aliases: &["trait", "protocol"],
    },
    // ── Dataset ──────────────────────────────────────────────────────────────
    // benchmark belongs to Dataset, not Concept (it evaluates models, it is not itself a concept).
    EntityTypeDef {
        kind: EntityKind::Dataset,
        type_name: "benchmark",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Dataset,
        type_name: "corpus",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Dataset,
        type_name: "training_set",
        aliases: &["train_set"],
    },
    EntityTypeDef {
        kind: EntityKind::Dataset,
        type_name: "evaluation_set",
        aliases: &["eval_set"],
    },
    EntityTypeDef {
        kind: EntityKind::Dataset,
        type_name: "test_set",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Dataset,
        type_name: "synthetic_dataset",
        aliases: &["synthetic"],
    },
    // ── Project ─────────────────────────────────────────────────────────────
    // Project subtypes: library, framework, tool, application, repository.
    // "service" and "svc" are omitted — EntityKind::Service handles running
    // instances; a service codebase repo is Project + application or tool.
    EntityTypeDef {
        kind: EntityKind::Project,
        type_name: "library",
        aliases: &["lib"],
    },
    EntityTypeDef {
        kind: EntityKind::Project,
        type_name: "framework",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Project,
        type_name: "tool",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Project,
        type_name: "application",
        aliases: &["app"],
    },
    EntityTypeDef {
        kind: EntityKind::Project,
        type_name: "repository",
        aliases: &["repo"],
    },
    // ── Org ──────────────────────────────────────────────────────────────────
    EntityTypeDef {
        kind: EntityKind::Org,
        type_name: "academic_institution",
        aliases: &["university", "uni"],
    },
    EntityTypeDef {
        kind: EntityKind::Org,
        type_name: "company",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Org,
        type_name: "research_lab",
        aliases: &["lab"],
    },
    EntityTypeDef {
        kind: EntityKind::Org,
        type_name: "nonprofit",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Org,
        type_name: "government_agency",
        aliases: &["gov_agency"],
    },
    EntityTypeDef {
        kind: EntityKind::Org,
        type_name: "consortium",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Org,
        type_name: "standards_body",
        aliases: &[],
    },
    // ── Artifact ─────────────────────────────────────────────────────────────
    EntityTypeDef {
        kind: EntityKind::Artifact,
        type_name: "checkpoint",
        aliases: &["ckpt"],
    },
    EntityTypeDef {
        kind: EntityKind::Artifact,
        type_name: "snapshot",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Artifact,
        type_name: "export",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Artifact,
        type_name: "embedding_index",
        aliases: &["embed_index"],
    },
    EntityTypeDef {
        kind: EntityKind::Artifact,
        type_name: "state_bundle",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Artifact,
        type_name: "profile",
        aliases: &[],
    },
    // ── Service ──────────────────────────────────────────────────────────────
    // Service subtypes: inference_engine, retrieval_engine,
    // embedding_engine, api, database, search_engine, mcp_server.
    EntityTypeDef {
        kind: EntityKind::Service,
        type_name: "inference_engine",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Service,
        type_name: "retrieval_engine",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Service,
        type_name: "embedding_engine",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Service,
        type_name: "api",
        aliases: &["endpoint"],
    },
    EntityTypeDef {
        kind: EntityKind::Service,
        type_name: "database",
        aliases: &["db"],
    },
    EntityTypeDef {
        kind: EntityKind::Service,
        type_name: "search_engine",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Service,
        type_name: "mcp_server",
        aliases: &["mcp"],
    },
    // Person  — no standard subtypes (roles are metadata, not subtypes).
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

/// Registry for `(EntityKind, entity_type)` pair validation and alias normalisation.
#[derive(Clone)]
pub struct EntityTypeRegistry {
    /// `alias_or_name (lowercase) → def index`.  Covers both canonical names
    /// and all registered aliases.
    lookup: HashMap<String, usize>,
    defs: Vec<EntityTypeDef>,
}

impl EntityTypeRegistry {
    /// Build a fresh registry from the supplied definitions.
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

    /// Return the built-in registry with all static subtype definitions.
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

    /// Validate and normalise a `(kind, entity_type)` wire pair.
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

        let normalised = to_snake_case(raw_type.trim());

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
        // Use a pack-specific subtype that is NOT in BUILTIN_DEFS.
        let r = EntityTypeRegistry::with_extra([EntityTypeDef {
            kind: EntityKind::Service,
            type_name: "grpc_service",
            aliases: &["grpc"],
        }]);
        let res = r
            .resolve(EntityKind::Service, Some("grpc"))
            .expect("grpc alias for grpc_service");
        assert_eq!(res.entity_type.as_deref(), Some("grpc_service"));
    }

    #[test]
    fn service_api_is_builtin() {
        // F1 fix: Service subtypes are now in BUILTIN_DEFS; "api"/"endpoint" must
        // resolve without any pack extension.
        let r = reg();
        let res = r
            .resolve(EntityKind::Service, Some("api"))
            .expect("api is a built-in Service subtype");
        assert_eq!(res.entity_type.as_deref(), Some("api"));
        let res2 = r
            .resolve(EntityKind::Service, Some("endpoint"))
            .expect("endpoint is an alias for api");
        assert_eq!(res2.entity_type.as_deref(), Some("api"));
    }

    #[test]
    fn service_mcp_server_is_builtin() {
        let r = reg();
        let res = r
            .resolve(EntityKind::Service, Some("mcp_server"))
            .expect("mcp_server is a built-in Service subtype");
        assert_eq!(res.entity_type.as_deref(), Some("mcp_server"));
        let res2 = r
            .resolve(EntityKind::Service, Some("mcp"))
            .expect("mcp is an alias for mcp_server");
        assert_eq!(res2.entity_type.as_deref(), Some("mcp_server"));
    }

    #[test]
    fn project_has_no_service_subtype() {
        // F1 fix: "service"/"svc" must no longer resolve under Project.
        let r = reg();
        r.resolve(EntityKind::Project, Some("service"))
            .expect_err("service must not be a Project subtype");
        r.resolve(EntityKind::Project, Some("svc"))
            .expect_err("svc must not be a Project subtype");
    }

    #[test]
    fn benchmark_is_dataset_not_concept() {
        // F3 fix: benchmark belongs to Dataset, not Concept (it evaluates, it is not a concept).
        let r = reg();
        let res = r
            .resolve(EntityKind::Dataset, Some("benchmark"))
            .expect("benchmark is a valid Dataset subtype");
        assert_eq!(res.entity_type.as_deref(), Some("benchmark"));
        r.resolve(EntityKind::Concept, Some("benchmark"))
            .expect_err("benchmark must not be a Concept subtype");
    }

    #[test]
    fn model_alias_resolves_to_model_family() {
        // F3 fix: "model" is an accepted alias for the canonical name "model_family".
        let r = reg();
        let res = r
            .resolve(EntityKind::Concept, Some("model"))
            .expect("model is an alias for model_family");
        assert_eq!(res.entity_type.as_deref(), Some("model_family"));
        let res2 = r
            .resolve(EntityKind::Concept, Some("model_family"))
            .expect("model_family is the canonical name");
        assert_eq!(res2.entity_type.as_deref(), Some("model_family"));
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

    #[test]
    fn resolve_inductive_alias_to_structure() {
        let r = reg();
        let res = r
            .resolve(EntityKind::Concept, Some("inductive"))
            .expect("inductive is a valid alias for structure");
        assert_eq!(res.kind, EntityKind::Concept);
        assert_eq!(res.entity_type.as_deref(), Some("structure"));
    }

    #[test]
    fn resolve_goal_subtype() {
        let r = reg();
        let res = r
            .resolve(EntityKind::Concept, Some("goal"))
            .expect("goal is a valid Concept subtype");
        assert_eq!(res.kind, EntityKind::Concept);
        assert_eq!(res.entity_type.as_deref(), Some("goal"));
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

    // ── snake_case normalisation (ADR-001:106) ───────────────────────────────

    #[test]
    fn to_snake_case_converts_hyphen_to_underscore() {
        assert_eq!(to_snake_case("proof-goal"), "proof_goal");
    }

    #[test]
    fn to_snake_case_converts_space_to_underscore() {
        assert_eq!(to_snake_case("Proof Goal"), "proof_goal");
    }

    #[test]
    fn to_snake_case_collapses_mixed_separators() {
        assert_eq!(to_snake_case("proof - goal"), "proof_goal");
        assert_eq!(to_snake_case("proof__goal"), "proof_goal");
    }

    #[test]
    fn to_snake_case_strips_leading_trailing_separators() {
        assert_eq!(to_snake_case("-theorem-"), "theorem");
        assert_eq!(to_snake_case(" theorem "), "theorem");
    }

    #[test]
    fn resolve_proof_goal_hyphen_normalises_to_goal() {
        // ADR-001:106: "proof-goal" must normalise to "proof_goal" and then
        // alias-resolve to the canonical name "goal".
        let r = reg();
        let res = r
            .resolve(EntityKind::Concept, Some("proof-goal"))
            .expect("proof-goal must resolve via snake_case normalisation + alias");
        assert_eq!(res.entity_type.as_deref(), Some("goal"));
    }

    #[test]
    fn resolve_proof_goal_space_normalises_to_goal() {
        let r = reg();
        let res = r
            .resolve(EntityKind::Concept, Some("Proof Goal"))
            .expect("Proof Goal must resolve via snake_case normalisation + alias");
        assert_eq!(res.entity_type.as_deref(), Some("goal"));
    }

    #[test]
    fn resolve_blog_hyphen_normalises_to_blog_post_non_formal() {
        // Prove the fix is not formal-only: "blog-post" (a Document subtype) must
        // normalise and resolve to the canonical name "blog_post".
        let r = reg();
        let res = r
            .resolve(EntityKind::Document, Some("blog-post"))
            .expect("blog-post must normalise to blog_post");
        assert_eq!(res.entity_type.as_deref(), Some("blog_post"));
    }

    // ── ADR-085 code subtypes ────────────────────────────────────────────────

    #[test]
    fn entity_type_registry_accepts_code_tokens_and_aliases() {
        let r = reg();
        for (raw, canonical) in [
            ("module", "module"),
            ("mod", "module"),
            ("namespace", "module"),
            ("function", "function"),
            ("fn", "function"),
            ("func", "function"),
            ("method", "function"),
            ("datatype", "datatype"),
            ("enum", "datatype"),
            ("record", "datatype"),
            ("type_alias", "datatype"),
            ("interface", "interface"),
            ("trait", "interface"),
            ("protocol", "interface"),
        ] {
            let res = r
                .resolve(EntityKind::Concept, Some(raw))
                .unwrap_or_else(|e| panic!("{raw:?} must resolve for Concept: {e}"));
            assert_eq!(
                res.entity_type.as_deref(),
                Some(canonical),
                "{raw:?} must resolve to {canonical:?}"
            );
        }
    }

    #[test]
    fn entity_type_registry_does_not_claim_struct_or_class_for_code() {
        let r = reg();
        let struct_res = r
            .resolve(EntityKind::Concept, Some("struct"))
            .expect("struct remains a valid Concept subtype (owned by formal structure)");
        assert_eq!(
            struct_res.entity_type.as_deref(),
            Some("structure"),
            "struct must still resolve to formal-math structure, not datatype"
        );
        let class_res = r
            .resolve(EntityKind::Concept, Some("class"))
            .expect("class remains a valid Concept subtype (owned by formal structure)");
        assert_eq!(
            class_res.entity_type.as_deref(),
            Some("structure"),
            "class must still resolve to formal-math structure, not datatype"
        );
    }
}
