//! Pack trait — the declarative composition unit for khive.
//!
//! A pack declares vocabulary (note kinds, entity kinds), verbs, and edge
//! endpoint rules. This is purely static metadata — no I/O, no async.
//! Runtime dispatch lives in `khive-runtime` (`PackRuntime` trait +
//! `VerbRegistry`).
//!
//! This trait lives in khive-types (no_std, zero deps) so downstream crates
//! can reference pack metadata without pulling in the full runtime.

use crate::edge::EdgeRelation;

/// Visibility tier for a handler.
///
/// `Verb` entries appear on the MCP wire and are invokable by agents.
/// `Subhandler` entries are internal — callable by the operator via CLI
/// but not surfaced as top-level MCP verbs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    /// Externally invokable via MCP `request` tool.
    Verb,
    /// Internal — operator-only via `kkernel call <pack> <handler>`.
    Subhandler,
}

/// Illocutionary force classification for a verb handler.
///
/// Follows Searle's five speech-act categories (1976). Every `Visibility::Verb`
/// handler in the MCP surface MUST carry a category. `Subhandler` entries may
/// use the category of their parent verb or `Assertive` as a sensible default.
///
/// The category is a documentation / introspection tag. It is NOT used for
/// permission checking, transport routing, or return-shape selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerbCategory {
    /// Speaker represents a state of affairs — retrieves and presents facts.
    /// Examples: `get`, `list`, `search`, `recall`.
    Assertive,
    /// Speaker attempts to get the hearer to do something.
    /// Examples: `assign`, `transition`.
    Directive,
    /// Speaker commits to a persistent change.
    /// Examples: `create`, `remember`, `link`, `send`.
    Commissive,
    /// Speaker changes institutional status by fiat.
    /// Examples: `update`, `delete`, `merge`, `complete`.
    Declaration,
    // `Expressive` is intentionally absent — no verb currently uses it.
}

/// Parameter type for `help=true` schema envelopes.
///
/// Declares the name, type hint, required flag, and one-line description for
/// a single verb parameter. Stored as a `&'static` slice on [`HandlerDef`] so
/// the registry can return it without any allocation at call time.
///
/// The `param_type` field is a free-form string (e.g. `"string"`, `"uuid"`,
/// `"bool"`, `"integer"`, `"string | null"`) — it is documentation-only and
/// not used for validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParamDef {
    /// Parameter name as used in the DSL (e.g. `"id"`, `"kind"`, `"query"`).
    pub name: &'static str,
    /// Free-form type hint for documentation (e.g. `"string"`, `"uuid"`, `"bool"`).
    pub param_type: &'static str,
    /// Whether the caller must supply this parameter.
    pub required: bool,
    /// One-line human-readable description.
    pub description: &'static str,
}

/// Handler metadata for discovery and documentation.
///
/// Replaces the previous `VerbDef`. Every entry carries a `visibility` tag
/// so the registry can separate the MCP-exposed surface from internal handlers,
/// and a `category` that classifies the illocutionary force of the verb
/// per the speech-act taxonomy.
///
/// The `params` slice is used by `VerbRegistry::describe_verb` to build the
/// `help=true` schema envelope. Packs that predate this field leave it empty
/// (`&[]`) which is backward-compatible — callers receive a schema envelope
/// with zero params rather than an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandlerDef {
    pub name: &'static str,
    pub description: &'static str,
    pub visibility: Visibility,
    /// Illocutionary force classification. Use `Assertive` for `Subhandler`
    /// entries that have no external callers.
    pub category: VerbCategory,
    /// Parameter schema for `help=true` introspection (issue #287).
    ///
    /// Empty (`&[]`) is the correct default for handlers that predate this
    /// field or have no fixed parameter schema (e.g. free-form query verbs).
    pub params: &'static [ParamDef],
}

/// Presentation override for a verb handler.
///
/// Most verbs use the default `Standard` policy which allows the caller's
/// requested `PresentationMode` to apply.  A small set declare `AlwaysVerbose`
/// because Agent-mode trimming (UUID shortening, empty-field dropping) would
/// corrupt their response for downstream chaining — e.g. `get` returns UUIDs
/// that callers pipe into `link`; shortening them here breaks the chain.
///
/// The policy is carried as a `const` in [`HandlerDef`] so the registry can
/// consult it before applying the presentation transform.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VerbPresentationPolicy {
    /// Apply the caller's requested `PresentationMode` unchanged.
    #[default]
    Standard,
    /// Always use `Verbose` output regardless of the caller's mode.
    ///
    /// Declared verbs: `get`, `link`, `query`, `traverse`, `neighbors`,
    /// `brain.feedback`.
    ///
    /// `link` is included because the returned edge ID is the only handle for
    /// follow-up `neighbors`/`traverse` calls; short-form IDs risk prefix
    /// collision at scale (~65K edges can share an 8-char prefix).
    ///
    /// `brain.feedback` is included because callers chain `target_id` from the
    /// response back into subsequent feedback or profile queries; an 8-char
    /// prefix is ambiguous and defeats the acknowledged-ID contract (#545).
    AlwaysVerbose,
}

