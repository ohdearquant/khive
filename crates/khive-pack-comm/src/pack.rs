//! `CommPack` struct, `Pack` impl, `PackRuntime` impl, and self-registration.

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    EmailMessageIdDomains, KhiveRuntime, KindHook, NamespaceToken, NoteEmbeddingPolicy,
    NoteEmbeddingPolicySpec, RuntimeError, VerbRegistry,
};
use khive_types::{HandlerDef, NotePropertyPolicySpec, Pack, PropertyPolicy};

use crate::handlers;
use crate::inbox_signal::InboxSignal;
use crate::vocab::{COMM_HANDLERS, COMM_SCHEMA_PLAN_STMTS};

/// Communication pack providing fifteen public `comm.*` verbs.
///
/// Stores and queries `message` notes in the standard notes table; message
/// metadata lives in the `properties` JSON column.
pub struct CommPack {
    runtime: KhiveRuntime,
    inbox_signal: InboxSignal,
    quarantine_retention: std::time::Duration,
    /// Instance-bound trusted channel-ingest grant (khive #1839 round 3).
    ///
    /// Previously a process-global `OnceLock` shared by every `CommPack`
    /// instance the factory ever created, regardless of which runtime or
    /// composition it belonged to — a grant to one instance leaked into
    /// every other, and a direct `CommPack::new` composition (bypassing
    /// `PackRegistry::register_packs`) could never receive it at all. Scoped
    /// to `self` so each instance's grant reflects only what was explicitly
    /// given to it.
    channel_ingest: std::sync::OnceLock<khive_runtime::ChannelIngestCapability>,
    email_message_id_domains: Result<Option<EmailMessageIdDomains>, String>,
    /// Route used by `comm.send` for node-addressed recipients. Without one, a recipient in the
    /// node address form is handled as an ordinary actor label.
    node_route: Option<std::sync::Arc<dyn crate::node_route::NodeSendRoute>>,
}

impl Pack for CommPack {
    const NAME: &'static str = "comm";
    const NOTE_KINDS: &'static [&'static str] = &["message", "channel_health"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &COMM_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["kg"];
    const NOTE_PROPERTY_POLICIES: &'static [NotePropertyPolicySpec] = &[
        NotePropertyPolicySpec {
            kind: "message",
            key: "quarantined",
            policy: PropertyPolicy::OwnerOnly,
        },
        NotePropertyPolicySpec {
            kind: "message",
            key: "channel_kind",
            policy: PropertyPolicy::OwnerOnly,
        },
        NotePropertyPolicySpec {
            kind: "message",
            key: "channel_slug",
            policy: PropertyPolicy::OwnerOnly,
        },
        NotePropertyPolicySpec {
            kind: "message",
            key: "delivery_hold",
            policy: PropertyPolicy::OwnerOnly,
        },
        NotePropertyPolicySpec {
            kind: "message",
            key: "delivery_hold_reason",
            policy: PropertyPolicy::OwnerOnly,
        },
        NotePropertyPolicySpec {
            kind: "message",
            key: "delivery_hold_at",
            policy: PropertyPolicy::OwnerOnly,
        },
        NotePropertyPolicySpec {
            kind: "message",
            key: "external_id_diagnostic_note_id",
            policy: PropertyPolicy::OwnerOnly,
        },
        NotePropertyPolicySpec {
            kind: "channel_health",
            key: "channel_kind",
            policy: PropertyPolicy::OwnerOnly,
        },
        NotePropertyPolicySpec {
            kind: "channel_health",
            key: "channel_slug",
            policy: PropertyPolicy::OwnerOnly,
        },
    ];
    const NOTE_EMBEDDING_POLICIES: &'static [NoteEmbeddingPolicySpec] =
        &[NoteEmbeddingPolicySpec {
            kind: "message",
            policy: NoteEmbeddingPolicy::DefaultModel,
        }];
    const SCHEMA_PLAN: Option<khive_runtime::PackSchemaPlan> =
        Some(khive_runtime::PackSchemaPlan {
            pack: "comm",
            statements: &COMM_SCHEMA_PLAN_STMTS,
        });
}

