struct ConfigIdFields<'a> {
    packs: &'a str,
    db: &'a str,
    embed: &'a str,
    extra: &'a str,
    fresh_tail: &'a str,
    blob_hydration_bytes: &'a str,
    backend: &'a str,
    outbound: &'a str,
    gate: &'a str,
    git_write: &'a str,
    brain: &'a str,
    telemetry: &'a str,
    display_timezone: &'a str,
    visibility_receipts: &'a str,
    backends: Option<&'a str>,
    pack_backends: Option<&'a str>,
}

fn parse_config_id(config_id: &str) -> Option<ConfigIdFields<'_>> {
    let (base, backends, pack_backends) =
        if let Some((before_routing, routing)) = config_id.rsplit_once("];pack_backends=[") {
            let pack_backends = routing.strip_suffix(']')?;
            let (base, backends) = before_routing.rsplit_once(";backends=[")?;
            (base, Some(backends), Some(pack_backends))
        } else {
            (config_id, None, None)
        };

    let base = base.strip_prefix("packs=[")?;
    let (packs, rest) = base.split_once("];db=")?;
    let (rest, visibility_receipts) = rest
        .rsplit_once(";visibility_receipts=")
        .unwrap_or((rest, "<legacy-absent>"));
    let (rest, display_timezone) = rest
        .rsplit_once(";display_tz=")
        .unwrap_or((rest, "<legacy-absent>"));
    let (rest, telemetry) = rest
        .rsplit_once(";telemetry=")
        .unwrap_or((rest, "<legacy-absent>"));
    let (rest, brain) = rest
        .rsplit_once(";brain=")
        .unwrap_or((rest, "<legacy-absent>"));
    let (rest, git_write) = rest.rsplit_once(";git_write=")?;
    let (rest, gate) = rest
        .rsplit_once(";gate=")
        .unwrap_or((rest, "<legacy-absent>"));
    let (rest, outbound) = rest.rsplit_once(";outbound=[")?;
    let outbound = outbound.strip_suffix(']')?;
    let (rest, backend) = rest.rsplit_once(";backend=")?;
    let (rest, blob_hydration_bytes) = rest
        .rsplit_once(";blob_hydration_bytes=")
        .unwrap_or((rest, "<legacy-absent>"));
    let (rest, fresh_tail) = rest.rsplit_once(";fresh_tail=")?;
    let (rest, extra) = rest.rsplit_once(";extra=[")?;
    let extra = extra.strip_suffix(']')?;
    let (db, embed) = rest.rsplit_once(";embed=")?;

    Some(ConfigIdFields {
        packs,
        db,
        embed,
        extra,
        fresh_tail,
        blob_hydration_bytes,
        backend,
        outbound,
        gate,
        git_write,
        brain,
        telemetry,
        display_timezone,
        visibility_receipts,
        backends,
        pack_backends,
    })
}

fn extra_embedder_set(extra: &str) -> std::collections::BTreeSet<&str> {
    extra.split(',').filter(|name| !name.is_empty()).collect()
}

/// Return daemon-configured extra models that are absent from a compatible client configuration.
pub fn config_id_extra_embedder_exclusions(
    client_id: &str,
    daemon_id: &str,
) -> Option<Vec<String>> {
    if client_id == daemon_id {
        return Some(Vec::new());
    }
    let client = parse_config_id(client_id)?;
    let daemon = parse_config_id(daemon_id)?;
    let mut client_available = extra_embedder_set(client.extra);
    client_available.insert(client.embed);
    let daemon_extras = extra_embedder_set(daemon.extra);
    Some(
        daemon_extras
            .difference(&client_available)
            .map(|name| {
                serde_json::from_value::<lattice_embed::EmbeddingModel>(serde_json::Value::String(
                    (*name).to_string(),
                ))
                .map(|model| model.to_string())
                .unwrap_or_else(|_| (*name).to_string())
            })
            .collect(),
    )
}

/// Whether a daemon configuration can serve a client's requested runtime.
/// Every fingerprint field must match except that the daemon may have more
/// configured extra embedding models than the client requested.
pub fn config_ids_compatible(client_id: &str, daemon_id: &str) -> bool {
    if client_id == daemon_id {
        return true;
    }
    let (Some(client), Some(daemon)) = (parse_config_id(client_id), parse_config_id(daemon_id))
    else {
        return false;
    };

    let client_extras = extra_embedder_set(client.extra);
    let mut daemon_available = extra_embedder_set(daemon.extra);
    daemon_available.insert(daemon.embed);
    let daemon_has_requested_extras = client_extras.is_subset(&daemon_available);

    client.packs == daemon.packs
        && client.db == daemon.db
        && client.embed == daemon.embed
        && daemon_has_requested_extras
        && client.fresh_tail == daemon.fresh_tail
        && client.blob_hydration_bytes == daemon.blob_hydration_bytes
        && client.backend == daemon.backend
        && client.outbound == daemon.outbound
        && client.gate == daemon.gate
        && client.git_write == daemon.git_write
        && client.brain == daemon.brain
        && client.telemetry == daemon.telemetry
        && client.display_timezone == daemon.display_timezone
        && client.visibility_receipts == daemon.visibility_receipts
        && client.backends == daemon.backends
        && client.pack_backends == daemon.pack_backends
}

/// Name the first differing configuration component for diagnostics.
pub fn first_config_mismatch_field(client_id: &str, daemon_id: Option<&str>) -> &'static str {
    let Some(daemon_id) = daemon_id else {
        return "unknown";
    };
    let (Some(client), Some(daemon)) = (parse_config_id(client_id), parse_config_id(daemon_id))
    else {
        return "unknown";
    };
    let client_extras = extra_embedder_set(client.extra);
    let mut daemon_available = extra_embedder_set(daemon.extra);
    daemon_available.insert(daemon.embed);

    if client.packs != daemon.packs {
        "packs"
    } else if client.db != daemon.db {
        "db"
    } else if client.embed != daemon.embed {
        "embed"
    } else if !client_extras.is_subset(&daemon_available) {
        "extra"
    } else if client.fresh_tail != daemon.fresh_tail {
        "fresh_tail"
    } else if client.blob_hydration_bytes != daemon.blob_hydration_bytes {
        "blob_hydration_bytes"
    } else if client.backend != daemon.backend {
        "backend"
    } else if client.outbound != daemon.outbound {
        "outbound"
    } else if client.gate != daemon.gate {
        "gate"
    } else if client.git_write != daemon.git_write {
        "git_write"
    } else if client.brain != daemon.brain {
        "brain"
    } else if client.telemetry != daemon.telemetry {
        "telemetry"
    } else if client.display_timezone != daemon.display_timezone {
        "display_tz"
    } else if client.visibility_receipts != daemon.visibility_receipts {
        "visibility_receipts"
    } else if client.backends != daemon.backends {
        "backends"
    } else if client.pack_backends != daemon.pack_backends {
        "pack_backends"
    } else if client.extra != daemon.extra {
        // A pre-v8 daemon compared exact ids and could refuse a safe superset.
        "extra"
    } else {
        "unknown"
    }
}
