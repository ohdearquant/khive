INSERT INTO scheduled_notice_outbox_v3
(id,store_id,occurrence_id,terminal_receipt_id,namespace,recipient_kind,recipient_id,
notice_kind,intent_digest,canonical_intent,created_at_us)
VALUES (?1,?2,?3,?4,?5,?6,?7,'outcome_notice',?8,?9,?10);