impl CommPack {
    /// Create a new `CommPack` bound to the given runtime.
    ///
    /// Holds no channel-ingest capability: `comm.ingest` fails closed with a
    /// configuration/startup error until one is granted, either
    /// automatically (registering through `PackRegistry::register_packs`
    /// under the `comm` name) or explicitly (see
    /// [`Self::new_with_channel_ingest_capability`] or
    /// [`khive_runtime::PackRuntime::accept_channel_ingest_capability`]).
    pub fn new(runtime: KhiveRuntime) -> Self {
        runtime.install_note_embedding_policies(<Self as Pack>::NOTE_EMBEDDING_POLICIES);
        Self {
            runtime,
            inbox_signal: InboxSignal::new(),
            quarantine_retention: std::time::Duration::from_secs(14 * 24 * 60 * 60),
            channel_ingest: std::sync::OnceLock::new(),
            email_message_id_domains: EmailMessageIdDomains::from_env(),
            node_route: None,
        }
    }

    /// Explicit composition of the selected email channel's domain policy.
    pub fn new_with_email_message_id_domains(
        runtime: KhiveRuntime,
        domains: EmailMessageIdDomains,
    ) -> Self {
        Self::new(runtime).with_email_message_id_domains(domains)
    }

    pub fn with_email_message_id_domains(mut self, domains: EmailMessageIdDomains) -> Self {
        self.email_message_id_domains = Ok(Some(domains));
        self
    }

    /// Install the route that seals node-addressed `comm.send` recipients and refuses the ones
    /// it cannot route (disabled channel, unparseable address, unknown recipient pin).
    pub fn with_node_route(
        mut self,
        route: std::sync::Arc<dyn crate::node_route::NodeSendRoute>,
    ) -> Self {
        self.node_route = Some(route);
        self
    }

    /// Create a new `CommPack` with the trusted channel-ingest capability
    /// already granted — for composition roots that construct packs
    /// directly rather than going through `PackRegistry::register_packs`.
    pub fn new_with_channel_ingest_capability(
        runtime: KhiveRuntime,
        capability: khive_runtime::ChannelIngestCapability,
    ) -> Self {
        let pack = Self::new(runtime);
        let _ = pack.channel_ingest.set(capability);
        pack
    }

    /// Set how long a quarantined message and its original-byte attachment
    /// remain live before the channel poller's expiry pass removes them.
    pub fn with_quarantine_retention(mut self, retention: std::time::Duration) -> Self {
        self.quarantine_retention = retention;
        self
    }

    pub(crate) fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }

    pub(crate) fn channel_ingest_capability(
        &self,
    ) -> Option<&khive_runtime::ChannelIngestCapability> {
        self.channel_ingest.get()
    }
}

/// Message properties established only by trusted transport bookkeeping.
///
/// `comm.health` interprets quarantine/channel fields as transport evidence;
/// outbound delivery and inbound thread resolution trust `external_id` as a
/// transport identity; the outbox keeps a message with an unverifiable stored
/// Message-ID out of delivery through the `delivery_hold*` fields and links
/// its diagnostic note through `external_id_diagnostic_note_id`. Generic
/// message creates/updates must not forge any of these fields. `comm.ingest`
/// writes through the trusted ingest path, while the internal outbox claim
/// establishes outbound `external_id` later and the outbox's owner-side hold
/// establishes the delivery-hold and diagnostic fields.
const TRANSPORT_OWNED_MESSAGE_PROPERTIES: &[&str] = &[
    "quarantined",
    "channel_kind",
    "channel_slug",
    "external_id",
    "delivery_hold",
    "delivery_hold_reason",
    "delivery_hold_at",
    "external_id_diagnostic_note_id",
];

fn transport_owned_message_property_named_in(
    properties: &serde_json::Map<String, Value>,
) -> Option<&'static str> {
    TRANSPORT_OWNED_MESSAGE_PROPERTIES
        .iter()
        .copied()
        .find(|key| properties.contains_key(*key))
}

fn refuse_transport_owned_message_property(key: &str) -> RuntimeError {
    if key == "external_id" {
        return RuntimeError::InvalidInput(
            "`external_id` is transport-owned on a `message` note and cannot be supplied by a \
             generic record mutation; only the internal outbox claim or `comm.ingest` may \
             establish it"
                .into(),
        );
    }
    if matches!(
        key,
        "delivery_hold"
            | "delivery_hold_reason"
            | "delivery_hold_at"
            | "external_id_diagnostic_note_id"
    ) {
        return RuntimeError::InvalidInput(format!(
            "`{key}` is transport-owned on a `message` note and cannot be supplied by a generic \
             record mutation; only the internal outbox owner-side hold may establish \
             delivery-hold and diagnostic fields"
        ));
    }
    RuntimeError::InvalidInput(format!(
        "`{key}` is transport-owned on a `message` note and cannot be supplied by a generic \
         record mutation; only `comm.ingest` may establish quarantine disposition and channel \
         provenance"
    ))
}

#[derive(Debug, Default)]
struct MessageHook;

