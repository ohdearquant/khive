SELECT COUNT(*) AS present_tables FROM sqlite_schema
WHERE type='table' AND name IN
('scheduled_definitions_v3','scheduled_series_v3','scheduled_occurrences_v3',
 'scheduled_attempts_v3','scheduled_receipts_v3','scheduled_notice_outbox_v3');
