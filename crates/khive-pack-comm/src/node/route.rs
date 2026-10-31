//! Concrete node send route.
//!
//! Seals one outgoing message for a pinned node recipient. The route only seals: it writes
//! nothing, and every refusal happens before the send handler stores a message row. The sealed
//! envelope is returned to the handler, which persists it as a sender transport row for the node
//! channel client to submit later.

use std::sync::Arc;

use async_trait::async_trait;
use khive_channel_node::client::NodeClient;
use khive_channel_node::encoding::{CanonicalUuid, NodeAddress, ProtocolVersion};
use khive_channel_node::envelope::{EnvelopeHeader, MAX_PLAINTEXT_BYTES};
use khive_channel_node::keys::KeyFacility;
use khive_channel_node::pins::{PinIdentity, PinSource, PinState};
use khive_channel_node::plaintext::Plaintext;
use khive_channel_node::response::{NodeContactResult, NodeError};
use khive_channel_node::timestamp::UtcTimestamp;
use khive_runtime::comm_transport::{PolicyMode, SenderAssurance, SenderEnvelope};
use khive_runtime::RuntimeError;
use uuid::Uuid;

use super::NodeHostState;
use crate::node_route::{NodeSealRequest, NodeSendRefusal, NodeSendRoute, SealFuture};

/// Transport kind recorded on every sealed node envelope.
const NODE_KIND: &str = "khive";

/// The only node protocol version this route seals for.
const PROTOCOL_VERSION: u64 = 1;

/// Looks up a recipient's current contact record on the node service.
///
/// The production implementation is the node client; tests supply a fake.
#[async_trait]
pub(crate) trait NodeContactLookup: Send + Sync {
    /// Fetch the current device, key epoch and contact generation for `agent`.
    async fn contact(&self, agent: Uuid) -> Result<NodeContactResult, NodeError>;
}

#[async_trait]
impl NodeContactLookup for NodeClient {
    async fn contact(&self, agent: Uuid) -> Result<NodeContactResult, NodeError> {
        NodeClient::contact(self, agent).await
    }
}

/// Send route backed by the shared node host state.
pub(crate) struct RuntimeNodeSendRoute {
    state: Arc<NodeHostState>,
}

impl RuntimeNodeSendRoute {
    /// Build a route over the shared host state. No I/O happens here; services are opened on the
    /// first send.
    pub(crate) fn new(state: Arc<NodeHostState>) -> Self {
        Self { state }
    }

