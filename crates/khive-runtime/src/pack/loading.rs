use super::{
    Arc, DispatchHook, HashMap, KhiveRuntime, PackByIdResolver, PackRuntime, RuntimeError,
    VerbRegistry, VerbRegistryBuilder, Visibility, CHANNEL_INGEST_CAPABLE_PACKS,
};

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