impl HandlerDef {
    /// Resolve the presentation policy for this handler.
    ///
    /// Returns [`VerbPresentationPolicy::AlwaysVerbose`] for verbs whose
    /// semantics demand full output (full UUIDs, complete timestamps) regardless
    /// of the caller's requested presentation mode.
    ///
    /// New verbs that need this override must be added here; omission from the
    /// list means `Standard` applies.
    pub fn presentation_policy(&self) -> VerbPresentationPolicy {
        // AlwaysVerbose verbs bypass agent-mode transforms entirely.
        //
        // `link` is AlwaysVerbose because the edge ID returned is the only handle
        // for follow-up `neighbors`/`traverse` calls. At scale, two edges can share
        // the same 8-char prefix (birthday collision ~65K edges), so shortening the
        // edge ID in agent mode breaks downstream chaining.
        match self.name {
            "get" | "link" | "query" | "traverse" | "neighbors" | "brain.feedback" => {
                VerbPresentationPolicy::AlwaysVerbose
            }
            _ => VerbPresentationPolicy::Standard,
        }
    }
}

/// Backward-compatible type alias.  Existing code that names `VerbDef` still
/// compiles; new code should use `HandlerDef` directly.
#[deprecated(since = "0.2.0", note = "Use HandlerDef instead")]
pub type VerbDef = HandlerDef;

/// Match spec for one end of an [`EdgeEndpointRule`].
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

/// A pack-declared endpoint rule for a specific edge relation.
///
/// Rules are **additive**: they extend the set of allowed
/// `(source, relation, target)` triples beyond the base contract.
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

/// Lifecycle specification for a note kind.
///
/// Declares which field holds the kind's domain state, the initial value,
/// terminal values, and allowed transitions.  The runtime uses this to
/// validate lifecycle operations at the verb boundary without hard-coding
/// kind-specific logic in the shared CRUD path.
///
/// Phase 1 (current): packs declare the spec; the runtime records it for
/// documentation and future enforcement.
/// Phase 2 (future): the runtime uses `field` to route lifecycle writes
/// to a first-class column rather than `properties`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteLifecycleSpec {
    /// The field name that holds the kind's lifecycle state.
    ///
    /// Use `"kind_status"` for pack-owned lifecycle fields to avoid the
    /// semantic collision with `Note.status` (NoteStatus).
    pub field: &'static str,
    /// The value assigned when a note of this kind is first created.
    pub initial: &'static str,
    /// Values from which no further transitions are possible.
    pub terminal: &'static [&'static str],
    /// Allowed `(from, to)` transitions. `"*"` as `from` matches any state.
    pub transitions: &'static [(&'static str, &'static str)],
}

/// Kind-level schema specification for a note kind.
///
/// Each pack-registered note kind may declare a `NoteKindSpec` to describe
/// its lifecycle semantics.  The runtime collects these at boot time via
/// [`Pack::NOTE_KIND_SPECS`] for documentation, introspection, and future
/// enforcement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteKindSpec {
    /// The note kind string this spec governs (e.g. `"task"`).
    pub kind: &'static str,
    /// Alternate names this kind accepts on the wire.
    pub aliases: &'static [&'static str],
    /// Lifecycle state machine for this kind.
    pub lifecycle: NoteLifecycleSpec,
}

/// DDL statements the pack needs applied to the auxiliary schema.
///
/// Pack-auxiliary tables use idempotent `CREATE TABLE IF NOT EXISTS`; they are
/// not part of the core versioned migration chain.  The runtime applies these
/// statements once at pack registration time (or startup) against the active
/// storage backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackSchemaPlan {
    /// The pack this schema plan belongs to (used for error reporting).
    pub pack: &'static str,
    /// Idempotent SQL statements to apply.
    pub statements: &'static [&'static str],
}

/// A composable module that contributes vocabulary, verbs, and edge endpoint
/// rules to the khive runtime.
///
/// Packs declare what entity kinds, note kinds, and verbs they introduce, and
/// optionally extend the per-relation endpoint contract via [`EDGE_RULES`].
/// The runtime merges vocabularies from all loaded packs and rejects
/// unregistered kinds at the service boundary.
///
/// The closed [`EdgeRelation`] enum is not extensible — only its
/// per-relation endpoint contract is extensible by packs.
///
/// [`EDGE_RULES`]: Pack::EDGE_RULES
pub trait Pack {
    /// Short identifier for this pack (e.g. "kg", "tasks").
    const NAME: &'static str;

