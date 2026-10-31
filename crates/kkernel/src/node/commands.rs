//! The four node setup commands: `init`, `assign`, `pin` and `status`.
//!
//! Every command reads and writes through the [`CommandEnv`] it is handed: the
//! selected configuration file, an injected key store, an injected
//! confirmation prompt and three byte streams. No command prints directly,
//! signals or restarts the daemon, or polls the service. Refusal text and
//! exit codes follow the design's refusal table; messages carry no `node: `
//! prefix because `run_cli` adds it.

use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use khive_channel_node::client::{validate_service_url, NodeClient};
use khive_channel_node::encoding::{CanonicalUuid, Epoch, HexBytes, NodeAddress, Realm};
use khive_channel_node::keys::{EnrolmentBundle, KeyFacility};
use khive_channel_node::pins::{ConfirmedPin, PinIdentity, PinSource, PinSourceError, PinState};
use khive_channel_node::response::{ChannelError, NodeError};
use khive_channel_node::source::{
    NodeClientBinding, OutboundSource, PersistedSubmission, SourceError,
};
use khive_channel_node::wire::{ContactResponse, RefusalCode};
use khive_runtime::engine_config::ConfigError;
use khive_runtime::node_config::{NodeChannelConfig, NodePinConfig};
use serde::Serialize;
use uuid::Uuid;

use super::config_edit::{
    apply_edits, lock_config, plan_edit, publish_config, read_config, save_public_file, NodeEdit,
};
use super::formats::{
    fingerprint_lines, parse_card, parse_enrolment, AssignedReceipt, CardInput, Enrolment,
    ExportReceipt, InitializedReceipt, KeychainCheck, PinnedReceipt, SetupState, ShareCard,
    StatusReport,
};
use super::{CommandEnv, InitArgs, NodeSetupError, StatusArgs};

/// Largest accepted input file or stdin payload.
const MAX_INPUT_BYTES: usize = 65_536;
/// Exact prompt shown on the controlling terminal before a pin is saved.
const PIN_PROMPT: &str =
    "Have you compared every group with this contact on another channel? Type yes to confirm: ";

fn fail(exit: u8, message: &str) -> NodeSetupError {
    NodeSetupError {
        exit,
        message: message.to_string(),
    }
}

fn fail_owned(exit: u8, message: String) -> NodeSetupError {
    NodeSetupError { exit, message }
}

fn config_err(error: ConfigError) -> NodeSetupError {
    match error {
        ConfigError::InvalidNodeConfig { reason } => NodeSetupError {
            exit: 2,
            message: format!("channel.node: {reason}"),
        },
        other => NodeSetupError {
            exit: 2,
            message: other.to_string(),
        },
    }
}

fn url_err() -> NodeSetupError {
    fail(
        2,
        "service_url must be an HTTPS origin without credentials, query or fragment",
    )
}

/// Map a service-side failure to the design's remote-refusal rows.
fn node_err(error: &NodeError) -> NodeSetupError {
    let NodeError::Channel { error, diagnostic } = error else {
        return fail(5, "service returned an invalid response");
    };
    let status = diagnostic.as_ref().map(|d| d.status);
    let refusal = diagnostic.as_ref().and_then(|d| d.refusal);
    if status == Some(401) || matches!(error, ChannelError::Auth(_)) {
        return fail(
            5,
            "device authentication was refused; check this Mac's clock and enrolment with the operator",
        );
    }
    if refusal == Some(RefusalCode::ContactNotActive) {
        return fail(
            5,
            "contact is not active; both owners must complete contact consent before pinning",
        );
    }
    if matches!(status, Some(429) | Some(503))
        || matches!(
            refusal,
            Some(RefusalCode::RateLimited | RefusalCode::CapacityExhausted)
        )
    {
        return fail(5, "service is temporarily unavailable; retry later");
    }
    if matches!(error, ChannelError::InvalidEnvelope(_)) {
        return fail(5, "service returned an invalid response");
    }
    if matches!(error, ChannelError::Transport(_)) && status.is_none() {
        return fail(5, "could not reach the configured service");
    }
    fail(5, "service refused the operation")
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn io_write(sink: &mut dyn Write, bytes: &[u8]) -> Result<(), NodeSetupError> {
    sink.write_all(bytes)
        .and_then(|()| sink.flush())
        .map_err(|_| {
            fail(
                6,
                "could not write public output; use node status to export it again",
            )
        })
}

fn diag(env: &mut CommandEnv<'_>, text: &str) {
    // Diagnostics are advisory; a closed stderr must not fail a completed command.
    let _ = env.diagnostics.write_all(text.as_bytes());
    let _ = env.diagnostics.flush();
}

/// Pretty JSON with two-space indent, declared field order, one trailing LF.
fn json_line<T: Serialize>(value: &T) -> Result<Vec<u8>, NodeSetupError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|_| {
        fail(
            6,
            "could not write public output; use node status to export it again",
        )
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|dir| dir.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn metadata_path(config: &Path) -> PathBuf {
    let mut name = config
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".node-assignment.json");
    config.with_file_name(name)
}

fn is_stdout(out: Option<&Path>) -> bool {
    out.is_none_or(|p| p == Path::new("-"))
}

/// Refuse an existing output path before any Keychain or config mutation.
fn preflight_out(out: Option<&Path>) -> Result<(), NodeSetupError> {
    if is_stdout(out) {
        return Ok(());
    }
    let path = out.expect("file output has a path");
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(fail(2, "the output file already exists"));
    }
    let parent = absolute(path);
    match parent.parent() {
        Some(dir) if dir.is_dir() => Ok(()),
        _ => Err(fail(
            6,
            "could not write public output; use node status to export it again",
        )),
    }
}