#[async_trait]
impl KindHook for MessageHook {
    async fn prepare_create(
        &self,
        _runtime: &KhiveRuntime,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        if let Some(key) = args
            .get("properties")
            .and_then(Value::as_object)
            .and_then(transport_owned_message_property_named_in)
        {
            return Err(refuse_transport_owned_message_property(key));
        }
        Ok(())
    }

    async fn validate_note_update(
        &self,
        _runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        _note: &khive_storage::Note,
        properties: Option<&Value>,
    ) -> Result<(), RuntimeError> {
        if let Some(key) = properties
            .and_then(Value::as_object)
            .and_then(transport_owned_message_property_named_in)
        {
            return Err(refuse_transport_owned_message_property(key));
        }
        Ok(())
    }
}

/// Keeps shared mutations from changing the derived identity of `channel_health` rows.
#[derive(Debug, Default)]
struct ChannelHealthHook;

#[async_trait]
impl KindHook for ChannelHealthHook {
    async fn prepare_create(
        &self,
        _runtime: &KhiveRuntime,
        _args: &mut Value,
    ) -> Result<(), RuntimeError> {
        // A `channel_health` row is addressed, not searched: `handle_heartbeat` derives a v5
        // UUID from the namespace, channel kind and slug and writes at that id, so one channel
        // keeps exactly one health row that later heartbeats update in place. A row created
        // through a generic path gets a fresh random id instead — one no heartbeat will ever
        // find or update, and one `comm.health` nevertheless reports as an additional channel,
        // because it queries by kind. Unlike a row missing its defaults or its validation, this
        // one cannot be repaired by reading it: it has content, a kind and a timestamp, and it
        // looks complete. The defect is in its identity.
        //
        // The refusal lives here, in the pack that owns the kind, rather than in the generic
        // create handler, because this hook is where all three admitting paths converge: shared
        // `create`, a `stream.batch` write member, and standalone `stream.append` all call it
        // before writing the note. In a batch, the refusal aborts preparation before any sibling
        // commits. A hook also exists only when the pack that owns it is registered, so the
        // refusal can never reach a caller who has no `comm.heartbeat` to dispatch — the hook's
        // existence is that capability check, with nothing to ask the registry.
        //
        // Falsifier: if `create` ever accepts a caller-supplied id, the derivation could be
        // applied on all three paths instead and this refusal would be the wrong shape.
        khive_runtime::hooks::refuse_shared_create("channel_health", "comm.heartbeat")
            .prepare_create(_runtime, _args)
            .await
    }

