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
    /// Internal — operator-only via `kkernel exec '<pack>.<handler>(...)'`.
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
    /// An entity whose base `kind` AND `entity_type` subtype both match the
    /// given strings (e.g. `kind: "concept", entity_type: "theorem"`). Both
    /// fields must match — enforcing the `(EntityKind, entity_type)` registry
    /// invariant required by ADR-001:102. Required for granular entity subtypes
    /// (formal-math theorem/definition, AMR gene/drug/pathogen): `EntityOfKind`
    /// only sees the base kind (`"concept"`), so an `EntityOfKind("theorem")`
    /// rule is silently inert. Additive — tightens nothing in the closed relation
    /// set.
    EntityOfType {
        /// Base entity kind that must match (e.g. `"concept"`).
        kind: &'static str,
        /// Canonical `entity_type` subtype that must match (e.g. `"theorem"`).
        entity_type: &'static str,
    },
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

/// ADR-099 D3 — the v1 atomic-admissible verb set for `--atomic` bulk apply.
///
/// This is an EXPLICIT per-verb allowlist, never derived from [`VerbCategory`]
/// or any other classification ("never a pack-level category", ADR-099 D3).
/// Every verb here has a prepare/apply seam whose in-transaction phase reduces
/// to synchronous DML — the atomic-unit suspend-free invariant
/// (`SqlAccess::atomic_unit` in `khive-storage`). Extending this list is a
/// design decision (ADR-099 amendment), not a code-review-only change; the
/// `atomic_admissible_list_matches_adr` test below pins this exact set so an
/// edit here forces the editor to touch that test and its ADR citation.
pub const ATOMIC_ADMISSIBLE_VERBS: &[&str] = &[
    "update",
    "delete",
    "link",
    "merge",
    "gtd.transition",
    "gtd.complete",
    "propose",
    "review",
    "withdraw",
];

/// Verbs rejected under `--atomic` because their write still computes an
/// embedding synchronously and no prepare/apply seam hoists that embedding
/// out of the transaction yet (ADR-099 D3, "v1 rejected — embedding-bearing").
const ATOMIC_EMBEDDING_BEARING_VERBS: &[&str] = &[
    "create",
    "memory.remember",
    "gtd.assign",
    "comm.send",
    "comm.reply",
    "comm.ingest",
];

/// Verbs that ADR-099 D3 lists as conceptually admissible (they remain on
/// [`ATOMIC_ADMISSIBLE_VERBS`] — the ADR intends each to eventually gain a
/// prepare/apply seam) but for which no *full-parity* seam exists yet in this
/// slice, so they are rejected up front rather than admitted with a gap:
///
/// - `propose` / `review` / `withdraw` (ADR-046's event-sourced change-proposal
///   lifecycle): their apply path is a changeset-interpreter over a dedicated
///   `proposals_open` table, not a small number of guarded DML statements —
///   no prepare implementation exists at all.
/// - `merge`: a full-parity atomic prepare (field folding, survivor FTS/vector
///   reindex, loser index purge, merge provenance, same-kind rejection,
///   graceful edge-conflict resolution — see `curation::merge_entity_sql`) was
///   drafted and unit-tested in the B3 fix round, but deferred rather than
///   shipped: `curation.rs`'s edge-rewire conflict handling does per-row
///   procedural branching (read, canonicalize, probe for a conflicting
///   triple, delete-and-refresh vs. update-in-place) that cannot be expressed
///   as ADR-099 D1's static predicate/guard plan shape, so full parity is not
///   achievable without either accepting a documented behavioral gap or a
///   design change to the plan model — bias-toward-defer (Leo directive,
///   fix-round refinement) over shipping a partially-scoped atomic merge.
///   `merge` stays admissible under the *non-atomic* verb.
///
/// B3 fix round (codex REJECT, Medium finding — governance verbs): this set
/// previously passed the static pre-runtime admissibility check (since it's a
/// subset of `ATOMIC_ADMISSIBLE_VERBS`) and only failed later, inside
/// `atomic_prepare::prepare_op`, AFTER `KhiveRuntime::new` had already run.
/// `atomic_admissibility` now checks this set FIRST so the CLI boundary
/// (`khive_request::atomic::check_atomic_admissible`) rejects these verbs
/// before any runtime is built or any write attempted — the same
/// before-any-write guarantee every other rejection class gets.
pub const ATOMIC_KNOWN_UNIMPLEMENTED_VERBS: &[&str] = &["propose", "review", "withdraw", "merge"];