/// Write public bytes to stdout or create-only to a file; returns the absolute
/// file path when a file was written.
fn emit_public(
    env: &mut CommandEnv<'_>,
    out: Option<&Path>,
    bytes: &[u8],
) -> Result<Option<PathBuf>, NodeSetupError> {
    if is_stdout(out) {
        io_write(env.output, bytes)?;
        return Ok(None);
    }
    let path = absolute(out.expect("file output has a path"));
    save_public_file(&path, bytes)?;
    Ok(Some(path))
}

/// Read the whole input file, or stdin for `-`, within the 64 KiB bound.
fn read_bounded(path: &Path, stdin: &mut dyn Read) -> Result<Vec<u8>, NodeSetupError> {
    let mut buf = Vec::new();
    let limit = (MAX_INPUT_BYTES as u64) + 1;
    if path == Path::new("-") {
        stdin
            .take(limit)
            .read_to_end(&mut buf)
            .map_err(|_| fail(6, "could not read the selected file"))?;
    } else {
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(fail(2, "the selected input file does not exist"))
            }
            Err(_) => return Err(fail(6, "could not read the selected file")),
        };
        file.take(limit)
            .read_to_end(&mut buf)
            .map_err(|_| fail(6, "could not read the selected file"))?;
    }
    if buf.len() > MAX_INPUT_BYTES {
        return Err(fail(2, "input exceeds 65536 bytes"));
    }
    Ok(buf)
}

/// Escape control characters so a label cannot rewrite the terminal.
fn escape_controls(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Setup-only client sources. They can neither send nor bless a key.
// ---------------------------------------------------------------------------

/// Outbound source that never has a row: setup cannot send.
pub(crate) struct EmptyOutbound;

impl OutboundSource for EmptyOutbound {
    fn current<'life0, 'async_trait>(
        &'life0 self,
        _logical_id: Uuid,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<PersistedSubmission>, SourceError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async { Ok(None) })
    }
}

/// Pin source that never confirms: a lookup cannot count as consent.
pub(crate) struct UnconfirmedPins;

impl PinSource for UnconfirmedPins {
    fn resolve<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _identity: &'life1 PinIdentity,
    ) -> Pin<Box<dyn Future<Output = Result<PinState, PinSourceError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async { Ok(PinState::UnconfirmedEpoch { candidate: None }) })
    }
}

/// Bind a client from a complete enabled table.
pub(crate) fn binding_from(
    config: &NodeChannelConfig,
) -> Result<NodeClientBinding, NodeSetupError> {
    let missing = || fail(2, "assign this node before using this command");
    let realm = Realm::parse(config.realm.as_deref().ok_or_else(missing)?).map_err(|_| {
        fail(
            2,
            "configuration does not satisfy the khive configuration schema",
        )
    })?;
    let agent =
        CanonicalUuid::parse(config.agent_id.as_deref().ok_or_else(missing)?).map_err(|_| {
            fail(
                2,
                "configuration does not satisfy the khive configuration schema",
            )
        })?;
    let device =
        CanonicalUuid::parse(config.device_id.as_deref().ok_or_else(missing)?).map_err(|_| {
            fail(
                2,
                "configuration does not satisfy the khive configuration schema",
            )
        })?;
    let key_epoch = Epoch::new(config.key_epoch.ok_or_else(missing)?).map_err(|_| {
        fail(
            2,
            "configuration does not satisfy the khive configuration schema",
        )
    })?;
    Ok(NodeClientBinding {
        namespace: config.namespace.clone(),
        slug: config.slug.clone().ok_or_else(missing)?,
        realm,
        agent,
        device,
        key_epoch,
        key_reference: config.key_reference.clone().ok_or_else(missing)?,
    })
}

