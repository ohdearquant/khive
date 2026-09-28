use super::*;
use khive_channel::DeliveryReceiptBinding;
fn fixture() -> (
    KhiveRuntime,
    NamespaceToken,
    LocalRecipientBinding,
    DeliveryReceiptBinding,
) {
    let runtime = KhiveRuntime::memory().unwrap();
    let local = LocalRecipientBinding {
        actor: "lambda:receiver".into(),
        realm: "example".into(),
        slug: "device".into(),
        agent_id: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4(),
        key_epoch: 1,
    };
    let binding = DeliveryReceiptBinding {
        protocol_version: 1,
        logical_message_id: Uuid::new_v4(),
        sender_agent_id: Uuid::new_v4().to_string(),
        recipient_agent_id: local.agent_id.clone(),
        recipient_device_id: local.device_id,
        recipient_key_epoch: 1,
        contact_generation: 1,
        delivery_attempt_id: Uuid::new_v4(),
    };
    (runtime, NamespaceToken::local(), local, binding)
}
fn payload(correlation: Option<String>) -> VerifiedInboundContent {
    VerifiedInboundContent::Message {
        content: "hello".into(),
        subject: None,
        correlation,
    }
}
#[tokio::test]
async fn local_binding_fixes_recipient_and_derives_sender() {
    let (runtime, token, local, binding) = fixture();
    let mut wrong = binding.clone();
    wrong.recipient_agent_id = Uuid::new_v4().to_string();
    assert!(
        runtime
            .ingest_verified_recipient(
                &token,
                &local,
                InboundReceiptTicket::new(wrong, 1),
                payload(None),
                vec![]
            )
            .await
            .is_err(),
        "remote recipient must not redirect local binding"
    );
    let result = runtime
        .ingest_verified_recipient(
            &token,
            &local,
            InboundReceiptTicket::new(binding.clone(), 1),
            payload(None),
            vec![],
        )
        .await
        .unwrap();
    let note = result.note.unwrap();
    let props = note.properties.unwrap();
    assert_eq!(
        props["to_actor"], local.actor,
        "recipient actor must come from local binding"
    );
    assert_eq!(
        props["from_actor"],
        format!("khive1:example/{}", binding.sender_agent_id)
    );
}
#[tokio::test]
async fn correlation_is_confined_to_authenticated_pair() {
    for same_pair in [false, true] {
        let (runtime, token, local, binding) = fixture();
        let thread = Uuid::new_v4();
        let mut root = Note::new("local", "message", "root");
        root.properties = Some(
            json!({"external_id":"correlation","thread_id":thread,"from_actor":local.actor,"to_actor":if same_pair {format!("khive1:example/{}",binding.sender_agent_id)} else {"lambda:other".into()},"direction":"outbound"}),
        );
        runtime
            .notes(&token)
            .unwrap()
            .upsert_note(root)
            .await
            .unwrap();
        let result = runtime
            .ingest_verified_recipient(
                &token,
                &local,
                InboundReceiptTicket::new(binding, 1),
                payload(Some("correlation".into())),
                vec![],
            )
            .await
            .unwrap();
        let note = result.note.unwrap();
        assert_eq!(
            note.properties.unwrap()["thread_id"],
            if same_pair {
                thread.to_string()
            } else {
                note.id.to_string()
            },
            "thread must be confined to authenticated pair"
        );
    }
}
#[tokio::test]
async fn quarantine_notification_is_body_free_and_replay_is_verbatim() {
    let (runtime, token, local, binding) = fixture();
    let bytes = b"{ \"ciphertext\": \"opaque-sensitive-payload\" }".to_vec();
    let result = runtime
        .ingest_verified_recipient(
            &token,
            &local,
            InboundReceiptTicket::new(binding, 1),
            VerifiedInboundContent::Quarantine {
                reason: QuarantineReason::InvalidPlaintext,
            },
            bytes.clone(),
        )
        .await
        .unwrap();
    assert_eq!(result.disposition, RecipientDisposition::Quarantined);
    let note = result.note.unwrap();
    assert!(!note.content.contains("opaque-sensitive-payload"));
    assert!(!note
        .properties
        .unwrap()
        .to_string()
        .contains("opaque-sensitive-payload"));
    let stored: Vec<u8> = runtime
        .backend()
        .pool()
        .writer()
        .unwrap()
        .conn()
        .query_row(
            "SELECT delivery_item FROM comm_recipient_quarantine",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, bytes);
}
