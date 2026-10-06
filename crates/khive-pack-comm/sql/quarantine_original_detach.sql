DELETE FROM attachments WHERE record_uuid = ?1
AND role = 'quarantine-original' AND substrate = 'note'
AND content_ref = ?2