/// Build the setup-only client for an assigned node. It carries an empty
/// outbound source and a pin source that never confirms.
pub(crate) fn setup_client(
    config: &NodeChannelConfig,
    facility: Arc<dyn KeyFacility>,
) -> Result<NodeClient, NodeSetupError> {
    let binding = binding_from(config)?;
    let service_url = config.service_url.as_deref().ok_or_else(url_err)?;
    NodeClient::new(
        binding,
        service_url,
        Arc::new(EmptyOutbound),
        Arc::new(UnconfirmedPins),
        facility,
    )
    .map_err(|_| url_err())
}

/// The seven shared card facts, whichever schema supplied them.
struct CardFacts<'a> {
    address: &'a NodeAddress,
    agent: CanonicalUuid,
    realm: &'a Realm,
    label: &'a str,
    epoch: Epoch,
    fingerprint: &'a HexBytes<32>,
    device: Option<CanonicalUuid>,
}

fn card_facts(card: &CardInput) -> CardFacts<'_> {
    match card {
        CardInput::Card(c) => CardFacts {
            address: &c.address,
            agent: c.agent_id,
            realm: &c.realm,
            label: &c.label,
            epoch: c.key_epoch,
            fingerprint: &c.fingerprint,
            device: None,
        },
        CardInput::Enrolment(e) => CardFacts {
            address: &e.address,
            agent: e.agent_id,
            realm: &e.realm,
            label: &e.label,
            epoch: e.key_epoch,
            fingerprint: &e.fingerprint,
            device: Some(e.device_id),
        },
    }
}

