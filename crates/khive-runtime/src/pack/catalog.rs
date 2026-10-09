//! Registry catalog, kind hooks, and pack lifecycle integration.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::error::RuntimeError;
use crate::operations::{LinkSpec, Resolved};
use crate::runtime::NamespaceToken;
use crate::validation::ValidationRule;
use crate::KhiveRuntime;

use super::request_identity::extract_table_names;
#[cfg(doc)]
use super::VerbPresentationPolicy;
use super::{
    EdgeEndpointRule, EntityTypeDef, HandlerDef, KindHook, NoteEmbeddingPolicySpec, NoteKindSpec,
    PackByIdResolver, PackColumnAddition, PackRuntime, SchemaPlan, VerbCategory, VerbRegistry,
    Visibility, GENERIC_CRUD_PACK,
};

impl VerbRegistry {
    /// Registered pack-level by-ID resolvers, in registration order.
    ///
    /// Each element is `(pack_name, resolver)`. The kg `get` and `delete` handlers
    /// iterate this slice to probe pack-private tables when the standard KG
    /// substrates (entity/note/edge/event) return `None` for a given UUID.
    pub fn resolvers(&self) -> &[(String, Box<dyn PackByIdResolver>)] {
        &self.resolvers
    }

    /// The daemon-warm recently-referenced ring (unified-verb draft ADR,
    /// Slice 1). Consumed by `resolve_reference` (Layer 0 stage 2) and by the
    /// `resolve` verb handler; admitted-to by every successful by-id
    /// dispatch (see the admission block in `dispatch_with_identity`).
    pub fn reference_ring(&self) -> &Arc<crate::reference_ring::ReferenceRing> {
        &self.reference_ring
    }

    /// Find a kind hook among the registered packs.
    ///
    /// Walks packs in registration order; the first pack that both owns the
    /// kind (declares it in `note_kinds()` or `entity_kinds()`) and returns
    /// a hook from `kind_hook(kind)` wins. Returns `None` if the kind is
    /// unknown to all packs or no owning pack registered a hook.
    pub fn find_kind_hook(&self, kind: &str) -> Option<Arc<dyn KindHook>> {
        for pack in self.packs.iter() {
            let owns = pack.note_kinds().contains(&kind) || pack.entity_kinds().contains(&kind);
            if owns {
                if let Some(hook) = pack.kind_hook(kind) {
                    return Some(hook);
                }
            }
        }
        None
    }

    /// Every `(entity kind, hook)` pair for which the owning pack declares
    /// the entity kind and registers a `KindHook` — the entity-scoped
    /// subset of [`Self::find_kind_hook`]'s ownership check, computed once.
    ///
    /// `khive-runtime` does not hold a `VerbRegistry` (ownership runs the
    /// other way: packs are constructed FROM a runtime handle), so
    /// `KhiveRuntime::install_entity_kind_hooks` is the extension point
    /// that carries this aggregate to the runtime layer — the transport
    /// calls this after the registry is built, same timing as
    /// [`Self::all_edge_rules`]. `Arc<dyn KindHook>` values returned here
    /// hold no reference back to the pack or registry that produced them
    /// (every production `kind_hook()` implementation constructs a fresh,
    /// stateless hook per call), so installing this aggregate on the
    /// runtime creates no ownership cycle.
    pub fn entity_kind_hooks(&self) -> crate::runtime::EntityKindHooks {
        let mut hooks = Vec::new();
        for pack in self.packs.iter() {
            for kind in pack.entity_kinds().iter().copied() {
                if let Some(hook) = pack.kind_hook(kind) {
                    hooks.push((kind.to_string(), hook));
                }
            }
        }
        hooks
    }

    /// Run the owning kind's shared-note-update normalizer/validator, if it declares one.
    ///
    /// Compatibility wrapper for callers that only need normalization and
    /// validation. Writers use [`Self::prepare_note_update_policy`] and attach
    /// its returned policy so kind-specific property removals reach storage.
    ///
    /// The ordering lives here, at the single dispatch site, rather than in a
    /// [`KindHook`] method a pack could override: a pack implements the two
    /// halves and cannot express a sequence, so it cannot replace the
    /// validator by overriding the sequence. See ADR-017.
    pub async fn prepare_note_update_hook(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        note: &khive_storage::Note,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        self.prepare_note_update_policy(runtime, token, note, args)
            .await
            .map(|_| ())
    }

