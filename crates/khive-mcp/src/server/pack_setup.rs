//! Single-runtime pack setup and provider binding.

use super::*;

impl KhiveMcpServer {
    #[allow(clippy::result_large_err)]
    pub(super) fn with_mounted_packs(
        runtime: KhiveRuntime,
        packs: &[String],
        mounted: Vec<khive_mounts::MountedPack>,
        config: Option<&khive_runtime::KhiveConfig>,
    ) -> Result<Self, PackRegError> {
        #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
        let channel_loop_admission = ChannelLoopAdmission::for_single_runtime(&runtime, packs);
        let gate = runtime.config().gate.clone();
        let default_namespace = runtime.config().default_namespace.clone();
        let config_id = compute_config_id_with_runtime_policies(
            runtime.config(),
            config,
            runtime.ann_fresh_tail_enabled(),
            runtime.is_read_only(),
        );
        let visible_namespaces = runtime.config().visible_namespaces.clone();
        let actor_id = runtime.config().actor_id.clone();
        let mut builder = VerbRegistryBuilder::new();
        builder.with_gate(gate);
        builder.with_default_namespace(default_namespace.as_str());
        builder.with_visible_namespaces(visible_namespaces);
        builder.with_actor_id(actor_id);
        // A read-only snapshot deliberately retains no EventStore handle; the
        // registry exposes an advisory beside each successful result instead.
        if runtime.is_read_only() {
            builder.with_read_only_audit_store();
        } else {
            // The configured sink must open at build before serving starts.
            builder
                .with_runtime_event_store(&runtime)
                .map_err(|source| PackRegError {
                    failure: PackRegFailure::Registry(source),
                    runtime: runtime.clone(),
                })?;
        }
        if let Err(load_err) = PackRegistry::register_packs(packs, runtime.clone(), &mut builder) {
            let failure = match load_err {
                PackLoadError::UnknownPack(name) => PackRegFailure::UnknownPack(name),
                PackLoadError::DuplicatePack(name) => PackRegFailure::DuplicatePack(name),
                PackLoadError::MissingDependency { pack, dep } => {
                    PackRegFailure::MissingDependency { pack, dep }
                }
                PackLoadError::NoPublicVerbs { pack } => PackRegFailure::NoPublicVerbs { pack },
            };
            return Err(PackRegError { failure, runtime });
        }
        for mount in mounted {
            builder
                .register_mounted(Box::new(mount))
                .map_err(|source| PackRegError {
                    failure: PackRegFailure::Registry(source),
                    runtime: runtime.clone(),
                })?;
        }
        if let Some(config) = config {
            for (pack, policy) in &config.packs {
                builder.with_disabled_verbs(pack, &policy.verbs_disabled);
            }
        }
        let registry = builder.build().map_err(|source| PackRegError {
            failure: PackRegFailure::Registry(source),
            runtime: runtime.clone(),
        })?;
        // Aggregate pack-declared edge endpoint rules into the runtime
        // so `validate_edge_relation_endpoints` can consult them.
        runtime.install_edge_rules(registry.all_edge_rules());
        registry
            .initialize_embedding_engines(&[&runtime])
            .map_err(|source| PackRegError {
                failure: PackRegFailure::Registry(source),
                runtime: runtime.clone(),
            })?;
        // Invoke `PackRuntime::register_entity_type_validator` on every pack so
        // entity-type validation is active at the runtime layer for all write
        // paths, including direct `create_many` callers that bypass the handler.
        registry.call_register_entity_type_validators(&runtime);
        // #750: install pack-owned note-mutation hooks (currently
        // only khive-pack-memory's warm-ANN-cache invalidation) so KG's
        // update/delete verbs notify caching packs even though there is no
        // crate-level dependency between them.
        registry.call_register_note_mutation_hooks(&runtime);
        registry.call_register_note_search_ann_providers(&runtime);
        // Note-write identity: the pack-owned kind set drives `update`'s
        // properties refusal and `merge`'s identity preservation; the
        // validator derives owned identity properties at every note-write.
        runtime.install_pack_owned_note_kinds(
            registry
                .pack_owned_note_kinds()
                .into_iter()
                .map(str::to_string)
                .collect(),
        );
        runtime.install_note_embedding_policies(&registry.all_note_embedding_policies());
        registry.call_register_note_write_validators(&runtime);
        // #2943: install entity-kind update hooks so the generic entity
        // `update` path can re-run a pack's create-time invariant against
        // the merged properties — same scope/timing as the entity-type
        // validator above.
        runtime.install_entity_kind_hooks(registry.entity_kind_hooks());
        // A required pack schema failure must refuse boot before any handler
        // can run. Use the same validation and error path as multi-backend boot.
        registry
            .apply_schema_plans_with_map(&HashMap::new(), runtime.backend())
            .map_err(|source| PackRegError {
                failure: PackRegFailure::Schema(source),
                runtime: runtime.clone(),
            })?;
        // Capture the pool arc for the WAL checkpoint task. Only available for
        // file-backed databases; in-memory backends return None here.
        let pool = if runtime.backend().is_file_backed() && !runtime.is_read_only() {
            Some(runtime.backend().pool_arc())
        } else {
            None
        };
        Ok(Self {
            registry,
            default_namespace: default_namespace.as_str().to_string(),
            config_id,
            coordinator: None,
            pool,
            secondary_pools: Vec::new(),
            default_output_format: OutputFormat::Json,
            schedule_ticker_last_tick_micros: Arc::new(AtomicI64::new(0)),
            #[cfg(unix)]
            bridge_executable: None,
            stdio_bridge: false,
            #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
            channel_outbox_runtime: Some(runtime.clone()),
            runtime: Some(runtime),
            #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
            channel_loop_admission,
        })
    }
}
