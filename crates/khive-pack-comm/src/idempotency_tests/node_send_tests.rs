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
    let args =
        json!({"to": NODE_TO, "subject": "s", "content": "c", "idempotency_key": "node-op-1"});
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

fn shared_backend() -> Arc<khive_db::StorageBackend> {
    let backend = khive_db::StorageBackend::memory().expect("in-memory backend");
    backend.pool().run_migrations().expect("migrations");
    Arc::new(backend)
}

fn as_route(route: &Arc<NodeTestRoute>) -> Arc<dyn crate::node_route::NodeSendRoute> {
    route.clone()
}

async fn transport_row_count(runtime: &KhiveRuntime) -> i64 {
    let result = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_scalar(SqlStatement {
            sql: "SELECT count(*) FROM comm_sender_transport".into(),
            params: vec![],
            label: Some("node-send-transport-count".into()),
        })
        .await
        .unwrap();
    match result {
        Some(SqlValue::Integer(n)) => n,
        other => panic!("unexpected transport count: {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_recipient_without_a_route_is_refused_disabled_and_writes_nothing() {
    let (sender, _, runtime) = parties();
    for to in [NODE_TO, "KHIVE1:realm/agent", "Khive1:realm/agent"] {
        let err = sender
            .dispatch(
                "comm.send",
                json!({"to": to, "subject": "s", "content": "no route"}),
            )
            .await
            .expect_err("a node address with no route must be refused");
        let text = format!("{err:?}");
        assert!(text.contains("disabled"), "{to}: {text}");
        assert_eq!(message_count(&runtime, false).await, 0, "{to}");
        assert_eq!(transport_row_count(&runtime).await, 0, "{to}");
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_scheme_is_case_insensitive_when_a_route_is_installed() {
    let route = NodeTestRoute::accepting();
    let (sender, runtime) = routed_sender(&route);
    sender
        .dispatch(
            "comm.send",
            json!({"to": "KHIVE1:realm/agent", "subject": "s", "content": "upper"}),
        )
        .await
        .expect("upper-case scheme send");
    assert_eq!(
        route.ids().len(),
        1,
        "the upper-case scheme reaches the route"
    );
    assert_eq!(transport_row_count(&runtime).await, 1);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_recipient_with_leading_whitespace_is_refused_with_zero_writes() {
    let route = NodeTestRoute::accepting();
    let (sender, runtime) = routed_sender(&route);
    for to in [
        " khive1:realm/agent",
        "\tkhive1:realm/agent",
        "khive1:realm/agent ",
    ] {
        let result = sender
            .dispatch(
                "comm.send",
                json!({"to": to, "subject": "s", "content": "padded"}),
            )
            .await;
        if to.starts_with(char::is_whitespace) {
            let text = format!(
                "{:?}",
                result.expect_err("padded node address must be refused")
            );
            assert!(text.contains("not a valid node address"), "{to:?}: {text}");
            assert_eq!(message_count(&runtime, false).await, 0, "{to:?}");
            assert_eq!(transport_row_count(&runtime).await, 0, "{to:?}");
        }
    }
    assert!(
        route.ids().is_empty(),
        "a padded address never reaches the route"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_keyed_replay_of_a_succeeded_send_never_calls_the_route_again() {
    let backend = shared_backend();
    let first_route = NodeTestRoute::accepting();
    let (first_sender, runtime) = actor_registry_with_route(
        Arc::clone(&backend),
        "actor:sender",
        "local",
        Some(as_route(&first_route)),
    );
    let args =
        json!({"to": NODE_TO, "subject": "s", "content": "c", "idempotency_key": "node-op-2"});
    let first = first_sender
        .dispatch("comm.send", args.clone())
        .await
        .expect("first keyed node send");
    assert_eq!(first_route.ids().len(), 1);
    let again = first_sender
        .dispatch("comm.send", args.clone())
        .await
        .expect("replay");
    assert_eq!(again["replayed"], true);
    assert_eq!(first_route.ids().len(), 1, "a replay must not seal again");
    // The same actor now has a route that refuses everything; the replay still succeeds because
    // the idempotency decision comes before the route is consulted.
    let refusing_route = NodeTestRoute::refusing(crate::node_route::NodeSendRefusal::Disabled);
    let (refusing_sender, _) = actor_registry_with_route(
        backend,
        "actor:sender",
        "local",
        Some(as_route(&refusing_route)),
    );
    let replay = refusing_sender
        .dispatch("comm.send", args)
        .await
        .expect("replay with a refusing route");
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["full_id"], first["full_id"]);
    assert!(
        refusing_route.ids().is_empty(),
        "the refusing route was never called"
    );
    assert_eq!(transport_row_count(&runtime).await, 1);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_send_failure_at_the_transport_write_leaves_no_half_pair() {
    for key in [None, Some("node-op-atomic")] {
        let route = NodeTestRoute::accepting();
        let (sender, runtime) = routed_sender(&route);
        runtime
            .sql()
            .writer()
            .await
            .expect("comm writer")
            .execute_script(
                "CREATE TRIGGER fail_sender_transport_insert BEFORE INSERT ON comm_sender_transport \
                 BEGIN SELECT RAISE(ABORT, 'injected transport write failure'); END;"
                    .into(),
            )
            .await
            .expect("install fault at the second write");
        let mut args = json!({"to": NODE_TO, "subject": "s", "content": "atomic"});
        if let Some(key) = key {
            args["idempotency_key"] = json!(key);
        }
        let err = sender
            .dispatch("comm.send", args)
            .await
            .expect_err("the injected transport failure must fail the send");
        assert!(
            format!("{err:?}").contains("injected transport write failure"),
            "{err:?}"
        );
        assert_eq!(
            message_count(&runtime, false).await,
            0,
            "no outbound or inbound note may survive a failed transport write ({key:?})"
        );
        assert_eq!(transport_row_count(&runtime).await, 0);
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn node_send_two_senders_with_one_key_get_distinct_logical_ids() {
    let backend = shared_backend();
    let route_a = NodeTestRoute::accepting();
    let route_b = NodeTestRoute::accepting();
    let (sender_a, runtime) = actor_registry_with_route(
        Arc::clone(&backend),
        "actor:sender",
        "local",
        Some(as_route(&route_a)),
    );
    let (sender_b, _) =
        actor_registry_with_route(backend, "actor:other", "local", Some(as_route(&route_b)));
    let args =
        json!({"to": NODE_TO, "subject": "s", "content": "c", "idempotency_key": "shared-key"});
    let first = sender_a
        .dispatch("comm.send", args.clone())
        .await
        .expect("sender a");
    let second = sender_b
        .dispatch("comm.send", args)
        .await
        .expect("sender b");
    assert_eq!(
        second["replayed"], false,
        "another sender's key is not a replay"
    );
    assert_ne!(first["full_id"], second["full_id"]);
    let (ids_a, ids_b) = (route_a.ids(), route_b.ids());
    assert_eq!(ids_a.len(), 1);
    assert_eq!(ids_b.len(), 1);
    assert_ne!(ids_a[0], ids_b[0], "each sender owns its own logical id");
    assert_eq!(transport_row_count(&runtime).await, 2);
}

const RACE_SENT_AT: &str = "2026-01-01T00:00:00Z";

/// A sealed envelope for a node send whose logical id the test chooses.
fn race_envelope(logical_message_id: Uuid) -> SenderEnvelope {
    SenderEnvelope {
        namespace: "local".to_string(),
        logical_message_id,
        outbound_note_id: Uuid::nil(),
        kind: "node".to_string(),
        slug: "test".to_string(),
        credential_ref: "test-credential".to_string(),
        recipient_address: NODE_TO.to_string(),
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
    }
}

/// The comm-level result of one keyed node send, shaped as `comm.send` renders it.
fn race_response(
    identity: &crate::idempotency::MessageIdentity,
    write: &crate::message::MessageWrite,
) -> Value {
    let mut response = json!({
        "full_id": write.outbound.id.as_hyphenated().to_string(),
        "sent_at": RACE_SENT_AT,
    });
    identity.annotate_response(&mut response, &write.outbound, write.replayed);
    response
}

/// Two keyed node sends with one key both pass the holder lookup before either commits, so both
/// reach the pair writer with the same logical id. The key claim decides the race: the loser
/// replays the winner instead of failing on the transport row, and nothing is written twice.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn keyed_node_send_that_loses_the_duplicate_race_replays_the_winner() {
    let route = NodeTestRoute::accepting();
    let (_registry, runtime) = routed_sender(&route);
    let token = runtime
        .authorize(Namespace::local())
        .expect("authorize local");
    let namespace = token.namespace().as_str().to_string();
    let from_actor = token.actor().id.clone();
    // The request is shaped exactly as `comm.send` builds it (handlers.rs): replay compares the
    // stored subject, thread and tags against it, so a partial request never replays.
    let identity = crate::idempotency::MessageIdentity::new(Some("race-key"), || {
        crate::file_attachments::identify_request(
            json!({
                "version": 1, "op": "send", "to": NODE_TO, "content": "raced",
                "subject": "s", "thread_id": null, "tags": [], "reply_parent_id": null,
            }),
            &[],
        )
    })
    .expect("valid key")
    .expect("key present");
    let logical_message_id = Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        identity.physical_key(&token).as_bytes(),
    );

    // Both sends found no holder, so each arrives here with the same identity and the same
    // sealed envelope; the second write runs against the committed first.
    let mut writes = Vec::new();
    for _ in 0..2 {
        let write = crate::message::dual_write_message_with_transport(
            &runtime,
            &token,
            "comm.send",
            &namespace,
            &namespace,
            Some("s"),
            "raced",
            None,
            RACE_SENT_AT,
            None,
            Some(&from_actor),
            Some(NODE_TO),
            None,
            None,
            None,
            &[],
            Some(&identity),
            Some(race_envelope(logical_message_id)),
        )
        .await
        .expect("a keyed node send never fails on losing the race");
        writes.push(write);
    }
    let (first, second) = (&writes[0], &writes[1]);

    assert!(!first.replayed, "the winner creates the pair");
    assert!(second.replayed, "the loser replays the existing holder");
    assert_eq!(second.outbound.id, first.outbound.id);

    assert_eq!(
        message_count(&runtime, false).await,
        2,
        "exactly one outbound and one inbound note"
    );
    assert_eq!(message_count(&runtime, true).await, 1);
    assert_eq!(
        transport_row_count(&runtime).await,
        1,
        "exactly one transport row"
    );
    let record = runtime
        .sender_transport(node_key(logical_message_id))
        .await
        .unwrap()
        .expect("sender transport row");
    assert_eq!(record.envelope.outbound_note_id, first.outbound.id);
    assert_eq!(record.state, TransportState::Pending);

    let mut first_response = race_response(&identity, first);
    let mut second_response = race_response(&identity, second);
    assert_eq!(first_response["replayed"], false);
    assert_eq!(second_response["replayed"], true);
    first_response["replayed"] = Value::Null;
    second_response["replayed"] = Value::Null;
    assert_eq!(
        first_response, second_response,
        "the comm-level result is the same for both calls"
    );
}
