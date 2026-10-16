#[test]
fn delivery_page_checks_counts_and_retains_ticket_order() {
    let make_page = || {
        ChannelPollPage::stateless(vec![
            ChannelEnvelope::new("a", "b", "first"),
            ChannelEnvelope::new("a", "b", "second"),
        ])
    };
    for count in [0, 1, 3] {
        let tickets = (0..count).map(|_| None).collect();
        let error = DeliveryPage::new(make_page(), tickets).unwrap_err();
        assert!(matches!(error, ChannelError::InvalidEnvelope(_)));
        assert_eq!(
            error.delivery_failure_class(),
            DeliveryFailureClass::Permanent
        );
    }
    let binding = receipt(ReceiptDisposition::Stored).binding;
    let ticket = InboundReceiptTicket::new(binding.clone(), 9);
    let page = DeliveryPage::new(make_page(), vec![None, Some(ticket)]).unwrap();
    assert_eq!(page.page().envelopes.len(), 2);
    assert!(page.tickets()[0].is_none());
    let ticket = page.tickets()[1].as_ref().unwrap();
    assert_eq!(ticket.binding(), &binding);
    assert_eq!(ticket.sender_key_epoch(), 9);
    assert!(DeliveryPage::new(ChannelPollPage::stateless(vec![]), vec![]).is_ok());
}

#[test]
fn receipt_disposition_is_closed_and_outcomes_must_agree() {
    let stored = receipt(ReceiptDisposition::Stored);
    assert!(SendOutcome::LegacyAccepted.validate_receipt().is_ok());
    assert!(SendOutcome::Pending(PendingDetail::Admitted {
        admitted_at: Utc::now(),
    })
    .validate_receipt()
    .is_ok());
    assert!(
        SendOutcome::Pending(PendingDetail::Held(HoldReason::InsufficientCredit))
            .validate_receipt()
            .is_ok()
    );
    assert!(
        SendOutcome::Pending(PendingDetail::Held(HoldReason::RecipientKeyChanged))
            .validate_receipt()
            .is_ok()
    );
    assert!(SendOutcome::Pending(PendingDetail::ReceiptUnverified {
        reason: "receipt signature does not match the pinned key".into(),
    })
    .validate_receipt()
    .is_ok());
    let verified_stored = verified_receipt(ReceiptDisposition::Stored);
    let verified_quarantined = verified_receipt(ReceiptDisposition::Quarantined);
    assert!(SendOutcome::RecipientStored(verified_stored.clone())
        .validate_receipt()
        .is_ok());
    assert!(
        SendOutcome::RecipientQuarantined(verified_quarantined.clone())
            .validate_receipt()
            .is_ok()
    );
    assert!(SendOutcome::RecipientStored(verified_quarantined)
        .validate_receipt()
        .is_err());
    assert!(SendOutcome::RecipientQuarantined(verified_stored)
        .validate_receipt()
        .is_err());
    let mut value = serde_json::to_value(&stored).unwrap();
    assert_eq!(value["disposition"], "stored");
    assert_eq!(
        serde_json::from_value::<DeliveryReceipt>(value.clone()).unwrap(),
        stored
    );
    value["disposition"] = serde_json::json!("pending");
    assert!(serde_json::from_value::<DeliveryReceipt>(value).is_err());
}
