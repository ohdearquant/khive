UPDATE notes SET
 properties=json_set(COALESCE(properties,'{}'), '$.status',
  (SELECT CASE
   WHEN state='cancelled' THEN 'cancelled'
   WHEN next_seconds IS NOT NULL OR selected_seconds IS NOT NULL THEN 'pending'
   WHEN active_invocation_id IS NOT NULL THEN 'firing'
   WHEN suspended_invocation_id IS NOT NULL THEN 'suspended'
   ELSE 'exhausted' END
   FROM scheduled_series_v3 WHERE store_id=?1 AND schedule_id=?2),
  '$.trigger_at',COALESCE(?4,json_extract(properties,'$.trigger_at'))),
 updated_at=MAX(updated_at+1,?5)
WHERE id=?2 AND namespace=?3 AND kind='scheduled_event' AND deleted_at IS NULL
 AND version>=1 AND version<9223372036854775807 AND updated_at<9223372036854775807
 AND json_type(properties,'$."khive:secret_gate"') IS NULL
 AND json_type(properties,'$."khive:web_receipt"') IS NULL
 AND EXISTS(SELECT 1 FROM scheduled_series_v3 WHERE store_id=?1 AND schedule_id=?2);
