pub(super) struct ConfigIdFields<'a> {
    packs: &'a str,
    db: &'a str,
    embed: &'a str,
    extra: &'a str,
    pub(super) engine_peers: Option<&'a str>,
    fresh_tail: &'a str,
    blob_hydration_bytes: &'a str,
    backend: &'a str,
    outbound: &'a str,
    gate: &'a str,
    git_write: &'a str,
    brain: &'a str,
    telemetry: &'a str,
    pub(super) display_timezone: &'a str,
    visibility_receipts: &'a str,
    /// Digest of an enabled `[channel.node]` table; absent from ids that carry none.
    pub(super) node_channel: Option<&'a str>,
    backends: Option<&'a str>,
    pub(super) pack_backends: Option<&'a str>,
    /// Trailing writable-SQLite disk policy; absent from ids that carry none.
    pub(super) sqlite_disk_guard: Option<&'a str>,
}

pub(super) fn parse_config_id(config_id: &str) -> Option<ConfigIdFields<'_>> {
    let (config_id, sqlite_disk_guard) = match config_id.rsplit_once(";sqlite_disk_guard=") {
        Some((before, policy)) => (before, Some(policy)),
        None => (config_id, None),
    };
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
    // The digest is 64 lowercase hex characters. Trailing policy segments that
    // this parser does not split out (`blob_file_transfers`, `disabled_verbs`)
    // may follow it; they stay in the compared value so a difference there
    // still refuses reuse.
    let (rest, node_channel) = match rest.rsplit_once(";node_channel=") {
        Some((rest, value)) => {
            let digest = value.get(..64)?;
            let tail = &value[64..];
            if !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                || !(tail.is_empty() || tail.starts_with(';'))
            {
                return None;
            }
            (rest, Some(value))
        }
        None => (rest, None),
    };
    let (rest, visibility_receipts) = rest
        .rsplit_once(";visibility_receipts=")
        .unwrap_or((rest, LEGACY_ABSENT));
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
    let (rest, engine_peers) = match rest.rsplit_once(";engine_peers=") {
        Some((rest, peers)) => {
            if peers.len() != 64 || !peers.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return None;
            }
            (rest, Some(peers))
        }
        None => (rest, None),
    };
    let (rest, extra) = rest.rsplit_once(";extra=[")?;
    let extra = extra.strip_suffix(']')?;
    let (db, embed) = rest.rsplit_once(";embed=")?;

    Some(ConfigIdFields {
        packs,
        db,
        embed,
        extra,
        engine_peers,
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
        node_channel,
        backends,
        pack_backends,
        sqlite_disk_guard,
    })
}

/// Value the parser assigns to a field that an older configuration id does not carry.
const LEGACY_ABSENT: &str = "<legacy-absent>";

/// Domain-separation tag hashed ahead of the receipt declaration.
const VISIBILITY_RECEIPT_POLICY_TAG: &[u8] = b"khive.visibility-receipt-policy.v2";

/// Fingerprint of a receipt declaration, as carried in the `visibility_receipts`
/// field of a configuration id.
///
/// `declaration_json` is the JSON encoding of the receipt credential
/// declarations followed by the optional key ring. Id minting and the
/// compatibility check both go through this one function, so the value for a
/// configuration without a ring cannot drift between them.
pub fn visibility_receipt_policy_fingerprint(declaration_json: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(VISIBILITY_RECEIPT_POLICY_TAG);
    hasher.update(declaration_json);
    format!("{:x}", hasher.finalize())
}

/// The `visibility_receipts` fingerprint of a configuration with no
/// `[visibility_receipts]` section: no receipt credentials and no key ring.
pub fn no_receipt_ring_fingerprint() -> &'static str {
    static NO_RING: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NO_RING.get_or_init(|| {
        let declaration = serde_json::to_vec(&(Vec::<()>::new(), None::<()>))
            .expect("empty receipt declaration is JSON serializable");
        visibility_receipt_policy_fingerprint(&declaration)
    })
}

/// Split a parsed `visibility_receipts` value into its fingerprint and the
/// trailing policy segments (`;blob_file_transfers=...`, `;disabled_verbs=...`)
/// that the parser leaves attached to it.
fn split_receipt_fingerprint(value: &str) -> (&str, &str) {
    value.split_at(value.find(';').unwrap_or(value.len()))
}

/// Compare two parsed `visibility_receipts` values. They must be equal, except
/// that a client with no receipt ring is compatible with a daemon carrying any
/// receipt policy other than the legacy absent marker. Trailing policy segments
/// must always be equal.
fn visibility_receipts_compatible(client: &str, daemon: &str) -> bool {
    if client == daemon {
        return true;
    }
    let (client_policy, client_tail) = split_receipt_fingerprint(client);
    let (daemon_policy, daemon_tail) = split_receipt_fingerprint(daemon);
    client_tail == daemon_tail
        && client_policy == no_receipt_ring_fingerprint()
        && daemon_policy != LEGACY_ABSENT
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
    if client.engine_peers.is_some() || daemon.engine_peers.is_some() {
        return config_ids_compatible(client_id, daemon_id).then(Vec::new);
    }
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
/// Every peer fingerprint must match exactly. Only legacy ids without peer
/// fingerprints allow a daemon to hold more extra models than the client.
///
/// One field is compared asymmetrically. A client whose `visibility_receipts`
/// is the value of a configuration with no receipt ring is compatible with a
/// daemon carrying any receipt policy of the current version, because only the
/// daemon seals and opens receipts and a forwarding client never holds or uses
/// the ring. Every other combination keeps exact equality: a client that
/// declares a ring must declare the daemon's ring, a daemon without a ring does
/// not serve a client that declares one, and a client without a ring does not
/// match a daemon id that predates the field.
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
    let daemon_has_requested_extras =
        if client.engine_peers.is_some() || daemon.engine_peers.is_some() {
            client.extra == daemon.extra
        } else {
            client_extras.is_subset(&daemon_available)
        };

    client.packs == daemon.packs
        && client.db == daemon.db
        && client.engine_peers == daemon.engine_peers
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
        && visibility_receipts_compatible(client.visibility_receipts, daemon.visibility_receipts)
        && client.node_channel == daemon.node_channel
        && client.backends == daemon.backends
        && client.pack_backends == daemon.pack_backends
        && client.sqlite_disk_guard == daemon.sqlite_disk_guard
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
    } else if client.engine_peers != daemon.engine_peers {
        "engine_peers"
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
    } else if !visibility_receipts_compatible(
        client.visibility_receipts,
        daemon.visibility_receipts,
    ) {
        "visibility_receipts"
    } else if client.node_channel != daemon.node_channel {
        "node_channel"
    } else if client.backends != daemon.backends {
        "backends"
    } else if client.pack_backends != daemon.pack_backends {
        "pack_backends"
    } else if client.sqlite_disk_guard != daemon.sqlite_disk_guard {
        "sqlite_disk_guard"
    } else if client.extra != daemon.extra {
        // A pre-v8 daemon compared exact ids and could refuse a safe superset.
        "extra"
    } else {
        "unknown"
    }
}
