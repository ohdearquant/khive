// Node-addressed `comm.send`: sealing, refusals, stored envelope and caller-keyed replay.

use khive_db::stores::note::transport::{
    EnvelopeKey, SenderAssurance, SenderEnvelope, TransportState,
};

const NODE_TO: &str = "khive1:realm/agent";
const NODE_DEVICE: u128 = 9;
const NODE_CIPHERTEXT: &[u8] = b"sealed-by-test-route";

/// Test route: records every logical id it is asked to seal, and either refuses or returns a
/// fixed sealed envelope.
struct NodeTestRoute {
    refusal: Option<crate::node_route::NodeSendRefusal>,
    sealed_ids: std::sync::Mutex<Vec<Uuid>>,
}

impl NodeTestRoute {
    fn accepting() -> Arc<Self> {
        Arc::new(Self {
            refusal: None,
            sealed_ids: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn refusing(refusal: crate::node_route::NodeSendRefusal) -> Arc<Self> {
        Arc::new(Self {
            refusal: Some(refusal),
            sealed_ids: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn ids(&self) -> Vec<Uuid> {
        self.sealed_ids.lock().unwrap().clone()
    }
}

impl crate::node_route::NodeSendRoute for NodeTestRoute {
    fn seal<'a>(
        &'a self,
        request: crate::node_route::NodeSealRequest<'a>,
    ) -> crate::node_route::SealFuture<'a> {
        Box::pin(async move {
            self.sealed_ids
                .lock()
                .unwrap()
                .push(request.logical_message_id);
            if let Some(refusal) = &self.refusal {
                return Err(refusal.clone());
            }
            Ok(SenderEnvelope {
                namespace: "local".to_string(),
                logical_message_id: request.logical_message_id,
                outbound_note_id: Uuid::nil(),
                kind: "node".to_string(),
                slug: "test".to_string(),
                credential_ref: "test-credential".to_string(),
                recipient_address: request.address.to_string(),
                protocol_version: 1,
                sender_agent_id: Uuid::from_u128(1).to_string(),
                sender_assurance: SenderAssurance::Claimed,
                recipient_agent_id: Uuid::from_u128(2).to_string(),
                recipient_device_id: Uuid::from_u128(NODE_DEVICE),
                recipient_key_epoch: 1,
                contact_generation: 1,
                sender_key_epoch: 1,
                recipient_key_fingerprint: "ab".repeat(32),
                enc: vec![7u8; 32],
                ciphertext: NODE_CIPHERTEXT.to_vec(),
            })
        })
    }
}

fn node_key(logical_message_id: Uuid) -> EnvelopeKey {
    EnvelopeKey {
        logical_message_id,
        recipient_device_id: Uuid::from_u128(NODE_DEVICE),
        recipient_key_epoch: 1,
    }
}

fn routed_sender(route: &Arc<NodeTestRoute>) -> (VerbRegistry, KhiveRuntime) {
    let backend = khive_db::StorageBackend::memory().expect("in-memory backend");
    backend.pool().run_migrations().expect("migrations");
    let route: Arc<dyn crate::node_route::NodeSendRoute> = route.clone();
    actor_registry_with_route(Arc::new(backend), "actor:sender", "local", Some(route))
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_send_stores_the_sealed_envelope_for_the_outbound_note() {
    let route = NodeTestRoute::accepting();
    let (sender, runtime) = routed_sender(&route);
    let sent = sender
        .dispatch(
            "comm.send",
            json!({"to": NODE_TO, "subject": "s", "content": "hello node"}),
        )
        .await
        .expect("node send");
    let ids = route.ids();
    assert_eq!(ids.len(), 1, "the message is sealed exactly once");
    let record = runtime
        .sender_transport(node_key(ids[0]))
        .await
        .unwrap()
        .expect("sender transport row");
    assert_eq!(record.envelope.ciphertext.as_slice(), NODE_CIPHERTEXT);
    assert_eq!(record.envelope.logical_message_id, ids[0]);
    assert_eq!(
        record.envelope.outbound_note_id.to_string(),
        sent["full_id"].as_str().expect("full_id"),
        "the row points at the outbound note"
    );
    assert_eq!(record.state, TransportState::Pending);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_send_refusals_write_nothing_and_have_distinct_reasons() {
    use crate::node_route::NodeSendRefusal;
    let cases = [
        (NodeSendRefusal::Disabled, "disabled"),
        (
            NodeSendRefusal::UnparseableAddress("bad address".to_string()),
            "not a valid node address",
        ),
        (
            NodeSendRefusal::UnknownRecipientPin(NODE_TO.to_string()),
            "no pinned key",
        ),
    ];
    let mut texts = Vec::new();
    for (refusal, needle) in cases {
        let route = NodeTestRoute::refusing(refusal);
        let (sender, runtime) = routed_sender(&route);
        let err = sender
            .dispatch(
                "comm.send",
                json!({"to": NODE_TO, "subject": "s", "content": "refused"}),
            )
            .await
            .expect_err("refused send");
        let text = format!("{err:?}");
        assert!(text.contains(needle), "{needle:?} not in {text}");
        assert_eq!(
            message_count(&runtime, false).await,
            0,
            "a refused send leaves no message row"
        );
        let ids = route.ids();
        assert_eq!(ids.len(), 1);
        assert!(
            runtime
                .sender_transport(node_key(ids[0]))
                .await
                .unwrap()
                .is_none(),
            "a refused send leaves no transport row"
        );
        texts.push(text);
    }
    assert_ne!(texts[0], texts[1]);
    assert_ne!(texts[1], texts[2]);
    assert_ne!(texts[0], texts[2]);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_send_keyed_replay_keeps_the_first_logical_id_and_one_row() {
    let route = NodeTestRoute::accepting();
    let (sender, runtime) = routed_sender(&route);
    let args = json!({"to": NODE_TO, "subject": "s", "content": "c", "idempotency_key": "node-op-1"});
    let first = sender
        .dispatch("comm.send", args.clone())
        .await
        .expect("first keyed node send");
    let replay = sender
        .dispatch("comm.send", args)
        .await
        .expect("replayed keyed node send");
    assert_eq!(first["replayed"], false);
    assert_eq!(replay["replayed"], true);
    let ids = route.ids();
    assert!(!ids.is_empty());
    assert!(
        ids.iter().all(|id| *id == ids[0]),
        "a keyed replay must keep the logical id of the first attempt: {ids:?}"
    );
    let record = runtime
        .sender_transport(node_key(ids[0]))
        .await
        .unwrap()
        .expect("sender transport row");
    assert_eq!(
        record.envelope.outbound_note_id.to_string(),
        first["full_id"].as_str().expect("full_id")
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_route_is_not_used_for_ordinary_actor_recipients() {
    let route = NodeTestRoute::accepting();
    let (sender, runtime) = routed_sender(&route);
    sender
        .dispatch(
            "comm.send",
            json!({"to": "actor:recipient", "subject": "s", "content": "local"}),
        )
        .await
        .expect("local send");
    assert!(route.ids().is_empty(), "only node addresses are sealed");
    assert_eq!(message_count(&runtime, false).await, 2);
}
