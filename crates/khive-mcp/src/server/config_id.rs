use super::*;

/// Compute daemon identity from construction-captured runtime policies.
///
/// `storage_read_only` is authoritative. Re-probing filesystem permissions
/// here could relabel a runtime that already retained a write-capable SQLite
/// handle after a later chmod; only the pre-open wrappers above may probe.
pub(crate) fn compute_config_id_with_runtime_policies(
    config: &RuntimeConfig,
    khive_cfg: Option<&khive_runtime::KhiveConfig>,
    ann_fresh_tail_enabled: bool,
    storage_read_only: bool,
) -> String {
    let mut packs = config.packs.clone();
    packs.sort();
    let db = config
        .db_path
        .as_deref()
        .map(canonical_fingerprint_path)
        .unwrap_or_else(|| ":memory:".to_string());
    let primary = config
        .embedding_model
        .as_ref()
        .map(|m| format!("{m:?}"))
        .unwrap_or_else(|| "none".to_string());
    let mut extra: Vec<String> = config
        .additional_embedding_models
        .iter()
        .map(|m| format!("{m:?}"))
        .collect();
    extra.sort();
    let mut outbound: Vec<String> = config
        .allowed_outbound_namespaces
        .iter()
        .map(|ns| ns.as_str().to_owned())
        .collect();
    outbound.sort();
    outbound.dedup();
    let gate = config
        .gate
        .configuration_fingerprint()
        .map(|fingerprint| format!(";gate={fingerprint}"))
        .unwrap_or_default();
    let mut git_write_hasher = Sha256::new();
    git_write_hasher.update(b"khive.git-write-policy.v2");
    git_write_hasher.update(
        serde_json::to_vec(&config.mounts).expect("mount configuration is JSON serializable"),
    );
    git_write_hasher.update(
        serde_json::to_vec(&config.git_write)
            .expect("git-write configuration is JSON serializable"),
    );
    let git_write = format!("{:x}", git_write_hasher.finalize());

    let mut fleet_readers = config.brain.fleet_readers.clone();
    fleet_readers.sort();
    fleet_readers.dedup();
    let mut brain_hasher = Sha256::new();
    brain_hasher.update(b"khive.brain-read-policy.v1");
    brain_hasher.update(
        serde_json::to_vec(&fleet_readers).expect("brain read policy is JSON serializable"),
    );
    let brain = format!("{:x}", brain_hasher.finalize());

    let mut telemetry_hasher = Sha256::new();
    telemetry_hasher.update(b"khive.telemetry-policy.v1");
    telemetry_hasher.update(
        serde_json::to_vec(&config.telemetry)
            .expect("telemetry configuration is JSON serializable"),
    );
    let telemetry = format!("{:x}", telemetry_hasher.finalize());

    // A warm daemon must not keep issuing receipts with a superseded key ring.
    // Hash declarations only: provider resolution and secret bytes never enter
    // daemon identity. Include an explicit version even when custody is absent
    // so pre-cutover writers cannot serve the new receipt contract.
    let mut receipt_credentials: Vec<_> = config
        .credentials
        .iter()
        .filter(|entry| {
            config
                .visibility_receipts
                .as_ref()
                .is_some_and(|ring| ring.keys.iter().any(|key| key.credential == entry.name))
        })
        .map(|entry| {
            (
                &entry.name,
                format!("{:?}", entry.kind),
                &entry.provider,
                &entry.env_var,
                &entry.header,
            )
        })
        .collect();
    receipt_credentials.sort();
    let receipt_keys = config.visibility_receipts.as_ref().map(|ring| {
        let mut keys: Vec<_> = ring
            .keys
            .iter()
            .map(|key| (&key.id, &key.credential, key.encrypt))
            .collect();
        keys.sort();
        keys
    });
    let mut receipt_hasher = Sha256::new();
    receipt_hasher.update(b"khive.visibility-receipt-policy.v2");
    receipt_hasher.update(
        serde_json::to_vec(&(receipt_credentials, receipt_keys))
            .expect("receipt configuration references are JSON serializable"),
    );
    let visibility_receipts = format!("{:x}", receipt_hasher.finalize());

    // The daemon compatibility parser compares this existing `backend` field.
    // For a declared topology, main uses the same effective value as its row
    // below; without one, RuntimeConfig already holds the resolved implicit
    // main value. Read-only storage has no writer ceiling even when a value
    // was configured for a writable deployment.
    let main_wal_ceiling_bytes = if storage_read_only {
        0
    } else {
        khive_cfg
            .and_then(|cfg| {
                cfg.backends
                    .iter()
                    .find(|backend| backend.name == khive_runtime::BackendId::MAIN)
            })
            .map(|backend| effective_named_wal_ceiling_bytes(config, backend))
            .unwrap_or(config.wal_ceiling_bytes)
    };
    let main_wal_ceiling = wal_ceiling_identity_suffix(main_wal_ceiling_bytes);
    let backend = if storage_read_only {
        format!("{:?}:read_only{main_wal_ceiling}", config.backend_id)
    } else {
        format!("{:?}{main_wal_ceiling}", config.backend_id)
    };
    // `display_timezone` is part of daemon identity, not merely of rendering
    // (ADR-169). `gtd.assign` anchors a date-only `due` through
    // `config.display_timezone` and PERSISTS the resulting instant, so two
    // runtimes differing only in this field are not interchangeable: a warm
    // daemon reused across them writes an instant that is wrong by the offset
    // between the zones, silently and durably.
    //
    // Included unconditionally rather than only when non-default. The default
    // is the HOST's zone, not UTC, so "differs from the default" is itself a
    // host-dependent predicate and would make identity depend on where the
    // fingerprint was computed.
    //
    // The cost, stated as it actually happens: a daemon already warm when this
    // lands keeps the identity it computed at startup, so a client built from
    // this code sends an ID that daemon does not recognise. The daemon answers
    // `config_mismatch` and the client falls back to LOCAL dispatch. It does
    // not respawn — `FallbackReason::ConfigMismatch` is classified
    // `FallbackSeverity::Illegitimate`, and the kill-and-respawn path
    // (#644/#539) governs the protocol/parse reasons, not this one. So until
    // that daemon is restarted, every request pays a failed forwarding round
    // trip and loses the daemon's warm indexes and embedders, and each one
    // increments a counter documented as never expected on a correctly
    // configured fleet.
    //
    // Spelled out because "the daemon takes a new identity" invites the reading
    // that it restarts itself. It does not, and nothing here makes it: this is
    // a one-time operational cost that ends when the daemon is restarted, by
    // whoever restarts it.
    let base = format!(
        concat!(
            "packs=[{}];db={};embed={};extra=[{}];fresh_tail={};",
            "blob_hydration_bytes={};backend={};outbound=[{}]{};",
            "git_write={};brain={};telemetry={};display_tz={};",
            "visibility_receipts={}"
        ),
        packs.join(","),
        db,
        primary,
        extra.join(","),
        ann_fresh_tail_enabled,
        config.blob_hydration_bytes,
        backend,
        outbound.join(","),
        gate,
        git_write,
        brain,
        telemetry,
        config.display_timezone.name(),
        visibility_receipts,
    );

    // Fold backend topology when non-empty so two configs differing only in
    // pack→backend routing produce different config_ids (ADR-049).
    // When backends is empty this branch is skipped; the implicit main ceiling
    // is already included in the `backend` component above.
    let topology = khive_cfg
        .filter(|cfg| !cfg.backends.is_empty())
        .map(|cfg| encode_backend_topology(cfg, config))
        .unwrap_or_default();

    // Default-disabled callers retain the established daemon identity. An
    // enabled host must not serve a caller whose boot policy disables local
    // filesystem transfers. Use the captured runtime policy, never the live
    // environment, so forwarding and pack initialization agree.
    let blob_file_transfers = if config.blob.file_transfers {
        ";blob_file_transfers=true"
    } else {
        ""
    };
    let disk_guard = disk_guard_policy_fingerprint(config, khive_cfg, storage_read_only);
    // Operator policy applies with implicit main as well as declared backends.
    let disabled_verbs = khive_cfg
        .map(disabled_verb_policy_suffix)
        .unwrap_or_default();
    format!("{base}{topology}{blob_file_transfers}{disk_guard}{disabled_verbs}")
}
