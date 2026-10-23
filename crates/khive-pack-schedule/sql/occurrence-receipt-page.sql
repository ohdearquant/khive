SELECT r.* FROM scheduled_receipts_v3 r
JOIN scheduled_occurrences_v3 o ON o.id=r.occurrence_id
WHERE o.store_id=?1 AND (?2 IS NULL OR r.namespace=?2) AND r.occurrence_id=?3
AND (?4 IS NULL OR r.recorded_at_us>?4 OR (r.recorded_at_us=?4 AND r.id>?5))
AND (?7 IS NULL OR r.record_kind=?7)
ORDER BY r.recorded_at_us,r.id LIMIT ?6;
