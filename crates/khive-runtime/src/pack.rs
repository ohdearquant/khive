// FILE SIZE JUSTIFICATION: pack.rs is the load-bearing dispatch core — VerbRegistry,
// VerbRegistryBuilder, PackRuntime, DispatchHook, and their test scaffolding all
// share internal state (packs Vec, gate, event_store) that cannot be cleanly split
// without exposing private fields or duplicating the scaffolding. Inline tests cover
// collision detection and dispatch path that require direct access to VerbRegistry
// internals. Split plan: when the verb surface reaches a stable v1 API, extract
// VerbRegistryBuilder into `pack/builder.rs` and gate/event logic into `pack/dispatch.rs`.
//! Pack runtime trait and verb registry.
//!
//! `PackRuntime` mirrors `Pack`'s const associated items as methods for object safety.
//! Build a [`VerbRegistry`] via `VerbRegistryBuilder::build()`; registration is builder-only.

#[cfg(test)]
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

#[cfg(test)]
use crate::operations::LinkSpec;
#[cfg(test)]
use crate::runtime::NamespaceToken;
#[cfg(test)]
use async_trait::async_trait;
#[cfg(test)]
use khive_gate::{AllowAllGate, GateRef};
use khive_gate::{AuditEvent, GateDecision, GateRequest};
#[cfg(test)]
use khive_storage::EventView;
use khive_storage::{Event, EventStore, SubstrateKind};
use khive_types::{EventKind, EventOutcome, Namespace};
use serde_json::Value;

pub use khive_types::{
    json_type_name, EdgeEndpointRule, EndpointKind, EntityTypeDef, HandlerDef, IdResolutionMode,
    NoteEmbeddingPolicy, NoteEmbeddingPolicySpec, NoteKindSpec, NoteLifecycleSpec,
    PackColumnAddition, PackColumnAffinity, PackSchemaPlan, ParamDef, VerbCategory,
    VerbPresentationPolicy, Visibility, RESERVED_ENVELOPE_ARGS,
};
// Backward-compat re-export.
#[allow(deprecated)]
pub use khive_types::VerbDef;

/// Name of the pack providing the shared CRUD verbs and the general-purpose
/// note kinds those verbs exist to serve.
///
/// Its note kinds are the ones any caller may author freely through `create`
/// and `update`; every other pack's note kinds are records maintained by that
/// pack's own verbs. Used by
/// [`VerbRegistry::pack_owned_note_kinds`].
pub const GENERIC_CRUD_PACK: &str = "kg";

/// Stable advisory code emitted when a successful inspection cannot persist
/// its dispatch audit because the configured audit backend is read-only.
pub const AUDIT_PERSISTENCE_SKIPPED_READ_ONLY: &str = "audit_persistence_skipped_read_only";

const FULL_UUID_IDENTIFIER_HELP: &str = "A complete UUID spelling accepted by the consuming \
    parameter directly names one globally unique record; direct UUID lookup is not a namespace \
    search. Strict identifier responses use canonical lowercase dashed UUIDs.";
const SHORT_PREFIX_IDENTIFIER_HELP: &str = "A short UUID prefix is at least 8 hexadecimal \
    characters without dashes that do not parse as a complete UUID. It is a resolution, not a \
    direct identifier; a 32-character compact UUID is complete input instead. Its lookup scope \
    belongs to the consuming parameter — see `identifier_resolution.resolution_modes` for the \
    exhaustive per-mode rule, and each `uuid`/`array of uuid` parameter's own description for \
    which mode it uses. A prefix can be missing or ambiguous.";
const IDENTIFIER_PARAMETER_HELP: &str = "A parameter that requires a full UUID rejects prefixes \
    and explains the resolution consequence. Its corresponding response field remains a \
    canonical full UUID so the value can be submitted again.";

/// Single-source, per-[`IdResolutionMode`] contract text.
///
/// Every `uuid`/`array of uuid` [`ParamDef`] declares which of these modes its
/// handler actually implements (see [`IdResolutionMode`]'s own doc comment).
/// [`VerbRegistry::describe_verb`] renders the SAME text in two places: once
/// per matching parameter's description, and once in the top-level
/// `identifier_resolution.resolution_modes` map — so the wording can never
/// drift between the two call sites, and a caller reading only the top-level
/// envelope still sees every mode that exists on the wire, not just the ones
/// this particular verb happens to use.
///
/// `None` for [`IdResolutionMode::NotApplicable`]: nothing is appended to a
/// non-identifier parameter's description, and it is never listed in
/// `resolution_modes`.
fn resolution_mode_contract(mode: IdResolutionMode) -> Option<&'static str> {
    match mode {
        IdResolutionMode::NotApplicable => None,
        IdResolutionMode::UnscopedById => Some(
            "ID contract (unscoped by-ID, ADR-007 Rev 6): a full UUID and a short hex prefix \
             (8+ hex chars) both resolve with no namespace filter — the caller already knows \
             the specific record, and authorization is the Gate's seam, not resolution's. A \
             prefix matching nothing or matching more than one record is rejected. Used by \
             get/update/delete/merge/link (link's source_id/target_id resolve through the same \
             unfiltered path as the four record-level by-ID verbs), GTD's lifecycle id \
             parameters, and brain's feedback target_id.",
        ),
        IdResolutionMode::PrefixScopedToPrimary => Some(
            "ID contract (prefix scoped to primary namespace): a full UUID resolves as given, \
             with no namespace check performed by this resolver. A short hex prefix (8+ hex \
             chars) is resolved by searching only the caller's primary namespace, and is \
             rejected if it matches nothing or matches more than one record there.",
        ),
        IdResolutionMode::FullAndPrefixScopedToPrimary => Some(
            "ID contract (full UUID and prefix both scoped to primary namespace): both a full \
             UUID and a short hex prefix (8+ hex chars) are validated against the caller's \
             primary namespace — a record that exists but belongs to a different namespace \
             resolves as not found. A prefix matching more than one record in that namespace \
             is rejected as ambiguous.",
        ),
        IdResolutionMode::FullUuidOnlyScopedToPrimary => Some(
            "ID contract (full UUID only, scoped to primary namespace): only a complete UUID \
             is accepted — a short hex prefix is rejected outright because this field stores \
             an explicit stable reference — and the UUID is validated against the caller's own \
             (primary) namespace; a record that exists in a different namespace resolves as \
             not found.",
        ),
        IdResolutionMode::UnscopedFullUuidOnly => Some(
            "ID contract (full UUID only, unscoped): only a complete UUID is accepted — a \
             short hex prefix is rejected outright — and no namespace check is performed on \
             this parameter itself; any namespace scoping comes from the enclosing operation, \
             not from this identifier.",
        ),
        IdResolutionMode::EdgeOrEventTarget => Some(
            "ID contract (list target by kind): kind=event accepts only a full subject UUID; \
             prefixes and names are rejected without graph resolution. Event rows remain \
             scoped to the authorized event namespace. For kind=edge, a full UUID resolves as \
             given; a unique 8+ hex prefix or entity name resolves in the primary namespace.",
        ),
    }
}

