use super::*;

/// Fingerprint the engine-coherence parts of a resolved [`RuntimeConfig`].
///
/// Identical resolved configurations produce the same id. A daemon may also
/// serve a compatible client with a different id when it has a superset of
/// the client's requested extra embedders. Every other field must match:
/// same pack set (order-independent), same storage target and effective access
/// mode, same primary embedder, same backend topology/routing, and same
/// construction-baked fresh-tail, blob-hydration, outbound, caller-enrollment,
/// and git-write policies.
/// Identity fields (`namespace`, `actor_id`, `visible_namespaces`) are carried
/// per request in the daemon frame and must never enter this key. The daemon
/// compares this against each forwarded request's `config_id` and rejects any
/// difference outside the extra-embedder superset rule, so a restricted client
/// cannot execute through a broader runtime with incompatible behavior.
///
/// When `khive_cfg` is supplied and contains a non-empty `[[backends]]`
/// declaration, the backend topology (sorted backend list, explicit read-only
/// modes, effective WAL ceilings, served-substrate declarations, and
/// pack→backend assignments) is
/// folded into the fingerprint so that two configs differing only in routing,
/// access mode, or effective ceiling produce different ids (ADR-049 / B-SHOULD-FIX-4).
/// Delimiter-free topologies retain their legacy field encoding; a topology
/// containing reserved delimiter text uses an injective, escaped v2 encoding
/// so path data can never impersonate access mode.
///
/// When `khive_cfg` is `None` or its `backends` list is empty, the implicit
/// main backend's effective WAL ceiling is folded into the `backend` field.
/// An existing path with no filesystem write bits gains the read-only backend
/// marker before the runtime opens it, so forwarding and server fingerprints
/// converge.
///
/// `config.db_path` and each declared backend path are canonicalized against
/// the process's current working directory before entering the fingerprint. A
/// raw relative string (e.g. `./data/main.db`) would otherwise fingerprint
/// identically for two different projects that happen to declare or override
/// the same relative path, even though they resolve to two different files —
/// letting a warm daemon started for one project accept requests meant for
/// the other's database.
pub fn compute_config_id(
    config: &RuntimeConfig,
    khive_cfg: Option<&khive_runtime::KhiveConfig>,
) -> String {
    compute_config_id_with_runtime_policies(
        config,
        khive_cfg,
        khive_runtime::ann_fresh_tail_enabled_from_env(),
        configured_storage_read_only(config, khive_cfg),
    )
}

/// Compute the daemon identity with an already-snapshotted ADR-118 policy.
///
/// Test-only compatibility wrapper for exercising one already-snapshotted
/// policy. Runtime-owning call sites pass both captured policies through
/// [`compute_config_id_with_runtime_policies`].
#[cfg(test)]
pub(crate) fn compute_config_id_with_ann_fresh_tail(
    config: &RuntimeConfig,
    khive_cfg: Option<&khive_runtime::KhiveConfig>,
    ann_fresh_tail_enabled: bool,
) -> String {
    compute_config_id_with_runtime_policies(
        config,
        khive_cfg,
        ann_fresh_tail_enabled,
        configured_storage_read_only(config, khive_cfg),
    )
}

fn configured_storage_read_only(
    config: &RuntimeConfig,
    khive_cfg: Option<&khive_runtime::KhiveConfig>,
) -> bool {
    if let Some(main) = khive_cfg
        .filter(|cfg| !cfg.backends.is_empty())
        .and_then(|cfg| cfg.backends.iter().find(|backend| backend.name == "main"))
    {
        return main.kind == khive_runtime::BackendKind::Sqlite && main.read_only;
    }

    config.db_path.as_ref().is_some_and(|path| {
        std::fs::metadata(khive_runtime::expand_tilde(path))
            .is_ok_and(|metadata| metadata.permissions().readonly())
    })
}

/// Compute the daemon identity with an authoritative effective storage mode.
///
/// A chmod-detected snapshot has the same configured path as its writable
/// source but cannot safely share a warm daemon with it: the writable daemon
/// would omit the audit advisory and could retain a write-capable file handle.
/// Fold the effective main-backend mode into the existing `backend` component
/// so the mismatch remains parseable as a structured backend mismatch.
/// Pre-open callers that have already applied a storage override (for example,
/// multi-backend `--db :memory:`) must use this form rather than re-reading
/// the superseded declaration through [`compute_config_id`].
pub fn compute_config_id_with_storage_mode(
    config: &RuntimeConfig,
    khive_cfg: Option<&khive_runtime::KhiveConfig>,
    storage_read_only: bool,
) -> String {
    compute_config_id_with_runtime_policies(
        config,
        khive_cfg,
        khive_runtime::ann_fresh_tail_enabled_from_env(),
        storage_read_only,
    )
}