    /// Normalize and validate a note update, then carry the owning kind's
    /// property policy into the shared prepared write. Writers must attach the
    /// returned policy to their `NotePatch` or snapshot update preparation;
    /// [`Self::prepare_note_update_hook`] remains the validation-only wrapper.
    pub async fn prepare_note_update_policy(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        note: &khive_storage::Note,
        args: &mut Value,
    ) -> Result<crate::NoteUpdatePolicy, RuntimeError> {
        crate::curation::normalize_note_update_tags(args)?;
        if let Some(hook) = self.find_kind_hook(&note.kind) {
            hook.normalize_note_update(runtime, token, note, args)
                .await?;
            let properties = args.get("properties").filter(|value| !value.is_null());
            hook.validate_note_update(runtime, token, note, properties)
                .await?;
            return Ok(crate::NoteUpdatePolicy::for_kind(
                &note.kind,
                hook.note_update_null_clearing_properties(),
            ));
        }
        Ok(crate::NoteUpdatePolicy::default())
    }

    /// Run the owning kind's shared-note-update property validator, if it
    /// declares one.
    ///
    /// Kept as the validation-only compatibility seam for callers that do not
    /// own a mutable request object. Canonical and atomic CRUD use
    /// [`Self::prepare_note_update_hook`] instead, so a hook's
    /// [`KindHook::normalize_note_update`] can run before its validation does.
    /// Reaching a hook through this seam therefore runs the validator alone:
    /// that is the point of it, and it is why callers that CAN supply a
    /// mutable request should not use it.
    pub async fn validate_note_update_hook(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        note: &khive_storage::Note,
        properties: Option<&Value>,
    ) -> Result<(), RuntimeError> {
        if let Some(hook) = self.find_kind_hook(&note.kind) {
            hook.validate_note_update(runtime, token, note, properties)
                .await?;
        }
        Ok(())
    }

    /// Run shared-link validators grouped by the owning source-note kind.
    ///
    /// Supplying the whole proposed batch lets a kind hook reject an invariant
    /// violation formed only by multiple entries in that batch. Sources that
    /// are not live notes, or whose kind has no hook, remain the canonical
    /// endpoint validator's responsibility.
    pub async fn validate_link_hooks(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        specs: &[LinkSpec],
    ) -> Result<(), RuntimeError> {
        let mut specs_by_kind: HashMap<String, Vec<LinkSpec>> = HashMap::new();
        for spec in specs {
            let Some(Resolved::Note(source)) = runtime.resolve_by_id(token, spec.source_id).await?
            else {
                continue;
            };
            specs_by_kind
                .entry(source.kind)
                .or_default()
                .push(spec.clone());
        }
        for (kind, kind_specs) in specs_by_kind {
            if let Some(hook) = self.find_kind_hook(&kind) {
                hook.validate_links(runtime, token, &kind_specs).await?;
            }
        }
        Ok(())
    }

    /// Whether any registered pack declares a handler with this verb name.
    ///
    /// A non-dispatch capability check: callers that would otherwise pay a
    /// guaranteed-failed `dispatch` (and its audit write) when an optional
    /// pack is absent can probe first and skip the call entirely.
    pub fn has_verb(&self, verb: &str) -> bool {
        self.handler_by_name.contains_key(verb) && !self.is_verb_disabled(verb)
    }

    /// Whether operator policy disables this registered public handler.
    pub fn is_verb_disabled(&self, verb: &str) -> bool {
        self.disabled_verbs.contains(verb)
    }

    pub(crate) fn unknown_verb_error(&self, verb: &str) -> RuntimeError {
        RuntimeError::UnknownVerb(format!(
            "unknown verb {verb:?}; available: {}",
            self.available_verbs.join(", ")
        ))
    }

