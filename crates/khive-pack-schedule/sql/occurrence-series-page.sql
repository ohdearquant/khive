WITH clock AS (
 SELECT (?3/1000000)-CASE WHEN ?3<0 AND ?3%1000000!=0 THEN 1 ELSE 0 END AS seconds,
 ((?3%1000000+1000000)%1000000)*1000 AS nanos
), work AS (
 SELECT s.*, o.id AS ready_unclaimed,
 EXISTS (SELECT 1 FROM scheduled_occurrences_v3 e
 JOIN scheduled_attempts_v3 a ON a.occurrence_id=e.id
 WHERE e.store_id=s.store_id AND e.schedule_id=s.schedule_id
 AND a.lease_expires_at_us<=?3
 AND NOT EXISTS (SELECT 1 FROM scheduled_receipts_v3 r
 WHERE r.occurrence_id=e.id AND r.record_kind='terminal')) AS expired_attempt
 FROM scheduled_series_v3 s
 LEFT JOIN scheduled_occurrences_v3 o ON o.store_id=s.store_id
 AND o.schedule_id=s.schedule_id AND o.definition_revision=s.definition_revision
 AND o.scheduled_seconds=s.selected_seconds AND o.scheduled_nanos=s.selected_nanos
 AND NOT EXISTS (SELECT 1 FROM scheduled_attempts_v3 a WHERE a.occurrence_id=o.id)
 AND NOT EXISTS (SELECT 1 FROM scheduled_receipts_v3 r
 WHERE r.occurrence_id=o.id AND r.record_kind='terminal')
 WHERE s.store_id=?1 AND (?5 IS NULL OR s.schedule_id>?5)
)
SELECT w.* FROM work w, clock c WHERE
 (?2='unresolved_evidence' AND w.suspended_invocation_id IS NOT NULL) OR
 (?2='accounting_or_recovery' AND (
 w.ready_unclaimed IS NOT NULL OR w.expired_attempt OR
 (w.cancellation_cut_us IS NOT NULL AND w.next_seconds IS NOT NULL AND
 (w.next_seconds > (w.cancellation_cut_us/1000000)-CASE WHEN w.cancellation_cut_us<0 AND w.cancellation_cut_us%1000000!=0 THEN 1 ELSE 0 END OR
 (w.next_seconds = (w.cancellation_cut_us/1000000)-CASE WHEN w.cancellation_cut_us<0 AND w.cancellation_cut_us%1000000!=0 THEN 1 ELSE 0 END
 AND w.next_nanos >= ((w.cancellation_cut_us%1000000+1000000)%1000000)*1000))) OR
 (?4>0 AND w.next_seconds IS NOT NULL AND (
 (w.horizon_seconds IS NOT NULL AND
 (w.next_seconds<w.horizon_seconds OR
 (w.next_seconds=w.horizon_seconds AND w.next_nanos<=w.horizon_nanos))) OR
 (w.horizon_seconds IS NULL AND w.state IN ('active','suspended') AND
 (w.next_seconds<c.seconds OR (w.next_seconds=c.seconds AND w.next_nanos<=c.nanos)))))))
ORDER BY w.schedule_id LIMIT ?6;
