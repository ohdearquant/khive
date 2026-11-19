use super::*;
use crate::{ChannelCheckpoint, ChannelEnvelope, ChannelPollPage, DeliveryPage};

#[test]
fn receipt_page_preserves_order_cursor_and_envelope_checkpoint() {
    let claim = SenderReceiptClaim {
        logical_message_id: Some(Uuid::new_v4()),
        recipient_device_id: Some(Uuid::new_v4()),
        recipient_key_epoch: Some(7),
    };
    let checkpoint = ChannelCheckpoint {
        source: "envelope-source".into(),
        generation: 2,
        high_water: Some(13),
    };
    let envelope = ChannelEnvelope::new("a", "b", "retained");
    let envelope_bytes = serde_json::to_vec(&envelope).unwrap();
    let results = vec![
        SenderReceiptResult::Rejected {
            claim: claim.clone(),
            reason: ReceiptRejectionReason::BindingMismatch,
        },
        SenderReceiptResult::Unhandled {
            claim: claim.clone(),
            reason: ReceiptReadFailure::PinUnavailable,
        },
    ];
    let page = DeliveryPage::new_with_receipts(
        ChannelPollPage {
            envelopes: vec![envelope],
            next_checkpoint: Some(checkpoint.clone()),
        },
        vec![None],
        results,
        Some(0),
    )
    .unwrap();
    assert_eq!(page.receipts_cursor(), Some(0));
    assert_eq!(page.receipt_results().len(), 2);
    assert!(matches!(&page.receipt_results()[0],
        SenderReceiptResult::Rejected { claim: observed, reason: ReceiptRejectionReason::BindingMismatch }
        if observed == &claim));
    assert!(matches!(&page.receipt_results()[1],
        SenderReceiptResult::Unhandled { claim: observed, reason: ReceiptReadFailure::PinUnavailable }
        if observed == &claim));
    let (page, tickets, results, cursor) = page.into_receipt_parts();
    assert_eq!(cursor, Some(0));
    assert_eq!(results.len(), 2);
    assert_eq!(tickets.len(), 1);
    assert!(tickets[0].is_none());
    assert_eq!(page.next_checkpoint, Some(checkpoint));
    assert_eq!(
        serde_json::to_vec(&page.envelopes[0]).unwrap(),
        envelope_bytes
    );
}

#[test]
fn legacy_receipt_page_has_no_sender_results_or_cursor() {
    let page = DeliveryPage::legacy(ChannelPollPage::stateless(vec![ChannelEnvelope::new(
        "a", "b", "legacy",
    )]));
    assert!(page.receipt_results().is_empty());
    assert_eq!(page.receipts_cursor(), None);
    let (page, tickets) = page.into_parts();
    assert_eq!(page.envelopes[0].content, "legacy");
    assert_eq!(tickets.len(), 1);
}

#[test]
fn receipt_page_rejects_mismatched_envelope_ticket_counts() {
    let error = DeliveryPage::new_with_receipts(
        ChannelPollPage::stateless(Vec::new()),
        vec![None],
        Vec::new(),
        Some(0),
    )
    .unwrap_err();
    assert!(matches!(error, crate::ChannelError::InvalidEnvelope(_)));
}

#[test]
fn sender_receipt_rejection_spellings_are_stable() {
    for (reason, spelling) in [
        (ReceiptRejectionReason::ParseFailure, "ParseFailure"),
        (ReceiptRejectionReason::SourceMissing, "SourceMissing"),
        (ReceiptRejectionReason::SourceMismatch, "SourceMismatch"),
        (ReceiptRejectionReason::BindingMismatch, "BindingMismatch"),
        (ReceiptRejectionReason::PinUnconfirmed, "PinUnconfirmed"),
        (
            ReceiptRejectionReason::FingerprintMismatch,
            "FingerprintMismatch",
        ),
        (ReceiptRejectionReason::InvalidSignature, "InvalidSignature"),
    ] {
        assert_eq!(reason.as_str(), spelling);
    }
}