/// Stable wire key for an [`IdResolutionMode`], used as the key under
/// `identifier_resolution.resolution_modes`.
fn resolution_mode_key(mode: IdResolutionMode) -> &'static str {
    match mode {
        IdResolutionMode::NotApplicable => "not_applicable",
        IdResolutionMode::UnscopedById => "unscoped_by_id",
        IdResolutionMode::PrefixScopedToPrimary => "prefix_scoped_to_primary",
        IdResolutionMode::FullAndPrefixScopedToPrimary => "full_and_prefix_scoped_to_primary",
        IdResolutionMode::FullUuidOnlyScopedToPrimary => "full_uuid_only_scoped_to_primary",
        IdResolutionMode::UnscopedFullUuidOnly => "unscoped_full_uuid_only",
        IdResolutionMode::EdgeOrEventTarget => "edge_or_event_target",
    }
}

/// Shared identifier-resolution contract included in every operation help schema.
pub fn identifier_resolution_help() -> Value {
    let modes: serde_json::Map<String, Value> = [
        IdResolutionMode::UnscopedById,
        IdResolutionMode::PrefixScopedToPrimary,
        IdResolutionMode::FullAndPrefixScopedToPrimary,
        IdResolutionMode::FullUuidOnlyScopedToPrimary,
        IdResolutionMode::UnscopedFullUuidOnly,
        IdResolutionMode::EdgeOrEventTarget,
    ]
    .into_iter()
    .map(|mode| {
        (
            resolution_mode_key(mode).to_string(),
            Value::String(
                resolution_mode_contract(mode)
                    .expect("every non-NotApplicable mode has contract text")
                    .to_string(),
            ),
        )
    })
    .collect();

    serde_json::json!({
        "full_uuid": FULL_UUID_IDENTIFIER_HELP,
        "short_prefix": SHORT_PREFIX_IDENTIFIER_HELP,
        "parameter_rule": IDENTIFIER_PARAMETER_HELP,
        "resolution_modes": modes,
    })
}

mod traits;
pub use traits::{
    DispatchHook, KindHook, NoteUpdateEffect, PackByIdResolver, PackRuntime, SchemaPlan,
};

#[cfg(test)]
use crate::error::DispatchError;
use crate::error::{AuditObligationFailure, RuntimeError};
use crate::KhiveRuntime;

mod builder;
pub use builder::{PackMetadataRegistry, VerbRegistryBuilder};

mod catalog;
mod dispatch;
mod registry_access;
mod request_identity;
pub(crate) use request_identity::is_special_relation;
pub use request_identity::{
    InterceptedDispatchResult, PackSchemaCollisionError, RequestIdentity, VerbRegistry,
    VerifiedActor,
};

/// Relations `validate_edge_relation_endpoints`
/// (`crates/khive-runtime/src/operations.rs`) resolves in its own dedicated
/// branch — before the generic pack-rule branch (`pack_rule_allows`) is ever
/// reached. For these three relations the validator additionally accepts
/// any `note -> note` pair unconditionally, regardless of note kind
/// (ADR-002 §"Versioning" and §"Epistemic"), and never consults pack
/// `EDGE_RULES` at all, on either substrate.
pub(crate) const SPECIAL_RELATIONS: &[khive_types::EdgeRelation] = &[
    khive_types::EdgeRelation::Supersedes,
    khive_types::EdgeRelation::Supports,
    khive_types::EdgeRelation::Refutes,
];

// ── Inventory-based dynamic pack loading ────────────────────────────────────

/// Output of [`PackFactory::create_install`] — bundles the pack runtime with
/// its optional by-ID resolver and dispatch hook so a factory can hand back
/// all three built from one shared instance (see `BrainPackFactory` for why
/// this matters: the dispatch hook must observe the same state the runtime
/// mutates, not a second unrelated instance).
pub struct PackInstall {
    /// The pack runtime, registered into the builder's pack list.
    pub runtime: Box<dyn PackRuntime>,
    /// Optional by-ID resolver, registered when present.
    pub resolver: Option<Box<dyn PackByIdResolver>>,
    /// Optional post-dispatch observer, wired via `VerbRegistryBuilder::with_dispatch_hook`.
    pub dispatch_hook: Option<Arc<dyn DispatchHook>>,
}

/// Factory for creating pack instances registered via `inventory` at link time.
/// Each pack crate submits a `&'static dyn PackFactory` wrapped in a
/// [`PackRegistration`]; the binary's linker collects them all into a single
/// slice iterable at runtime.
///
/// Implementors must be `Send + Sync + 'static` because the registry is built
/// once and shared across async tasks.
/// Possession-bounded capability for the trusted channel-ingest note path.
///
/// Constructible only inside `khive-runtime` (the field is private), and
/// granted during pack registration exclusively to factories named in
/// `CHANNEL_INGEST_CAPABLE_PACKS`. Every call to
/// [`crate::KhiveRuntime::try_create_note_as_trusted_ingest`] must present a
/// reference to one, so the set of callers able to establish transport-owned
/// message properties is bounded by possession at the composition root, not
/// by a documentation prohibition. Two ways to obtain one: registering
/// through [`PackRegistry::register_packs`]/`register_packs_with_runtimes`
/// under the `comm` name (the allowlisted, automatic path), or a composition
/// root that builds packs directly calling
/// [`ChannelIngestCapability::grant_for_direct_composition`] and passing the
/// result to [`crate::PackRuntime::accept_channel_ingest_capability`] (or a
/// pack's constructor variant that does so) itself. The residual trust
/// assumption is unchanged either way: whoever assembles the
/// `VerbRegistryBuilder` already decides which packs are wired in and already
/// holds a `KhiveRuntime`, so minting the grant explicitly carries no more
/// privilege than that composition already had by choosing to register `comm`
/// at all.
pub struct ChannelIngestCapability {
    pub(crate) _sealed: (),
}

impl ChannelIngestCapability {
    /// Mint a capability for a composition root that constructs
    /// channel-transport packs directly, bypassing
    /// [`PackRegistry::register_packs`] (which grants this automatically).
    ///
    /// See the type-level doc for the trust argument: this carries no more
    /// privilege than the caller already has by virtue of holding a
    /// `KhiveRuntime` and choosing to wire the pack in.
    pub fn grant_for_direct_composition() -> Self {
        Self { _sealed: () }
    }
}

/// Pack names entitled to a [`ChannelIngestCapability`] grant at registration.
pub(crate) const CHANNEL_INGEST_CAPABLE_PACKS: &[&str] = &["comm"];

pub trait PackFactory: Send + Sync + 'static {
    /// Canonical lowercase name for this pack (e.g. `"kg"`, `"gtd"`).
    fn name(&self) -> &'static str;

    /// Names of packs that must be loaded before this one.
    ///
    /// Defaults to empty so pack crates that have no dependencies compile
    /// without changes. [`PackRegistry::register_packs`] validates that every
    /// name listed here is present in the caller's explicit pack list — absent
    /// dependencies are a boot error, not silently auto-added.
    fn requires(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether this pack intentionally exposes no top-level MCP verbs.
    ///
    /// Defaults to `false` so a declared pack whose runtime contributes no
    /// [`Visibility::Verb`] handlers fails at registration instead of silently
    /// disappearing from the served surface. Vocabulary- or ontology-only
    /// packs must opt in explicitly.
    fn intentionally_verbless(&self) -> bool {
        false
    }

    /// Create a new pack instance for the given runtime.
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime>;

    /// Build the full installation bundle for this pack: runtime, optional
    /// resolver, optional dispatch hook.
    ///
    /// Defaults to composing `create` and `create_resolver` with no dispatch
    /// hook, so existing factories compile unchanged. Packs whose dispatch
    /// hook must observe the same instance as the runtime (e.g. `brain`)
    /// override this method instead of `create`, since the default would
    /// otherwise require two independent instances to share state.
    fn create_install(&self, runtime: KhiveRuntime) -> PackInstall {
        let resolver = self.create_resolver(runtime.clone());
        PackInstall {
            runtime: self.create(runtime),
            resolver,
            dispatch_hook: None,
        }
    }

    /// Optionally create a `PackByIdResolver` for this pack.
    ///
    /// Packs that own private SQL tables implement this to hook into
    /// `get(id)` and `delete(id)`. Defaults to `None` so existing packs
    /// compile without changes.
    fn create_resolver(&self, _runtime: KhiveRuntime) -> Option<Box<dyn PackByIdResolver>> {
        None
    }
}