    /// The fixed sealing order. Every step either passes or returns a refusal; nothing is written.
    async fn seal_message(
        &self,
        request: NodeSealRequest<'_>,
    ) -> Result<SenderEnvelope, NodeSendRefusal> {
        let config = self.state.config();
        let binding = self.state.binding();

        // 1. Actor and request namespace must be the configured node binding.
        let Some(actor) = config.actor.as_deref() else {
            return Err(NodeSendRefusal::InvalidConfiguration(
                "actor is not set".to_string(),
            ));
        };
        if request.from_actor != actor || request.namespace != config.namespace {
            return Err(NodeSendRefusal::SenderBindingMismatch);
        }

        // 2. Canonical recipient address in this node's realm.
        let address = NodeAddress::parse(request.address).map_err(|_| {
            NodeSendRefusal::UnparseableAddress("expected khive1:<realm>/<agent id>".to_string())
        })?;
        if address.to_string() != request.address {
            return Err(NodeSendRefusal::UnparseableAddress(
                "address is not in canonical form".to_string(),
            ));
        }
        if address.require_realm(&binding.realm).is_err() {
            return Err(NodeSendRefusal::SenderBindingMismatch);
        }

        // 3. Shared services (opened once, on first use).
        let services = self
            .state
            .services()
            .await
            .map_err(|error| services_refusal(&error))?;
        let lookup: &dyn NodeContactLookup = services.lookup().as_ref();
        let facility: &dyn KeyFacility = services.key_facility().as_ref();
        let pins: &dyn PinSource = services.pin_source().as_ref();

        // 4. Current contact record; it must agree with the owner's pin.
        let contact = lookup
            .contact(address.agent().into_uuid())
            .await
            .map_err(|error| contact_refusal(&error))?;
        let observed = &contact.observed;
        if observed.address != address
            || observed.agent_id != address.agent()
            || observed.validated_keys(&binding.realm).is_err()
        {
            return Err(NodeSendRefusal::ContactUnavailable);
        }
        if !contact.matches_owner_pin {
            return Err(NodeSendRefusal::UnknownRecipientPin(address.to_string()));
        }

        // 5. Resolve the exact pinned keys independently of the contact response. A source
        //    failure is an outage, never an unknown pin.
        let identity = PinIdentity {
            realm: binding.realm.clone(),
            agent: observed.agent_id,
            device: observed.device_id,
            epoch: observed.key_epoch,
        };
        let pin = match pins.resolve(&identity).await {
            Ok(PinState::Confirmed(pin)) => pin,
            Ok(_) => return Err(NodeSendRefusal::UnknownRecipientPin(address.to_string())),
            Err(_) => return Err(NodeSendRefusal::SourceUnavailable),
        };
        if pin.identity() != &identity || pin.fingerprint() != &observed.fingerprint {
            return Err(NodeSendRefusal::UnknownRecipientPin(address.to_string()));
        }

        // 6. Plaintext from the request. The current send carries no kind and no reply target.
        let version =
            ProtocolVersion::new(PROTOCOL_VERSION).map_err(|_| NodeSendRefusal::SealFailed)?;
        let sent_at = chrono::DateTime::parse_from_rfc3339(request.sent_at)
            .map_err(|_| NodeSendRefusal::SealFailed)?
            .with_timezone(&chrono::Utc);
        let thread_id = match request.thread_id {
            Some(id) => Some(CanonicalUuid::parse(id).map_err(|_| NodeSendRefusal::SealFailed)?),
            None => None,
        };
        let plaintext = Plaintext {
            v: version,
            subject: request.subject.map(str::to_owned),
            body: request.content.to_owned(),
            sent_at: UtcTimestamp::from_utc(sent_at),
            thread_id,
            in_reply_to: None,
            kind: None,
        };
        let plaintext_bytes =
            serde_json::to_vec(&plaintext).map_err(|_| NodeSendRefusal::SealFailed)?;
        if plaintext_bytes.len() > MAX_PLAINTEXT_BYTES {
            return Err(NodeSendRefusal::SealFailed);
        }

        // 7. One HPKE seal to the pinned key.
        let header = EnvelopeHeader {
            protocol_version: version,
            realm: binding.realm.clone(),
            sender_agent_id: binding.agent,
            sender_device_id: binding.device,
            sender_key_epoch: binding.key_epoch,
            recipient_agent_id: observed.agent_id,
            recipient_device_id: observed.device_id,
            recipient_key_epoch: observed.key_epoch,
        };
        let sealed = facility
            .seal(
                &header,
                CanonicalUuid::from_uuid(request.logical_message_id),
                &pin.keys().kem,
                &plaintext_bytes,
            )
            .map_err(|_| NodeSendRefusal::SealFailed)?;

        // 8. Map to the stored envelope. The handler overwrites the note id once the outbound
        //    note exists.
        let fingerprint: String = pin
            .fingerprint()
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(SenderEnvelope {
            namespace: request.namespace.to_owned(),
            logical_message_id: request.logical_message_id,
            outbound_note_id: Uuid::nil(),
            kind: NODE_KIND.to_string(),
            slug: binding.slug.clone(),
            credential_ref: binding.key_reference.clone(),
            recipient_address: address.to_string(),
            protocol_version: PROTOCOL_VERSION as u32,
            sender_agent_id: binding.agent.to_string(),
            sender_assurance: SenderAssurance::Claimed,
            recipient_agent_id: observed.agent_id.to_string(),
            recipient_device_id: observed.device_id.into_uuid(),
            recipient_key_epoch: observed.key_epoch.get(),
            contact_generation: observed.contact_generation.get(),
            sender_key_epoch: binding.key_epoch.get(),
            recipient_key_fingerprint: fingerprint,
            enc: sealed.enc.as_bytes().to_vec(),
            ciphertext: sealed.ciphertext.as_bytes().to_vec(),
        })
    }
}

impl NodeSendRoute for RuntimeNodeSendRoute {
    fn seal<'a>(&'a self, request: NodeSealRequest<'a>) -> SealFuture<'a> {
        Box::pin(async move {
            // Pair policy evaluation is not available yet: refuse before anything else runs.
            if matches!(
                self.state.config().pair_policy_mode,
                PolicyMode::Shadow | PolicyMode::Enforce
            ) {
                return Err(NodeSendRefusal::PairPolicyUnavailable);
            }
            self.seal_message(request).await
        })
    }
}

/// Map a service-initialisation failure to a bounded refusal. No raw error text is carried.
fn services_refusal(error: &RuntimeError) -> NodeSendRefusal {
    match error {
        RuntimeError::InvalidInput(_) => {
            NodeSendRefusal::InvalidConfiguration("service settings".to_string())
        }
        _ => NodeSendRefusal::SourceUnavailable,
    }
}

/// Map a contact lookup failure to a bounded refusal. Response bodies are never carried.
fn contact_refusal(error: &NodeError) -> NodeSendRefusal {
    match error {
        NodeError::Source(_) | NodeError::Pins(_) => NodeSendRefusal::SourceUnavailable,
        NodeError::Channel { .. } => NodeSendRefusal::ContactUnavailable,
    }
}
