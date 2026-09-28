-- Scope a live note's external ID to its exact channel identity. The V5
-- index has already reconciled historical global duplicates, so widening its
-- key does not require a data rewrite. Missing channel fields occupy the
-- empty partition and retain uniqueness for unattributed legacy notes.
DROP INDEX IF EXISTS idx_comm_message_external_id;
CREATE UNIQUE INDEX idx_comm_message_external_id
    ON notes(
        namespace,
        kind,
        json_extract(properties, '$.external_id'),
        ifnull(json_extract(properties, '$.channel_kind'), '') COLLATE BINARY,
        ifnull(json_extract(properties, '$.channel_slug'), '') COLLATE BINARY
    )
    WHERE deleted_at IS NULL
      AND json_extract(properties, '$.external_id') IS NOT NULL
      AND json_extract(properties, '$.external_id') != '';