/// Newtype wrapper collected by `inventory` so pack crates can submit
/// `&'static dyn PackFactory` references without the type-ascription syntax
/// that `inventory::submit!` does not support for bare trait-object references.
pub struct PackRegistration(pub &'static dyn PackFactory);

inventory::collect!(PackRegistration);

/// Error returned by [`PackRegistry::register_packs`] when boot validation fails.
#[derive(Debug)]
pub enum PackLoadError {
    /// The requested pack name was not found in the inventory.
    UnknownPack(String),
    /// The requested pack name occurs more than once.
    DuplicatePack(String),
    /// A pack was requested but a declared dependency is absent from the list.
    MissingDependency {
        /// The pack that declared the dependency.
        pack: String,
        /// The dependency that is missing from the requested pack list.
        dep: String,
    },
    /// A declared pack contributed no top-level verbs without explicitly
    /// declaring itself vocabulary/ontology-only.
    NoPublicVerbs {
        /// The declared pack name.
        pack: String,
    },
}

impl std::fmt::Display for PackLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackLoadError::UnknownPack(name) => write!(f, "unknown pack {name:?}"),
            PackLoadError::DuplicatePack(name) => write!(f, "duplicate pack {name:?}"),
            PackLoadError::MissingDependency { pack, dep } => write!(
                f,
                "pack {pack:?} requires {dep:?}, which is not in the requested pack list; \
                 add --pack {dep} before --pack {pack}"
            ),
            PackLoadError::NoPublicVerbs { pack } => write!(
                f,
                "declared pack {pack:?} registers no public verbs; if this pack is \
                 intentionally vocabulary- or ontology-only, its factory must declare \
                 intentionally_verbless() = true"
            ),
        }
    }
}

impl std::error::Error for PackLoadError {}

/// Reject a declared pack whose runtime contributes no [`Visibility::Verb`]
/// handlers unless its factory explicitly opts out via
/// [`PackFactory::intentionally_verbless`].
fn check_pack_has_public_verbs(
    factory: &dyn PackFactory,
    install: &PackInstall,
    name: &str,
) -> Result<(), PackLoadError> {
    if !factory.intentionally_verbless()
        && !install
            .runtime
            .handlers()
            .iter()
            .any(|handler| matches!(handler.visibility, Visibility::Verb))
    {
        return Err(PackLoadError::NoPublicVerbs {
            pack: name.to_string(),
        });
    }
    Ok(())
}

/// Registry of pack factories discovered via `inventory` at link time.
///
/// No instance is needed — all methods are associated functions that walk the
/// globally-collected [`PackRegistration`] slice.
pub struct PackRegistry;

/// Whether [`PackRegistry::build_ingest_registry`] attaches the runtime's
/// event store to the registry it builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestAuditStore {
    /// Mirror `KhiveMcpServer::with_packs` (`khive-mcp/src/server.rs`): a
    /// writable runtime attaches its own event store and refuses to build if
    /// sink initialization fails; a read-only runtime retains no `EventStore`
    /// handle and an advisory travels beside each result instead.
    Attach,
    /// Build the registry with no audit event store, for a caller with no use
    /// for persisted audit rows.
    Detach,
}