/// Read verbs rejected under `--atomic` — they produce no write plan to apply
/// (ADR-099 D3, "v1 rejected — reads").
const ATOMIC_READ_VERBS: &[&str] = &[
    "search",
    "recall",
    "query",
    "traverse",
    "list",
    "get",
    "neighbors",
    "context",
    "stats",
    "verbs",
];

/// Conservative default maximum op count for one `--atomic` unit (ADR-099
/// migration step 7 / B3). The ADR does not pin an exact number — D2 defers
/// the precise threshold to harness measurement ("a recommended default on
/// the order of a few thousand ops ... configurable with a conservative
/// default", Open Question 2) — so this constant is an explicit interim
/// choice, not a value read out of the ADR text. Rationale for 2000: it is
/// inside D2's "a few thousand" band, comfortably bounds the duration of the
/// single cross-process `BEGIN IMMEDIATE` hold an atomic unit takes on the
/// daemon's writer lock (ADR-099 D5 daemon-coexistence), and is cheap to
/// override per invocation (`kkernel exec --atomic --atomic-max-ops N`)
/// without touching this default. Revisit once the load-harness (ADR-067
/// Component A) has real per-op-count latency data under contention.
pub const ATOMIC_MAX_OPS_DEFAULT: usize = 2000;

/// Why a verb was rejected from an `--atomic` op list (ADR-099 D3, migration
/// step 2). Distinguishes the two named rejection classes from a generic
/// "not yet admitted" fallback so callers can produce an actionable message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtomicRejectionReason {
    /// The verb still computes an embedding synchronously in its write path.
    EmbeddingBearing,
    /// The verb is a read — it has no write plan to apply.
    Read,
    /// Neither on the v1 admissible list nor a known rejected category (e.g.
    /// a verb added after this list was written). Rejected by default —
    /// admissibility is opt-in, never inferred.
    Unlisted,
    /// On [`ATOMIC_ADMISSIBLE_VERBS`] per ADR-099 D3 (conceptually admissible,
    /// intended to gain a seam) but has no prepare/apply implementation in
    /// this slice yet ([`ATOMIC_KNOWN_UNIMPLEMENTED_VERBS`]). Rejected at the
    /// same pre-runtime static-guard stage as every other rejection reason —
    /// never silently no-opped, never deferred until after a runtime/write
    /// attempt.
    KnownUnimplemented,
}

