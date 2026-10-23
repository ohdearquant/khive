INSERT INTO scheduled_receipts_v3
(id,occurrence_id,namespace,record_kind,outcome,reason_code,invocation_id,exec_receipt_id,
prior_receipt_id,recorded_at_us,final_decision_at_us,started_at_us,completed_at_us,
exit_code,exit_signal,canonical_evidence)
VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16);