impl PackRegistry {
    /// Names of all pack factories discovered via `inventory`.
    pub fn discovered_names() -> Vec<&'static str> {
        inventory::iter::<PackRegistration>
            .into_iter()
            .map(|r| r.0.name())
            .collect()
    }

    /// Validate linked pack names and explicit dependencies without creating
    /// runtimes, opening stores, or constructing pack instances.
    ///
    /// Launchers can use this before publishing ownership. Registration uses
    /// the same validation, including when extra factories are supplied.
    pub fn validate_pack_selection(names: &[String]) -> Result<(), PackLoadError> {
        let all: Vec<&'static dyn PackFactory> = inventory::iter::<PackRegistration>
            .into_iter()
            .map(|r| r.0)
            .collect();
        Self::validate_pack_selection_from(&all, names)
    }

    fn validate_pack_selection_from(
        factories: &[&'static dyn PackFactory],
        names: &[String],
    ) -> Result<(), PackLoadError> {
        let factory_for = |name: &str| factories.iter().copied().find(|f| f.name() == name);
        let mut requested = std::collections::HashSet::new();
        for name in names {
            factory_for(name).ok_or_else(|| PackLoadError::UnknownPack(name.clone()))?;
            if !requested.insert(name.as_str()) {
                return Err(PackLoadError::DuplicatePack(name.clone()));
            }
        }
        for name in names {
            let factory = factory_for(name).unwrap(); // All names were validated above.
            for &dep in factory.requires() {
                if !requested.contains(dep) {
                    return Err(PackLoadError::MissingDependency {
                        pack: name.clone(),
                        dep: dep.to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Register the named packs into `builder` using the supplied `runtime`.
    ///
    /// Validates the explicit pack list against `PackFactory::requires()` —
    /// if any requested pack declares a dependency that is absent from `names`,
    /// registration fails (missing dependency is a boot error, not silently
    /// auto-added). Callers must include all required packs explicitly.
    ///
    /// The [`VerbRegistryBuilder::build`] topo-sort enforces correct load order.
    ///
    /// Returns `Ok(())` when all names are recognised and all declared
    /// dependencies are satisfied; returns `Err(PackLoadError)` with a
    /// distinct variants for unknown or duplicate packs and missing dependencies.
    pub fn register_packs(
        names: &[String],
        runtime: KhiveRuntime,
        builder: &mut VerbRegistryBuilder,
    ) -> Result<(), PackLoadError> {
        // Build a name→factory index once.
        let all: Vec<&'static dyn PackFactory> = inventory::iter::<PackRegistration>
            .into_iter()
            .map(|r| r.0)
            .collect();
        let factory_for = |name: &str| -> Option<&'static dyn PackFactory> {
            all.iter().copied().find(|f| f.name() == name)
        };

        Self::validate_pack_selection_from(&all, names)?;

        // Register every requested pack; VerbRegistryBuilder::build()
        // performs the topo-sort, so insertion order here does not matter.
        for name in names {
            let factory = factory_for(name.as_str()).unwrap(); // validated above
            let install = factory.create_install(runtime.clone());
            check_pack_has_public_verbs(factory, &install, name)?;
            if CHANNEL_INGEST_CAPABLE_PACKS.contains(&name.as_str()) {
                install
                    .runtime
                    .accept_channel_ingest_capability(ChannelIngestCapability { _sealed: () });
            }
            builder.register_boxed(install.runtime);
            if let Some(resolver) = install.resolver {
                builder.register_resolver(name.clone(), resolver);
            }
            if let Some(hook) = install.dispatch_hook {
                builder.with_dispatch_hook(hook);
            }
        }

        Ok(())
    }

    /// Build a `VerbRegistry` from `runtime`'s own configuration: gate,
    /// default namespace, visible namespaces, actor id, and the configured
    /// pack set, then install the registry's aggregated edge rules back onto
    /// `runtime`. This is the wiring shared by every one-shot CLI ingest path
    /// (`kkernel code-ingest`, `kkernel git-ingest`) that needs a real
    /// registry to dispatch through outside of a live MCP server.
    ///
    /// `audit_store` selects whether the registry gets the runtime's event
    /// store; see [`IngestAuditStore`] for what each variant does.
    ///
    /// This helper carries only the subset every ingest path duplicated
    /// verbatim. The MCP server's own registry construction additionally
    /// wires channel-loop admission, `config_id`, embedder/entity-type/
    /// note-mutation-hook registration, schema-plan application, and the WAL
    /// checkpoint pool handle — all server-only concerns a one-shot CLI pass
    /// has no use for, so `KhiveMcpServer::with_packs` keeps its own
    /// construction rather than calling this helper.
    pub fn build_ingest_registry(
        runtime: &KhiveRuntime,
        audit_store: IngestAuditStore,
    ) -> Result<VerbRegistry, RuntimeError> {
        let mut builder = VerbRegistryBuilder::new();
        builder.with_gate(runtime.config().gate.clone());
        builder.with_default_namespace(runtime.config().default_namespace.as_str());
        builder.with_visible_namespaces(runtime.config().visible_namespaces.clone());
        builder.with_actor_id(runtime.config().actor_id.clone());
        if audit_store == IngestAuditStore::Attach {
            if runtime.is_read_only() {
                builder.with_read_only_audit_store();
            } else {
                // Attach requires a usable sink; build propagates open failures.
                builder.with_runtime_event_store(runtime)?;
            }
        }
        Self::register_packs(
            &runtime.config().packs.clone(),
            runtime.clone(),
            &mut builder,
        )
        .map_err(|e| RuntimeError::Internal(format!("pack registration failed: {e:?}")))?;
        let registry = builder.build()?;
        runtime.install_edge_rules(registry.all_edge_rules());
        Ok(registry)
    }

    /// Register the named packs into `builder`, routing each pack to its own runtime.
    ///
    /// `runtimes` maps pack name → `KhiveRuntime` (one per backend assignment).
    /// `default_runtime` is used for any pack whose name is not in `runtimes`.
    /// The validation logic (unknown pack, missing dependency) is identical to
    /// [`PackRegistry::register_packs`].
    ///
    /// This is the multi-backend boot path (ADR-028). Single-backend callers
    /// should continue using [`PackRegistry::register_packs`].
    pub fn register_packs_with_runtimes(
        names: &[String],
        runtimes: &HashMap<String, KhiveRuntime>,
        default_runtime: &KhiveRuntime,
        builder: &mut VerbRegistryBuilder,
    ) -> Result<(), PackLoadError> {
        let all: Vec<&'static dyn PackFactory> = inventory::iter::<PackRegistration>
            .into_iter()
            .map(|r| r.0)
            .collect();
        Self::register_packs_with_runtimes_from(&all, names, runtimes, default_runtime, builder)
    }

    /// Like [`Self::register_packs_with_runtimes`], but resolves pack names
    /// against the link-time `inventory` registry **plus** `extra_factories` —
    /// pack factories the composition root supplies directly rather than
    /// discovers through `inventory::iter::<PackRegistration>` (ADR-191 D6,
    /// ADR-192 S4: "a pack compiled outside this repository ... extends the
    /// web ontology without any change here" — a host binary that depends on
    /// a pinned khive revision plus an out-of-tree pack crate, or a
    /// composition root registering a credential-provider/request-hook
    /// consumer pack, has no `inventory` presence in *this* binary short of
    /// its own force-link anchor). An inventory-discovered factory always
    /// wins a name collision with an `extra_factories` entry — the linked set
    /// is the trusted default; an extra factory only fills a name inventory
    /// does not already answer.
    ///
    /// This is the seam D6 describes as "kkernel exposes its server
    /// construction as a library entry point that accepts additional pack
    /// factories" — the `kkernel` library entry point itself lives in
    /// `kkernel::compose`, built on this function exactly as
    /// `khive-mcp/src/serve.rs` builds on [`Self::register_packs_with_runtimes`].
    pub fn register_packs_with_runtimes_with_extra_factories(
        extra_factories: &[&'static dyn PackFactory],
        names: &[String],
        runtimes: &HashMap<String, KhiveRuntime>,
        default_runtime: &KhiveRuntime,
        builder: &mut VerbRegistryBuilder,
    ) -> Result<(), PackLoadError> {
        let mut all: Vec<&'static dyn PackFactory> = inventory::iter::<PackRegistration>
            .into_iter()
            .map(|r| r.0)
            .collect();
        all.extend(extra_factories.iter().copied());
        Self::register_packs_with_runtimes_from(&all, names, runtimes, default_runtime, builder)
    }

    /// Shared body for [`Self::register_packs_with_runtimes`] and
    /// [`Self::register_packs_with_runtimes_with_extra_factories`]: both
    /// build a `factories` index (inventory-only, or inventory-plus-extra)
    /// and delegate here. `factory_for` resolves by first match, so a
    /// duplicate name earlier in `factories` wins over a later one — the two
    /// public callers above rely on that for their stated collision rule.
    fn register_packs_with_runtimes_from(
        factories: &[&'static dyn PackFactory],
        names: &[String],
        runtimes: &HashMap<String, KhiveRuntime>,
        default_runtime: &KhiveRuntime,
        builder: &mut VerbRegistryBuilder,
    ) -> Result<(), PackLoadError> {
        let factory_for = |name: &str| -> Option<&'static dyn PackFactory> {
            factories.iter().copied().find(|f| f.name() == name)
        };

        Self::validate_pack_selection_from(factories, names)?;

        builder.kg_read_resolver = Some(Arc::new(crate::kg_read::KgReadResolver::new(
            default_runtime,
            runtimes,
        )));

        for name in names {
            let factory = factory_for(name.as_str()).unwrap();
            let runtime = runtimes
                .get(name.as_str())
                .cloned()
                .unwrap_or_else(|| default_runtime.clone());
            let install = factory.create_install(runtime);
            check_pack_has_public_verbs(factory, &install, name)?;
            if CHANNEL_INGEST_CAPABLE_PACKS.contains(&name.as_str()) {
                install
                    .runtime
                    .accept_channel_ingest_capability(ChannelIngestCapability { _sealed: () });
            }
            builder.register_boxed(install.runtime);
            if let Some(resolver) = install.resolver {
                builder.register_resolver(name.clone(), resolver);
            }
            if let Some(hook) = install.dispatch_hook {
                builder.with_dispatch_hook(hook);
            }
        }

        Ok(())
    }
}

/// Audit target in the submitted args; only `link` also accepts `target` for `target_id`.
fn target_id_from_args(verb: &str, args: &serde_json::Value) -> Option<uuid::Uuid> {
    let alias = args.get("target").filter(|_| verb == "link");
    args.get("target_id")
        .or(alias)
        .and_then(serde_json::Value::as_str)
        .and_then(|s| s.parse::<uuid::Uuid>().ok())
}

/// Build the [`AuditEvent`] for one gate check, masking `deny_reason` before
/// it can reach either downstream sink.
///
/// `deny_reason` is gate-authored text this crate does not control: a custom
/// `Gate` implementation (a Rego policy, an external backend) can echo
/// request content into why it denied, so the same secret-detection pass
/// applied to backend error text elsewhere in this file also has to run on a
/// denial's stated reason. This can't live on [`AuditEvent`] itself —
/// `khive-gate` cannot depend on `khive-runtime`'s masking, which itself
/// depends on `khive-gate` (see `khive-runtime/Cargo.toml`); a masker inside
/// `AuditEvent::from_check` would be a dependency cycle. So masking happens
/// once, here, immediately after construction and before the event is used
/// anywhere: every call site that turns a [`GateDecision`] into an
/// [`AuditEvent`] must go through this function, never `AuditEvent::from_check`
/// directly, so the `gate.check` tracing line and the row
/// [`build_audit_storage_event`] re-serializes for the event store always see
/// the same masked value rather than each needing its own redaction.
fn masked_audit_event(
    gate_req: &GateRequest,
    decision: &GateDecision,
    gate_impl: &str,
) -> AuditEvent {
    let mut audit = AuditEvent::from_check(gate_req, decision, gate_impl)
        .with_operation_attribution(
            khive_storage::operation_context::current_operation_attribution(),
        );
    if let Some(reason) = audit.deny_reason.take() {
        audit.deny_reason = Some(crate::secret_gate::bounded_masked_log_text(&reason));
    }
    audit
}

/// Build a v1-shape audit storage event from a gate check outcome.
/// See `docs/api/pack.md#build_audit_storage_event` for the `resource` payload contract.
fn build_audit_storage_event(
    gate_req: &GateRequest,
    audit: &AuditEvent,
    outcome: EventOutcome,
    resource: Option<Value>,
) -> Event {
    let mut audit_data = serde_json::to_value(audit).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "failed to serialize AuditEvent for EventStore");
        serde_json::Value::Null
    });
    if let Some(resource) = resource {
        if let Value::Object(ref mut map) = audit_data {
            map.insert("resource".to_string(), resource);
        }
    }
    let mut storage_event = Event::new(
        gate_req.namespace.as_str(),
        gate_req.verb.as_str(),
        EventKind::Audit,
        SubstrateKind::Event,
        format!("{}:{}", gate_req.actor.kind, gate_req.actor.id),
    )
    .with_outcome(outcome)
    .with_payload(audit_data);
    storage_event.op_index = audit.op_index;
    storage_event.ref_resolution = audit.ref_resolution;
    if let Some(target_id) = target_id_from_args(&gate_req.verb, &gate_req.args) {
        storage_event = storage_event.with_target(target_id);
    }
    storage_event
}

/// Process-wide pure-observability audit appends whose errors were logged
/// and swallowed — never an obligation-bearing row, which fails its dispatch
/// instead and is counted separately by
/// [`AUDIT_OBLIGATION_APPEND_FAILURES`]/[`audit_obligation_append_failure_count`].
/// Keeping this counter obligation-free preserves its documented contract
/// (`docs/guide/api-reference.md`, `khive-db`'s `WriterContentionDiagnostics::audit_append_failures`
/// doc comment): every unit counted here was swallowed, none was propagated.
static AUDIT_APPEND_FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn audit_append_failure_count() -> u64 {
    AUDIT_APPEND_FAILURES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Process-wide commit failures for obligation-bearing audit rows (ADR-133
/// D2/D3/D4): gate denials, dispatch outcomes, unknown-verb rows, and
/// `git.digest` success receipts. Most call sites fold this failure into the
/// dispatch's own error (a would-be success becomes an error, per
/// [`fold_audit_obligation`]); a denial's own audit row is the one
/// exception — its dispatch already returns `PermissionDenied` independent
/// of whether this row commits, so the failure is logged and counted here
/// but not separately propagated. Disjoint from [`AUDIT_APPEND_FAILURES`] —
/// each failing row is classified by [`crate::audit_batch::classify`] into
/// exactly one of the two classes and increments exactly one of these two
/// counters, never both.
static AUDIT_OBLIGATION_APPEND_FAILURES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Runtime diagnostics exposes this process-wide counter separately from
/// swallowed audit errors and batch-generation failures (#2784).
pub(crate) fn audit_obligation_append_failure_count() -> u64 {
    AUDIT_OBLIGATION_APPEND_FAILURES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Process-wide count of `DispatchObligation` rows **refused before they
/// could be enqueued** (`AuditTerminalReason::QueueAdmissionExhausted`) for an
/// [`VerbRegistry::admission_degrade_safe`] verb (#2147/#2217).
/// This is a confirmed, terminal accounting loss: the row never shared a
/// generation with anyone and will never commit. Disjoint from both
/// [`AUDIT_APPEND_FAILURES`] and [`AUDIT_OBLIGATION_APPEND_FAILURES`]: this
/// case is neither. It is not [`AUDIT_APPEND_FAILURES`] — that counter's own
/// contract (`khive-db`'s `WriterContentionDiagnostics::audit_append_failures`
/// doc) says an obligation-bearing row's commit failure "either fail[s] the
/// dispatch... or [is] tracked by the runtime's own separate
/// obligation-failure counter instead", and this dispatch does neither: it
/// reports the caller's already-computed success with no error. It is not
/// [`AUDIT_OBLIGATION_APPEND_FAILURES`] either — that counter's contract is
/// "most call sites fold this failure into the dispatch's own error", which
/// is exactly the propagation this admission-degrade path exists to avoid.
/// Also disjoint from [`AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS`] — that
/// counter's row was enqueued and may still commit; this one's was not.
/// Read in production by [`VerbRegistry::audit_batch_metrics`], which feeds
/// it into `khive_db::diagnostics::RuntimeAuditBatchMetrics::admission_refused_obligations`
/// and from there into the `db_diagnostics` verb's
/// `writer_contention.audit_admission_refused_obligations` field (ADR-103
/// Amendment 3) — an operator can read this counter without a test-only
/// feature gate. The mechanism tests also read it directly, including the
/// admission-pressure regression tests in `tests/read_verb_admission_exhaustion.rs`,
/// which (like `khive-runtime/src/audit_batch.rs`'s own `test_internals`
/// module) need it as `pub`, not `pub(crate)`, since they compile as a
/// separate external binary outside this crate.
///
/// This counter is CUMULATIVE for the life of the process. Nothing decrements
/// it and nothing resolves it: the only writes in the tree are this
/// declaration and one `fetch_add`. A value that does not move therefore means
/// no refusal happened in that window, which is the healthy reading, not a
/// stalled subsystem (#2791). Because a total cannot say when it was last
/// earned, it is paired with
/// [`AUDIT_ADMISSION_REFUSED_OBLIGATIONS_LAST_MS`].
static AUDIT_ADMISSION_REFUSED_OBLIGATIONS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Wall-clock milliseconds at which [`AUDIT_ADMISSION_REFUSED_OBLIGATIONS`]
/// last moved; `0` means it has never moved in this process. This is the field
/// that makes a static count readable: an old mark beside a non-zero count is
/// history, a recent mark beside the same count is an active condition (#2791).
static AUDIT_ADMISSION_REFUSED_OBLIGATIONS_LAST_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub fn audit_admission_refused_obligation_count() -> u64 {
    AUDIT_ADMISSION_REFUSED_OBLIGATIONS.load(std::sync::atomic::Ordering::Relaxed)
}

/// `None` until the counter first moves in this process.
pub fn audit_admission_refused_obligation_last_at_ms() -> Option<u64> {
    match AUDIT_ADMISSION_REFUSED_OBLIGATIONS_LAST_MS.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        at => Some(at),
    }
}

/// Process-wide count of `DispatchObligation` rows that were **already
/// enqueued but had not resolved by the time the caller's admission wait
/// deadline elapsed** (`AuditTerminalReason::AdmissionDeadlineExpired`) for a
/// succeeded dispatch of any verb (#2147/#2217 introduced the count for
/// [`VerbRegistry::admission_degrade_safe`] reads; writes joined it once a
/// committed write stopped reporting failure over a row that still commits).
/// Unlike [`AUDIT_ADMISSION_REFUSED_OBLIGATIONS`], a row counted here is not
/// a confirmed loss: per `AuditTerminalReason::AdmissionDeadlineExpired`'s own
/// doc, the row may still be committed (or terminally failed) by the
/// generation driver independently of the caller's timeout, so this counter
/// is an upper bound on the eventual undercount, not the undercount itself.
/// Read in production by [`VerbRegistry::audit_batch_metrics`], which feeds
/// it into `khive_db::diagnostics::RuntimeAuditBatchMetrics::admission_unresolved_obligations`
/// and from there into the `db_diagnostics` verb's
/// `writer_contention.audit_admission_unresolved_obligations` field (ADR-103
/// Amendment 3).
///
/// This counter is CUMULATIVE for the life of the process, and its name is the
/// one that misleads: "unresolved obligations" reads as the size of a live set
/// that something drains. There is no such set and no resolver. The only
/// writes in the tree are this declaration and one `fetch_add`, so a value that
/// does not move means no admission deadline expired in that window — the
/// healthy reading (#2791). Each increment records one past event whose row,
/// per `AuditTerminalReason::AdmissionDeadlineExpired`, most likely committed
/// afterwards. Paired with [`AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS_LAST_MS`]
/// so a reader can tell history from an active condition.
static AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Wall-clock milliseconds at which [`AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS`]
/// last moved; `0` means it has never moved in this process (#2791).
static AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS_LAST_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub fn audit_admission_unresolved_obligation_count() -> u64 {
    AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS.load(std::sync::atomic::Ordering::Relaxed)
}

/// `None` until the counter first moves in this process.
pub fn audit_admission_unresolved_obligation_last_at_ms() -> Option<u64> {
    match AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS_LAST_MS.load(std::sync::atomic::Ordering::Relaxed)
    {
        0 => None,
        at => Some(at),
    }
}

/// Stamp an admission-obligation counter's "last moved" mark.
///
/// A clock that reads before 1970, or a host clock stepped backwards, must not
/// be able to write `0` and make a counter that HAS moved report that it never
/// did, so a non-positive reading is clamped to 1ms.
fn mark_admission_obligation_counter(mark: &std::sync::atomic::AtomicU64) {
    let now = chrono::Utc::now().timestamp_millis();
    let now = u64::try_from(now).unwrap_or(1).max(1);
    mark.store(now, std::sync::atomic::Ordering::Relaxed);
}

const GIT_DIGEST_RECEIPT_FAILURE: &str =
    "git_digest_receipt_persist_failed: git.digest writes may have committed, but no durable \
     success receipt was confirmed; inspect ingest state before retrying";

/// Tells the dispatch seam whether it should consume the deferred audit or
/// reuse it for the ordinary generic Error row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitDigestReceiptOutcome {
    /// The schema-v2 receipt landed; no second audit row may be appended.
    Persisted,
    /// The handler's nominal success could not be shaped into a receipt. The
    /// helper has converted it to an error, and the original audit remains
    /// available for one generic Error row.
    BuildRejected,
    /// Persistence could not be attempted or its append failed. A second
    /// best-effort append would either be impossible or duplicate the same
    /// known store failure, so the caller must not retry it here.
    PersistenceUnavailable,
}

fn fail_git_digest_receipt(
    result: &mut Result<Value, RuntimeError>,
    failure: AuditObligationFailure,
) {
    let Ok(value) = result else {
        return;
    };
    let domain_result = std::mem::take(value);
    *result = Err(RuntimeError::AuditObligation {
        failure: Box::new(failure),
        domain_result,
    });
}

/// Persist the complete successful `git.digest` report as a schema-v2 audit
/// event and add that event's UUID to the returned report as `receipt_id`.
///
/// This is intentionally strict while every other dispatch audit remains
/// best-effort: a caller must never receive an unqualified digest success if
/// response loss would leave it unable to recover the exact per-pass report.
/// Missing audit/store configuration, an invalid handler report, or an append
/// failure therefore replaces the handler success with a stable safe error.
/// The error does not expose storage paths, source URLs, or command stderr and
/// explicitly warns that ingest writes may already have committed.
async fn persist_git_digest_receipt(
    store: Option<&Arc<dyn EventStore>>,
    audit_batch: Option<&Arc<crate::audit_batch::AuditBatch>>,
    gate_req: &GateRequest,
    audit: Option<&AuditEvent>,
    result: &mut Result<Value, RuntimeError>,
    duration_us: i64,
    resource: Option<Value>,
) -> GitDigestReceiptOutcome {
    let Ok(report) = result else {
        return GitDigestReceiptOutcome::PersistenceUnavailable;
    };
    let Some(store) = store else {
        tracing::error!(
            verb = "git.digest",
            "durable receipt store is not configured"
        );
        fail_git_digest_receipt(
            result,
            AuditObligationFailure::git_digest_receipt("event store is not configured"),
        );
        return GitDigestReceiptOutcome::PersistenceUnavailable;
    };
    let Some(audit) = audit else {
        tracing::error!(
            verb = "git.digest",
            "durable receipt cannot be built because the gate produced no audit decision"
        );
        fail_git_digest_receipt(
            result,
            AuditObligationFailure::git_digest_receipt("gate audit decision is absent"),
        );
        return GitDigestReceiptOutcome::PersistenceUnavailable;
    };

    let Some(report_object) = report.as_object_mut() else {
        tracing::error!(
            verb = "git.digest",
            "digest handler returned a non-object report"
        );
        fail_git_digest_receipt(
            result,
            AuditObligationFailure::git_digest_receipt("handler report is not an object"),
        );
        return GitDigestReceiptOutcome::BuildRejected;
    };
    let Some(project_id) = report_object
        .get("project_id")
        .and_then(Value::as_str)
        .and_then(|raw| raw.parse::<uuid::Uuid>().ok())
    else {
        tracing::error!(
            verb = "git.digest",
            "digest handler report omitted a valid project_id"
        );
        fail_git_digest_receipt(
            result,
            AuditObligationFailure::git_digest_receipt("handler report has no valid project_id"),
        );
        return GitDigestReceiptOutcome::BuildRejected;
    };

    // Allocate the event first so the exact durable key can be embedded in
    // both the caller-visible report and the report snapshot stored in it.
    let mut event = Event::new(
        gate_req.namespace.as_str(),
        gate_req.verb.as_str(),
        EventKind::Audit,
        SubstrateKind::Event,
        format!("{}:{}", gate_req.actor.kind, gate_req.actor.id),
    )
    .with_outcome(EventOutcome::Success)
    .with_target(project_id)
    .with_payload_schema_version(2)
    .with_duration_us(duration_us);
    let receipt_id = event.id;
    report_object.insert(
        "receipt_id".to_string(),
        Value::String(receipt_id.to_string()),
    );

    let mut payload = serde_json::to_value(audit).unwrap_or_else(|serialize_err| {
        tracing::error!(
            verb = "git.digest",
            error = %serialize_err,
            "failed to serialize gate audit for durable digest receipt"
        );
        Value::Null
    });
    let Value::Object(payload_object) = &mut payload else {
        tracing::error!(
            verb = "git.digest",
            "gate audit serialization did not produce an object"
        );
        fail_git_digest_receipt(
            result,
            AuditObligationFailure::git_digest_receipt("gate audit payload is not an object"),
        );
        return GitDigestReceiptOutcome::BuildRejected;
    };
    if let Some(resource) = resource {
        payload_object.insert("resource".to_string(), resource);
    }
    payload_object.insert("result".to_string(), report.clone());
    event.payload = payload;

    // Strict path (ADR-133): a git.digest success receipt must still commit
    // exactly once before the caller can see success, so this row waits on
    // its generation's commit through the batch seam rather than
    // best-effort — the batching only changes whether it shares a writer
    // acquisition with concurrent rows, never whether it is durable before
    // the caller observes success.
    let submit_result = if let Some(audit_batch) = audit_batch {
        audit_batch
            .submit_until_resolved(crate::audit_batch::PreparedAuditRow {
                event,
                producer: crate::audit_batch::AuditProducer::GitDigestReceipt,
            })
            .await
            .map(|_outcome| ())
            .map_err(|reason| AuditObligationFailure::new("git.digest", reason))
    } else {
        store
            .append_event(event)
            .await
            .map_err(|error| AuditObligationFailure::from_store("git.digest", error))
    };
    if let Err(mut failure) = submit_result {
        // `GitDigestReceipt` is always `DispatchObligation` (see
        // `crate::audit_batch::classify`) and this failure always
        // propagates below, so it belongs on the obligation counter, not
        // the swallowed-failures one.
        AUDIT_OBLIGATION_APPEND_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::error!(
            verb = "git.digest",
            error = %failure,
            receipt_id = %receipt_id,
            "durable digest receipt append failed"
        );
        failure.message = format!(
            "{GIT_DIGEST_RECEIPT_FAILURE}; audit submission failed ({})",
            failure.wire_code()
        );
        fail_git_digest_receipt(result, failure);
        return GitDigestReceiptOutcome::PersistenceUnavailable;
    }
    GitDigestReceiptOutcome::Persisted
}

/// Append an audit event, propagating a persistent failure for
/// obligation-bearing producers and swallowing it for pure-observability
/// producers.
///
/// ADR-133 D2/D3/D4: a dispatch must not report success when the row that
/// accounts for, authorizes, or audits it did not commit. Producers
/// classified [`crate::audit_batch::AuditProductionClass::DispatchObligation`]
/// (gate denials, dispatch outcomes, unknown-verb, git.digest receipts)
/// therefore return `Err` here on a persistent commit failure; the caller is
/// responsible for folding that into the dispatch result on the
/// success path — see [`fold_audit_obligation`]. Producers classified
/// [`crate::audit_batch::AuditProductionClass::PureObservability`]
/// (config-lock rows, `memory.recall` execution) degrade gracefully: the
/// failure is logged and counted but never returned, matching the pre-ADR-133
/// best-effort contract.
///
/// Every failure — obligation or observability — increments one of the
/// process-wide diagnostics counters above; the one exception is the
/// admission-degrade case below, which increments one of its own dedicated
/// [`AUDIT_ADMISSION_REFUSED_OBLIGATIONS`] /
/// [`AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS`] counters instead — it is
/// neither a swallowed observability failure nor a propagated obligation
/// failure.
///
/// `degrade_allowlisted` (#2147/#2217) narrows that obligation for
/// one specific case: a *successful* dispatch (`AuditProducer::DispatchSucceeded`)
/// for a verb that [`VerbRegistry::admission_degrade_safe`] has explicitly
/// opted in (Assertive alone is not a sufficient signal — see that method's
/// doc) performs no domain write, so this row's own admission being
/// transiently refused or timed out (`AuditTerminalReason::QueueAdmissionExhausted`
/// / `AdmissionDeadlineExpired`) degrades to best-effort instead of failing
/// the dispatch — the caller-visible read result is preserved. This function
/// derives eligibility from `producer` itself rather than trusting the
/// caller's `degrade_allowlisted` answer in isolation, so a `DispatchFailed`
/// row can never take the degrade path no matter what a caller passes: every
/// failed dispatch and every gate-denial/unknown-verb/git.digest row stays
/// strictly obligation-bearing. A succeeded write degrades on exactly one
/// reason, `AdmissionDeadlineExpired`: its row is already enqueued and its
/// generation commits it independently of the caller's wait, so failing the
/// dispatch would report a committed domain write as failed while changing
/// nothing about the row. `QueueAdmissionExhausted` (refused before enqueue,
/// a confirmed loss) still fails a write's dispatch.
///
/// When the registry has an audit-batch seam configured (it is whenever
/// `store` is), the row routes through
/// [`crate::audit_batch::AuditBatchControl::submit`] instead of taking its
/// own writer-task acquisition — concurrent producers collapse onto one
/// commit per generation. `audit_batch: None` (a `VerbRegistry` predating
/// the seam, or constructed without going through the builder) falls back to
/// the pre-ADR-133 direct append, classified the same way.
async fn append_audit_event_best_effort(
    audit_batch: Option<&Arc<crate::audit_batch::AuditBatch>>,
    store: &Arc<dyn EventStore>,
    event: Event,
    verb: &str,
    producer: crate::audit_batch::AuditProducer,
    degrade_allowlisted: bool,
) -> Result<(), AuditObligationFailure> {
    use crate::audit_batch::{
        classify, AuditBatchControl, AuditProducer, AuditProductionClass, AuditTerminalReason,
    };

    let is_obligation = classify(producer) == AuditProductionClass::DispatchObligation;
    let admission_degrade_eligible =
        degrade_allowlisted && producer == AuditProducer::DispatchSucceeded;
    // A row that was enqueued before the caller's admission wait elapsed is
    // committed by its generation independently of this response, so the
    // only thing failing the dispatch would do is report a committed domain
    // write as failed. That holds for every succeeded dispatch, allowlisted
    // read or not; the refused-before-enqueue arm below stays strict for
    // writes because that one is a confirmed audit loss.
    let enqueued_row_outlives_deadline = producer == AuditProducer::DispatchSucceeded;

    if let Some(audit_batch) = audit_batch {
        let row = crate::audit_batch::PreparedAuditRow { event, producer };
        // khive#2256: for a successful non-degrade-safe operation, the
        // domain effect may already be committed. Once its audit row is
        // enqueued, keep awaiting the generation's real result past the
        // ordinary admission deadline instead of reporting a false failure
        // that invites an unsafe retry. Admission-degrade-safe reads retain
        // their bounded-wait behavior, as do error/denial observations whose
        // caller-visible outcome is already fixed.
        let submit_result =
            if producer == AuditProducer::DispatchSucceeded && !admission_degrade_eligible {
                audit_batch.submit_until_resolved(row).await
            } else {
                audit_batch.submit(row).await
            };
        if let Err(reason) = submit_result {
            if is_obligation {
                // #2147/#2217: a read verb performs no domain write, so
                // when the audit-lane's OWN admission is merely under transient
                // pressure (the row was refused before enqueue, or the caller's
                // wait deadline elapsed on a row that is still likely to commit),
                // failing the read discards a valid result to protect an
                // obligation the read never needed as strictly as a write does.
                // Any other reason (a definite store/durability failure) still
                // fails the dispatch for reads exactly as it does for writes.
                //
                // The two admission-pressure reasons are not the same fact and
                // are counted on separate counters: `QueueAdmissionExhausted`
                // never enqueued, so it is a confirmed terminal loss, while
                // `AdmissionDeadlineExpired` was already enqueued and may still
                // commit later — see `AuditTerminalReason::AdmissionDeadlineExpired`'s
                // own doc.
                if enqueued_row_outlives_deadline
                    && reason == AuditTerminalReason::AdmissionDeadlineExpired
                {
                    AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    mark_admission_obligation_counter(
                        &AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS_LAST_MS,
                    );
                    tracing::warn!(
                        verb,
                        reason = ?reason,
                        degrade_allowlisted,
                        "audit obligation row was still enqueued and unresolved when \
                         the caller's admission wait deadline elapsed; its generation \
                         commits it independently of this response. Dispatch reports \
                         its own committed result (non-fatal)"
                    );
                    return Ok(());
                }
                if admission_degrade_eligible
                    && reason == AuditTerminalReason::QueueAdmissionExhausted
                {
                    AUDIT_ADMISSION_REFUSED_OBLIGATIONS
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    mark_admission_obligation_counter(&AUDIT_ADMISSION_REFUSED_OBLIGATIONS_LAST_MS);
                    tracing::warn!(
                        verb,
                        reason = ?reason,
                        "read verb's audit obligation row was refused before \
                         enqueue under audit-lane admission pressure; dispatch \
                         still reports its own result (non-fatal)"
                    );
                    return Ok(());
                }
                AUDIT_OBLIGATION_APPEND_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::error!(
                    verb,
                    reason = ?reason,
                    "audit obligation batch submission failed; failing dispatch"
                );
                return Err(AuditObligationFailure::new(verb, reason));
            }
            AUDIT_APPEND_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                verb,
                reason = ?reason,
                "audit event batch submission failed (non-fatal)"
            );
        }
        return Ok(());
    }

    if let Err(store_err) = store.append_event(event).await {
        if is_obligation {
            AUDIT_OBLIGATION_APPEND_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::error!(
                verb,
                error = %store_err,
                "audit obligation store write failed; failing dispatch"
            );
            return Err(AuditObligationFailure::from_store(verb, store_err));
        }
        AUDIT_APPEND_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            verb,
            error = %store_err,
            "audit event store write failed (non-fatal)"
        );
    }
    Ok(())
}

/// Fold an audit-obligation outcome into a dispatch result.
///
/// A dispatch that would otherwise report success cannot claim it once the
/// row accounting for it fails to commit (ADR-133 D2/D3/D4), so `Ok` becomes
/// the audit's `Err`. A dispatch that already reports failure keeps its
/// original error — the obligation is on never reporting a false success,
/// not on replacing one error with another.
fn fold_audit_obligation<T>(
    result: Result<T, RuntimeError>,
    audit_outcome: Result<(), AuditObligationFailure>,
    domain_value: impl FnOnce(T) -> Value,
) -> Result<T, RuntimeError> {
    match (result, audit_outcome) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(value), Err(failure)) => Err(RuntimeError::AuditObligation {
            failure: Box::new(failure),
            domain_result: domain_value(value),
        }),
        (Err(err), _) => Err(err),
    }
}