    /// Note kinds this pack contributes to the runtime vocabulary.
    const NOTE_KINDS: &'static [&'static str];

    /// Entity kinds this pack contributes to the runtime vocabulary.
    const ENTITY_KINDS: &'static [&'static str];

    /// Handlers this pack registers.
    ///
    /// The runtime routes verb calls to the pack that declares them.
    /// Only entries with `visibility: Visibility::Verb` are surfaced on the
    /// MCP wire; `Visibility::Subhandler` entries are internal.
    const HANDLERS: &'static [HandlerDef];

    /// Additional edge endpoint rules this pack contributes.
    ///
    /// Defaults to empty — packs that introduce no new endpoint pairs (or
    /// only rely on the base endpoint contract) can ignore this.
    const EDGE_RULES: &'static [EdgeEndpointRule] = &[];

    /// Other pack names whose vocabulary this pack references.
    ///
    /// The runtime checks that every name in `REQUIRES` appears in the
    /// loaded pack set before any pack is registered. Defaults to empty
    /// so existing packs compile without changes.
    const REQUIRES: &'static [&'static str] = &[];

    /// Lifecycle and schema specs for note kinds this pack owns.
    ///
    /// Packs that introduce note kinds with explicit lifecycle semantics
    /// (e.g. GTD's `task` kind) declare the spec here.  The runtime collects
    /// these at boot time for introspection and future enforcement.  Defaults
    /// to empty so existing packs compile without changes.
    const NOTE_KIND_SPECS: &'static [NoteKindSpec] = &[];

    /// Pack-auxiliary schema plan.
    ///
    /// Packs that need their own auxiliary tables (e.g. GTD's
    /// `gtd_lifecycle_audit`) declare idempotent DDL statements here.
    /// The runtime applies them once at registration time.  Defaults to
    /// `None` so packs with no auxiliary schema cost nothing.
    const SCHEMA_PLAN: Option<PackSchemaPlan> = None;

    /// Validation rule IDs contributed by this pack.
    ///
    /// Rule IDs are namespaced by pack name: `<pack-name>/<rule-id>`.
    /// The runtime merges rule IDs from all packs; the actual rule
    /// implementations live in `khive-runtime::validation::ValidationRule`
    /// (not in `khive-types`, which stays `no_std`). This const serves as
    /// the declarative catalog of rule identifiers so the validation
    /// infrastructure can enumerate what rules a pack claims without
    /// loading the runtime.
    ///
    /// Defaults to empty — packs with no domain-specific validation rules
    /// can leave this unset.
    const VALIDATION_RULES: &'static [&'static str] = &[];
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestPack;

    impl Pack for TestPack {
        const NAME: &'static str = "test";
        const NOTE_KINDS: &'static [&'static str] = &["memo"];
        const ENTITY_KINDS: &'static [&'static str] = &["widget"];
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "do_thing",
            description: "does a thing",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &[],
        }];
    }

    #[test]
    fn pack_trait_compiles() {
        assert_eq!(TestPack::NAME, "test");
        assert_eq!(TestPack::NOTE_KINDS, &["memo"]);
        assert_eq!(TestPack::ENTITY_KINDS, &["widget"]);
        assert_eq!(TestPack::HANDLERS.len(), 1);
        assert_eq!(TestPack::HANDLERS[0].name, "do_thing");
        assert_eq!(TestPack::HANDLERS[0].visibility, Visibility::Verb);
        assert_eq!(TestPack::HANDLERS[0].category, VerbCategory::Commissive);
    }

    #[test]
    fn verb_category_variants_exist() {
        // Just ensuring the enum variants are accessible — no runtime assertion
        // needed beyond confirming they exist at compile time.
        let _ = VerbCategory::Assertive;
        let _ = VerbCategory::Directive;
        let _ = VerbCategory::Commissive;
        let _ = VerbCategory::Declaration;
    }

    #[test]
    fn pack_validation_rules_default_empty() {
        assert!(TestPack::VALIDATION_RULES.is_empty());
    }

    // `link` must be AlwaysVerbose so edge IDs are not shortened.
    #[test]
    fn link_handler_is_always_verbose() {
        let link_def = HandlerDef {
            name: "link",
            description: "Create a typed directed edge",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &[],
        };
        assert_eq!(
            link_def.presentation_policy(),
            VerbPresentationPolicy::AlwaysVerbose,
            "link must be AlwaysVerbose"
        );
    }

    // AlwaysVerbose set regression: ensure get/query/traverse/neighbors/brain.feedback remain.
    #[test]
    fn always_verbose_set_contains_expected_verbs() {
        let always_verbose = [
            "get",
            "link",
            "query",
            "traverse",
            "neighbors",
            "brain.feedback",
        ];
        for name in always_verbose {
            let h = HandlerDef {
                name,
                description: "",
                visibility: Visibility::Verb,
                category: VerbCategory::Assertive,
                params: &[],
            };
            assert_eq!(
                h.presentation_policy(),
                VerbPresentationPolicy::AlwaysVerbose,
                "{name:?} must be AlwaysVerbose"
            );
        }
    }

    // Standard policy for all other verbs.
    #[test]
    fn non_verbose_verbs_are_standard_policy() {
        let standard = [
            "create", "list", "update", "delete", "search", "recall", "remember",
        ];
        for name in standard {
            let h = HandlerDef {
                name,
                description: "",
                visibility: Visibility::Verb,
                category: VerbCategory::Commissive,
                params: &[],
            };
            assert_eq!(
                h.presentation_policy(),
                VerbPresentationPolicy::Standard,
                "{name:?} must be Standard (not AlwaysVerbose)"
            );
        }
    }
}