/// Static admissibility classification for `verb_name` under ADR-099
/// `--atomic` bulk apply.
///
/// Returns `None` when the verb is admissible; `Some(reason)` names why it is
/// rejected. Default-deny: a verb name absent from every list here is
/// [`AtomicRejectionReason::Unlisted`], never silently admitted.
///
/// `ATOMIC_KNOWN_UNIMPLEMENTED_VERBS` is checked BEFORE the general
/// admissible-list membership check (B3 fix round, Medium finding): those
/// verbs are members of `ATOMIC_ADMISSIBLE_VERBS`, so checking membership
/// first would admit them (`None`) and defer their rejection to prepare time,
/// after a runtime has already been constructed.
pub fn atomic_admissibility(verb_name: &str) -> Option<AtomicRejectionReason> {
    if ATOMIC_KNOWN_UNIMPLEMENTED_VERBS.contains(&verb_name) {
        return Some(AtomicRejectionReason::KnownUnimplemented);
    }
    if ATOMIC_ADMISSIBLE_VERBS.contains(&verb_name) {
        return None;
    }
    if ATOMIC_EMBEDDING_BEARING_VERBS.contains(&verb_name) {
        return Some(AtomicRejectionReason::EmbeddingBearing);
    }
    if ATOMIC_READ_VERBS.contains(&verb_name) {
        return Some(AtomicRejectionReason::Read);
    }
    Some(AtomicRejectionReason::Unlisted)
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

    // ── ADR-099 D3 atomic admissibility ────────────────────────────────────

    // Drift-pin: a hardcoded copy of the ADR-099 D3 v1 admissible list. If
    // someone edits `ATOMIC_ADMISSIBLE_VERBS`, this test fails until they also
    // update this literal — forcing a look at ADR-099 D3 ("Decision: admit
    // only verbs that expose a prepare/apply seam...") before the set changes.
    #[test]
    fn atomic_admissible_list_matches_adr099_d3() {
        let adr_099_d3_v1_admissible_set: &[&str] = &[
            "update",
            "delete",
            "link",
            "merge",
            "gtd.transition",
            "gtd.complete",
            "propose",
            "review",
            "withdraw",
        ];
        assert_eq!(
            ATOMIC_ADMISSIBLE_VERBS, adr_099_d3_v1_admissible_set,
            "ATOMIC_ADMISSIBLE_VERBS drifted from ADR-099 D3's explicit v1 list"
        );
    }

    #[test]
    fn atomic_admissible_verbs_are_admitted() {
        for verb in ATOMIC_ADMISSIBLE_VERBS {
            // Governance verbs are on ATOMIC_ADMISSIBLE_VERBS per ADR-099 D3
            // (conceptually admissible) but are checked separately below:
            // they are rejected at this same static layer for a distinct
            // reason (KnownUnimplemented), not admitted (None).
            if ATOMIC_KNOWN_UNIMPLEMENTED_VERBS.contains(verb) {
                continue;
            }
            assert_eq!(
                atomic_admissibility(verb),
                None,
                "{verb:?} is on the v1 admissible list and must be admitted"
            );
        }
    }

    #[test]
    fn atomic_known_unimplemented_verbs_rejected_before_runtime() {
        // B3 fix round (codex REJECT, Medium finding): propose/review/withdraw
        // remain on ATOMIC_ADMISSIBLE_VERBS (ADR-099 D3 intends them to gain a
        // seam) but must be rejected at this SAME static pre-runtime guard —
        // not admitted here and only failed later inside
        // `atomic_prepare::prepare_op` after a runtime was already built.
        for verb in ATOMIC_KNOWN_UNIMPLEMENTED_VERBS {
            assert!(
                ATOMIC_ADMISSIBLE_VERBS.contains(verb),
                "{verb:?} must remain on ATOMIC_ADMISSIBLE_VERBS per ADR-099 D3"
            );
            assert_eq!(
                atomic_admissibility(verb),
                Some(AtomicRejectionReason::KnownUnimplemented),
                "{verb:?} must be rejected as known-unimplemented, not admitted"
            );
        }
    }

    #[test]
    fn atomic_embedding_bearing_verbs_rejected_named() {
        for verb in [
            "create",
            "memory.remember",
            "gtd.assign",
            "comm.send",
            "comm.reply",
        ] {
            assert_eq!(
                atomic_admissibility(verb),
                Some(AtomicRejectionReason::EmbeddingBearing),
                "{verb:?} must be rejected as embedding-bearing (ADR-099 acceptance criteria)"
            );
        }
    }

    #[test]
    fn atomic_read_verbs_rejected() {
        for verb in [
            "search",
            "recall",
            "query",
            "traverse",
            "list",
            "get",
            "neighbors",
            "context",
        ] {
            assert_eq!(
                atomic_admissibility(verb),
                Some(AtomicRejectionReason::Read),
                "{verb:?} must be rejected as a read verb"
            );
        }
    }

    #[test]
    fn atomic_unknown_verb_defaults_to_unlisted_rejection() {
        assert_eq!(
            atomic_admissibility("some_future_verb_nobody_classified_yet"),
            Some(AtomicRejectionReason::Unlisted),
            "an unrecognized verb must default-deny, never silently admit"
        );
    }
}