/// Schema v2 audit payload for a successful singleton `link` call — additive
/// over v1 via `#[serde(flatten)]`. See `docs/api/pack.md#linkauditsuccessv2`.
#[derive(Debug, Clone, serde::Serialize)]
struct LinkAuditSuccessV2 {
    #[serde(flatten)]
    audit: AuditEvent,
    edge_id: uuid::Uuid,
    source_id: uuid::Uuid,
    target_id: uuid::Uuid,
    relation: String,
    weight: f64,
}

/// Extract edge fields to enrich a successful singleton `link` audit row.
/// Returns `None` on any missing/malformed field (falls back to v1 shape).
/// See `docs/api/pack.md#link_audit_success_from_result`.
fn link_audit_success_from_result(
    audit: AuditEvent,
    result: &serde_json::Value,
) -> Option<(uuid::Uuid, serde_json::Value)> {
    let edge_id = result.get("id")?.as_str()?.parse::<uuid::Uuid>().ok()?;
    let source_id = result
        .get("source_id")?
        .as_str()?
        .parse::<uuid::Uuid>()
        .ok()?;
    let target_id = result
        .get("target_id")?
        .as_str()?
        .parse::<uuid::Uuid>()
        .ok()?;
    let relation = result.get("relation")?.as_str()?.to_string();
    let weight = result.get("weight")?.as_f64()?;
    let enriched = LinkAuditSuccessV2 {
        audit,
        edge_id,
        source_id,
        target_id,
        relation,
        weight,
    };
    let payload = serde_json::to_value(&enriched).ok()?;
    Some((edge_id, payload))
}

