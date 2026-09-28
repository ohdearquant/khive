SELECT record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at
FROM attachments
ORDER BY record_uuid COLLATE BINARY, role COLLATE BINARY
LIMIT ?1