    async fn validate_note_update(
        &self,
        _runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        _note: &khive_storage::Note,
        properties: Option<&Value>,
    ) -> Result<(), RuntimeError> {
        if let Some(properties) = properties.and_then(Value::as_object) {
            for key in ["channel_kind", "channel_slug"] {
                if properties.contains_key(key) {
                    return Err(RuntimeError::InvalidInput(format!(
                        "`{key}` is not patchable on a `channel_health` note: \
                         `comm.heartbeat` derives its id from the namespace, channel kind \
                         and slug; changing a coordinate would detach the row from that \
                         identity. Omit `{key}` from the patch and use `comm.heartbeat` \
                         to report a channel's health"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Derive a `message` note's authored-by identity from the authorization
/// token, whatever the caller supplied.
///
/// `comm.send` already resolves `from_actor` from `token.actor().id`
/// (`handlers.rs::handle_send`); this is that same resolution applied at the
/// runtime note-write, so a `message` row's `from_actor` is a function of the
/// token on every write path rather than of caller input on some of them. The
/// derived value therefore names the actor whose token performed the write —
/// on the proposal-apply path that is the applying caller, not the proposer
/// who composed the changeset.
///
/// `from_actor` is derived, while transport-owned quarantine disposition and
/// channel provenance are refused on every covered generic/proposal write.
/// `direction` and `sent_at` are equally identity-bearing (they are preserved
/// through `merge` and refused by `update`), but neither is a function of the
/// token: `dual_write_message` writes an inbound copy with
/// `direction="inbound"` under the sender's own token, and `comm.ingest` carries
/// a transport-supplied `sent_at`. Deriving either here would overwrite a value
/// a legitimate caller must set.
///
/// Kinds other than `message` pass through this validator untouched: the
/// runtime holds one validator slot for all packs. comm's own `channel_health`
/// is answered elsewhere — `ChannelHealthHook::prepare_create` refuses shared
/// creation of that kind outright, because what goes wrong there is the row's
/// identity rather than a field this validator could derive.
pub(crate) fn derive_message_identity(
    kind: &str,
    actor_id: &str,
    properties: Option<Value>,
) -> Result<Option<Value>, RuntimeError> {
    if kind != "message" {
        return Ok(properties);
    }
    let mut props = match properties {
        Some(Value::Object(map)) => map,
        Some(other) => {
            return Err(RuntimeError::InvalidInput(format!(
                "a `message` note's properties must be a JSON object, got: {other}"
            )));
        }
        None => serde_json::Map::new(),
    };
    if let Some(key) = transport_owned_message_property_named_in(&props) {
        return Err(refuse_transport_owned_message_property(key));
    }
    props.insert(
        "from_actor".to_string(),
        Value::String(actor_id.to_string()),
    );
    Ok(Some(Value::Object(props)))
}

struct CommPackFactory;

impl khive_runtime::PackFactory for CommPackFactory {
    khive_runtime::pack_factory_metadata!(CommPack);
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(CommPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&CommPackFactory) }

#[async_trait]
impl PackRuntime for CommPack {
    khive_runtime::pack_runtime_metadata!();
    fn note_property_policies(&self) -> &'static [NotePropertyPolicySpec] {
        <Self as Pack>::NOTE_PROPERTY_POLICIES
    }
    fn kind_hook(&self, kind: &str) -> Option<std::sync::Arc<dyn KindHook>> {
        match kind {
            "message" => Some(std::sync::Arc::new(MessageHook)),
            "channel_health" => Some(std::sync::Arc::new(ChannelHealthHook)),
            _ => None,
        }
    }
    fn register_note_write_validator(&self, runtime: &KhiveRuntime) {
        runtime.install_note_write_validator(std::sync::Arc::new(derive_message_identity));
    }
    fn accept_channel_ingest_capability(&self, capability: khive_runtime::ChannelIngestCapability) {
        let _ = self.channel_ingest.set(capability);
    }
    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "comm.suppression_add" => {
                handlers::handle_suppression_add(self.runtime(), token, params).await
            }
            "comm.suppression_list" => {
                handlers::handle_suppression_list(self.runtime(), token, params).await
            }
            "comm.suppression_remove" => {
                handlers::handle_suppression_remove(self.runtime(), token, params).await
            }
            "comm.withheld_body" => {
                handlers::handle_withheld_body(self.runtime(), token, params).await
            }
            "comm.send" => {
                handlers::handle_send_with_route(
                    self.runtime(),
                    &self.inbox_signal,
                    token,
                    params,
                    self.node_route.as_deref(),
                )
                .await
            }
            "comm.delivered" => handlers::handle_delivered(self.runtime(), token, params).await,
            "comm.transport_status" => {
                handlers::handle_transport_status(self.runtime(), token, params).await
            }
            "comm.inbox" => {
                handlers::handle_inbox(self.runtime(), &self.inbox_signal, token, params).await
            }
            "comm.read" => handlers::handle_read(self.runtime(), token, params).await,
            "comm.mark_read" => handlers::handle_mark_read(self.runtime(), token, params).await,
            "comm.unread" => handlers::handle_unread(self.runtime(), token, params).await,
            "comm.reply" => {
                handlers::handle_reply(
                    self.runtime(),
                    &self.inbox_signal,
                    &self.email_message_id_domains,
                    token,
                    params,
                )
                .await
            }
            "comm.thread" => handlers::handle_thread(self.runtime(), token, params).await,
            "comm.ingest" => {
                handlers::handle_ingest(
                    self.runtime(),
                    &self.inbox_signal,
                    self.channel_ingest_capability(),
                    &self.email_message_id_domains,
                    token,
                    params,
                    self.quarantine_retention,
                )
                .await
            }
            "comm.cleanup_expired_quarantine" => {
                handlers::handle_cleanup_expired_quarantine(
                    self.runtime(),
                    token,
                    params,
                    self.quarantine_retention,
                )
                .await
            }
            "comm.heartbeat" => handlers::handle_heartbeat(self.runtime(), token, params).await,
            "comm.health" => handlers::handle_health(self.runtime(), token, params).await,
            "comm.probe" => handlers::handle_probe(self.runtime(), token, params).await,
            "comm.cursor_get" => handlers::handle_cursor_get(self.runtime(), params).await,
            "comm.cursor_commit" => handlers::handle_cursor_commit(self.runtime(), params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "comm pack does not handle verb {verb:?}"
            ))),
        }
    }
}

#[cfg(test)]
#[path = "pack_help_tests.rs"]
mod help_tests;

#[cfg(test)]
#[path = "pack_message_identity_tests.rs"]
mod message_identity_tests;