/// Resolve and validate a caller-supplied `namespace` argument the same way
/// on every MCP ingress path.
///
/// - Absent `namespace` key → parse `default_namespace`.
/// - Present `namespace: "<string>"` → parse the caller's value.
/// - Present non-string `namespace` (null, number, bool, array, object) →
///   fail closed with `RuntimeError::InvalidInput`. ADR-018 requires this:
///   a malformed explicit value must never be silently coerced to the
///   default namespace.
///
/// Single chokepoint for both `VerbRegistry::dispatch` and the multi-backend
/// coordinator intercept — see `docs/api/pack.md#resolve_explicit_namespace`.
pub fn resolve_explicit_namespace(
    params: &Value,
    default_namespace: &str,
) -> Result<Namespace, RuntimeError> {
    match params.get("namespace") {
        None => Namespace::parse(default_namespace)
            .map_err(|e| RuntimeError::InvalidInput(format!("invalid namespace: {e}"))),
        Some(Value::String(ns_str)) => Namespace::parse(ns_str)
            .map_err(|e| RuntimeError::InvalidInput(format!("invalid namespace {ns_str:?}: {e}"))),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "invalid namespace: expected string when present, got {}",
            json_type_name(other),
        ))),
    }
}

// INLINE TEST JUSTIFICATION: tests here exercise VerbRegistry collision detection,
// gate enforcement, and dispatch ordering that depend on direct access to the
// registry's private `packs` Vec and gate field. Moving them to tests/ would
// require pub-exporting registry internals. Broad behavioral dispatch tests
// live in tests/integration.rs.
#[cfg(test)]
#[path = "pack_tests.rs"]
pub(crate) mod tests;

// ---- Inter-pack dependency checking ----

#[cfg(test)]
#[path = "pack/dep_tests.rs"]
mod dep_tests;

// ── Note-update hook sequencing tests ───────────────────────────
//
// These tests exercise the DISPATCHER (`VerbRegistry::prepare_note_update_hook`),
// not any one pack's hook. The probe below overrides `normalize_note_update`
// and `validate_note_update`, which since #2956 are the only two halves a pack
// can implement — there is no sequencing method on the trait — so the only way
// both can run, in order, is through the registry's own sequencing.

#[cfg(test)]
#[path = "pack/note_update_sequencing_tests.rs"]
mod note_update_sequencing_tests;

// ── Dispatch hook tests ─────────────────────────────────────────

#[cfg(test)]
#[path = "pack/hook_tests.rs"]
mod hook_tests;

// ── help=true tests ──────────────────────────────────────────────

#[cfg(test)]
#[path = "pack/help_tests.rs"]
mod help_tests;

#[cfg(test)]
#[path = "gate_argument_contract_tests.rs"]
mod gate_argument_contract_tests;