/// Reserved syntax in the legacy topology spelling.
///
/// Keeping the legacy representation when every caller-controlled component
/// excludes these bytes avoids needless changes to topology encoding without
/// retaining its ambiguity. The v2 marker itself contains `|`, so a safe
/// legacy value can never equal a v2 value.
fn legacy_topology_component_is_safe(value: &str) -> bool {
    !value
        .bytes()
        .any(|byte| matches!(byte, b':' | b',' | b'[' | b']' | b'=' | b';' | b'|'))
}

/// Percent-encode a v2 topology field so its payload can contain none of the
/// structural `:`, `,`, or `=` delimiters. `%` itself is always escaped, making
/// the mapping injective over the original UTF-8 bytes.
fn escape_topology_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut escaped = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/') {
            escaped.push(byte as char);
        } else {
            escaped.push('%');
            escaped.push(HEX[(byte >> 4) as usize] as char);
            escaped.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    escaped
}

/// Format a backend's served-kinds fingerprint component (`""` when the
/// backend serves everything, `:serves=<kind>+<kind>` otherwise), shared by
/// both the legacy and escaped topology encodings below so they always agree
/// on the same served-kinds suffix for the same input.
pub(super) fn format_served_kinds_suffix(served_kinds: Option<&str>) -> String {
    served_kinds
        .map(|kinds| format!(":serves={kinds}"))
        .unwrap_or_default()
}

/// Identity spelling of an effective WAL ceiling. The numeric value is always
/// encoded, zero included: a disabled ceiling is an explicit policy that a
/// daemon must be able to report, so a client with a disabled ceiling must not
/// reuse a daemon built before ceilings existed. That daemon fingerprints
/// differently and the client falls back to local dispatch until it restarts.
fn wal_ceiling_identity_suffix(effective_bytes: u64) -> String {
    format!(":wal_ceiling_bytes={effective_bytes}")
}

/// Only the ceiling enforced by a writable SQLite backend participates in
/// daemon identity. The configured value and its source remain operator
/// diagnostics; a read-only or in-memory backend enforces no writer policy.
fn effective_named_wal_ceiling_bytes(
    config: &RuntimeConfig,
    backend: &khive_runtime::BackendConfig,
) -> u64 {
    if backend.read_only || backend.kind != khive_runtime::BackendKind::Sqlite {
        0
    } else {
        backend
            .wal_ceiling_bytes
            .unwrap_or(config.wal_ceiling_configured_bytes)
    }
}

