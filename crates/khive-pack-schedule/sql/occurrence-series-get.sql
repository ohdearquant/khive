SELECT s.*,n.id AS source_note_id,n.namespace AS source_note_namespace,
n.kind AS source_note_kind,n.deleted_at AS source_note_deleted_at
FROM scheduled_series_v3 s LEFT JOIN notes n ON n.id=s.schedule_id
WHERE s.store_id=?1 AND s.schedule_id=?2;
