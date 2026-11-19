#[tokio::test]
async fn receipt_defaults_preserve_legacy_send_and_poll() {
    let envelope = ChannelEnvelope::new("legacy:sender", "legacy:recipient", "body")
        .with_subject("subject")
        .with_sent_at(Utc::now())
        .with_external_id("external-id")
        .with_correlation("thread-id")
        .with_quarantine_replay(vec![0, 255, 13, 10], "legacy:sender");
    let expected_bytes = serde_json::to_vec(&envelope).unwrap();
    let adapter = LegacyChannel {
        sent: Mutex::new(Vec::new()),
        inbound: vec![envelope.clone()],
        fail: false,
    };
    let channel: &dyn Channel = &adapter;
    assert_eq!(
        channel.send_with_receipt(envelope).await.unwrap(),
        SendOutcome::LegacyAccepted
    );
    {
        let sent = adapter.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(serde_json::to_vec(&sent[0]).unwrap(), expected_bytes);
        assert_eq!(
            sent[0].quarantine_replay.as_ref().unwrap().bytes,
            [0, 255, 13, 10]
        );
    }
    let deliveries = channel.poll_deliveries(Utc::now(), None).await.unwrap();
    assert!(deliveries.receipt_results().is_empty());
    assert_eq!(deliveries.receipts_cursor(), None);
    assert_eq!(deliveries.tickets().len(), 1);
    assert!(deliveries.tickets().iter().all(Option::is_none));
    let (page, tickets) = deliveries.into_parts();
    assert_eq!(page.envelopes.len(), tickets.len());
    assert_eq!(
        serde_json::to_vec(&page.envelopes[0]).unwrap(),
        expected_bytes
    );
    assert_eq!(
        page.envelopes[0].quarantine_replay.as_ref().unwrap().bytes,
        [0, 255, 13, 10]
    );
    assert!(page.next_checkpoint.is_none());

    let error = channel
        .acknowledge_receipt(&receipt(ReceiptDisposition::Stored))
        .await
        .unwrap_err();
    assert_eq!(
        error.delivery_failure_class(),
        DeliveryFailureClass::Permanent
    );
    assert!(
        matches!(error, ChannelError::Config(message) if message == "receipts unsupported by legacy")
    );
}

#[tokio::test]
async fn receipt_defaults_propagate_legacy_errors() {
    let channel = LegacyChannel {
        sent: Mutex::new(Vec::new()),
        inbound: vec![],
        fail: true,
    };
    assert!(matches!(
        channel.send_with_receipt(ChannelEnvelope::new("a", "b", "c")).await,
        Err(ChannelError::Transport(message)) if message == "send unavailable"
    ));
    assert!(matches!(
        channel.poll_deliveries(Utc::now(), None).await,
        Err(ChannelError::UnauthorizedSender(message)) if message == "poll denied"
    ));
    assert!(channel.sent.lock().unwrap().is_empty());
}