pub(super) fn encode_backend_topology(
    cfg: &khive_runtime::KhiveConfig,
    config: &RuntimeConfig,
) -> String {
    let mut legacy_safe = true;
    let mut backend_rows: Vec<(String, String, String, bool, Option<String>, u64)> = cfg
        .backends
        .iter()
        .map(|backend| {
            let kind = format!("{:?}", backend.kind);
            let path = backend
                .path
                .as_deref()
                .map(canonical_fingerprint_path)
                .unwrap_or_else(|| ":memory:".to_string());
            legacy_safe &= legacy_topology_component_is_safe(&backend.name)
                && legacy_topology_component_is_safe(&kind)
                && backend
                    .path
                    .as_ref()
                    .is_none_or(|_| legacy_topology_component_is_safe(&path));
            let served_kinds = backend.served_kinds.as_ref().map(|kinds| {
                kinds
                    .iter()
                    .map(|kind| kind.name())
                    .collect::<Vec<_>>()
                    .join("+")
            });
            (
                backend.name.clone(),
                kind,
                path,
                backend.read_only,
                served_kinds,
                effective_named_wal_ceiling_bytes(config, backend),
            )
        })
        .collect();
    backend_rows.sort();

    let mut pack_rows: Vec<(String, String, bool)> = cfg
        .packs
        .iter()
        .map(|(pack, pack_config)| {
            legacy_safe &= legacy_topology_component_is_safe(pack)
                && legacy_topology_component_is_safe(&pack_config.backend);
            (
                pack.clone(),
                pack_config.backend.clone(),
                pack_config.no_embed,
            )
        })
        .collect();
    pack_rows.sort();

    let (backends, pack_backends) = if legacy_safe {
        let backends = backend_rows
            .iter()
            .map(
                |(name, kind, path, is_read_only, served_kinds, wal_ceiling_bytes)| {
                    let read_only = if *is_read_only { ":read_only" } else { "" };
                    let served_kinds = format_served_kinds_suffix(served_kinds.as_deref());
                    let wal_ceiling = wal_ceiling_identity_suffix(*wal_ceiling_bytes);
                    format!("{name}:{kind}:{path}{read_only}{served_kinds}{wal_ceiling}")
                },
            )
            .collect::<Vec<_>>()
            .join(",");
        let pack_backends = pack_rows
            .iter()
            .map(|(pack, backend, no_embed)| {
                // `no_embed` changes runtime behavior (that pack's runtime
                // carries zero embedders), so it must move the fingerprint;
                // emitted only when set so pre-existing configs keep their id.
                let no_embed = if *no_embed { ":no_embed" } else { "" };
                format!("{pack}={backend}{no_embed}")
            })
            .collect::<Vec<_>>()
            .join(",");
        (backends, pack_backends)
    } else {
        let backends = backend_rows
            .iter()
            .map(
                |(name, kind, path, read_only, served_kinds, wal_ceiling_bytes)| {
                    let mode = if *read_only { "r" } else { "w" };
                    let served_kinds = format_served_kinds_suffix(served_kinds.as_deref());
                    let wal_ceiling = wal_ceiling_identity_suffix(*wal_ceiling_bytes);
                    format!(
                        "{}:{}:{}:{mode}{served_kinds}{wal_ceiling}",
                        escape_topology_component(name),
                        escape_topology_component(kind),
                        escape_topology_component(path),
                    )
                },
            )
            .collect::<Vec<_>>()
            .join(",");
        let pack_backends = pack_rows
            .iter()
            .map(|(pack, backend, no_embed)| {
                let no_embed = if *no_embed { ":no_embed" } else { "" };
                format!(
                    "{}={}{no_embed}",
                    escape_topology_component(pack),
                    escape_topology_component(backend),
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        (format!("v2|{backends}"), format!("v2|{pack_backends}"))
    };

    format!(";backends=[{backends}];pack_backends=[{pack_backends}]")
}

fn disabled_verb_policy_suffix(config: &khive_runtime::KhiveConfig) -> String {
    let disabled: std::collections::BTreeMap<_, _> = config
        .packs
        .iter()
        .filter_map(|(pack, policy)| {
            let verbs: std::collections::BTreeSet<_> = policy.verbs_disabled.iter().collect();
            (!verbs.is_empty()).then_some((pack, verbs))
        })
        .collect();
    if disabled.is_empty() {
        String::new()
    } else {
        format!(
            ";disabled_verbs={}",
            serde_json::to_string(&disabled).expect("string policy serializes")
        )
    }
}

/// Resolve any path headed into `config_id` fingerprinting — a declared
/// `[[backends]].path` or the resolved `RuntimeConfig.db_path` (itself
/// derived from `--db`/`KHIVE_DB`) — to a stable, cwd-independent string
/// without creating anything on disk.
///
/// Delegates to [`crate::serve::canonical_path_no_side_effects`] — the same
/// no-side-effects canonicalization the `--db` override equivalence check
/// uses — so a relative path resolves against the process's current working
/// directory the same way a real backend open would. Falls back to the raw
/// display string only on a canonicalization error (e.g. an unreadable
/// ancestor directory); this is strictly no worse than the pre-fix behavior,
/// which always used the raw string.
pub(super) fn canonical_fingerprint_path(path: &std::path::Path) -> String {
    crate::serve::canonical_path_no_side_effects(path)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

/// Build a sorted, human-readable verb catalog from `(pack_name, verb_name, description)` triples.
///
/// When multiple packs register the same verb name, each pack's description is
/// emitted on its own continuation line with a `[pack]` prefix so the caller can
/// see every contributing pack. A `tracing::warn!` is emitted once per duplicate.
pub(super) fn build_verb_catalog(
    verbs: impl IntoIterator<Item = (String, String, String)>,
) -> String {
    let mut by_verb: std::collections::BTreeMap<String, Vec<(String, String)>> =
        std::collections::BTreeMap::new();
    for (pack_name, verb_name, description) in verbs {
        by_verb
            .entry(verb_name)
            .or_default()
            .push((pack_name, description));
    }
    let mut out = String::new();
    for (name, pack_descs) in &by_verb {
        if pack_descs.len() > 1 {
            let packs: Vec<&str> = pack_descs.iter().map(|(p, _)| p.as_str()).collect();
            tracing::warn!(
                target: "khive_mcp::server",
                verb = %name,
                packs = ?packs,
                "verb registered by multiple packs; all descriptions included in catalog"
            );
        }
        out.push_str("  ");
        out.push_str(name);
        out.push_str(" — ");
        if pack_descs.len() == 1 {
            out.push_str(&pack_descs[0].1);
        } else {
            for (i, (pack, desc)) in pack_descs.iter().enumerate() {
                if i > 0 {
                    out.push_str("\n    ");
                }
                out.push('[');
                out.push_str(pack);
                out.push_str("] ");
                out.push_str(desc);
            }
        }
        out.push('\n');
    }
    out
}

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