    /// Advisory metadata for synchronous planning and MCP initialization.
    pub fn mounted_verb_snapshot(&self) -> Vec<Value> {
        self.packs
            .iter()
            .flat_map(|pack| {
                pack.mounted_catalog_snapshot()
                    .into_iter()
                    .map(|verb| verb.describe(pack.name()))
            })
            .collect()
    }

    pub async fn mounted_verb_catalog(&self) -> Result<Vec<Value>, RuntimeError> {
        let mut catalog = Vec::new();
        for pack in self.packs.iter() {
            for definition in pack.mounted_catalog().await? {
                catalog.push(definition.describe(pack.name()));
            }
        }
        Ok(catalog)
    }

    /// Apply section evidence through the installed brain instance. Callers must
    /// validate their domain target and authorize their own operation first;
    /// this trusted Rust hook adds no handler to dispatch or the wire catalog.
    pub async fn apply_profile_section_feedback(
        &self,
        token: &NamespaceToken,
        profile_id: &str,
        section_signals: Value,
        target_attribution: Option<String>,
    ) -> Result<Value, RuntimeError> {
        let brain = self
            .packs
            .iter()
            .find(|pack| pack.name() == "brain")
            .ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "profile section feedback requires the brain pack".into(),
                )
            })?;
        brain
            .apply_profile_section_feedback(token, profile_id, section_signals, target_attribution)
            .await
    }

    /// All MCP-exposed handlers across all registered packs (`Visibility::Verb` only).
    ///
    /// Disabled verbs and subhandlers (`Visibility::Subhandler`) are excluded — subhandlers are internal
    /// pipeline steps not surfaced on the MCP wire. Returned with `'static`
    /// lifetime since pack handlers are `&'static [HandlerDef]` constants.
    pub fn all_verbs(&self) -> Vec<&'static HandlerDef> {
        self.packs
            .iter()
            .flat_map(|p| p.handlers().iter())
            .filter(|h| matches!(h.visibility, Visibility::Verb) && !self.is_verb_disabled(h.name))
            .collect()
    }

    /// All MCP-exposed handlers paired with the name of the pack that owns them
    /// (`Visibility::Verb` only).
    ///
    /// Subhandlers (`Visibility::Subhandler`) are excluded from the MCP catalog
    /// Use `all_handlers_with_names` when internal handlers must
    /// also be enumerated (e.g. runtime introspection).
    pub fn all_verbs_with_names(&self) -> Vec<(&str, &'static HandlerDef)> {
        self.packs
            .iter()
            .flat_map(|p| p.handlers().iter().map(move |v| (p.name(), v)))
            .filter(|(_, h)| {
                matches!(h.visibility, Visibility::Verb) && !self.is_verb_disabled(h.name)
            })
            .collect()
    }

    /// All handler definitions across all registered packs, including disabled verbs and subhandlers.
    ///
    /// Unlike `all_verbs`, this includes `Visibility::Subhandler` entries. Useful
    /// for runtime introspection (e.g. `list_handlers`) and tooling that needs
    /// the complete handler surface.
    pub fn all_handlers_with_names(&self) -> Vec<(&str, &'static HandlerDef)> {
        self.packs
            .iter()
            .flat_map(|p| p.handlers().iter().map(move |v| (p.name(), v)))
            .collect()
    }

    /// Collect declared kinds once, retaining their first-seen registration order.
    fn collect_pack_kinds(
        &self,
        select: impl Fn(&dyn PackRuntime) -> &'static [&'static str],
    ) -> Vec<&'static str> {
        let mut seen = std::collections::HashSet::new();
        self.packs
            .iter()
            .flat_map(|pack| select(pack.as_ref()).iter().copied())
            .filter(|kind| seen.insert(*kind))
            .collect()
    }

    /// Merged set of note kinds across all registered packs (deduplicated,
    /// first-seen order preserved).
    pub fn all_note_kinds(&self) -> Vec<&'static str> {
        self.collect_pack_kinds(|pack| pack.note_kinds())
    }

    /// Note kinds owned by a pack, i.e. every kind in [`all_note_kinds`] that
    /// is not one of the generic-CRUD pack's own kinds.
    ///
    /// [`GENERIC_CRUD_PACK`] declares the general-purpose note kinds the shared
    /// CRUD verbs exist to serve (`observation`, `insight`, …); every other
    /// pack's kinds are records that pack's own verbs create and maintain.
    /// Derived from the packs' `NOTE_KINDS` constants, so a pack that adds or
    /// drops a kind moves this set with it — nothing is hardcoded here but the
    /// name of the generic pack itself.
    ///
    /// [`all_note_kinds`]: Self::all_note_kinds
    pub fn pack_owned_note_kinds(&self) -> Vec<&'static str> {
        let generic: std::collections::HashSet<&'static str> = self
            .packs
            .iter()
            .filter(|p| p.name() == GENERIC_CRUD_PACK)
            .flat_map(|p| p.note_kinds().iter().copied())
            .collect();
        let mut seen = std::collections::HashSet::new();
        self.packs
            .iter()
            .filter(|p| p.name() != GENERIC_CRUD_PACK)
            .flat_map(|p| p.note_kinds().iter().copied())
            .filter(|k| !generic.contains(k) && seen.insert(*k))
            .collect()
    }

    /// Merged set of entity kinds across all registered packs (deduplicated,
    /// first-seen order preserved).
    pub fn all_entity_kinds(&self) -> Vec<&'static str> {
        self.collect_pack_kinds(|pack| pack.entity_kinds())
    }

    /// Merged set of brain profile consumer kinds requested by registered
    /// packs (deduplicated, first-seen order preserved).
    pub fn all_brain_consumer_kinds(&self) -> Vec<&'static str> {
        self.collect_pack_kinds(|pack| pack.brain_consumer_kinds())
    }

    /// Names of packs in topological load order.
    pub fn pack_names(&self) -> Vec<&str> {
        self.packs.iter().map(|p| p.name()).collect()
    }

    /// Borrow a registered pack's shared host state without reconstructing
    /// that pack. Missing packs, absent state, and type mismatches return None.
    pub fn pack_host_state<T: Any + Send + Sync>(&self, name: &str) -> Option<Arc<T>> {
        self.packs
            .iter()
            .find(|pack| pack.name() == name)?
            .host_state()?
            .downcast::<T>()
            .ok()
    }

    /// Declared dependencies for a registered pack.
    pub fn pack_requires(&self, name: &str) -> Option<&'static [&'static str]> {
        self.packs
            .iter()
            .find(|p| p.name() == name)
            .map(|p| p.requires())
    }

    /// Note kinds owned by a specific registered pack.
    ///
    /// Returns `None` if no pack with `name` is registered. The slice is
    /// the pack's `NOTE_KINDS` constant — `'static` lifetime, no allocation.
    pub fn pack_note_kinds(&self, name: &str) -> Option<&'static [&'static str]> {
        self.packs
            .iter()
            .find(|p| p.name() == name)
            .map(|p| p.note_kinds())
    }

    /// Entity kinds owned by a specific registered pack.
    ///
    /// Returns `None` if no pack with `name` is registered. The slice is
    /// the pack's `ENTITY_KINDS` constant — `'static` lifetime, no allocation.
    pub fn pack_entity_kinds(&self, name: &str) -> Option<&'static [&'static str]> {
        self.packs
            .iter()
            .find(|p| p.name() == name)
            .map(|p| p.entity_kinds())
    }

    /// Handlers declared by a specific registered pack.
    ///
    /// Returns `None` if no pack with `name` is registered. Each `HandlerDef`
    /// carries name + description + visibility — sufficient for introspection clients.
    pub fn pack_verbs(&self, name: &str) -> Option<&'static [HandlerDef]> {
        self.packs
            .iter()
            .find(|p| p.name() == name)
            .map(|p| p.handlers())
    }

    /// Collect pack items in registration order without filtering or deduplication.
    fn collect_pack_items<T, I>(&self, select: impl Fn(&dyn PackRuntime) -> I) -> Vec<T>
    where
        I: IntoIterator<Item = T>,
    {
        self.packs
            .iter()
            .flat_map(|pack| select(pack.as_ref()))
            .collect()
    }

    /// All pack-declared edge endpoint rules across registered packs.
    ///
    /// Order follows topological pack registration; duplicates are *not* deduplicated —
    /// validation only checks membership, and an exact-duplicate rule is a
    /// harmless restatement.
    pub fn all_edge_rules(&self) -> Vec<EdgeEndpointRule> {
        self.collect_pack_items(|pack| pack.edge_rules().iter().copied())
    }

    /// All pack-declared entity-type subtypes across registered packs.
    ///
    /// Order follows topological pack registration; duplicates are *not*
    /// deduplicated here — same posture as [`all_edge_rules`](Self::all_edge_rules).
    /// Consumers compose this with `EntityTypeRegistry::builtin()` via
    /// `EntityTypeRegistry::with_extra` to get the boot-time composed registry.
    pub fn all_entity_types(&self) -> Vec<EntityTypeDef> {
        self.collect_pack_items(|pack| pack.entity_types().iter().cloned())
    }

    /// Collect all `NoteKindSpec` declarations from every loaded pack.
    ///
    /// Used by the runtime for lifecycle introspection and future enforcement.
    pub fn all_note_kind_specs(&self) -> Vec<&'static NoteKindSpec> {
        self.collect_pack_items(|pack| pack.note_kind_specs().iter())
    }

    /// Collect pack-declared embedding policies for registered note kinds.
    pub fn all_note_embedding_policies(&self) -> Vec<NoteEmbeddingPolicySpec> {
        self.collect_pack_items(|pack| pack.note_embedding_policies().iter().copied())
    }

    /// All pack-contributed validation rules across registered packs.
    ///
    /// Returns references into the pack-owned `'static` slices — no allocation
    /// beyond the outer `Vec`. Rule IDs are namespaced by pack; callers can
    /// group by `rule.id.split_once('/')` to attribute rules to their packs.
    pub fn all_validation_rules(&self) -> Vec<&'static ValidationRule> {
        self.collect_pack_items(|pack| pack.validation_rules().iter())
    }

    /// Pack-auxiliary schema plans for all registered packs.
    ///
    /// Returns one `SchemaPlan` per pack. Callers (typically the runtime
    /// bootstrap) apply each plan to the pack's assigned backend. Empty plans
    /// are included so the caller can iterate uniformly; callers that want to
    /// skip empty plans should check `plan.is_empty()`. Schema application must
    /// use [`Self::all_schema_plans_with_columns`] to retain column upgrades.
    pub fn all_schema_plans(&self) -> Vec<SchemaPlan> {
        self.packs.iter().map(|p| p.schema_plan()).collect()
    }

    /// Schema plans paired with the same owning pack's nullable-column upgrades.
    ///
    /// Callers applying plans directly must pass both entries to
    /// `StorageBackend::apply_pack_ddl_statements_with_columns`.
    pub fn all_schema_plans_with_columns(
        &self,
    ) -> Vec<(SchemaPlan, &'static [PackColumnAddition])> {
        self.packs
            .iter()
            .map(|pack| (pack.schema_plan(), pack.schema_column_additions()))
            .collect()
    }

    /// Invoke `PackRuntime::register_embedders` on every registered pack.
    ///
    /// Called by the transport during startup, after the registry is built and
    /// before the first verb dispatch, so that custom embedding providers
    /// contributed by packs are reachable via `KhiveRuntime::embedder(name)`.
    ///
    /// Packs whose `register_embedders` is the default no-op pay no overhead.
    /// The method is idempotent when the underlying registry uses last-wins
    /// semantics for duplicate provider names.
    pub fn call_register_embedders(&self, runtime: &KhiveRuntime) {
        for pack in self.packs.iter() {
            pack.register_embedders(runtime);
        }
    }

    /// Invoke `PackRuntime::register_entity_type_validator` on every registered pack.
    ///
    /// Called by the transport during startup, after the registry is built and
    /// before the first verb dispatch, so that entity-type validation at the
    /// runtime layer is active for all write paths including direct `create_many`
    /// callers that bypass the handler layer.
    ///
    /// Packs whose `register_entity_type_validator` is the default no-op pay
    /// no overhead.
    ///
    /// Composes [`all_entity_types`](Self::all_entity_types) once and passes
    /// the same aggregate to every pack, mirroring how `install_edge_rules`
    /// installs one `all_edge_rules()` aggregate for the whole registry.
    pub fn call_register_entity_type_validators(&self, runtime: &KhiveRuntime) {
        let entity_types = self.all_entity_types();
        for pack in self.packs.iter() {
            pack.register_entity_type_validator_with_types(runtime, &entity_types);
        }
    }

    /// Invoke `PackRuntime::register_note_mutation_hook` on every registered pack.
    ///
    /// Called by the transport during startup, after the registry is built and
    /// before the first verb dispatch, so that note-mutation notifications at
    /// the runtime layer are active for all write paths — including KG's
    /// `update`/`delete` verbs reaching a `kind="memory"` note, which have no
    /// crate-level dependency on `khive-pack-memory`.
    ///
    /// Packs whose `register_note_mutation_hook` is the default no-op pay no
    /// overhead.
    pub fn call_register_note_mutation_hooks(&self, runtime: &KhiveRuntime) {
        for pack in self.packs.iter() {
            pack.register_note_mutation_hook(runtime);
        }
    }

    /// Install pack-owned note-search candidate sources before warm-up or
    /// dispatch, following the same registration timing as mutation hooks.
    pub fn call_register_note_search_ann_providers(&self, runtime: &KhiveRuntime) {
        for pack in self.packs.iter() {
            pack.register_note_search_ann_provider(runtime);
        }
    }

    /// Invoke `PackRuntime::register_note_write_validator` on every registered pack.
    ///
    /// Called by the transport during startup with the same timing as
    /// `call_register_note_mutation_hooks`, so note-write validation is active
    /// at the runtime layer for every write path — the generic `create` verb,
    /// direct Rust callers, and proposal apply, none of which dispatch a pack
    /// hook of their own on the note-write.
    pub fn call_register_note_write_validators(&self, runtime: &KhiveRuntime) {
        for pack in self.packs.iter() {
            pack.register_note_write_validator(runtime);
        }
    }

    /// Invoke `PackRuntime::warm` on every registered pack.
    /// Called by the daemon at boot (in a background task) so expensive in-memory
    /// state (ANN indexes) is pre-loaded without blocking request serving.
    pub async fn call_warm_all(&self) {
        for pack in self.packs.iter() {
            pack.warm().await;
        }
    }

    /// Resolve the presentation policy for a verb name.
    ///
    /// Uses the first registered handler (including subhandlers) with this name
    /// and returns its declared [`VerbPresentationPolicy`].
    /// Returns `Standard` for unknown verbs — unknown verbs will fail at
    /// dispatch anyway, so the fallback here is safe.
    pub fn presentation_policy_for(&self, verb: &str) -> khive_types::VerbPresentationPolicy {
        self.handler_by_name
            .get(verb)
            .map_or(khive_types::VerbPresentationPolicy::Standard, |handler| {
                handler.presentation_policy()
            })
    }

    /// Resolve the declared [`VerbCategory`] for a verb name.
    ///
    /// Uses the first registered handler (including subhandlers) with this name
    /// and returns its speech-act category. Returns `None` for
    /// an unregistered verb name, so a caller deciding transport-level
    /// behavior (e.g. whether a post-dispatch condition is safe to retry)
    /// can fail closed on an unknown verb instead of guessing a category.
    pub fn verb_category(&self, verb: &str) -> Option<VerbCategory> {
        self.handler_by_name
            .get(verb)
            .map(|handler| handler.category)
    }

    /// Verbs classified [`VerbCategory::Assertive`] that nonetheless schedule
    /// can schedule a persisted write on a successful dispatch, so a caller re-issuing
    /// a call in this list after a lost response duplicates that write:
    ///
    /// - `memory.recall` schedules `brain.record_serve`, which inserts a
    ///   serve-ledger row keyed in part on a `served_at` timestamp captured
    ///   fresh at dispatch time — a second dispatch inserts a second row
    ///   rather than colliding with the first.
    /// - `search` (the `kg` pack's bare verb) appends a `search_executed`
    ///   event with a freshly generated id and no natural key at all.
    /// - `telemetry.emit` can append a durable stream record with a fresh
    ///   identity and sequence, depending on the configured channel policy.
    /// - `tool.check` appends a `tool_check_decided` receipt with a fresh
    ///   event id for every evaluated decision (ADR-180 Amendment 6).
    ///
    /// The speech-act category alone cannot rule this out — it describes
    /// what the verb tells the *caller*, not what it schedules against
    /// storage. Adding a verb here (or removing one because its side effect
    /// was made idempotent) is a correctness decision requiring the same
    /// scrutiny as the categorization itself.
    pub const SIDE_EFFECTING_ASSERTIVE_VERBS: &'static [&'static str] =
        &["memory.recall", "search", "telemetry.emit", "tool.check"];

    /// Whether a response lost to the daemon frame budget may be truthfully
    /// advertised as safe to re-issue: the verb is [`VerbCategory::Assertive`]
    /// (no institutional commitment was made) and is not on
    /// `Self::SIDE_EFFECTING_ASSERTIVE_VERBS` (no persisted write to
    /// duplicate on a second dispatch). An unregistered verb name resolves to
    /// `None` from [`Self::verb_category`] and fails closed here.
    ///
    /// Used only by the MCP daemon's frame-budget omission decision; never
    /// for permission checking or return-shape selection.
    pub fn is_retry_safe_after_frame_omission(&self, verb: &str) -> bool {
        matches!(self.verb_category(verb), Some(VerbCategory::Assertive))
            && !Self::SIDE_EFFECTING_ASSERTIVE_VERBS.contains(&verb)
    }

    /// Returns `true` if the named verb exists and is tagged
    /// `Visibility::Subhandler` (internal / operator-only).
    ///
    /// Used by the MCP server to gate subhandler invocation at the wire
    /// boundary without blocking internal callers that invoke the same verbs
    /// through the runtime directly.
    pub fn is_subhandler_verb(&self, verb: &str) -> bool {
        self.handler_by_name
            .get(verb)
            .is_some_and(|handler| matches!(handler.visibility, Visibility::Subhandler))
    }

    /// Apply all non-empty pack-auxiliary schema plans to the given backend.
    ///
    /// This is the centralized startup hook that replaced the previous lazy
    /// per-pack self-bootstrap pattern. Each pack's `SchemaPlan` carries
    /// idempotent `CREATE TABLE IF NOT EXISTS` DDL; calling this more than once
    /// is safe. Plans with neither SQL nor column upgrades are skipped.
    ///
    /// Errors from individual plans are logged via `tracing::warn!` and not
    /// propagated so that a single pack's schema failure does not prevent the
    /// rest from loading. Serving hosts must instead use the fallible
    /// [`Self::apply_schema_plans_with_map`] (with an empty map for one backend)
    /// so a required schema failure cannot leave a pack's verbs unavailable.
    pub fn apply_schema_plans(&self, backend: &khive_db::StorageBackend) {
        if backend.is_read_only() {
            tracing::info!(
                "skipping pack schema plans because the backend is read-only; snapshot schema is used as-is"
            );
            return;
        }
        for (plan, additions) in self.all_schema_plans_with_columns() {
            if plan.is_empty() && additions.is_empty() {
                continue;
            }
            if let Err(e) =
                backend.apply_pack_ddl_statements_with_columns(plan.statements, additions)
            {
                tracing::warn!(
                    pack = plan.pack,
                    error = %e,
                    "failed to apply pack schema plan at startup (non-fatal)"
                );
            }
        }
    }

    /// Pack-auxiliary schema plans with their owning pack names.
    ///
    /// Returns `(pack_name, SchemaPlan)` pairs for every registered pack.
    /// Used by the multi-backend boot path to apply each plan to the pack's
    /// assigned backend rather than a single shared backend. Direct schema
    /// application must use [`Self::all_schema_plans_with_columns`] so column
    /// upgrades are retained.
    pub fn all_schema_plans_named(&self) -> Vec<(&'static str, SchemaPlan)> {
        self.packs
            .iter()
            .map(|p| {
                let plan = p.schema_plan();
                (plan.pack, plan)
            })
            .collect()
    }

    /// Apply pack-auxiliary schema plans using a per-pack backend map.
    ///
    /// For each plan and its owning pack's column additions, applies the full
    /// plan to `backend_for_pack[plan.pack]` when present,
    /// falling back to `default_backend` for any pack not in the map.
    ///
    /// Returns an error when two packs on the same backend declare the same
    /// auxiliary table (ADR-028 §7 collision policy: boot failure naming both
    /// packs and the conflicting table).
    ///
    /// Both single- and multi-backend hosts use this boot path (ADR-028).
    /// An empty map selects the default backend for every pack. Read-only
    /// backends validate declared columns without applying SQL or acquiring a
    /// writer; missing or incompatible columns refuse boot with the pack name.
    pub fn apply_schema_plans_with_map(
        &self,
        backend_for_pack: &HashMap<&str, &khive_db::StorageBackend>,
        default_backend: &khive_db::StorageBackend,
    ) -> Result<(), crate::PackSchemaCollisionError> {
        // Track which pack first claimed each table on each backend.
        // Backend identity is the raw pointer of the underlying connection pool Arc.
        let mut claimed: HashMap<(*const (), String), &'static str> = HashMap::new();

        let plans = self.all_schema_plans_with_columns();
        // Check every declaration before applying any pack DDL. A collision
        // must not leave earlier plans installed on a failed boot.
        for (plan, additions) in &plans {
            if plan.is_empty() && additions.is_empty() {
                continue;
            }
            let pack_name = plan.pack;
            let backend = backend_for_pack
                .get(pack_name)
                .copied()
                .unwrap_or(default_backend);
            let backend_ptr = std::sync::Arc::as_ptr(&backend.pool_arc()) as *const ();

            // Collect DDL table ownership for the full plan set.
            for stmt in plan.statements {
                for table_name in extract_table_names(stmt) {
                    let key = (backend_ptr, table_name.clone());
                    match claimed.entry(key) {
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert(pack_name);
                        }
                        std::collections::hash_map::Entry::Occupied(e) => {
                            let prior_pack = *e.get();
                            return Err(crate::PackSchemaCollisionError {
                                pack_a: prior_pack,
                                pack_b: pack_name,
                                table: table_name,
                            });
                        }
                    }
                }
            }
            for addition in *additions {
                let table_name = addition.table.to_ascii_lowercase();
                let key = (backend_ptr, table_name.clone());
                match claimed.entry(key) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(pack_name);
                    }
                    std::collections::hash_map::Entry::Occupied(entry) => {
                        let prior_pack = *entry.get();
                        // A pack's full CREATE and its upgrades declare the
                        // same table; this is one ownership claim.
                        if prior_pack != pack_name {
                            return Err(crate::PackSchemaCollisionError {
                                pack_a: prior_pack,
                                pack_b: pack_name,
                                table: table_name,
                            });
                        }
                    }
                }
            }
        }

        for (plan, additions) in plans {
            if plan.is_empty() && additions.is_empty() {
                continue;
            }
            let pack_name = plan.pack;
            let backend = backend_for_pack
                .get(pack_name)
                .copied()
                .unwrap_or(default_backend);
            if backend.is_read_only() {
                backend.validate_pack_schema_columns(additions).map_err(|error| {
                    crate::PackSchemaCollisionError {
                        pack_a: pack_name,
                        pack_b: pack_name,
                        table: format!("read-only schema validation failed: {error}; open the database writable to apply the pack schema upgrade"),
                    }
                })?;
                continue;
            }

            backend
                .apply_pack_ddl_statements_with_columns(plan.statements, additions)
                .map_err(|e| crate::PackSchemaCollisionError {
                    pack_a: pack_name,
                    pack_b: pack_name,
                    table: format!("DDL error: {e}"),
                })?;
        }
        Ok(())
    }
}