/// Fetch the contact's public material with one signed lookup and require it
/// to match the supplied card. A false `matches_owner_pin` is expected on first
/// confirmation and is neither an error nor consent.
pub(crate) async fn lookup_contact(
    client: &NodeClient,
    card: &CardInput,
) -> Result<ContactResponse, NodeSetupError> {
    let facts = card_facts(card);
    let result = client
        .contact(facts.agent.into_uuid())
        .await
        .map_err(|e| node_err(&e))?;
    let observed = result.observed;
    observed
        .validated_keys(&client.binding().realm)
        .map_err(|_| fail(2, "contact public keys or fingerprint are invalid"))?;
    let device_differs = facts.device.is_some_and(|d| d != observed.device_id);
    if observed.agent_id != facts.agent
        || &observed.address != facts.address
        || observed.key_epoch != facts.epoch
        || &observed.fingerprint != facts.fingerprint
        || device_differs
    {
        return Err(fail(
            2,
            "contact keys differ from the card; obtain a current card and compare again",
        ));
    }
    Ok(observed)
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

fn is_prepared(node: &NodeChannelConfig) -> bool {
    !node.enabled
        && node.key_reference.is_some()
        && node.key_fingerprint.is_none()
        && node.agent_id.is_none()
        && node.device_id.is_none()
        && node.pins.is_empty()
}

fn parse_realm(text: &str) -> Result<Realm, NodeSetupError> {
    Realm::parse(text).map_err(|_| {
        fail(
            2,
            "channel.node: realm must contain 1..=64 lowercase letters, digits, '.', '_' or '-'",
        )
    })
}

/// Verify a bundle and return the fingerprint of its public keys.
fn bundle_fingerprint(bundle: &EnrolmentBundle) -> Result<HexBytes<32>, NodeSetupError> {
    let keys = bundle
        .verify()
        .map_err(|_| fail(4, "stored key material is invalid"))?;
    Ok(keys.fingerprint())
}

/// Write the bundle to stdout or a file and show the fingerprint on stderr.
fn emit_bundle(
    env: &mut CommandEnv<'_>,
    out: Option<&Path>,
    bundle: &EnrolmentBundle,
    fingerprint: &HexBytes<32>,
) -> Result<(), NodeSetupError> {
    let bytes = json_line(bundle)?;
    let written = emit_public(env, out, &bytes)?;
    let instruction = if written.is_some() {
        "Send the bundle file to the service operator. Compare the fingerprint on another channel."
    } else {
        "Send the JSON object to the service operator. Compare the fingerprint on another channel."
    };
    diag(
        env,
        &format!("{}\n{instruction}\n", fingerprint_lines(fingerprint)),
    );
    if let Some(file) = written {
        let receipt = InitializedReceipt {
            state: "initialized",
            enabled: false,
            config: path_text(env.config_path),
            bundle_file: path_text(&file),
            fingerprint: fingerprint.clone(),
        };
        io_write(env.output, &json_line(&receipt)?)?;
    }
    Ok(())
}

/// `kkernel node init`: prepare a disabled table, add one Keychain item, record
/// its fingerprint. Add-only; never replaces or deletes an item. With
/// `--recover` it only re-reads the public bundle of an incomplete init.
pub(crate) async fn init(args: &InitArgs, env: &mut CommandEnv<'_>) -> Result<(), NodeSetupError> {
    let out = Some(args.out.as_path());
    if args.recover {
        return init_recover(args, out, env);
    }
    let (Some(service_url), Some(realm_text), Some(actor)) =
        (&args.service_url, &args.realm, &args.actor)
    else {
        return Err(fail(
            2,
            "channel.node: service_url is required when enabled",
        ));
    };
    let realm = parse_realm(realm_text)?;
    preflight_out(out)?;

    let snapshot = read_config(env.config_path, true)?;
    let existing = snapshot.config.channel.node.clone();
    let mut reference = format!("node-{}", Uuid::new_v4());
    let mut resume = false;
    if let Some(node) = &existing {
        if *node == NodeChannelConfig::default() {
            // An empty table is the same as no table.
        } else if is_prepared(node) {
            let same = node.service_url.as_deref() == Some(service_url.as_str())
                && node.realm.as_deref() == Some(realm_text.as_str())
                && node.actor.as_deref() == Some(actor.as_str())
                && node.slug.as_deref() == Some(args.slug.as_str())
                && node.namespace == args.namespace;
            if !same {
                return Err(fail(
                    2,
                    "initialization settings differ from the saved preparation",
                ));
            }
            reference = node.key_reference.clone().unwrap_or(reference);
            resume = true;
        } else if !node.enabled
            && node.key_fingerprint.is_some()
            && node.agent_id.is_none()
            && node.device_id.is_none()
            && node.pins.is_empty()
        {
            return Err(fail(
                2,
                "node keys are already initialized; use node status --bundle",
            ));
        } else {
            return Err(fail(
                2,
                "node configuration already contains a binding; init cannot replace it",
            ));
        }
    }

    let prepared = NodeChannelConfig {
        enabled: false,
        slug: Some(args.slug.clone()),
        namespace: args.namespace.clone(),
        actor: Some(actor.clone()),
        realm: Some(realm_text.clone()),
        service_url: Some(service_url.clone()),
        key_epoch: Some(1),
        key_reference: Some(reference.clone()),
        ..NodeChannelConfig::default()
    };
    // Enabled projection with placeholder identity values; never persisted.
    let mut probe = prepared.clone();
    probe.enabled = true;
    probe.agent_id = Some("00000000-0000-4000-8000-000000000001".to_string());
    probe.device_id = Some("00000000-0000-4000-8000-000000000002".to_string());
    probe.key_fingerprint = Some("0".repeat(64));
    probe.validate().map_err(config_err)?;
    validate_service_url(service_url).map_err(|_| url_err())?;

    let first_bytes = if resume {
        None
    } else {
        let edits = plan_edit(&snapshot, &NodeEdit::Prepare(prepared))?;
        Some(apply_edits(&snapshot.bytes, &edits)?)
    };
    let mut guard = lock_config(snapshot)?;
    if let Some(bytes) = &first_bytes {
        publish_config(&mut guard, bytes)?;
    }

    let bundle = env.keys.create(&reference, realm)?;
    let fingerprint = bundle_fingerprint(&bundle)?;

    let after = read_config(env.config_path, false)?;
    let edits = plan_edit(
        &after,
        &NodeEdit::RecoverFingerprint(hex(fingerprint.as_bytes())),
    )?;
    let bytes = apply_edits(&after.bytes, &edits)?;
    publish_config(&mut guard, &bytes)?;
    drop(guard);

    emit_bundle(env, out, &bundle, &fingerprint)
}

fn init_recover(
    args: &InitArgs,
    out: Option<&Path>,
    env: &mut CommandEnv<'_>,
) -> Result<(), NodeSetupError> {
    let refused = || fail(2, "recovery requires an incomplete disabled initialization");
    if args.service_url.is_some() || args.realm.is_some() || args.actor.is_some() {
        return Err(refused());
    }
    preflight_out(out)?;
    let snapshot = read_config(env.config_path, true)?;
    let node = snapshot.config.channel.node.clone().ok_or_else(refused)?;
    if !is_prepared(&node) {
        return Err(refused());
    }
    let reference = node.key_reference.clone().ok_or_else(refused)?;
    let realm = parse_realm(node.realm.as_deref().ok_or_else(refused)?)?;

    let mut guard = lock_config(snapshot)?;
    let bundle = env.keys.recover(&reference, realm)?;
    let fingerprint = bundle_fingerprint(&bundle)?;

    let after = read_config(env.config_path, false)?;
    let edits = plan_edit(
        &after,
        &NodeEdit::RecoverFingerprint(hex(fingerprint.as_bytes())),
    )?;
    let bytes = apply_edits(&after.bytes, &edits)?;
    publish_config(&mut guard, &bytes)?;
    drop(guard);

    emit_bundle(env, out, &bundle, &fingerprint)
}

// ---------------------------------------------------------------------------
// assign
// ---------------------------------------------------------------------------

/// Parse the locally recorded fingerprint into raw bytes.
fn local_fingerprint(node: &NodeChannelConfig) -> Result<[u8; 32], NodeSetupError> {
    let text = node
        .key_fingerprint
        .as_deref()
        .ok_or_else(|| fail(2, "initialize this node before assigning an enrolment"))?;
    HexBytes::<32>::parse(text)
        .map(|h| *h.as_bytes())
        .map_err(|_| {
            fail(
                2,
                "configuration does not satisfy the khive configuration schema",
            )
        })
}

/// Validate an existing pin entry through the protocol types.
fn confirmed_from_config(
    index: usize,
    realm: &Realm,
    pin: &NodePinConfig,
) -> Result<ConfirmedPin, NodeSetupError> {
    let bad = || {
        fail_owned(
            2,
            format!("channel.node: pins[{index}] contains invalid or inconsistent public keys"),
        )
    };
    use khive_channel_node::keys::{DevicePublicKeys, KemPublicKey, SigningPublicKey};
    let kem = HexBytes::<32>::parse(&pin.kem_public_key).map_err(|_| bad())?;
    let signing = HexBytes::<32>::parse(&pin.signing_public_key).map_err(|_| bad())?;
    let fingerprint = HexBytes::<32>::parse(&pin.fingerprint).map_err(|_| bad())?;
    let keys = DevicePublicKeys {
        kem: KemPublicKey::new(*kem.as_bytes()).map_err(|_| bad())?,
        signing: SigningPublicKey::new(*signing.as_bytes()).map_err(|_| bad())?,
    };
    let identity = PinIdentity {
        realm: realm.clone(),
        agent: CanonicalUuid::parse(&pin.agent_id).map_err(|_| bad())?,
        device: CanonicalUuid::parse(&pin.device_id).map_err(|_| bad())?,
        epoch: Epoch::new(pin.key_epoch).map_err(|_| bad())?,
    };
    ConfirmedPin::new(identity, keys, fingerprint).map_err(|_| bad())
}

/// `kkernel node assign`: bind the initialized local key to the service's
/// Enrolment and enable the table. Restart is required; nothing is signalled.
pub(crate) async fn assign(input: &Path, env: &mut CommandEnv<'_>) -> Result<(), NodeSetupError> {
    let bytes = read_bounded(input, env.input)?;
    let enrolment = parse_enrolment(&bytes)?;
    let snapshot = read_config(env.config_path, false)?;
    let node = snapshot
        .config
        .channel
        .node
        .clone()
        .ok_or_else(|| fail(2, "initialize this node before assigning an enrolment"))?;
    let init_first = || fail(2, "initialize this node before assigning an enrolment");
    let reference = node.key_reference.clone().ok_or_else(init_first)?;
    let node_realm = node.realm.clone().ok_or_else(init_first)?;
    let local_fp = local_fingerprint(&node)?;

    if enrolment.address.realm() != &enrolment.realm
        || enrolment.address.agent() != enrolment.agent_id
    {
        return Err(fail(
            2,
            "address does not match the supplied realm and agent",
        ));
    }
    if enrolment.realm.as_str() != node_realm {
        return Err(fail(2, "enrolment realm does not match this node"));
    }
    if enrolment.key_epoch.get() != 1 {
        return Err(fail(2, "initial enrolment must use key epoch 1"));
    }
    if enrolment.fingerprint.as_bytes() != &local_fp {
        return Err(fail(
            2,
            "enrolment fingerprint does not match this Mac's keys",
        ));
    }
    let facility = env.keys.open(&reference, &local_fp)?;
    if facility.public_keys().fingerprint().as_bytes() != &local_fp {
        return Err(fail(
            2,
            "enrolment fingerprint does not match this Mac's keys",
        ));
    }

    let agent = enrolment.agent_id.to_string();
    let device = enrolment.device_id.to_string();
    let fp_hex = hex(&local_fp);
    let identical = node.enabled
        && node.agent_id.as_deref() == Some(agent.as_str())
        && node.device_id.as_deref() == Some(device.as_str())
        && node.key_epoch == Some(1)
        && node.key_fingerprint.as_deref() == Some(fp_hex.as_str());
    if node.enabled && !identical {
        return Err(fail(
            2,
            "this node is already assigned; assignment cannot replace its identity",
        ));
    }
    if !node.enabled && (node.agent_id.is_some() || node.device_id.is_some()) {
        return Err(fail(2, "assignment cannot re-enable an existing identity"));
    }

    let mut candidate = node.clone();
    candidate.enabled = true;
    candidate.agent_id = Some(agent);
    candidate.device_id = Some(device);
    candidate.key_epoch = Some(1);
    candidate.key_fingerprint = Some(fp_hex);
    candidate.validate().map_err(config_err)?;
    for (index, pin) in candidate.pins.iter().enumerate() {
        confirmed_from_config(index, &enrolment.realm, pin)?;
    }

    let planned = if identical {
        None
    } else {
        let edits = plan_edit(&snapshot, &NodeEdit::Assign(candidate))?;
        Some(apply_edits(&snapshot.bytes, &edits)?)
    };
    let mut guard = lock_config(snapshot)?;

    // Public metadata first, create-only; an identical retry is accepted.
    let meta_path = metadata_path(env.config_path);
    let payload = json_line(&enrolment)?;
    match std::fs::read(&meta_path) {
        Ok(existing) => {
            if existing != payload {
                return Err(fail(2, "saved enrolment metadata does not match this node"));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            save_public_file(&meta_path, &payload)?;
        }
        Err(_) => return Err(fail(6, "could not read the selected file")),
    }

    if let Some(bytes) = &planned {
        publish_config(&mut guard, bytes)?;
    }
    drop(guard);

    let receipt = AssignedReceipt {
        state: "assigned",
        changed: planned.is_some(),
        enabled: true,
        config: path_text(env.config_path),
        address: enrolment.address.clone(),
        restart_required: true,
    };
    io_write(env.output, &json_line(&receipt)?)?;
    diag(
        env,
        "Configuration saved. Restart the daemon before using this node.\n",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// pin
// ---------------------------------------------------------------------------

enum PinDecision {
    Unchanged,
    Append,
}

/// Decide how a candidate pin relates to the retained entries. Pure; it is
/// evaluated before the prompt and again against the re-read config.
fn classify_pin(
    node: &NodeChannelConfig,
    candidate: &NodePinConfig,
) -> Result<PinDecision, NodeSetupError> {
    let mut unchanged = false;
    for existing in &node.pins {
        if existing.agent_id != candidate.agent_id || existing.device_id != candidate.device_id {
            continue;
        }
        if existing.key_epoch == candidate.key_epoch {
            if existing == candidate {
                unchanged = true;
            } else {
                return Err(fail(
                    2,
                    "node pin identity already has different confirmed keys",
                ));
            }
        } else if existing.key_epoch > candidate.key_epoch {
            return Err(fail(
                2,
                "contact key epoch is older than a confirmed epoch for this device",
            ));
        }
    }
    Ok(if unchanged {
        PinDecision::Unchanged
    } else {
        PinDecision::Append
    })
}

/// `kkernel node pin`: fetch the contact's public keys with one signed lookup,
/// show the complete fingerprint, require the exact answer `yes`, fetch again,
/// then append one pin to the configuration. Restart is required.
pub(crate) async fn pin(card: &Path, env: &mut CommandEnv<'_>) -> Result<(), NodeSetupError> {
    let bytes = read_bounded(card, env.input)?;
    let card = parse_card(&bytes)?;
    let snapshot = read_config(env.config_path, false)?;
    let node = snapshot
        .config
        .channel
        .node
        .clone()
        .filter(|n| n.enabled)
        .ok_or_else(|| fail(2, "assign this node before using this command"))?;
    let binding = binding_from(&node)?;
    let local_fp = local_fingerprint(&node)?;
    let facts = card_facts(&card);
    if facts.realm != &binding.realm {
        return Err(fail(2, "contact card belongs to another realm"));
    }
    if facts.agent == binding.agent {
        return Err(fail(2, "a node cannot pin itself as a contact"));
    }

    let facility = env.keys.open(&binding.key_reference, &local_fp)?;
    let client = env.service.client(&node, facility)?;
    let observed = lookup_contact(&client, &card).await?;
    let keys = observed
        .validated_keys(&binding.realm)
        .map_err(|_| fail(2, "contact public keys or fingerprint are invalid"))?;
    let identity = PinIdentity {
        realm: binding.realm.clone(),
        agent: observed.agent_id,
        device: observed.device_id,
        epoch: observed.key_epoch,
    };
    ConfirmedPin::new(identity, keys, observed.fingerprint.clone())
        .map_err(|_| fail(2, "contact public keys or fingerprint are invalid"))?;

    let new_pin = NodePinConfig {
        agent_id: observed.agent_id.to_string(),
        device_id: observed.device_id.to_string(),
        key_epoch: observed.key_epoch.get(),
        kem_public_key: hex(observed.kem_public_key.as_bytes()),
        signing_public_key: hex(observed.signing_public_key.as_bytes()),
        fingerprint: hex(observed.fingerprint.as_bytes()),
    };
    let first = classify_pin(&node, &new_pin)?;
    let receipt = |changed: bool| PinnedReceipt {
        state: "pinned",
        changed,
        address: observed.address.clone(),
        device_id: observed.device_id,
        key_epoch: observed.key_epoch,
        fingerprint: observed.fingerprint.clone(),
        restart_required: true,
    };
    if matches!(first, PinDecision::Unchanged) {
        io_write(env.output, &json_line(&receipt(false))?)?;
        diag(
            env,
            "Contact key confirmed in configuration. Restart the daemon to use it.\n",
        );
        return Ok(());
    }

    let original = snapshot.bytes.clone();
    let guard_snapshot = snapshot;
    let mut guard = lock_config(guard_snapshot)?;

    let shown = format!(
        "Contact: {}\nLabel: {}\nRealm: {}\nDevice: {}\nKey epoch: {}\n{}\n",
        observed.address,
        escape_controls(facts.label),
        binding.realm.as_str(),
        observed.device_id,
        observed.key_epoch.get(),
        fingerprint_lines(&observed.fingerprint),
    );
    diag(env, &shown);
    if !env.confirmation.confirm(PIN_PROMPT)? {
        return Err(fail(3, "confirmation cancelled; nothing changed"));
    }

    let again = lookup_contact(&client, &card).await?;
    if again != observed {
        return Err(fail(
            2,
            "contact changed during confirmation; run node pin again",
        ));
    }

    let after = read_config(env.config_path, false)?;
    if after.bytes != original {
        return Err(fail(
            6,
            "configuration changed during setup; run the command again",
        ));
    }
    let after_node = after
        .config
        .channel
        .node
        .clone()
        .ok_or_else(|| fail(2, "assign this node before using this command"))?;
    let decision = classify_pin(&after_node, &new_pin)?;
    let changed = matches!(decision, PinDecision::Append);
    if changed {
        let edits = plan_edit(&after, &NodeEdit::AppendPin(new_pin.clone()))?;
        let candidate = apply_edits(&after.bytes, &edits)?;
        publish_config(&mut guard, &candidate)?;
    }
    drop(guard);

    io_write(env.output, &json_line(&receipt(changed))?)?;
    diag(
        env,
        "Contact key confirmed in configuration. Restart the daemon to use it.\n",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

fn classify_state(node: Option<&NodeChannelConfig>) -> Result<SetupState, NodeSetupError> {
    let Some(node) = node else {
        return Ok(SetupState::Absent);
    };
    if *node == NodeChannelConfig::default() {
        return Ok(SetupState::Absent);
    }
    let identity = node.agent_id.is_some() && node.device_id.is_some();
    let has_fp = node.key_fingerprint.is_some();
    let state = if is_prepared(node) {
        Some(SetupState::Prepared)
    } else if !node.enabled
        && has_fp
        && !identity
        && node.agent_id.is_none()
        && node.device_id.is_none()
    {
        Some(SetupState::Initialized)
    } else if node.enabled && identity && has_fp && node.key_reference.is_some() {
        Some(SetupState::Assigned)
    } else if !node.enabled && identity && has_fp && node.key_reference.is_some() {
        Some(SetupState::Disabled)
    } else {
        None
    };
    state.ok_or_else(|| fail(2, "node configuration is in an inconsistent state"))
}

/// `kkernel node status`: local state only. No network or database access.
/// With `--bundle` or `--card` it exports the matching public file.
pub(crate) async fn status(
    args: &StatusArgs,
    env: &mut CommandEnv<'_>,
) -> Result<(), NodeSetupError> {
    if args.out.is_some() && !args.bundle && !args.card {
        return Err(fail(2, "output file requires --bundle or --card"));
    }
    if args.bundle && args.card {
        return Err(fail(2, "output modes are mutually exclusive"));
    }
    let out = args.out.as_deref();
    preflight_out(out)?;
    let snapshot = read_config(env.config_path, true)?;
    let node = snapshot.config.channel.node.clone();
    let state = classify_state(node.as_ref())?;
    let namespace = node
        .as_ref()
        .map_or_else(|| "local".to_string(), |n| n.namespace.clone());

    // Open the Keychain item only when a fingerprint exists.
    let mut facility: Option<Arc<dyn KeyFacility>> = None;
    if let Some(n) = node.as_ref() {
        if let (Some(reference), Some(_)) = (&n.key_reference, &n.key_fingerprint) {
            let fp = local_fingerprint(n)?;
            let opened = env.keys.open(reference, &fp)?;
            if opened.public_keys().fingerprint().as_bytes() != &fp {
                return Err(fail(4, "stored keys do not match the expected fingerprint"));
            }
            facility = Some(opened);
        }
    }

    // Assignment metadata, when present, must agree with the binding.
    let mut metadata: Option<Enrolment> = None;
    if matches!(state, SetupState::Assigned | SetupState::Disabled) {
        match std::fs::read(metadata_path(env.config_path)) {
            Ok(raw) => {
                let parsed = parse_enrolment(&raw)?;
                let n = node.as_ref().expect("assigned state has a node table");
                let agrees = n.agent_id.as_deref() == Some(parsed.agent_id.to_string().as_str())
                    && n.device_id.as_deref() == Some(parsed.device_id.to_string().as_str())
                    && n.realm.as_deref() == Some(parsed.realm.as_str())
                    && n.key_epoch == Some(parsed.key_epoch.get())
                    && n.key_fingerprint.as_deref()
                        == Some(hex(parsed.fingerprint.as_bytes()).as_str());
                if !agrees {
                    return Err(fail(2, "saved enrolment metadata does not match this node"));
                }
                metadata = Some(parsed);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(fail(6, "could not read the selected file")),
        }
    }

    if args.bundle {
        let (Some(n), Some(facility)) = (node.as_ref(), facility.as_ref()) else {
            return Err(fail(
                2,
                "initialization is incomplete; use node init --recover",
            ));
        };
        let realm = parse_realm(n.realm.as_deref().unwrap_or_default())?;
        let bundle = facility
            .enrolment_bundle(realm)
            .map_err(|_| fail(4, "stored key material is invalid"))?;
        return export_public(env, out, "bundle", &json_line(&bundle)?);
    }
    if args.card {
        if !matches!(state, SetupState::Assigned | SetupState::Disabled) {
            return Err(fail(2, "assign this node before using this command"));
        }
        let meta = metadata.ok_or_else(|| {
            fail(
                2,
                "share-card metadata is missing; repeat the original assignment",
            )
        })?;
        let card = ShareCard {
            protocol_version: meta.protocol_version,
            address: meta.address,
            agent_id: meta.agent_id,
            realm: meta.realm,
            label: meta.label,
            key_epoch: meta.key_epoch,
            fingerprint: meta.fingerprint,
        };
        return export_public(env, out, "card", &json_line(&card)?);
    }

    let n = node.as_ref();
    let schema = || {
        fail(
            2,
            "configuration does not satisfy the khive configuration schema",
        )
    };
    let fingerprint = n
        .and_then(|n| n.key_fingerprint.as_deref())
        .map(|text| HexBytes::<32>::parse(text).map_err(|_| schema()))
        .transpose()?;
    let device_id = n
        .and_then(|n| n.device_id.as_deref())
        .map(|text| CanonicalUuid::parse(text).map_err(|_| schema()))
        .transpose()?;
    let key_epoch = n
        .and_then(|n| n.key_epoch)
        .map(|value| Epoch::new(value).map_err(|_| schema()))
        .transpose()?;
    let address = match n {
        Some(n) if matches!(state, SetupState::Assigned | SetupState::Disabled) => {
            match (n.realm.as_deref(), n.agent_id.as_deref()) {
                (Some(r), Some(a)) => match (Realm::parse(r), CanonicalUuid::parse(a)) {
                    (Ok(realm), Ok(agent)) => Some(NodeAddress::new(realm, agent)),
                    _ => None,
                },
                _ => None,
            }
        }
        _ => None,
    };
    let report = StatusReport {
        state,
        config: path_text(env.config_path),
        enabled: n.is_some_and(|n| n.enabled),
        service_url: n.and_then(|n| n.service_url.clone()),
        realm: n.and_then(|n| n.realm.clone()),
        actor: n.and_then(|n| n.actor.clone()),
        namespace,
        slug: n.and_then(|n| n.slug.clone()),
        address,
        device_id,
        key_epoch,
        fingerprint,
        pin_count: n.map_or(0, |n| n.pins.len()),
        config_digest: n.filter(|n| n.enabled).map(|n| n.config_digest()),
        keychain: if facility.is_some() {
            KeychainCheck::Matches
        } else {
            KeychainCheck::NotChecked
        },
    };
    io_write(env.output, &json_line(&report)?)?;
    if let Some(fp) = &report.fingerprint {
        diag(env, &format!("{}\n", fingerprint_lines(fp)));
    }
    Ok(())
}

/// Write a bundle or card to stdout, or to a create-only file with a receipt.
fn export_public(
    env: &mut CommandEnv<'_>,
    out: Option<&Path>,
    kind: &'static str,
    bytes: &[u8],
) -> Result<(), NodeSetupError> {
    if let Some(file) = emit_public(env, out, bytes)? {
        let receipt = ExportReceipt {
            state: "exported",
            kind,
            file: path_text(&file),
        };
        io_write(env.output, &json_line(&receipt)?)?;
    }
    Ok(())
}
