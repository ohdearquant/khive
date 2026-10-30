INSERT INTO scheduled_attempts_v3
(id,occurrence_id,namespace,invocation_id,claim_generation,owner_nonce,lease_expires_at_us)
VALUES (?1,?2,?3,?4,1,?5,?6)
ON CONFLICT(occurrence_id) DO UPDATE SET
 claim_generation=scheduled_attempts_v3.claim_generation+1,
 owner_nonce=excluded.owner_nonce,lease_expires_at_us=excluded.lease_expires_at_us
WHERE scheduled_attempts_v3.launch_intent_id IS NULL
AND scheduled_attempts_v3.lease_expires_at_us<=?7;
