SELECT sql FROM sqlite_schema
WHERE type = 'index'
  AND name = 'idx_comm_message_outbound_due'
  AND tbl_name = 'notes';
