SELECT o.*,a.id AS attempt_id,a.invocation_id,a.claim_generation,a.owner_nonce,
a.lease_expires_at_us,a.launch_intent_id,a.admission_at_us,a.launch_intent_at_us,
r.id AS terminal_receipt_id,r.outcome AS terminal_outcome,r.reason_code AS terminal_reason_code,
r.exec_receipt_id AS terminal_exec_receipt_id,r.recorded_at_us AS terminal_recorded_at_us,
r.final_decision_at_us AS terminal_final_decision_at_us,r.started_at_us AS terminal_started_at_us,
r.completed_at_us AS terminal_completed_at_us,r.exit_code AS terminal_exit_code,
r.exit_signal AS terminal_exit_signal,r.canonical_evidence AS terminal_evidence
FROM scheduled_occurrences_v3 o
LEFT JOIN scheduled_attempts_v3 a ON a.occurrence_id=o.id
LEFT JOIN scheduled_receipts_v3 r ON r.occurrence_id=o.id AND r.record_kind='terminal'
WHERE o.store_id=?1 AND (?2 IS NULL OR o.namespace=?2) AND o.schedule_id=?3
AND (?4 IS NULL OR o.definition_revision=?4)
AND (?5 IS NULL OR o.scheduled_seconds>?5 OR
(o.scheduled_seconds=?5 AND (o.scheduled_nanos>?6 OR
(o.scheduled_nanos=?6 AND o.id>?7))))
AND (?9 IS NULL OR o.id=?9)
ORDER BY o.scheduled_seconds,o.scheduled_nanos,o.id LIMIT ?8;
