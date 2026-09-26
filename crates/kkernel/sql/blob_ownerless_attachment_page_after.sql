SELECT record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at
FROM attachments
WHERE (record_uuid, role) > (?1, ?2)
ORDER BY record_uuid COLLATE BINARY, role COLLATE BINARY
LIMIT ?3
